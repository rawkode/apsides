import type { LiveResource, ResolvedResource } from "../contracts";

export const ownerAnnotation = "prototype.apsides.dev/controller";
export const identityAnnotation = "prototype.apsides.dev/resource-id";
export function manifest(resource: ResolvedResource, controller: string, namespace: string): any {
  const p = resource.props;
  const result: any = { apiVersion: resource.kind === "kubernetes:Deployment" ? "apps/v1" : "v1", kind: resource.kind.split(":")[1],
    metadata: { name: resource.name, namespace: resource.namespace ?? namespace,
      annotations: { [ownerAnnotation]: controller, [identityAnnotation]: resource.id }, finalizers: resource.finalizerKeys } };
  switch (resource.kind) {
    case "kubernetes:Deployment": result.spec = { replicas: p.replicas ?? 1, selector: { matchLabels: p.selector },
      template: { metadata: { labels: p.selector, annotations: p.podAnnotations ?? {} }, spec: { containers: [{ name: "app", image: p.image,
        ports: p.ports ?? (p.port ? [{ containerPort: p.port }] : []),
        envFrom: p.envFrom ?? [],
        env: Object.entries(p.env ?? {}).map(([name, value]) => ({ name, value })) }] } } }; break;
    case "kubernetes:Service": result.spec = { type: p.type ?? "ClusterIP", selector: p.selector,
      ports: p.ports ?? [{ port: p.port, targetPort: p.targetPort ?? p.port }] }; break;
    case "kubernetes:ConfigMap": result.data = p.data ?? {}; break;
    default: throw new Error(`Unsupported Kubernetes resource: ${resource.kind}`);
  }
  return result;
}
/** Compare authored fields while tolerating API defaults and controller-owned status. */
export function contains(actual: any, desired: any): boolean {
  if (Array.isArray(desired)) return (!desired.length && actual == null) || (Array.isArray(actual) && actual.length === desired.length && desired.every((v, i) => contains(actual[i], v)));
  if (desired && typeof desired === "object") return (actual == null && Object.keys(desired).length === 0) || actual != null && Object.entries(desired).every(([key, value]) => contains(actual[key], value));
  return actual === desired;
}
export function liveResource(resource: Pick<ResolvedResource, "id" | "kind" | "name" | "namespace">, value: any): LiveResource {
  const status = value.status ?? {}, spec = value.spec ?? {};
  let ready = !value.metadata.deletionTimestamp;
  const outputs: Record<string, unknown> = { name: value.metadata.name };
  if (resource.kind === "kubernetes:Deployment") ready &&= (status.observedGeneration ?? 0) >= value.metadata.generation
    && (status.updatedReplicas ?? 0) === (spec.replicas ?? 1) && (status.availableReplicas ?? 0) >= (spec.replicas ?? 1)
    && (status.unavailableReplicas ?? 0) === 0;
  if (resource.kind === "kubernetes:Service") {
    outputs.clusterIP = spec.clusterIP;
    const ingress = status.loadBalancer?.ingress?.[0];
    if (ingress?.ip) outputs.ipv4 = ingress.ip;
    if (ingress?.hostname) outputs.hostname = ingress.hostname;
    ready &&= spec.type === "LoadBalancer" ? !!(ingress?.ip || ingress?.hostname) : !!spec.clusterIP;
  }
  return { ...resource, uid: value.metadata.uid, props: resource.kind === "kubernetes:ConfigMap" ? { data: value.data ?? {} } : spec,
    outputs, ready, finalizers: value.metadata.finalizers ?? [], deleting: !!value.metadata.deletionTimestamp };
}
