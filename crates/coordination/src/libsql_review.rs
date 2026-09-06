//! Content-addressed review publication with read-only legacy receipt fallback.
use super::LibsqlCoordination;
use crate::{Error, MAX_REVIEW_METADATA_BYTES, Result, ReviewMetadataV1, ReviewRef, ReviewStore};
use async_trait::async_trait;
use libsql::{Connection, TransactionBehavior, named_params, params};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reakt_coordination_review_objects (
 registry_uid TEXT NOT NULL,
 cluster_binding TEXT NOT NULL,
 control_namespace TEXT NOT NULL,
 workload_namespace TEXT NOT NULL,
 domain_id TEXT NOT NULL,
 transition_id TEXT NOT NULL,
 sequence TEXT NOT NULL,
 digest TEXT NOT NULL,
 metadata BLOB NOT NULL,
 PRIMARY KEY (registry_uid,cluster_binding,control_namespace,workload_namespace,domain_id,transition_id,digest));
";

#[derive(Clone, Copy)]
enum ReviewTable {
    Objects,
    Legacy,
}

impl ReviewTable {
    const fn name(self) -> &'static str {
        match self {
            Self::Objects => "reakt_coordination_review_objects",
            Self::Legacy => "reakt_coordination_reviews",
        }
    }

    const fn schema_query(self) -> &'static str {
        match self {
            Self::Objects => "PRAGMA table_info(reakt_coordination_review_objects)",
            Self::Legacy => "PRAGMA table_info(reakt_coordination_reviews)",
        }
    }

    const fn read_query(self) -> &'static str {
        match self {
            Self::Objects => {
                "SELECT \
                CASE WHEN typeof(sequence)='text' AND length(sequence)<=20 THEN sequence ELSE NULL END,\
                CASE WHEN typeof(digest)='text' AND length(digest)=71 THEN digest ELSE NULL END,\
                CASE WHEN typeof(metadata)='blob' AND length(metadata)<=?7 THEN metadata ELSE NULL END \
                FROM reakt_coordination_review_objects \
                WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
                AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6 AND digest=?8"
            }
            Self::Legacy => {
                "SELECT \
                CASE WHEN typeof(sequence)='text' AND length(sequence)<=20 THEN sequence ELSE NULL END,\
                CASE WHEN typeof(digest)='text' AND length(digest)=71 THEN digest ELSE NULL END,\
                CASE WHEN typeof(metadata)='blob' AND length(metadata)<=?7 THEN metadata ELSE NULL END \
                FROM reakt_coordination_reviews \
                WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
                AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6 AND digest=?8"
            }
        }
    }
}

/// Initialization requires the new object table but never creates a legacy table.
pub(super) async fn validate_schema(connection: &Connection) -> Result<()> {
    if !validate_if_present(connection, ReviewTable::Objects).await? {
        return Err(Error::Invalid("missing immutable review table"));
    }
    validate_if_present(connection, ReviewTable::Legacy).await?;
    Ok(())
}

async fn validate_if_present(connection: &Connection, table: ReviewTable) -> Result<bool> {
    let mut rows = connection
        .query(
            "SELECT type FROM sqlite_master WHERE name=:table_name",
            named_params! { ":table_name": table.name() },
        )
        .await
        .map_err(|_| Error::Backend("read review table presence"))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|_| Error::Backend("read review table presence"))?
    else {
        return Ok(false);
    };
    if row
        .get::<String>(0)
        .map_err(|_| Error::Invalid("review table schema"))?
        != "table"
        || rows
            .next()
            .await
            .map_err(|_| Error::Backend("read review table presence"))?
            .is_some()
    {
        return Err(Error::Invalid("review table schema"));
    }
    validate_table_schema(connection, table).await?;
    Ok(true)
}

async fn validate_table_schema(connection: &Connection, table: ReviewTable) -> Result<()> {
    let digest_key = match table {
        ReviewTable::Objects => 7,
        ReviewTable::Legacy => 0,
    };
    let expected = [
        ("registry_uid", "TEXT", 1),
        ("cluster_binding", "TEXT", 2),
        ("control_namespace", "TEXT", 3),
        ("workload_namespace", "TEXT", 4),
        ("domain_id", "TEXT", 5),
        ("transition_id", "TEXT", 6),
        ("sequence", "TEXT", 0),
        ("digest", "TEXT", digest_key),
        ("metadata", "BLOB", 0),
    ];
    let mut rows = connection
        .query(table.schema_query(), ())
        .await
        .map_err(|_| Error::Invalid("review table schema"))?;
    for (name, sql_type, primary_key) in expected {
        let row = rows
            .next()
            .await
            .map_err(|_| Error::Invalid("review table schema"))?
            .ok_or(Error::Invalid("review table schema"))?;
        if row
            .get::<String>(1)
            .map_err(|_| Error::Invalid("review table schema"))?
            != name
            || row
                .get::<String>(2)
                .map_err(|_| Error::Invalid("review table schema"))?
                != sql_type
            || row
                .get::<i64>(3)
                .map_err(|_| Error::Invalid("review table schema"))?
                != 1
            || row
                .get::<i64>(5)
                .map_err(|_| Error::Invalid("review table schema"))?
                != primary_key
        {
            return Err(Error::Invalid("review table schema"));
        }
    }
    if rows
        .next()
        .await
        .map_err(|_| Error::Invalid("review table schema"))?
        .is_some()
    {
        return Err(Error::Invalid("review table schema"));
    }
    Ok(())
}

#[async_trait]
impl ReviewStore for LibsqlCoordination {
    async fn put_review(&self, metadata: &ReviewMetadataV1) -> Result<ReviewRef> {
        if self.read_only {
            return Err(Error::Invalid("read-only coordination store"));
        }
        let bytes = metadata.encode()?;
        let reference = metadata.reference()?;
        let connection = self.connection()?;
        if self.local {
            connection
                .execute_batch("PRAGMA synchronous=FULL;")
                .await
                .map_err(|_| Error::Backend("review commit durability"))?;
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(|_| Error::Backend("begin immutable review publication"))?;
        let result = async {
            if let Some((digest, existing)) = read_bytes(&transaction, &reference).await?
                && (digest != reference.digest || existing != bytes)
            {
                return Err(Error::Invalid("immutable review mismatch"));
            }
            let scope = &reference.scope;
            let claims = &scope.claim_scope;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO reakt_coordination_review_objects(\
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
                .map_err(|_| Error::Backend("write immutable review"))?;
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
            .map_err(|_| Error::Backend("commit review; retry exact metadata"))?;
        let actual = self
            .read_review(&reference)
            .await?
            .ok_or(Error::Invalid("committed review missing"))?;
        if actual != *metadata {
            return Err(Error::Invalid("committed review mismatch"));
        }
        Ok(reference)
    }

    async fn read_review(&self, reference: &ReviewRef) -> Result<Option<ReviewMetadataV1>> {
        reference.validate()?;
        read_bytes(&self.connection()?, reference)
            .await?
            .map(|(digest, bytes)| {
                if digest != reference.digest {
                    return Err(Error::Invalid("review reference mismatch"));
                }
                ReviewMetadataV1::decode(&bytes, reference)
            })
            .transpose()
    }
}

async fn read_bytes(
    connection: &Connection,
    reference: &ReviewRef,
) -> Result<Option<(String, Vec<u8>)>> {
    // Detect on every read: a read-only client can predate table provisioning.
    // Validate both present schemas before accepting data from either one.
    let objects = validate_if_present(connection, ReviewTable::Objects).await?;
    let legacy = validate_if_present(connection, ReviewTable::Legacy).await?;
    if !objects && !legacy {
        return Err(Error::Invalid("missing immutable review table"));
    }
    if objects
        && let Some(bytes) = read_table_bytes(connection, reference, ReviewTable::Objects).await?
    {
        return Ok(Some(bytes));
    }
    if legacy {
        return read_table_bytes(connection, reference, ReviewTable::Legacy).await;
    }
    Ok(None)
}

async fn read_table_bytes(
    connection: &Connection,
    reference: &ReviewRef,
    table: ReviewTable,
) -> Result<Option<(String, Vec<u8>)>> {
    let limit = i64::try_from(MAX_REVIEW_METADATA_BYTES)
        .map_err(|_| Error::Invalid("review metadata bound"))?;
    let scope = &reference.scope;
    let claims = &scope.claim_scope;
    let mut rows = connection
        .query(
            table.read_query(),
            params![
                claims.registry_uid.clone(),
                claims.cluster_binding.clone(),
                claims.control_namespace.clone(),
                claims.workload_namespace.clone(),
                scope.domain_id.clone(),
                scope.transition_id.clone(),
                limit,
                reference.digest.clone(),
            ],
        )
        .await
        .map_err(|_| Error::Backend("read immutable review"))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|_| Error::Backend("read review row"))?
    else {
        return Ok(None);
    };
    let sequence = row
        .get::<String>(0)
        .map_err(|_| Error::Invalid("review sequence"))?;
    let digest = row
        .get::<String>(1)
        .map_err(|_| Error::Invalid("review digest"))?;
    let bytes = row
        .get::<Vec<u8>>(2)
        .map_err(|_| Error::Invalid("review bytes or size"))?;
    if sequence != scope.sequence.to_string()
        || rows
            .next()
            .await
            .map_err(|_| Error::Backend("read review row"))?
            .is_some()
    {
        return Err(Error::Invalid("review allocation or row count"));
    }
    Ok(Some((digest, bytes)))
}
