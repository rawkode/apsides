//! Strict content-addressed review publication and lookup.
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

const TABLE: &str = "reakt_coordination_review_objects";
const READ_QUERY: &str = "SELECT \
 CASE WHEN typeof(sequence)='text' AND length(sequence)<=20 THEN sequence ELSE NULL END,\
 CASE WHEN typeof(digest)='text' AND length(digest)=71 THEN digest ELSE NULL END,\
 CASE WHEN typeof(metadata)='blob' AND length(metadata)<=?7 THEN metadata ELSE NULL END \
 FROM reakt_coordination_review_objects \
 WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
 AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6 AND digest=?8";

/// Require the complete immutable object-table schema.
pub(super) async fn validate_schema(connection: &Connection) -> Result<()> {
    if !validate_if_present(connection).await? {
        return Err(Error::Invalid("missing immutable review table"));
    }
    Ok(())
}

async fn validate_if_present(connection: &Connection) -> Result<bool> {
    let mut rows = connection
        .query(
            "SELECT type FROM sqlite_master WHERE name=:table_name",
            named_params! { ":table_name": TABLE },
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
    validate_table_schema(connection).await?;
    Ok(true)
}

async fn validate_table_schema(connection: &Connection) -> Result<()> {
    let expected = [
        ("registry_uid", "TEXT", 1),
        ("cluster_binding", "TEXT", 2),
        ("control_namespace", "TEXT", 3),
        ("workload_namespace", "TEXT", 4),
        ("domain_id", "TEXT", 5),
        ("transition_id", "TEXT", 6),
        ("sequence", "TEXT", 0),
        ("digest", "TEXT", 7),
        ("metadata", "BLOB", 0),
    ];
    let mut rows = connection
        .query("PRAGMA table_info(reakt_coordination_review_objects)", ())
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
    // A read-only connection may predate provisioning; every read validates it.
    validate_schema(connection).await?;
    read_table_bytes(connection, reference).await
}

async fn read_table_bytes(
    connection: &Connection,
    reference: &ReviewRef,
) -> Result<Option<(String, Vec<u8>)>> {
    let limit = i64::try_from(MAX_REVIEW_METADATA_BYTES)
        .map_err(|_| Error::Invalid("review metadata bound"))?;
    let scope = &reference.scope;
    let claims = &scope.claim_scope;
    let mut rows = connection
        .query(
            READ_QUERY,
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
