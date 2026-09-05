import { ConfigMap, Controller, Deployment, Fragment, useConfigMap, useResourceRef, useSecret, type DataProjection, type DomainOptions, type ReaktIR } from "./index.ts";
import { jsxDEV } from "./jsx-dev-runtime.ts";
import { renderExport } from "./compiler.ts";

const context = { provenance: { inputs: [{ path: "app.tsx", sha256: "a".repeat(64) }] } };
const domain: Omit<DomainOptions, "provenance"> = {
  id: "demo", namespace: "workload", controlNamespace: "reakt-system", clusterBinding: "local",
  policyRef: "platform/demo", enforcement: "api-bound",
  budgets: { maxResources: 8, maxInputs: 8, maxReactions: 8, maxObjectBytes: 32_768, maxQueueDepth: 16, maxConcurrentRequests: 2 },
};
const deployment = {
  id: "api", metadata: { name: "api", namespace: "workload" },
  spec: { selector: { matchLabels: { app: "api" } }, template: { metadata: { labels: { app: "api" } }, spec: { containers: [{ name: "api", image: "test/api:v1" }] } } },
};
const config = { id: "config", metadata: { name: "config", namespace: "workload" }, data: { level: "info" } };
const element = (type: unknown, props: unknown, line = 1) => jsxDEV(type, props, undefined, false, { fileName: "reakt:/source/app.tsx", lineNumber: line, columnNumber: 1 });
const controller = (children: unknown) => element(Controller, { ...domain, children });
const compile = (node: unknown) => renderExport(node, context).productionIR as ReaktIR;
function assert(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
function throws(callback: () => unknown, expected: string) {
  try { callback(); } catch (error) { assert(error instanceof Error && error.message.includes(expected), `expected ${expected}; received ${String(error)}`); return; }
  throw new Error(`expected failure: ${expected}`);
}

let calls = 0;
function Api() {
  calls++;
  const managed = useResourceRef("ConfigMap");
  const external = useConfigMap({ id: "settings", name: "settings" });
  const secret = useSecret({ id: "credentials", name: "credentials" });
  assert(Object.isFrozen(secret.data) && Object.keys(secret.data).length === 0, "secret projection exposes no data");
  return element(Fragment, { children: [
    element(Deployment, { ...deployment, rolloutOn: [managed.data, external.data, secret.data] }, 20),
    element(ConfigMap, { ...config, ref: managed }, 30),
  ] });
}
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
}, {}))), "opaque Reakt values");
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
}, {}))), "opaque Reakt values");
throws(() => compile(controller(element(() => {
  useConfigMap({ name: "config" });
  return element(ConfigMap, config);
}, {}))), "aliases observed");
throws(() => compile(element(Controller, { ...domain, provenance: context.provenance })), "compiler-owned");
throws(() => compile(controller(element(() => { throw new Error("component failed"); }, {}))), "component failed");
throws(() => useConfigMap({ name: "after-error" }), "synchronous component");
assert(compile(controller(element(ConfigMap, config))).resources.length === 1, "failed render leaves no active frame or graph");
