import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createAlchemyEngine, SimulatedWorld } from "../src/engine";
import type { Observation, ResolvedResource } from "../src/contracts";

const directories: string[] = [];
function directory() { const path = mkdtempSync(join(tmpdir(), "tsx-engine-")); directories.push(path); return path; }
afterEach(() => { for (const path of directories.splice(0)) rmSync(path, { recursive: true, force: true }); });
const resource = (overrides: Partial<ResolvedResource> = {}): ResolvedResource => ({
  id: "web", kind: "kubernetes:Deployment", name: "web", namespace: "demo",
  props: { image: "nginx:stable" }, finalizerKeys: [], deletionPolicy: "delete", ...overrides,
});

test("actual Alchemy persists lifecycle state, repairs drift, and preserves UID after restart", async () => {
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  const desired = resource();
  await engine.apply([desired]);
  const initial = await engine.observe("web");
  expect(initial?.uid).toBeString();
  expect(readFileSync(join(path, "alchemy-state.json"), "utf8")).toContain('"status": "created"');
  await engine.apply([desired]);
  expect(engine.world.events().filter((e) => e.action === "create")).toHaveLength(1);
  expect(engine.world.events().filter((e) => e.action === "update")).toHaveLength(0);
  engine.world.drift("web", { image: "wrong" });
  const restarted = createAlchemyEngine({ directory: path });
  await restarted.apply([desired]);
  expect((await restarted.observe("web"))?.props).toEqual(desired.props);
  expect((await restarted.observe("web"))?.uid).toBe(initial!.uid);
});

test("cloud success before Alchemy state failure recovers without duplicate creation", async () => {
  const path = directory(); let failed = false;
  const engine = createAlchemyEngine({ directory: path, beforeStateSave(value) {
    if (!failed && (value as { status?: string })?.status === "created") { failed = true; throw new Error("injected persistence failure"); }
  } });
  await expect(engine.apply([resource()])).rejects.toThrow();
  const uid = engine.world.get("web")!.uid;
  const restarted = createAlchemyEngine({ directory: path });
  await restarted.apply([resource()]);
  expect(restarted.world.get("web")?.uid).toBe(uid);
  expect(restarted.world.events().filter((e) => e.action === "create")).toHaveLength(1);
});

test("finalizers and deletion are UID-guarded and survive desired refresh", async () => {
  const engine = createAlchemyEngine({ directory: directory() });
  await engine.apply([resource({ finalizerKeys: ["cleanup", "other"] })]);
  const uid = engine.world.get("web")!.uid;
  await engine.requestDelete("web", uid);
  await engine.apply([resource()]);
  expect(engine.world.get("web")?.deleting).toBe(true);
  expect(engine.world.get("web")?.finalizers).toEqual(["cleanup", "other"]);
  await expect(engine.removeFinalizer("web", "wrong", "cleanup")).rejects.toThrow("UID conflict");
  await engine.removeFinalizer("web", uid, "cleanup");
  expect(engine.world.get("web")?.finalizers).toEqual(["other"]);
  await engine.removeFinalizer("web", uid, "other");
  expect(engine.world.get("web")).toBeUndefined();
});

test("persisted observation UID rejects replacement across restart", async () => {
  const path = directory(), world = new SimulatedWorld(path);
  const target: Observation = { kind: "kubernetes:ConfigMap", name: "cleanup", namespace: "demo", access: ["read", "delete", "patch"] };
  world.seedObservation(target, { data: { owner: "first" } });
  await createAlchemyEngine({ directory: path }).operations.get(target);
  world.seedObservation(target, { data: { owner: "replacement" } });
  const restarted = createAlchemyEngine({ directory: path });
  await expect(restarted.operations.ensureDeleted(target)).rejects.toThrow("UID conflict");
  expect(world.getObservation(target)).toBeDefined();
});

test("preconstructed engines reload UID bindings before touching observations", async () => {
  const path = directory(), world = new SimulatedWorld(path);
  const first = createAlchemyEngine({ directory: path }), second = createAlchemyEngine({ directory: path });
  const target: Observation = { kind: "kubernetes:ConfigMap", name: "cleanup", namespace: "demo", access: ["read", "delete"] };
  world.seedObservation(target, { data: { owner: "first" } });
  await first.operations.get(target);
  world.seedObservation(target, { data: { owner: "replacement" } });
  await expect(second.operations.get(target)).rejects.toThrow("UID conflict");
  await expect(second.operations.ensureDeleted(target)).rejects.toThrow("UID conflict");
  expect(world.getObservation(target)).toBeDefined();
});

test("absence observed before cleanup never authorizes deleting a later object", async () => {
  const path = directory(), world = new SimulatedWorld(path);
  const target: Observation = { kind: "kubernetes:ConfigMap", name: "cleanup", namespace: "demo", access: ["read", "delete"] };
  await createAlchemyEngine({ directory: path }).operations.get(target);
  world.seedObservation(target, { data: { owner: "later" } });
  await expect(createAlchemyEngine({ directory: path }).operations.ensureDeleted(target)).rejects.toThrow("UID conflict");
  expect(world.getObservation(target)).toBeDefined();
});

test("delete-only absent cleanup persists absence before a later retry", async () => {
  const path = directory(), world = new SimulatedWorld(path);
  const target: Observation = { kind: "kubernetes:ConfigMap", name: "cleanup", namespace: "demo", access: ["delete"] };
  await createAlchemyEngine({ directory: path }).operations.ensureDeleted(target);
  world.seedObservation(target, { data: { owner: "later" } });
  await expect(createAlchemyEngine({ directory: path }).operations.ensureDeleted(target)).rejects.toThrow("UID conflict");
  expect(world.getObservation(target)).toBeDefined();
});

test("omitted resources are retained for explicit finalization and identity changes fail", async () => {
  const engine = createAlchemyEngine({ directory: directory() });
  await engine.apply([resource()]);
  await expect(engine.apply([resource({ name: "replacement" })])).rejects.toThrow("Identity change");
  await engine.apply([]);
  expect(engine.world.get("web")).toBeDefined();
});

test("provider outputs survive desired refresh and externally deleted objects converge", async () => {
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  const service = resource({ id: "service", kind: "kubernetes:Service", name: "service" });
  await engine.apply([service]);
  expect(engine.world.get("service")?.ready).toBe(false);
  engine.world.setServiceIPv4("service", "203.0.113.8");
  await engine.apply([service]);
  expect(engine.world.get("service")?.outputs.ipv4).toBe("203.0.113.8");
  const uid = engine.world.get("service")!.uid;
  engine.world.deleteExternally("service");
  await engine.apply([service]);
  expect(engine.world.get("service")?.uid).not.toBe(uid);
  await expect(engine.requestDelete("service", uid)).rejects.toThrow("UID conflict");
});

test("inactive siblings neither repair drift nor recreate while another resource applies", async () => {
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  const sibling = resource({ id: "sibling", name: "sibling" });
  const resources = [resource(), sibling];
  await engine.apply(resources);
  const uid = engine.world.get("sibling")!.uid;
  engine.world.drift("sibling", { image: "drifted" });
  await engine.apply(resources, ["web"]);
  expect(engine.world.get("sibling")?.props).toEqual({ image: "drifted" });
  expect(engine.world.get("sibling")?.uid).toBe(uid);
  engine.world.deleteExternally("sibling");
  await engine.apply(resources, ["web"]);
  expect(engine.world.get("sibling")).toBeUndefined();
  const restarted = createAlchemyEngine({ directory: path });
  await restarted.apply(resources, ["sibling"]);
  expect(restarted.world.get("sibling")?.props).toEqual(sibling.props);
});

test("inactive cold-adopted live resource remains read-only", async () => {
  const path = directory(), engine = createAlchemyEngine({ directory: path });
  const sibling = resource({ id: "sibling", name: "sibling" });
  engine.world.reconcile(sibling);
  engine.world.drift("sibling", { image: "externally-changed" });
  await engine.apply([resource(), sibling], ["web"]);
  expect(engine.world.get("sibling")?.props).toEqual({ image: "externally-changed" });
  expect(engine.world.events().filter((event) => event.action === "update" && event.id === "sibling")).toHaveLength(0);
});
