use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

pub const ABI_VERSION_V0: &str = "renderer.oxideloop.dev/v0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootProjection {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    #[ts(type = "number")]
    pub generation: u64,
    #[ts(type = "JsonValue")]
    pub spec: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observation {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub resource_version: String,
    #[ts(type = "JsonValue")]
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderInputV0 {
    pub abi_version: String,
    pub root: RootProjection,
    pub bundle_digest: String,
    pub cache_epoch: String,
    pub observation_token: String,
    pub observations: Vec<Observation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Dependency {
    Object {
        api_version: String,
        kind: String,
        namespace: String,
        name: String,
        uid: Option<String>,
        resource_version: Option<String>,
    },
    Collection {
        api_version: String,
        kind: String,
        namespace: String,
        selector: String,
        collection_revision: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadinessDeclaration {
    pub policy: String,
    #[ts(type = "JsonValue | null")]
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceDeclaration {
    pub logical_id: String,
    #[ts(type = "JsonValue")]
    pub object: Value,
    pub readiness: Option<ReadinessDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationDeclaration {
    pub identity: String,
    pub source_dependency: String,
    pub action_module: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub enum RenderDisposition {
    Apply,
    Hold,
    ExplicitEmpty,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderOutputV0 {
    pub abi_version: String,
    pub disposition: RenderDisposition,
    pub resources: Vec<ResourceDeclaration>,
    pub dependencies: Vec<Dependency>,
    pub automations: Vec<AutomationDeclaration>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionInputV0 {
    pub abi_version: String,
    pub automation_identity: String,
    pub occurrence_id: String,
    pub source_uid: String,
    pub source_resource_version: String,
    #[ts(type = "JsonValue")]
    pub event_metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionDeclaration {
    pub logical_id: String,
    pub kind: String,
    #[ts(type = "JsonValue")]
    pub specification: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionOutputV0 {
    pub abi_version: String,
    pub actions: Vec<ActionDeclaration>,
    pub diagnostics: Vec<Diagnostic>,
}
