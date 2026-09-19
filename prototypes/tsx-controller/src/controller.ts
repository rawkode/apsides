import type { LiveResource, ResourceEngine, ResourceGraph, ResourceNode, ResourceOperations, ResolvedResource } from "./contracts.ts";
import { withResourceOperations } from "./authoring/context.ts";
import { orderedResources, PendingOutput, resolveInputs } from "./outputs.ts";
import { readInventory, saveInventory, withControllerLock, type Inventory, type InventoryEntry } from "./state.ts";

export interface Condition { resource: string; reason: string }
export interface CycleReport { phase: "ready" | "blocked"; conditions: Condition[]; managed: number }

export class ControllerRunner {
  constructor(
    readonly graph: ResourceGraph,
    readonly engine: ResourceEngine,
    readonly options: {
      directory: string;
      finalizerTimeoutMs?: number;
      /** Executes under the same writer lock as inventory and provider mutations. */
      prepare?: (finalizing: boolean) => Promise<{ graph: ResourceGraph; pending?: Condition[]; commit?: () => void }>;
    },
  ) {}

  reconcile(): Promise<CycleReport> { return this.run(false); }
  /** Runs existing cleanup code without applying this executable's desired graph. */
  finalizeOnly(ids?: string[]): Promise<CycleReport> { return this.run(true, ids); }

  private async run(finalizeOnly: boolean, selectedIds?: string[]): Promise<CycleReport> {
    return withControllerLock(this.options.directory, async () => {
      const prepared = await this.options.prepare?.(finalizeOnly);
      const graph = prepared?.graph ?? this.graph;
      const inventory = readInventory(this.options.directory, graph.name);
      const nodes = new Map(graph.resources.map((node) => [node.id, node]));
      const conditions: Condition[] = [];
      const live = new Map<string, LiveResource | undefined>();
      for (const id of new Set([...nodes.keys(), ...Object.keys(inventory.resources)])) {
        live.set(id, await this.engine.observe(id));
      }

      // Check the entire previous inventory before allowing any update/prune.
      for (const [id, entry] of Object.entries(inventory.resources)) {
        const node = nodes.get(id);
        const observed = live.get(id);
        if (observed && entry.uid && observed.uid !== entry.uid) {
          conditions.push({ resource: id, reason: "ResourceIdentityChanged" });
          continue;
        }
        if (!finalizeOnly && node && entry.desired && (
          node.kind !== entry.desired.kind || node.name !== entry.desired.name || node.namespace !== entry.desired.namespace
        )) conditions.push({ resource: id, reason: "ExplicitRetirementRequiredForIdentityChange" });
        if (observed) for (const key of observed.finalizers.filter(key => entry.finalizerKeys.includes(key))) {
          if (!node?.finalizers[key]) conditions.push({ resource: id, reason: `MissingFinalizerHandler:${key}` });
        }
      }
      if (conditions.length) return this.report(inventory, conditions);
      // Preserve the last recoverable render when a candidate fails lifecycle preflight.
      prepared?.commit?.();

      if (finalizeOnly) {
        const selected = selectedIds ? new Set(selectedIds) : new Set(Object.keys(inventory.resources));
        for (const id of selected) if (!inventory.resources[id]) throw new Error(`Unknown inventory resource: ${id}`);
        for (const id of selected) {
          if (Object.entries(inventory.resources).some(([other, entry]) =>
            !selected.has(other) && entry.dependsOn.includes(id))) {
            conditions.push({ resource: id, reason: "CleanupScopeHasDependents" });
          }
        }
        if (conditions.length) return this.report(inventory, conditions);
        await this.retire(inventory, nodes, selected, conditions);
        return this.report(inventory, conditions);
      }

      if (prepared?.pending?.length) {
        // Missing unmanaged input pauses desired changes; existing deletion still progresses.
        const selected = new Set(Object.keys(inventory.resources).filter(id =>
          inventory.resources[id]?.deleting || live.get(id)?.deleting));
        await this.retire(inventory, nodes, selected, conditions);
        return this.report(inventory, [...conditions, ...prepared.pending]);
      }

      // Persist logical obligations before the engine can create anything.
      for (const node of graph.resources) {
        const previous = inventory.resources[node.id];
        inventory.resources[node.id] = {
          ...previous,
          finalizerKeys: [...new Set([...(previous?.finalizerKeys ?? []), ...Object.keys(node.finalizers)])],
          // The recorded policy governs removal; a present declaration can explicitly change it.
          deletionPolicy: node.deletionPolicy,
          dependsOn: [...new Set([...node.dependsOn, ...node.readyAfter])],
        };
      }
      this.save(inventory);

      const removed = new Set(Object.keys(inventory.resources).filter((id) => !nodes.has(id)));
      const deleting = new Set([...removed, ...Object.keys(inventory.resources).filter((id) =>
        inventory.resources[id]?.deleting || live.get(id)?.deleting)]);
      await this.retire(inventory, nodes, deleting, conditions);

      const blocked = new Set(conditions.map((condition) => condition.resource));
      for (const node of orderedResources(graph.resources)) {
        if (deleting.has(node.id) || blocked.has(node.id)) continue;
        const dependencyBlocked = [...node.dependsOn, ...node.readyAfter].find((id) => blocked.has(id));
        const unready = node.readyAfter.find((id) => !live.get(id)?.ready || live.get(id)?.deleting);
        if (dependencyBlocked || unready) {
          conditions.push({ resource: node.id, reason: `DependencyNotReady:${dependencyBlocked ?? unready}` });
          blocked.add(node.id);
          continue;
        }
        let props: Record<string, unknown>;
        try { props = resolveInputs(node.props, live) as Record<string, unknown>; }
        catch (error) {
          if (!(error instanceof PendingOutput)) throw error;
          conditions.push({ resource: node.id, reason: `PendingOutput:${error.message}` });
          blocked.add(node.id);
          continue;
        }
        const entry = inventory.resources[node.id]!;
        const desired: ResolvedResource = {
          id: node.id, kind: node.kind, name: node.name, namespace: node.namespace,
          props, finalizerKeys: entry.finalizerKeys, deletionPolicy: node.deletionPolicy,
        };
        entry.desired = desired;
        this.save(inventory);
        // Include last successfully resolved props of every blocked sibling.
        // An unresolved output never becomes an implicit Alchemy deletion.
        await this.engine.apply(Object.values(inventory.resources).flatMap((resource) => resource.desired ? [resource.desired] : []), [node.id]);
        const observed = await this.engine.observe(node.id);
        if (observed) entry.uid = observed.uid;
        live.set(node.id, observed);
        this.save(inventory);
        if (!observed?.ready) conditions.push({ resource: node.id, reason: "ResourceNotReady" });
      }
      return this.report(inventory, conditions);
    });
  }

  private async retire(inventory: Inventory, nodes: Map<string, ResourceNode>, selected: Set<string>, conditions: Condition[]) {
    const pending = new Set(selected);
    const failed = new Set<string>();
    while (pending.size) {
      const id = [...pending].find((candidate) => ![...pending].some((other) =>
        other !== candidate && inventory.resources[other]?.dependsOn.includes(candidate)));
      if (!id) throw new Error("Persisted cleanup dependency cycle");
      pending.delete(id);
      const dependentFailed = [...failed].some((other) => inventory.resources[other]?.dependsOn.includes(id));
      if (dependentFailed) {
        conditions.push({ resource: id, reason: "DependentCleanupBlocked" }); failed.add(id); continue;
      }
      const entry = inventory.resources[id];
      if (!entry) continue;
      const observed = await this.engine.observe(id);
      if (!observed) { delete inventory.resources[id]; this.save(inventory); continue; }
      if (entry.uid && observed.uid !== entry.uid) {
        conditions.push({ resource: id, reason: "ResourceIdentityChanged" }); failed.add(id); continue;
      }
      entry.uid = observed.uid;
      if (entry.deletionPolicy === "retain") {
        if (observed.finalizers.some(key => entry.finalizerKeys.includes(key))) {
          conditions.push({ resource: id, reason: "RetainedResourceHasFinalizers" }); failed.add(id); continue;
        }
        delete inventory.resources[id]; this.save(inventory); continue;
      }
      entry.deleting = true;
      this.save(inventory);
      await this.engine.requestDelete(id, observed.uid);
      let unfinished = false;
      for (const key of observed.finalizers.filter(key => entry.finalizerKeys.includes(key))) {
        const callback = nodes.get(id)?.finalizers[key];
        if (!callback) {
          conditions.push({ resource: id, reason: `MissingFinalizerHandler:${key}` }); unfinished = true; break;
        }
        const abort = new AbortController();
        const timeoutMs = this.options.finalizerTimeoutMs ?? 5_000;
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          const execution = withResourceOperations(this.finalizerOperations(abort.signal), () => callback({
            signal: abort.signal,
            resource: { id, name: observed.name, uid: observed.uid, outputs: observed.outputs },
          }));
          await Promise.race([
            execution,
            new Promise<never>((_, reject) => {
              timer = setTimeout(() => { abort.abort(); reject(new Error("FinalizerTimeout")); }, timeoutMs);
            }),
          ]);
          await this.engine.removeFinalizer(id, observed.uid, key);
          entry.finalizerKeys = entry.finalizerKeys.filter((value) => value !== key);
          this.save(inventory);
        } catch {
          // User callback errors may contain credentials; public diagnostics identify the handler only.
          conditions.push({ resource: id, reason: `FinalizerFailed:${key}` }); unfinished = true; break;
        } finally {
          clearTimeout(timer); abort.abort();
        }
      }
      if (unfinished) { failed.add(id); continue; }
      if (await this.engine.observe(id)) {
        conditions.push({ resource: id, reason: "DeletionPending" }); failed.add(id);
      } else { delete inventory.resources[id]; this.save(inventory); }
    }
  }

  private finalizerOperations(signal: AbortSignal): ResourceOperations {
    const combined = (provided?: AbortSignal) => {
      const result = provided ? AbortSignal.any([signal, provided]) : signal;
      result.throwIfAborted(); return result;
    };
    return {
      get: (target, provided) => this.engine.operations.get(target, combined(provided)),
      ensureDeleted: (target, provided) => this.engine.operations.ensureDeleted(target, combined(provided)),
      patch: (target, value, provided) => this.engine.operations.patch(target, value, combined(provided)),
    };
  }

  private save(inventory: Inventory) { saveInventory(this.options.directory, inventory); }
  private report(inventory: Inventory, conditions: Condition[]): CycleReport {
    return { phase: conditions.length ? "blocked" : "ready", conditions, managed: Object.keys(inventory.resources).length };
  }
}
