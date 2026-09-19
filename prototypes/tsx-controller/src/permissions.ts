import type { ResourceGraph, ResourceKind } from "./contracts.ts";

export interface PolicyRule {
  apiGroups: string[];
  resources: string[];
  verbs: string[];
  resourceNames?: string[];
}

export interface PermissionPlan {
  roles: Array<{
    apiVersion: "rbac.authorization.k8s.io/v1";
    kind: "Role";
    metadata: { name: string; namespace: string };
    rules: PolicyRule[];
  }>;
  cloud: Array<{ provider: string; resource: string; operations: string[] }>;
  limitations: string[];
}

const kubernetesKinds: Partial<Record<ResourceKind, [string, string]>> = {
  "kubernetes:Deployment": ["apps", "deployments"],
  "kubernetes:Service": ["", "services"],
  "kubernetes:ConfigMap": ["", "configmaps"],
};

/** Generate workload RBAC from declarations, never from executing callbacks. */
export function permissions(graph: ResourceGraph): PermissionPlan {
  const byNamespace = new Map<string, Map<string, PolicyRule>>();
  const cloud: PermissionPlan["cloud"] = [];
  const limitations = new Set<string>();
  function add(namespace: string, group: string, resource: string, name: string | undefined, verbs: string[]) {
    const rules = byNamespace.get(namespace) ?? new Map<string, PolicyRule>();
    byNamespace.set(namespace, rules);
    const key = JSON.stringify([group, resource, name]);
    const previous = rules.get(key);
    rules.set(key, {
      apiGroups: [group], resources: [resource],
      ...(name ? { resourceNames: [name] } : {}),
      verbs: [...new Set([...(previous?.verbs ?? []), ...verbs])].sort(),
    });
  }
  for (const node of graph.resources) {
    const mapping = kubernetesKinds[node.kind];
    if (!mapping) {
      cloud.push({
        provider: node.kind.split(":")[0]!, resource: node.name,
        operations: ["read", "create", "update", ...(node.deletionPolicy === "delete" ? ["delete"] : [])],
      });
      continue;
    }
    const [group, resource] = mapping;
    const ns = node.namespace ?? graph.namespace;
    add(ns, group, resource, node.name, ["get", "list", "watch", "patch",
      ...(node.deletionPolicy === "delete" ? ["delete"] : [])]);
    add(ns, group, resource, undefined, ["create"]);
    limitations.add(`Creating ${resource} requires namespace-wide create authority; Kubernetes RBAC cannot restrict create by name.`);
  }
  for (const observation of graph.observations) {
    const mapping = kubernetesKinds[observation.kind];
    if (!mapping) {
      cloud.push({ provider: observation.kind.split(":")[0]!, resource: observation.name,
        operations: observation.access });
      continue;
    }
    const verbs = new Set<string>();
    for (const access of observation.access) {
      if (access === "read") for (const verb of ["get", "list", "watch"]) verbs.add(verb);
      else verbs.add(access);
    }
    // Conditional mutations first inspect and bind the observed UID.
    if (observation.access.includes("patch") || observation.access.includes("delete")) verbs.add("get");
    add(observation.namespace ?? graph.namespace, ...mapping, observation.name, [...verbs]);
  }
  if (byNamespace.size) {
    limitations.add("Named list/watch requests must use metadata.name field selectors. Patch authority is not restricted to specific fields.");
    limitations.add("These are workload Role templates, not installed grants. Binding, bootstrap and controller-state permissions are separate.");
  }
  if (cloud.length) limitations.add("Cloud capabilities are a requirements report, not minted Cloudflare or PlanetScale credentials.");
  return {
    roles: [...byNamespace].sort(([a], [b]) => a.localeCompare(b)).map(([namespace, rules]) => ({
      apiVersion: "rbac.authorization.k8s.io/v1", kind: "Role",
      metadata: { name: `${graph.name}-controller`, namespace },
      rules: [...rules.values()].sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b))),
    })),
    cloud: cloud.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b))),
    limitations: [...limitations].sort(),
  };
}
