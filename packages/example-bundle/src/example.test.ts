import { renderController } from "@oxideloop/kubernetes";
import controller from "./controller.js";

const baseInput = {
  abiVersion: "renderer.oxideloop.dev/v0",
  bundleDigest: "0".repeat(64),
  cacheEpoch: "1",
  observationToken: "token",
  root: {
    apiVersion: "demo.oxideloop.dev/v1alpha1",
    kind: "WebApplication",
    namespace: "demo",
    name: "portal",
    uid: "root-uid",
    generation: 1,
    spec: { image: "example/web:v1", replicas: 2, expose: true },
  },
  observations: [],
};

function render(input = baseInput) {
  return renderController(controller, input);
}

interface RenderedObject {
  readonly metadata?: { readonly name?: string };
  readonly data?: { readonly message?: string };
  readonly spec?: {
    readonly replicas?: number;
    readonly template?: { readonly spec?: { readonly containers?: ReadonlyArray<{ readonly image?: string }> } };
    readonly ports?: ReadonlyArray<{ readonly targetPort?: string | number }>;
  };
}

function resource(output: ReturnType<typeof render>, logicalId: string) {
  const found = output.resources.find((candidate) => candidate.logicalId === logicalId);
  if (!found) throw new Error(`missing ${logicalId} resource`);
  return found.object as RenderedObject;
}

const exposed = render();
if (exposed.resources.length !== 3) throw new Error("exposed controller must render three resources");
const config = resource(exposed, "config");
const deployment = resource(exposed, "deployment");
const service = resource(exposed, "service");
if (config.metadata?.name !== "portal-config" || config.data?.message !== "Hello from OxideLoop") {
  throw new Error("controller did not render the expected fallback ConfigMap");
}
if (deployment.spec?.replicas !== 2 || deployment.spec?.template?.spec?.containers?.[0]?.image !== "example/web:v1") {
  throw new Error("controller did not render the expected Deployment");
}
if (service.metadata?.name !== "portal-service" || service.spec?.ports?.[0]?.targetPort !== "http") {
  throw new Error("controller did not render the expected Service");
}
if (exposed.dependencies.length !== 1 || (exposed.dependencies[0] as { name?: string } | undefined)?.name !== "demo-input") {
  throw new Error("controller did not retain the demo-input dependency");
}
if (exposed.automations.length !== 0) throw new Error("absent demo-input must not create an automation");

const hiddenService = render({
  ...baseInput,
  root: { ...baseInput.root, spec: { ...baseInput.root.spec, expose: false } },
});
if (hiddenService.resources.some((candidate) => candidate.logicalId === "service")) {
  throw new Error("expose=false must omit the Service");
}

const zeroReplicas = render({
  ...baseInput,
  root: { ...baseInput.root, spec: { ...baseInput.root.spec, replicas: 0 } },
});
if (resource(zeroReplicas, "deployment").spec?.replicas !== 0) {
  throw new Error("replicas=0 must remain zero");
}

// The action descriptor stays private to controller.tsx. Bundle-builder tests
// execute it through the same compiler binding used by the renderer.
