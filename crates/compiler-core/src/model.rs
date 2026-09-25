use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const IR_SCHEMA: &str = "apsides.dev/ir/v2";
pub const REVISION_ANNOTATION: &str = "apsides.dev/revision";
/// Reserved label marking a managed `ConfigMap` as a handler output: required
/// on every `handlerOutput` resource, rejected on every other resource.
pub const HANDLER_OUTPUT_LABEL: &str = "apsides.dev/handler-output";
/// The only supported handler trigger kind in this increment.
pub const HANDLER_TRIGGER_INPUT_CHANGED: &str = "input-changed";
/// A handler's declared timeout may not exceed 30 seconds.
pub const MAX_HANDLER_TIMEOUT_MILLIS: u32 = 30_000;
/// A handler's declared V8 heap ceiling may not exceed 64 MiB.
pub const MAX_HANDLER_HEAP_BYTES: u64 = 64 * 1024 * 1024;
/// At most 16 handlers per domain.
pub const MAX_HANDLERS: usize = 16;
/// At most 16 distinct egress hosts across every handler in a domain.
pub const MAX_EGRESS_HOSTS: usize = 16;
/// At most 256 total `(host, port, handler)` egress rows across every
/// handler in a domain.
///
/// `MAX_HANDLERS` handlers each declaring up to `MAX_EGRESS_HOSTS` hosts at
/// several distinct ports can otherwise exceed the review metadata
/// catalog's own row budget (`coordination`'s `MAX_REVIEW_DECLARATIONS`,
/// also 256): this bound makes that a clear compile-time error instead of
/// an opaque review-time rejection.
pub const MAX_EGRESS_ROWS: usize = 256;

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
    /// The smallest Kubernetes version this domain requires, as `"MAJOR.MINOR"`
    /// (major fixed at 1, minor >= 34 — the smallest release the absent-only
    /// conditional SSA create contract is qualified for). Always present in
    /// the IR; the SDK fills the default ("1.34") when an author omits it.
    pub min_kubernetes_version: String,
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
    /// True for the managed `ConfigMap` a handler `Output` compiles to. Its
    /// desired value carries only `metadata` (never `data`) and must carry
    /// the reserved [`HANDLER_OUTPUT_LABEL`]; every other resource must not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub handler_output: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputMode {
    Managed,
    Observed,
    /// A non-sensitive `ConfigMap` input auto-created for a handler
    /// [`Output`], observed the same way as [`InputMode::Observed`] but
    /// distinguished so review and surface projections can label it
    /// accurately. `managedResourceId` names the output's resource.
    Output,
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

/// A handler-written managed `ConfigMap` output. The resource itself (kind
/// `ConfigMap`, `handlerOutput: true`) lives in `Ir::resources`; this row is
/// only the output's own identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Output {
    pub id: String,
    pub name: String,
    pub namespace: String,
}

/// The only supported handler trigger shape in this increment: fires when
/// the named input's revision changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandlerTrigger {
    pub kind: String,
    pub input: String,
}

/// An egress host a handler's `fetch` op may reach: a lowercase DNS name
/// (never an IP literal or a wildcard), always over HTTPS.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EgressHost {
    pub host: String,
    pub port: u16,
}

/// A Brigade-style event handler.
///
/// A bundled TypeScript module invoked when its trigger input's revision
/// changes, reading declared inputs, reaching declared egress hosts, and
/// writing declared outputs. Runtime execution is out of scope for this IR:
/// the compiled bundle is embedded in the controller image for a later phase
/// to run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Handler {
    pub id: String,
    pub module: String,
    pub export: String,
    pub trigger: HandlerTrigger,
    pub inputs: Vec<String>,
    pub egress: Vec<EgressHost>,
    pub outputs: Vec<String>,
    pub timeout_millis: u32,
    pub max_heap_bytes: u64,
    pub bundle_sha256: String,
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
    pub handlers: Vec<Handler>,
    pub outputs: Vec<Output>,
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
