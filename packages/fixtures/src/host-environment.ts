import type {
  HookEnvironment,
} from "@oxideloop/sdk/hooks";
import type {
  JsonValue,
  Diagnostic,
  Dependency,
  RenderDisposition,
  RenderInputV0,
  ResourceHandle,
  ResourceObject,
  ResourceRef,
} from "@oxideloop/sdk";

export function hookEnvironment(input: RenderInputV0): HookEnvironment {
  return {
    observationToken: input.observationToken,
    read: <T extends ResourceObject>(ref: ResourceRef): ResourceHandle<T> => {
      const normalizedRef = {
        ...ref,
        namespace: ref.namespace ?? input.root.namespace,
      };
      const observation = input.observations.find(
        (candidate) =>
          candidate.apiVersion === normalizedRef.apiVersion &&
          candidate.kind === normalizedRef.kind &&
          candidate.name === normalizedRef.name &&
          candidate.namespace === normalizedRef.namespace,
      );
      // A retained tombstone is a negative observation, not a present
      // resource. Its final UID/RV must not leak into hook dependency identity.
      const present = observation !== undefined && observation.value !== null;
      const value = !present
        ? undefined
        : ({ ...normalizedRef, ...(observation?.value as object) } as T);
      return {
        ref: normalizedRef,
        value,
        present,
        observation: {
          uid: present ? observation?.uid ?? null : null,
          resourceVersion: present ? observation?.resourceVersion ?? null : null,
        },
      };
    },
  };
}

export function renderOutput(
  resources: readonly JsonValue[],
  dependencies: readonly Dependency[] = [],
  diagnostics: readonly Diagnostic[] = [],
  disposition: RenderDisposition = resources.length === 0 ? "hold" : "apply",
) {
  return {
    abiVersion: "renderer.oxideloop.dev/v0" as const,
    disposition,
    resources: resources.map((object, index) => ({
      logicalId: `resource-${index}`,
      object,
      readiness: null,
    })),
    dependencies: [...dependencies],
    automations: [],
    diagnostics: [...diagnostics],
  };
}
