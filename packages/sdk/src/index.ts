import type {
  ActionInputV0,
  ActionOutputV0,
  AutomationDeclaration,
  Dependency,
  RenderDisposition,
} from "./abi.js";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface ResourceRef { readonly apiVersion: string; readonly kind: string; readonly namespace?: string; readonly name: string; }
export interface ResourceObject extends ResourceRef { readonly uid?: string; readonly resourceVersion?: string; readonly spec?: Json; readonly status?: Json; readonly metadata?: Record<string, Json>; }
export interface RenderContext { readonly root: ResourceObject; readonly now: string; readonly instanceId: string; }
export interface ResourceNode { readonly apiVersion: string; readonly kind: string; readonly metadata: { readonly name: string; readonly namespace?: string; readonly labels?: Record<string, string>; readonly annotations?: Record<string, string>; }; readonly spec?: Json; }
export interface ResourceObservation { readonly uid: string | null; readonly resourceVersion: string | null; }
export type ResourceDependency = Extract<Dependency, { type: "object" }>;
/** Render-only graph before the authored program decorates it as RenderOutputV0. */
export interface RenderGraph { readonly disposition: RenderDisposition; readonly resources: readonly ResourceNode[]; readonly dependencies: readonly ResourceDependency[]; readonly automations: readonly AutomationDeclaration[]; }

export interface ActionContext<T = Json> {
  /** Ergonomic authored view of ActionInputV0.eventMetadata. */
  readonly eventMetadata: T;
  readonly occurrenceId: string;
  /** The complete canonical host-owned action input. */
  readonly canonical: ActionInputV0;
}

export type ActionHandler<T = Json> = (context: ActionContext<T>) => ActionOutputV0;

export interface ProgramContext {
  readonly root: ResourceObject;
}

export function defineProgram<T extends (context: ProgramContext) => unknown>(program: T): T {
  return program;
}

export function defineAction<T>(action: ActionHandler<T>): ActionHandler<T> {
  return action;
}

export interface ResourceHandle<T extends ResourceObject = ResourceObject> { readonly ref: ResourceRef; readonly value: T | undefined; readonly present: boolean; readonly observation?: ResourceObservation; }
export type EnrichmentHook<T> = (value: T | undefined) => T | undefined;

export interface OxideLoopComponentProps { readonly children?: unknown; readonly [key: string]: unknown; }
export type OxideLoopComponent = (props: OxideLoopComponentProps) => unknown;

export function resource(node: ResourceNode): ResourceNode { return node; }

export type * from "./abi.js";
