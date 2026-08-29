import { useResource, withHookEnvironment } from "@oxideloop/sdk/hooks";
import { explicitEmpty, Fragment, jsx, type Element } from "./jsx-runtime.js";
import { createRenderer, RenderTreeError } from "./runtime.js";

const element = jsx("Deployment", {
  apiVersion: "apps/v1",
  kind: "Deployment",
  metadata: { name: "api" },
  spec: { replicas: 2 },
});
const output = createRenderer().render(element);

if (output.disposition !== "apply") {
  throw new Error("a non-empty graph must use apply disposition");
}
if (output.resources.length !== 1) {
  throw new Error("the React-like renderer must emit exactly one resource");
}
if (output.resources[0]?.metadata.name !== "api") {
  throw new Error("the React-like renderer lost the resource identity");
}
let outsideRenderRejected = false;
try {
  useResource({ apiVersion: "v1", kind: "ConfigMap", name: "settings" });
} catch {
  outsideRenderRejected = true;
}
if (!outsideRenderRejected) {
  throw new Error("tracked hooks must fail outside an explicit render environment");
}

const observed = withHookEnvironment(
  {
    read: (ref) => ({ ref, value: undefined, present: false }),
  },
  () => useResource({ apiVersion: "v1", kind: "Service", name: "api" }),
);
if (observed.present) {
  throw new Error("the tracked hook result must come from the supplied environment");
}

const empty = createRenderer().render(explicitEmpty());
if (empty.disposition !== "explicitEmpty" || empty.resources.length !== 0) {
  throw new Error("only explicitEmpty() may authorize an empty graph");
}

for (const unsafe of [
  jsx(() => null, {}),
  false as unknown as Element,
  7 as unknown as Element,
  jsx(Fragment, { children: [] }),
  jsx("Deployment", { kind: "Deployment", metadata: { name: "missing-api-version" } }),
  { type: 7, props: {} } as Element,
  jsx(Fragment, { children: [explicitEmpty(), element] }),
]) {
  let rejected = false;
  try {
    createRenderer().render(unsafe);
  } catch (error) {
    rejected = error instanceof RenderTreeError;
  }
  if (!rejected) {
    throw new Error("unsafe or ambiguous render trees must fail closed");
  }
}
