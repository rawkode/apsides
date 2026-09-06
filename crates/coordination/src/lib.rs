#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
#![forbid(unsafe_code)]
//! Immutable claim metadata, without a mutable authority pointer.
//!
//! A snapshot asserts a complete registry view; this library cannot prove that
//! completeness or grant authority. A future protected registry pointer must bind
//! its digest before consumers use it. Hydration belongs above `deploy`, which
//! continues to accept `ProtectedClaim` values and never depends on this crate.
mod model;
pub use model::*;

#[cfg(feature = "libsql-backend")]
mod libsql_backend;
#[cfg(feature = "libsql-backend")]
pub use libsql_backend::LibsqlCoordination;

use async_trait::async_trait;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid claim snapshot: {0}")]
    Invalid(&'static str),
    #[error("coordination backend operation failed: {0}")]
    Backend(&'static str),
}

/// A typed immutable snapshot store. There is no head, update, delete or arbitrary payload API.
#[async_trait]
pub trait ClaimStore: Send + Sync {
    /// Store validated claims immutably and return their scope-bound digest reference.
    /// # Errors
    /// Rejects malformed metadata, conflicting/corrupt existing rows, and storage failures.
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> Result<ClaimRef>;

    /// Read and verify exactly the referenced immutable claims.
    /// # Errors
    /// Returns errors for invalid references, corrupt data and backend failures;
    /// `None` means only that the referenced digest has no row.
    async fn read_claims(&self, reference: &ClaimRef) -> Result<Option<ClaimSnapshotV1>>;
}
