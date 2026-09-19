import { createHash } from "node:crypto";
import type { Observation, ResourceGraph, ResourceKind, ResourceNode, ResourceRef } from "../contracts";
import { Controller, resourceKind, type DeploymentProps, type ResourceComponent } from "./components";
import { declarations, observationKey, type DeclarationContext } from "./context";
import { Fragment, type Child, type Element } from "./jsx-runtime";

function qualifiedFinalizer(key: string): boolean {
  const parts = key.split("/");
  if (parts.length !== 2) return false;
  const [domain, name] = parts as [string, string];
  return domain.length <= 253 && domain.split(".").every(label =>
    label.length > 0 && label.length <= 63 && /^[a-z0-9](?:[-a-z0-9]*[a-z0-9])?$/.test(label)) &&
    name.length <= 63 && /^[A-Za-z0-9](?:[-A-Za-z0-9_.]*[A-Za-z0-9])?$/.test(name);
}

function physicalIdentity(kind: ResourceKind, name: string, namespace: string | undefined, props: Record<string, unknown>): string {
  const scope = kind.startsWith("kubernetes:") ? namespace :
    kind === "cloudflare:DNSRecord" ? props.zoneId : props.organization;
  return JSON.stringify([kind, scope, name, kind === "cloudflare:DNSRecord" ? props.type : undefined]);
}

/** Expand components and validate a complete desired graph without provider IO. */
export function compile(element: Child, options: { snapshots?: Map<string, Record<string, unknown> | undefined> } = {}): ResourceGraph {
  const graph: ResourceGraph = { name: "", namespace: "default", resources: [], observations: [] };
  const observations: Observation[] = [];
  const hooks: DeclarationContext["finalizers"] = [];
  const pending = new Map<ResourceNode, readonly ResourceRef<any>[]>();
  const physical = new Set<string>();
  const ids = new Set<string>();
  const boundRefs = new Set<ResourceRef<any>>();
  let rootSeen = false;

  function visit(child: Child, namespace?: string, deployment?: Pick<DeploymentProps, "name" | "selector" | "port" | "ports">): void {
    if (child == null || typeof child === "boolean") return;
    if (Array.isArray(child)) { for (const item of child) visit(item, namespace, deployment); return; }
    if (typeof child !== "object" || !("type" in child)) throw new Error("Components must return JSX elements");
    const { type, props } = child as Element;
    if (type === Fragment) { visit(props.children, namespace, deployment); return; }
    if (type === Controller) {
      if (rootSeen) throw new Error("A graph must have exactly one Controller");
      if (!props.name) throw new Error("Controller requires a name");
      rootSeen = true;
      graph.name = props.name;
      graph.namespace = props.namespace ?? "default";
      visit(props.children, graph.namespace);
      return;
    }
    const kind = (type as ResourceComponent)[resourceKind];
    if (!kind) {
      if (typeof type !== "function") throw new Error("Unsupported JSX element");
      const result = declarations.run({ observations, namespace, finalizers: hooks, snapshots: options.snapshots }, () => type(props));
      if (result instanceof Promise) throw new Error("Components must be synchronous; declare deferred references for outputs");
      visit(result, namespace, deployment);
      return;
    }
    if (!rootSeen || namespace === undefined) throw new Error("Resources must be nested inside Controller");
    const { name: explicitName, id: explicitId, ref, readyAfter = [], finalizers = {}, deletionPolicy = "delete", children, namespace: explicitNamespace, ...inputs } = props;
    const name = explicitName ?? (kind === "kubernetes:Service" ? deployment?.name : undefined);
    if (typeof name !== "string" || !name) throw new Error(`${kind} requires a name`);
    if (explicitId !== undefined && (typeof explicitId !== "string" || !explicitId)) throw new Error("Resource id must be nonempty");
    if (!["delete", "retain"].includes(deletionPolicy)) throw new Error("Invalid deletionPolicy");
    const resourceNamespace = kind.startsWith("kubernetes:") ? explicitNamespace ?? namespace : undefined;
    const id = explicitId ?? `${kind}/${resourceNamespace ? `${resourceNamespace}/` : ""}${name}`;
    if (ids.has(id)) throw new Error(`Duplicate resource id: ${id}`);
    ids.add(id);
    const identity = physicalIdentity(kind, name, resourceNamespace, inputs);
    if (physical.has(identity)) throw new Error(`Duplicate physical resource: ${name}`);
    physical.add(identity);
    for (const [key, fn] of Object.entries(finalizers)) {
      if (!qualifiedFinalizer(key)) throw new Error(`Finalizer must have a qualified key: ${key}`);
      if (typeof fn !== "function") throw new Error(`Finalizer ${key} must be a function`);
    }
    if (ref) {
      if (typeof ref.output !== "function") throw new Error("Invalid resource ref");
      if (ref.id !== undefined && ref.id !== id) throw new Error(`Resource ref is already bound to ${ref.id}`);
      ref.id = id;
      boundRefs.add(ref);
    }
    if (kind === "kubernetes:Deployment") {
      inputs.selector ??= { "app.kubernetes.io/name": name };
      if (inputs.envFrom !== undefined) {
        if (!Array.isArray(inputs.envFrom)) throw new Error("envFrom requires observed ConfigMaps");
        const configs = inputs.envFrom.map((config: any) => {
          if (config?.tag !== "observed" || config.target?.kind !== "kubernetes:ConfigMap") throw new Error("envFrom requires observed ConfigMaps");
          if (config.target.namespace !== resourceNamespace) throw new Error("envFrom ConfigMap must share the Deployment namespace");
          if (!observations.some(target => observationKey(target) === observationKey(config.target))) throw new Error("envFrom requires a declared useResource observation");
          const canonical = (data: Record<string, string>) => Object.fromEntries(Object.entries(data).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0));
          return { name: config.target.name, data: canonical(config.data) };
        });
        const checksum = createHash("sha256").update(JSON.stringify(configs)).digest("hex");
        const key = "tsx-controller.example.com/config-checksum";
        if (inputs.podAnnotations?.[key] !== undefined) throw new Error(`Annotation ${key} is reserved for envFrom`);
        inputs.podAnnotations = { ...inputs.podAnnotations, ...(configs.length ? { [key]: checksum } : {}) };
        inputs.envFrom = configs.map((config: { name: string }) => ({ configMapRef: { name: config.name } }));
      } else {
        if (inputs.podAnnotations?.["tsx-controller.example.com/config-checksum"] !== undefined) throw new Error("Annotation tsx-controller.example.com/config-checksum is reserved for envFrom");
        inputs.envFrom = [];
        inputs.podAnnotations ??= {};
      }
    }
    if (kind === "kubernetes:Service") {
      inputs.selector ??= deployment?.selector;
      if (!inputs.selector) throw new Error(`Service ${name} requires a selector or a parent Deployment`);
      if (inputs.port === undefined && inputs.ports === undefined) {
        if (deployment?.port !== undefined) inputs.port = deployment.port;
        else if (deployment?.ports?.length === 1) inputs.port = deployment.ports[0]!.containerPort;
        else throw new Error(`Service ${name} requires an explicit port when its Deployment has no single unambiguous port`);
      }
      if (inputs.port !== undefined) inputs.targetPort ??= inputs.port;
      inputs.type ??= "ClusterIP";
    }
    const node: ResourceNode = {
      id, kind, name, namespace: resourceNamespace, props: inputs,
      dependsOn: [], readyAfter: [], finalizers, deletionPolicy,
    };
    graph.resources.push(node);
    pending.set(node, readyAfter);
    if (children !== undefined && kind !== "kubernetes:Deployment") throw new Error(`${kind} does not accept children`);
    if (kind === "kubernetes:Deployment") {
      visit(children, resourceNamespace, { name, selector: inputs.selector, port: inputs.port, ports: inputs.ports });
    }
  }

  declarations.run({ observations, finalizers: hooks, snapshots: options.snapshots }, () => visit(element));
  if (!rootSeen) throw new Error("A graph requires a Controller");

  function refId(ref: ResourceRef<any>): string {
    if (!ref || !boundRefs.has(ref) || !ref.id || !ids.has(ref.id)) throw new Error("Unresolved resource reference");
    return ref.id;
  }
  function outputs(value: unknown, dependencies: Set<string>, ancestors = new Set<object>()): void {
    if (!value || typeof value !== "object") return;
    if ("tag" in value && value.tag === "output") {
      if (!("ref" in value)) throw new Error("Invalid output reference");
      dependencies.add(refId(value.ref as ResourceRef<any>));
      return;
    }
    if (ancestors.has(value)) throw new Error("Resource props must not contain circular objects");
    ancestors.add(value);
    for (const item of Object.values(value)) outputs(item, dependencies, ancestors);
    ancestors.delete(value);
  }
  for (const hook of hooks) {
    const id = refId(hook.ref);
    if (!qualifiedFinalizer(hook.key)) throw new Error(`Finalizer must have a qualified key: ${hook.key}`);
    if (typeof hook.callback !== "function") throw new Error(`Finalizer ${hook.key} must be a function`);
    const node = graph.resources.find(resource => resource.id === id)!;
    if (Object.hasOwn(node.finalizers, hook.key)) throw new Error(`Duplicate finalizer: ${hook.key}`);
    node.finalizers = { ...node.finalizers, [hook.key]: hook.callback };
  }
  for (const node of graph.resources) {
    const dependencies = new Set<string>();
    outputs(node.props, dependencies);
    node.dependsOn = [...dependencies].sort();
    node.readyAfter = [...new Set(pending.get(node)!.map(refId))].sort();
  }
  const byId = new Map(graph.resources.map(node => [node.id, node]));
  const active = new Set<string>();
  const visited = new Set<string>();
  function checkCycle(id: string): void {
    if (active.has(id)) throw new Error(`Resource dependency cycle at ${id}`);
    if (visited.has(id)) return;
    active.add(id);
    const node = byId.get(id)!;
    for (const upstream of [...node.dependsOn, ...node.readyAfter]) checkCycle(upstream);
    active.delete(id);
    visited.add(id);
  }
  for (const id of ids) checkCycle(id);

  const uniqueObservations = new Map<string, Observation>();
  for (const observation of observations) {
    if (observation.kind.startsWith("kubernetes:")) observation.namespace ??= graph.namespace;
    else delete observation.namespace;
    const key = JSON.stringify([observation.kind, observation.namespace, observation.name]);
    const existing = uniqueObservations.get(key);
    if (existing) existing.access = [...new Set([...existing.access, ...observation.access])];
    else uniqueObservations.set(key, { ...observation, access: [...observation.access] });
  }
  graph.observations = [...uniqueObservations.values()];
  return graph;
}
