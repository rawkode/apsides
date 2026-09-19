import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import type { LiveResource, Observation, ResolvedResource } from "../contracts";

export interface WorldEvent { action: string; id: string; uid?: string }
interface WorldData {
  resources: Record<string, LiveResource>;
  external: Record<string, Record<string, unknown>>;
  events: WorldEvent[];
}
export function atomicJSON(file: string, value: unknown): void {
  const temporary = `${file}.${randomUUID()}.tmp`;
  writeFileSync(temporary, JSON.stringify(value, null, 2), { mode: 0o600 });
  renameSync(temporary, file);
}
export const observationKey = (target: Pick<Observation, "kind" | "namespace" | "name">) =>
  JSON.stringify([target.kind, target.namespace ?? "", target.name]);

/** A durable simulated API server. It is deliberately separate from Alchemy state. */
export class SimulatedWorld {
  private readonly file: string;
  constructor(directory: string) {
    mkdirSync(directory, { recursive: true });
    this.file = join(directory, "world.json");
  }
  private read(): WorldData {
    try { return JSON.parse(readFileSync(this.file, "utf8")); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      return { resources: {}, external: {}, events: [] };
    }
  }
  private mutate<T>(fn: (state: WorldData) => T): T {
    const state = this.read();
    const result = fn(state);
    atomicJSON(this.file, state);
    return result;
  }
  get(id: string): LiveResource | undefined { return this.read().resources[id]; }
  list(): LiveResource[] { return Object.values(this.read().resources); }
  events(): WorldEvent[] { return this.read().events; }
  reconcile(desired: ResolvedResource): LiveResource {
    return this.mutate((state) => {
      let live = state.resources[desired.id];
      if (live && (live.kind !== desired.kind || live.name !== desired.name || live.namespace !== desired.namespace)) {
        throw new Error(`Identity change requires explicit removal: ${desired.id}`);
      }
      if (!live) {
        live = { ...desired, uid: randomUUID(), outputs: { name: desired.name },
          ready: desired.kind !== "planetscale:Database" && desired.kind !== "kubernetes:Service",
          finalizers: [...desired.finalizerKeys], deleting: false };
        if (desired.kind === "planetscale:Database") live.outputs.host = `${desired.name}.simulated.invalid`;
        state.resources[desired.id] = live;
        state.events.push({ action: "create", id: live.id, uid: live.uid });
      } else if (!live.deleting) {
        const keys = [...new Set([...live.finalizers, ...desired.finalizerKeys])];
        if (JSON.stringify(live.props) !== JSON.stringify(desired.props) || JSON.stringify(keys) !== JSON.stringify(live.finalizers)) {
          live.props = structuredClone(desired.props);
          live.finalizers = keys;
          state.events.push({ action: "update", id: live.id, uid: live.uid });
        }
      }
      return live;
    });
  }
  setReady(id: string, ready: boolean): void { this.change(id, (live) => { live.ready = ready; }, "readiness"); }
  setServiceIPv4(id: string, address: string): void {
    this.change(id, (live) => { live.outputs.ipv4 = address; live.outputs.address = address; live.ready = true; }, "load-balancer");
  }
  drift(id: string, props: Record<string, unknown>): void { this.change(id, (live) => { live.props = props; }, "drift"); }
  deleteExternally(id: string): void { this.mutate((s) => { delete s.resources[id]; s.events.push({ action: "external-delete", id }); }); }
  private change(id: string, fn: (live: LiveResource) => void, action: string): void {
    this.mutate((s) => { const live = s.resources[id]; if (!live) throw new Error(`Missing ${id}`); fn(live); s.events.push({ action, id, uid: live.uid }); });
  }
  requestDelete(id: string, uid: string): void {
    this.mutate((s) => {
      const live = s.resources[id]; if (!live) return;
      if (live.uid !== uid) throw new Error(`UID conflict deleting ${id}`);
      if (!live.deleting) s.events.push({ action: "delete-request", id, uid });
      live.deleting = true;
      if (!live.finalizers.length) { delete s.resources[id]; s.events.push({ action: "delete", id, uid }); }
    });
  }
  removeFinalizer(id: string, uid: string, key: string): void {
    this.mutate((s) => {
      const live = s.resources[id]; if (!live) return;
      if (live.uid !== uid) throw new Error(`UID conflict finalizing ${id}`);
      live.finalizers = live.finalizers.filter((value) => value !== key);
      s.events.push({ action: `finalizer-remove:${key}`, id, uid });
      if (live.deleting && !live.finalizers.length) { delete s.resources[id]; s.events.push({ action: "delete", id, uid }); }
    });
  }
  seedObservation(target: Observation, value: Record<string, unknown>): void {
    this.mutate((s) => { s.external[observationKey(target)] = { ...value, metadata: { name: target.name, namespace: target.namespace, ...(value.metadata as object), uid: randomUUID() } }; });
  }
  getObservation(target: Observation): Record<string, unknown> | undefined { return this.read().external[observationKey(target)]; }
  mutateObservation(target: Observation, uid: string, patch?: Record<string, unknown>): void {
    this.mutate((s) => {
      const key = observationKey(target), current = s.external[key]; if (!current) return;
      if ((current.metadata as { uid: string }).uid !== uid) throw new Error(`UID conflict on ${target.name}`);
      if (patch) s.external[key] = { ...current, ...patch, metadata: { ...(current.metadata as object), ...(patch.metadata as object), uid } };
      else delete s.external[key];
      s.events.push({ action: patch ? "observation-patch" : "observation-delete", id: key, uid });
    });
  }
}
