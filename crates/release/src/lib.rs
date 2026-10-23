mod compiler;
mod fetch;
mod fixture;
pub use fetch::{FetchedRelease, OrasTool, ReleaseReference, fetch_release};
pub use fixture::validate_fixture_projection;
mod manifest;
mod oci;
mod provenance;

pub use compiler::{
    ApprovedRuntime, AuthoringCompiler, BuilderImage, COMPILER_CLOSURE_SCHEMA, CompilerClosure,
    CompilerFile, HostTool, NativeCompiler, RustyV8, RustyV8Component, validate_approved_runtime,
    validate_compiler_closure,
};
pub use manifest::{
    IntegrityVerifiedCandidate, ReleaseManifest, ReleaseMode, read_release,
    validate_controller_binary,
};
pub use oci::{ControllerImage, controller_runtime_config, verify_controller_image};
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
