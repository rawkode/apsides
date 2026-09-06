import * as model from "./model.ts";

declare const elementBrand: unique symbol;
declare const projectionBrand: unique symbol;
declare const referenceBrand: unique symbol;
export interface ReaktElement { readonly [elementBrand]: true }
export type ReaktNode = ReaktElement | readonly ReaktNode[] | null | undefined | boolean;
export type Component<P = Record<string, unknown>> = (props: P) => ReaktNode;
export interface DataProjection { readonly [projectionBrand]: true }
export interface ResourceRef<K extends ResourceKind = ResourceKind> {
  readonly [referenceBrand]: K;
  readonly data: K extends "ConfigMap" ? DataProjection : never;
}
export interface ObservedReference { readonly data: DataProjection }
type ResourceKind = "ConfigMap" | "Deployment" | "Service";
type LifecycleProps = { readonly deletionPolicy?: "orphan" | "delete"; readonly force?: boolean };
export type ControllerProps = Omit<model.DomainOptions, "provenance"> & { readonly children?: ReaktNode };
export type ConfigMapProps = model.ConfigMapDefinition & LifecycleProps & { readonly ref?: ResourceRef<"ConfigMap"> };
export type ServiceProps = model.ServiceDefinition & LifecycleProps & { readonly ref?: ResourceRef<"Service"> };
export type DeploymentProps = model.DeploymentDefinition & LifecycleProps & {
  readonly ref?: ResourceRef<"Deployment">;
  readonly rolloutOn?: readonly DataProjection[];
};
export interface ObserveProps { readonly id?: string; readonly name: string; readonly namespace?: string }

function builtin(): never { throw new Error("Reakt components must be rendered as JSX by the compiler"); }
export function Controller(_props: ControllerProps): ReaktElement { return builtin(); }
export function ConfigMap(_props: ConfigMapProps): ReaktElement { return builtin(); }
export function Service(_props: ServiceProps): ReaktElement { return builtin(); }
export function Deployment(_props: DeploymentProps): ReaktElement { return builtin(); }
export const Fragment = Symbol("Reakt.Fragment");

interface ElementState { readonly type: unknown; readonly props: Record<string, unknown>; readonly source?: model.SourceLocation }
const elements = new WeakMap<object, ElementState>();
interface ReferenceState {
  readonly owner: RenderState;
  readonly kind: ResourceKind;
  managed?: model.ManagedResource;
}
interface ProjectionState { readonly owner: RenderState; readonly ref?: ReferenceState; readonly observed?: model.ObservedInput }
const references = new WeakMap<object, ReferenceState>();
const projections = new WeakMap<object, ProjectionState>();
interface RenderState {
  readonly domain: model.Domain;
  readonly namespace: string;
  readonly refs: ReferenceState[];
  readonly rollouts: { target: model.ManagedResource<model.Deployment>; inputs: readonly unknown[]; source: model.SourceLocation }[];
}
interface Frame { readonly state: RenderState; readonly source: model.SourceLocation }
let frame: Frame | undefined;

function record(value: unknown, context: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error(`${context} must be plain data`);
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== null && prototype !== Object.prototype) throw new Error(`${context} must be plain data`);
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== "string" || !("value" in Object.getOwnPropertyDescriptor(value, key)!)) throw new Error(`${context} requires data properties`);
  }
  return value as Record<string, unknown>;
}

/** Factories are inert: JSX child expressions are created before their Controller. */
export function makeElement(type: unknown, props: unknown, source?: { fileName: string; lineNumber: number; columnNumber: number }): ReaktElement {
  const node = Object.freeze(Object.create(null)) as ReaktElement;
  const values = props == null ? {} : record(props, "JSX props");
  let location: model.SourceLocation | undefined;
  if (source !== undefined) {
    const raw = record(source, "JSX source");
    if (typeof raw.fileName !== "string" || !raw.fileName.startsWith("reakt:/source/") || !Number.isSafeInteger(raw.lineNumber) || (raw.lineNumber as number) < 1 || !Number.isSafeInteger(raw.columnNumber) || (raw.columnNumber as number) < 1) throw new Error("JSX source must come from the locked compiler transform");
    location = Object.freeze({ file: raw.fileName.slice("reakt:/source/".length), line: raw.lineNumber as number, column: raw.columnNumber as number });
  }
  elements.set(node, { type, props: Object.freeze({ ...values }), ...(location ? { source: location } : {}) });
  return node;
}

function requireFrame(): Frame {
  if (!frame) throw new Error("Reakt hooks require a synchronous component inside Controller");
  return frame;
}
function makeProjection(state: ProjectionState): DataProjection {
  const value = Object.freeze(Object.create(null)) as DataProjection;
  projections.set(value, state);
  return value;
}
export function useResourceRef<K extends ResourceKind>(kind: K): ResourceRef<K> {
  const { state } = requireFrame();
  if (kind !== "ConfigMap" && kind !== "Deployment" && kind !== "Service") throw new Error("unsupported resource reference kind");
  const reference: ReferenceState = { owner: state, kind };
  const value = Object.freeze(kind === "ConfigMap" ? { data: makeProjection({ owner: state, ref: reference }) } : {});
  references.set(value, reference);
  state.refs.push(reference);
  return value as ResourceRef<K>;
}
function observe(kind: typeof model.ConfigMap | typeof model.Secret, options: ObserveProps): ObservedReference {
  const { state, source } = requireFrame();
  const raw = record(options, "observation");
  for (const key of Object.keys(raw)) if (!["id", "name", "namespace"].includes(key)) throw new Error(`unsupported observation field: ${key}`);
  const supplied = { ...raw, namespace: raw.namespace ?? state.namespace, projection: ["data"], missing: "block-dependent-mutations", source } as unknown as model.ObserveOptions;
  const observed = kind === model.Secret ? state.domain.observe(model.Secret, supplied) : state.domain.observe(model.ConfigMap, supplied);
  return Object.freeze({ data: makeProjection({ owner: state, observed }) });
}
export function useConfigMap(options: ObserveProps): ObservedReference { return observe(model.ConfigMap, options); }
export function useSecret(options: ObserveProps): ObservedReference { return observe(model.Secret, options); }

function bindReference(ref: unknown, state: RenderState, managed: model.ManagedResource, kind: ResourceKind): void {
  if (ref === undefined) return;
  const value = ref !== null && typeof ref === "object" ? references.get(ref) : undefined;
  if (!value || value.owner !== state) throw new Error("foreign or invalid resource reference");
  if (value.kind !== kind) throw new Error("resource reference kind mismatch");
  if (value.managed) throw new Error("resource reference is already bound");
  value.managed = managed;
}

function snapshotNativeData(value: unknown, visiting = new Set<object>()): unknown {
  if (typeof value === "function" || typeof value === "symbol" || typeof value === "bigint") throw new Error("native resource fields require plain data, never callbacks or opaque projections");
  if (value === null || typeof value !== "object") return value;
  if (projections.has(value) || references.has(value) || elements.has(value)) throw new Error("opaque Reakt values cannot appear in native resource fields");
  if (visiting.has(value)) throw new Error("cyclic native resource data");
  const array = Array.isArray(value);
  const prototype = Object.getPrototypeOf(value);
  if (!array && prototype !== null && prototype !== Object.prototype) throw new Error("native resource fields must be plain data");
  const snapshot: object = array ? [] : Object.create(null);
  visiting.add(value);
  try {
    for (const key of Reflect.ownKeys(value)) {
      // A Proxy may change its answers between reads. Consume each descriptor once
      // and pass only this recursively detached snapshot to the model validators.
      const descriptor = Object.getOwnPropertyDescriptor(value, key);
      if (typeof key !== "string" || !descriptor || !("value" in descriptor)) throw new Error("native resource fields require data properties");
      if (!descriptor.enumerable && !(array && key === "length")) throw new Error("native resource fields require enumerable data properties");
      const child = snapshotNativeData(descriptor.value, visiting);
      Object.defineProperty(snapshot, key, { value: child, writable: true, enumerable: !(array && key === "length"), configurable: !(array && key === "length") });
    }
  } finally { visiting.delete(value); }
  return snapshot;
}

/** Compiler-only entry: author code never supplies its own provenance to Controller. */
export function renderTree(root: unknown, provenance: model.DomainOptions["provenance"]): model.ReaktIR {
  if (frame) throw new Error("nested compiler rendering is unsupported");
  let state: RenderState | undefined;
  let count = 0;
  const visiting = new Set<object>();
  function visit(node: unknown, current?: RenderState, inheritedSource?: model.SourceLocation, depth = 0): void {
    if (++count > 65_536 || depth > 128) throw new Error("component tree budget exceeded");
    if (node == null || typeof node === "boolean") return;
    if (typeof node !== "object") throw new Error("components must return Reakt JSX nodes");
    if (visiting.has(node)) throw new Error("cyclic component tree");
    visiting.add(node);
    try {
      if (Array.isArray(node)) { for (const child of node) visit(child, current, inheritedSource, depth + 1); return; }
      const element = elements.get(node);
      if (!element) throw new Error("components must return synchronous Reakt JSX nodes; promises are unsupported");
      const { type, props } = element;
      const source = element.source ?? inheritedSource;
      if (type === Fragment) { visit(props.children, current, source, depth + 1); return; }
      if (type === Controller) {
        if (state || current) throw new Error("exactly one Controller is supported; nested Controllers are unsupported");
        const { children, ...options } = props;
        if ("provenance" in options) throw new Error("Controller provenance is compiler-owned");
        const domain = model.defineDomain({ ...options, provenance } as model.DomainOptions);
        state = { domain, namespace: (options as unknown as ControllerProps).namespace, refs: [], rollouts: [] };
        visit(children, state, source, depth + 1);
        return;
      }
      if (type === ConfigMap || type === Service || type === Deployment) {
        if (!current) throw new Error("resources require Controller");
        if (!source) throw new Error("resource JSX requires compiler source information");
        const { ref, deletionPolicy, force, rolloutOn, ...definition } = props;
        const options = { source, ...(deletionPolicy === undefined ? {} : { deletionPolicy }), ...(force === undefined ? {} : { force }) } as model.ManageOptions;
        if (type !== Deployment && rolloutOn !== undefined) throw new Error("rolloutOn is supported only on Deployment");
        const desired = snapshotNativeData(definition);
        const kind = type === ConfigMap ? "ConfigMap" : type === Service ? "Service" : "Deployment";
        const resource = type === ConfigMap ? new model.ConfigMap(desired as model.ConfigMapDefinition) : type === Service ? new model.Service(desired as model.ServiceDefinition) : new model.Deployment(desired as model.DeploymentDefinition);
        const managed = current.domain.manage(resource, options);
        bindReference(ref, current, managed, kind);
        if (rolloutOn !== undefined) {
          if (!Array.isArray(rolloutOn) || rolloutOn.length === 0) throw new Error("rolloutOn requires a non-empty array of opaque data projections");
          const inputs: unknown[] = [];
          for (const key of Reflect.ownKeys(rolloutOn)) {
            if (typeof key !== "string" || !("value" in Object.getOwnPropertyDescriptor(rolloutOn, key)!)) throw new Error("rolloutOn requires data properties");
          }
          for (let index = 0; index < rolloutOn.length; index++) {
            const descriptor = Object.getOwnPropertyDescriptor(rolloutOn, String(index));
            if (!descriptor) throw new Error("rolloutOn cannot contain empty entries");
            inputs.push(descriptor.value);
          }
          current.rollouts.push({ target: managed as model.ManagedResource<model.Deployment>, inputs, source });
        }
        return;
      }
      if (typeof type !== "function") throw new Error("unsupported JSX element type");
      const previous = frame;
      try {
        frame = current && source ? { state: current, source } : undefined;
        const result = type(props);
        visit(result, current, source, depth + 1);
      } finally { frame = previous; }
    } finally { visiting.delete(node); }
  }
  visit(root);
  if (!state) throw new Error("source must render exactly one Controller");
  const completed = state as RenderState;
  for (const ref of completed.refs) if (!ref.managed) throw new Error("unbound resource reference");
  for (const rollout of completed.rollouts) {
    for (const input of rollout.inputs) {
      const projection = input !== null && typeof input === "object" ? projections.get(input) : undefined;
      if (!projection || projection.owner !== completed) throw new Error("rolloutOn requires a data projection from this Controller");
      const targetInput = projection.observed ?? projection.ref?.managed;
      if (!targetInput || (projection.ref && projection.ref.kind !== "ConfigMap")) throw new Error("rolloutOn requires ConfigMap or Secret data");
      model.watch(targetInput as model.ManagedResource<model.ConfigMap> | model.ObservedInput).project("data").rollout(rollout.target, { source: rollout.source });
    }
  }
  return completed.domain.toIR();
}
