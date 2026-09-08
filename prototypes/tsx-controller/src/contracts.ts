/** Shared contract for the isolated TSX prototype. Not a stable public API. */
export type ResourceKind =
  | "kubernetes:Deployment"
  | "kubernetes:Service"
  | "kubernetes:ConfigMap"
  | "planetscale:Database"
  | "cloudflare:DNSRecord";

export type Access = "read" | "patch" | "delete";

export interface ResourceRef<T = Record<string, unknown>> {
  id?: string;
  output<K extends keyof T & string>(key: K): Output<T[K]>;
}

export interface Output<T = unknown> {
  readonly tag: "output";
  readonly ref: ResourceRef<any>;
  readonly path: readonly string[];
  /** Type witness only; output values are resolved by the controller. */
  readonly valueType?: T;
}

export interface Observation {
  kind: ResourceKind;
  name: string;
  namespace?: string;
  access: Access[];
}

export interface ObservedResource {
  readonly tag: "observed";
  readonly target: Observation;
  readonly data: Record<string, string>;
  readonly binaryData?: Record<string, string>;
}

export interface ResourceHandle {
  get(options?: { signal?: AbortSignal }): Promise<Record<string, unknown> | undefined>;
  ensureDeleted(options?: { signal?: AbortSignal }): Promise<void>;
  patch(value: Record<string, unknown>, options?: { signal?: AbortSignal }): Promise<void>;
}

export interface ResourceOperations {
  get(target: Observation, signal?: AbortSignal): Promise<Record<string, unknown> | undefined>;
  ensureDeleted(target: Observation, signal?: AbortSignal): Promise<void>;
  patch(target: Observation, value: Record<string, unknown>, signal?: AbortSignal): Promise<void>;
}

export interface FinalizerContext {
  signal: AbortSignal;
  resource: { id: string; name: string; uid: string; outputs: Record<string, unknown> };
}

export type Finalizer = (context: FinalizerContext) => Promise<void>;

export interface ResourceNode {
  id: string;
  kind: ResourceKind;
  name: string;
  namespace?: string;
  props: Record<string, unknown>;
  dependsOn: string[];
  readyAfter: string[];
  finalizers: Record<string, Finalizer>;
  deletionPolicy: "delete" | "retain";
}

export interface ResourceGraph {
  name: string;
  namespace: string;
  resources: ResourceNode[];
  observations: Observation[];
}

export interface ResolvedResource {
  id: string;
  kind: ResourceKind;
  name: string;
  namespace?: string;
  props: Record<string, unknown>;
  finalizerKeys: string[];
  deletionPolicy: "delete" | "retain";
}

export interface LiveResource {
  id: string;
  uid: string;
  kind: ResourceKind;
  name: string;
  namespace?: string;
  props: Record<string, unknown>;
  outputs: Record<string, unknown>;
  ready: boolean;
  finalizers: string[];
  deleting: boolean;
}

/** An engine owns application and persistence; the runner supplies observation triggers. */
export interface ResourceEngine {
  observe(id: string): Promise<LiveResource | undefined>;
  /** Only active IDs may mutate; omitted activeIds means all supplied resources. */
  apply(resources: readonly ResolvedResource[], activeIds?: readonly string[]): Promise<void>;
  requestDelete(id: string, uid: string): Promise<void>;
  removeFinalizer(id: string, uid: string, key: string): Promise<void>;
  operations: ResourceOperations;
}
