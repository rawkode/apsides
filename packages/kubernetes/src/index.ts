import { Fragment, jsx, jsxs, explicitEmpty, type Element } from "@oxideloop/react-like-runtime";
import { createRenderer } from "@oxideloop/react-like-runtime";
import {
  defineAction as defineSdkAction,
  defineController,
  type ActionDescriptor,
  type ActionSource,
  type CompilerActionBindings,
  type ConfigMapCurrent,
  type Controller,
  type DeepReadonly,
  type Json,
  type ResourceDescriptor,
  type ResourceRequest,
  type ResourceObject,
  type ResourceRef,
} from "@oxideloop/sdk";
import { useAutomation, useEnrichment, useResource as sdkUseResource, withHookEnvironment, type HookSession, type ResourceRead } from "@oxideloop/sdk/hooks";
import type { ActionInputV0, ActionOutputV0, RenderInputV0, RenderOutputV0 } from "@oxideloop/sdk";

const ABI = "renderer.oxideloop.dev/v0";
const MAX_DEPTH = 64;
const MAX_CANONICAL_BYTES = 2 * 1024 * 1024;
const MAX_STRING_BYTES = 256 * 1024;

export { Fragment, jsx, jsxs, explicitEmpty, defineController, useAutomation, useEnrichment, withHookEnvironment, createRenderer };
export type { ActionDescriptor, ActionSource, Controller, Json, ResourceDescriptor, ResourceObject };

type SemanticComponent<Props> = ((props: Props) => Element) & { readonly __oxideloopSemantic: string };
type ResourceSemanticComponent<Props, Object extends ResourceObject> = SemanticComponent<Props> & ResourceDescriptor<Object>;
type Children = { readonly children?: unknown };
export interface ConfigMapProps extends Children { readonly id: string; readonly name: string; }
export interface DeploymentProps extends Children { readonly id: string; readonly name: string; readonly replicas?: number; readonly selector: Readonly<Record<string, string>>; }
export interface ServiceProps extends Children { readonly id: string; readonly name: string; readonly selector: Readonly<Record<string, string>>; }
export interface ContainerProps extends Children { readonly name: string; readonly image: string; }
export interface DataProps { readonly name: string; readonly value: string; }
export interface ContainerPortProps { readonly name: string; readonly containerPort: number; }
export interface ServicePortProps { readonly name: string; readonly port: number; readonly targetPort: string | number; }
export interface EnvProps { readonly name: string; readonly configMap: { readonly name: string; readonly key: string }; }
export interface ResourcesProps { readonly requests: Readonly<Record<"cpu" | "memory", string>> | Readonly<Partial<Record<"cpu" | "memory", string>>>; readonly limits: Readonly<Record<"cpu" | "memory", string>> | Readonly<Partial<Record<"cpu" | "memory", string>>>; }
export interface EventProps { readonly id: string; readonly regarding: ActionSource; readonly reason: string; readonly action: string; readonly note: string; }

export const ConfigMap: ResourceSemanticComponent<ConfigMapProps, ConfigMapCurrent> = component("ConfigMap", "v1", "ConfigMap") as ResourceSemanticComponent<ConfigMapProps, ConfigMapCurrent>;
export const Deployment = component<DeploymentProps>("Deployment");
export const Service = component<ServiceProps>("Service");
export const Container = component<ContainerProps>("Container");
export const Data = component<DataProps>("Data");
export const Port = component<ContainerPortProps | ServicePortProps>("Port");
export const Env = component<EnvProps>("Env");
export const Resources = component<ResourcesProps>("Resources");
export const Event = component<EventProps>("Event");

function component<Props>(semantic: string, apiVersion = "oxideloop.dev/v1", kind = `Semantic${semantic}`): SemanticComponent<Props> {
  const impossible = (() => { throw new Error("semantic components are only valid in TSX"); }) as unknown as SemanticComponent<Props>;
  return Object.freeze(Object.assign(impossible, { apiVersion, kind, __oxideloopSemantic: semantic }));
}

/** The public v1 action surface returns an Event element but remains SDK-branded. */
export interface ActionDefinition<Metadata extends Json> {
  readonly id: string;
  readonly version: string;
  readonly run: (context: { readonly source: ActionSource; readonly metadata: DeepReadonly<Metadata> }) => Element;
}
export function defineAction<Metadata extends Json>(definition: ActionDefinition<Metadata>): ActionDescriptor<Metadata> {
  return defineSdkAction(definition as unknown as Parameters<typeof defineSdkAction>[0]) as ActionDescriptor<Metadata>;
}

/** Converts a semantic component descriptor to the SDK's data-only lookup descriptor. */
export function useResource<T extends ResourceObject>(descriptor: ResourceDescriptor<T>, request: ResourceRequest): ResourceRead<T> {
  return sdkUseResource<T>({ apiVersion: descriptor.apiVersion, kind: descriptor.kind }, request);
}

/** Invokes a controller once under one hook session and produces the canonical ABI output. */
export function renderController<Spec>(controller: Controller<Spec, unknown>, input: RenderInputV0, compilerActionBindings?: CompilerActionBindings): RenderOutputV0 {
  const canonical = snapshot(input).value as RenderInputV0;
  if (canonical.abiVersion !== ABI) throw new Error("invalid render ABI version");
  const root = canonical.root;
  requireText(root.namespace, "root.namespace"); requireText(root.name, "root.name");
  const environment = {
    observationToken: canonical.observationToken,
    ...(compilerActionBindings === undefined ? {} : { compilerActionBindings }),
    read<T extends ResourceObject>(ref: ResourceRef) {
      const namespace = ref.namespace ?? root.namespace;
      const observation = canonical.observations.find((item) => item.apiVersion === ref.apiVersion && item.kind === ref.kind && item.namespace === namespace && item.name === ref.name);
      const present = observation !== undefined && observation.value !== null;
      return Object.freeze({ ref: Object.freeze({ ...ref, namespace }), present, value: present ? observation!.value as unknown as T : undefined, observation: present ? { uid: observation!.uid, resourceVersion: observation!.resourceVersion } : { uid: null, resourceVersion: null } });
    },
  };
  let graph: ReturnType<ReturnType<typeof createRenderer>["render"]> | undefined;
  withHookEnvironment(environment, (session: HookSession) => {
    const tree = controller({ root: { metadata: { name: root.name, namespace: root.namespace }, spec: root.spec as Spec } });
    graph = createRenderer(session).render(tree as Element);
  });
  if (!graph) throw new Error("controller did not produce a render graph");
  const rows = graph.resources.map((resource) => {
    const record = snapshot(resource).value as Record<string, unknown>;
    const logicalId = record.__logicalId;
    if (typeof logicalId !== "string" || !logicalId) throw new Error("semantic resource has no logical id");
    const { __logicalId: _internal, ...rest } = record;
    const sourceMetadata = plain(rest.metadata, "resource.metadata");
    const metadata = { ...sourceMetadata, ...(sourceMetadata.namespace === undefined ? { namespace: root.namespace } : {}) };
    return { logicalId, object: { ...rest, metadata } as Record<string, unknown>, readiness: null };
  });
  const logical = new Set<string>(); const physical = new Set<string>();
  for (const row of rows) {
    const object = row.object; const metadata = plain(object.metadata, "resource.metadata");
    const key = [text(object.apiVersion, "resource.apiVersion"), text(object.kind, "resource.kind"), text(metadata.namespace, "resource.metadata.namespace"), text(metadata.name, "resource.metadata.name")].join("\0");
    if (logical.has(row.logicalId)) throw new Error(`duplicate logical ID: ${row.logicalId}`);
    if (physical.has(key)) throw new Error(`duplicate managed resource: ${key}`);
    logical.add(row.logicalId); physical.add(key);
  }
  rows.sort((left, right) => compareTuple(resourceKey(left), resourceKey(right)));
  return { abiVersion: ABI, disposition: graph.disposition, resources: rows, dependencies: [...graph.dependencies], automations: [...graph.automations], diagnostics: [] } as RenderOutputV0;
}

/** Lowers the sole action Event to a parse/validate-compatible ActionOutputV0. */
export function renderAction<Metadata extends Json>(action: ActionDescriptor<Metadata>, input: ActionInputV0): ActionOutputV0 {
  const canonical = snapshot(input).value as ActionInputV0;
  exact(plain(canonical, "action input"), ["abiVersion", "actionModuleDigest", "actionModuleIdentity", "automationIdentity", "automationVersion", "bundleDigest", "eventMetadata", "occurrenceId", "sourceResourceVersion", "sourceTransitionId", "sourceUid"], "action input");
  if (canonical.abiVersion !== ABI || canonical.automationIdentity !== action.id || canonical.automationVersion !== action.version || canonical.actionModuleIdentity !== action.id) throw new Error("action input does not match the compiled action");
  rawDigest(canonical.bundleDigest, "bundleDigest"); rawDigest(canonical.actionModuleDigest, "actionModuleDigest");
  requireText(canonical.sourceTransitionId, "sourceTransitionId"); requireText(canonical.sourceUid, "sourceUid"); requireText(canonical.sourceResourceVersion, "sourceResourceVersion");
  const envelope = actionEnvelope(canonical.eventMetadata);
  if (envelope.source.uid !== canonical.sourceUid || envelope.source.resourceVersion !== canonical.sourceResourceVersion) throw new Error("action context source does not match action input");
  const event = (action.run as unknown as (context: { readonly source: ActionSource; readonly metadata: Json }) => unknown)({ source: envelope.source, metadata: envelope.metadata });
  if (!isElement(event) || semantic(event.type) !== "Event") throw new Error("defineAction run must return exactly one Event");
  const props = plain(event.props, "Event"); exact(props, ["action", "id", "note", "reason", "regarding"], "Event");
  const regarding = sourceFrom(props.regarding);
  if (!sameSource(regarding, envelope.source)) throw new Error("Event regarding must match the exact action source");
  const eventId = requireText(props.id, "Event.id");
  return { abiVersion: ABI, actions: [{ logicalId: `${eventId}/${requireText(canonical.occurrenceId, "occurrenceId")}`, kind: "Event", specification: {
    apiVersion: "events.k8s.io/v1", kind: "Event", metadata: { namespace: envelope.source.namespace },
    regarding: { apiVersion: envelope.source.apiVersion, kind: envelope.source.kind, namespace: envelope.source.namespace, name: envelope.source.name },
    action: requireText(props.action, "Event.action"), reason: requireText(props.reason, "Event.reason"), note: requireText(props.note, "Event.note"), type: "Normal",
  } }], diagnostics: [] };
}

interface ActionEnvelope { readonly source: ActionSource; readonly metadata: Json; }
function actionEnvelope(value: unknown): ActionEnvelope {
  const eventMetadata = plain(snapshot(value).value, "eventMetadata"); exact(eventMetadata, ["$oxideloop"], "eventMetadata");
  const envelope = plain(eventMetadata.$oxideloop, "eventMetadata.$oxideloop"); exact(envelope, ["kind", "metadata", "source", "version"], "eventMetadata.$oxideloop");
  if (envelope.kind !== "action-context" || envelope.version !== "v1") throw new Error("unsupported action context");
  return Object.freeze({ source: Object.freeze(sourceFrom(envelope.source)), metadata: snapshot(envelope.metadata).value as Json });
}
function sourceFrom(value: unknown): ActionSource {
  const source = plain(value, "action source"); exact(source, ["apiVersion", "kind", "name", "namespace", "resourceVersion", "uid"], "action source");
  return { apiVersion: requireText(source.apiVersion, "source.apiVersion"), kind: requireText(source.kind, "source.kind"), namespace: requireText(source.namespace, "source.namespace"), name: requireText(source.name, "source.name"), uid: requireText(source.uid, "source.uid"), resourceVersion: requireText(source.resourceVersion, "source.resourceVersion") };
}
function sameSource(left: ActionSource, right: ActionSource): boolean { return left.apiVersion === right.apiVersion && left.kind === right.kind && left.namespace === right.namespace && left.name === right.name && left.uid === right.uid && left.resourceVersion === right.resourceVersion; }
function resourceKey(row: { readonly logicalId: string; readonly object: Record<string, unknown> }): readonly string[] { const metadata = plain(row.object.metadata, "resource.metadata"); return [text(row.object.apiVersion, "resource.apiVersion"), text(row.object.kind, "resource.kind"), text(metadata.namespace, "resource.metadata.namespace"), text(metadata.name, "resource.metadata.name"), row.logicalId]; }
function compareTuple(left: readonly string[], right: readonly string[]): number { for (let index = 0; index < left.length; index += 1) { if (left[index]! < right[index]!) return -1; if (left[index]! > right[index]!) return 1; } return 0; }
function semantic(type: unknown): string | undefined { if ((typeof type !== "object" && typeof type !== "function") || type === null) return undefined; const value = (type as Record<string, unknown>).__oxideloopSemantic; return typeof value === "string" ? value : undefined; }
function isElement(value: unknown): value is Element { return value !== null && typeof value === "object" && "type" in value && "props" in value; }
function text(value: unknown, owner: string): string { if (typeof value !== "string" || !value) throw new Error(`${owner} must be a nonempty string`); return value; }
function requireText(value: unknown, owner: string): string { return text(value, owner); }
function rawDigest(value: unknown, owner: string): void { if (typeof value !== "string" || !/^[0-9a-f]{64}$/.test(value)) throw new Error(`${owner} must be a SHA-256 digest`); }
function plain(value: unknown, owner: string): Record<string, unknown> { if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error(`${owner} must be a plain object`); const prototype = Object.getPrototypeOf(value); if (prototype !== Object.prototype && prototype !== null) throw new Error(`${owner} must be a plain object`); return value as Record<string, unknown>; }
function exact(value: Record<string, unknown>, expected: readonly string[], owner: string): void { const keys = Reflect.ownKeys(value); if (keys.length !== expected.length || keys.some((key) => typeof key !== "string" || !expected.includes(key))) throw new Error(`${owner} has unknown or missing fields`); for (const key of expected) { const descriptor = Object.getOwnPropertyDescriptor(value, key); if (!descriptor || !("value" in descriptor) || !descriptor.enumerable) throw new Error(`${owner} must contain enumerable data fields`); } }
function snapshot(value: unknown, depth = 0, ancestors = new Set<object>()): { readonly value: unknown; readonly bytes: number } {
  if (depth > MAX_DEPTH) throw new Error("JSON exceeds depth limit");
  if (value === null || typeof value === "boolean") return { value, bytes: 4 };
  if (typeof value === "string") { if (hasSurrogate(value)) throw new Error("invalid JSON string"); const bytes = utf8Length(value); if (bytes > MAX_STRING_BYTES) throw new Error("oversized JSON string"); return { value, bytes }; }
  if (typeof value === "number") { if (!Number.isFinite(value) || Object.is(value, -0) || (Number.isInteger(value) && !Number.isSafeInteger(value))) throw new Error("invalid JSON number"); return { value, bytes: 24 }; }
  if (value === null || typeof value !== "object") throw new Error("invalid JSON value");
  if (ancestors.has(value)) throw new Error("cyclic JSON value"); ancestors.add(value);
  try {
    if (Array.isArray(value)) {
      const descriptors = Object.getOwnPropertyDescriptors(value);
      if (Reflect.ownKeys(value).some((key) => typeof key === "symbol" || (key !== "length" && (!/^\d+$/.test(key) || Number(key) >= value.length)))) throw new Error("JSON arrays must be dense and data-only");
      const output: unknown[] = [];
      for (let index = 0; index < value.length; index += 1) {
        const descriptor = descriptors[String(index)];
        if (!descriptor || !("value" in descriptor) || !descriptor.enumerable) throw new Error("JSON arrays must be dense and data-only");
        output.push(snapshot(descriptor.value, depth + 1, ancestors).value);
      }
      return frozen(output);
    }
    const input = plain(value, "JSON"); const output = Object.create(null) as Record<string, unknown>;
    for (const key of Reflect.ownKeys(input)) {
      if (typeof key !== "string") throw new Error("JSON cannot contain symbols");
      const descriptor = Object.getOwnPropertyDescriptor(input, key);
      if (!descriptor || !("value" in descriptor) || !descriptor.enumerable) throw new Error("JSON must be data-only");
    }
    for (const key of Object.keys(input).sort()) { const descriptor = Object.getOwnPropertyDescriptor(input, key); if (!descriptor || !("value" in descriptor) || !descriptor.enumerable) throw new Error("JSON must be data-only"); output[key] = snapshot(descriptor.value, depth + 1, ancestors).value; }
    return frozen(output);
  } finally { ancestors.delete(value); }
}
function frozen(value: object): { readonly value: unknown; readonly bytes: number } { const text = JSON.stringify(value); const bytes = utf8Length(text); if (bytes > MAX_CANONICAL_BYTES) throw new Error("JSON exceeds byte limit"); return { value: Object.freeze(value), bytes }; }
function utf8Length(value: string): number { let bytes = 0; for (let index = 0; index < value.length; index += 1) { const unit = value.charCodeAt(index); if (unit <= 0x7f) bytes += 1; else if (unit <= 0x7ff) bytes += 2; else if (unit >= 0xd800 && unit <= 0xdbff) { bytes += 4; index += 1; } else bytes += 3; } return bytes; }
function hasSurrogate(value: string): boolean { for (let index = 0; index < value.length; index += 1) { const unit = value.charCodeAt(index); if (unit >= 0xd800 && unit <= 0xdbff) { const next = value.charCodeAt(index + 1); if (!(next >= 0xdc00 && next <= 0xdfff)) return true; index += 1; } else if (unit >= 0xdc00 && unit <= 0xdfff) return true; } return false; }
