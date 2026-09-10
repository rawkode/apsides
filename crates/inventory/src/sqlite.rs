//! Metadata-only inventory on a local durable SQLite database.
//!
//! The store is local by construction. Publication compares and sets the head
//! revision inside one immediate transaction, which assumes reads observe the
//! last commit — true of a file, not of a replica.

use crate::model::{snapshot_id, valid_digest};
use crate::{
    Error, InventoryScope, InventorySnapshot, InventoryStore, MAX_SNAPSHOT_BYTES, Result,
    VersionedInventory,
};
use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// How long a write waits for another writer before reporting contention.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS apsides_inventory_schema (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
INSERT OR IGNORE INTO apsides_inventory_schema VALUES (1,1);
CREATE TABLE IF NOT EXISTS apsides_inventory_snapshots (
 scope TEXT NOT NULL, digest TEXT NOT NULL, metadata BLOB NOT NULL,
 PRIMARY KEY(scope,digest));
CREATE TABLE IF NOT EXISTS apsides_inventory_heads (
 scope TEXT PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision>0), digest TEXT NOT NULL);
";

/// Default inventory backend, suited to a persistent volume.
///
/// SQLite is synchronous, so every operation runs on a blocking task. The one
/// connection is opened when the store is opened and guarded by a mutex, which
/// makes a store's operations serial. Separate stores are independent.
pub struct SqliteInventory {
    connection: Arc<Mutex<Connection>>,
    read_only: bool,
}

impl SqliteInventory {
    /// Open a local database on an application-owned persistent filesystem.
    ///
    /// Uses SQLite's rollback journal and FULL synchronization, not a custom WAL.
    ///
    /// # Errors
    /// Rejects relative paths, inaccessible databases or unsupported schemas.
    pub async fn open_local(path: &Path) -> Result<Self> {
        let store = Self::open(path, false, "open local database").await?;
        store.initialize().await?;
        Ok(store)
    }

    /// Open an existing local database without creating or mutating it.
    ///
    /// # Errors
    /// Rejects relative or missing paths and databases without the supported schema and columns.
    pub async fn open_local_read_only(path: &Path) -> Result<Self> {
        let store = Self::open(path, true, "open local read-only database").await?;
        store
            .with_connection(|connection| validate_existing(connection))
            .await?;
        Ok(store)
    }

    async fn open(path: &Path, read_only: bool, failure: &'static str) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Invalid("database path must be absolute"));
        }
        let owned = path.to_path_buf();
        let connection = blocking(move || {
            // Read-only never creates: an absent file is an error, not a new database.
            let flags = if read_only {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            } else {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            };
            let connection =
                Connection::open_with_flags(&owned, flags).map_err(|_| Error::Backend(failure))?;
            connection
                .busy_timeout(BUSY_TIMEOUT)
                .map_err(|_| Error::Backend("busy timeout"))?;
            Ok(connection)
        })
        .await?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            read_only,
        })
    }

    /// Run one unit of SQLite work on a blocking task, holding this store's connection.
    async fn with_connection<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let connection = Arc::clone(&self.connection);
        blocking(move || {
            let mut guard = connection
                .lock()
                .map_err(|_| Error::Backend("inventory connection poisoned"))?;
            work(&mut guard)
        })
        .await
    }

    async fn initialize(&self) -> Result<()> {
        self.with_connection(|connection| {
            connection
                .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
                .map_err(|_| Error::Backend("durability configuration"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| Error::Backend("begin schema initialization"))?;
            transaction
                .execute_batch(SCHEMA)
                .map_err(|_| Error::Backend("initialize schema"))?;
            validate_existing(&transaction)?;
            transaction
                .commit()
                .map_err(|_| Error::Backend("commit schema"))?;
            Ok(())
        })
        .await
    }
}

/// Run synchronous SQLite work without blocking the async runtime.
async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::Backend("inventory task"))?
}

fn validate_existing(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare("SELECT version FROM apsides_inventory_schema WHERE id=1")
        .map_err(|_| Error::Invalid("missing inventory schema"))?;
    let mut rows = statement
        .query([])
        .map_err(|_| Error::Invalid("missing inventory schema"))?;
    let row = rows
        .next()
        .map_err(|_| Error::Backend("read schema row"))?
        .ok_or(Error::Invalid("missing inventory schema"))?;
    if row
        .get::<usize, i64>(0)
        .map_err(|_| Error::Invalid("schema version"))?
        != 1
        || rows
            .next()
            .map_err(|_| Error::Backend("read schema row"))?
            .is_some()
    {
        return Err(Error::Invalid("unsupported inventory schema"));
    }
    drop(rows);
    drop(statement);
    connection
        .prepare("SELECT scope,digest,metadata FROM apsides_inventory_snapshots LIMIT 0")
        .map_err(|_| Error::Invalid("missing immutable inventory snapshot table"))?;
    connection
        .prepare("SELECT scope,revision,digest FROM apsides_inventory_heads LIMIT 0")
        .map_err(|_| Error::Invalid("missing inventory head table"))?;
    Ok(())
}

fn head(connection: &Connection, scope: &str) -> Result<Option<(i64, String)>> {
    let mut statement = connection
        .prepare("SELECT revision,digest FROM apsides_inventory_heads WHERE scope=?1")
        .map_err(|_| Error::Backend("read head"))?;
    let mut rows = statement
        .query([scope])
        .map_err(|_| Error::Backend("read head"))?;
    rows.next()
        .map_err(|_| Error::Backend("read head row"))?
        .map(|row| {
            let revision = row
                .get::<usize, i64>(0)
                .map_err(|_| Error::Invalid("head revision"))?;
            let digest = row
                .get::<usize, String>(1)
                .map_err(|_| Error::Invalid("head digest"))?;
            if revision <= 0 || !valid_digest(&digest) {
                return Err(Error::Invalid("head identity"));
            }
            Ok((revision, digest))
        })
        .transpose()
}

fn read_snapshot_bytes(connection: &Connection, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
    let limit = i64::try_from(MAX_SNAPSHOT_BYTES).map_err(|_| Error::Invalid("snapshot bound"))?;
    let mut statement = connection
        .prepare(
            "SELECT CASE WHEN length(metadata)<=?3 THEN metadata ELSE NULL END \
             FROM apsides_inventory_snapshots WHERE scope=?1 AND digest=?2",
        )
        .map_err(|_| Error::Backend("read snapshot"))?;
    let mut rows = statement
        .query(params![scope, id, limit])
        .map_err(|_| Error::Backend("read snapshot"))?;
    let Some(row) = rows
        .next()
        .map_err(|_| Error::Backend("read snapshot row"))?
    else {
        return Ok(None);
    };
    Ok(Some(
        row.get::<usize, Vec<u8>>(0)
            .map_err(|_| Error::Invalid("snapshot bytes"))?,
    ))
}

#[async_trait]
impl InventoryStore for SqliteInventory {
    async fn load(&self, scope: &InventoryScope) -> Result<Option<VersionedInventory>> {
        let key = scope.key()?;
        let lookup = key.clone();
        let Some((revision, id)) = self
            .with_connection(move |connection| head(connection, &lookup))
            .await?
        else {
            return Ok(None);
        };
        let snapshot = self
            .read_snapshot(scope, &id)
            .await?
            .ok_or(Error::Invalid("head snapshot is missing"))?;
        Ok(Some(VersionedInventory {
            revision: revision.to_string(),
            snapshot_id: id,
            snapshot,
        }))
    }

    async fn read_snapshot(
        &self,
        scope: &InventoryScope,
        id: &str,
    ) -> Result<Option<InventorySnapshot>> {
        let key = scope.key()?;
        if !valid_digest(id) {
            return Err(Error::Invalid("snapshot identifier"));
        }
        let wanted = id.to_owned();
        let Some(bytes) = self
            .with_connection(move |connection| read_snapshot_bytes(connection, &key, &wanted))
            .await?
        else {
            return Ok(None);
        };
        let snapshot: InventorySnapshot =
            serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("snapshot shape"))?;
        if snapshot.encode(scope)? != bytes || snapshot_id(scope, &snapshot)? != id {
            return Err(Error::Invalid("snapshot integrity"));
        }
        Ok(Some(snapshot))
    }

    async fn publish(
        &self,
        scope: &InventoryScope,
        expected: Option<&str>,
        snapshot: &InventorySnapshot,
    ) -> Result<VersionedInventory> {
        if self.read_only {
            return Err(Error::Invalid("read-only inventory store"));
        }
        let key = scope.key()?;
        let bytes = snapshot.encode(scope)?;
        let id = snapshot_id(scope, snapshot)?;
        let published = id.clone();
        let previous = expected.map(str::to_owned);
        let revision = self
            .with_connection(move |connection| {
                connection
                    .execute_batch("PRAGMA synchronous=FULL;")
                    .map_err(|_| Error::Backend("commit durability"))?;
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(|_| Error::Backend("begin publication"))?;
                let revision = publish_in_transaction(
                    &transaction,
                    &key,
                    previous.as_deref(),
                    &published,
                    &bytes,
                )?;
                transaction
                    .commit()
                    .map_err(|_| Error::Backend("commit publication; reload before retry"))?;
                Ok(revision)
            })
            .await?;
        Ok(VersionedInventory {
            revision: revision.to_string(),
            snapshot_id: id,
            snapshot: snapshot.clone(),
        })
    }
}

fn publish_in_transaction(
    connection: &Connection,
    scope: &str,
    expected: Option<&str>,
    id: &str,
    bytes: &[u8],
) -> Result<i64> {
    // Verify an existing immutable row even for an idempotent retry. A damaged
    // row must never be accepted or silently repaired by publication.
    let existing = read_snapshot_bytes(connection, scope, id).map_err(|error| match error {
        Error::Backend("read snapshot") => Error::Backend("verify immutable snapshot"),
        other => other,
    })?;
    let exists = existing.is_some();
    if let Some(row) = existing
        && row != bytes
    {
        return Err(Error::Invalid("immutable snapshot mismatch"));
    }
    let current = head(connection, scope)?;
    if let Some((revision, digest)) = &current
        && digest == id
    {
        if !exists {
            return Err(Error::Invalid("head snapshot is missing"));
        }
        return Ok(*revision);
    }
    let actual = current.as_ref().map(|(revision, _)| revision.to_string());
    if actual.as_deref() != expected {
        return Err(Error::Conflict);
    }
    let revision = current.map_or(Ok(1), |(revision, _)| {
        revision
            .checked_add(1)
            .ok_or(Error::Invalid("revision exhausted"))
    })?;
    connection.execute("INSERT OR IGNORE INTO apsides_inventory_snapshots(scope,digest,metadata) VALUES (?1,?2,?3)", params![scope, id, bytes])
        .map_err(|_| Error::Backend("write immutable snapshot"))?;
    connection.execute("INSERT INTO apsides_inventory_heads(scope,revision,digest) VALUES (?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision,digest=excluded.digest", params![scope, revision, id])
        .map_err(|_| Error::Backend("publish head"))?;
    Ok(revision)
}
