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
mod permissions;
mod predecessor;
mod writer_checkpoint;
pub use permissions::{
    CreationTarget, DeploymentPermissions, ResourceAccess, RoleDelegation, deployment_permissions,
};
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
pub use cleanup::CleanupProgress;
#[cfg(feature = "deployer")]
mod controller;
mod credentials;
mod crypto;
mod model;
mod ownership;
pub use ownership::OWNERSHIP_INTENT_NAMESPACE;
mod runtime;
mod staging;
mod store;
mod termination;
pub use credentials::DeploymentIdentity;
pub use crypto::install_rustls_ring_provider;
pub use model::*;
pub use runtime::RuntimeAuthority;
pub use store::PlatformClient;
pub use termination::{
    ExpectedPod, ExpectedWriters, ProcessTerminationObservation, TERMINATION_FINALIZER,
};

pub const SCHEMA_VERSION: &str = "reakt.dev/platform/v1";
pub const MAX_STATE_BYTES: usize = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("platform state rejected: {0}")]
    Invalid(String),
    #[error("platform operation pending: {0}")]
    Pending(String),
    #[error("platform activation is not active")]
    Inactive,
    #[error("Kubernetes platform request failed: {0}")]
    Kubernetes(#[from] kube::Error),
    #[error(transparent)]
    Core(#[from] compiler_core::Error),
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
pub use proxy::{ADMIN_IMAGE, ENVOY_IMAGE, MAX_PROXY_LEDGER_BYTES, ProxyDrainObservation};

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
