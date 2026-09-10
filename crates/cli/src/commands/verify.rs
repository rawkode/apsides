//! `aps verify` — check local artifact integrity and recompute its plan.
//!
//! This establishes neither approved provenance nor production readiness; see
//! `aps verify-release` for verification against a trust policy.

use crate::{Result, artifact, cli, report};

/// # Errors
/// Returns an error if the artifact is incomplete, or if any recorded digest
/// disagrees with the file it names.
pub fn command(arguments: &cli::Verify) -> Result<()> {
    report::emit(&artifact::verify(&arguments.artifact)?)
}
