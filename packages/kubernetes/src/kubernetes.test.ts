import { ConfigMap, Data, Deployment, Container, Env, Event, Port, Resources, Service, defineAction, explicitEmpty, jsx, renderAction, renderController, useAutomation, useEnrichment, useResource } from "./index.js";
import type { RenderInputV0 } from "@oxideloop/sdk";
import { createCompilerActionBindings } from "@oxideloop/sdk";

const input: RenderInputV0 = {
  abiVersion: "renderer.oxideloop.dev/v0", bundleDigest: "0".repeat(64), cacheEpoch: "1", observationToken: "token",
  root: { apiVersion: "demo.oxideloop.dev/v1alpha1", kind: "WebApplication", namespace: "demo", name: "portal", uid: "root", generation: 1, spec: { image: "example/web:v1" } }, observations: [],
};
const output = renderController(() => jsx(ConfigMap, { id: "config", name: "portal-config", children: jsx(Data, { name: "message", value: "hello" }) }), input);
const configObject = output.resources[0]?.object as { metadata?: { namespace?: string } } | undefined;
if (output.resources[0]?.logicalId !== "config" || configObject?.metadata?.namespace !== "demo") throw new Error("semantic ConfigMap lowering failed");
const tree = jsx(Deployment, { id: "deployment", name: "portal", selector: { app: "portal" }, children: jsx(Container, { name: "web", image: "example/web:v1", children: jsx(Port, { name: "http", containerPort: 8080 }) }) });
const service = jsx(Service, { id: "service", name: "portal-service", selector: { app: "portal" }, children: jsx(Port, { name: "http", port: 80, targetPort: "http" }) });
const combined = renderController(() => [tree, service], input);
if (combined.resources.length !== 2 || combined.resources[0]?.logicalId !== "deployment") throw new Error("semantic resource sorting failed");

const binding = { bundleDigest: "a".repeat(64), actionModuleIdentity: "configuration-changed", actionModuleDigest: "b".repeat(64) };
const changed = defineAction<{ message: string }>({
  id: "configuration-changed", version: "v1",
  run: ({ source, metadata }) => jsx(Event, { id: "notification", regarding: source, reason: "ConfigurationChanged", action: "ConfigurationChanged", note: metadata.message }),
});
const bindings = createCompilerActionBindings([{ action: changed as never, ...binding }]);
const canonical = renderController(({ root }) => {
  const settings = useResource(ConfigMap, { name: "demo-input" });
  const Defaults = useEnrichment({ labels: { app: root.metadata.name }, podSecurityContext: { runAsNonRoot: true } });
  useAutomation({ when: settings.changed(), action: changed, metadata: { message: settings.current?.data?.message ?? "fallback" } });
  return jsx(Defaults, { children: [
    jsx(ConfigMap, { id: "config", name: `${root.metadata.name}-config`, children: jsx(Data, { name: "message", value: settings.current?.data?.message ?? "fallback" }) }),
    jsx(Deployment, { id: "deployment", name: root.metadata.name, replicas: 2, selector: { app: root.metadata.name }, children: jsx(Container, { name: "web", image: "example/web:v1", children: [jsx(Port, { name: "http", containerPort: 8080 }), jsx(Env, { name: "MESSAGE", configMap: { name: `${root.metadata.name}-config`, key: "message" } }), jsx(Resources, { requests: { cpu: "10m", memory: "32Mi" }, limits: { memory: "64Mi" } })] }) }),
    jsx(Service, { id: "service", name: `${root.metadata.name}-service`, selector: { app: root.metadata.name }, children: jsx(Port, { name: "http", port: 80, targetPort: "http" }) }),
  ] });
}, { ...input, observations: [{ apiVersion: "v1", kind: "ConfigMap", namespace: "demo", name: "demo-input", uid: "input-uid", resourceVersion: "9", value: { data: { message: "from-input" } } }] }, bindings);
if (canonical.resources.length !== 3 || canonical.resources.some((resource) => resource.object !== null && typeof resource.object === "object" && "__logicalId" in resource.object) || canonical.resources[0]?.logicalId !== "deployment") throw new Error("canonical semantic resources did not lower deterministically");
if (canonical.dependencies.length !== 1 || (canonical.dependencies[0] as { uid?: string } | undefined)?.uid !== "input-uid" || canonical.automations.length !== 1) throw new Error("one hook session did not retain exact dependencies and automation");

const actionInput = {
  abiVersion: "renderer.oxideloop.dev/v0", bundleDigest: binding.bundleDigest, automationIdentity: changed.id, automationVersion: changed.version,
  actionModuleIdentity: binding.actionModuleIdentity, actionModuleDigest: binding.actionModuleDigest, occurrenceId: "occurrence", sourceTransitionId: "transition", sourceUid: "input-uid", sourceResourceVersion: "9",
  eventMetadata: canonical.automations[0]!.eventMetadata,
};
const actionOutput = renderAction(changed, actionInput);
if (actionOutput.actions[0]?.logicalId !== "notification/occurrence" || (actionOutput.actions[0]?.specification as { regarding?: { name?: string } }).regarding?.name !== "demo-input") throw new Error("Event action did not lower to the exact source-bound output");
if (renderController(() => explicitEmpty(), input).disposition !== "explicitEmpty") throw new Error("explicitEmpty did not authorize the empty graph");
const omitted = renderController(() => jsx(ConfigMap, { id: "omitted", name: "omitted", children: [null, false, undefined, jsx(Data, { name: "present", value: "yes" })] }), input);
if ((omitted.resources[0]?.object as { data?: { present?: string } }).data?.present !== "yes") throw new Error("null/false/undefined descendants must omit");
const aggregate = "x".repeat(200_000);
renderController(() => explicitEmpty(), {
  ...input,
  root: { ...input.root, spec: { chunks: [aggregate, aggregate, aggregate] } },
});
let oversizedDocumentRejected = false;
try {
  renderController(() => explicitEmpty(), {
    ...input,
    root: { ...input.root, spec: { chunks: Array.from({ length: 11 }, () => aggregate) } },
  });
} catch {
  oversizedDocumentRejected = true;
}
if (!oversizedDocumentRejected) throw new Error("aggregate JSON above 2 MiB must be rejected");
const cyclic = jsx(ConfigMap, { id: "cycle", name: "cycle" }) as { props: { children?: unknown } };
cyclic.props.children = cyclic;
let tooDeep: unknown = jsx(ConfigMap, { id: "deep", name: "deep" });
for (let index = 0; index < 65; index += 1) tooDeep = [tooDeep];
const hiddenActionMetadata = { ...(actionInput.eventMetadata as object) } as { readonly $oxideloop: unknown; readonly hidden?: string };
Object.defineProperty(hiddenActionMetadata, "hidden", { enumerable: false, value: "no" });
for (const invalid of [
  () => renderController(() => null, input),
  () => renderController(() => jsx(ConfigMap, { id: "dup", name: "a", children: [jsx(Data, { name: "x", value: "1" }), jsx(Data, { name: "x", value: "2" })] }), input),
  () => renderController(() => jsx(Deployment, { id: "bad", name: "bad", selector: { app: "bad" }, children: true }), input),
  () => renderController(() => jsx(Deployment, { id: "resources", name: "resources", selector: { app: "resources" }, children: jsx(Container, { name: "web", image: "web", children: [jsx(Resources, { requests: { cpu: "1" }, limits: { memory: "1Mi" } }), jsx(Resources, { requests: { cpu: "1" }, limits: { memory: "1Mi" } })] }) }), input),
  () => renderController(() => jsx(ConfigMap, { id: "primitive", name: "primitive", children: 7 as never }), input),
  () => renderController(() => cyclic as never, input),
  () => renderController(() => tooDeep as never, input),
  () => renderController(() => [jsx(ConfigMap, { id: "one", name: "same" }), jsx(ConfigMap, { id: "two", name: "same" })], input),
  () => renderAction(changed, { ...actionInput, sourceUid: "retarget" }),
  () => renderAction(changed, { ...actionInput, eventMetadata: hiddenActionMetadata as never }),
]) {
  let rejected = false; try { invalid(); } catch { rejected = true; }
  if (!rejected) throw new Error("semantic/action boundary accepted an invalid graph or retarget");
}
