#![forbid(unsafe_code)]
//! Apsides' implementation acceptance suite.
//!
//! The suite deploys the example programs with the public `aps` CLI on
//! disposable kind clusters and checks what the specification's acceptance
//! table requires of them (docs/reference/ACCEPTANCE.md maps each scenario to
//! its row). `acceptance-prepare` builds and signs the releases the scenarios
//! deploy and writes an inputs file; the scenarios in `tests/kind.rs` read it
//! from [`inputs::ENVIRONMENT`] and fail, never skip, when it is missing.
//!
//! Everything here reaches a cluster through an explicit kubeconfig kept in a
//! private temporary directory. Nothing reads `KUBECONFIG`, `~/.kube/config`
//! or in-cluster configuration, and `aps` itself is only ever given the
//! scenario's own short-lived issuer kubeconfig.

pub mod aps;
pub mod bootstrap;
pub mod cnpg;
pub mod inputs;
pub mod node;
pub mod objects;
pub mod pair;
pub mod prepare;
pub mod record;
pub mod wait;
pub mod workload;

/// Why a step of the suite failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The prepared inputs are missing, malformed or inconsistent.
    #[error("acceptance inputs: {0}")]
    Input(String),
    /// An acceptance assertion did not hold.
    #[error("acceptance check failed: {0}")]
    Check(String),
    /// Something the suite depends on is unavailable or misbehaved.
    #[error("test infrastructure failed: {0}")]
    Infrastructure(String),
    /// A condition was not reached in time.
    #[error("deadline exceeded: {0}")]
    Deadline(String),
    #[error(transparent)]
    Cluster(#[from] test_cluster::Error),
    #[error("Kubernetes: {0}")]
    Kube(#[from] kube::Error),
    #[error("platform: {0}")]
    Platform(#[from] platform::Error),
    #[error("coordination store: {0}")]
    Coordination(#[from] coordination::Error),
    #[error("release: {0}")]
    Release(#[from] release::Error),
    #[error("compiler core: {0}")]
    Core(#[from] compiler_core::Error),
    #[error("deployment core: {0}")]
    Deploy(#[from] deploy::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T = ()> = std::result::Result<T, Error>;

/// Fail the current check with `message` unless `condition` holds.
///
/// # Errors
/// Returns [`Error::Check`] when `condition` is false.
pub fn ensure(condition: bool, message: impl Into<String>) -> Result {
    if condition {
        Ok(())
    } else {
        Err(Error::Check(message.into()))
    }
}

/// Fail with an infrastructure error.
///
/// # Errors
/// Always.
pub fn infrastructure<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Infrastructure(message.into()))
}

/// The lowercase hex SHA-256 digest of `bytes`, prefixed `sha256:`.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Whether `value` is a `sha256:` digest with 64 lowercase hex digits.
#[must_use]
pub fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// `count` bytes from the operating system's random source.
///
/// # Errors
/// Fails when `/dev/urandom` cannot be read.
pub fn random_bytes(count: usize) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = vec![0; count];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_prefixed_lowercase_sha256() {
        let value = digest(b"apsides");
        assert!(valid_digest(&value), "{value}");
        assert!(!valid_digest("sha256:ABC"));
        assert!(!valid_digest(&value.replace("sha256:", "sha512:")));
    }

    #[test]
    fn ensure_reports_the_failed_check() {
        assert!(ensure(true, "fine").is_ok());
        let outcome = ensure(false, "row differs");
        assert!(
            matches!(&outcome, Err(Error::Check(message)) if message == "row differs"),
            "{outcome:?}"
        );
    }

    #[test]
    fn random_bytes_have_the_requested_length() -> Result {
        let first = random_bytes(32)?;
        assert_eq!(first.len(), 32);
        assert_ne!(first, random_bytes(32)?);
        Ok(())
    }
}
