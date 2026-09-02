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
declare const resourceDescriptorType: unique symbol;

/** A typed Kubernetes object descriptor accepted by the authoring hooks. */
export interface ResourceDescriptor<T extends ResourceObject = ResourceObject> {
  readonly apiVersion: string;
  readonly kind: string;
  readonly [resourceDescriptorType]?: T;
}

/** The intentionally small object lookup surface available to authored code. */
export interface ResourceRequest {
  readonly name: string;
  readonly namespace?: string;
}

export interface ConfigMapCurrent extends ResourceObject {
  readonly data?: Readonly<Record<string, string>>;
}

/**
 * A convenience descriptor for the built-in Kubernetes ConfigMap type.
 * Semantic component packages may re-export this as their `ConfigMap` source.
 */
export const ConfigMapDescriptor: ResourceDescriptor<ConfigMapCurrent> = Object.freeze({
  apiVersion: "v1",
  kind: "ConfigMap",
});

export function defineResource<T extends ResourceObject>(
  apiVersion: string,
  kind: string,
): ResourceDescriptor<T> {
  if (typeof apiVersion !== "string" || apiVersion.length === 0 ||
      typeof kind !== "string" || kind.length === 0) {
    throw new Error("resource descriptors require nonempty apiVersion and kind");
  }
  return Object.freeze({ apiVersion, kind });
}
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

declare const actionDescriptorType: unique symbol;
declare const compilerActionBindingsType: unique symbol;

/** The exact observed object delivered to object-form action code. */
export interface ActionSource {
  readonly apiVersion: string;
  readonly kind: string;
  readonly namespace: string;
  readonly name: string;
  readonly uid: string;
  readonly resourceVersion: string;
}

export interface AuthoredActionContext<Metadata extends Json = Json> {
  readonly source: ActionSource;
  readonly metadata: DeepReadonly<Metadata>;
}

export interface ActionDefinition<Metadata extends Json = Json> {
  readonly id: string;
  readonly version: string;
  readonly run: (context: AuthoredActionContext<Metadata>) => ActionOutputV0;
}

/** A branded action descriptor. Its compiler bindings never live on this object. */
export interface ActionDescriptor<Metadata extends Json = Json> {
  readonly id: string;
  readonly version: string;
  readonly run: (context: AuthoredActionContext<Metadata>) => ActionOutputV0;
  readonly [actionDescriptorType]: never;
}

/** Compiler-owned values that are intentionally absent from authored action descriptors. */
export interface CompilerActionBinding {
  readonly bundleDigest: string;
  readonly actionModuleIdentity: string;
  readonly actionModuleDigest: string;
}

export interface CompilerActionBindingEntry extends CompilerActionBinding {
  readonly action: ActionDescriptor<never>;
}

/** Opaque registry injected by the compiler/runtime into one hook environment. */
export interface CompilerActionBindings {
  readonly [compilerActionBindingsType]: never;
}

const definedActions = new WeakSet<object>();
const compilerBindingRegistries = new WeakSet<object>();
const compilerBindingEntries = new WeakMap<object, WeakMap<object, CompilerActionBinding>>();

export function defineAction<T>(action: ActionHandler<T>): ActionHandler<T>;
export function defineAction<Metadata extends Json>(action: ActionDefinition<Metadata>): ActionDescriptor<Metadata>;
export function defineAction(action: unknown): unknown {
  if (typeof action === "function") return action;
  if (!isActionDefinition(action)) throw new Error("defineAction() requires id, version, and run");
  const descriptor = Object.freeze({ id: action.id, version: action.version, run: action.run }) as ActionDescriptor;
  definedActions.add(descriptor);
  return descriptor;
}

/**
 * Creates the opaque compiler/runtime registry used to bind an authored action
 * descriptor to its installed module and bundle. The registry is verified by
 * identity and cannot be replaced with an authored lookalike object.
 */
export function createCompilerActionBindings(
  entries: readonly CompilerActionBindingEntry[],
): CompilerActionBindings {
  const table = new WeakMap<object, CompilerActionBinding>();
  for (const entry of entries) {
    if (!isDefinedAction(entry.action) || !isCompilerActionBinding(entry)) {
      throw new Error("compiler action bindings require defined actions and canonical digests");
    }
    if (table.has(entry.action)) throw new Error(`duplicate compiler action binding: ${entry.action.id}`);
    table.set(entry.action, Object.freeze({
      bundleDigest: entry.bundleDigest,
      actionModuleIdentity: entry.actionModuleIdentity,
      actionModuleDigest: entry.actionModuleDigest,
    }));
  }
  const registry = Object.freeze(Object.create(null)) as CompilerActionBindings;
  compilerBindingRegistries.add(registry);
  compilerBindingEntries.set(registry, table);
  return registry;
}

export function compilerActionBindingFor(
  registry: CompilerActionBindings | undefined,
  action: ActionDescriptor<never>,
): CompilerActionBinding | undefined {
  if (registry === undefined) return undefined;
  if (!compilerBindingRegistries.has(registry)) throw new Error("unrecognized compiler action binding registry");
  return compilerBindingEntries.get(registry)?.get(action);
}

export function isDefinedAction(action: unknown): action is ActionDescriptor<never> {
  return action !== null && typeof action === "object" && definedActions.has(action);
}

function isActionDefinition(value: unknown): value is ActionDefinition {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  if (!hasExactDataFields(value, ["id", "run", "version"])) return false;
  const candidate = value as Record<string, unknown>;
  return isNonemptyText(candidate.id) && isNonemptyText(candidate.version) && typeof candidate.run === "function";
}

function hasExactDataFields(value: object, expected: readonly string[]): boolean {
  const keys = Reflect.ownKeys(value);
  if (keys.some(key => typeof key !== "string") || keys.length !== expected.length) return false;
  const descriptors = Object.getOwnPropertyDescriptors(value);
  return expected.every((key) => {
    const descriptor = descriptors[key];
    return descriptor !== undefined && descriptor.enumerable && "value" in descriptor;
  });
}

function isCompilerActionBinding(value: CompilerActionBinding): boolean {
  return isRawDigest(value.bundleDigest) && isNonemptyText(value.actionModuleIdentity) && isRawDigest(value.actionModuleDigest);
}

function isNonemptyText(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function isRawDigest(value: string): boolean {
  return /^[0-9a-f]{64}$/.test(value);
}

export interface ProgramContext {
  readonly root: ResourceObject;
}

/** The entire root surface available to a controller render. */
export interface ControllerRoot<Spec> {
  readonly metadata: {
    readonly name: string;
    readonly namespace: string;
  };
  readonly spec: Spec;
}

export interface ControllerContext<Spec> {
  readonly root: ControllerRoot<Spec>;
}

export type Controller<Spec, Output = unknown> = (context: ControllerContext<Spec>) => Output;

export function defineProgram<T extends (context: ProgramContext) => unknown>(program: T): T {
  return program;
}

/**
 * Marks a pure, synchronously-dispatched controller render. The host owns the
 * root projection and enters the hook context; authored code receives only
 * name, namespace, and its plain declared spec.
 */
export function defineController<Spec, Output = unknown>(
  controller: Controller<Spec, Output>,
): Controller<Spec, Output> {
  return controller;
}

export interface ResourceHandle<T extends ResourceObject = ResourceObject> { readonly ref: ResourceRef; readonly value: T | undefined; readonly present: boolean; readonly observation?: ResourceObservation; }
export type EnrichmentHook<T> = (value: T | undefined) => T | undefined;

export interface OxideLoopComponentProps { readonly children?: unknown; readonly [key: string]: unknown; }
export type OxideLoopComponent = (props: OxideLoopComponentProps) => unknown;

export type DeepReadonly<T> =
  T extends readonly (infer Item)[] ? readonly DeepReadonly<Item>[] :
  T extends object ? { readonly [Key in keyof T]: DeepReadonly<T[Key]> } :
  T;

/**
 * A render-local wrapper component carrying an immutable strict-JSON config
 * snapshot. Rendering packages consume `config`; callers may not mutate it.
 */
export interface EnrichmentComponent<Config extends Json = Json> extends OxideLoopComponent {
  readonly config: DeepReadonly<Config>;
}

export function resource(node: ResourceNode): ResourceNode { return node; }

export {
  createHookSession,
  useEnriched,
  useEnrichment,
  useAutomation,
  useResource,
  withHookEnvironment,
} from "./hooks.js";
export type {
  HookEnvironment,
  HookSession,
  AutomationOptions,
  ResourceChange,
  ResourceRead,
} from "./hooks.js";

export type * from "./abi.js";
