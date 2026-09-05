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
mod json;
mod model;
mod review;
mod rollout;
mod schema;
mod semantic;
mod test_plan;
mod validate;

pub use json::{canonical_bytes, digest, parse_strict};
pub use model::*;
pub use review::review_documents;
pub use rollout::*;
pub use semantic::*;
pub use validate::{validate, validate_ceiling};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid Reakt declaration: {0}")]
    Invalid(String),
    #[error("platform permission ceiling rejected plan: {0}")]
    Ceiling(String),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Parse strict JSON and derive a trusted plan from the accepted IR vocabulary.
///
/// # Errors
/// Rejects malformed JSON, unknown fields, and unsupported IR contracts.
pub fn parse_and_validate(input: &str) -> Result<CompiledPlan> {
    let ir = serde_json::from_value::<Ir>(parse_strict(input)?)
        .map_err(|error| Error::Invalid(format!("IR schema mismatch: {error}")))?;
    validate(&ir)
}
