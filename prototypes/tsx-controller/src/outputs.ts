import type { LiveResource, Output, ResourceNode } from "./contracts.ts";

export class PendingOutput extends Error {}

export function resolveInputs(value: unknown, live: Map<string, LiveResource | undefined>): unknown {
  if (value && typeof value === "object" && "tag" in value && value.tag === "output") {
    const output = value as Output;
    const id = output.ref.id;
    if (!id) throw new Error("Unbound resource reference");
    let resolved: unknown = live.get(id)?.outputs;
    for (const part of output.path) {
      resolved = resolved && typeof resolved === "object" ? (resolved as Record<string, unknown>)[part] : undefined;
    }
    if (resolved === undefined || resolved === null) throw new PendingOutput(`${id}.${output.path.join(".")} is unavailable`);
    return resolved;
  }
  if (Array.isArray(value)) return value.map((item) => resolveInputs(item, live));
  if (value && typeof value === "object") return Object.fromEntries(
    Object.entries(value).map(([key, item]) => [key, resolveInputs(item, live)]),
  );
  return value;
}

export function orderedResources(nodes: readonly ResourceNode[]): ResourceNode[] {
  const pending = new Map(nodes.map((node) => [node.id, node]));
  const result: ResourceNode[] = [];
  const done = new Set<string>();
  while (pending.size) {
    let progressed = false;
    for (const [id, node] of pending) {
      if ([...node.dependsOn, ...node.readyAfter].every((dependency) => done.has(dependency))) {
        result.push(node); pending.delete(id); done.add(id); progressed = true;
      }
    }
    if (!progressed) throw new Error("Dependency cycle or undeclared resource in graph");
  }
  return result;
}
