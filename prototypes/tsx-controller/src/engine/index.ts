import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import { BunServices } from "@effect/platform-bun";
import { AlchemyContext } from "alchemy/AlchemyContext";
import { provideFreshArtifactStore } from "alchemy/Artifacts";
import { LoggingCli } from "alchemy/Cli/LoggingCli";
import { Resource } from "alchemy/Resource";
import * as Provider from "alchemy/Provider";
import * as Stack from "alchemy/Stack";
import { deploy } from "alchemy/Deploy";
import * as RemovalPolicy from "alchemy/RemovalPolicy";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { LiveResource, Observation, ResolvedResource, ResourceEngine, ResourceOperations } from "../contracts";
import { durableState } from "./state";
import { atomicJSON, observationKey, SimulatedWorld } from "./world";
export { SimulatedWorld } from "./world";

type Simulated = Resource<"Prototype.Simulated", ResolvedResource, LiveResource>;
const Simulated = Resource<Simulated>("Prototype.Simulated");

export interface AlchemyEngineOptions {
  directory: string;
  world?: SimulatedWorld;
  stackName?: string;
  /** Fault injection at the persistence boundary, used by the recovery test. */
  beforeStateSave?: (value: unknown) => void;
}

export class AlchemyEngine implements ResourceEngine {
  readonly world: SimulatedWorld;
  readonly operations: ResourceOperations;
  private applying = false;
  constructor(private readonly options: AlchemyEngineOptions) {
    this.world = options.world ?? new SimulatedWorld(options.directory);
    const uidFile = join(options.directory, "observation-uids.json");
    const readBindings = (): Record<string, string | null> => {
      // Engines may be constructed before another runner acquires the lock.
      // Always load bindings inside the operation, after the caller's lock.
      try { return JSON.parse(readFileSync(uidFile, "utf8")); }
      catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; return {}; }
    };
    const bind = (target: Observation, value: Record<string, unknown>) => {
      const bindings = readBindings();
      const key = observationKey(target), uid = (value.metadata as { uid: string }).uid;
      if (key in bindings && bindings[key] !== uid) throw new Error(`UID conflict on ${target.name}`);
      if (!(key in bindings)) { bindings[key] = uid; atomicJSON(uidFile, bindings); }
    };
    const assertAccess = (target: Observation, access: "read" | "patch" | "delete", signal?: AbortSignal) => {
      signal?.throwIfAborted();
      if (!target.access.includes(access)) throw new Error(`Missing ${access} permission for ${target.name}`);
    };
    const mutate = async (target: Observation, signal?: AbortSignal, patch?: Record<string, unknown>) => {
      assertAccess(target, patch ? "patch" : "delete", signal);
      const key = observationKey(target);
      let bindings = readBindings();
      if (!(key in bindings)) {
        const current = this.world.getObservation(target);
        if (!current) {
          bindings[key] = null;
          atomicJSON(uidFile, bindings);
          return;
        }
        bind(target, current);
        bindings = readBindings();
      }
      if (bindings[key] === null) {
        if (this.world.getObservation(target)) throw new Error(`UID conflict on ${target.name}`);
        return;
      }
      this.world.mutateObservation(target, bindings[key]!, patch);
    };
    this.operations = {
      get: async (target, signal) => {
        assertAccess(target, "read", signal);
        const value = this.world.getObservation(target);
        if (value) bind(target, value);
        else {
          const bindings = readBindings(), key = observationKey(target);
          if (!(key in bindings)) { bindings[key] = null; atomicJSON(uidFile, bindings); }
        }
        return value;
      },
      ensureDeleted: (target, signal) => mutate(target, signal),
      patch: (target, value, signal) => mutate(target, signal, value),
    };
  }
  async observe(id: string): Promise<LiveResource | undefined> { return this.world.get(id); }
  async requestDelete(id: string, uid: string): Promise<void> { this.world.requestDelete(id, uid); }
  async removeFinalizer(id: string, uid: string, key: string): Promise<void> { this.world.removeFinalizer(id, uid, key); }
  async apply(resources: readonly ResolvedResource[], activeIds?: readonly string[]): Promise<void> {
    if (this.applying) throw new Error("Only one Alchemy apply may run at a time");
    this.applying = true;
    try {
      const world = this.world;
      const active = new Set(activeIds ?? resources.map((resource) => resource.id));
      const providers = Provider.succeed(Simulated, Simulated.Provider.of({
        // Observe on every plan; cached props alone cannot establish convergence.
        diff: ({ news, output }) => Effect.sync(() => {
          // The adapter accepts ResolvedResource only, never Alchemy Outputs.
          const desired = news as ResolvedResource;
          if (!active.has(desired.id)) return { action: "noop" as const };
          const live = world.get(desired.id);
          const unchanged = live && JSON.stringify(live) === JSON.stringify(output)
            && live.kind === desired.kind && live.name === desired.name && live.namespace === desired.namespace
            && JSON.stringify(live.props) === JSON.stringify(desired.props)
            && desired.finalizerKeys.every((key) => live.finalizers.includes(key));
          return { action: unchanged ? "noop" as const : "update" as const };
        }),
        read: ({ id, output }) => Effect.sync(() => world.get(output?.id ?? id)),
        reconcile: ({ news }) => Effect.sync(() => {
          if (active.has(news.id)) return world.reconcile(news);
          // Cold adoption can force an update even when diff returns noop.
          // Frozen resources therefore also enforce read-only behavior here.
          const existing = world.get(news.id);
          if (!existing) throw new Error(`Frozen resource disappeared: ${news.id}`);
          return existing;
        }),
        delete: ({ output }) => Effect.sync(() => world.requestDelete(output.id, output.uid)),
        list: () => Effect.succeed(world.list()),
      }));
      const stack = Stack.Stack(this.options.stackName ?? "tsx-prototype", {
        providers,
        state: durableState(this.options.directory, this.options.beforeStateSave),
      }, Effect.gen(function* () {
        for (const resource of resources) {
          // A blocked sibling stays declared while live, but an absent blocked
          // resource must not enter Alchemy's create path. Retain makes omission
          // non-destructive; the runner keeps its last desired props in inventory.
          if (!active.has(resource.id) && !world.get(resource.id)) continue;
          // Removal is explicit through the runner's UID/finalizer protocol.
          // Alchemy must forget omitted rows without bypassing those hooks.
          yield* Simulated(resource.id, resource).pipe(RemovalPolicy.retain());
        }
        return {};
      }));
      const run = deploy({ stack, stage: "controller" }).pipe(
        provideFreshArtifactStore,
        Effect.provide(durableState(this.options.directory, this.options.beforeStateSave)),
        Effect.provide(LoggingCli),
        Effect.provide(Layer.succeed(AlchemyContext, { dotAlchemy: this.options.directory, dev: false, adopt: false })),
        Effect.provide(BunServices.layer),
      );
      await Effect.runPromise(run);
    } finally { this.applying = false; }
  }
}
export const createAlchemyEngine = (options: AlchemyEngineOptions): AlchemyEngine => new AlchemyEngine(options);
