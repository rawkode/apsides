//! Immutable coordination metadata on a local durable SQLite database.
//!
//! The store is local by construction. There is no remote client and no
//! replica: every read is a read of the same file the last commit wrote, which
//! is what the compare-and-set publication protocol above assumes.

use crate::{
    AuditRef, AuditStore, ClaimRef, ClaimSnapshotV1, ClaimStore, Error, MAX_CLAIM_SNAPSHOT_BYTES,
    MAX_TRANSITION_AUDIT_BYTES, Result, TransitionAuditV1,
};
use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

mod review;

/// How long a write waits for another writer before reporting contention.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS apsides_coordination_schema (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
INSERT OR IGNORE INTO apsides_coordination_schema VALUES (1,1);
CREATE TABLE IF NOT EXISTS apsides_coordination_claims (
 digest TEXT PRIMARY KEY NOT NULL, metadata BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS apsides_coordination_audits (
 registry_uid TEXT NOT NULL,
 cluster_binding TEXT NOT NULL,
 control_namespace TEXT NOT NULL,
 workload_namespace TEXT NOT NULL,
 domain_id TEXT NOT NULL,
 transition_id TEXT NOT NULL,
 sequence TEXT NOT NULL,
 digest TEXT NOT NULL,
 metadata BLOB NOT NULL,
 PRIMARY KEY (
  registry_uid, cluster_binding, control_namespace, workload_namespace,
  domain_id, transition_id));
";

/// Immutable claim, review and audit records on one durable database.
///
/// SQLite is synchronous, so every operation runs on a blocking task. The one
/// connection is opened when the store is opened and guarded by a mutex, which
/// makes a store's operations serial: two transactions cannot overlap on it.
/// Separate stores are independent.
pub struct SqliteCoordination {
    connection: Arc<Mutex<Connection>>,
    read_only: bool,
}

impl SqliteCoordination {
    /// Open an application-owned absolute filesystem path with FULL synchronization.
    ///
    /// # Errors
    /// Rejects relative paths, unavailable storage and unsupported database schemas.
    pub async fn open_local(path: &Path) -> Result<Self> {
        let store = Self::open(path, false, "open local database").await?;
        store.initialize().await?;
        Ok(store)
    }

    /// Open an existing local database without creating or mutating it.
    ///
    /// # Errors
    /// Rejects relative or missing paths and databases without the exact supported schema.
    pub async fn open_local_read_only(path: &Path) -> Result<Self> {
        let store = Self::open(path, true, "open local read-only database").await?;
        store.validate_existing().await?;
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
                .map_err(|_| Error::Backend("coordination connection poisoned"))?;
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
            transaction
                .execute_batch(review::SCHEMA)
                .map_err(|_| Error::Backend("initialize review schema"))?;
            review::validate_schema(&transaction)?;
            require_schema_version(&transaction)?;
            transaction
                .commit()
                .map_err(|_| Error::Backend("commit schema"))?;
            Ok(())
        })
        .await
    }

    async fn validate_existing(&self) -> Result<()> {
        self.with_connection(|connection| {
            require_schema_version(connection)?;
            connection
                .prepare("SELECT digest FROM apsides_coordination_claims LIMIT 0")
                .map_err(|_| Error::Invalid("missing immutable claim table"))?;
            connection
                .prepare("SELECT digest FROM apsides_coordination_audits LIMIT 0")
                .map_err(|_| Error::Invalid("missing immutable audit table"))?;
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
        .map_err(|_| Error::Backend("coordination task"))?
}

fn require_schema_version(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare("SELECT version FROM apsides_coordination_schema WHERE id=1")
        .map_err(|_| Error::Invalid("missing coordination schema"))?;
    let mut rows = statement
        .query([])
        .map_err(|_| Error::Invalid("missing coordination schema"))?;
    let row = rows
        .next()
        .map_err(|_| Error::Backend("read schema row"))?
        .ok_or(Error::Invalid("missing coordination schema"))?;
    if row
        .get::<usize, i64>(0)
        .map_err(|_| Error::Invalid("schema version"))?
        != 1
        || rows
            .next()
            .map_err(|_| Error::Backend("read schema row"))?
            .is_some()
    {
        return Err(Error::Invalid("unsupported coordination schema"));
    }
    Ok(())
}

#[async_trait]
impl ClaimStore for SqliteCoordination {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> Result<ClaimRef> {
        if self.read_only {
            return Err(Error::Invalid("read-only coordination store"));
        }
        let bytes = snapshot.encode()?;
        let reference = snapshot.reference()?;
        let digest = reference.digest.clone();
        let written = bytes.clone();
        self.with_connection(move |connection| {
            connection
                .execute_batch("PRAGMA synchronous=FULL;")
                .map_err(|_| Error::Backend("commit durability"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| Error::Backend("begin immutable publication"))?;
            if let Some(existing) = read_claim_bytes(&transaction, &digest)?
                && existing != written
            {
                return Err(Error::Invalid("immutable snapshot mismatch"));
            }
            // Ignoring a concurrent insert never changes an existing immutable
            // row. The required post-commit read verifies what actually won.
            transaction
                .execute(
                    "INSERT OR IGNORE INTO apsides_coordination_claims(digest,metadata) VALUES (?1,?2)",
                    params![digest, written],
                )
                .map_err(|_| Error::Backend("write immutable claims"))?;
            transaction
                .commit()
                .map_err(|_| Error::Backend("commit claims; retry exact snapshot"))?;
            Ok(())
        })
        .await?;
        let actual = self
            .read_claims(&reference)
            .await?
            .ok_or(Error::Invalid("committed claim snapshot missing"))?;
        if actual != *snapshot {
            return Err(Error::Invalid("committed claim snapshot mismatch"));
        }
        Ok(reference)
    }

    async fn read_claims(&self, reference: &ClaimRef) -> Result<Option<ClaimSnapshotV1>> {
        reference.validate()?;
        let digest = reference.digest.clone();
        self.with_connection(move |connection| read_claim_bytes(connection, &digest))
            .await?
            .map(|bytes| ClaimSnapshotV1::decode(&bytes, reference))
            .transpose()
    }
}

#[async_trait]
impl AuditStore for SqliteCoordination {
    async fn put_audit(&self, audit: &TransitionAuditV1) -> Result<AuditRef> {
        if self.read_only {
            return Err(Error::Invalid("read-only coordination store"));
        }
        let bytes = audit.encode()?;
        let reference = audit.reference()?;
        let target = reference.clone();
        let written = bytes.clone();
        self.with_connection(move |connection| {
            connection
                .execute_batch("PRAGMA synchronous=FULL;")
                .map_err(|_| Error::Backend("commit durability"))?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| Error::Backend("begin immutable audit publication"))?;
            if let Some((existing_digest, existing)) = read_audit_bytes(&transaction, &target)?
                && (existing_digest != target.digest || existing != written)
            {
                return Err(Error::Invalid("immutable audit mismatch"));
            }
            let scope = &target.scope;
            let claims = &scope.claim_scope;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO apsides_coordination_audits(\
                     registry_uid,cluster_binding,control_namespace,workload_namespace,\
                     domain_id,transition_id,sequence,digest,metadata) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        claims.registry_uid,
                        claims.cluster_binding,
                        claims.control_namespace,
                        claims.workload_namespace,
                        scope.domain_id,
                        scope.transition_id,
                        scope.sequence.to_string(),
                        target.digest,
                        written,
                    ],
                )
                .map_err(|_| Error::Backend("write immutable audit"))?;
            transaction
                .commit()
                .map_err(|_| Error::Backend("commit audit; retry exact record"))?;
            Ok(())
        })
        .await?;
        let actual = self
            .read_audit(&reference)
            .await?
            .ok_or(Error::Invalid("committed audit missing"))?;
        if actual != *audit {
            return Err(Error::Invalid("committed audit mismatch"));
        }
        Ok(reference)
    }

    async fn read_audit(&self, reference: &AuditRef) -> Result<Option<TransitionAuditV1>> {
        reference.validate()?;
        let target = reference.clone();
        self.with_connection(move |connection| read_audit_bytes(connection, &target))
            .await?
            .map(|(digest, bytes)| {
                if digest != reference.digest {
                    return Err(Error::Invalid("audit reference mismatch"));
                }
                TransitionAuditV1::decode(&bytes, reference)
            })
            .transpose()
    }
}

fn read_claim_bytes(connection: &Connection, digest: &str) -> Result<Option<Vec<u8>>> {
    let limit =
        i64::try_from(MAX_CLAIM_SNAPSHOT_BYTES).map_err(|_| Error::Invalid("snapshot bound"))?;
    let mut statement = connection.prepare(
        "SELECT CASE WHEN typeof(metadata)='blob' AND length(metadata)<=?2 THEN metadata ELSE NULL END FROM apsides_coordination_claims WHERE digest=?1",
    ).map_err(|_| Error::Backend("read immutable claims"))?;
    let mut rows = statement
        .query(params![digest, limit])
        .map_err(|_| Error::Backend("read immutable claims"))?;
    let Some(row) = rows.next().map_err(|_| Error::Backend("read claim row"))? else {
        return Ok(None);
    };
    let bytes = row
        .get::<usize, Vec<u8>>(0)
        .map_err(|_| Error::Invalid("snapshot bytes or size"))?;
    if rows
        .next()
        .map_err(|_| Error::Backend("read claim row"))?
        .is_some()
    {
        return Err(Error::Invalid("duplicate immutable claim rows"));
    }
    Ok(Some(bytes))
}

fn read_audit_bytes(
    connection: &Connection,
    reference: &AuditRef,
) -> Result<Option<(String, Vec<u8>)>> {
    let limit =
        i64::try_from(MAX_TRANSITION_AUDIT_BYTES).map_err(|_| Error::Invalid("audit bound"))?;
    let scope = &reference.scope;
    let claims = &scope.claim_scope;
    let mut statement = connection
        .prepare(
            "SELECT sequence,digest,\
             CASE WHEN typeof(metadata)='blob' AND length(metadata)<=?7 THEN metadata ELSE NULL END \
             FROM apsides_coordination_audits \
             WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
             AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6",
        )
        .map_err(|_| Error::Backend("read immutable audit"))?;
    let mut rows = statement
        .query(params![
            claims.registry_uid,
            claims.cluster_binding,
            claims.control_namespace,
            claims.workload_namespace,
            scope.domain_id,
            scope.transition_id,
            limit,
        ])
        .map_err(|_| Error::Backend("read immutable audit"))?;
    let Some(row) = rows.next().map_err(|_| Error::Backend("read audit row"))? else {
        return Ok(None);
    };
    let sequence = row
        .get::<usize, String>(0)
        .map_err(|_| Error::Invalid("audit sequence"))?;
    let digest = row
        .get::<usize, String>(1)
        .map_err(|_| Error::Invalid("audit digest"))?;
    let bytes = row
        .get::<usize, Vec<u8>>(2)
        .map_err(|_| Error::Invalid("audit bytes or size"))?;
    if sequence != scope.sequence.to_string()
        || rows
            .next()
            .map_err(|_| Error::Backend("read audit row"))?
            .is_some()
    {
        return Err(Error::Invalid("audit row identity"));
    }
    Ok(Some((digest, bytes)))
}
