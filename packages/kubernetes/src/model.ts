export const schemaVersion = "apsides.dev/ir/v2" as const;
export const compilerVersion = "apsides/0.1" as const;

/** Reserved label marking a managed ConfigMap as a handler output. */
export const HANDLER_OUTPUT_LABEL = "apsides.dev/handler-output" as const;
/** The only supported handler trigger kind in this increment. */
export const HANDLER_TRIGGER_INPUT_CHANGED = "input-changed" as const;
export const MAX_HANDLER_TIMEOUT_MILLIS = 30_000;
export const MAX_HANDLER_HEAP_BYTES = 64 * 1024 * 1024;
export const MAX_HANDLERS = 16;
export const MAX_EGRESS_HOSTS = 16;

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface SourceLocation {
  readonly file: string;
  readonly line: number;
  readonly column: number;
}

export interface ProvenanceInput {
  readonly path: string;
  readonly sha256: string;
}

export interface DomainBudgets {
  readonly maxResources: number;
  readonly maxInputs: number;
  readonly maxReactions: number;
  readonly maxObjectBytes: number;
  readonly maxQueueDepth: number;
  readonly maxConcurrentRequests: number;
}

export interface DomainOptions {
  readonly id: string;
  readonly namespace: string;
  readonly controlNamespace: string;
  readonly clusterBinding: string;
  readonly policyRef: string;
  readonly enforcement: "api-bound";
  readonly budgets: DomainBudgets;
  /**
   * The smallest Kubernetes version this domain requires, as `"MAJOR.MINOR"`.
   * Defaults to `"1.34"`, the smallest release the absent-only conditional
   * SSA create contract is qualified for. Authors may only raise it: major
   * must be 1 and minor must be at least 34.
   */
  readonly minKubernetesVersion?: string;
  readonly force?: boolean;
  readonly prune?: boolean;
  readonly provenance: {
    readonly inputs: readonly ProvenanceInput[];
  };
}

export interface ObjectMetadata {
  readonly name: string;
  readonly namespace: string;
  readonly labels?: Readonly<Record<string, string>>;
  readonly annotations?: Readonly<Record<string, string>>;
}

export interface NamespacedApiResourceDescriptor {
  readonly plural: string;
  readonly scope: "Namespaced";
  readonly schemaSha256: string;
}

export interface NamespacedResourceDescriptor {
  readonly apiVersion: string;
  readonly kind: string;
  readonly apiResource: NamespacedApiResourceDescriptor;
}

export interface NamespacedResourceDefinition<Spec> {
  readonly id?: string;
  readonly metadata: ObjectMetadata;
  readonly spec: Spec;
}

export interface ConfigMapDefinition {
  readonly id?: string;
  readonly metadata: ObjectMetadata;
  readonly data?: Readonly<Record<string, string>>;
}

export interface ServicePort {
  readonly name?: string;
  readonly port: number;
  readonly targetPort: number | string;
  readonly protocol?: "TCP" | "UDP" | "SCTP";
}

export interface ServiceDefinition {
  readonly id?: string;
  readonly metadata: ObjectMetadata;
  readonly spec: {
    readonly selector: Readonly<Record<string, string>>;
    readonly ports: readonly ServicePort[];
    readonly type?: "ClusterIP";
  };
}

export interface ContainerDefinition {
  readonly name: string;
  readonly image: string;
  readonly imagePullPolicy?: "Always" | "IfNotPresent" | "Never";
  readonly ports?: readonly {
    readonly name?: string;
    readonly containerPort: number;
    readonly protocol?: "TCP" | "UDP" | "SCTP";
  }[];
  readonly envFrom?: readonly (
    | { readonly configMapRef: { readonly name: string } }
    | { readonly secretRef: { readonly name: string } }
  )[];
  readonly env?: readonly (
    | { readonly name: string; readonly value: string }
    | { readonly name: string; readonly valueFrom: { readonly secretKeyRef: { readonly name: string; readonly key: string } } }
  )[];
  readonly resources?: {
    readonly requests?: Readonly<Partial<Record<"cpu" | "memory", string>>>;
    readonly limits?: Readonly<Partial<Record<"cpu" | "memory", string>>>;
  };
}

export interface DeploymentDefinition {
  readonly id?: string;
  readonly metadata: ObjectMetadata;
  readonly spec: {
    readonly replicas?: number;
    readonly selector: {
      readonly matchLabels: Readonly<Record<string, string>>;
    };
    readonly template: {
      readonly metadata: {
        readonly labels: Readonly<Record<string, string>>;
        readonly annotations?: Readonly<Record<string, string>>;
      };
      readonly spec: {
        readonly serviceAccountName?: string;
        readonly automountServiceAccountToken?: boolean;
        readonly containers: readonly ContainerDefinition[];
      };
    };
  };
}

type ConfigMapDesired = {
  readonly apiVersion: "v1";
  readonly kind: "ConfigMap";
  readonly metadata: ObjectMetadata;
  readonly data?: Readonly<Record<string, string>>;
};

type ServiceDesired = {
  readonly apiVersion: "v1";
  readonly kind: "Service";
  readonly metadata: ObjectMetadata;
  readonly spec: ServiceDefinition["spec"];
};

type DeploymentDesired = {
  readonly apiVersion: "apps/v1";
  readonly kind: "Deployment";
  readonly metadata: ObjectMetadata;
  readonly spec: DeploymentDefinition["spec"];
};

type NamespacedResourceDesired = {
  readonly apiVersion: string;
  readonly kind: string;
  readonly metadata: ObjectMetadata;
  readonly spec: Json;
};

type ResourceDesired = ConfigMapDesired | ServiceDesired | DeploymentDesired | NamespacedResourceDesired;

const resourceState = Symbol("apsides.resource");
const managedState = Symbol("apsides.managed");
const inputState = Symbol("apsides.input");
const managedInputFor = Symbol("apsides.managed-input");
const declareRollout = Symbol("apsides.declare-rollout");
const declareOutput = Symbol("apsides.declare-output");
const declareHandler = Symbol("apsides.declare-handler");

interface ResourceState {
  readonly id?: string;
  readonly apiVersion: string;
  readonly kind: string;
  readonly apiResource?: NamespacedApiResourceDescriptor;
  readonly desired: ResourceDesired;
}

export class ConfigMap {
  static readonly apiVersion = "v1" as const;
  static readonly kind = "ConfigMap" as const;
  private declare readonly __configMapBrand: never;
  readonly [resourceState]: ResourceState;

  constructor(definition: ConfigMapDefinition) {
    const value = record(definition, "ConfigMap");
    exact(value, ["id", "metadata", "data"], "ConfigMap");
    const id = optionalId(value.id, "ConfigMap.id");
    const metadata = validateMetadata(value.metadata, "ConfigMap.metadata");
    const data = value.data === undefined ? undefined : stringMap(value.data, "ConfigMap.data");
    this[resourceState] = freeze({
      ...(id === undefined ? {} : { id }),
      apiVersion: "v1",
      kind: "ConfigMap",
      desired: {
        apiVersion: "v1",
        kind: "ConfigMap",
        metadata,
        ...(data === undefined ? {} : { data }),
      },
    });
    Object.freeze(this);
  }
}

export class Service {
  static readonly apiVersion = "v1" as const;
  static readonly kind = "Service" as const;
  private declare readonly __serviceBrand: never;
  readonly [resourceState]: ResourceState;

  constructor(definition: ServiceDefinition) {
    const value = record(definition, "Service");
    exact(value, ["id", "metadata", "spec"], "Service");
    const id = optionalId(value.id, "Service.id");
    const metadata = validateMetadata(value.metadata, "Service.metadata");
    const spec = validateServiceSpec(value.spec);
    this[resourceState] = freeze({
      ...(id === undefined ? {} : { id }),
      apiVersion: "v1",
      kind: "Service",
      desired: { apiVersion: "v1", kind: "Service", metadata, spec },
    });
    Object.freeze(this);
  }
}

export class Deployment {
  static readonly apiVersion = "apps/v1" as const;
  static readonly kind = "Deployment" as const;
  private declare readonly __deploymentBrand: never;
  readonly [resourceState]: ResourceState;

  constructor(definition: DeploymentDefinition) {
    const value = record(definition, "Deployment");
    exact(value, ["id", "metadata", "spec"], "Deployment");
    const id = optionalId(value.id, "Deployment.id");
    const metadata = validateMetadata(value.metadata, "Deployment.metadata");
    const spec = validateDeploymentSpec(value.spec);
    this[resourceState] = freeze({
      ...(id === undefined ? {} : { id }),
      apiVersion: "apps/v1",
      kind: "Deployment",
      desired: { apiVersion: "apps/v1", kind: "Deployment", metadata, spec },
    });
    Object.freeze(this);
  }
}

/** Compiler-facing resource value created by defineNamespacedResource(). */
export class NamespacedResource<Spec> {
  readonly [resourceState]: ResourceState;

  constructor(descriptor: NamespacedResourceDescriptor, definition: NamespacedResourceDefinition<Spec>) {
    const resource = validateNamespacedResourceDescriptor(descriptor);
    const value = record(definition, resource.kind);
    exact(value, ["id", "metadata", "spec"], resource.kind);
    const id = optionalId(value.id, `${resource.kind}.id`);
    const metadata = validateMetadata(value.metadata, `${resource.kind}.metadata`);
    const spec = canonical(record(value.spec, `${resource.kind}.spec`));
    this[resourceState] = freeze({
      ...(id === undefined ? {} : { id }),
      apiVersion: resource.apiVersion,
      kind: resource.kind,
      apiResource: resource.apiResource,
      desired: { apiVersion: resource.apiVersion, kind: resource.kind, metadata, spec },
    });
    Object.freeze(this);
  }
}

export type ManageableResource = ConfigMap | Service | Deployment | NamespacedResource<unknown>;

export interface ManageOptions {
  readonly force?: boolean;
  readonly id?: string;
  readonly deletionPolicy?: "orphan" | "delete";
  readonly source: SourceLocation;
}

export interface ObserveOptions {
  readonly id?: string;
  readonly name: string;
  readonly namespace: string;
  readonly projection: readonly ["data"];
  readonly missing: "block-dependent-mutations";
  readonly source: SourceLocation;
}

export interface RolloutOptions {
  readonly id?: string;
  readonly source?: SourceLocation;
}

interface ResourceDeclaration {
  readonly force?: boolean;
  readonly id: string;
  readonly apiVersion: string;
  readonly kind: string;
  readonly apiResource?: NamespacedApiResourceDescriptor;
  readonly name: string;
  readonly namespace: string;
  readonly desired: ResourceDesired;
  readonly lifecycle: { readonly deletionPolicy: "orphan" | "delete" };
  readonly source: SourceLocation;
  /** True only for the managed ConfigMap a handler `Output` compiles to. */
  readonly handlerOutput?: true;
}

interface InputDeclaration {
  readonly id: string;
  readonly apiVersion: "v1";
  readonly kind: "ConfigMap" | "Secret";
  readonly name: string;
  readonly namespace: string;
  readonly mode: "managed" | "observed" | "output";
  readonly managedResourceId?: string;
  readonly projection: readonly ["data"];
  readonly sensitive: boolean;
  readonly missing: "block-dependent-mutations";
  readonly source: SourceLocation;
}

interface ReactionDeclaration {
  readonly id: string;
  readonly op: "rollout";
  readonly inputs: readonly string[];
  readonly target: string;
  readonly source: SourceLocation;
}

export interface EgressHost {
  readonly host: string;
  readonly port: number;
}

export interface HandlerTriggerDeclaration {
  readonly kind: "input-changed";
  readonly input: string;
}

interface HandlerDeclaration {
  readonly id: string;
  readonly module: string;
  readonly export: string;
  readonly trigger: HandlerTriggerDeclaration;
  readonly inputs: readonly string[];
  readonly egress: readonly EgressHost[];
  readonly outputs: readonly string[];
  readonly timeoutMillis: number;
  readonly maxHeapBytes: number;
  /** Filled by the compiler after bundling; the SDK always emits `""`. */
  readonly bundleSha256: string;
  readonly source: SourceLocation;
}

interface OutputDeclaration {
  readonly id: string;
  readonly name: string;
  readonly namespace: string;
}

export interface ApsidesIR {
  readonly schemaVersion: typeof schemaVersion;
  /** Production authoring always emits false. Test compilation owns the true transition. */
  readonly testOnly: boolean;
  readonly domain: {
    readonly id: string;
    readonly namespace: string;
    readonly controlNamespace: string;
    readonly clusterBinding: string;
    readonly policyRef: string;
    readonly enforcement: "api-bound";
    readonly budgets: DomainBudgets;
    readonly minKubernetesVersion: string;
  };
  readonly resources: readonly ResourceDeclaration[];
  readonly inputs: readonly InputDeclaration[];
  readonly reactions: readonly ReactionDeclaration[];
  readonly handlers: readonly HandlerDeclaration[];
  readonly outputs: readonly OutputDeclaration[];
  readonly provenance: {
    readonly compiler: typeof compilerVersion;
    readonly inputs: readonly ProvenanceInput[];
  };
}

interface DomainState {
  readonly options: DomainOptions;
  readonly resources: Map<string, ResourceDeclaration>;
  readonly identities: Map<string, string>;
  readonly inputs: Map<string, InputDeclaration>;
  readonly inputIdentities: Map<string, string>;
  readonly reactions: Map<string, ReactionDeclaration>;
  readonly handlers: Map<string, HandlerDeclaration>;
  readonly outputs: Map<string, OutputDeclaration>;
}

export class ManagedResource<R extends ManageableResource = ManageableResource> {
  readonly [managedState]: { readonly domain: Domain; readonly declaration: ResourceDeclaration; readonly resource: R };

  private constructor(domain: Domain, declaration: ResourceDeclaration, resource: R) {
    this[managedState] = freeze({ domain, declaration, resource });
    Object.freeze(this);
  }

  static create<R extends ManageableResource>(domain: Domain, declaration: ResourceDeclaration, resource: R): ManagedResource<R> {
    return new ManagedResource(domain, declaration, resource);
  }

  get id(): string { return this[managedState].declaration.id; }
  get name(): string { return this[managedState].declaration.name; }
  get namespace(): string { return this[managedState].declaration.namespace; }
}

export class ObservedInput<K extends "ConfigMap" | "Secret" = "ConfigMap" | "Secret"> {
  readonly [inputState]: { readonly domain: Domain; readonly declaration: InputDeclaration & { readonly kind: K } };

  private constructor(domain: Domain, declaration: InputDeclaration & { readonly kind: K }) {
    this[inputState] = freeze({ domain, declaration });
    Object.freeze(this);
  }

  static create<K extends "ConfigMap" | "Secret">(domain: Domain, declaration: InputDeclaration & { readonly kind: K }): ObservedInput<K> {
    return new ObservedInput(domain, declaration);
  }

  get id(): string { return this[inputState].declaration.id; }
  get name(): string { return this[inputState].declaration.name; }
  get namespace(): string { return this[inputState].declaration.namespace; }
  get sensitive(): K extends "Secret" ? true : false {
    return this[inputState].declaration.sensitive as K extends "Secret" ? true : false;
  }
}

/** A handler `Output`'s auto-created, live-observed input: watchable the
 * same way as [ObservedInput], distinguished only so review/surface
 * projections can label it accurately (see `InputMode::Output`). */
export class HandlerOutputInput {
  readonly [inputState]: { readonly domain: Domain; readonly declaration: InputDeclaration & { readonly kind: "ConfigMap" } };
  readonly outputId: string;

  private constructor(domain: Domain, declaration: InputDeclaration & { readonly kind: "ConfigMap" }, outputId: string) {
    this[inputState] = freeze({ domain, declaration });
    this.outputId = outputId;
    Object.freeze(this);
  }

  static create(domain: Domain, declaration: InputDeclaration & { readonly kind: "ConfigMap" }, outputId: string): HandlerOutputInput {
    return new HandlerOutputInput(domain, declaration, outputId);
  }

  get id(): string { return this[inputState].declaration.id; }
  get name(): string { return this[inputState].declaration.name; }
  get namespace(): string { return this[inputState].declaration.namespace; }
}

export class Secret {
  private constructor() {}
  static readonly apiVersion = "v1" as const;
  static readonly kind = "Secret" as const;
}

type ObservedKind = typeof ConfigMap | typeof Secret;

export class Domain {
  readonly #state: DomainState;

  constructor(options: DomainOptions) {
    this.#state = {
      options: validateDomainOptions(options),
      resources: new Map(),
      identities: new Map(),
      inputs: new Map(),
      inputIdentities: new Map(),
      reactions: new Map(),
      handlers: new Map(),
      outputs: new Map(),
    };
  }

  manage<R extends ManageableResource>(resource: R, options: ManageOptions): ManagedResource<R> {
    if (!(resource instanceof ConfigMap || resource instanceof Service || resource instanceof Deployment || resource instanceof NamespacedResource)) {
      throw new Error("manage() accepts only Apsides resource values");
    }
    const rawOptions = record(options, "manage options");
    exact(rawOptions, ["id", "deletionPolicy", "force", "source"], "manage options");
    const definition = resource[resourceState];
    const source = validateSource(rawOptions.source, "manage.source");
    const name = kubernetesName(definition.desired.metadata.name, `${definition.kind}.metadata.name`);
    const namespace = dnsLabel(definition.desired.metadata.namespace, 63, `${definition.kind}.metadata.namespace`);
    this.#requireNamespace(namespace, `${definition.kind} ${name}`);
    const id = dnsLabel(optionalId(rawOptions.id, "manage.id") ?? definition.id ?? derivedId(`${definition.kind.toLowerCase()}-${name}`), 63, "manage.id");
    const deletionPolicy = rawOptions.deletionPolicy === undefined ? (this.#state.options.prune ? "delete" : "orphan") : enumValue(rawOptions.deletionPolicy, ["orphan", "delete"] as const, "manage.deletionPolicy");
    const force = optionalBoolean(rawOptions.force, "manage.force") ?? this.#state.options.force ?? false;
    if (this.#state.resources.has(id)) throw new Error(`duplicate managed resource id: ${id}`);
    const identity = definition.apiResource === undefined
      ? `${definition.apiVersion}/${definition.kind}/${namespace}/${name}`
      : `${definition.apiVersion.split("/")[0]}/${definition.apiResource.plural}/${namespace}/${name}`;
    const identityOwner = this.#state.identities.get(identity);
    if (identityOwner !== undefined) throw new Error(`managed resource ${identity} is already declared as ${identityOwner}`);
    const observedOwner = this.#state.inputIdentities.get(`v1/${definition.kind}/${namespace}/${name}`);
    if (observedOwner !== undefined) throw new Error(`managed resource aliases observed input ${observedOwner}`);
    const desired = canonical(definition.desired) as unknown as ResourceDeclaration["desired"];
    if (utf8Length(JSON.stringify(desired)) > this.#state.options.budgets.maxObjectBytes) {
      throw new Error(`managed resource ${id} exceeds maxObjectBytes`);
    }
    if (this.#state.resources.size >= this.#state.options.budgets.maxResources) throw new Error("domain maxResources budget exceeded");
    if (this.#state.resources.size >= this.#state.options.budgets.maxQueueDepth) throw new Error("domain maxQueueDepth cannot hold every managed resource");
    const declaration = freeze({
      id,
      apiVersion: definition.apiVersion,
      kind: definition.kind,
      ...(definition.apiResource === undefined ? {} : { apiResource: definition.apiResource }),
      name,
      namespace,
      desired,
      ...(force ? { force: true } : {}),
      lifecycle: { deletionPolicy },
      source,
    });
    this.#state.resources.set(id, declaration);
    this.#state.identities.set(identity, id);
    return ManagedResource.create(this, declaration, resource);
  }

  observe(kind: typeof ConfigMap, options: ObserveOptions): ObservedInput<"ConfigMap">;
  observe(kind: typeof Secret, options: ObserveOptions): ObservedInput<"Secret">;
  observe(kind: ObservedKind, options: ObserveOptions): ObservedInput {
    if (kind !== ConfigMap && kind !== Secret) throw new Error("observe() supports only ConfigMap and Secret");
    const raw = record(options, "observe options");
    exact(raw, ["id", "name", "namespace", "projection", "missing", "source"], "observe options");
    const name = kubernetesName(raw.name, "observe.name");
    const namespace = dnsLabel(raw.namespace, 63, "observe.namespace");
    this.#requireNamespace(namespace, `${kind.kind} ${name}`);
    projection(raw.projection, "observe.projection");
    enumValue(raw.missing, ["block-dependent-mutations"] as const, "observe.missing");
    const id = dnsLabel(optionalId(raw.id, "observe.id") ?? derivedId(`observed-${kind.kind.toLowerCase()}-${name}`), 63, "observe.id");
    const declaration = this.#addInput({
      id,
      apiVersion: "v1",
      kind: kind.kind,
      name,
      namespace,
      mode: "observed",
      projection: ["data"],
      sensitive: kind === Secret,
      missing: "block-dependent-mutations",
      source: validateSource(raw.source, "observe.source"),
    });
    return ObservedInput.create(this, declaration as InputDeclaration & { readonly kind: "ConfigMap" | "Secret" });
  }

  toIR(): ApsidesIR {
    const sources = [
      ...this.#state.resources.values(),
      ...this.#state.inputs.values(),
      ...this.#state.reactions.values(),
      ...this.#state.handlers.values(),
    ].map((declaration) => declaration.source.file);
    const provenancePaths = new Set(this.#state.options.provenance.inputs.map((input) => input.path));
    for (const source of sources) {
      if (!provenancePaths.has(source)) throw new Error(`source file is absent from provenance inputs: ${source}`);
    }
    const { provenance: _, force: _force, prune: _prune, ...domain } = this.#state.options;
    return freeze(canonical({
      schemaVersion,
      testOnly: false,
      domain,
      resources: sorted(this.#state.resources),
      inputs: sorted(this.#state.inputs),
      reactions: sorted(this.#state.reactions),
      handlers: sorted(this.#state.handlers),
      outputs: sorted(this.#state.outputs),
      provenance: { compiler: compilerVersion, inputs: this.#state.options.provenance.inputs },
    }) as unknown as ApsidesIR);
  }

  [managedInputFor](resource: ManagedResource<ConfigMap>): InputDeclaration {
    const state = resource[managedState];
    if (state.domain !== this) throw new Error("cross-domain watch is unsupported");
    const managed = state.declaration;
    if (managed.kind !== "ConfigMap") throw new Error("only ConfigMap data can be watched in this release");
    const id = dnsLabel(derivedId(`managed-${managed.id}`), 63, "managed input id");
    const existing = this.#state.inputs.get(id);
    if (existing !== undefined) return existing;
    return this.#addInput({
      id,
      apiVersion: "v1",
      kind: "ConfigMap",
      name: managed.name,
      namespace: managed.namespace,
      mode: "managed",
      managedResourceId: managed.id,
      projection: ["data"],
      sensitive: false,
      missing: "block-dependent-mutations",
      source: managed.source,
    });
  }

  [declareRollout](input: InputDeclaration, target: ManagedResource<Deployment>, options?: RolloutOptions): void {
    const targetState = target[managedState];
    if (targetState.domain !== this) throw new Error("cross-domain mutation is unsupported");
    if (targetState.declaration.kind !== "Deployment") throw new Error("rollout target must be a managed Deployment");
    const raw = options === undefined ? {} : record(options, "rollout options");
    exact(raw, ["id", "source"], "rollout options");
    const id = dnsLabel(optionalId(raw.id, "rollout.id") ?? derivedId(`rollout-${input.id}-${target.id}`), 63, "rollout.id");
    if (this.#state.reactions.has(id)) throw new Error(`duplicate reaction id: ${id}`);
    if (this.#state.reactions.size >= this.#state.options.budgets.maxReactions) throw new Error("domain maxReactions budget exceeded");
    const source = raw.source === undefined ? input.source : validateSource(raw.source, "rollout.source");
    this.#state.reactions.set(id, freeze({ id, op: "rollout", inputs: [input.id], target: target.id, source }));
  }

  [declareOutput](options: { id?: string; name: string; namespace?: string; source: SourceLocation }): HandlerOutputInput {
    const raw = record(options, "output options");
    exact(raw, ["id", "name", "namespace", "source"], "output options");
    const name = kubernetesName(raw.name, "output.name");
    const namespace = raw.namespace === undefined ? this.#state.options.namespace : dnsLabel(raw.namespace, 63, "output.namespace");
    this.#requireNamespace(namespace, `output ${name}`);
    const source = validateSource(raw.source, "output.source");
    const id = dnsLabel(optionalId(raw.id, "output.id") ?? derivedId(`output-${name}`), 63, "output.id");
    if (this.#state.outputs.has(id) || this.#state.resources.has(id)) throw new Error(`duplicate output id: ${id}`);
    const identity = `v1/ConfigMap/${namespace}/${name}`;
    const identityOwner = this.#state.identities.get(identity);
    if (identityOwner !== undefined) throw new Error(`output ${identity} aliases managed resource ${identityOwner}`);
    if (this.#state.resources.size >= this.#state.options.budgets.maxResources) throw new Error("domain maxResources budget exceeded");
    if (this.#state.resources.size >= this.#state.options.budgets.maxQueueDepth) throw new Error("domain maxQueueDepth cannot hold every managed resource");
    const deletionPolicy = this.#state.options.prune ? "delete" : "orphan";
    const desired = canonical({
      apiVersion: "v1",
      kind: "ConfigMap",
      metadata: { name, namespace, labels: { [HANDLER_OUTPUT_LABEL]: id } },
    }) as unknown as ResourceDeclaration["desired"];
    const resourceDeclaration: ResourceDeclaration = freeze({
      id, apiVersion: "v1", kind: "ConfigMap", name, namespace, desired,
      lifecycle: { deletionPolicy }, source, handlerOutput: true,
    });
    this.#state.resources.set(id, resourceDeclaration);
    this.#state.identities.set(identity, id);
    this.#state.outputs.set(id, freeze({ id, name, namespace }));
    const inputId = dnsLabel(derivedId(`output-input-${id}`), 63, "output input id");
    const inputDeclaration = this.#addInput({
      id: inputId,
      apiVersion: "v1",
      kind: "ConfigMap",
      name,
      namespace,
      mode: "output",
      managedResourceId: id,
      projection: ["data"],
      sensitive: false,
      missing: "block-dependent-mutations",
      source,
    });
    return HandlerOutputInput.create(this, inputDeclaration as InputDeclaration & { readonly kind: "ConfigMap" }, id);
  }

  [declareHandler](options: {
    id?: string;
    module: string;
    export: string;
    trigger: HandlerTriggerDeclaration;
    inputs: readonly string[];
    egress: readonly string[];
    outputs: readonly string[];
    timeoutMillis?: number;
    maxHeapBytes?: number;
    source: SourceLocation;
  }): void {
    const id = dnsLabel(optionalId(options.id, "handler.id") ?? derivedId(`handler-${options.export}-${options.module}`), 63, "handler.id");
    if (this.#state.handlers.has(id)) throw new Error(`duplicate handler id: ${id}`);
    if (this.#state.handlers.size >= MAX_HANDLERS) throw new Error(`domain exceeds maxHandlers budget of ${MAX_HANDLERS}`);
    const module = normalizedRelativePath(options.module, "handler.module");
    const exportName = jsIdentifier(options.export, "handler.export");
    const timeoutMillis = boundedInteger(options.timeoutMillis ?? MAX_HANDLER_TIMEOUT_MILLIS, 1, MAX_HANDLER_TIMEOUT_MILLIS, "handler.timeoutMillis");
    const maxHeapBytes = boundedInteger(options.maxHeapBytes ?? MAX_HANDLER_HEAP_BYTES, 1, MAX_HANDLER_HEAP_BYTES, "handler.maxHeapBytes");
    const inputs = [...new Set(options.inputs)];
    if (inputs.length === 0 || inputs.length !== options.inputs.length) throw new Error(`handler ${id} requires distinct nonempty inputs`);
    if (!inputs.includes(options.trigger.input)) throw new Error(`handler ${id} trigger input must be one of its declared inputs`);
    for (const inputId of inputs) {
      const input = this.#state.inputs.get(inputId);
      if (!input) throw new Error(`handler ${id} has unresolved input ${inputId}`);
      if (input.kind !== "ConfigMap" || input.sensitive) throw new Error(`handler ${id} input ${inputId} must be a non-sensitive ConfigMap input`);
    }
    const outputs = [...new Set(options.outputs)];
    if (outputs.length !== options.outputs.length) throw new Error(`handler ${id} outputs must be distinct`);
    for (const outputId of outputs) {
      if (!this.#state.outputs.has(outputId)) throw new Error(`handler ${id} has unresolved output ${outputId}`);
    }
    for (const inputId of inputs) {
      const input = this.#state.inputs.get(inputId)!;
      if (input.mode === "output" && input.managedResourceId !== undefined && outputs.includes(input.managedResourceId)) {
        throw new Error(`handler ${id} may not trigger on or read its own output`);
      }
    }
    const egressSeen = new Set<string>();
    const egress = options.egress.map((host) => {
      const validated = egressHost(host, `handler ${id} egress`);
      const key = `${validated.host}:${String(validated.port)}`;
      if (egressSeen.has(key)) throw new Error(`handler ${id} egress hosts must be distinct`);
      egressSeen.add(key);
      return validated;
    });
    const existingHosts = [...this.#state.handlers.values()].flatMap((handler) => handler.egress.map((entry) => entry.host));
    const totalEgressHosts = new Set([...existingHosts, ...egress.map((entry) => entry.host)]);
    if (totalEgressHosts.size > MAX_EGRESS_HOSTS) throw new Error(`domain exceeds maxEgressHosts budget of ${MAX_EGRESS_HOSTS}`);
    const source = validateSource(options.source, "handler.source");
    const declaration = freeze(canonical({
      id,
      module,
      export: exportName,
      trigger: { kind: "input-changed", input: options.trigger.input },
      inputs,
      egress,
      outputs,
      timeoutMillis,
      maxHeapBytes,
      bundleSha256: "",
      source,
    }) as unknown as HandlerDeclaration);
    this.#state.handlers.set(id, declaration);
  }

  #addInput(input: InputDeclaration): InputDeclaration {
    if (this.#state.inputs.has(input.id)) throw new Error(`duplicate input id: ${input.id}`);
    if (this.#state.inputs.size >= this.#state.options.budgets.maxInputs) throw new Error("domain maxInputs budget exceeded");
    const identity = `${input.apiVersion}/${input.kind}/${input.namespace}/${input.name}`;
    const existing = this.#state.inputIdentities.get(identity);
    if (existing !== undefined) throw new Error(`input ${identity} is already declared as ${existing}`);
    if (input.mode === "observed") {
      const managedIdentity = this.#state.identities.get(identity);
      if (managedIdentity !== undefined) throw new Error(`observed input aliases managed resource ${managedIdentity}`);
    }
    const declaration = freeze(canonical(input) as unknown as InputDeclaration);
    this.#state.inputs.set(input.id, declaration);
    this.#state.inputIdentities.set(identity, input.id);
    return declaration;
  }

  #requireNamespace(namespace: string, context: string): void {
    if (namespace !== this.#state.options.namespace) {
      throw new Error(`${context} is outside domain namespace ${this.#state.options.namespace}; cross-domain operations are unsupported`);
    }
  }
}

export const defineDomain = (options: DomainOptions): Domain => {
  return new Domain(options);
};

export const validateNamespacedResourceDescriptor = (value: unknown): NamespacedResourceDescriptor => {
  const descriptor = record(value, "namespaced resource descriptor");
  exact(descriptor, ["apiVersion", "kind", "apiResource"], "namespaced resource descriptor");
  const apiVersion = boundedText(descriptor.apiVersion, 253, "namespaced resource descriptor.apiVersion");
  const apiVersionParts = apiVersion.split("/");
  const [group, version] = apiVersionParts;
  if (apiVersionParts.length !== 2 || group === undefined || version === undefined) throw new Error("namespaced resource descriptor.apiVersion must contain a custom resource group and version");
  for (const groupPart of group.split(".")) {
    dnsLabel(groupPart, 63, "namespaced resource descriptor.apiVersion group");
  }
  dnsLabel(version, 63, "namespaced resource descriptor.apiVersion version");
  const kind = boundedText(descriptor.kind, 63, "namespaced resource descriptor.kind");
  if (!/^[A-Z][A-Za-z0-9]*$/.test(kind)) throw new Error("namespaced resource descriptor.kind must be an upper camel case Kubernetes kind");
  if (["ConfigMap", "Secret", "Service", "Deployment"].includes(kind)) {
    throw new Error(`namespaced resource descriptor.kind cannot replace built-in ${kind}`);
  }
  const rawApiResource = record(descriptor.apiResource, "namespaced resource descriptor.apiResource");
  exact(rawApiResource, ["plural", "scope", "schemaSha256"], "namespaced resource descriptor.apiResource");
  const plural = dnsLabel(rawApiResource.plural, 63, "namespaced resource descriptor.apiResource.plural");
  const scope = enumValue(rawApiResource.scope, ["Namespaced"] as const, "namespaced resource descriptor.apiResource.scope");
  const schemaSha256 = text(rawApiResource.schemaSha256, "namespaced resource descriptor.apiResource.schemaSha256");
  if (!/^sha256:[0-9a-f]{64}$/.test(schemaSha256)) {
    throw new Error("namespaced resource descriptor.apiResource.schemaSha256 must be a lowercase sha256 digest");
  }
  return freeze({ apiVersion, kind, apiResource: { plural, scope, schemaSha256 } });
};

/** The input id watching a managed ConfigMap's data, creating it if absent. */
export const managedInputId = (input: ManagedResource<ConfigMap>): string => {
  const state = input[managedState];
  return state.domain[managedInputFor](input).id;
};

type Watchable = ManagedResource<ConfigMap> | ObservedInput | HandlerOutputInput;

export const watch = (input: Watchable): {
  project(projectionName: "data"): {
    rollout(target: ManagedResource<Deployment>, options?: RolloutOptions): void;
  };
} => {
  if (!(input instanceof ManagedResource || input instanceof ObservedInput || input instanceof HandlerOutputInput)) throw new Error("watch() requires a managed ConfigMap, observed input, or handler output");
  const state = input instanceof ManagedResource ? input[managedState] : input[inputState];
  const domain = state.domain;
  return freeze({
    project(projectionName: "data") {
      projection(projectionName, "watch.project");
      const declaration = input instanceof ManagedResource ? domain[managedInputFor](input as ManagedResource<ConfigMap>) : input[inputState].declaration;
      return freeze({
        rollout(target: ManagedResource<Deployment>, options?: RolloutOptions) {
          domain[declareRollout](declaration, target, options);
        },
      });
    },
  });
};

/** Declare a handler `Output`: a managed `ConfigMap` (metadata only, flagged
 * `handlerOutput`) plus its auto-created live-observed input. Compiler-facing
 * entry point for `useHandlerOutput()`; see `Domain[declareOutput]`. */
export const defineHandlerOutput = (domain: Domain, options: { id?: string; name: string; namespace?: string; source: SourceLocation }): HandlerOutputInput => {
  return domain[declareOutput](options);
};

/** Declare a Brigade-style event handler. Compiler-facing entry point for
 * the `<On>` element; see `Domain[declareHandler]`. */
export const defineHandler = (domain: Domain, options: {
  id?: string;
  module: string;
  export: string;
  trigger: HandlerTriggerDeclaration;
  inputs: readonly string[];
  egress: readonly string[];
  outputs: readonly string[];
  timeoutMillis?: number;
  maxHeapBytes?: number;
  source: SourceLocation;
}): void => {
  domain[declareHandler](options);
};

/** @internal Shared fixture seed completion for the SDK and source compiler. */
export const fixtureInputData = (
  input: { readonly id: string; readonly kind: string; readonly name: string; readonly namespace: string },
  resources: readonly { readonly kind: string; readonly namespace: string; readonly desired: unknown }[],
): Readonly<Record<string, string>> => {
  const keys = new Set<string>();
  if (input.kind === "Secret") {
    for (const resource of resources) {
      if (resource.kind !== "Deployment" || resource.namespace !== input.namespace) continue;
      const desired = record(resource.desired, "fixture Deployment");
      const spec = record(record(record(desired.spec, "fixture Deployment.spec").template, "fixture Deployment.template").spec, "fixture Pod.spec");
      for (const container of array(spec.containers, "fixture containers")) {
        const env = record(container, "fixture container").env;
        for (const entry of env === undefined ? [] : array(env, "fixture env")) {
          const valueFrom = record(entry, "fixture env entry").valueFrom;
          if (valueFrom === undefined) continue;
          const reference = record(record(valueFrom, "fixture valueFrom").secretKeyRef, "fixture secretKeyRef");
          if (reference.name === input.name) keys.add(text(reference.key, "fixture Secret key"));
        }
      }
    }
  }
  const value = input.kind === "Secret" ? `synthetic-secret-${input.id}` : `synthetic-config-${input.id}`;
  return freeze(Object.fromEntries(["value", ...keys].sort(compareText).map((key) => [key, value])));
};

/** The smallest Kubernetes minor version a declared `minKubernetesVersion` may name. */
const MIN_KUBERNETES_VERSION_FLOOR = { major: 1, minor: 34 } as const;
const DEFAULT_MIN_KUBERNETES_VERSION = `${MIN_KUBERNETES_VERSION_FLOOR.major}.${MIN_KUBERNETES_VERSION_FLOOR.minor}`;

const validateMinKubernetesVersion = (value: unknown): string => {
  const provided = value === undefined ? DEFAULT_MIN_KUBERNETES_VERSION : boundedText(value, 16, "domain.minKubernetesVersion");
  const match = /^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$/.exec(provided);
  if (!match || match[1] === undefined || match[2] === undefined) {
    throw new Error("domain.minKubernetesVersion must be formatted as canonical MAJOR.MINOR (no leading zeros)");
  }
  const major = Number(match[1]);
  const minor = Number(match[2]);
  if (major !== MIN_KUBERNETES_VERSION_FLOOR.major || minor < MIN_KUBERNETES_VERSION_FLOOR.minor) {
    throw new Error(
      `domain.minKubernetesVersion must be at least the qualified floor ${DEFAULT_MIN_KUBERNETES_VERSION} (major ${MIN_KUBERNETES_VERSION_FLOOR.major}, minor >= ${MIN_KUBERNETES_VERSION_FLOOR.minor}); received ${provided}`,
    );
  }
  return provided;
};

const validateDomainOptions = (options: DomainOptions): DomainOptions => {
  const value = record(options, "domain options");
  exact(value, ["id", "namespace", "controlNamespace", "clusterBinding", "policyRef", "enforcement", "budgets", "minKubernetesVersion", "provenance", "force", "prune"], "domain options");
  const namespace = dnsLabel(value.namespace, 63, "domain.namespace");
  const controlNamespace = dnsLabel(value.controlNamespace, 63, "domain.controlNamespace");
  if (namespace === controlNamespace) throw new Error("controlNamespace must differ from the workload namespace");
  enumValue(value.enforcement, ["api-bound"] as const, "domain.enforcement");
  const budgetRecord = record(value.budgets, "domain.budgets");
  exact(budgetRecord, ["maxResources", "maxInputs", "maxReactions", "maxObjectBytes", "maxQueueDepth", "maxConcurrentRequests"], "domain.budgets");
  const budgets = freeze({
    maxResources: boundedInteger(budgetRecord.maxResources, 1, 256, "budgets.maxResources"),
    maxInputs: boundedInteger(budgetRecord.maxInputs, 1, 256, "budgets.maxInputs"),
    maxReactions: boundedInteger(budgetRecord.maxReactions, 1, 256, "budgets.maxReactions"),
    maxObjectBytes: boundedInteger(budgetRecord.maxObjectBytes, 1_024, 262_144, "budgets.maxObjectBytes"),
    maxQueueDepth: boundedInteger(budgetRecord.maxQueueDepth, 1, 1_024, "budgets.maxQueueDepth"),
    maxConcurrentRequests: boundedInteger(budgetRecord.maxConcurrentRequests, 1, 32, "budgets.maxConcurrentRequests"),
  });
  const provenance = record(value.provenance, "domain.provenance");
  exact(provenance, ["inputs"], "domain.provenance");
  if (!Array.isArray(provenance.inputs) || provenance.inputs.length === 0) throw new Error("provenance.inputs must be a nonempty array");
  const seen = new Set<string>();
  const inputs = provenance.inputs.map((entry, index) => {
    const item = record(entry, `provenance.inputs[${index}]`);
    exact(item, ["path", "sha256"], `provenance.inputs[${index}]`);
    const path = boundedText(item.path, 512, `provenance.inputs[${index}].path`);
    if (path.startsWith("/") || path.includes("\\") || path.split("/").some((part) => part === "" || part === "." || part === "..")) {
      throw new Error(`provenance input path must be normalized and relative: ${path}`);
    }
    const sha256 = text(item.sha256, `provenance.inputs[${index}].sha256`);
    if (!/^[0-9a-f]{64}$/.test(sha256)) throw new Error(`provenance input ${path} requires a lowercase SHA-256 digest`);
    if (seen.has(path)) throw new Error(`duplicate provenance input path: ${path}`);
    seen.add(path);
    return freeze({ path, sha256 });
  }).sort((left, right) => compareText(left.path, right.path));
  return freeze({
    id: dnsLabel(value.id, 40, "domain.id"),
    namespace,
    controlNamespace,
    clusterBinding: boundedText(value.clusterBinding, 256, "domain.clusterBinding"),
    policyRef: boundedText(value.policyRef, 256, "domain.policyRef"),
    enforcement: "api-bound",
    budgets,
    minKubernetesVersion: validateMinKubernetesVersion(value.minKubernetesVersion),
    force: optionalBoolean(value.force, "domain.force") ?? false,
    prune: optionalBoolean(value.prune, "domain.prune") ?? false,
    provenance: { inputs },
  });
};

const validateMetadata = (value: unknown, context: string): ObjectMetadata => {
  const metadata = record(value, context);
  exact(metadata, ["name", "namespace", "labels", "annotations"], context);
  const annotations = metadata.annotations === undefined ? undefined : stringMap(metadata.annotations, `${context}.annotations`);
  return freeze({
    name: text(metadata.name, `${context}.name`),
    namespace: text(metadata.namespace, `${context}.namespace`),
    ...(metadata.labels === undefined ? {} : { labels: stringMap(metadata.labels, `${context}.labels`) }),
    ...(annotations === undefined ? {} : { annotations }),
  });
};

const validateServiceSpec = (value: unknown): ServiceDefinition["spec"] => {
  const spec = record(value, "Service.spec");
  exact(spec, ["selector", "ports", "type"], "Service.spec");
  const servicePorts = array(spec.ports, "Service.spec.ports");
  const ports = servicePorts.map((entry, index) => {
    const port = record(entry, `Service.spec.ports[${index}]`);
    exact(port, ["name", "port", "targetPort", "protocol"], `Service.spec.ports[${index}]`);
    return freeze({
      ...(port.name === undefined ? {} : { name: text(port.name, `Service.spec.ports[${index}].name`) }),
      port: boundedInteger(port.port, 1, 65_535, `Service.spec.ports[${index}].port`),
      targetPort: typeof port.targetPort === "number" ? boundedInteger(port.targetPort, 1, 65_535, `Service.spec.ports[${index}].targetPort`) : boundedText(port.targetPort, 2_048, `Service.spec.ports[${index}].targetPort`),
      ...(port.protocol === undefined ? {} : { protocol: enumValue(port.protocol, ["TCP", "UDP", "SCTP"] as const, `Service.spec.ports[${index}].protocol`) }),
    });
  });
  return freeze({
    selector: stringMap(spec.selector, "Service.spec.selector"),
    ports,
    ...(spec.type === undefined ? {} : { type: enumValue(spec.type, ["ClusterIP"] as const, "Service.spec.type") }),
  });
};

const validateDeploymentSpec = (value: unknown): DeploymentDefinition["spec"] => {
  const spec = record(value, "Deployment.spec");
  exact(spec, ["replicas", "selector", "template"], "Deployment.spec");
  const selector = record(spec.selector, "Deployment.spec.selector");
  exact(selector, ["matchLabels"], "Deployment.spec.selector");
  const template = record(spec.template, "Deployment.spec.template");
  exact(template, ["metadata", "spec"], "Deployment.spec.template");
  const metadata = record(template.metadata, "Deployment.spec.template.metadata");
  exact(metadata, ["labels", "annotations"], "Deployment.spec.template.metadata");
  const annotations = metadata.annotations === undefined ? undefined : stringMap(metadata.annotations, "Deployment.spec.template.metadata.annotations");
  if (annotations !== undefined && Object.prototype.hasOwnProperty.call(annotations, "apsides.dev/revision")) {
    throw new Error("apsides.dev/revision is reserved for the rollout primitive");
  }
  const podSpec = record(template.spec, "Deployment.spec.template.spec");
  exact(podSpec, ["serviceAccountName", "automountServiceAccountToken", "containers"], "Deployment.spec.template.spec");
  const automount = optionalBoolean(podSpec.automountServiceAccountToken, "Deployment.spec.template.spec.automountServiceAccountToken");
  const containers = array(podSpec.containers, "Deployment containers").map(validateContainer);
  const names = new Set<string>();
  for (const container of containers) {
    if (names.has(container.name)) throw new Error(`duplicate Deployment container name: ${container.name}`);
    names.add(container.name);
  }
  const matchLabels = stringMap(selector.matchLabels, "Deployment.spec.selector.matchLabels");
  if (Object.keys(matchLabels).length === 0) throw new Error("Deployment selector must not be empty");
  const templateLabels = stringMap(metadata.labels, "Deployment.spec.template.metadata.labels");
  for (const [key, value] of Object.entries(matchLabels)) {
    if (templateLabels[key] !== value) throw new Error(`Deployment template label ${key} does not match selector`);
  }
  return freeze({
    ...(spec.replicas === undefined ? {} : { replicas: boundedInteger(spec.replicas, 0, 10_000, "Deployment.spec.replicas") }),
    selector: { matchLabels },
    template: {
      metadata: {
        labels: templateLabels,
        ...(annotations === undefined ? {} : { annotations }),
      },
      spec: {
        ...(podSpec.serviceAccountName === undefined ? {} : { serviceAccountName: text(podSpec.serviceAccountName, "Deployment.spec.template.spec.serviceAccountName") }),
        ...(automount === undefined ? {} : { automountServiceAccountToken: automount }),
        containers,
      },
    },
  });
};

const validateContainer = (value: unknown, index: number): ContainerDefinition => {
  const context = `Deployment containers[${index}]`;
  const container = record(value, context);
  exact(container, ["name", "image", "imagePullPolicy", "ports", "env", "envFrom", "resources"], context);
  const ports = container.ports === undefined ? undefined : array(container.ports, `${context}.ports`).map((entry, portIndex) => {
    const item = record(entry, `${context}.ports[${portIndex}]`);
    exact(item, ["name", "containerPort", "protocol"], `${context}.ports[${portIndex}]`);
    return freeze({
      ...(item.name === undefined ? {} : { name: text(item.name, `${context}.ports[${portIndex}].name`) }),
      containerPort: boundedInteger(item.containerPort, 1, 65_535, `${context}.ports[${portIndex}].containerPort`),
      ...(item.protocol === undefined ? {} : { protocol: enumValue(item.protocol, ["TCP", "UDP", "SCTP"] as const, `${context}.ports[${portIndex}].protocol`) }),
    });
  });
  const envFrom = container.envFrom === undefined ? undefined : array(container.envFrom, `${context}.envFrom`).map((entry, envIndex) => {
    const item = record(entry, `${context}.envFrom[${envIndex}]`);
    const keys = Object.keys(item);
    if (keys.length !== 1 || (keys[0] !== "configMapRef" && keys[0] !== "secretRef")) throw new Error(`${context}.envFrom[${envIndex}] must have exactly one supported reference`);
    const key = keys[0] as "configMapRef" | "secretRef";
    const reference = record(item[key], `${context}.envFrom[${envIndex}].${key}`);
    exact(reference, ["name"], `${context}.envFrom[${envIndex}].${key}`);
    const name = text(reference.name, `${context}.envFrom[${envIndex}].${key}.name`);
    return key === "configMapRef" ? freeze({ configMapRef: { name } }) : freeze({ secretRef: { name } });
  });
  const envNames = new Set<string>();
  const env = container.env === undefined ? undefined : array(container.env, `${context}.env`).map((entry, envIndex) => {
    const envContext = `${context}.env[${envIndex}]`;
    const item = record(entry, envContext);
    exact(item, ["name", "value", "valueFrom"], envContext);
    const name = text(item.name, `${envContext}.name`);
    if (envNames.has(name)) throw new Error(`duplicate Deployment environment variable name: ${name}`);
    envNames.add(name);
    const hasValue = Object.prototype.hasOwnProperty.call(item, "value");
    const hasValueFrom = Object.prototype.hasOwnProperty.call(item, "valueFrom");
    if (hasValue === hasValueFrom) throw new Error(`${envContext} requires exactly one of value or valueFrom`);
    if (hasValue) {
      if (typeof item.value !== "string") throw new Error(`${envContext}.value must be a string`);
      return freeze({ name, value: item.value });
    }
    const valueFrom = record(item.valueFrom, `${envContext}.valueFrom`);
    exact(valueFrom, ["secretKeyRef"], `${envContext}.valueFrom`);
    const secretKeyRef = record(valueFrom.secretKeyRef, `${envContext}.valueFrom.secretKeyRef`);
    exact(secretKeyRef, ["name", "key"], `${envContext}.valueFrom.secretKeyRef`);
    return freeze({
      name,
      valueFrom: {
        secretKeyRef: {
          name: text(secretKeyRef.name, `${envContext}.valueFrom.secretKeyRef.name`),
          key: text(secretKeyRef.key, `${envContext}.valueFrom.secretKeyRef.key`),
        },
      },
    });
  });
  let resources: ContainerDefinition["resources"];
  if (container.resources !== undefined) {
    const item = record(container.resources, `${context}.resources`);
    exact(item, ["requests", "limits"], `${context}.resources`);
    const requests = item.requests === undefined ? undefined : stringMap(item.requests, `${context}.resources.requests`);
    const limits = item.limits === undefined ? undefined : stringMap(item.limits, `${context}.resources.limits`);
    if (requests !== undefined) exact(requests as Record<string, unknown>, ["cpu", "memory"], `${context}.resources.requests`);
    if (limits !== undefined) exact(limits as Record<string, unknown>, ["cpu", "memory"], `${context}.resources.limits`);
    resources = freeze({
      ...(requests === undefined ? {} : { requests }),
      ...(limits === undefined ? {} : { limits }),
    });
  }
  return freeze({
    name: text(container.name, `${context}.name`),
    image: text(container.image, `${context}.image`),
    ...(container.imagePullPolicy === undefined ? {} : { imagePullPolicy: enumValue(container.imagePullPolicy, ["Always", "IfNotPresent", "Never"] as const, `${context}.imagePullPolicy`) }),
    ...(ports === undefined ? {} : { ports }),
    ...(env === undefined ? {} : { env }),
    ...(envFrom === undefined ? {} : { envFrom }),
    ...(resources === undefined ? {} : { resources }),
  });
};

const validateSource = (value: unknown, context: string): SourceLocation => {
  const source = record(value, context);
  exact(source, ["file", "line", "column"], context);
  return freeze({
    file: boundedText(source.file, 512, `${context}.file`),
    line: boundedInteger(source.line, 1, 4_294_967_295, `${context}.line`),
    column: boundedInteger(source.column, 1, 4_294_967_295, `${context}.column`),
  });
};

const record = (value: unknown, context: string): Record<string, unknown> => {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error(`${context} must be a plain data object`);
  const prototype = Object.getPrototypeOf(value);
  if (prototype !== Object.prototype && prototype !== null) throw new Error(`${context} must be a plain data object`);
  const descriptors = Object.getOwnPropertyDescriptors(value);
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== "string") throw new Error(`${context} must not contain symbol keys`);
    const descriptor = descriptors[key];
    if (descriptor === undefined || !descriptor.enumerable || !("value" in descriptor)) throw new Error(`${context}.${key} must be an enumerable data property`);
  }
  return value as Record<string, unknown>;
};

const exact = (value: Record<string, unknown>, allowed: readonly string[], context: string): void => {
  const allowedKeys = new Set(allowed);
  for (const key of Object.keys(value)) if (!allowedKeys.has(key)) throw new Error(`${context} contains unsupported field ${key}`);
};

const text = (value: unknown, context: string): string => {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${context} must be a nonempty string`);
  return value;
};

const boundedText = (value: unknown, maximum: number, context: string): string => {
  const result = text(value, context);
  if (result.length > maximum) throw new Error(`${context} exceeds ${maximum} characters`);
  return result;
};

const dnsLabelPattern = /^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/;

const dnsLabel = (value: unknown, maximum: number, context: string): string => {
  const result = boundedText(value, maximum, context);
  if (!dnsLabelPattern.test(result)) throw new Error(`${context} must be a lowercase DNS label`);
  return result;
};

const derivedIdLimit = 63;
const derivedIdHashLength = 10;

/**
 * Bound an SDK-derived ID to a DNS label. IDs that already fit are returned unchanged; others become a
 * readable sanitized prefix plus 10 hex characters of a 64-bit FNV-1a hash of the full derived string.
 * A collision is reported by the existing duplicate-ID checks. Author-supplied IDs never pass through here.
 */
const derivedId = (value: string): string => {
  if (value.length <= derivedIdLimit && dnsLabelPattern.test(value)) return value;
  const prefix = value
    .slice(0, derivedIdLimit - derivedIdHashLength - 1)
    .replace(/[^a-z0-9-]/g, "-")
    .replace(/^-+|-+$/g, "");
  let hash = 0xcbf29ce484222325n;
  for (let index = 0; index < value.length; index += 1) {
    hash = ((hash ^ BigInt(value.charCodeAt(index))) * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  const suffix = hash.toString(16).padStart(16, "0").slice(0, derivedIdHashLength);
  return prefix.length === 0 ? suffix : `${prefix}-${suffix}`;
};

const kubernetesName = (value: unknown, context: string): string => {
  const result = boundedText(value, 253, context);
  if (result.split(".").some((part) => !/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(part))) throw new Error(`${context} must be a Kubernetes DNS name`);
  return result;
};

const optionalId = (value: unknown, context: string): string | undefined => {
  return value === undefined ? undefined : text(value, context);
};

/** A normalized relative module path: an optional leading `./` (the natural
 * author-facing form) is stripped; the stored value has no leading slash, no
 * backslash, and no empty/`.`/`..` segment (mirrors the provenance-path
 * contract IR resources and inputs already use). */
const normalizedRelativePath = (value: unknown, context: string): string => {
  const original = boundedText(value, 512, context);
  const result = original.startsWith("./") ? original.slice(2) : original;
  if (result.length === 0 || result.startsWith("/") || result.includes("\\") || result.split("/").some((part) => part === "" || part === "." || part === "..")) {
    throw new Error(`${context} must be a normalized relative path`);
  }
  return result;
};

const jsIdentifier = (value: unknown, context: string): string => {
  const result = boundedText(value, 128, context);
  if (!/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(result)) throw new Error(`${context} must be a valid identifier`);
  return result;
};

const isIpLiteral = (value: string): boolean => {
  return /^[0-9]{1,3}(?:\.[0-9]{1,3}){3}$/.test(value) || value.includes(":");
};

/** Parse and validate one `<On egress>` entry: a lowercase DNS name (never an
 * IP literal or wildcard), with an optional `:port` suffix defaulting to 443. */
const egressHost = (value: unknown, context: string): EgressHost => {
  const raw = text(value, context);
  const separator = raw.lastIndexOf(":");
  const maybePort = separator === -1 ? undefined : raw.slice(separator + 1);
  const hasPort = maybePort !== undefined && /^[0-9]+$/.test(maybePort);
  const host = hasPort ? raw.slice(0, separator) : raw;
  const port = hasPort ? Number(maybePort) : 443;
  if (!Number.isSafeInteger(port) || port < 1 || port > 65_535) throw new Error(`${context} port must be in 1..=65535`);
  if (
    host.length === 0 ||
    host.length > 253 ||
    host.includes("*") ||
    isIpLiteral(host) ||
    !host.split(".").every((part) => dnsLabelPattern.test(part) && part.length <= 63)
  ) {
    throw new Error(`${context} host must be a lowercase DNS name (never an IP literal or wildcard)`);
  }
  return freeze({ host, port });
};

const positiveInteger = (value: unknown, context: string): number => {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) throw new Error(`${context} must be a positive safe integer`);
  return value;
};

const boundedInteger = (value: unknown, minimum: number, maximum: number, context: string): number => {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${context} must be an integer in ${minimum}..=${maximum}`);
  }
  return value;
};

const enumValue = <const T extends readonly string[]>(value: unknown, allowed: T, context: string): T[number] => {
  if (typeof value !== "string" || !allowed.includes(value)) throw new Error(`${context} must be one of ${allowed.join(", ")}`);
  return value as T[number];
};

const projection = (value: unknown, context: string): void => {
  if (value === "data") return;
  if (!Array.isArray(value) || value.length !== 1 || value[0] !== "data") throw new Error(`${context} supports only data`);
};

const array = (value: unknown, context: string): unknown[] => {
  if (!Array.isArray(value) || value.length === 0 || value.length > 128) throw new Error(`${context} must be a nonempty array of at most 128 entries`);
  return value;
};

const stringMap = (value: unknown, context: string): Readonly<Record<string, string>> => {
  const map = record(value, context);
  if (Object.keys(map).length > 1_024) throw new Error(`${context} exceeds 1024 entries`);
  const output: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const key of Object.keys(map).sort()) {
    if (key.length === 0 || key.length > 253) throw new Error(`${context} contains an invalid or oversized key`);
    if (typeof map[key] !== "string") throw new Error(`${context}.${key} must be a string`);
    output[key] = map[key];
  }
  return freeze(output);
};

const sorted = <T>(values: Map<string, T>): T[] => {
  return [...values.entries()].sort(([left], [right]) => compareText(left, right)).map(([, value]) => value);
};

const compareText = (left: string, right: string): number => {
  return left < right ? -1 : left > right ? 1 : 0;
};

const canonical = (value: unknown, seen = new Set<object>()): Json => {
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new Error("non-finite numbers are not valid IR data");
    return value;
  }
  if (typeof value !== "object") throw new Error(`unsupported IR data type: ${typeof value}`);
  if (seen.has(value)) throw new Error("cyclic values are not valid IR data");
  seen.add(value);
  if (Array.isArray(value)) {
    const result = value.map((entry) => canonical(entry, seen));
    seen.delete(value);
    return result;
  }
  const source = record(value, "IR value");
  const result: Record<string, Json> = Object.create(null) as Record<string, Json>;
  for (const key of Object.keys(source).sort()) result[key] = canonical(source[key], seen);
  seen.delete(value);
  return result;
};

const freeze = <T>(value: T, seen = new Set<object>()): T => {
  if (value === null || typeof value !== "object" || seen.has(value)) return value;
  seen.add(value);
  for (const child of Object.values(value)) freeze(child, seen);
  return Object.freeze(value);
};

const utf8Length = (value: string): number => {
  let bytes = 0;
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff && index + 1 < value.length && value.charCodeAt(index + 1) >= 0xdc00 && value.charCodeAt(index + 1) <= 0xdfff) { bytes += 4; index += 1; }
    else bytes += 3;
  }
  return bytes;
};

const optionalBoolean = (value: unknown, label: string): boolean | undefined => {
  if (value === undefined) return undefined;
  if (typeof value !== "boolean") throw new Error(`${label} must be a boolean`);
  return value;
};
