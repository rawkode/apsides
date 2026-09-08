import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { Observation, ResourceGraph } from "./contracts";
import { observationKey } from "./authoring/context";
import { atomicJSON } from "./engine/world";
import { readInventory } from "./state";
import type { Condition } from "./controller";

export type Snapshots = Map<string, Record<string, unknown> | undefined>;
export type Render = (snapshots?: Snapshots) => ResourceGraph;

/** A deployed program cannot acquire new permissions by branching on live data. */
export function assertWithinPlan(plan: ResourceGraph, graph: ResourceGraph) {
  if (graph.name !== plan.name || graph.namespace !== plan.namespace) throw new Error("Rendered controller scope changed");
  for (const observed of graph.observations) {
    const allowed = plan.observations.find(value => observationKey(value) === observationKey(observed));
    if (!allowed || observed.access.some(access => access !== "read" || !allowed.access.includes(access))) {
      throw new Error(`Resource observation requires redeployment: ${observed.name}`);
    }
  }
  for (const node of graph.resources) {
    const allowed = plan.resources.find(value => value.id === node.id);
    if (!allowed || allowed.kind !== node.kind || allowed.name !== node.name || allowed.namespace !== node.namespace ||
      (allowed.deletionPolicy === "retain" && node.deletionPolicy !== "retain")) {
      throw new Error(`Managed resource requires redeployment: ${node.id}`);
    }
  }
}

/** Called by ControllerRunner under its writer lock, including snapshot persistence. */
export function prepareReactive(
  directory: string, plan: ResourceGraph, render: Render,
  read: (target: Observation) => Promise<Record<string, unknown> | undefined>,
) {
  const path = join(directory, "render-snapshots.json");
  function recovered(): ResourceGraph {
    try {
      const saved = JSON.parse(readFileSync(path, "utf8"));
      if (saved.version !== 1 || saved.controller !== plan.name || saved.namespace !== plan.namespace) {
        throw new Error("Unsupported render snapshot or controller scope");
      }
      const graph = render(new Map(saved.snapshots));
      assertWithinPlan(plan, graph);
      return graph;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      if (Object.keys(readInventory(directory, plan.name).resources).length) {
        throw new Error("Managed inventory has no recoverable render snapshot");
      }
      return plan;
    }
  }
  return async (finalizing: boolean): Promise<{ graph: ResourceGraph; pending?: Condition[]; commit?: () => void }> => {
    if (finalizing) return { graph: recovered() };
    const snapshots: Snapshots = new Map();
    const pending: Condition[] = [];
    for (const target of plan.observations) {
      const value = await read(target);
      snapshots.set(observationKey(target), value);
      if (!value || (value.metadata as { deletionTimestamp?: string } | undefined)?.deletionTimestamp) {
        pending.push({ resource: `${target.namespace}/${target.name}`, reason: "RequiredResourceUnavailable" });
      }
    }
    if (pending.length) return { graph: recovered(), pending };
    const graph = render(snapshots);
    assertWithinPlan(plan, graph);
    return { graph, commit: () => atomicJSON(path, {
      version: 1, controller: plan.name, namespace: plan.namespace, snapshots: [...snapshots],
    }) };
  };
}
