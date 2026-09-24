import * as model from "./model.ts";

declare const elementBrand: unique symbol;
declare const projectionBrand: unique symbol;
declare const referenceBrand: unique symbol;
declare const changeEventBrand: unique symbol;
declare const handlerOutputBrand: unique symbol;
export interface ApsidesElement { readonly [elementBrand]: true }
export type ApsidesNode = ApsidesElement | readonly ApsidesNode[] | null | undefined | boolean;
export type Component<P = Record<string, unknown>> = (props: P) => ApsidesNode;
export interface DataProjection { readonly [projectionBrand]: true }
/** Fires when its backing input's revision changes; usable only as an `<On trigger>`. */
export interface ChangeEvent { readonly [changeEventBrand]: true }
export interface ResourceRef<K extends ResourceKind = ResourceKind> {
  readonly [referenceBrand]: K;
  readonly data: K extends "ConfigMap" ? DataProjection : never;
  readonly changed: K extends "ConfigMap" ? ChangeEvent : never;
}
export interface ObservedReference { readonly data: DataProjection; readonly changed: ChangeEvent }
/** A handler-written managed ConfigMap output: usable as `<On outputs>` and,
 * via `.data`, in `rolloutOn`. */
export interface HandlerOutputRef { readonly [handlerOutputBrand]: true; readonly data: DataProjection }
type ResourceKind = "ConfigMap" | "Deployment" | "Service";
type LifecycleProps = { readonly deletionPolicy?: "orphan" | "delete"; readonly force?: boolean };
export type ControllerProps = Omit<model.DomainOptions, "provenance"> & { readonly children?: ApsidesNode };
export type ConfigMapProps = model.ConfigMapDefinition & LifecycleProps & { readonly ref?: ResourceRef<"ConfigMap"> };
export type ServiceProps = model.ServiceDefinition & LifecycleProps & { readonly ref?: ResourceRef<"Service"> };
export type DeploymentProps = model.DeploymentDefinition & LifecycleProps & {
  readonly ref?: ResourceRef<"Deployment">;
  readonly rolloutOn?: readonly DataProjection[];
};
export type NamespacedResourceProps<Spec> = model.NamespacedResourceDefinition<Spec> & LifecycleProps;
export type NamespacedResourceComponent<Spec> = Component<NamespacedResourceProps<Spec>>;
export interface ObserveProps { readonly id?: string; readonly name: string; readonly namespace?: string }
export interface HandlerOutputProps { readonly id?: string; readonly name: string; readonly namespace?: string }
/**
 * A Brigade-style event handler.
 *
 * `handler` MUST be a module reference string (`"./handlers/sync.ts#exportName"`,
 * split here as `handler` + `export`) — never a function: handlers run in a
 * separate execution context and cannot capture author closures.
 */
export interface HandlerProps {
  readonly id?: string;
  readonly trigger: ChangeEvent;
  readonly handler: string;
  readonly inputs: readonly (ObservedReference | ResourceRef<"ConfigMap">)[];
  readonly egress?: readonly string[];
  readonly outputs?: readonly HandlerOutputRef[];
  readonly timeoutMillis?: number;
  readonly maxHeapBytes?: number;
}

const builtin = (): never => { throw new Error("Apsides components must be rendered as JSX by the compiler"); };
export const Controller = (_props: ControllerProps): ApsidesElement => { return builtin(); };
export const ConfigMap = (_props: ConfigMapProps): ApsidesElement => { return builtin(); };
export const Service = (_props: ServiceProps): ApsidesElement => { return builtin(); };
export const Deployment = (_props: DeploymentProps): ApsidesElement => { return builtin(); };
/** A Brigade-style event handler element; see {@link HandlerProps}. */
export const On = (_props: HandlerProps): ApsidesElement => { return builtin(); };
export const Fragment = Symbol("Apsides.Fragment");

const namespacedResources = new WeakMap<Function, model.NamespacedResourceDescriptor>();

/** Defines a typed JSX component for one namespaced Kubernetes custom resource. */
export const defineNamespacedResource = <Spec>(descriptor: model.NamespacedResourceDescriptor): NamespacedResourceComponent<Spec> => {
  const resource = model.validateNamespacedResourceDescriptor(snapshotNativeData(descriptor));
  const component = (_props: NamespacedResourceProps<Spec>): ApsidesElement => builtin();
  namespacedResources.set(component, resource);
  return component;
};

interface ElementState { readonly type: unknown; readonly props: Record<string, unknown>; readonly source?: model.SourceLocation }
const elements = new WeakMap<object, ElementState>();
interface ReferenceState {
  readonly owner: RenderState;
  readonly kind: ResourceKind;
  managed?: model.ManagedResource;
}
interface ProjectionState { readonly owner: RenderState; readonly ref?: ReferenceState; readonly observed?: model.ObservedInput | model.HandlerOutputInput }
const references = new WeakMap<object, ReferenceState>();
const projections = new WeakMap<object, ProjectionState>();
const handlerOutputs = new WeakMap<object, model.HandlerOutputInput>();
interface RenderState {
  readonly domain: model.Domain;
  readonly namespace: string;
  readonly refs: ReferenceState[];
  readonly rollouts: { target: model.ManagedResource<model.Deployment>; inputs: readonly unknown[]; source: model.SourceLocation }[];
  readonly handlers: { props: HandlerProps; source: model.SourceLocation }[];
}
interface Frame { readonly state: RenderState; readonly source: model.SourceLocation }
let frame: Frame | undefined;

const record = (value: unknown, context: string): Record<string, unknown> => {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error(`${context} must be plain data`);
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== null && prototype !== Object.prototype) throw new Error(`${context} must be plain data`);
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== "string" || !("value" in Object.getOwnPropertyDescriptor(value, key)!)) throw new Error(`${context} requires data properties`);
  }
  return value as Record<string, unknown>;
};

/** Factories are inert: JSX child expressions are created before their Controller. */
export const makeElement = (type: unknown, props: unknown, source?: { fileName: string; lineNumber: number; columnNumber: number }): ApsidesElement => {
  const node = Object.freeze(Object.create(null)) as ApsidesElement;
  const values = props == null ? {} : record(props, "JSX props");
  let location: model.SourceLocation | undefined;
  if (source !== undefined) {
    const raw = record(source, "JSX source");
    if (typeof raw.fileName !== "string" || !raw.fileName.startsWith("apsides:/source/") || !Number.isSafeInteger(raw.lineNumber) || (raw.lineNumber as number) < 1 || !Number.isSafeInteger(raw.columnNumber) || (raw.columnNumber as number) < 1) throw new Error("JSX source must come from the locked compiler transform");
    location = Object.freeze({ file: raw.fileName.slice("apsides:/source/".length), line: raw.lineNumber as number, column: raw.columnNumber as number });
  }
  elements.set(node, { type, props: Object.freeze({ ...values }), ...(location ? { source: location } : {}) });
  return node;
};

const requireFrame = (): Frame => {
  if (!frame) throw new Error("Apsides hooks require a synchronous component inside Controller");
  return frame;
};
const makeProjection = (state: ProjectionState): DataProjection => {
  const value = Object.freeze(Object.create(null)) as DataProjection;
  projections.set(value, state);
  return value;
};
const makeChangeEvent = (state: ProjectionState): ChangeEvent => {
  const value = Object.freeze(Object.create(null)) as ChangeEvent;
  projections.set(value, state);
  return value;
};
export const useResourceRef = <K extends ResourceKind>(kind: K): ResourceRef<K> => {
  const { state } = requireFrame();
  if (kind !== "ConfigMap" && kind !== "Deployment" && kind !== "Service") throw new Error("unsupported resource reference kind");
  const reference: ReferenceState = { owner: state, kind };
  const value = Object.freeze(kind === "ConfigMap" ? { data: makeProjection({ owner: state, ref: reference }), changed: makeChangeEvent({ owner: state, ref: reference }) } : {});
  references.set(value, reference);
  state.refs.push(reference);
  return value as ResourceRef<K>;
};
const observe = (kind: typeof model.ConfigMap | typeof model.Secret, options: ObserveProps): ObservedReference => {
  const { state, source } = requireFrame();
  const raw = record(options, "observation");
  for (const key of Object.keys(raw)) if (!["id", "name", "namespace"].includes(key)) throw new Error(`unsupported observation field: ${key}`);
  const supplied = { ...raw, namespace: raw.namespace ?? state.namespace, projection: ["data"], missing: "block-dependent-mutations", source } as unknown as model.ObserveOptions;
  const observed = kind === model.Secret ? state.domain.observe(model.Secret, supplied) : state.domain.observe(model.ConfigMap, supplied);
  return Object.freeze({ data: makeProjection({ owner: state, observed }), changed: makeChangeEvent({ owner: state, observed }) });
};
export const useConfigMap = (options: ObserveProps): ObservedReference => { return observe(model.ConfigMap, options); };
export const useSecret = (options: ObserveProps): ObservedReference => { return observe(model.Secret, options); };

/** Declare a handler `Output`: a managed ConfigMap the handler writes to,
 * usable as `<On outputs>` and, through `.data`, in `rolloutOn`. */
export const useHandlerOutput = (options: HandlerOutputProps): HandlerOutputRef => {
  const { state, source } = requireFrame();
  const raw = record(options, "handler output");
  for (const key of Object.keys(raw)) if (!["id", "name", "namespace"].includes(key)) throw new Error(`unsupported handler output field: ${key}`);
  const supplied = { ...raw, namespace: raw.namespace ?? state.namespace, source } as { readonly id?: string; readonly name: string; readonly namespace: string; readonly source: model.SourceLocation };
  const output = model.defineHandlerOutput(state.domain, supplied);
  const value = Object.freeze({ data: makeProjection({ owner: state, observed: output }) }) as HandlerOutputRef;
  handlerOutputs.set(value, output);
  return value;
};

const bindReference = (ref: unknown, state: RenderState, managed: model.ManagedResource, kind: ResourceKind): void => {
  if (ref === undefined) return;
  const value = ref !== null && typeof ref === "object" ? references.get(ref) : undefined;
  if (!value || value.owner !== state) throw new Error("foreign or invalid resource reference");
  if (value.kind !== kind) throw new Error("resource reference kind mismatch");
  if (value.managed) throw new Error("resource reference is already bound");
  value.managed = managed;
};

const snapshotNativeData = (value: unknown, visiting = new Set<object>()): unknown => {
  if (typeof value === "function" || typeof value === "symbol" || typeof value === "bigint") throw new Error("native resource fields require plain data, never callbacks or opaque projections");
  if (value === null || typeof value !== "object") return value;
  if (projections.has(value) || references.has(value) || elements.has(value)) throw new Error("opaque Apsides values cannot appear in native resource fields");
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
};

/** Compiler-only entry: author code never supplies its own provenance to Controller. */
export const renderTree = (root: unknown, provenance: model.DomainOptions["provenance"]): model.ApsidesIR => {
  if (frame) throw new Error("nested compiler rendering is unsupported");
  let state: RenderState | undefined;
  let count = 0;
  const visiting = new Set<object>();
  const visit = (node: unknown, current?: RenderState, inheritedSource?: model.SourceLocation, depth = 0): void => {
    if (++count > 65_536 || depth > 128) throw new Error("component tree budget exceeded");
    if (node == null || typeof node === "boolean") return;
    if (typeof node !== "object") throw new Error("components must return Apsides JSX nodes");
    if (visiting.has(node)) throw new Error("cyclic component tree");
    visiting.add(node);
    try {
      if (Array.isArray(node)) { for (const child of node) visit(child, current, inheritedSource, depth + 1); return; }
      const element = elements.get(node);
      if (!element) throw new Error("components must return synchronous Apsides JSX nodes; promises are unsupported");
      const { type, props } = element;
      const source = element.source ?? inheritedSource;
      if (type === Fragment) { visit(props.children, current, source, depth + 1); return; }
      if (type === Controller) {
        if (state || current) throw new Error("exactly one Controller is supported; nested Controllers are unsupported");
        const { children, ...options } = props;
        if ("provenance" in options) throw new Error("Controller provenance is compiler-owned");
        const domain = model.defineDomain({ ...options, provenance } as model.DomainOptions);
        state = { domain, namespace: (options as unknown as ControllerProps).namespace, refs: [], rollouts: [], handlers: [] };
        visit(children, state, source, depth + 1);
        return;
      }
      const customResource = typeof type === "function" ? namespacedResources.get(type) : undefined;
      if (customResource !== undefined) {
        if (!current) throw new Error("resources require Controller");
        if (!source) throw new Error("resource JSX requires compiler source information");
        const { deletionPolicy, force, ...definition } = props;
        const options = { source, ...(deletionPolicy === undefined ? {} : { deletionPolicy }), ...(force === undefined ? {} : { force }) } as model.ManageOptions;
        const resource = new model.NamespacedResource(customResource, snapshotNativeData(definition) as model.NamespacedResourceDefinition<unknown>);
        current.domain.manage(resource, options);
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
      if (type === On) {
        if (!current) throw new Error("handlers require Controller");
        if (!source) throw new Error("handler JSX requires compiler source information");
        current.handlers.push({ props: props as unknown as HandlerProps, source });
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
  };
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
  for (const handler of completed.handlers) declareHandlerFromProps(completed, handler.props, handler.source);
  return completed.domain.toIR();
};

type Watchable = model.ManagedResource<model.ConfigMap> | model.ObservedInput | model.HandlerOutputInput;

const resolveProjectionTarget = (projection: ProjectionState | undefined, owner: RenderState, context: string): Watchable => {
  if (!projection || projection.owner !== owner) throw new Error(`${context} requires a reference from this Controller`);
  const targetInput = projection.observed ?? projection.ref?.managed;
  if (!targetInput || (projection.ref && projection.ref.kind !== "ConfigMap")) throw new Error(`${context} requires a ConfigMap reference`);
  return targetInput as Watchable;
};

const inputIdOf = (target: Watchable): string => {
  return target instanceof model.ManagedResource ? model.managedInputId(target) : target.id;
};

const onPropKeys = new Set(["id", "trigger", "handler", "inputs", "egress", "outputs", "timeoutMillis", "maxHeapBytes"]);

const declareHandlerFromProps = (owner: RenderState, props: HandlerProps, source: model.SourceLocation): void => {
  const raw = record(props, "On props");
  for (const key of Object.keys(raw)) if (!onPropKeys.has(key)) throw new Error(`unsupported On field: ${key}`);
  if (typeof raw.handler !== "string") {
    throw new Error('On.handler must be a module reference string ("./handlers/sync.ts#exportName"), not a function: handlers run in a separate execution context and cannot capture author closures');
  }
  const separator = raw.handler.lastIndexOf("#");
  if (separator <= 0 || separator === raw.handler.length - 1) {
    throw new Error('On.handler must be formatted as "<module path>#<export name>"');
  }
  const modulePath = raw.handler.slice(0, separator);
  const exportName = raw.handler.slice(separator + 1);

  const triggerProjection = raw.trigger !== null && typeof raw.trigger === "object" ? projections.get(raw.trigger as object) : undefined;
  const triggerTarget = resolveProjectionTarget(triggerProjection, owner, "On.trigger");
  const triggerId = inputIdOf(triggerTarget);

  if (!Array.isArray(raw.inputs) || raw.inputs.length === 0) throw new Error("On.inputs requires a non-empty array of ConfigMap references");
  const inputIds = raw.inputs.map((entry) => {
    const dataValue = entry !== null && typeof entry === "object" ? (entry as { data?: unknown }).data : undefined;
    const projection = dataValue !== null && typeof dataValue === "object" ? projections.get(dataValue as object) : undefined;
    return inputIdOf(resolveProjectionTarget(projection, owner, "On.inputs"));
  });

  const outputs = raw.outputs === undefined ? [] : raw.outputs;
  if (!Array.isArray(outputs)) throw new Error("On.outputs must be an array of handler output references");
  const outputIds = outputs.map((entry) => {
    const output = entry !== null && typeof entry === "object" ? handlerOutputs.get(entry as object) : undefined;
    if (!output) throw new Error("On.outputs requires references returned by useHandlerOutput()");
    return output.outputId;
  });

  const egress = raw.egress === undefined ? [] : raw.egress;
  if (!Array.isArray(egress)) throw new Error("On.egress must be an array of host strings");

  model.defineHandler(owner.domain, {
    ...(raw.id === undefined ? {} : { id: raw.id as string }),
    module: modulePath,
    export: exportName,
    trigger: { kind: "input-changed", input: triggerId },
    inputs: inputIds,
    egress: egress as readonly string[],
    outputs: outputIds,
    ...(raw.timeoutMillis === undefined ? {} : { timeoutMillis: raw.timeoutMillis as number }),
    ...(raw.maxHeapBytes === undefined ? {} : { maxHeapBytes: raw.maxHeapBytes as number }),
    source,
  });
};
