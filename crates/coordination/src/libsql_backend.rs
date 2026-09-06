use crate::{
    AuditRef, AuditStore, ClaimRef, ClaimSnapshotV1, ClaimStore, Error, MAX_CLAIM_SNAPSHOT_BYTES,
    MAX_TRANSITION_AUDIT_BYTES, Result, TransitionAuditV1,
};
use async_trait::async_trait;
use libsql::{Builder, Connection, Database, TransactionBehavior, params};
use std::{path::Path, time::Duration};

#[path = "libsql_review.rs"]
mod review;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reakt_coordination_schema (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
INSERT OR IGNORE INTO reakt_coordination_schema VALUES (1,1);
CREATE TABLE IF NOT EXISTS reakt_coordination_claims (
 digest TEXT PRIMARY KEY NOT NULL, metadata BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS reakt_coordination_audits (
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

/// Immutable claim snapshots on a local durable database or a remote primary.
/// Remote providers must guarantee durable commits and read-after-write primary
/// reads. This client never uses an eventually consistent replica. Authentication
/// tokens never enter snapshot rows.
pub struct LibsqlCoordination {
    database: Database,
    local: bool,
    read_only: bool,
}

impl LibsqlCoordination {
    /// Open an application-owned absolute filesystem path with FULL synchronization.
    /// # Errors
    /// Rejects relative paths, unavailable storage and unsupported database schemas.
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

    /// Open a TLS libSQL/Turso primary with authentication supplied out of band.
    /// Credentials and endpoint details are excluded from diagnostics and metadata.
    /// # Errors
    /// Rejects insecure endpoints, connection failures and unsupported schemas.
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
    /// Rejects relative or missing paths and databases without the exact supported schema.
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
        store.validate_existing().await?;
        Ok(store)
    }

    /// Open an existing remote primary through a client that exposes reads only.
    /// The supplied remote credential must also be restricted to database reads.
    /// # Errors
    /// Rejects invalid endpoints, connections and databases without the exact supported schema.
    pub async fn open_remote_read_only(url: String, token: String) -> Result<Self> {
        validate_remote_configuration(&url, &token)?;
        let database = Builder::new_remote(url, token)
            .build()
            .await
            .map_err(|_| Error::Backend("connect remote read-only database"))?;
        let store = Self {
            database,
            local: false,
            read_only: true,
        };
        store.validate_existing().await?;
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
            transaction
                .execute_batch(review::SCHEMA)
                .await
                .map_err(|_| Error::Backend("initialize review schema"))?;
            review::validate_schema(&transaction).await?;
            let mut rows = transaction
                .query(
                    "SELECT version FROM reakt_coordination_schema WHERE id=1",
                    (),
                )
                .await
                .map_err(|_| Error::Backend("read schema"))?;
            let row = rows
                .next()
                .await
                .map_err(|_| Error::Backend("read schema row"))?
                .ok_or(Error::Invalid("missing coordination schema"))?;
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
                return Err(Error::Invalid("unsupported coordination schema"));
            }
            Ok(())
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

    async fn validate_existing(&self) -> Result<()> {
        let connection = self.connection()?;
        let mut rows = connection
            .query(
                "SELECT version FROM reakt_coordination_schema WHERE id=1",
                (),
            )
            .await
            .map_err(|_| Error::Invalid("missing coordination schema"))?;
        let row = rows
            .next()
            .await
            .map_err(|_| Error::Backend("read schema row"))?
            .ok_or(Error::Invalid("missing coordination schema"))?;
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
            return Err(Error::Invalid("unsupported coordination schema"));
        }
        connection
            .query("SELECT digest FROM reakt_coordination_claims LIMIT 0", ())
            .await
            .map_err(|_| Error::Invalid("missing immutable claim table"))?;
        connection
            .query("SELECT digest FROM reakt_coordination_audits LIMIT 0", ())
            .await
            .map_err(|_| Error::Invalid("missing immutable audit table"))?;
        Ok(())
    }
}

#[async_trait]
impl ClaimStore for LibsqlCoordination {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> Result<ClaimRef> {
        if self.read_only {
            return Err(Error::Invalid("read-only coordination store"));
        }
        let bytes = snapshot.encode()?;
        let reference = snapshot.reference()?;
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
            .map_err(|_| Error::Backend("begin immutable publication"))?;
        let result = async {
            if let Some(existing) = read_bytes(&transaction, &reference.digest).await?
                && existing != bytes
            {
                return Err(Error::Invalid("immutable snapshot mismatch"));
            }
            // Ignoring a concurrent insert never changes an existing immutable
            // row. The required post-commit read verifies what actually won.
            transaction.execute(
                "INSERT OR IGNORE INTO reakt_coordination_claims(digest,metadata) VALUES (?1,?2)",
                params![reference.digest.clone(), bytes],
            ).await.map_err(|_| Error::Backend("write immutable claims"))?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = transaction.rollback().await;
            return Err(error);
        }
        transaction
            .commit()
            .await
            .map_err(|_| Error::Backend("commit claims; retry exact snapshot"))?;
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
        read_bytes(&self.connection()?, &reference.digest)
            .await?
            .map(|bytes| ClaimSnapshotV1::decode(&bytes, reference))
            .transpose()
    }
}

#[async_trait]
impl AuditStore for LibsqlCoordination {
    async fn put_audit(&self, audit: &TransitionAuditV1) -> Result<AuditRef> {
        if self.read_only {
            return Err(Error::Invalid("read-only coordination store"));
        }
        let bytes = audit.encode()?;
        let reference = audit.reference()?;
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
            .map_err(|_| Error::Backend("begin immutable audit publication"))?;
        let result = async {
            if let Some((existing_digest, existing)) =
                read_audit_bytes(&transaction, &reference).await?
                && (existing_digest != reference.digest || existing != bytes)
            {
                return Err(Error::Invalid("immutable audit mismatch"));
            }
            let scope = &reference.scope;
            let claims = &scope.claim_scope;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO reakt_coordination_audits(\
                     registry_uid,cluster_binding,control_namespace,workload_namespace,\
                     domain_id,transition_id,sequence,digest,metadata) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        claims.registry_uid.clone(),
                        claims.cluster_binding.clone(),
                        claims.control_namespace.clone(),
                        claims.workload_namespace.clone(),
                        scope.domain_id.clone(),
                        scope.transition_id.clone(),
                        scope.sequence.to_string(),
                        reference.digest.clone(),
                        bytes,
                    ],
                )
                .await
                .map_err(|_| Error::Backend("write immutable audit"))?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = transaction.rollback().await;
            return Err(error);
        }
        transaction
            .commit()
            .await
            .map_err(|_| Error::Backend("commit audit; retry exact record"))?;
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
        read_audit_bytes(&self.connection()?, reference)
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

async fn read_bytes(connection: &Connection, digest: &str) -> Result<Option<Vec<u8>>> {
    let limit =
        i64::try_from(MAX_CLAIM_SNAPSHOT_BYTES).map_err(|_| Error::Invalid("snapshot bound"))?;
    let mut rows = connection.query(
        "SELECT CASE WHEN typeof(metadata)='blob' AND length(metadata)<=?2 THEN metadata ELSE NULL END FROM reakt_coordination_claims WHERE digest=?1",
        params![digest, limit],
    ).await.map_err(|_| Error::Backend("read immutable claims"))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|_| Error::Backend("read claim row"))?
    else {
        return Ok(None);
    };
    let bytes = row
        .get::<Vec<u8>>(0)
        .map_err(|_| Error::Invalid("snapshot bytes or size"))?;
    if rows
        .next()
        .await
        .map_err(|_| Error::Backend("read claim row"))?
        .is_some()
    {
        return Err(Error::Invalid("duplicate immutable claim rows"));
    }
    Ok(Some(bytes))
}

async fn read_audit_bytes(
    connection: &Connection,
    reference: &AuditRef,
) -> Result<Option<(String, Vec<u8>)>> {
    let limit =
        i64::try_from(MAX_TRANSITION_AUDIT_BYTES).map_err(|_| Error::Invalid("audit bound"))?;
    let scope = &reference.scope;
    let claims = &scope.claim_scope;
    let mut rows = connection
        .query(
            "SELECT sequence,digest,\
             CASE WHEN typeof(metadata)='blob' AND length(metadata)<=?7 THEN metadata ELSE NULL END \
             FROM reakt_coordination_audits \
             WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
             AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6",
            params![
                claims.registry_uid.clone(),
                claims.cluster_binding.clone(),
                claims.control_namespace.clone(),
                claims.workload_namespace.clone(),
                scope.domain_id.clone(),
                scope.transition_id.clone(),
                limit,
            ],
        )
        .await
        .map_err(|_| Error::Backend("read immutable audit"))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|_| Error::Backend("read audit row"))?
    else {
        return Ok(None);
    };
    let sequence = row
        .get::<String>(0)
        .map_err(|_| Error::Invalid("audit sequence"))?;
    let digest = row
        .get::<String>(1)
        .map_err(|_| Error::Invalid("audit digest"))?;
    let bytes = row
        .get::<Vec<u8>>(2)
        .map_err(|_| Error::Invalid("audit bytes or size"))?;
    if sequence != scope.sequence.to_string()
        || rows
            .next()
            .await
            .map_err(|_| Error::Backend("read audit row"))?
            .is_some()
    {
        return Err(Error::Invalid("audit row identity"));
    }
    Ok(Some((digest, bytes)))
}
