use super::*;
use libsql::{Builder, Connection, Rows, TransactionBehavior, params};

const FAILURE_CANARY: &str = "private-write-failure-canary";
const FAILURE_TRIGGER: &str = "private_write_failure";

async fn prepared() -> TestResult<(tempfile::TempDir, PathBuf, Fixture, RaceFixture)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let prior = fixture()?;
    seed(&path, &prior).await?;
    let candidate = race_fixture(&prior)?;
    Ok((directory, path, prior, candidate))
}

async fn install_failure_trigger(connection: &Connection, table: &str) -> TestResult {
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER {FAILURE_TRIGGER} AFTER INSERT ON {table} \
             BEGIN SELECT RAISE(FAIL,'{FAILURE_CANARY}'); END"
        ))
        .await?;
    Ok(())
}

async fn remove_failure_trigger(connection: &Connection) -> TestResult {
    connection
        .execute_batch(&format!("DROP TRIGGER {FAILURE_TRIGGER}"))
        .await?;
    Ok(())
}

fn require_raw_failure(result: libsql::Result<u64>) -> TestResult {
    let Err(error) = result else {
        return Err("failure trigger accepted raw insert".into());
    };
    if !error.to_string().contains(FAILURE_CANARY) {
        return Err("raw insert did not return the synthetic failure".into());
    }
    Ok(())
}

fn require_public_failure(error: &coordination::Error, expected: &'static str) -> TestResult {
    let display = error.to_string();
    let debug = format!("{error:?}");
    if display.contains(FAILURE_CANARY) || debug.contains(FAILURE_CANARY) {
        return Err("public write error disclosed the failure canary".into());
    }
    match error {
        coordination::Error::Backend(actual) if *actual == expected => Ok(()),
        _ => Err("public write returned the wrong backend error".into()),
    }
}

async fn require_exact_metadata(mut rows: Rows, expected: &[u8]) -> TestResult {
    let row = rows
        .next()
        .await?
        .ok_or("failed raw insert was not visible in its transaction")?;
    if row.get::<Vec<u8>>(0)? != expected || rows.next().await?.is_some() {
        return Err("failed raw insert metadata differed in its transaction".into());
    }
    Ok(())
}

#[tokio::test]
async fn claim_write_failure_rolls_back_and_exact_retry_succeeds() -> TestResult {
    let (_directory, path, prior, candidate) = prepared().await?;
    let reference = candidate.claims.reference()?;
    if reference != candidate.claim_ref {
        return Err("claim candidate reference differs".into());
    }
    let bytes = compiler_core::canonical_bytes(&candidate.claims)?;
    let database = Builder::new_local(&path).build().await?;
    let connection = database.connect()?;
    install_failure_trigger(&connection, "reakt_coordination_claims").await?;

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    require_raw_failure(
        transaction
            .execute(
                "INSERT OR IGNORE INTO reakt_coordination_claims(digest,metadata) VALUES (?1,?2)",
                params![reference.digest.clone(), bytes.clone()],
            )
            .await,
    )?;
    require_exact_metadata(
        transaction
            .query(
                "SELECT metadata FROM reakt_coordination_claims WHERE digest=?1",
                params![reference.digest.clone()],
            )
            .await?,
        &bytes,
    )
    .await?;
    transaction.rollback().await?;

    let after_control = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&after_control, &prior).await?;
    if after_control.read_claims(&reference).await?.is_some() {
        return Err("rolled-back raw claim remained visible".into());
    }
    drop(after_control);

    let store = LibsqlCoordination::open_local(&path).await?;
    let error = store
        .put_claims(&candidate.claims)
        .await
        .err()
        .ok_or("failure trigger accepted public claim write")?;
    require_public_failure(&error, "write immutable claims")?;
    let failed_reopen = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&failed_reopen, &prior).await?;
    if failed_reopen.read_claims(&reference).await?.is_some() {
        return Err("failed public claim write was not rolled back".into());
    }
    drop(failed_reopen);

    remove_failure_trigger(&connection).await?;
    if store.put_claims(&candidate.claims).await? != reference
        || store.put_claims(&candidate.claims).await? != reference
    {
        return Err("claim retry reference differs".into());
    }
    let successful_reopen = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&successful_reopen, &prior).await?;
    if successful_reopen.read_claims(&reference).await? != Some(candidate.claims) {
        return Err("retried claim differs after reopen".into());
    }
    Ok(())
}

#[tokio::test]
async fn review_write_failure_rolls_back_and_exact_retry_succeeds() -> TestResult {
    let (_directory, path, prior, candidate) = prepared().await?;
    let reference = candidate.left_review.reference()?;
    if reference != candidate.left_review_ref {
        return Err("review candidate reference differs".into());
    }
    let bytes = compiler_core::canonical_bytes(&candidate.left_review)?;
    let database = Builder::new_local(&path).build().await?;
    let connection = database.connect()?;
    install_failure_trigger(&connection, "reakt_coordination_review_objects").await?;
    let scope = &reference.scope;
    let claims = &scope.claim_scope;

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    require_raw_failure(
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
                    bytes.clone(),
                ],
            )
            .await,
    )?;
    require_exact_metadata(
        transaction
            .query(
                "SELECT metadata FROM reakt_coordination_review_objects \
                 WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
                 AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6",
                params![
                    claims.registry_uid.clone(),
                    claims.cluster_binding.clone(),
                    claims.control_namespace.clone(),
                    claims.workload_namespace.clone(),
                    scope.domain_id.clone(),
                    scope.transition_id.clone(),
                ],
            )
            .await?,
        &bytes,
    )
    .await?;
    transaction.rollback().await?;

    let after_control = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&after_control, &prior).await?;
    if after_control.read_review(&reference).await?.is_some() {
        return Err("rolled-back raw review remained visible".into());
    }
    drop(after_control);

    let store = LibsqlCoordination::open_local(&path).await?;
    let error = store
        .put_review(&candidate.left_review)
        .await
        .err()
        .ok_or("failure trigger accepted public review write")?;
    require_public_failure(&error, "write immutable review")?;
    let failed_reopen = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&failed_reopen, &prior).await?;
    if failed_reopen.read_review(&reference).await?.is_some() {
        return Err("failed public review write was not rolled back".into());
    }
    drop(failed_reopen);

    remove_failure_trigger(&connection).await?;
    if store.put_review(&candidate.left_review).await? != reference
        || store.put_review(&candidate.left_review).await? != reference
    {
        return Err("review retry reference differs".into());
    }
    let successful_reopen = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&successful_reopen, &prior).await?;
    if successful_reopen.read_review(&reference).await? != Some(candidate.left_review) {
        return Err("retried review differs after reopen".into());
    }
    Ok(())
}

#[tokio::test]
async fn audit_write_failure_rolls_back_and_exact_retry_succeeds() -> TestResult {
    let (_directory, path, prior, candidate) = prepared().await?;
    let reference = candidate.left_audit.reference()?;
    if reference != candidate.left_audit_ref {
        return Err("audit candidate reference differs".into());
    }
    let bytes = compiler_core::canonical_bytes(&candidate.left_audit)?;
    let database = Builder::new_local(&path).build().await?;
    let connection = database.connect()?;
    install_failure_trigger(&connection, "reakt_coordination_audits").await?;
    let scope = &reference.scope;
    let claims = &scope.claim_scope;

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    require_raw_failure(
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
                    bytes.clone(),
                ],
            )
            .await,
    )?;
    require_exact_metadata(
        transaction
            .query(
                "SELECT metadata FROM reakt_coordination_audits \
                 WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
                 AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6",
                params![
                    claims.registry_uid.clone(),
                    claims.cluster_binding.clone(),
                    claims.control_namespace.clone(),
                    claims.workload_namespace.clone(),
                    scope.domain_id.clone(),
                    scope.transition_id.clone(),
                ],
            )
            .await?,
        &bytes,
    )
    .await?;
    transaction.rollback().await?;

    let after_control = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&after_control, &prior).await?;
    if after_control.read_audit(&reference).await?.is_some() {
        return Err("rolled-back raw audit remained visible".into());
    }
    drop(after_control);

    let store = LibsqlCoordination::open_local(&path).await?;
    let error = store
        .put_audit(&candidate.left_audit)
        .await
        .err()
        .ok_or("failure trigger accepted public audit write")?;
    require_public_failure(&error, "write immutable audit")?;
    let failed_reopen = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&failed_reopen, &prior).await?;
    if failed_reopen.read_audit(&reference).await?.is_some() {
        return Err("failed public audit write was not rolled back".into());
    }
    drop(failed_reopen);

    remove_failure_trigger(&connection).await?;
    if store.put_audit(&candidate.left_audit).await? != reference
        || store.put_audit(&candidate.left_audit).await? != reference
    {
        return Err("audit retry reference differs".into());
    }
    let successful_reopen = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&successful_reopen, &prior).await?;
    if successful_reopen.read_audit(&reference).await? != Some(candidate.left_audit) {
        return Err("retried audit differs after reopen".into());
    }
    Ok(())
}
