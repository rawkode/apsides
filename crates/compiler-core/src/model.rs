use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const IR_SCHEMA: &str = "reakt.dev/ir/v1";
pub const REVISION_ANNOTATION: &str = "reakt.dev/revision";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budgets {
    pub max_resources: usize,
    pub max_inputs: usize,
    pub max_reactions: usize,
    pub max_object_bytes: usize,
    pub max_queue_depth: usize,
    pub max_concurrent_requests: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Domain {
    pub id: String,
    pub namespace: String,
    pub control_namespace: String,
    pub cluster_binding: String,
    pub policy_ref: String,
    pub enforcement: String,
    pub budgets: Budgets,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeletionPolicy {
    Orphan,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Lifecycle {
    pub deletion_policy: DeletionPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiResourceScope {
    Namespaced,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApiResourceMapping {
    pub plural: String,
    pub scope: ApiResourceScope,
    pub schema_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resource {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    pub id: String,
    pub api_version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_resource: Option<ApiResourceMapping>,
    pub name: String,
    pub namespace: String,
    pub desired: Value,
    pub lifecycle: Lifecycle,
    pub source: SourceLocation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputMode {
    Managed,
    Observed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    pub id: String,
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: String,
    pub mode: InputMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_resource_id: Option<String>,
    pub projection: Vec<String>,
    pub sensitive: bool,
    pub missing: String,
    pub source: SourceLocation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reaction {
    pub id: String,
    pub op: String,
    pub inputs: Vec<String>,
    pub target: String,
    pub source: SourceLocation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildInput {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    pub compiler: String,
    pub inputs: Vec<BuildInput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ir {
    #[serde(default)]
    pub test_only: bool,
    pub schema_version: String,
    pub domain: Domain,
    pub resources: Vec<Resource>,
    pub inputs: Vec<Input>,
    pub reactions: Vec<Reaction>,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Capability {
    pub namespace: String,
    pub api_group: String,
    pub resource: String,
    pub subresource: Option<String>,
    pub verb: String,
    /// Empty only for the intentionally broader create permission.
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub purpose: String,
    pub dry_run: bool,
    pub capability: Capability,
    pub field_selector: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Effect {
    pub target: String,
    pub operation: String,
    pub field_paths: Vec<String>,
    pub source: SourceLocation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestCase {
    pub reaction_id: String,
    /// Sorted inputs to which this scenario applies; other reaction inputs
    /// remain present at their valid baseline.
    pub input_ids: Vec<String>,
    pub source: SourceLocation,
    pub scenario: String,
    pub layer: String,
    pub assertion: String,
    pub required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompiledPlan {
    pub ir_digest: String,
    pub ir: Ir,
    pub effects: Vec<Effect>,
    pub request_plan: Vec<Request>,
    pub capabilities: Vec<Capability>,
    pub enforcement_gaps: Vec<String>,
    pub indirect_risks: Vec<String>,
    pub test_plan: Vec<TestCase>,
}

/// Independently supplied platform policy; never taken from the source IR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionCeiling {
    pub policy_ref: String,
    pub domain_id: String,
    pub cluster_binding: String,
    pub workload_namespace: String,
    pub control_namespace: String,
    pub capabilities: Vec<Capability>,
    pub accepted_enforcement_gaps: Vec<String>,
    pub accepted_indirect_risks: Vec<String>,
}
