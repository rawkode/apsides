#![forbid(unsafe_code)]
mod handler_budgets;
mod json;
mod kubernetes_version;
mod model;
mod resource_api;
mod review;
mod rollout;
mod schema;
mod semantic;
mod surface;
mod surface_diff;
mod surface_diff_markdown;
mod surface_policy;
mod test_plan;
mod validate;

pub use handler_budgets::{
    HANDLER_ARRAY_BUFFER_CAP_BYTES, HANDLER_FETCH_BUDGET_TOTAL_BYTES,
    HANDLER_FETCH_REQUEST_BODY_BYTES, HANDLER_HEAP_HEADROOM_TOTAL_BYTES,
    HANDLER_MAX_CONCURRENT_FETCHES, POD_MEMORY_BASELINE_BYTES, POD_MEMORY_SLACK_BYTES,
    array_buffer_cap_bytes, controller_memory_limit_bytes,
};
pub use json::{canonical_bytes, digest, parse_strict};
pub use kubernetes_version::{
    QUALIFIED_FLOOR as KUBERNETES_QUALIFIED_FLOOR, is_supported_min_version, parse_min_version,
    parse_server_version,
};
pub use model::*;
pub use resource_api::{ResolvedApiResource, resolve_namespaced_api, resolve_resource_api};
pub use review::review_documents;
pub use rollout::*;
pub use semantic::*;
pub use surface::{
    SURFACE_SCHEMA, Surface, SurfaceDomain, SurfaceEgress, SurfaceHandler, SurfaceImage,
    SurfaceInput, SurfaceKubernetes, SurfaceOutput, SurfacePermission, SurfaceReaction,
    SurfaceResource, SurfaceSecretRead, SurfaceVerb, derive_surface,
};
pub use surface_diff::{
    DomainChange, EgressDiff, HandlerChange, HandlersDiff, ImageChange, ImagesDiff, InputChange,
    InputsDiff, KubernetesChange, OutputChange, OutputsDiff, PermissionChange,
    PermissionVerbChange, PermissionsDiff, ReactionsDiff, ResourceChange, ResourcesDiff,
    SURFACE_DIFF_SCHEMA, SecretsReadDiff, StringSetDiff, SurfaceDiff, ValueChange, diff_surfaces,
};
pub use surface_diff_markdown::render_markdown;
pub use surface_policy::{
    PolicyRule, SURFACE_POLICY_SCHEMA, SurfacePolicy, evaluate_policy, parse_policy,
};
pub use validate::{validate, validate_ceiling};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid Apsides declaration: {0}")]
    Invalid(String),
    #[error("platform permission ceiling rejected plan: {0}")]
    Ceiling(String),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Parse strict JSON and derive a trusted plan from the accepted IR vocabulary.
///
/// # Errors
/// Rejects malformed JSON, unknown fields, and unsupported IR contracts.
pub fn parse_and_validate(input: &str) -> Result<CompiledPlan> {
    let ir = serde_json::from_value::<Ir>(parse_strict(input)?)
        .map_err(|error| Error::Invalid(format!("IR schema mismatch: {error}")))?;
    validate(&ir)
}
