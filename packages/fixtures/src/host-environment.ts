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
      const value = observation === undefined
        ? undefined
        : ({ ...normalizedRef, ...(observation.value as object) } as T);
      return {
        ref: normalizedRef,
        value,
        present: observation !== undefined,
        observation: {
          uid: observation?.uid ?? null,
          resourceVersion: observation?.resourceVersion ?? null,
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
