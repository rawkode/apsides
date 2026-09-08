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
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { join } from "node:path";
import type { LiveResource, Observation, ResolvedResource, ResourceEngine, ResourceGraph, ResourceOperations } from "../contracts";
import { durableState } from "../engine/state";
import { atomicJSON, observationKey } from "../engine/world";
import { Connection, inClusterConnection, KubernetesClient, KubernetesError, resourcePath } from "./client";
import { waitForChanges } from "./watch";
import { observationKey as reactiveKey } from "../authoring/context";
import { contains, identityAnnotation, liveResource, manifest, ownerAnnotation } from "./manifest";
export type { Connection } from "./client";

type Kubernetes = Resource<"Prototype.Kubernetes", ResolvedResource, LiveResource>;
const Kubernetes = Resource<Kubernetes>("Prototype.Kubernetes");
export interface KubernetesEngineOptions {
  directory: string; namespace: string; graph: ResourceGraph; connection?: Connection; stackName?: string;
}
const attempt = <A>(fn: () => Promise<A>) => Effect.tryPromise({ try: fn, catch: (cause) => cause instanceof Error ? cause : new Error(String(cause)) });

export class KubernetesEngine implements ResourceEngine {
  readonly client: KubernetesClient;
  readonly operations: ResourceOperations;
  private readonly owner: string;
  private applying = false;
  private readonly observedVersions = new Map<string, string>();
  private descriptors = new Map<string, Pick<ResolvedResource, "id" | "kind" | "name" | "namespace">>();
  constructor(readonly options: KubernetesEngineOptions) {
    mkdirSync(options.directory, { recursive: true, mode: 0o700 });
    const ownerFile = join(options.directory, "kubernetes-owner");
    try { writeFileSync(ownerFile, randomUUID(), { flag: "wx", mode: 0o600 }); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error; }
    this.owner = `${options.graph.name}/${readFileSync(ownerFile, "utf8").trim()}`;
    this.client = new KubernetesClient(options.connection ?? inClusterConnection());
    for (const node of options.graph.resources) this.descriptors.set(node.id, node);
    this.operations = this.createOperations();
  }
  private descriptor(id: string) {
    const known = this.descriptors.get(id);
    if (known) return known;
    try {
      const inventory = JSON.parse(readFileSync(join(this.options.directory, "inventory.json"), "utf8"));
      const previous = inventory.resources?.[id]?.desired as ResolvedResource | undefined;
      if (previous) { this.descriptors.set(id, previous); return previous; }
    } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
    throw new Error(`Unknown Kubernetes resource ID: ${id}`);
  }
  private path(target: { kind: ResolvedResource["kind"]; name: string; namespace?: string }) { return resourcePath(target, this.options.namespace); }
  private ownership(value: any, id: string) {
    if (value.metadata.annotations?.[ownerAnnotation] !== this.owner || value.metadata.annotations?.[identityAnnotation] !== id)
      throw new Error(`Refusing unmanaged Kubernetes object ${value.metadata.name}`);
  }
  private async raw(id: string) {
    const value = await this.client.get(this.path(this.descriptor(id)));
    if (value) this.ownership(value, id);
    return value;
  }
  /** Read-only observations follow replacement UIDs; they never establish ownership. */
  async readObserved(target: Observation): Promise<Record<string, unknown> | undefined> {
    if (target.access.some(access => access !== "read")) throw new Error("Reactive observations are read-only");
    const value = await this.client.get(this.path(target));
    this.observedVersions.set(reactiveKey(target), value?.metadata.resourceVersion ?? "");
    return value;
  }
  waitForObservationChange(targets: Observation[], signal: AbortSignal, timeoutMs: number) {
    return waitForChanges(this.client.connection, this.options.namespace, targets, this.observedVersions, signal, timeoutMs);
  }
  async observe(id: string): Promise<LiveResource | undefined> {
    const value = await this.raw(id);
    return value ? liveResource(this.descriptor(id), value) : undefined;
  }
  async requestDelete(id: string, uid: string): Promise<void> {
    const value = await this.raw(id);
    if (!value) return;
    if (value.metadata.uid !== uid) throw new Error(`UID conflict on ${id}`);
    await this.client.call("DELETE", this.path(this.descriptor(id)), { apiVersion: "v1", kind: "DeleteOptions", preconditions: { uid }, propagationPolicy: "Foreground" });
  }
  async removeFinalizer(id: string, uid: string, key: string): Promise<void> {
    const value = await this.raw(id);
    if (!value) return;
    if (value.metadata.uid !== uid) throw new Error(`UID conflict on ${id}`);
    const index = (value.metadata.finalizers ?? []).indexOf(key);
    if (index < 0) return;
    await this.client.call("PATCH", this.path(this.descriptor(id)), [
      { op: "test", path: "/metadata/uid", value: uid },
      { op: "test", path: "/metadata/resourceVersion", value: value.metadata.resourceVersion },
      { op: "test", path: `/metadata/finalizers/${index}`, value: key },
      { op: "remove", path: `/metadata/finalizers/${index}` },
    ], "application/json-patch+json");
  }
  private async reconcileResource(resource: ResolvedResource, active: boolean): Promise<LiveResource> {
    const value = await this.raw(resource.id);
    if (!active || value?.metadata.deletionTimestamp) {
      if (!value) throw new Error(`Frozen resource disappeared: ${resource.id}`);
      return liveResource(resource, value);
    }
    const desired = manifest(resource, this.owner, this.options.namespace);
    if (value) {
      desired.metadata.uid = value.metadata.uid;
      desired.metadata.resourceVersion = value.metadata.resourceVersion;
      desired.metadata.finalizers = [...new Set([...(value.metadata.finalizers ?? []), ...resource.finalizerKeys])];
    }
    const path = this.path(resource);
    const applied = value
      ? await this.client.call("PATCH", `${path}?fieldManager=tsx-controller&force=true`, desired, "application/apply-patch+yaml")
      : await this.client.call("POST", `${path.substring(0, path.lastIndexOf("/"))}?fieldManager=tsx-controller`, desired);
    return liveResource(resource, applied);
  }
  async apply(resources: readonly ResolvedResource[], activeIds?: readonly string[]): Promise<void> {
    if (this.applying) throw new Error("Only one Alchemy apply may run at a time");
    this.applying = true;
    try {
      for (const resource of resources) this.descriptors.set(resource.id, resource);
      const active = new Set(activeIds ?? resources.map((resource) => resource.id));
      const included: ResolvedResource[] = [];
      for (const resource of resources) if (active.has(resource.id) || await this.raw(resource.id)) included.push(resource);
      const engine = this;
      const providers = Provider.succeed(Kubernetes, Kubernetes.Provider.of({
        diff: ({ news, olds, output }) => attempt(async () => {
          const resource = news as ResolvedResource;
          if (!active.has(resource.id)) return { action: "noop" as const };
          const value = await engine.raw(resource.id);
          if (value?.metadata.deletionTimestamp) return { action: "update" as const };
          const desired = manifest(resource, engine.owner, engine.options.namespace);
          if (value) desired.metadata.finalizers = [...new Set([...(value.metadata.finalizers ?? []), ...resource.finalizerKeys])];
          const live = value ? liveResource(resource, value) : undefined;
          return { action: value && JSON.stringify(olds) === JSON.stringify(resource) && contains(value, desired) && JSON.stringify(live) === JSON.stringify(output) ? "noop" as const : "update" as const };
        }),
        read: ({ id, output }) => attempt(() => engine.observe(output?.id ?? id)),
        reconcile: ({ news }) => attempt(() => engine.reconcileResource(news, active.has(news.id))),
        delete: ({ output }) => attempt(() => engine.requestDelete(output.id, output.uid)),
        list: () => attempt(async () => (await Promise.all(included.map((r) => engine.observe(r.id)))).filter((r): r is LiveResource => !!r)),
      }));
      const stack = Stack.Stack(this.options.stackName ?? this.options.graph.name, { providers, state: durableState(this.options.directory) }, Effect.gen(function* () {
        for (const resource of included) yield* Kubernetes(resource.id, resource).pipe(RemovalPolicy.retain());
        return {};
      }));
      await Effect.runPromise(deploy({ stack, stage: "controller" }).pipe(provideFreshArtifactStore,
        Effect.provide(durableState(this.options.directory)), Effect.provide(LoggingCli),
        Effect.provide(Layer.succeed(AlchemyContext, { dotAlchemy: this.options.directory, dev: false, adopt: false })), Effect.provide(BunServices.layer)));
    } finally { this.applying = false; }
  }
  private createOperations(): ResourceOperations {
    const file = join(this.options.directory, "kubernetes-observation-uids.json");
    const bindings = (): Record<string, string | null> => {
      try { return JSON.parse(readFileSync(file, "utf8")); }
      catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; return {}; }
    };
    const checked = async (target: Observation, access: "read" | "delete" | "patch", signal?: AbortSignal) => {
      signal?.throwIfAborted();
      if (!target.access.includes(access)) throw new Error(`Missing ${access} permission for ${target.name}`);
      const current = await this.client.get(this.path(target), signal);
      const saved = bindings(), key = observationKey(target), uid = current?.metadata.uid ?? null;
      if (!(key in saved)) { saved[key] = uid; atomicJSON(file, saved); }
      else if (current && saved[key] !== uid) throw new Error(`UID conflict on ${target.name}`);
      return current;
    };
    return {
      get: (target, signal) => checked(target, "read", signal),
      ensureDeleted: async (target, signal) => {
        const value = await checked(target, "delete", signal);
        if (!value) return;
        try { await this.client.call("DELETE", this.path(target), { apiVersion: "v1", kind: "DeleteOptions", preconditions: { uid: value.metadata.uid } }, undefined, signal); }
        catch (error) { if (!(error instanceof KubernetesError && error.status === 404)) throw error; }
        if (await this.client.get(this.path(target), signal)) throw new Error(`Deletion pending for ${target.name}`);
      },
      patch: async (target, patch, signal) => {
        const value = await checked(target, "patch", signal);
        if (!value) return;
        if ("metadata" in patch || "apiVersion" in patch || "kind" in patch) throw new Error("Observation patches may only change resource data or spec");
        await this.client.call("PATCH", this.path(target), { ...patch, metadata: { uid: value.metadata.uid, resourceVersion: value.metadata.resourceVersion } }, "application/merge-patch+json", signal);
      },
    };
  }
}
export const createKubernetesEngine = (options: KubernetesEngineOptions) => new KubernetesEngine(options);
