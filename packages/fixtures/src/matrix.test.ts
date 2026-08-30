import React from "react";
import { createOxideLoopRoot } from "@oxideloop/react-renderer";
import { jsx } from "@oxideloop/react-like-runtime/jsx-runtime";
import { createRenderer } from "@oxideloop/react-like-runtime";
import { ReactWorkload, Workload } from "./workload.js";
import { createHookSession, useAutomation, useResource } from "@oxideloop/sdk/hooks";
import { hookEnvironment } from "./host-environment.js";
import type { RenderInputV0 } from "@oxideloop/sdk";
const presentSession = createHookSession({ observationToken: "present", read: ref => ({ ref: { ...ref, namespace: ref.namespace ?? "default" }, present: true, value: { ...ref, spec: {}, uid: "svc-1", resourceVersion: "7" } as never, observation: { uid: "svc-1", resourceVersion: "7" } }) });
const root = createOxideLoopRoot(presentSession);
root.render(React.createElement(ReactWorkload, { name: "api", session: presentSession }));
await new Promise(resolve => setTimeout(resolve, 0));
if (root.snapshot()[0]?.type !== "Deployment") throw new Error("React renderer did not produce Deployment");
if (JSON.stringify(root.trace().dependencies) !== JSON.stringify([{ type: "object", apiVersion: "v1", kind: "Service", namespace: "default", name: "api-service", uid: "svc-1", resourceVersion: "7", observationToken: "present" }])) throw new Error("present dependency identity changed");
const absentSession = createHookSession({ observationToken: "absent", read: ref => ({ ref: { ...ref, namespace: ref.namespace ?? "default" }, present: false, value: undefined as never, observation: { uid: null, resourceVersion: null } }) });
const element = jsx(Workload, { name: "api", session: absentSession });
const output = createRenderer(absentSession).render(element);
if (output.resources.length !== 1) throw new Error("React-like runtime did not produce one resource");
const dependency = output.dependencies[0];
if (JSON.stringify(dependency) !== JSON.stringify({ type: "object", apiVersion: "v1", kind: "Service", namespace: "default", name: "api-service", uid: null, resourceVersion: null, observationToken: "absent" })) throw new Error("dependency identity or absence semantics changed");
if (output.dependencies.length !== 1) throw new Error("repeated reads must deduplicate deterministically");

const tombstoneInput: RenderInputV0 = {
  abiVersion: "renderer.oxideloop.dev/v0",
  root: { apiVersion: "demo.oxideloop.dev/v1alpha1", kind: "WebApplication", namespace: "default", name: "demo", uid: "root-1", generation: 1, spec: {} },
  bundleDigest: "a".repeat(64),
  cacheEpoch: "0",
  observationToken: "tombstone",
  observations: [{ apiVersion: "v1", kind: "Service", namespace: "default", name: "api-service", uid: "svc-gone", resourceVersion: "9", value: null }],
};
const tombstoneEnvironment = hookEnvironment(tombstoneInput);
const tombstoneHandle = tombstoneEnvironment.read({ apiVersion: "v1", kind: "Service", name: "api-service" });
if (tombstoneHandle.present || tombstoneHandle.value !== undefined || JSON.stringify(tombstoneHandle.observation) !== JSON.stringify({ uid: null, resourceVersion: null })) {
  throw new Error("tombstone observations must be absent to hooks");
}
const tombstoneSession = createHookSession(tombstoneEnvironment);
useResource(tombstoneSession, { apiVersion: "v1", kind: "Service", name: "api-service" });
if (tombstoneSession.automations.length !== 0) throw new Error("tombstone observations must not declare automations");

const metadataSession = createHookSession({ observationToken: "metadata", read: ref => ({ ref, present: true, value: { ...ref, uid: "uid-1", resourceVersion: "1" } as never, observation: { uid: "uid-1", resourceVersion: "1" } }) });
const validAutomation = {
  identity: "demo.metadata",
  version: "1",
  sourceDependency: { type: "object" as const, apiVersion: "v1", kind: "ConfigMap", namespace: "default", name: "input", uid: "uid-1", resourceVersion: "1", observationToken: "metadata" },
  eventMetadata: { source: { namespace: "default", name: "input" }, note: "changed" },
  bundleDigest: "a".repeat(64),
  actionModuleIdentity: "demo.action",
  actionModuleDigest: "b".repeat(64),
  rendererAbiVersion: "renderer.oxideloop.dev/v0",
};
useAutomation(metadataSession, validAutomation);
if (metadataSession.automations.length !== 1) throw new Error("valid event metadata was not retained");
const expectMetadataRejected = (identity: string, eventMetadata: unknown) => {
  try {
    useAutomation(metadataSession, { ...validAutomation, identity, eventMetadata: eventMetadata as never });
    throw new Error(`${identity} was accepted`);
  } catch (error) {
    if (error instanceof Error && error.message === `${identity} was accepted`) throw error;
  }
};
const expectSourceRejected = (identity: string, sourceDependency: unknown) => {
  try {
    useAutomation(metadataSession, { ...validAutomation, identity, sourceDependency: sourceDependency as never });
    throw new Error(`${identity} was accepted`);
  } catch (error) {
    if (error instanceof Error && error.message === `${identity} was accepted`) throw error;
  }
};
const sparse: unknown[] = [];
sparse.length = 1;
expectMetadataRejected("demo.sparse", sparse);
expectMetadataRejected("demo.date", new Date());
expectMetadataRejected("demo.map", new Map([["note", "changed"]]));
class MetadataClass { readonly note = "changed"; }
expectMetadataRejected("demo.class", new MetadataClass());
let deep: unknown = null;
for (let index = 0; index < 65; index += 1) deep = [deep];
expectMetadataRejected("demo.deep", deep);
expectMetadataRejected("demo.string", "x".repeat(256 * 1024 + 1));
const getterMetadata = {} as { readonly note: string };
Object.defineProperty(getterMetadata, "note", { enumerable: true, get: () => "changed" });
expectMetadataRejected("demo.getter", getterMetadata);
expectMetadataRejected("demo.key", { ["x".repeat(256 * 1024 + 1)]: "changed" });
expectMetadataRejected("demo.surrogate-key", { ["\ud800"]: "changed" });

expectSourceRejected("demo.source-number", { ...validAutomation.sourceDependency, uid: 1 });
expectSourceRejected("demo.source-object", { ...validAutomation.sourceDependency, uid: {} });
expectSourceRejected("demo.source-extra", { ...validAutomation.sourceDependency, extra: "nope" });
const missingSource = { ...validAutomation.sourceDependency };
delete (missingSource as { resourceVersion?: string }).resourceVersion;
expectSourceRejected("demo.source-missing", missingSource);
expectSourceRejected("demo.source-collection", { type: "collection", apiVersion: "v1", kind: "ConfigMap", namespace: "default", name: "input", selector: "all", collectionRevision: "1", observationToken: "metadata" });
const getterSource = { ...validAutomation.sourceDependency };
Object.defineProperty(getterSource, "uid", { enumerable: true, get: () => "uid-1" });
expectSourceRejected("demo.source-getter", getterSource);
const hiddenSource = { ...validAutomation.sourceDependency };
Object.defineProperty(hiddenSource, "uid", { enumerable: false, value: "uid-1" });
expectSourceRejected("demo.source-hidden", hiddenSource);

const mutableMetadata = { source: { namespace: "default", name: "input" }, note: "before" };
const mutableSource = { ...validAutomation.sourceDependency };
const mutableDeclaration = { ...validAutomation, identity: "demo.snapshot", sourceDependency: mutableSource, eventMetadata: mutableMetadata };
const acceptedSnapshot = useAutomation(metadataSession, mutableDeclaration);
mutableMetadata.note = "after";
mutableSource.uid = "uid-after";
const acceptedSource = acceptedSnapshot.sourceDependency;
if ((acceptedSnapshot.eventMetadata as { note: string }).note !== "before" || acceptedSource.type !== "object" || acceptedSource.uid !== "uid-1") throw new Error("automation snapshot was mutable");

const boundaryLength = Math.floor((512 * 1024 - 7) / 2);
useAutomation(metadataSession, { ...validAutomation, identity: "demo.boundary", eventMetadata: ["x".repeat(boundaryLength), "x".repeat(boundaryLength + 1)] });
expectMetadataRejected("demo.boundary-over", ["x".repeat(boundaryLength), "x".repeat(boundaryLength + 2)]);
console.log("runtime matrix: react=ok react-like=ok hook-phase=ok");
