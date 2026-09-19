import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Cloudflare, compile, ConfigMap, Controller, Deployment, PlanetScale, Service, useResourceRef, type Finalizer } from "../src";
import { ControllerRunner } from "../src/controller";
import { createAlchemyEngine } from "../src/engine";
import { readInventory } from "../src/state";

const directories: string[] = [];
function directory() {
  const path = mkdtempSync(join(tmpdir(), "tsx-controller-integration-"));
  directories.push(path);
  return path;
}
afterEach(() => { for (const path of directories.splice(0)) rmSync(path, { recursive: true, force: true }); });
const key = "example.com/cleanup";
function basic(finalizer?: Finalizer) {
  return compile(<Controller name="test" namespace="demo">
    <Deployment id="web" name="web" image="nginx:stable" deletionPolicy="delete" finalizers={finalizer ? { [key]: finalizer } : {}} />
    <ConfigMap id="sibling" name="sibling" data={{ version: "original" }} />
  </Controller>);
}

test("readiness and typed outputs converge across restart without pruning blocked resources", async () => {
  function App() {
    const database = useResourceRef<{ host: string }>();
    const service = useResourceRef<{ address: string }>();
    return <Controller name="test" namespace="demo">
      <PlanetScale.Database id="db" name="db" organization="example" ref={database} />
      <Deployment id="web" name="web" image="nginx:stable" readyAfter={[database]} env={{ DB_HOST: database.output("host") }} />
      <Service id="service" name="public" type="LoadBalancer" port={80} selector={{ "app.kubernetes.io/name": "web" }} ref={service} />
      <Cloudflare.DNSRecord id="dns" name="web.example.com" zoneId="zone" type="A" content={service.output("address")} />
    </Controller>;
  }
  const path = directory(), graph = compile(<App />), engine = createAlchemyEngine({ directory: path });
  expect((await new ControllerRunner(graph, engine, { directory: path }).reconcile()).phase).toBe("blocked");
  expect(engine.world.get("web")).toBeUndefined();
  expect(engine.world.get("dns")).toBeUndefined();
  engine.world.setReady("db", true);
  engine.world.setServiceIPv4("service", "203.0.113.10");
  const restarted = createAlchemyEngine({ directory: path });
  const runner = new ControllerRunner(graph, restarted, { directory: path });
  expect((await runner.reconcile()).phase).toBe("ready");
  const web = restarted.world.get("web")!, dns = restarted.world.get("dns")!;
  expect(web.props.env).toEqual({ DB_HOST: "db.simulated.invalid" });
  expect(dns.props.content).toBe("203.0.113.10");
  restarted.world.setReady("db", false);
  expect((await runner.reconcile()).phase).toBe("blocked");
  expect(restarted.world.get("web")?.uid).toBe(web.uid);
  expect(restarted.world.get("dns")?.uid).toBe(dns.uid);
  restarted.world.setReady("db", true);
  restarted.world.drift("web", { image: "wrong" });
  restarted.world.setServiceIPv4("service", "203.0.113.11");
  expect((await runner.reconcile()).phase).toBe("ready");
  expect(restarted.world.get("web")?.props.image).toBe("nginx:stable");
  expect(restarted.world.get("web")?.uid).toBe(web.uid);
  expect(restarted.world.get("dns")?.props.content).toBe("203.0.113.11");
});

test("failed finalizer survives restart and finalize-only does not restore stale sibling props", async () => {
  let fail = true, calls = 0;
  const graph = basic(async () => { calls++; if (fail) throw new Error("sensitive failure text"); });
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  const runner = new ControllerRunner(graph, engine, { directory: path });
  await runner.reconcile();
  const first = await runner.finalizeOnly(["web"]);
  expect(first.conditions).toEqual([{ resource: "web", reason: `FinalizerFailed:${key}` }]);
  expect(JSON.stringify(first)).not.toContain("sensitive failure text");
  expect(engine.world.get("web")?.finalizers).toEqual([key]);
  expect(engine.world.get("web")?.deleting).toBe(true);
  engine.world.drift("sibling", { data: { version: "new-executable" } });
  fail = false;
  const restarted = createAlchemyEngine({ directory: path });
  expect((await new ControllerRunner(graph, restarted, { directory: path }).finalizeOnly(["web"])).phase).toBe("ready");
  expect(calls).toBe(2);
  expect(restarted.world.get("web")).toBeUndefined();
  expect(restarted.world.get("sibling")?.props).toEqual({ data: { version: "new-executable" } });
  expect(readInventory(path, "test").resources.web).toBeUndefined();
});

test("removing a resource or its finalizer implementation blocks without erasing the obligation", async () => {
  const old = basic(async () => {}), path = directory(), engine = createAlchemyEngine({ directory: path });
  await new ControllerRunner(old, engine, { directory: path }).reconcile();
  for (const graph of [basic(), compile(<Controller name="test" namespace="demo" />)]) {
    const report = await new ControllerRunner(graph, engine, { directory: path }).reconcile();
    expect(report.conditions).toContainEqual({ resource: "web", reason: `MissingFinalizerHandler:${key}` });
    expect(engine.world.get("web")?.finalizers).toEqual([key]);
    expect(engine.world.get("web")?.deleting).toBe(false);
  }
  await new ControllerRunner(old, engine, { directory: path }).finalizeOnly(["web"]);
  expect(engine.world.get("web")).toBeUndefined();
});

test("a replaced UID blocks cleanup before invoking user code", async () => {
  let called = false;
  const graph = basic(async () => { called = true; });
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  await new ControllerRunner(graph, engine, { directory: path }).reconcile();
  const desired = readInventory(path, "test").resources.web!.desired!;
  engine.world.deleteExternally("web");
  const replacement = engine.world.reconcile(desired);
  const report = await new ControllerRunner(graph, createAlchemyEngine({ directory: path }), { directory: path }).finalizeOnly(["web"]);
  expect(report.conditions).toContainEqual({ resource: "web", reason: "ResourceIdentityChanged" });
  expect(called).toBe(false);
  expect(engine.world.get("web")?.uid).toBe(replacement.uid);
  expect(engine.world.get("web")?.deleting).toBe(false);
});

test("timeout cancels cleanup and preserves the finalizer", async () => {
  let release!: () => void, finished!: () => void, lateError: unknown;
  const gate = new Promise<void>(resolve => { release = resolve; });
  const done = new Promise<void>(resolve => { finished = resolve; });
  function App() {
    return <Controller name="test" namespace="demo"><Deployment id="web" name="web" image="nginx" deletionPolicy="delete" finalizers={{
      [key]: async ({ signal }) => { await gate; try { signal.throwIfAborted(); } catch (error) { lateError = error; } finally { finished(); } },
    }} /></Controller>;
  }
  const graph = compile(<App />), path = directory(), engine = createAlchemyEngine({ directory: path });
  const runner = new ControllerRunner(graph, engine, { directory: path, finalizerTimeoutMs: 10 });
  await runner.reconcile();
  const report = await runner.finalizeOnly(["web"]);
  expect(report.conditions).toContainEqual({ resource: "web", reason: `FinalizerFailed:${key}` });
  release(); await done;
  expect(lateError).toBeDefined();
  expect(engine.world.get("web")?.finalizers).toEqual([key]);
});

test("two runners cannot share a state directory during a live finalizer", async () => {
  let release!: () => void, started!: () => void;
  const gate = new Promise<void>(resolve => { release = resolve; });
  const entered = new Promise<void>(resolve => { started = resolve; });
  const graph = basic(async () => { started(); await gate; });
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  const runner = new ControllerRunner(graph, engine, { directory: path });
  await runner.reconcile();
  const first = runner.finalizeOnly(["web"]);
  await entered;
  try {
    const second = new ControllerRunner(graph, createAlchemyEngine({ directory: path }), { directory: path });
    await expect(second.reconcile()).rejects.toThrow("Another controller owns this state directory");
  } finally { release(); await first; }
});

function dependencyGraph() {
  const database = useResourceRef<{ host: string }>();
  return compile(<Controller name="test" namespace="demo">
    <PlanetScale.Database id="database" name="database" organization="example" ref={database} deletionPolicy="delete" />
    <Deployment id="web" name="web" image="nginx" readyAfter={[database]} env={{ DB_HOST: database.output("host") }} />
    <ConfigMap id="independent" name="independent" data={{ marker: "keep" }} />
  </Controller>);
}

test("selected cleanup refuses to delete a dependency with an unselected managed dependent", async () => {
  const graph = dependencyGraph(), path = directory(), engine = createAlchemyEngine({ directory: path });
  const runner = new ControllerRunner(graph, engine, { directory: path });
  await runner.reconcile();
  engine.world.setReady("database", true);
  await runner.reconcile();
  const original = engine.world.get("database")!;
  const report = await runner.finalizeOnly(["database"]);
  expect(report.phase).toBe("blocked");
  expect(report.conditions).toContainEqual({ resource: "database", reason: "CleanupScopeHasDependents" });
  expect(engine.world.get("database")?.uid).toBe(original.uid);
  expect(engine.world.get("database")?.deleting).toBe(false);
  expect(engine.world.get("web")).toBeDefined();
});

test("restart after finalizer removal succeeds before acknowledgment does not repeat completed cleanup", async () => {
  let calls = 0, inject = true;
  const graph = basic(async () => { calls++; });
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  const remove = engine.removeFinalizer.bind(engine);
  engine.removeFinalizer = async (id, uid, finalizerKey) => {
    await remove(id, uid, finalizerKey);
    if (inject) { inject = false; throw new Error("injected acknowledgment loss"); }
  };
  const runner = new ControllerRunner(graph, engine, { directory: path });
  await runner.reconcile();
  expect((await runner.finalizeOnly(["web"])).phase).toBe("blocked");
  expect(engine.world.get("web")).toBeUndefined();
  expect(readInventory(path, "test").resources.web).toBeDefined();
  const restarted = createAlchemyEngine({ directory: path });
  expect((await new ControllerRunner(graph, restarted, { directory: path }).finalizeOnly(["web"])).phase).toBe("ready");
  expect(calls).toBe(1);
  expect(restarted.world.get("web")).toBeUndefined();
  expect(restarted.world.events().filter(event => event.action === "create" && event.id === "web")).toHaveLength(1);
  expect(readInventory(path, "test").resources.web).toBeUndefined();
});

test("an externally deleted blocked resource is not recreated by another resource's apply", async () => {
  const graph = dependencyGraph(), path = directory(), engine = createAlchemyEngine({ directory: path });
  const runner = new ControllerRunner(graph, engine, { directory: path });
  await runner.reconcile();
  engine.world.setReady("database", true);
  await runner.reconcile();
  expect(engine.world.get("web")).toBeDefined();
  engine.world.setReady("database", false);
  engine.world.deleteExternally("web");
  engine.world.drift("independent", { data: { marker: "wrong" } });
  const restarted = createAlchemyEngine({ directory: path });
  const report = await new ControllerRunner(graph, restarted, { directory: path }).reconcile();
  expect(report.phase).toBe("blocked");
  expect(report.conditions).toContainEqual({ resource: "web", reason: "DependencyNotReady:database" });
  expect(restarted.world.get("web")).toBeUndefined();
  expect(restarted.world.get("independent")?.props).toEqual({ data: { marker: "keep" } });
  expect(restarted.world.events().filter(event => event.action === "create" && event.id === "web")).toHaveLength(1);
});

test("foreign finalizers remain untouched while owned cleanup completes", async () => {
  const path = directory();
  let calls = 0;
  const graph = basic(async () => { calls++; });
  const engine = createAlchemyEngine({ directory: path });
  const runner = new ControllerRunner(graph, engine, { directory: path });
  await runner.reconcile();
  const inventory = readInventory(path, graph.name);
  engine.world.reconcile({ ...inventory.resources.web!.desired!, finalizerKeys: [key, "foregroundDeletion"] });
  const report = await runner.finalizeOnly(["web"]);
  expect(calls).toBe(1);
  expect(report.conditions).toContainEqual({ resource: "web", reason: "DeletionPending" });
  expect(engine.world.get("web")!.finalizers).toEqual(["foregroundDeletion"]);
  expect((await runner.finalizeOnly(["web"])).conditions).toContainEqual({ resource: "web", reason: "DeletionPending" });
  expect(calls).toBe(1);
});
