//! Bind a deploy to the exact reviewed surface.
//!
//! `aps deploy` independently recomputes the deployment surface from the
//! verified release's own IR (never trusting `surface.json` bytes alone) and
//! requires the result to equal both the release's recorded `surface.json`
//! digest and the digest the reviewed, approved metadata carries. Either
//! mismatch fails closed with no override flag: the operator approving a
//! deploy sees exactly the egress hosts, handlers, outputs, secrets and
//! permissions of the release actually being deployed.

use crate::Result;
use std::fmt;

/// A deploy-time surface binding failure, naming the digests that disagree.
#[derive(Debug)]
pub(super) enum SurfaceBindingError {
    /// The surface recomputed from the verified release's own IR does not
    /// match the digest `release.json` recorded for `surface.json`.
    ///
    /// `release::read_release` already checks `surface.json`'s content
    /// against a fresh derivation and its bytes against the recorded
    /// digest, so this should never trigger in practice; it exists as an
    /// explicit, independent deploy-level assertion rather than an implicit
    /// consequence of release parsing.
    ReleaseMismatch {
        recomputed: String,
        recorded: String,
    },
    /// The surface recomputed from the verified release's own IR does not
    /// match the `surfaceDigest` the reviewed, approved metadata carries:
    /// the release being deployed is not the release that was reviewed.
    ReviewMismatch {
        recomputed: String,
        reviewed: String,
    },
}
impl fmt::Display for SurfaceBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReleaseMismatch {
                recomputed,
                recorded,
            } => write!(
                f,
                "deployment surface mismatch: recomputed surface digest {recomputed} does not match the release's recorded surface.json digest {recorded}"
            ),
            Self::ReviewMismatch {
                recomputed,
                reviewed,
            } => write!(
                f,
                "deployment surface mismatch: recomputed surface digest {recomputed} does not match the reviewed surfaceDigest {reviewed}; the release being deployed is not the release that was reviewed"
            ),
        }
    }
}
impl std::error::Error for SurfaceBindingError {}

/// Independently recompute the deployment surface from `verified`'s own IR
/// and require it to equal both the release's recorded `surface.json`
/// digest and `reviewed`'s `surfaceDigest`. No override flag.
/// # Errors
/// Returns [`SurfaceBindingError`] naming the disagreeing digests.
pub(super) fn verify_surface_binding(
    verified: &release::IntegrityVerifiedCandidate,
    reviewed: &coordination::ReviewMetadata,
) -> Result<()> {
    let surface = compiler_core::derive_surface(verified.compiled());
    let recomputed = compiler_core::digest(&surface)?;
    let recorded = verified
        .manifest()
        .files
        .get("surface.json")
        .ok_or("release is missing its surface.json payload digest")?;
    check(&recomputed, recorded, &reviewed.surface_digest)
}

/// The pure comparison at the heart of [`verify_surface_binding`], isolated
/// so its two failure modes are directly unit-testable without a signed
/// release fixture: `release::read_release` (which every real `recorded`
/// digest already passed through) independently guarantees `recomputed ==
/// recorded` for any real [`release::IntegrityVerifiedCandidate`] — see
/// `crates/release/tests/signatures.rs`'s
/// `tampered_surface_document_is_rejected_even_when_resigned`, which proves
/// that guarantee holds even across a re-sign — so `ReleaseMismatch` here is
/// a defense-in-depth assertion, while `ReviewMismatch` is the new check
/// this phase adds: the release actually being deployed may still disagree
/// with what was reviewed.
fn check(recomputed: &str, recorded: &str, reviewed: &str) -> Result<()> {
    if recomputed != recorded {
        return Err(Box::new(SurfaceBindingError::ReleaseMismatch {
            recomputed: recomputed.into(),
            recorded: recorded.into(),
        }));
    }
    if recomputed != reviewed {
        return Err(Box::new(SurfaceBindingError::ReviewMismatch {
            recomputed: recomputed.into(),
            reviewed: reviewed.into(),
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_digests_pass() -> Result<()> {
        check("sha256:aaaa", "sha256:aaaa", "sha256:aaaa")
    }

    #[test]
    fn release_mismatch_names_the_recomputed_and_recorded_digests() -> Result<()> {
        let error = check("sha256:aaaa", "sha256:bbbb", "sha256:aaaa")
            .err()
            .ok_or("recomputed-vs-recorded mismatch must be rejected")?;
        let message = error.to_string();
        assert!(message.contains("sha256:aaaa"), "{message}");
        assert!(message.contains("sha256:bbbb"), "{message}");
        assert!(
            error
                .downcast_ref::<SurfaceBindingError>()
                .is_some_and(|error| matches!(error, SurfaceBindingError::ReleaseMismatch { .. }))
        );
        Ok(())
    }

    #[test]
    fn review_mismatch_names_the_recomputed_and_reviewed_digests() -> Result<()> {
        // The release itself is internally consistent (recomputed ==
        // recorded); only the reviewed metadata's surfaceDigest has
        // drifted, e.g. because the release being deployed is not the one
        // that was reviewed.
        let error = check("sha256:aaaa", "sha256:aaaa", "sha256:cccc")
            .err()
            .ok_or("recomputed-vs-reviewed mismatch must be rejected")?;
        let message = error.to_string();
        assert!(message.contains("sha256:aaaa"), "{message}");
        assert!(message.contains("sha256:cccc"), "{message}");
        assert!(
            error
                .downcast_ref::<SurfaceBindingError>()
                .is_some_and(|error| matches!(error, SurfaceBindingError::ReviewMismatch { .. }))
        );
        Ok(())
    }
}
