use sha2::{Digest, Sha256};
use thiserror::Error;

/// Prefix used in the stable bootstrap hash input.
pub const BOOTSTRAP_DOMAIN: &[u8] = b"oxideloop-bootstrap/v0\0";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdError {
    #[error("root UID must not be empty")]
    EmptyRootUid,
    #[error("identifier input must not be empty")]
    EmptyInput,
}

/// Stable bootstrap identity for a Kubernetes root UID.
///
/// # Errors
///
/// Returns an error when `root_uid` is empty.
pub fn bootstrap_id(root_uid: &str) -> Result<String, IdError> {
    if root_uid.is_empty() {
        return Err(IdError::EmptyRootUid);
    }
    Ok(hex_digest(&[BOOTSTRAP_DOMAIN, root_uid.as_bytes()]))
}

/// Stable, DNS-safe name for the one state journal associated with a root UID.
///
/// # Errors
///
/// Returns an error when `root_uid` is empty.
pub fn state_name(root_uid: &str) -> Result<String, IdError> {
    stable_name("ol-state-", &[b"oxideloop-state/v1\0", root_uid.as_bytes()])
}

/// Stable, DNS-safe name for one automation occurrence.
///
/// # Errors
///
/// Returns an error when either identifier is empty.
pub fn automation_run_name(root_uid: &str, occurrence_id: &str) -> Result<String, IdError> {
    if root_uid.is_empty() || occurrence_id.is_empty() {
        return Err(IdError::EmptyInput);
    }
    stable_name(
        "ol-run-",
        &[
            b"oxideloop-automation-run/v1\0",
            root_uid.as_bytes(),
            b"\0",
            occurrence_id.as_bytes(),
        ],
    )
}

fn stable_name(prefix: &str, parts: &[&[u8]]) -> Result<String, IdError> {
    if parts.iter().any(|part| part.is_empty()) {
        return Err(IdError::EmptyInput);
    }
    // The SHA-256 prefix leaves 216 or 224 bits respectively after the fixed
    // DNS label prefix, making collisions operationally negligible while never
    // exceeding Kubernetes' 63-character label maximum.
    let hex = hex_digest(parts);
    let suffix_len = 63_usize.saturating_sub(prefix.len());
    Ok(format!("{prefix}{}", &hex[..suffix_len]))
}

fn hex_digest(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_id_is_the_domain_separated_sha256() {
        assert_eq!(
            bootstrap_id("a-root-uid").unwrap(),
            "8fa887dc640b27ec8db9d307201d276805d03f1d5db10db4749cb2d8d5cd1f4e"
        );
    }

    #[test]
    fn generated_names_are_stable_dns_labels_and_distinct() {
        let state = state_name("a-root-uid").unwrap();
        let run = automation_run_name("a-root-uid", "source/42").unwrap();
        assert_eq!(state, state_name("a-root-uid").unwrap());
        assert_eq!(run, automation_run_name("a-root-uid", "source/42").unwrap());
        assert_ne!(state, run);
        for name in [state, run] {
            assert!(name.len() <= 63);
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            );
        }
    }
}
