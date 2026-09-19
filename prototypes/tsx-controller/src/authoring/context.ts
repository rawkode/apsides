import { AsyncLocalStorage } from "node:async_hooks";
import type { Finalizer, Observation, ObservedResource, Output, ResourceOperations, ResourceRef } from "../contracts";
import { resourceKind, type ResourceComponent } from "./components";

export interface DeclarationContext {
  observations: Observation[];
  finalizers: { ref: ResourceRef<any>; key: string; callback: Finalizer }[];
  snapshots?: Map<string, Record<string, unknown> | undefined>;
  namespace?: string;
}

export const declarations = new AsyncLocalStorage<DeclarationContext | undefined>();
export const observationKey = (target: Pick<Observation, "kind" | "namespace" | "name">): string =>
  JSON.stringify([target.kind, target.namespace, target.name]);

export class PendingResource extends Error {
  constructor(readonly target: Observation) {
    super(`Waiting for ${target.kind} ${target.namespace}/${target.name}`);
    this.name = "PendingResource";
  }
}

export function useResourceRef<T = Record<string, unknown>>(): ResourceRef<T> {
  const ref: ResourceRef<T> = {
    output(key) {
      return {
        tag: "output", ref, path: [key],
        [Symbol.toPrimitive]() { throw new Error("An unresolved output cannot be coerced to a JavaScript value"); },
      } as Output<T[typeof key]>;
    },
  };
  return ref;
}

/** Finalizers cannot declare new hooks during execution. */
export function withResourceOperations<T>(_ops: ResourceOperations, fn: () => T): T {
  return declarations.run(undefined, fn);
}

function stringMap(value: unknown): Record<string, string> {
  if (value === undefined) return {};
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.values(value).some(item => typeof item !== "string")) {
    throw new Error("ConfigMap data must contain string values");
  }
  return { ...value } as Record<string, string>;
}

/** Observe a required, unmanaged ConfigMap. Runtime compilation waits for a snapshot. */
export function useResource(
  component: ResourceComponent,
  options: { name: string; namespace?: string },
): ObservedResource {
  const declaration = declarations.getStore();
  if (!declaration) throw new Error("useResource must be declared inside a component");
  if (!options.name) throw new Error("useResource requires a name");
  if ("access" in options) throw new Error("useResource is read-only; access cannot be overridden");
  if (component[resourceKind] !== "kubernetes:ConfigMap") throw new Error("useResource currently supports ConfigMap observations");
  const namespace = options.namespace ?? declaration.namespace;
  if (!namespace) throw new Error("useResource requires a Controller parent or an explicit namespace");
  const target: Observation = { kind: component[resourceKind], name: options.name, namespace, access: ["read"] };
  declaration.observations.push(target);
  const snapshot = declaration.snapshots?.get(observationKey(target));
  if (declaration.snapshots && (!snapshot || (snapshot.metadata as Record<string, unknown> | undefined)?.deletionTimestamp)) {
    throw new PendingResource(target);
  }
  return {
    tag: "observed", target,
    data: stringMap(snapshot?.data),
    ...(snapshot?.binaryData === undefined ? {} : { binaryData: stringMap(snapshot.binaryData) }),
  };
}

export function useFinalizer(ref: ResourceRef<any>, key: string, callback: Finalizer): void {
  const declaration = declarations.getStore();
  if (!declaration) throw new Error("useFinalizer must be declared inside a component");
  declaration.finalizers.push({ ref, key, callback });
}
