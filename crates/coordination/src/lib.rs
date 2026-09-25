#![forbid(unsafe_code)]
//! Immutable coordination metadata, without a mutable authority pointer.
//!
//! A snapshot asserts a complete registry view; this library cannot prove that
//! completeness or grant authority. The protected registry binds its exact
//! digest before lifecycle consumers use it. Hydration belongs above `deploy`, which
//! continues to accept `ProtectedClaim` values and never depends on this crate.
mod model;
pub use model::*;
mod audit;
pub use audit::*;
mod review;
pub use review::*;

#[cfg(feature = "sqlite-backend")]
mod sqlite;
#[cfg(feature = "sqlite-backend")]
pub use sqlite::SqliteCoordination;

use async_trait::async_trait;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid coordination metadata: {0}")]
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

/// A typed immutable transition-audit store. There is no mutable head, update,
/// delete or arbitrary payload API.
#[async_trait]
pub trait AuditStore: Send + Sync {
    /// Store a validated audit immutably and return its scope-bound reference.
    /// # Errors
    /// Rejects malformed metadata, conflicting/corrupt existing rows, and storage failures.
    async fn put_audit(&self, audit: &TransitionAuditV1) -> Result<AuditRef>;

    /// Read and verify exactly the referenced immutable audit.
    /// # Errors
    /// Returns errors for invalid references, corrupt data and backend failures;
    /// `None` means only that the referenced transition scope has no row.
    async fn read_audit(&self, reference: &AuditRef) -> Result<Option<TransitionAuditV1>>;
}

/// Immutable typed declaration metadata. It has no mutable head or authority.
#[async_trait]
pub trait ReviewStore: Send + Sync {
    /// Publish an immutable body addressed by its scope and metadata digest.
    /// # Errors
    /// Rejects invalid metadata, corrupt exact-reference rows and backend failures.
    async fn put_review(&self, metadata: &ReviewMetadata) -> Result<ReviewRef>;

    /// Read the exact validated scope-bound metadata. Only absent rows return `None`.
    /// # Errors
    /// Rejects invalid references, substituted or corrupt data and backend failures.
    async fn read_review(&self, reference: &ReviewRef) -> Result<Option<ReviewMetadata>>;
}
