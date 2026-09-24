//! Owned v1 leaves prevent upstream compiler schema changes from widening durable metadata.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewIdentity {
    pub api_group: String,
    pub resource: String,
    pub namespace: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSource {
    pub file: String,
    pub line: u32,
    pub column: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewDeletionPolicy {
    Orphan,
    Delete,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewLifecycle {
    pub deletion_policy: ReviewDeletionPolicy,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewInputMode {
    Managed,
    Observed,
    Output,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewInput {
    pub id: String,
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: String,
    pub mode: ReviewInputMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_resource_id: Option<String>,
    pub projection: Vec<String>,
    pub sensitive: bool,
    pub missing: String,
    pub source: ReviewSource,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewCapability {
    pub namespace: String,
    pub api_group: String,
    pub resource: String,
    pub subresource: Option<String>,
    pub verb: String,
    pub names: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewCeiling {
    pub policy_ref: String,
    pub domain_id: String,
    pub cluster_binding: String,
    pub workload_namespace: String,
    pub control_namespace: String,
    pub capabilities: Vec<ReviewCapability>,
    pub accepted_enforcement_gaps: Vec<String>,
    pub accepted_indirect_risks: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewBudgets {
    pub max_resources: usize,
    pub max_inputs: usize,
    pub max_reactions: usize,
    pub max_object_bytes: usize,
    pub max_queue_depth: usize,
    pub max_concurrent_requests: usize,
}
