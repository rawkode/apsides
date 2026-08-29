export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface ResourceRef { readonly apiVersion: string; readonly kind: string; readonly namespace?: string; readonly name: string; }
export interface ResourceObject extends ResourceRef { readonly uid?: string; readonly resourceVersion?: string; readonly spec?: Json; readonly status?: Json; readonly metadata?: Record<string, Json>; }
export interface RenderContext { readonly root: ResourceObject; readonly now: string; readonly instanceId: string; }
export interface ResourceNode { readonly apiVersion: string; readonly kind: string; readonly metadata: { readonly name: string; readonly namespace?: string; readonly labels?: Record<string, string>; readonly annotations?: Record<string, string>; }; readonly spec?: Json; }
export interface ResourceObservation { readonly uid: string | null; readonly resourceVersion: string | null; }
export type ResourceDependency = Extract<import("./abi.js").Dependency, { type: "object" }>;
export interface RenderOutput { readonly disposition: import("./abi.js").RenderDisposition; readonly resources: readonly ResourceNode[]; readonly dependencies: readonly ResourceDependency[]; }

export interface ResourceHandle<T extends ResourceObject = ResourceObject> { readonly ref: ResourceRef; readonly value: T | undefined; readonly present: boolean; readonly observation?: ResourceObservation; }
export type ReadHook = <T extends ResourceObject = ResourceObject>(ref: ResourceRef) => ResourceHandle<T>;
export type EnrichmentHook<T> = (value: T | undefined) => T | undefined;

export interface OxideLoopComponentProps { readonly children?: unknown; readonly [key: string]: unknown; }
export type OxideLoopComponent = (props: OxideLoopComponentProps) => unknown;

export function resource(node: ResourceNode): ResourceNode { return node; }

export type * from "./abi.js";
