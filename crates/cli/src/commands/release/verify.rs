//! `aps verify-release` — verify a release against a supplied trust policy.
//!
//! The result is scoped to the policy the caller supplied; it is not a claim
//! that the release is approved for production.

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
    let verified = runtime.block_on(release::verify_release(&arguments.artifact, &policy))?;
    report::emit(&json!({
        "status": "verified-against-supplied-policy",
        "releaseDigest": verified.release_digest(),
        "irDigest": verified.compiled().ir_digest,
        "provenanceDigest": verified.provenance_digest(),
        "trustPolicyDigest": verified.trust_policy_digest(),
    }))
}
