use crate::{ClaimRef, ClaimSnapshotV1, ClaimStore, Error, MAX_CLAIM_SNAPSHOT_BYTES, Result};
use async_trait::async_trait;
use libsql::{Builder, Connection, Database, TransactionBehavior, params};
use std::{path::Path, time::Duration};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reakt_coordination_schema (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
INSERT OR IGNORE INTO reakt_coordination_schema VALUES (1,1);
CREATE TABLE IF NOT EXISTS reakt_coordination_claims (
 digest TEXT PRIMARY KEY NOT NULL, metadata BLOB NOT NULL);
";

/// Immutable claim snapshots on a local durable database or a remote primary.
/// Remote providers must guarantee durable commits and read-after-write primary
/// reads. This client never uses an eventually consistent replica. Authentication
/// tokens never enter snapshot rows.
pub struct LibsqlCoordination {
    database: Database,
    local: bool,
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
        };
        store.initialize().await?;
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
}

#[async_trait]
impl ClaimStore for LibsqlCoordination {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> Result<ClaimRef> {
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
