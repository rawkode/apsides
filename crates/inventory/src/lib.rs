#![forbid(unsafe_code)]
//! Metadata-only pruning inventory. This store grants no mutation authority.
mod model;
pub use model::*;

#[cfg(feature = "sqlite-backend")]
mod sqlite;
#[cfg(feature = "sqlite-backend")]
pub use sqlite::SqliteInventory;

use async_trait::async_trait;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("inventory head changed; reload before publishing")]
    Conflict,
    #[error("invalid inventory metadata: {0}")]
    Invalid(&'static str),
    #[error("inventory backend operation failed: {0}")]
    Backend(&'static str),
}

/// Preserves immutable metadata snapshots and conditionally publishes a head.
#[async_trait]
pub trait InventoryStore: Send + Sync {
    /// Read a consistent head and its immutable snapshot.
    /// # Errors
    /// Returns an error on invalid scope, unavailable storage or corrupt metadata.
    async fn load(&self, scope: &InventoryScope) -> Result<Option<VersionedInventory>>;

    /// Read an earlier immutable snapshot by its metadata digest.
    /// # Errors
    /// Returns an error on invalid identifiers, storage failures or corrupt metadata.
    async fn read_snapshot(
        &self,
        scope: &InventoryScope,
        snapshot_id: &str,
    ) -> Result<Option<InventorySnapshot>>;

    /// Publish only if the opaque head revision matches. None means absent.
    /// Repeating the exact current snapshot is idempotent, including after a
    /// lost commit response. Previous snapshots remain available for pruning.
    /// # Errors
    /// Rejects stale revisions, invalid metadata, or a failed durable commit.
    async fn publish(
        &self,
        scope: &InventoryScope,
        expected_revision: Option<&str>,
        snapshot: &InventorySnapshot,
    ) -> Result<VersionedInventory>;
}
