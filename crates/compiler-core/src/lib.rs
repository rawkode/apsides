#![forbid(unsafe_code)]
mod json;
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

pub use json::{canonical_bytes, digest, parse_strict};
pub use model::*;
pub use resource_api::{ResolvedApiResource, resolve_namespaced_api, resolve_resource_api};
pub use review::review_documents;
pub use rollout::*;
pub use semantic::*;
pub use surface::{
    SURFACE_SCHEMA, Surface, SurfaceDomain, SurfaceImage, SurfaceInput, SurfaceKubernetes,
    SurfacePermission, SurfaceReaction, SurfaceResource, SurfaceSecretRead, SurfaceVerb,
    derive_surface,
};
pub use surface_diff::{
    DomainChange, ImageChange, ImagesDiff, InputChange, InputsDiff, KubernetesChange,
    PermissionChange, PermissionVerbChange, PermissionsDiff, ReactionsDiff, ResourceChange,
    ResourcesDiff, SURFACE_DIFF_SCHEMA, SecretsReadDiff, StringSetDiff, SurfaceDiff, ValueChange,
    diff_surfaces,
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
