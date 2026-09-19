import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createKubernetesEngine } from "../src/kubernetes";
import { contains, manifest, ownerAnnotation, identityAnnotation, liveResource } from "../src/kubernetes/manifest";
import type { ResolvedResource, ResourceGraph } from "../src/contracts";

const temporary: string[] = [];
afterEach(() => { for (const directory of temporary.splice(0)) rmSync(directory, { recursive: true, force: true }); });
function fixture() {
  const directory = mkdtempSync(join(tmpdir(), "tsx-kubernetes-")); temporary.push(directory);
  const desired: ResolvedResource = { id: "nginx", kind: "kubernetes:Deployment", name: "nginx", namespace: "test", props: {
    image: "nginx:stable", replicas: 2, port: 80, selector: { app: "nginx" }, env: { MODE: "test" },
  }, finalizerKeys: ["example.com/cleanup"], deletionPolicy: "delete" };
  const graph: ResourceGraph = { name: "test-controller", namespace: "test", observations: [],
    resources: [{ ...desired, finalizers: {}, dependsOn: [], readyAfter: [] }] };
  return { directory, desired, graph };
}

describe("Kubernetes adapter contracts", () => {
  test("manifest detects authored drift, tolerates API defaults and gates generation readiness", () => {
    const { desired } = fixture();
    const value = manifest(desired, "owner", "test");
    value.spec.template.spec.containers[0].imagePullPolicy = "Always";
    value.metadata.uid = "one"; value.metadata.generation = 2;
    value.status = { observedGeneration: 1, updatedReplicas: 2, availableReplicas: 2 };
    expect(contains(value, manifest(desired, "owner", "test"))).toBe(true);
    expect(liveResource(desired, value).ready).toBe(false);
    value.status.observedGeneration = 2;
    expect(liveResource(desired, value).ready).toBe(true);
    value.spec.template.spec.containers[0].image = "nginx:wrong";
    expect(contains(value, manifest(desired, "owner", "test"))).toBe(false);
  });

  test("ownership collision is rejected and finalizer removal tests UID/version without touching foreign keys", async () => {
    const { directory, desired, graph } = fixture();
    let object: any = { metadata: { name: "nginx", uid: "one", resourceVersion: "4", finalizers: ["foreign.example/cleanup", "example.com/cleanup"] } };
    const calls: any[] = [];
    const server = Bun.serve({ port: 0, fetch: async (request) => {
      if (request.method === "GET") return Response.json(object);
      const body = await request.json(); calls.push({ method: request.method, body });
      return Response.json(object);
    } });
    try {
      const options = { directory, namespace: "test", graph, connection: { server: server.url.toString() } };
      const engine = createKubernetesEngine(options);
      await expect(engine.observe("nginx")).rejects.toThrow("Refusing unmanaged");
      object = { ...manifest(desired, `test-controller/${readFileSync(join(directory, "kubernetes-owner"), "utf8")}`, "test"),
        metadata: { ...object.metadata, annotations: { [ownerAnnotation]: `test-controller/${readFileSync(join(directory, "kubernetes-owner"), "utf8")}`, [identityAnnotation]: "nginx" } } };
      await expect(engine.removeFinalizer("nginx", "replacement", "example.com/cleanup")).rejects.toThrow("UID conflict");
      expect(calls).toHaveLength(0);
      await engine.removeFinalizer("nginx", "one", "example.com/cleanup");
      expect(calls[0].body).toEqual([
        { op: "test", path: "/metadata/uid", value: "one" },
        { op: "test", path: "/metadata/resourceVersion", value: "4" },
        { op: "test", path: "/metadata/finalizers/1", value: "example.com/cleanup" },
        { op: "remove", path: "/metadata/finalizers/1" },
      ]);
      await engine.requestDelete("nginx", "one");
      expect(calls[1].body.preconditions).toEqual({ uid: "one" });
      expect((await createKubernetesEngine(options).observe("nginx"))?.uid).toBe("one");
    } finally { server.stop(true); }
  });

  test("observations reject replacement after restart and cleanup waits for actual absence", async () => {
    const { directory, graph } = fixture();
    let object: any = { metadata: { name: "registration", uid: "one", resourceVersion: "2" }, data: {} };
    let deletes = 0;
    const server = Bun.serve({ port: 0, fetch: (request) => {
      if (request.method === "DELETE") { deletes++; return Response.json({ status: "Success" }); }
      return object ? Response.json(object) : Response.json({ message: "Not found" }, { status: 404 });
    } });
    try {
      const options = { directory, namespace: "test", graph, connection: { server: server.url.toString() } };
      const target = { kind: "kubernetes:ConfigMap" as const, namespace: "test", name: "registration", access: ["read", "delete"] as ("read" | "delete")[] };
      await createKubernetesEngine(options).operations.get(target);
      object.metadata.uid = "replacement";
      await expect(createKubernetesEngine(options).operations.ensureDeleted(target)).rejects.toThrow("UID conflict");
      expect(deletes).toBe(0);
      object.metadata.uid = "one";
      const restarted = createKubernetesEngine(options);
      await expect(restarted.operations.ensureDeleted(target)).rejects.toThrow("Deletion pending");
      expect(deletes).toBe(1);
      object = undefined;
      await restarted.operations.ensureDeleted(target);
      expect(deletes).toBe(1);
    } finally { server.stop(true); }
  });
});
