// Generated from renderer-abi Rust types. Do not edit.
export type JsonPrimitive = null | boolean | number | string;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export type RootProjection = { apiVersion: string, kind: string, namespace: string, name: string, uid: string, generation: number, spec: JsonValue, };

export type Observation = { apiVersion: string, kind: string, namespace: string, name: string, uid: string, resourceVersion: string, value: JsonValue, };

export type RenderInputV0 = { abiVersion: string, root: RootProjection, bundleDigest: string, cacheEpoch: string, observationToken: string, observations: Array<Observation>, };

export type Dependency = { "type": "object", apiVersion: string, kind: string, namespace: string, name: string, uid: string | null, resourceVersion: string | null, observationToken: string, } | { "type": "collection", apiVersion: string, kind: string, namespace: string, selector: string, collectionRevision: string, observationToken: string, };

export type ReadinessDeclaration = { policy: string, parameters: JsonValue | null, };

export type ResourceDeclaration = { logicalId: string, object: JsonValue, readiness: ReadinessDeclaration | null, };

export type AutomationDeclaration = { identity: string, version: string,
/**
 * The exact observed object that caused this automation. Collections and
 * unobserved objects are not valid automation sources.
 */
sourceDependency: Dependency,
/**
 * Canonical event payload supplied to the action module for this
 * automation. It is captured at declaration time so retries use the
 * exact same authored metadata.
 */
eventMetadata: JsonValue,
/**
 * The digest-addressed installed bundle that owns this declaration.
 */
bundleDigest: string, actionModuleIdentity: string, actionModuleDigest: string, rendererAbiVersion: string, };

export type DiagnosticSeverity = "info" | "warning" | "error";

export type Diagnostic = { severity: DiagnosticSeverity, code: string, message: string, };

export type RenderDisposition = "apply" | "hold" | "explicitEmpty";

export type RenderOutputV0 = { abiVersion: string, disposition: RenderDisposition, resources: Array<ResourceDeclaration>, dependencies: Array<Dependency>, automations: Array<AutomationDeclaration>, diagnostics: Array<Diagnostic>, };

export type ActionInputV0 = { abiVersion: string, bundleDigest: string, automationIdentity: string, automationVersion: string, actionModuleIdentity: string, actionModuleDigest: string, occurrenceId: string, sourceTransitionId: string, sourceUid: string, sourceResourceVersion: string, eventMetadata: JsonValue, };

export type ActionDeclaration = { logicalId: string, kind: string, specification: JsonValue, };

export type ActionOutputV0 = { abiVersion: string, actions: Array<ActionDeclaration>, diagnostics: Array<Diagnostic>, };
