mod compiler;
mod fetch;
mod fixture;
pub use fetch::{FetchedRelease, OrasTool, ReleaseReference, fetch_release};
pub use fixture::validate_fixture_projection;
mod manifest;
mod oci;
mod provenance;

pub use compiler::validate_compiler_closure;
pub use manifest::{IntegrityVerifiedCandidate, ReleaseManifest, ReleaseMode, read_release};
pub use provenance::{SignaturePolicy, TrustPolicy, VerifierTool, verify_candidate};

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
