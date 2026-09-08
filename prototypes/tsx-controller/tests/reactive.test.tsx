import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { compile, ConfigMap, Controller, Deployment, useFinalizer, useResource, useResourceRef } from "../src";
import { ControllerRunner } from "../src/controller";
import { createAlchemyEngine } from "../src/engine";
import { prepareReactive, type Snapshots } from "../src/reactive";
import { waitForChanges } from "../src/kubernetes/watch";
import { observationKey } from "../src/authoring/context";

const paths: string[] = [];
afterEach(() => paths.splice(0).forEach(path => rmSync(path, { recursive: true, force: true })));
function setup() {
  const directory = mkdtempSync(join(tmpdir(), "tsx-reactive-")); paths.push(directory);
  let config: Record<string, unknown> | undefined;
  let unsafeName = false;
  const cleanup: string[] = [];
  function App() {
    const config = useResource(ConfigMap, { name: "config" });
    const ref = useResourceRef();
    if (config.data.DISABLE !== "yes") useFinalizer(ref, "test.example/cleanup", async () => { cleanup.push(config.data.MODE!); });
    return <Deployment id="web" name={unsafeName ? config.data.NAME ?? "web" : "web"}
      image="nginx:stable" envFrom={[config]} ref={ref} />;
  }
  const render = (snapshots?: Snapshots) => compile(<Controller name="reactive" namespace="demo"><App /></Controller>, { snapshots });
  const plan = render();
  const engine = createAlchemyEngine({ directory });
  function runner() {
    return new ControllerRunner(plan, engine, { directory,
      prepare: prepareReactive(directory, plan, render, async () => config ? structuredClone(config) : undefined) });
  }
  return { directory, engine, runner, cleanup, set: (next: Record<string, unknown> | undefined) => { config = next; },
    unsafe: () => { unsafeName = true; } };
}

test("required observations suspend without pruning and resume through UID replacement", async () => {
  const f = setup();
  expect((await f.runner().reconcile()).conditions[0]!.reason).toBe("RequiredResourceUnavailable");
  expect(f.engine.world.list()).toHaveLength(0);
  f.set({ metadata: { uid: "one", resourceVersion: "1" }, data: { MODE: "one" } });
  await f.runner().reconcile();
  const first = f.engine.world.get("web")!;
  f.set({ metadata: { uid: "one", resourceVersion: "2", annotations: { note: "metadata" } }, data: { MODE: "one" } });
  await f.runner().reconcile();
  expect(f.engine.world.get("web")!.props).toEqual(first.props);
  f.set(undefined);
  f.engine.world.drift("web", { image: "external-drift" });
  expect((await f.runner().reconcile()).phase).toBe("blocked");
  expect(f.engine.world.get("web")!.uid).toBe(first.uid);
  expect(f.engine.world.get("web")!.props.image).toBe("external-drift");
  f.set({ metadata: { uid: "replacement", resourceVersion: "8" }, data: { MODE: "two" } });
  await f.runner().reconcile();
  expect(f.engine.world.get("web")!.uid).toBe(first.uid);
  expect(f.engine.world.get("web")!.props.podAnnotations).not.toEqual(first.props.podAnnotations);
});

test("missing input after restart still finalizes from persisted real snapshots", async () => {
  const f = setup();
  f.set({ metadata: { uid: "one" }, data: { MODE: "real-value" } });
  await f.runner().reconcile();
  f.set(undefined);
  const live = f.engine.world.get("web")!;
  f.engine.world.requestDelete("web", live.uid);
  const report = await f.runner().reconcile();
  expect(report.conditions).toContainEqual({ resource: "demo/config", reason: "RequiredResourceUnavailable" });
  expect(f.cleanup).toEqual(["real-value"]);
  expect(f.engine.world.get("web")).toBeUndefined();
});

test("explicit finalize while missing never applies the cached desired graph", async () => {
  const f = setup();
  f.set({ metadata: { uid: "one" }, data: { MODE: "last-render" } });
  await f.runner().reconcile();
  f.set(undefined);
  expect((await f.runner().finalizeOnly()).phase).toBe("ready");
  expect(f.cleanup).toEqual(["last-render"]);
  expect(f.engine.world.list()).toHaveLength(0);
});

test("runtime permission expansion is rejected before snapshot or provider mutation", async () => {
  const f = setup();
  f.set({ data: { MODE: "one" } });
  await f.runner().reconcile();
  const previous = readFileSync(join(f.directory, "render-snapshots.json"), "utf8");
  f.unsafe(); f.set({ data: { MODE: "two", NAME: "unplanned-target" } });
  await expect(f.runner().reconcile()).rejects.toThrow("requires redeployment");
  expect(readFileSync(join(f.directory, "render-snapshots.json"), "utf8")).toBe(previous);
  expect(f.engine.world.get("web")!.name).toBe("web");
});

test("named Kubernetes watch resumes from observed version and wakes on modification", async () => {
  let query: URL | undefined;
  const server = Bun.serve({ port: 0, fetch(request) {
    query = new URL(request.url);
    return new Response(JSON.stringify({ type: "MODIFIED", object: { metadata: { resourceVersion: "12" } } }) + "\n");
  } });
  try {
    const target = { kind: "kubernetes:ConfigMap" as const, name: "config", namespace: "demo", access: ["read" as const] };
    const reason = await waitForChanges({ server: server.url.toString() }, "demo", [target],
      new Map([[observationKey(target), "11"]]), new AbortController().signal, 1000);
    expect(reason).toBe("watch");
    expect(query!.searchParams.get("resourceVersion")).toBe("11");
    expect(query!.searchParams.get("fieldSelector")).toBe("metadata.name=config");
  } finally { server.stop(true); }
});


test("rejected handler removal preserves previous snapshots for finalization", async () => {
  const f = setup();
  f.set({ data: { MODE: "recoverable" } });
  await f.runner().reconcile();
  const previous = readFileSync(join(f.directory, "render-snapshots.json"), "utf8");
  f.set({ data: { MODE: "rejected", DISABLE: "yes" } });
  expect((await f.runner().reconcile()).conditions).toContainEqual({ resource: "web", reason: "MissingFinalizerHandler:test.example/cleanup" });
  expect(readFileSync(join(f.directory, "render-snapshots.json"), "utf8")).toBe(previous);
  expect((await f.runner().finalizeOnly()).phase).toBe("ready");
  expect(f.cleanup).toEqual(["recoverable"]);
});
