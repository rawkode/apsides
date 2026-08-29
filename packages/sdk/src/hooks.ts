import type { ReadHook, ResourceDependency, ResourceHandle, ResourceObject, ResourceRef } from "./index.js";

export interface HookEnvironment { read<T extends ResourceObject>(ref: ResourceRef): ResourceHandle<T>; }
let current: HookEnvironment | undefined;
const dependencies = new Map<string, ResourceDependency>();
export function withHookEnvironment<T>(environment: HookEnvironment, render: () => T): T {
  const previous = current; current = environment;
  try { return render(); } finally { current = previous; }
}
export function currentDependencies(): readonly ResourceDependency[] { return [...dependencies.values()].sort((a, b) => dependencyKey(a) < dependencyKey(b) ? -1 : dependencyKey(a) > dependencyKey(b) ? 1 : 0); }
export function resetDependencies(): void { dependencies.clear(); }
function environment(): HookEnvironment { if (!current) throw new Error("OxideLoop hook called outside a render phase"); return current; }
function dependencyKey(dependency: ResourceDependency | ResourceRef): string { return JSON.stringify([dependency.apiVersion, dependency.kind, dependency.namespace ?? "", dependency.name]); }
export const useResource: ReadHook = <T extends ResourceObject>(ref: ResourceRef) => {
  const handle = environment().read<T>(ref);
  const observation = handle.observation ?? { uid: handle.value?.uid ?? null, resourceVersion: handle.value?.resourceVersion ?? null };
  const dependency: ResourceDependency = { type: "object", apiVersion: handle.ref.apiVersion, kind: handle.ref.kind, namespace: handle.ref.namespace ?? "", name: handle.ref.name, uid: observation.uid, resourceVersion: observation.resourceVersion };
  dependencies.set(dependencyKey(dependency), dependency);
  return handle;
};
export function useEnriched<T>(value: T | undefined, enrich: (value: T) => T): T | undefined { return value === undefined ? undefined : enrich(value); }
