use crate::model::{snapshot_id, valid_digest};
use crate::{
    Error, InventoryScope, InventorySnapshot, InventoryStore, MAX_SNAPSHOT_BYTES, Result,
    VersionedInventory,
};
use async_trait::async_trait;
use libsql::{Builder, Connection, Database, TransactionBehavior, params};
use std::{path::Path, time::Duration};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reakt_inventory_schema (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
INSERT OR IGNORE INTO reakt_inventory_schema VALUES (1,1);
CREATE TABLE IF NOT EXISTS reakt_inventory_snapshots (
 scope TEXT NOT NULL, digest TEXT NOT NULL, metadata BLOB NOT NULL,
 PRIMARY KEY(scope,digest));
CREATE TABLE IF NOT EXISTS reakt_inventory_heads (
 scope TEXT PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision>0), digest TEXT NOT NULL);
";

/// Default inventory backend. Local files suit a PVC; the remote constructor
/// uses Turso's supported libSQL client. Credentials are never inventory data.
pub struct LibsqlInventory {
    database: Database,
    local: bool,
    read_only: bool,
}

impl LibsqlInventory {
    /// Open a local database on an application-owned persistent filesystem.
    /// Uses SQLite's rollback journal and FULL synchronization, not a custom WAL.
    /// # Errors
    /// Rejects relative paths, inaccessible databases or unsupported schemas.
    pub async fn open_local(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Invalid("database path must be absolute"));
        }
        let database = Builder::new_local(path)
            .build()
            .await
            .map_err(|_| Error::Backend("open local database"))?;
        let store = Self {
            database,
            local: true,
            read_only: false,
        };
        store.initialize().await?;
        Ok(store)
    }

    /// Connect to a remote libSQL/Turso primary, without an eventually consistent replica.
    /// # Errors
    /// Rejects insecure endpoints, connection failures or unsupported schemas.
    pub async fn open_remote(url: String, token: String) -> Result<Self> {
        validate_remote_configuration(&url, &token)?;
        let database = Builder::new_remote(url, token)
            .build()
            .await
            .map_err(|_| Error::Backend("connect remote database"))?;
        let store = Self {
            database,
            local: false,
            read_only: false,
        };
        store.initialize().await?;
        Ok(store)
    }

    /// Open an existing local database without creating or mutating it.
    /// # Errors
    /// Rejects relative or missing paths and databases without the supported schema and columns.
    pub async fn open_local_read_only(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Invalid("database path must be absolute"));
        }
        let database = Builder::new_local(path)
            .flags(libsql::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .build()
            .await
            .map_err(|_| Error::Backend("open local read-only database"))?;
        let store = Self {
            database,
            local: true,
            read_only: true,
        };
        validate_existing(&store.connection()?).await?;
        Ok(store)
    }

    fn connection(&self) -> Result<Connection> {
        let connection = self
            .database
            .connect()
            .map_err(|_| Error::Backend("connect"))?;
        if self.local {
            connection
                .busy_timeout(Duration::from_secs(5))
                .map_err(|_| Error::Backend("busy timeout"))?;
        }
        Ok(connection)
    }

    async fn initialize(&self) -> Result<()> {
        let connection = self.connection()?;
        if self.local {
            connection
                .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
                .await
                .map_err(|_| Error::Backend("durability configuration"))?;
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(|_| Error::Backend("begin schema initialization"))?;
        let result = async {
            transaction
                .execute_batch(SCHEMA)
                .await
                .map_err(|_| Error::Backend("initialize schema"))?;
            validate_existing(&transaction).await
        }
        .await;
        if let Err(error) = result {
            let _ = transaction.rollback().await;
            return Err(error);
        }
        transaction
            .commit()
            .await
            .map_err(|_| Error::Backend("commit schema"))?;
        Ok(())
    }
}

fn validate_remote_configuration(url: &str, token: &str) -> Result<()> {
    if url.len() > 2048 || url.chars().any(char::is_control) {
        return Err(Error::Invalid("remote endpoint"));
    }
    let endpoint = url::Url::parse(url).map_err(|_| Error::Invalid("remote endpoint"))?;
    if !matches!(endpoint.scheme(), "https" | "libsql")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !matches!(endpoint.path(), "" | "/")
    {
        return Err(Error::Invalid(
            "remote endpoint must use TLS without embedded credentials, query or path",
        ));
    }
    if token.is_empty() || token.len() > 8192 || token.chars().any(char::is_control) {
        return Err(Error::Invalid("remote authentication token"));
    }
    Ok(())
}

async fn validate_existing(connection: &Connection) -> Result<()> {
    let mut rows = connection
        .query("SELECT version FROM reakt_inventory_schema WHERE id=1", ())
        .await
        .map_err(|_| Error::Invalid("missing inventory schema"))?;
    let row = rows
        .next()
        .await
        .map_err(|_| Error::Backend("read schema row"))?
        .ok_or(Error::Invalid("missing inventory schema"))?;
    if row
        .get::<i64>(0)
        .map_err(|_| Error::Invalid("schema version"))?
        != 1
        || rows
            .next()
            .await
            .map_err(|_| Error::Backend("read schema row"))?
            .is_some()
    {
        return Err(Error::Invalid("unsupported inventory schema"));
    }
    connection
        .query(
            "SELECT scope,digest,metadata FROM reakt_inventory_snapshots LIMIT 0",
            (),
        )
        .await
        .map_err(|_| Error::Invalid("missing immutable inventory snapshot table"))?;
    connection
        .query(
            "SELECT scope,revision,digest FROM reakt_inventory_heads LIMIT 0",
            (),
        )
        .await
        .map_err(|_| Error::Invalid("missing inventory head table"))?;
    Ok(())
}

async fn head(connection: &Connection, scope: &str) -> Result<Option<(i64, String)>> {
    let mut rows = connection
        .query(
            "SELECT revision,digest FROM reakt_inventory_heads WHERE scope=?1",
            [scope],
        )
        .await
        .map_err(|_| Error::Backend("read head"))?;
    rows.next()
        .await
        .map_err(|_| Error::Backend("read head row"))?
        .map(|row| {
            let revision = row
                .get::<i64>(0)
                .map_err(|_| Error::Invalid("head revision"))?;
            let digest = row
                .get::<String>(1)
                .map_err(|_| Error::Invalid("head digest"))?;
            if revision <= 0 || !valid_digest(&digest) {
                return Err(Error::Invalid("head identity"));
            }
            Ok((revision, digest))
        })
        .transpose()
}

#[async_trait]
impl InventoryStore for LibsqlInventory {
    async fn load(&self, scope: &InventoryScope) -> Result<Option<VersionedInventory>> {
        let key = scope.key()?;
        let Some((revision, id)) = head(&self.connection()?, &key).await? else {
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
        let mut rows = self.connection()?.query(
            "SELECT CASE WHEN length(metadata)<=?3 THEN metadata ELSE NULL END FROM reakt_inventory_snapshots WHERE scope=?1 AND digest=?2",
            params![key, id, i64::try_from(MAX_SNAPSHOT_BYTES).map_err(|_| Error::Invalid("snapshot bound"))?],
        ).await.map_err(|_| Error::Backend("read snapshot"))?;
        let Some(row) = rows
            .next()
            .await
            .map_err(|_| Error::Backend("read snapshot row"))?
        else {
            return Ok(None);
        };
        let bytes = row
            .get::<Vec<u8>>(0)
            .map_err(|_| Error::Invalid("snapshot bytes"))?;
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
        let connection = self.connection()?;
        if self.local {
            connection
                .execute_batch("PRAGMA synchronous=FULL;")
                .await
                .map_err(|_| Error::Backend("commit durability"))?;
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(|_| Error::Backend("begin publication"))?;
        let result = publish_in_transaction(&transaction, &key, expected, &id, bytes).await;
        match result {
            Ok(revision) => {
                transaction
                    .commit()
                    .await
                    .map_err(|_| Error::Backend("commit publication; reload before retry"))?;
                Ok(VersionedInventory {
                    revision: revision.to_string(),
                    snapshot_id: id,
                    snapshot: snapshot.clone(),
                })
            }
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }
}

async fn publish_in_transaction(
    connection: &Connection,
    scope: &str,
    expected: Option<&str>,
    id: &str,
    bytes: Vec<u8>,
) -> Result<i64> {
    // Verify an existing immutable row even for an idempotent retry. A damaged
    // row must never be accepted or silently repaired by publication.
    let mut rows = connection
        .query(
            "SELECT CASE WHEN length(metadata)<=?3 THEN metadata ELSE NULL END FROM reakt_inventory_snapshots WHERE scope=?1 AND digest=?2",
            params![scope, id, i64::try_from(MAX_SNAPSHOT_BYTES).map_err(|_| Error::Invalid("snapshot bound"))?],
        )
        .await
        .map_err(|_| Error::Backend("verify immutable snapshot"))?;
    let existing = rows
        .next()
        .await
        .map_err(|_| Error::Backend("verify snapshot row"))?;
    let exists = existing.is_some();
    if let Some(row) = existing
        && row
            .get::<Vec<u8>>(0)
            .map_err(|_| Error::Invalid("snapshot bytes"))?
            != bytes
    {
        return Err(Error::Invalid("immutable snapshot mismatch"));
    }
    drop(rows);
    let current = head(connection, scope).await?;
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
    connection.execute("INSERT OR IGNORE INTO reakt_inventory_snapshots(scope,digest,metadata) VALUES (?1,?2,?3)", params![scope, id, bytes]).await
        .map_err(|_| Error::Backend("write immutable snapshot"))?;
    connection.execute("INSERT INTO reakt_inventory_heads(scope,revision,digest) VALUES (?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision,digest=excluded.digest", params![scope, revision, id]).await
        .map_err(|_| Error::Backend("publish head"))?;
    Ok(revision)
}
