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
mod assertion_contract;
mod compiler;
mod evidence;
mod fetch;
mod fixture;
pub use fetch::{
    FetchedRelease, OrasTool, ReleaseReference, ResolvedRelease, fetch_release, resolve_release,
};
pub use fixture::validate_fixture_projection;
mod manifest;
mod merge_evidence;
pub use merge_evidence::{MergeSummary, merge_evidence};
mod oci;
mod production_evidence;
#[cfg(test)]
mod production_evidence_live;
mod provenance;
#[cfg(test)]
mod reactor_evidence_live;
mod runner_evidence;

pub use assertion_contract::validate_assertions;
pub use compiler::validate_compiler_closure;
pub use evidence::{CaseResult, CoverageReport, EvidenceResult, REQUIRED_CHECKS};
pub use manifest::{
    IntegrityVerifiedCandidate, ReleaseManifest, ReleaseMode, VerifiedRelease, read_release,
};
#[cfg(feature = "test-support")]
pub use manifest::{TestStagingAuthorization, TestStagingScope};
pub use production_evidence::ProductionEvidence;
pub use provenance::{
    SignaturePolicy, TrustPolicy, VerifierTool, verify_candidate, verify_release,
};
pub use runner_evidence::{Assertion, EvidenceRecord, ReactorEvidence, RunnerIdentity};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("release rejected: {0}")]
    Rejected(String),
    #[error("release I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("release JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("release contract: {0}")]
    Core(#[from] compiler_core::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

fn reject<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Rejected(message.into()))
}
