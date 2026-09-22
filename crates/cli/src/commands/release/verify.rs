//! `aps verify-release` — run the release checks `aps deploy` runs, offline.
//!
//! Integrity and provenance are checked against the trust policy the caller
//! supplied. No cluster is contacted, so platform admission is not checked.

use crate::{Result, cli, report};
use serde_json::json;
use std::fs;

/// # Errors
/// Returns an error if the trust policy is unreadable or invalid, or if the
/// release does not verify against it.
pub fn command(arguments: &cli::VerifyRelease) -> Result<()> {
    let policy: release::TrustPolicy = serde_json::from_value(compiler_core::parse_strict(
        &fs::read_to_string(&arguments.trust_policy)?,
    )?)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let verified = runtime.block_on(release::verify_candidate(&arguments.artifact, &policy))?;
    report::emit(&json!({
        "status": "verified-against-supplied-policy",
        "releaseDigest": verified.release_digest(),
        "irDigest": verified.compiled().ir_digest,
        "provenanceDigest": verified.provenance_digest(),
        "trustPolicyDigest": verified.trust_policy_digest(),
    }))
}
