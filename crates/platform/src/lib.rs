#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]
#![forbid(unsafe_code)]
//! Kubernetes-backed protected state. Hashes identify content; authenticated
//! Kubernetes transport and platform-owned RBAC establish the writer boundary.
mod capacity_checks;
#[cfg(feature = "deployer")]
mod capacity_resize;
#[cfg(feature = "deployer")]
mod completion_envelope;
mod control_capacity;
#[cfg(feature = "deployer")]
mod envelope_record;
#[cfg(feature = "deployer")]
mod envelope_writer;
mod evidence_profile;
mod executor_binding;
pub use executor_binding::{EXECUTOR_BINDING_SCHEMA, ExecutorBinding};
mod permissions;
#[cfg(all(test, feature = "test-support"))]
mod phase_interruption;
#[cfg(all(test, feature = "test-support"))]
mod phase_interruption_live;
#[cfg(feature = "deployer")]
mod preview_adapter;
#[cfg(all(test, feature = "test-support"))]
mod preview_issued_live;
#[cfg(feature = "deployer")]
mod preview_permissions;
#[cfg(feature = "deployer")]
pub use preview_adapter::{
    LivePreviewReport, PendingPreview, PendingPreviewBinding, PendingPreviewOutcome,
    PendingPreviewUnavailable, PreviewCondition, PreviewInterval,
};
#[cfg(feature = "deployer")]
pub use preview_permissions::{
    PreviewPermissionLimitation, PreviewPermissions, PreviewTargetRequest, preview_permissions,
};
#[cfg(feature = "deployer")]
pub mod preview_composition;
#[cfg(feature = "deployer")]
pub mod preview_fields;
#[cfg(feature = "deployer")]
mod preview_limits;
#[cfg(feature = "deployer")]
pub mod preview_transport;
mod review_metadata;
mod review_projection;
mod ssa_request;
mod state_capacity;
#[cfg(feature = "deployer")]
mod topology_capture;
mod transition_inputs;
mod transition_record;
pub use evidence_profile::{CapturedContainer, CapturedContainers, EvidenceProfile};
pub use ssa_request::{ApplyPrecondition, ConditionalApplyRequest, conditional_apply_request};
pub use state_capacity::CapacityAdmission;
pub use transition_inputs::{FrozenPredecessor, FrozenRegistrySnapshot, TransitionInputs};
#[cfg(feature = "deployer")]
mod transition_session;
#[cfg(feature = "deployer")]
pub use transition_session::{ResolvedTransition, TransitionSession};
mod drain_evidence;
pub use drain_evidence::{ProcessTerminationObservation, ProxyDrainObservation};
#[cfg(all(test, feature = "deployer"))]
mod tests;
mod writer_checkpoint;
pub use permissions::{
    CreationTarget, DeploymentPermissions, ResourceAccess, RoleDelegation, deployment_permissions,
};
pub use review_projection::project_review_metadata;
pub use writer_checkpoint::WriterCheckpoint;

#[cfg(feature = "test-support")]
mod test_support;
#[cfg(feature = "test-support")]
pub use test_support::{DisposableTestCluster, TestClusterTarget};
#[cfg(feature = "deployer")]
mod activation;
#[cfg(feature = "deployer")]
mod cleanup;
#[cfg(feature = "deployer")]
mod inventory_capture;
#[cfg(feature = "deployer")]
mod inventory_store;
#[cfg(feature = "deployer")]
mod issued_deployment;
#[cfg(feature = "deployer")]
pub use cleanup::CleanupProgress;
#[cfg(feature = "deployer")]
pub use issued_deployment::IssuedDeployment;
#[cfg(feature = "deployer")]
mod controller;
mod credentials;
mod crypto;
mod model;
mod ownership;
mod registry;
mod registry_model;
mod transition_audit;
pub use ownership::OWNERSHIP_INTENT_NAMESPACE;
mod runtime;
#[cfg(feature = "deployer")]
mod staging;
mod store;
mod termination;
pub use credentials::{
    DEPLOYMENT_IDENTITY_SCHEMA_V2, DeploymentIdentity, DeploymentPreviewApproval,
    DeploymentPreviewProfile,
};
pub use crypto::install_rustls_ring_provider;
pub use model::*;
pub use runtime::RuntimeAuthority;
pub use store::PlatformClient;
pub use termination::{ExpectedPod, ExpectedWriters, TERMINATION_FINALIZER};

/// Return the fixed, bounded name of the separately provisioned incarnation anchor.
/// # Errors
/// Rejects invalid or overlapping Kubernetes namespace scopes.
pub fn registry_authority_name(
    control_namespace: &str,
    workload_namespace: &str,
) -> Result<String> {
    if !valid_namespace(control_namespace)
        || !valid_namespace(workload_namespace)
        || control_namespace == workload_namespace
        || control_namespace == REGISTRY_AUTHORITY_NAMESPACE
        || workload_namespace == REGISTRY_AUTHORITY_NAMESPACE
    {
        return Err(invalid("invalid registry authority scope"));
    }
    Ok(format!(
        "reakt-registry-authority.{control_namespace}.{workload_namespace}"
    ))
}

pub const REGISTRY_AUTHORITY_NAMESPACE: &str = "reakt-platform";
pub const REGISTRY_SCHEMA_VERSION: &str = "reakt.dev/namespace-registry/v8";
pub const REGISTRY_AUTHORITY_SCHEMA_VERSION: &str = "reakt.dev/registry-authority/v1";
pub const SCHEMA_VERSION: &str = "reakt.dev/platform/v1";
pub const MAX_STATE_BYTES: usize = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("platform state rejected: {0}")]
    Invalid(String),
    #[error("platform operation pending: {0}")]
    Pending(String),
    #[error("writer topology exceeds admitted evidence capacity; resize before quiescence")]
    CapacityResizeRequired,
    #[error("platform activation is not active")]
    Inactive,
    #[error("Kubernetes platform request failed: {0}")]
    Kubernetes(#[from] kube::Error),
    #[error(transparent)]
    Core(#[from] compiler_core::Error),
    #[error(transparent)]
    Coordination(#[from] coordination::Error),
    #[error(transparent)]
    Inventory(#[from] inventory::Error),
    #[error(transparent)]
    Deploy(#[from] deploy::Error),
    #[error("platform state serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

fn pending(message: &str) -> Error {
    Error::Pending(message.into())
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn valid_namespace(value: &str) -> bool {
    value.len() <= 63 && !value.contains('.') && valid_name(value)
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'))
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

#[cfg(feature = "deployer")]
mod proxy;
#[cfg(feature = "deployer")]
pub use proxy::{ADMIN_IMAGE, ENVOY_IMAGE, MAX_PROXY_LEDGER_BYTES};

#[cfg(feature = "deployer")]
mod proxy_config;
#[cfg(feature = "deployer")]
pub use proxy_config::{envoy_configuration, envoy_token_sds};

#[cfg(feature = "deployer")]
mod tls;

#[cfg(feature = "deployer")]
mod tls_integrity;

#[cfg(feature = "deployer")]
mod proxy_stage;
#[cfg(feature = "deployer")]
pub use proxy_stage::ProxyPolicy;

#[cfg(feature = "deployer")]
mod proxy_identity;

#[cfg(feature = "deployer")]
mod quiescence;
#[cfg(feature = "deployer")]
mod retirement;
#[cfg(feature = "deployer")]
mod upgrade;

#[cfg(test)]
mod allocation_tests {
    #[test]
    fn authority_name_preserves_maximum_namespace_scopes() -> crate::Result<()> {
        let control = "c".repeat(63);
        let workload = "w".repeat(63);
        let name = super::registry_authority_name(&control, &workload)?;
        assert!(name.len() <= 253);
        assert!(name.split('.').all(|label| label.len() <= 63));
        assert_ne!(name, super::registry_authority_name(&workload, &control)?);
        for invalid in [
            "a.b",
            "",
            "-control",
            "control-",
            "Control",
            &"c".repeat(64),
        ] {
            assert!(super::registry_authority_name(invalid, "workload").is_err());
            assert!(super::registry_authority_name("control", invalid).is_err());
        }
        Ok(())
    }
}
