import { describe, expect, test } from "bun:test";
import type { ResourceGraph } from "../src/contracts";
function website({ namespace = "production" } = {}): ResourceGraph {
  return { name: "website", namespace, resources: [
    { id: "nginx", kind: "kubernetes:Deployment", name: "nginx", namespace, props: {}, dependsOn: [], readyAfter: [], finalizers: {}, deletionPolicy: "delete" },
    { id: "service", kind: "kubernetes:Service", name: "nginx", namespace, props: {}, dependsOn: [], readyAfter: ["nginx"], finalizers: {}, deletionPolicy: "delete" },
  ], observations: [{ kind: "kubernetes:ConfigMap", name: "nginx-config", namespace, access: ["read"] }] };
}
import { permissions } from "../src/permissions";

describe("inferred permissions", () => {
  test("named callback access stays distinct from namespace-wide managed create", () => {
    const graph = website({ namespace: "production" });
    expect(graph.resources.find(node => node.id === "service")!.readyAfter).toEqual(["nginx"]);
    const plan = permissions(graph);
    expect(plan.roles).toHaveLength(1);
    const role = plan.roles[0]!;
    expect(role.metadata.namespace).toBe("production");
    expect(role.rules.find(rule => rule.resources.includes("configmaps"))).toEqual({
      apiGroups: [""], resources: ["configmaps"],
      resourceNames: ["nginx-config"], verbs: ["get", "list", "watch"],
    });
    for (const resource of ["deployments", "services"]) {
      const rules = role.rules.filter(rule => rule.resources.includes(resource));
      expect(rules).toHaveLength(2);
      expect(rules.find(rule => rule.verbs.includes("create"))).toEqual({
        apiGroups: [resource === "deployments" ? "apps" : ""], resources: [resource], verbs: ["create"],
      });
      expect(rules.find(rule => rule.resourceNames)).toMatchObject({
        resourceNames: ["nginx"], verbs: ["delete", "get", "list", "patch", "watch"],
      });
    }
    expect(plan.limitations.some(item => item.includes("cannot restrict create by name"))).toBe(true);
    expect(plan.limitations.some(item => item.includes("metadata.name field selectors"))).toBe(true);
  });

  test("cloud requirements respect retention and callbacks do not execute or generate wildcards", () => {
    const graph = website({ namespace: "production" });
    graph.resources.push(
      { id: "db", kind: "planetscale:Database", name: "website", props: {}, dependsOn: [], readyAfter: [], finalizers: {}, deletionPolicy: "retain" },
      { id: "dns", kind: "cloudflare:DNSRecord", name: "website.example.com", props: {}, dependsOn: [], readyAfter: [], finalizers: {}, deletionPolicy: "delete" },
    );
    let invoked = false;
    graph.resources.find(node => node.id === "nginx")!.finalizers["example.com/dynamic"] = async () => {
      invoked = true;
      throw new Error("Permission inference must never execute callbacks");
    };
    const plan = permissions(graph);
    expect(invoked).toBe(false);
    expect(plan.cloud).toContainEqual({ provider: "planetscale", resource: "website", operations: ["read", "create", "update"] });
    expect(plan.cloud).toContainEqual({ provider: "cloudflare", resource: "website.example.com", operations: ["read", "create", "update", "delete"] });
    for (const role of plan.roles) for (const rule of role.rules) {
      expect([...rule.apiGroups, ...rule.resources, ...rule.verbs, ...(rule.resourceNames ?? [])]).not.toContain("*");
    }
    for (const requirement of plan.cloud) expect(requirement.operations).not.toContain("*");
    expect(plan.limitations.some(item => item.includes("not minted"))).toBe(true);
  });

  test("merges verbs for the same named target while separating namespaces", () => {
    const graph = website({ namespace: "production" });
    graph.observations.push(
      { kind: "kubernetes:ConfigMap", name: "nginx-config", namespace: "production", access: ["patch", "read"] },
      { kind: "kubernetes:ConfigMap", name: "nginx-config", namespace: "staging", access: ["read"] },
      { kind: "kubernetes:Service", name: "nginx", namespace: "production", access: ["read"] },
    );
    const plan = permissions(graph);
    const production = plan.roles.find(role => role.metadata.namespace === "production")!;
    const markers = production.rules.filter(rule => rule.resources.includes("configmaps"));
    expect(markers).toHaveLength(1);
    expect(markers[0]!.verbs).toEqual(["get", "list", "patch", "watch"]);
    expect(production.rules.filter(rule => rule.resources.includes("services") && rule.resourceNames?.includes("nginx"))).toHaveLength(1);
    const staging = plan.roles.find(role => role.metadata.namespace === "staging")!;
    expect(staging.rules).toEqual([{
      apiGroups: [""], resources: ["configmaps"], resourceNames: ["nginx-config"], verbs: ["get", "list", "watch"],
    }]);
  });
});
