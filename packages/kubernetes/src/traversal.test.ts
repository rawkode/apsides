import { ConfigMap, Controller, Deployment, Fragment, defineNamespacedResource, useConfigMap, useResourceRef, useSecret, type DataProjection, type DomainOptions, type ApsidesIR } from "./index.ts";
import { jsxDEV } from "./jsx-dev-runtime.ts";
import { renderExport } from "./compiler.ts";

const context = { provenance: { inputs: [{ path: "app.tsx", sha256: "a".repeat(64) }] } };
const domain: Omit<DomainOptions, "provenance"> = {
  id: "demo", namespace: "workload", controlNamespace: "apsides-system", clusterBinding: "local",
  policyRef: "platform/demo", enforcement: "api-bound",
  budgets: { maxResources: 8, maxInputs: 8, maxReactions: 8, maxObjectBytes: 32_768, maxQueueDepth: 16, maxConcurrentRequests: 2 },
};
const deployment = {
  id: "api", metadata: { name: "api", namespace: "workload" },
  spec: { selector: { matchLabels: { app: "api" } }, template: { metadata: { labels: { app: "api" } }, spec: { containers: [{ name: "api", image: "test/api:v1" }] } } },
};
const config = { id: "config", metadata: { name: "config", namespace: "workload" }, data: { level: "info" } };
const element = (type: unknown, props: unknown, line = 1) => jsxDEV(type, props, undefined, false, { fileName: "apsides:/source/app.tsx", lineNumber: line, columnNumber: 1 });
const controller = (children: unknown) => element(Controller, { ...domain, children });
const compile = (node: unknown) => renderExport(node, context) as ApsidesIR;
const assert: (value: unknown, message: string) => asserts value = (value, message) => { if (!value) throw new Error(message); };
const throws = (callback: () => unknown, expected: string) => {
  try { callback(); } catch (error) { assert(error instanceof Error && error.message.includes(expected), `expected ${expected}; received ${String(error)}`); return; }
  throw new Error(`expected failure: ${expected}`);
};

let calls = 0;
const Api = () => {
  calls++;
  const managed = useResourceRef("ConfigMap");
  const external = useConfigMap({ id: "settings", name: "settings" });
  const secret = useSecret({ id: "credentials", name: "credentials" });
  assert(Object.isFrozen(secret.data) && Object.keys(secret.data).length === 0, "secret projection exposes no data");
  return element(Fragment, { children: [
    element(Deployment, { ...deployment, rolloutOn: [managed.data, external.data, secret.data] }, 20),
    element(ConfigMap, { ...config, ref: managed }, 30),
  ] });
};
const root = controller(element(Api, {}, 10));
assert(calls === 0, "JSX factories must not execute components");
const ir = compile(root);
assert(Number(calls) === 1, "component executes exactly once per render");
assert(ir.resources.length === 2 && ir.inputs.length === 3 && ir.reactions.length === 3, "forward managed reference and observations lower completely");
assert(ir.resources.find(value => value.id === "config")?.source.line === 30, "resource source retains its JSX location");
assert(ir.inputs.find(value => value.id === "credentials")?.source.line === 10, "hook source explicitly uses component invocation");
assert(ir.reactions.every(value => value.source.line === 20), "rollout source is target JSX location");
assert(ir.inputs.find(value => value.id === "credentials")?.sensitive, "Secret sensitivity preserved");
assert(JSON.stringify(ir) === JSON.stringify(compile(root)), "independent renders have stable IR");
assert(!JSON.stringify(ir).includes("function"), "IR includes no callbacks");

interface ClusterSpec { readonly instances: number; readonly storage: { readonly size: string } }
const Cluster = defineNamespacedResource<ClusterSpec>({
  apiVersion: "postgresql.cnpg.io/v1",
  kind: "Cluster",
  apiResource: { plural: "clusters", scope: "Namespaced", schemaSha256: `sha256:${"b".repeat(64)}` },
});
const customIR = compile(controller(element(Cluster, {
  id: "database",
  metadata: { name: "database", namespace: "workload" },
  spec: { instances: 2, storage: { size: "1Gi" } },
  deletionPolicy: "delete",
}, 40)));
const custom = customIR.resources[0]!;
assert(custom.apiVersion === "postgresql.cnpg.io/v1" && custom.kind === "Cluster", "custom resource identity is factory-owned");
assert(custom.apiResource?.plural === "clusters" && custom.apiResource.scope === "Namespaced", "custom discovery descriptor is carried into IR");
assert((custom.desired as unknown as { spec: { instances: number } }).spec.instances === 2, "typed custom spec lowers to plain JSON");
assert(custom.source.line === 40 && custom.lifecycle.deletionPolicy === "delete", "custom JSX preserves source and lifecycle controls");
throws(() => defineNamespacedResource({
  apiVersion: "postgresql.cnpg.io/v1",
  kind: "Cluster",
  apiResource: { plural: "clusters", scope: "Namespaced", schemaSha256: "sha256:bad" },
}), "lowercase sha256 digest");
throws(() => compile(controller(element(Cluster, {
  metadata: { name: "database", namespace: "workload" },
  apiVersion: "forged/v1",
  spec: { instances: 2, storage: { size: "1Gi" } },
}))), "unsupported field apiVersion");
throws(() => compile(controller(element(Cluster, {
  metadata: { name: "database", namespace: "workload" },
  spec: new Date(),
}))), "plain data");

throws(() => useSecret({ name: "no-frame" }), "synchronous component");
throws(() => compile(controller(element(async () => null, {}))), "promises are unsupported");
throws(() => compile(controller(controller(null))), "nested Controllers");
throws(() => compile([controller(null), controller(null)]), "exactly one Controller");
throws(() => compile(element(ConfigMap, config)), "require Controller");
throws(() => compile(controller(element(() => { useResourceRef("ConfigMap"); return null; }, {}))), "unbound resource reference");
throws(() => compile(controller(element(() => {
  const ref = useResourceRef("ConfigMap");
  return [element(ConfigMap, { ...config, ref }), element(ConfigMap, { ...config, id: "other", metadata: { name: "other", namespace: "workload" }, ref })];
}, {}))), "already bound");
throws(() => compile(controller(element(() => {
  const ref = useResourceRef("Deployment");
  return element(ConfigMap, { ...config, ref });
}, {}))), "kind mismatch");
let foreign: DataProjection;
compile(controller(element(() => { foreign = useSecret({ name: "secret" }).data; return null; }, {})));
throws(() => compile(controller(element(Deployment, { ...deployment, rolloutOn: [foreign!] }))), "this Controller");
throws(() => compile(controller(element(Deployment, { ...deployment, rolloutOn: [() => null] }))), "data projection");
throws(() => compile(controller(element(Deployment, { ...deployment, rolloutOn: [{ data: "private" }] }))), "data projection");
throws(() => compile(controller(element(ConfigMap, { ...config, onChange: () => null }))), "plain data");
throws(() => compile(controller(element(ConfigMap, { ...config, unsupported: "field" }))), "unsupported");
throws(() => compile(controller(element(() => {
  const secret = useSecret({ name: "secret" });
  return element(ConfigMap, { ...config, data: secret.data });
}, {}))), "opaque Apsides values");
const hiddenData = {};
Object.defineProperty(hiddenData, "hidden", { value: "materialized", enumerable: false });
throws(() => compile(controller(element(ConfigMap, { ...config, data: hiddenData }))), "enumerable data properties");
const hiddenContainers = [{ name: "api", image: "test/api:v1" }];
Object.defineProperty(hiddenContainers, "0", { enumerable: false });
throws(() => compile(controller(element(Deployment, {
  ...deployment,
  spec: { ...deployment.spec, template: { ...deployment.spec.template, spec: { containers: hiddenContainers } } },
}))), "enumerable data properties");
let accessorReads = 0;
const accessor = { get password() { accessorReads++; return "private"; } };
throws(() => compile(controller(element(ConfigMap, { ...config, data: accessor }))), "data properties");
assert(accessorReads === 0, "native data accessors must not execute");
throws(() => element(ConfigMap, { get metadata() { accessorReads++; return config.metadata; } }), "data properties");
assert(accessorReads === 0, "JSX props accessors must not execute");
const accessorInputs: unknown[] = [];
Object.defineProperty(accessorInputs, "0", { get() { accessorReads++; return foreign; } });
throws(() => compile(controller(element(Deployment, { ...deployment, rolloutOn: accessorInputs }))), "data properties");
assert(accessorReads === 0, "rollout projection accessors must not execute");
let proxyReads = 0;
const proxyIR = compile(controller(element(() => {
  const secret = useSecret({ name: "proxy-secret" });
  const data = new Proxy({ leak: "safe" }, {
    get(_target, key) { if (key === "leak") return ++proxyReads === 1 ? "safe" : secret.data; return undefined; },
  });
  return element(ConfigMap, { ...config, data });
}, {})));
assert(proxyReads === 0, "native snapshots never invoke Proxy get traps");
assert(JSON.stringify(proxyIR.resources[0]?.desired).includes('"leak":"safe"'), "native constructors consume the descriptor snapshot, never a later Proxy answer");
assert(!JSON.stringify(proxyIR.resources[0]?.desired).includes('"leak":{}'), "opaque projections cannot be smuggled through a changing Proxy");
throws(() => compile(controller(element(() => {
  const secret = useSecret({ name: "descriptor-secret" });
  const data = new Proxy({ leak: "safe" }, {
    getOwnPropertyDescriptor() { return { value: secret.data, enumerable: true, configurable: true, writable: true }; },
  });
  return element(ConfigMap, { ...config, data });
}, {}))), "opaque Apsides values");
throws(() => compile(controller(element(() => {
  useConfigMap({ name: "config" });
  return element(ConfigMap, config);
}, {}))), "aliases observed");
throws(() => compile(element(Controller, { ...domain, provenance: context.provenance })), "compiler-owned");
throws(() => compile(controller(element(() => { throw new Error("component failed"); }, {}))), "component failed");
throws(() => useConfigMap({ name: "after-error" }), "synchronous component");
assert(compile(controller(element(ConfigMap, config))).resources.length === 1, "failed render leaves no active frame or graph");

const waddleDeployment = {
  metadata: { name: "waddle-server", namespace: "workload" },
  spec: { selector: { matchLabels: { app: "waddle-server" } }, template: { metadata: { labels: { app: "waddle-server" } }, spec: { containers: [{ name: "server", image: "test/waddle:v1" }] } } },
};
const Waddle = () => {
  const keypool = useSecret({ name: "waddle-clustering-keypool" });
  const serverConfig = useResourceRef("ConfigMap");
  return element(Fragment, { children: [
    element(ConfigMap, { metadata: { name: "waddle-server-config", namespace: "workload" }, data: { level: "info" }, ref: serverConfig }, 50),
    element(Deployment, { ...waddleDeployment, rolloutOn: [keypool.data, serverConfig.data] }, 60),
  ] });
};
const waddleIR = compile(controller(element(Waddle, {}, 45)));
const waddleRollouts = waddleIR.reactions.map((reaction) => reaction.id).sort();
assert(
  JSON.stringify(waddleRollouts) === JSON.stringify([
    "rollout-managed-configmap-waddle-server-config-deplo-56bc032ca9",
    "rollout-observed-secret-waddle-clustering-keypool-de-cb182e11f4",
  ]),
  `over-long derived rollout ids are bounded with a hash suffix; received ${waddleRollouts.join(",")}`,
);
assert(waddleRollouts.every((id) => id.length <= 63 && /^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(id)), "bounded derived ids are DNS labels");
assert(JSON.stringify(waddleIR.resources.map((resource) => resource.id).sort()) === JSON.stringify(["configmap-waddle-server-config", "deployment-waddle-server"]), "short derived resource ids are unchanged");
assert(JSON.stringify(waddleIR.inputs.map((input) => input.id).sort()) === JSON.stringify(["managed-configmap-waddle-server-config", "observed-secret-waddle-clustering-keypool"]), "short derived input ids are unchanged");
assert(waddleIR.reactions.every((reaction) => waddleIR.inputs.some((input) => input.id === reaction.inputs[0]) && reaction.target === "deployment-waddle-server"), "bounded reactions reference their inputs and target");
assert(JSON.stringify(waddleIR) === JSON.stringify(compile(controller(element(Waddle, {}, 45)))), "bounded derived ids are deterministic");
