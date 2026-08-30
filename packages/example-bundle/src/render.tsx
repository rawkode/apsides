import { Fragment, jsx } from "@oxideloop/react-like-runtime/jsx-runtime";
import { createRenderer } from "@oxideloop/react-like-runtime";
import { createHookSession, useAutomation, useResource } from "@oxideloop/sdk/hooks";
import type { HookSession } from "@oxideloop/sdk/hooks";
import type { RenderInputV0, RenderOutputV0, ResourceRef } from "@oxideloop/sdk";
import { ACTION_MODULE_IDENTITY } from "./action.js";

declare const __OXIDELOOP_ACTION_MODULE_DIGEST__: string;

const ABI = "renderer.oxideloop.dev/v0";
const PROGRAM_IDENTITY = "demo.web-application";
const AUTOMATION_VERSION = "1";

function WebApplication({ input, session }: { input: RenderInputV0; session: HookSession }) {
  const root = input.root;
  const spec = (root.spec && typeof root.spec === "object" ? root.spec : {}) as { message?: unknown; expose?: unknown };
  const observed = useResource(session, { apiVersion: "v1", kind: "ConfigMap", namespace: root.namespace, name: "demo-input" });
  const observedData = (observed.value as (typeof observed.value & { data?: { flavor?: unknown } }) | undefined)?.data;
  const flavor = observedData && typeof observedData.flavor === "string" ? observedData.flavor : "unknown";
  if (observed.observation?.uid && observed.observation.resourceVersion) {
    useAutomation(session, {
      identity: `${PROGRAM_IDENTITY}.input-changed`,
      version: AUTOMATION_VERSION,
      sourceDependency: {
        type: "object",
        apiVersion: observed.ref.apiVersion,
        kind: observed.ref.kind,
        namespace: observed.ref.namespace ?? "",
        name: observed.ref.name,
        uid: observed.observation.uid,
        resourceVersion: observed.observation.resourceVersion,
        observationToken: input.observationToken,
      },
      // Runtime-owned RenderInputV0 carries the final manifest digest, so this
      // callable never needs to contain the self-referential bundle digest.
      bundleDigest: input.bundleDigest,
      actionModuleIdentity: ACTION_MODULE_IDENTITY,
      actionModuleDigest: __OXIDELOOP_ACTION_MODULE_DIGEST__,
      rendererAbiVersion: ABI,
    });
  }
  const config = jsx("ConfigMap", { apiVersion: "v1", kind: "ConfigMap", metadata: { name: `${root.name}-config`, namespace: root.namespace }, data: { message: String(spec.message ?? "hello"), flavor } });
  const service = spec.expose === true ? jsx("Service", { apiVersion: "v1", kind: "Service", metadata: { name: `${root.name}-service`, namespace: root.namespace }, spec: { selector: { "app.kubernetes.io/name": root.name }, ports: [{ name: "http", port: 80, targetPort: 8080 }] } }) : null;
  return jsx(Fragment, { children: [config, ...(service === null ? [] : [service])] });
}

export function render(input: RenderInputV0): RenderOutputV0 {
  const session = createHookSession(environment(input));
  const rendered = createRenderer(session).render(jsx(WebApplication, { input, session }));
  return {
    abiVersion: ABI,
    disposition: rendered.disposition,
    resources: rendered.resources.map((object) => {
      const kind = object.kind;
      const name = object.metadata.name;
      return { logicalId: `${kind.toLowerCase()}/${name}`, object: object as never, readiness: null };
    }),
    dependencies: rendered.dependencies as never,
    automations: rendered.automations as never,
    diagnostics: [],
  };
}

function environment(input: RenderInputV0) {
  return { observationToken: input.observationToken, read(ref: ResourceRef) {
    const normalized = { ...ref, namespace: ref.namespace ?? input.root.namespace };
    const observation = input.observations.find((candidate) => candidate.apiVersion === normalized.apiVersion && candidate.kind === normalized.kind && candidate.namespace === normalized.namespace && candidate.name === normalized.name);
    return { ref: normalized, present: observation !== undefined, value: observation ? ({ ...normalized, ...(observation.value as object) } as never) : undefined, observation: { uid: observation?.uid ?? null, resourceVersion: observation?.resourceVersion ?? null } };
  } };
}
