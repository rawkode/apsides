// Generated from oxideloop-renderer-abi Rust types. Do not edit.
export type JsonPrimitive = null | boolean | number | string;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export type RootProjection = { apiVersion: string, kind: string, namespace: string, name: string, uid: string, generation: number, spec: JsonValue, };

export type Observation = { apiVersion: string, kind: string, namespace: string, name: string, uid: string, resourceVersion: string, value: JsonValue, };

export type RenderInputV0 = { abiVersion: string, root: RootProjection, bundleDigest: string, cacheEpoch: string, observationToken: string, observations: Array<Observation>, };

export type Dependency = { "type": "object", apiVersion: string, kind: string, namespace: string, name: string, uid: string | null, resourceVersion: string | null, } | { "type": "collection", apiVersion: string, kind: string, namespace: string, selector: string, collectionRevision: string, };

export type ReadinessDeclaration = { policy: string, parameters: JsonValue | null, };

export type ResourceDeclaration = { logicalId: string, object: JsonValue, readiness: ReadinessDeclaration | null, };

export type AutomationDeclaration = { identity: string, sourceDependency: string, actionModule: string, };

export type DiagnosticSeverity = "info" | "warning" | "error";

export type Diagnostic = { severity: DiagnosticSeverity, code: string, message: string, };

export type RenderDisposition = "apply" | "hold" | "explicitEmpty";

export type RenderOutputV0 = { abiVersion: string, disposition: RenderDisposition, resources: Array<ResourceDeclaration>, dependencies: Array<Dependency>, automations: Array<AutomationDeclaration>, diagnostics: Array<Diagnostic>, };

export type ActionInputV0 = { abiVersion: string, automationIdentity: string, occurrenceId: string, sourceUid: string, sourceResourceVersion: string, eventMetadata: JsonValue, };

export type ActionDeclaration = { logicalId: string, kind: string, specification: JsonValue, };

export type ActionOutputV0 = { abiVersion: string, actions: Array<ActionDeclaration>, diagnostics: Array<Diagnostic>, };
