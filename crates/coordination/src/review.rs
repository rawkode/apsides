//! Immutable declaration metadata. A reference requires an independent protected binding.
use crate::{ClaimScope, Error, Result};
use serde::{Deserialize, Serialize};

#[path = "review_encoding.rs"]
mod encoding;
#[path = "review_permissions.rs"]
mod permissions;
#[cfg(test)]
#[path = "review_validation_tests.rs"]
mod tests;
#[path = "review_types.rs"]
mod types;
#[path = "review_validation.rs"]
mod validation;
pub use types::*;

pub const REVIEW_METADATA_SCHEMA: &str = "reakt.dev/review-metadata/v1";
pub const MAX_REVIEW_METADATA_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REVIEW_SOURCE_ELEMENTS: usize = 100_000;
pub const MAX_REVIEW_DECLARATIONS: usize = 256;
pub const REVIEW_ROLLOUT_FIELD: &str = "/spec/template/metadata/annotations/reakt.dev~1revision";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewScope {
    pub claim_scope: ClaimScope,
    pub domain_id: String,
    pub transition_id: String,
    #[serde(with = "deploy::allocation_counter")]
    pub sequence: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewRef {
    pub scope: ReviewScope,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ReviewOwnershipPlan {
    CreateIfAbsent,
    PreviouslyManaged,
    AdoptionApproved {
        intent_object_name: String,
        object_uid: String,
        object_resource_version: String,
        observed_uid: String,
        observed_resource_version: String,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewApiResourceScope {
    Namespaced,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewApiResourceMapping {
    pub plural: String,
    pub scope: ReviewApiResourceScope,
    pub schema_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewResource {
    pub id: String,
    pub identity: ReviewIdentity,
    pub api_version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_resource: Option<ReviewApiResourceMapping>,
    pub apply_fields: Vec<String>,
    pub lifecycle: ReviewLifecycle,
    pub force: bool,
    pub source: ReviewSource,
    pub ownership_plan: ReviewOwnershipPlan,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewPrimitive {
    Rollout,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewReaction {
    pub id: String,
    pub op: ReviewPrimitive,
    pub input_ids: Vec<String>,
    pub target_resource_id: String,
    pub effect_fields: Vec<String>,
    pub source: ReviewSource,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewResourceAccess {
    pub namespace: Option<String>,
    pub api_group: String,
    pub resource: String,
    pub verb: String,
    pub resource_names: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewCreationTarget {
    pub namespace: String,
    pub api_group: String,
    pub resource: String,
    pub name: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewRoleCreation {
    HeldCapabilitiesOrEscalate,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewRoleBinding {
    HeldCapabilitiesOrBind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewRoleDelegation {
    pub namespace: String,
    pub role_name: String,
    pub granted_capabilities: Vec<ReviewCapability>,
    pub create_requirement: ReviewRoleCreation,
    pub bind_requirement: ReviewRoleBinding,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewDeploymentScope {
    IssuedDeployerOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewAuthorization {
    RequiredAccessOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewDeploymentLimitation {
    CreateNamesRequireAdmission,
    GeneratedNamesRequireBroadAccess,
    InvokingIdentityBootstrapCredentialsAndDiscoveryExcluded,
    DatabaseCredentialsSeparate,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewDeploymentPermissions {
    pub required_resources: Vec<ReviewResourceAccess>,
    pub creation_targets: Vec<ReviewCreationTarget>,
    pub role_delegation: Vec<ReviewRoleDelegation>,
    pub scope: ReviewDeploymentScope,
    pub authorization: ReviewAuthorization,
    pub limitations: Vec<ReviewDeploymentLimitation>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewApiRequirement {
    pub api_group: String,
    pub resource: String,
    pub subresource: Option<String>,
    pub verb: String,
    pub namespaced: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewSchemaLibrary {
    #[serde(rename = "k8s-openapi/0.28.0")]
    K8sOpenapi0280,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewSchemaMinor {
    #[serde(rename = "1.36")]
    V136,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReviewQualifiedServer {
    #[serde(rename = "v1.34.3")]
    V1343,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewFieldValidation {
    Strict,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewTransitionSchema {
    #[serde(rename = "reakt.dev/transition-plan/v4")]
    V4,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewCompatibility {
    pub runtime_budgets: ReviewBudgets,
    pub schema_library: ReviewSchemaLibrary,
    pub schema_kubernetes_minor: ReviewSchemaMinor,
    pub required_runtime_apis: Vec<ReviewApiRequirement>,
    pub required_cleanup_apis: Vec<ReviewApiRequirement>,
    pub required_deployment_apis: Vec<ReviewApiRequirement>,
    pub deployment_api_scope: ReviewDeploymentScope,
    pub qualified_create_precondition_servers: Vec<ReviewQualifiedServer>,
    pub field_validation: ReviewFieldValidation,
    pub transition_plan_schema: ReviewTransitionSchema,
    pub runtime_replicas: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewEnforcementProfile {
    #[serde(rename = "api-bound")]
    ApiBound,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewCeilingDecision {
    ApprovedByPlanner,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewEnforcement {
    pub profile: ReviewEnforcementProfile,
    pub gaps: Vec<String>,
    pub indirect_risks: Vec<String>,
    pub runtime_ceiling: ReviewCeiling,
    pub cleanup_ceiling: ReviewCeiling,
    pub decision: ReviewCeilingDecision,
}

/// This transition's planned declaration and policy metadata, not execution evidence.
/// Desired values, source bodies, private-value hashes and build commitments are excluded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewMetadataV1 {
    pub schema_version: String,
    pub scope: ReviewScope,
    pub resources: Vec<ReviewResource>,
    pub inputs: Vec<ReviewInput>,
    pub reactions: Vec<ReviewReaction>,
    pub runtime_permissions: Vec<ReviewCapability>,
    pub cleanup_permissions: Vec<ReviewCapability>,
    pub deployment_permissions: ReviewDeploymentPermissions,
    pub compatibility: ReviewCompatibility,
    pub enforcement: ReviewEnforcement,
}

impl ReviewScope {
    /// Validate allocation naming and namespace scope without granting authority.
    /// # Errors
    /// Rejects invalid scope or a transition from another registry incarnation or sequence.
    pub fn validate(&self) -> Result<()> {
        self.claim_scope.validate()?;
        let names = deploy::allocation_names(&self.claim_scope.registry_uid, self.sequence)
            .map_err(|_| Error::Invalid("review allocation scope"))?;
        if !validation::namespace(&self.domain_id)
            || self.domain_id.len() > 40
            || self.transition_id != names.transition_id
        {
            return Err(Error::Invalid("review allocation scope"));
        }
        Ok(())
    }
}
impl ReviewRef {
    /// Validate reference syntax without establishing protected-reference authority.
    /// # Errors
    /// Rejects malformed scope or digest.
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        if !encoding::valid_digest(&self.digest) {
            return Err(Error::Invalid("review reference digest"));
        }
        Ok(())
    }
}
impl ReviewMetadataV1 {
    /// Validate canonical, bounded, internally coherent metadata. Publication must
    /// separately compare this body to the trusted complete platform projection.
    /// # Errors
    /// Rejects invalid vocabulary, ordering, links, permissions, scope or size.
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        encoding::validate_size(self)?;
        validation::validate(self)?;
        permissions::validate(self)
    }
}
