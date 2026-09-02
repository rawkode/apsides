import type { RenderGraph, ResourceNode } from "@oxideloop/sdk";
import type { HookSession } from "@oxideloop/sdk/hooks";
import { ExplicitEmpty, Fragment, type Element } from "./jsx-runtime.js";

const MAX_DEPTH = 64;
const MAX_NODES = 4_096;
const SEMANTIC = "__oxideloopSemantic";

export interface RuntimeRenderer { render(element: Element): RenderGraph; }
interface InternalResource extends ResourceNode { readonly __logicalId: string; }
interface State { readonly resources: InternalResource[]; readonly ancestors: Set<object>; nodes: number; }
type SemanticElement = { readonly type: unknown; readonly props: Record<string, unknown> };
interface Defaults { readonly labels?: Record<string, string>; readonly podSecurityContext?: Record<string, unknown>; }

/** Strictly lowers the v1 semantic tree; arbitrary components are never Kubernetes resources. */
export function createRenderer(session?: HookSession): RuntimeRenderer {
  return { render(element) {
    if (isExplicitEmpty(element)) {
      exactProps(element.props, [], "explicitEmpty()");
      return { disposition: "explicitEmpty", resources: [], dependencies: [], automations: [] };
    }
    const state: State = { resources: [], ancestors: new Set(), nodes: 0 };
    root(element, state, {}, 0);
    if (state.resources.length === 0) throw new RenderTreeError("an empty graph requires explicitEmpty()");
    return { disposition: "apply", resources: state.resources, dependencies: session?.dependencies ?? [], automations: session?.automations ?? [] };
  } };
}

function root(value: unknown, state: State, defaults: Defaults, depth: number): void {
  if (value === null || value === false || value === undefined) return;
  guarded(value, state, depth, () => {
    if (Array.isArray(value)) { for (const child of value) root(child, state, defaults, depth + 1); return; }
    if (isLegacyResource(value)) { state.resources.push(value as InternalResource); return; }
    if (value === null || typeof value !== "object") throw new RenderTreeError("primitive render nodes are unsupported");
    const candidate = value as Record<string, unknown>;
    if (!("type" in candidate) || !("props" in candidate) || candidate.props === null || typeof candidate.props !== "object") throw new RenderTreeError("primitive render nodes are unsupported");
    const node = candidate as unknown as SemanticElement;
    if (isExplicitEmpty(node)) throw new RenderTreeError("explicitEmpty() must be the root render value");
    if (node.type === Fragment) { root(node.props.children, state, defaults, depth + 1); return; }
    if (typeof node.type === "string") {
      if (Object.hasOwn(node.props, "children")) throw new RenderTreeError("intrinsic resource cannot have children");
      if (!isLegacyResource(node.props)) throw new RenderTreeError("malformed Kubernetes resource node");
      state.resources.push(node.props as unknown as InternalResource); return;
    }
    const kind = semanticKind(node.type);
    if (kind) { lowerRoot(kind, node.props, state, defaults, depth + 1); return; }
    if (typeof node.type === "function" && isDefaults(node.type)) {
      root(node.props.children, state, mergeDefaults(defaults, node.type.config), depth + 1); return;
    }
    // Retain the pre-v1 flat component path for existing renderer fixtures.
    if (typeof node.type === "function") { root(node.type(node.props), state, defaults, depth + 1); return; }
    throw new RenderTreeError("unrecognized render element");
  });
}

function lowerRoot(kind: string, props: Record<string, unknown>, state: State, defaults: Defaults, depth: number): void {
  if (kind === "ConfigMap") return configMap(props, state, defaults, depth);
  if (kind === "Deployment") return deployment(props, state, defaults, depth);
  if (kind === "Service") return service(props, state, defaults, depth);
  throw new RenderTreeError(`${kind} must be nested in its owning semantic component`);
}

function configMap(props: Record<string, unknown>, state: State, defaults: Defaults, depth: number): void {
  exactProps(props, ["children", "id", "name"], "ConfigMap");
  const data: Record<string, string> = {};
  children(props.children, state, depth, (child) => {
    const dataProps = childOf(child, "Data");
    exactProps(dataProps, ["name", "value"], "Data");
    const name = text(dataProps.name, "Data.name");
    if (Object.hasOwn(data, name)) throw new RenderTreeError("ConfigMap Data names must be unique");
    data[name] = text(dataProps.value, "Data.value");
  });
  if (Object.keys(data).length === 0) throw new RenderTreeError("ConfigMap requires a Data child");
  state.resources.push({ apiVersion: "v1", kind: "ConfigMap", metadata: metadata(text(props.name, "ConfigMap.name"), defaults), data, __logicalId: text(props.id, "ConfigMap.id") } as InternalResource);
}

function deployment(props: Record<string, unknown>, state: State, defaults: Defaults, depth: number): void {
  exactProps(props, ["children", "id", "name", "replicas", "selector"], "Deployment");
  const selector = labels(props.selector, "Deployment.selector");
  const containers: Record<string, unknown>[] = [];
  const names = new Set<string>();
  children(props.children, state, depth, (child) => {
    const container = lowerContainer(child, state, depth + 1);
    const name = container.name as string;
    if (names.has(name)) throw new RenderTreeError("Deployment Container names must be unique");
    names.add(name); containers.push(container);
  });
  if (containers.length === 0) throw new RenderTreeError("Deployment requires a Container");
  state.resources.push({
    apiVersion: "apps/v1", kind: "Deployment", metadata: metadata(text(props.name, "Deployment.name"), defaults), __logicalId: text(props.id, "Deployment.id"),
    spec: { replicas: props.replicas === undefined ? 1 : whole(props.replicas, "Deployment.replicas"), selector: { matchLabels: selector }, template: { metadata: { labels: selector }, spec: { ...(defaults.podSecurityContext === undefined ? {} : { securityContext: defaults.podSecurityContext }), containers } } },
  } as InternalResource);
}

function service(props: Record<string, unknown>, state: State, defaults: Defaults, depth: number): void {
  exactProps(props, ["children", "id", "name", "selector"], "Service");
  const names = new Set<string>(); const ports: Record<string, unknown>[] = [];
  children(props.children, state, depth, (child) => {
    const port = childOf(child, "Port"); exactProps(port, ["name", "port", "targetPort"], "Service Port");
    const name = text(port.name, "Service Port.name");
    if (names.has(name)) throw new RenderTreeError("Service Port names must be unique");
    names.add(name);
    ports.push({ name, port: portNumber(port.port, "Service Port.port"), targetPort: typeof port.targetPort === "string" ? text(port.targetPort, "Service Port.targetPort") : portNumber(port.targetPort, "Service Port.targetPort") });
  });
  if (ports.length === 0) throw new RenderTreeError("Service requires a Port");
  state.resources.push({ apiVersion: "v1", kind: "Service", metadata: metadata(text(props.name, "Service.name"), defaults), spec: { selector: labels(props.selector, "Service.selector"), ports }, __logicalId: text(props.id, "Service.id") } as InternalResource);
}

function lowerContainer(value: unknown, state: State, depth: number): Record<string, unknown> {
  const props = childOf(value, "Container"); exactProps(props, ["children", "image", "name"], "Container");
  const ports: Record<string, unknown>[] = []; const env: Record<string, unknown>[] = [];
  const portNames = new Set<string>(); const envNames = new Set<string>(); let resources: Record<string, unknown> | undefined;
  children(props.children, state, depth, (child) => {
    const kind = childKind(child);
    if (kind === "Port") {
      const port = childOf(child, "Port"); exactProps(port, ["containerPort", "name"], "Container Port");
      const name = text(port.name, "Container Port.name"); if (portNames.has(name)) throw new RenderTreeError("Container Port names must be unique"); portNames.add(name);
      ports.push({ name, containerPort: portNumber(port.containerPort, "Container Port.containerPort") }); return;
    }
    if (kind === "Env") {
      const item = childOf(child, "Env"); exactProps(item, ["configMap", "name"], "Env");
      const name = text(item.name, "Env.name"); if (envNames.has(name)) throw new RenderTreeError("Env names must be unique"); envNames.add(name);
      const map = plain(item.configMap, "Env.configMap"); exactProps(map, ["key", "name"], "Env.configMap");
      env.push({ name, valueFrom: { configMapKeyRef: { name: text(map.name, "Env.configMap.name"), key: text(map.key, "Env.configMap.key") } } }); return;
    }
    if (kind === "Resources") {
      if (resources !== undefined) throw new RenderTreeError("Container allows one Resources child");
      const item = childOf(child, "Resources"); exactProps(item, ["limits", "requests"], "Resources");
      resources = { requests: quantities(item.requests, "Resources.requests"), limits: quantities(item.limits, "Resources.limits") }; return;
    }
    throw new RenderTreeError("Container children must be Port, Env, or Resources");
  });
  return { name: text(props.name, "Container.name"), image: text(props.image, "Container.image"), ...(ports.length ? { ports } : {}), ...(env.length ? { env } : {}), ...(resources === undefined ? {} : { resources }) };
}

function mergeDefaults(parent: Defaults, raw: unknown): Defaults {
  const config = plain(raw, "Defaults config"); exactProps(config, ["labels", "podSecurityContext"], "Defaults config");
  let podSecurityContext = parent.podSecurityContext;
  if (config.podSecurityContext !== undefined) {
    const security = plain(config.podSecurityContext, "podSecurityContext"); exactProps(security, ["runAsNonRoot"], "podSecurityContext");
    if (typeof security.runAsNonRoot !== "boolean") throw new RenderTreeError("podSecurityContext.runAsNonRoot must be boolean"); podSecurityContext = { runAsNonRoot: security.runAsNonRoot };
  }
  return { ...(config.labels === undefined ? (parent.labels === undefined ? {} : { labels: parent.labels }) : { labels: labels(config.labels, "Defaults labels") }), ...(podSecurityContext === undefined ? {} : { podSecurityContext }) };
}
function metadata(name: string, defaults: Defaults): InternalResource["metadata"] { return { name, ...(defaults.labels === undefined ? {} : { labels: defaults.labels }) }; }
function quantities(value: unknown, owner: string): Record<string, string> {
  const map = plain(value, owner); const result: Record<string, string> = {};
  for (const key of ["cpu", "memory"] as const) if (map[key] !== undefined) result[key] = text(map[key], `${owner}.${key}`);
  if (Object.keys(result).length === 0 || Object.keys(result).length !== Object.keys(map).length) throw new RenderTreeError(`${owner} accepts nonempty cpu and/or memory only`);
  return result;
}
function labels(value: unknown, owner: string): Record<string, string> { const map = plain(value, owner); const result: Record<string, string> = {}; for (const [key, item] of Object.entries(map)) { if (!key) throw new RenderTreeError(`${owner} keys must be nonempty`); result[key] = text(item, `${owner}.${key}`); } if (Object.keys(result).length === 0) throw new RenderTreeError(`${owner} must be nonempty`); return result; }
function whole(value: unknown, owner: string): number { if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new RenderTreeError(`${owner} must be a nonnegative integer`); return value; }
function portNumber(value: unknown, owner: string): number { if (typeof value !== "number" || !Number.isInteger(value) || value < 1 || value > 65_535) throw new RenderTreeError(`${owner} must be a port number`); return value; }
function text(value: unknown, owner: string): string { if (typeof value !== "string" || !value) throw new RenderTreeError(`${owner} must be a nonempty string`); return value; }
function plain(value: unknown, owner: string): Record<string, unknown> { if (value === null || typeof value !== "object" || Array.isArray(value)) throw new RenderTreeError(`${owner} must be a plain object`); const prototype = Object.getPrototypeOf(value); if (prototype !== Object.prototype && prototype !== null) throw new RenderTreeError(`${owner} must be a plain object`); return value as Record<string, unknown>; }
function exactProps(value: Record<string, unknown>, expected: readonly string[], owner: string): void { const allowed = new Set(expected); for (const key of Reflect.ownKeys(value)) { if (typeof key !== "string" || !allowed.has(key)) throw new RenderTreeError(`${owner} has unknown props`); const descriptor = Object.getOwnPropertyDescriptor(value, key); if (!descriptor || !("value" in descriptor) || !descriptor.enumerable) throw new RenderTreeError(`${owner} props must be enumerable data`); } }
function children(value: unknown, state: State, depth: number, consume: (child: unknown) => void): void { if (value === null || value === false || value === undefined) return; guarded(value, state, depth, () => { if (Array.isArray(value)) { for (const child of value) children(child, state, depth + 1, consume); return; } if (value === true || !isElement(value)) throw new RenderTreeError("semantic children must be elements"); consume(value); }); }
function childOf(value: unknown, expected: string): Record<string, unknown> { if (!isElement(value) || childKind(value) !== expected) throw new RenderTreeError(`expected ${expected} child`); return value.props; }
function childKind(value: unknown): string | undefined { return isElement(value) ? semanticKind(value.type) : undefined; }
function semanticKind(type: unknown): string | undefined { if ((typeof type !== "object" && typeof type !== "function") || type === null) return undefined; const kind = (type as Record<string, unknown>)[SEMANTIC]; return typeof kind === "string" ? kind : undefined; }
function isDefaults(type: Function): type is Function & { readonly config: unknown } { return Object.hasOwn(type, "config"); }
function isElement(value: unknown): value is SemanticElement { return value !== null && typeof value === "object" && "type" in value && "props" in value; }
function isLegacyResource(value: unknown): boolean {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const record = value as Record<string, unknown>;
  if (typeof record.apiVersion !== "string" || !record.apiVersion || typeof record.kind !== "string" || !record.kind) return false;
  const metadata = record.metadata;
  return metadata !== null && typeof metadata === "object" && !Array.isArray(metadata) && typeof (metadata as Record<string, unknown>).name === "string" && Boolean((metadata as Record<string, unknown>).name);
}
function isExplicitEmpty(value: unknown): boolean { return isElement(value) && value.type === ExplicitEmpty; }
function guarded(value: unknown, state: State, depth: number, body: () => void): void { if (depth > MAX_DEPTH) throw new RenderTreeError("render tree exceeds depth limit"); if (value === null || typeof value !== "object") { body(); return; } if (state.ancestors.has(value)) throw new RenderTreeError("render tree contains a cycle"); state.nodes += 1; if (state.nodes > MAX_NODES) throw new RenderTreeError("render tree exceeds node limit"); state.ancestors.add(value); try { body(); } finally { state.ancestors.delete(value); } }
export class RenderTreeError extends Error {}
