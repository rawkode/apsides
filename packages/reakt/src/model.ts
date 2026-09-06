export const schemaVersion = "reakt.dev/ir/v1" as const;
export const compilerVersion = "reakt/0.1" as const;

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

const resourceState = Symbol("reakt.resource");
const managedState = Symbol("reakt.managed");
const inputState = Symbol("reakt.input");
const managedInputFor = Symbol("reakt.managed-input");
const declareRollout = Symbol("reakt.declare-rollout");

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
}

interface InputDeclaration {
  readonly id: string;
  readonly apiVersion: "v1";
  readonly kind: "ConfigMap" | "Secret";
  readonly name: string;
  readonly namespace: string;
  readonly mode: "managed" | "observed";
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

export interface ReaktIR {
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
  };
  readonly resources: readonly ResourceDeclaration[];
  readonly inputs: readonly InputDeclaration[];
  readonly reactions: readonly ReactionDeclaration[];
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
    };
  }

  manage<R extends ManageableResource>(resource: R, options: ManageOptions): ManagedResource<R> {
    if (!(resource instanceof ConfigMap || resource instanceof Service || resource instanceof Deployment || resource instanceof NamespacedResource)) {
      throw new Error("manage() accepts only Reakt resource values");
    }
    const rawOptions = record(options, "manage options");
    exact(rawOptions, ["id", "deletionPolicy", "force", "source"], "manage options");
    const definition = resource[resourceState];
    const source = validateSource(rawOptions.source, "manage.source");
    const name = kubernetesName(definition.desired.metadata.name, `${definition.kind}.metadata.name`);
    const namespace = dnsLabel(definition.desired.metadata.namespace, 63, `${definition.kind}.metadata.namespace`);
    this.#requireNamespace(namespace, `${definition.kind} ${name}`);
    const id = dnsLabel(optionalId(rawOptions.id, "manage.id") ?? definition.id ?? `${definition.kind.toLowerCase()}-${name}`, 63, "manage.id");
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
    const id = dnsLabel(optionalId(raw.id, "observe.id") ?? `observed-${kind.kind.toLowerCase()}-${name}`, 63, "observe.id");
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

  toIR(): ReaktIR {
    const sources = [
      ...this.#state.resources.values(),
      ...this.#state.inputs.values(),
      ...this.#state.reactions.values(),
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
      provenance: { compiler: compilerVersion, inputs: this.#state.options.provenance.inputs },
    }) as unknown as ReaktIR);
  }

  [managedInputFor](resource: ManagedResource<ConfigMap>): InputDeclaration {
    const state = resource[managedState];
    if (state.domain !== this) throw new Error("cross-domain watch is unsupported");
    const managed = state.declaration;
    if (managed.kind !== "ConfigMap") throw new Error("only ConfigMap data can be watched in this release");
    const id = dnsLabel(`managed-${managed.id}`, 63, "managed input id");
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
    const id = dnsLabel(optionalId(raw.id, "rollout.id") ?? `rollout-${input.id}-${target.id}`, 63, "rollout.id");
    if (this.#state.reactions.has(id)) throw new Error(`duplicate reaction id: ${id}`);
    if (this.#state.reactions.size >= this.#state.options.budgets.maxReactions) throw new Error("domain maxReactions budget exceeded");
    const source = raw.source === undefined ? input.source : validateSource(raw.source, "rollout.source");
    this.#state.reactions.set(id, freeze({ id, op: "rollout", inputs: [input.id], target: target.id, source }));
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

export function defineDomain(options: DomainOptions): Domain {
  return new Domain(options);
}

export function validateNamespacedResourceDescriptor(value: unknown): NamespacedResourceDescriptor {
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
}

type Watchable = ManagedResource<ConfigMap> | ObservedInput;

export function watch(input: Watchable): {
  project(projectionName: "data"): {
    rollout(target: ManagedResource<Deployment>, options?: RolloutOptions): void;
  };
} {
  if (!(input instanceof ManagedResource || input instanceof ObservedInput)) throw new Error("watch() requires a managed ConfigMap or observed input");
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
}

export type InputFixtureState =
  | { readonly state: "present"; readonly data: Readonly<Record<string, string>> }
  | { readonly state: "missing" | "unauthorized" | "unavailable" | "invalid" };

export interface InputFixture {
  readonly inputId: string;
  readonly source: SourceLocation;
  /** Author assertion checked as part of the test-only fixture contract. */
  readonly synthetic: true;
  readonly state: InputFixtureState["state"];
  readonly data?: Readonly<Record<string, string>>;
}

export interface WorkloadFixture {
  readonly targetResourceId: string;
  readonly source: SourceLocation;
  readonly containers: readonly {
    readonly name: string;
    readonly image: string;
    readonly imagePullPolicy: "Never";
  }[];
}

export interface FixtureBundle {
  readonly schemaVersion: "reakt.dev/fixtures/v1";
  readonly testOnly: true;
  readonly productionReady: false;
  readonly inputs: readonly InputFixture[];
  readonly workloadSubstitutions: readonly WorkloadFixture[];
}

export interface GeneratedFixtureOptions {
  /** A locally imported image reference. Test compilation always sets Never. */
  readonly workloadImage: string;
  readonly inputs?: readonly InputFixture[];
  readonly workloadSubstitutions?: readonly WorkloadFixture[];
}

/** Produces a visibly test-only companion artifact; fixture values never enter Domain.toIR(). */
export function defineFixtureBundle(options: {
  readonly inputs?: readonly InputFixture[];
  readonly workloadSubstitutions?: readonly WorkloadFixture[];
}): FixtureBundle {
  const raw = record(options, "fixture bundle");
  exact(raw, ["inputs", "workloadSubstitutions"], "fixture bundle");
  const inputs = raw.inputs === undefined ? [] : array(raw.inputs, "fixture bundle.inputs") as InputFixture[];
  const workloadSubstitutions = raw.workloadSubstitutions === undefined ? [] : array(raw.workloadSubstitutions, "fixture bundle.workloadSubstitutions") as WorkloadFixture[];
  uniqueBy(inputs, (input) => input.inputId, "fixture input");
  uniqueBy(workloadSubstitutions, (fixture) => fixture.targetResourceId, "workload fixture target");
  return freeze(canonical({
    schemaVersion: "reakt.dev/fixtures/v1",
    testOnly: true,
    productionReady: false,
    inputs: [...inputs].sort((left, right) => compareText(left.inputId, right.inputId)),
    workloadSubstitutions: [...workloadSubstitutions].sort((left, right) => compareText(left.targetResourceId, right.targetResourceId)),
  }) as unknown as FixtureBundle);
}

/**
 * Completes the v1 fixture contract from the production IR. Rollout only needs
 * stable synthetic data, so observed ConfigMaps and Secrets have a supported,
 * deterministic generator. Explicit fixtures override the generated seed.
 */
export function generateFixtureBundle(ir: ReaktIR, options: GeneratedFixtureOptions): FixtureBundle {
  const raw = record(options, "generated fixture options");
  exact(raw, ["workloadImage", "inputs", "workloadSubstitutions"], "generated fixture options");
  if (ir.testOnly !== false) throw new Error("fixtures must be generated from production IR");
  const image = text(raw.workloadImage, "generated fixture workloadImage");
  const explicitInputs = raw.inputs === undefined ? [] : array(raw.inputs, "generated fixture inputs") as InputFixture[];
  const explicitWorkloads = raw.workloadSubstitutions === undefined ? [] : array(raw.workloadSubstitutions, "generated workload substitutions") as WorkloadFixture[];
  uniqueBy(explicitInputs, (fixture) => fixture.inputId, "fixture input");
  uniqueBy(explicitWorkloads, (fixture) => fixture.targetResourceId, "workload fixture target");
  const byInput = new Map(explicitInputs.map((fixture) => [fixture.inputId, fixture]));
  const byWorkload = new Map(explicitWorkloads.map((fixture) => [fixture.targetResourceId, fixture]));
  const inputs = ir.inputs.filter((input) => input.mode === "observed").map((input) => byInput.get(input.id) ?? freeze({
    inputId: input.id,
    source: input.source,
    synthetic: true as const,
    state: "present" as const,
    data: freeze({ value: input.kind === "Secret" ? `synthetic-secret-${input.id}` : `synthetic-config-${input.id}` }),
  }));
  for (const fixture of explicitInputs) {
    const input = ir.inputs.find((candidate) => candidate.id === fixture.inputId);
    if (input === undefined || input.mode !== "observed") throw new Error(`fixture input is not a declared observed input: ${fixture.inputId}`);
  }
  const workloads = ir.resources.filter((resource) => resource.kind === "Deployment").map((resource) => {
    const explicit = byWorkload.get(resource.id);
    if (explicit !== undefined) return explicit;
    const desired = resource.desired as DeploymentDesired;
    return freeze({
      targetResourceId: resource.id,
      source: resource.source,
      containers: desired.spec.template.spec.containers.map((container) => freeze({ name: container.name, image, imagePullPolicy: "Never" as const })),
    });
  });
  for (const fixture of explicitWorkloads) {
    const resource = ir.resources.find((candidate) => candidate.id === fixture.targetResourceId);
    if (resource?.kind !== "Deployment") throw new Error(`workload fixture is not a declared Deployment: ${fixture.targetResourceId}`);
  }
  return defineFixtureBundle({ inputs, workloadSubstitutions: workloads });
}

/** Materializes a fixture-substituted IR for a compiler's explicitly test-only path. */
export function materializeFixtureIR(ir: ReaktIR, bundle: FixtureBundle): ReaktIR {
  if (bundle.testOnly !== true || bundle.productionReady !== false || bundle.schemaVersion !== "reakt.dev/fixtures/v1") {
    throw new Error("fixture IR requires a recognized non-production fixture bundle");
  }
  const output = canonical(ir) as unknown as ReaktIR;
  const resources = output.resources.map((resource) => {
    const substitution = bundle.workloadSubstitutions.find((candidate) => candidate.targetResourceId === resource.id);
    if (substitution === undefined) return resource;
    if (resource.kind !== "Deployment") throw new Error(`workload substitution target is not a Deployment: ${resource.id}`);
    const desired = resource.desired as DeploymentDesired;
    const expected = desired.spec.template.spec.containers.map((container) => container.name).sort();
    const replacements = substitution.containers.map((container) => container.name).sort();
    if (JSON.stringify(expected) !== JSON.stringify(replacements)) throw new Error(`workload substitution does not cover every container in ${resource.id}`);
    const byName = new Map(substitution.containers.map((container) => [container.name, container]));
    return {
      ...resource,
      desired: {
        ...desired,
        spec: {
          ...desired.spec,
          template: {
            ...desired.spec.template,
            spec: {
              ...desired.spec.template.spec,
              containers: desired.spec.template.spec.containers.map((container) => {
                const replacement = byName.get(container.name)!;
                return { ...container, image: replacement.image, imagePullPolicy: "Never" as const };
              }),
            },
          },
        },
      },
    };
  });
  for (const substitution of bundle.workloadSubstitutions) {
    if (!resources.some((resource) => resource.id === substitution.targetResourceId)) throw new Error(`unresolved workload substitution target: ${substitution.targetResourceId}`);
  }
  if (ir.testOnly !== false) throw new Error("fixture materialization requires production IR");
  return freeze(canonical({ ...output, testOnly: true, resources }) as unknown as ReaktIR);
}

function validateDomainOptions(options: DomainOptions): DomainOptions {
  const value = record(options, "domain options");
  exact(value, ["id", "namespace", "controlNamespace", "clusterBinding", "policyRef", "enforcement", "budgets", "provenance", "force", "prune"], "domain options");
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
    force: optionalBoolean(value.force, "domain.force") ?? false,
    prune: optionalBoolean(value.prune, "domain.prune") ?? false,
    provenance: { inputs },
  });
}

function validateMetadata(value: unknown, context: string): ObjectMetadata {
  const metadata = record(value, context);
  exact(metadata, ["name", "namespace", "labels", "annotations"], context);
  const annotations = metadata.annotations === undefined ? undefined : stringMap(metadata.annotations, `${context}.annotations`);
  return freeze({
    name: text(metadata.name, `${context}.name`),
    namespace: text(metadata.namespace, `${context}.namespace`),
    ...(metadata.labels === undefined ? {} : { labels: stringMap(metadata.labels, `${context}.labels`) }),
    ...(annotations === undefined ? {} : { annotations }),
  });
}

function validateServiceSpec(value: unknown): ServiceDefinition["spec"] {
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
}

function validateDeploymentSpec(value: unknown): DeploymentDefinition["spec"] {
  const spec = record(value, "Deployment.spec");
  exact(spec, ["replicas", "selector", "template"], "Deployment.spec");
  const selector = record(spec.selector, "Deployment.spec.selector");
  exact(selector, ["matchLabels"], "Deployment.spec.selector");
  const template = record(spec.template, "Deployment.spec.template");
  exact(template, ["metadata", "spec"], "Deployment.spec.template");
  const metadata = record(template.metadata, "Deployment.spec.template.metadata");
  exact(metadata, ["labels", "annotations"], "Deployment.spec.template.metadata");
  const annotations = metadata.annotations === undefined ? undefined : stringMap(metadata.annotations, "Deployment.spec.template.metadata.annotations");
  if (annotations !== undefined && Object.prototype.hasOwnProperty.call(annotations, "reakt.dev/revision")) {
    throw new Error("reakt.dev/revision is reserved for the rollout primitive");
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
}

function validateContainer(value: unknown, index: number): ContainerDefinition {
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
}

function validateSource(value: unknown, context: string): SourceLocation {
  const source = record(value, context);
  exact(source, ["file", "line", "column"], context);
  return freeze({
    file: boundedText(source.file, 512, `${context}.file`),
    line: boundedInteger(source.line, 1, 4_294_967_295, `${context}.line`),
    column: boundedInteger(source.column, 1, 4_294_967_295, `${context}.column`),
  });
}

function record(value: unknown, context: string): Record<string, unknown> {
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
}

function exact(value: Record<string, unknown>, allowed: readonly string[], context: string): void {
  const allowedKeys = new Set(allowed);
  for (const key of Object.keys(value)) if (!allowedKeys.has(key)) throw new Error(`${context} contains unsupported field ${key}`);
}

function text(value: unknown, context: string): string {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${context} must be a nonempty string`);
  return value;
}

function boundedText(value: unknown, maximum: number, context: string): string {
  const result = text(value, context);
  if (result.length > maximum) throw new Error(`${context} exceeds ${maximum} characters`);
  return result;
}

function dnsLabel(value: unknown, maximum: number, context: string): string {
  const result = boundedText(value, maximum, context);
  if (!/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(result)) throw new Error(`${context} must be a lowercase DNS label`);
  return result;
}

function kubernetesName(value: unknown, context: string): string {
  const result = boundedText(value, 253, context);
  if (result.split(".").some((part) => !/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(part))) throw new Error(`${context} must be a Kubernetes DNS name`);
  return result;
}

function optionalId(value: unknown, context: string): string | undefined {
  return value === undefined ? undefined : text(value, context);
}

function positiveInteger(value: unknown, context: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) throw new Error(`${context} must be a positive safe integer`);
  return value;
}

function boundedInteger(value: unknown, minimum: number, maximum: number, context: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${context} must be an integer in ${minimum}..=${maximum}`);
  }
  return value;
}

function enumValue<const T extends readonly string[]>(value: unknown, allowed: T, context: string): T[number] {
  if (typeof value !== "string" || !allowed.includes(value)) throw new Error(`${context} must be one of ${allowed.join(", ")}`);
  return value as T[number];
}

function projection(value: unknown, context: string): void {
  if (value === "data") return;
  if (!Array.isArray(value) || value.length !== 1 || value[0] !== "data") throw new Error(`${context} supports only data`);
}

function array(value: unknown, context: string): unknown[] {
  if (!Array.isArray(value) || value.length === 0 || value.length > 128) throw new Error(`${context} must be a nonempty array of at most 128 entries`);
  return value;
}

function stringMap(value: unknown, context: string): Readonly<Record<string, string>> {
  const map = record(value, context);
  if (Object.keys(map).length > 1_024) throw new Error(`${context} exceeds 1024 entries`);
  const output: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const key of Object.keys(map).sort()) {
    if (key.length === 0 || key.length > 253) throw new Error(`${context} contains an invalid or oversized key`);
    if (typeof map[key] !== "string") throw new Error(`${context}.${key} must be a string`);
    output[key] = map[key];
  }
  return freeze(output);
}

function sorted<T>(values: Map<string, T>): T[] {
  return [...values.entries()].sort(([left], [right]) => compareText(left, right)).map(([, value]) => value);
}

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function uniqueBy<T>(values: readonly T[], key: (value: T) => string, context: string): void {
  const seen = new Set<string>();
  for (const value of values) {
    const id = key(value);
    if (seen.has(id)) throw new Error(`duplicate ${context}: ${id}`);
    seen.add(id);
  }
}

function canonical(value: unknown, seen = new Set<object>()): Json {
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
}

function freeze<T>(value: T, seen = new Set<object>()): T {
  if (value === null || typeof value !== "object" || seen.has(value)) return value;
  seen.add(value);
  for (const child of Object.values(value)) freeze(child, seen);
  return Object.freeze(value);
}

function utf8Length(value: string): number {
  let bytes = 0;
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff && index + 1 < value.length && value.charCodeAt(index + 1) >= 0xdc00 && value.charCodeAt(index + 1) <= 0xdfff) { bytes += 4; index += 1; }
    else bytes += 3;
  }
  return bytes;
}

function optionalBoolean(value: unknown, label: string): boolean | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "boolean") throw new Error(`${label} must be a boolean`);
  return value;
}
