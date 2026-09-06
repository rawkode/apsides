use super::*;
use libsql::{Builder, Connection, Transaction, TransactionBehavior, params};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CHILD_TEST: &str = "process_crash::local_coordination_sigkill_child";
const CHILD_NONCE_ENV: &str = "REAKT_COORDINATION_CRASH_CHILD_NONCE";
const CHILD_PATH_ENV: &str = "REAKT_COORDINATION_CRASH_CHILD_PATH";
const CHILD_READY_ENV: &str = "REAKT_COORDINATION_CRASH_CHILD_READY";
const CHILD_STORE_ENV: &str = "REAKT_COORDINATION_CRASH_CHILD_STORE";
const READY_LIMIT: u64 = 4096;
const SPILL_ROWS: u64 = 96;
const SPILL_BYTES: usize = 16 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreKind {
    Claim,
    Review,
    Audit,
}

impl StoreKind {
    const ALL: [Self; 3] = [Self::Claim, Self::Review, Self::Audit];

    fn name(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Review => "review",
            Self::Audit => "audit",
        }
    }

    fn parse(value: &str) -> TestResult<Self> {
        match value {
            "claim" => Ok(Self::Claim),
            "review" => Ok(Self::Review),
            "audit" => Ok(Self::Audit),
            _ => Err("coordination crash child store is invalid".into()),
        }
    }
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Ready<'a> {
    nonce: &'a str,
    kind: &'a str,
    outcome: &'a str,
    journal_bytes: u64,
}

struct OwnedChild {
    child: Option<Child>,
}

impl OwnedChild {
    fn spawn(path: &Path, ready: &Path, nonce: &str, kind: StoreKind) -> TestResult<Self> {
        let child = Command::new(std::env::current_exe()?)
            .arg("--ignored")
            .arg("--exact")
            .arg(CHILD_TEST)
            .arg("--nocapture")
            .env(CHILD_NONCE_ENV, nonce)
            .env(CHILD_PATH_ENV, path)
            .env(CHILD_READY_ENV, ready)
            .env(CHILD_STORE_ENV, kind.name())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self { child: Some(child) })
    }

    fn child_mut(&mut self) -> TestResult<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| "coordination crash child was already reaped".into())
    }

    fn kill_and_reap(&mut self) -> TestResult {
        if self.child_mut()?.try_wait()?.is_some() {
            return Err("coordination crash child exited before SIGKILL".into());
        }
        self.child_mut()?.kill()?;
        let deadline = Instant::now() + REAP_TIMEOUT;
        loop {
            if let Some(status) = self.child_mut()?.try_wait()? {
                self.child = None;
                if status.signal() != Some(9) {
                    return Err("coordination crash child did not exit from SIGKILL".into());
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("coordination crash child reap timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let _ = child.kill();
        let deadline = Instant::now() + REAP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
    }
}

async fn pragma_integer(connection: &Connection, query: &'static str) -> TestResult<i64> {
    let mut rows = connection.query(query, ()).await?;
    let row = rows
        .next()
        .await?
        .ok_or("coordination crash durability setting is missing")?;
    Ok(row.get::<i64>(0)?)
}

async fn configure_crash(connection: &Connection) -> TestResult {
    connection
        .execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; \
             PRAGMA cache_size=8; PRAGMA cache_spill=8;",
        )
        .await?;
    let mut rows = connection.query("PRAGMA journal_mode", ()).await?;
    let row = rows
        .next()
        .await?
        .ok_or("coordination crash journal setting is missing")?;
    if row.get::<String>(0)? != "delete"
        || pragma_integer(connection, "PRAGMA synchronous").await? != 2
        || pragma_integer(connection, "PRAGMA cache_size").await? != 8
        || pragma_integer(connection, "PRAGMA cache_spill").await? != 8
    {
        return Err("coordination crash durability profile differs".into());
    }
    Ok(())
}

async fn seed_spill_rows(connection: &Connection, kind: StoreKind) -> TestResult {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    let payload = vec![b's'; SPILL_BYTES];
    for index in 0..SPILL_ROWS {
        let inserted = match kind {
            StoreKind::Claim => {
                transaction
                    .execute(
                        "INSERT INTO reakt_coordination_claims(digest,metadata) VALUES (?1,?2)",
                        params![format!("synthetic-crash-claim-{index:04}"), payload.clone()],
                    )
                    .await?
            }
            StoreKind::Review => {
                transaction
                    .execute(
                        "INSERT INTO reakt_coordination_review_objects(\
                         registry_uid,cluster_binding,control_namespace,workload_namespace,\
                         domain_id,transition_id,sequence,digest,metadata) \
                         VALUES (?1,'cluster','control','workload','domain',?2,'2',?3,?4)",
                        params![
                            "synthetic-crash-review",
                            format!("transition-{index:04}"),
                            format!("synthetic-crash-review-{index:04}"),
                            payload.clone(),
                        ],
                    )
                    .await?
            }
            StoreKind::Audit => {
                transaction
                    .execute(
                        "INSERT INTO reakt_coordination_audits(\
                         registry_uid,cluster_binding,control_namespace,workload_namespace,\
                         domain_id,transition_id,sequence,digest,metadata) \
                         VALUES (?1,'cluster','control','workload','domain',?2,'2',?3,?4)",
                        params![
                            "synthetic-crash-audit",
                            format!("transition-{index:04}"),
                            format!("synthetic-crash-audit-{index:04}"),
                            payload.clone(),
                        ],
                    )
                    .await?
            }
        };
        if inserted != 1 {
            return Err("coordination crash spill row was not inserted".into());
        }
    }
    transaction.commit().await?;
    Ok(())
}

async fn insert_candidate(
    transaction: &Transaction,
    kind: StoreKind,
    candidate: &RaceFixture,
) -> TestResult {
    let inserted = match kind {
        StoreKind::Claim => {
            let bytes = compiler_core::canonical_bytes(&candidate.claims)?;
            transaction
                .execute(
                    "INSERT INTO reakt_coordination_claims(digest,metadata) VALUES (?1,?2)",
                    params![candidate.claim_ref.digest.clone(), bytes],
                )
                .await?
        }
        StoreKind::Review => {
            let bytes = compiler_core::canonical_bytes(&candidate.left_review)?;
            let scope = &candidate.left_review_ref.scope;
            let claims = &scope.claim_scope;
            transaction
                .execute(
                    "INSERT INTO reakt_coordination_review_objects(\
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
                        candidate.left_review_ref.digest.clone(),
                        bytes,
                    ],
                )
                .await?
        }
        StoreKind::Audit => {
            let bytes = compiler_core::canonical_bytes(&candidate.left_audit)?;
            let scope = &candidate.left_audit_ref.scope;
            let claims = &scope.claim_scope;
            transaction
                .execute(
                    "INSERT INTO reakt_coordination_audits(\
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
                        candidate.left_audit_ref.digest.clone(),
                        bytes,
                    ],
                )
                .await?
        }
    };
    if inserted != 1 {
        return Err("coordination crash candidate was not inserted".into());
    }
    Ok(())
}

async fn delete_spill_rows(transaction: &Transaction, kind: StoreKind) -> TestResult {
    let deleted = match kind {
        StoreKind::Claim => {
            transaction
                .execute(
                    "DELETE FROM reakt_coordination_claims \
                     WHERE digest GLOB 'synthetic-crash-claim-*'",
                    (),
                )
                .await?
        }
        StoreKind::Review => {
            transaction
                .execute(
                    "DELETE FROM reakt_coordination_review_objects \
                     WHERE registry_uid='synthetic-crash-review'",
                    (),
                )
                .await?
        }
        StoreKind::Audit => {
            transaction
                .execute(
                    "DELETE FROM reakt_coordination_audits \
                     WHERE registry_uid='synthetic-crash-audit'",
                    (),
                )
                .await?
        }
    };
    if deleted != SPILL_ROWS {
        return Err("coordination crash spill row count differs".into());
    }
    Ok(())
}

async fn verify_candidate_in_transaction(
    transaction: &Transaction,
    kind: StoreKind,
    candidate: &RaceFixture,
) -> TestResult {
    let (query, parameters, expected_digest, expected_bytes) = match kind {
        StoreKind::Claim => (
            "SELECT digest,metadata FROM reakt_coordination_claims WHERE digest=?1",
            vec![candidate.claim_ref.digest.clone()],
            candidate.claim_ref.digest.clone(),
            compiler_core::canonical_bytes(&candidate.claims)?,
        ),
        StoreKind::Review => (
            "SELECT digest,metadata FROM reakt_coordination_review_objects \
             WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
             AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6 AND digest=?7",
            {
                let scope = &candidate.left_review_ref.scope;
                vec![
                    scope.claim_scope.registry_uid.clone(),
                    scope.claim_scope.cluster_binding.clone(),
                    scope.claim_scope.control_namespace.clone(),
                    scope.claim_scope.workload_namespace.clone(),
                    scope.domain_id.clone(),
                    scope.transition_id.clone(),
                    candidate.left_review_ref.digest.clone(),
                ]
            },
            candidate.left_review_ref.digest.clone(),
            compiler_core::canonical_bytes(&candidate.left_review)?,
        ),
        StoreKind::Audit => (
            "SELECT digest,metadata FROM reakt_coordination_audits \
             WHERE registry_uid=?1 AND cluster_binding=?2 AND control_namespace=?3 \
             AND workload_namespace=?4 AND domain_id=?5 AND transition_id=?6",
            {
                let scope = &candidate.left_audit_ref.scope;
                vec![
                    scope.claim_scope.registry_uid.clone(),
                    scope.claim_scope.cluster_binding.clone(),
                    scope.claim_scope.control_namespace.clone(),
                    scope.claim_scope.workload_namespace.clone(),
                    scope.domain_id.clone(),
                    scope.transition_id.clone(),
                ]
            },
            candidate.left_audit_ref.digest.clone(),
            compiler_core::canonical_bytes(&candidate.left_audit)?,
        ),
    };
    let mut rows = transaction.query(query, parameters).await?;
    let row = rows
        .next()
        .await?
        .ok_or("coordination crash candidate is missing in its transaction")?;
    if row.get::<String>(0)? != expected_digest
        || row.get::<Vec<u8>>(1)? != expected_bytes
        || rows.next().await?.is_some()
    {
        return Err("coordination crash candidate differs in its transaction".into());
    }
    Ok(())
}

fn journal_bytes(path: &Path) -> TestResult<u64> {
    let metadata = std::fs::symlink_metadata(path.with_extension("db-journal"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || metadata.len() <= 512
    {
        return Err("coordination crash requires a populated rollback journal".into());
    }
    Ok(metadata.len())
}

fn write_ready(path: &Path, nonce: &str, kind: StoreKind, journal_bytes: u64) -> TestResult {
    let parent = path
        .parent()
        .ok_or("coordination crash readiness path has no parent")?;
    let temporary = parent.join(format!("crash-{nonce}.ready.tmp"));
    if path.exists() || temporary.exists() {
        return Err("coordination crash readiness paths must be absent".into());
    }
    let bytes = serde_json::to_vec(&Ready {
        nonce,
        kind: kind.name(),
        outcome: "ready",
        journal_bytes,
    })?;
    if u64::try_from(bytes.len())? > READY_LIMIT {
        return Err("coordination crash readiness exceeds its bound".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn child_configuration() -> TestResult<(PathBuf, PathBuf, String, StoreKind)> {
    let nonce =
        std::env::var(CHILD_NONCE_ENV).map_err(|_| "coordination crash child nonce is required")?;
    validate_nonce(&nonce)?;
    let path = PathBuf::from(
        std::env::var_os(CHILD_PATH_ENV).ok_or("coordination crash child path is required")?,
    );
    let ready = PathBuf::from(
        std::env::var_os(CHILD_READY_ENV)
            .ok_or("coordination crash child readiness path is required")?,
    );
    let kind = StoreKind::parse(
        &std::env::var(CHILD_STORE_ENV)
            .map_err(|_| "coordination crash child store is required")?,
    )?;
    if !path.is_absolute()
        || path.parent() != ready.parent()
        || path.file_name().and_then(|name| name.to_str())
            != Some(format!("coordination-{nonce}.db").as_str())
        || ready.file_name().and_then(|name| name.to_str())
            != Some(format!("crash-{nonce}.ready").as_str())
    {
        return Err("coordination crash child paths are invalid".into());
    }
    Ok((path, ready, nonce, kind))
}

async fn hold_uncommitted_candidate(
    path: &Path,
    ready: &Path,
    nonce: &str,
    kind: StoreKind,
) -> TestResult {
    let prior = fixture()?;
    let candidate = race_fixture(&prior)?;
    let database = Builder::new_local(path).build().await?;
    let connection = database.connect()?;
    configure_crash(&connection).await?;
    seed_spill_rows(&connection, kind).await?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    insert_candidate(&transaction, kind, &candidate).await?;
    delete_spill_rows(&transaction, kind).await?;
    verify_candidate_in_transaction(&transaction, kind, &candidate).await?;
    let bytes = journal_bytes(path)?;
    write_ready(ready, nonce, kind, bytes)?;
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    transaction.rollback().await?;
    Err("coordination crash child was not terminated".into())
}

fn read_ready(path: &Path, nonce: &str, kind: StoreKind) -> TestResult {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > READY_LIMIT
    {
        return Err("coordination crash readiness file is invalid".into());
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(READY_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len())? > READY_LIMIT {
        return Err("coordination crash readiness file exceeds its bound".into());
    }
    let ready: Ready<'_> = serde_json::from_slice(&bytes)?;
    if ready.nonce != nonce
        || ready.kind != kind.name()
        || ready.outcome != "ready"
        || ready.journal_bytes <= 512
    {
        return Err("coordination crash readiness metadata differs".into());
    }
    Ok(())
}

fn wait_until_ready(
    child: &mut OwnedChild,
    path: &Path,
    nonce: &str,
    kind: StoreKind,
) -> TestResult {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if child.child_mut()?.try_wait()?.is_some() {
            return Err("coordination crash child exited before readiness".into());
        }
        match path.symlink_metadata() {
            Ok(_) => {
                read_ready(path, nonce, kind)?;
                if child.child_mut()?.try_wait()?.is_some() {
                    return Err("coordination crash child exited at readiness".into());
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("coordination crash readiness cannot be inspected".into()),
        }
        if Instant::now() >= deadline {
            return Err("coordination crash child startup timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn verify_candidate_absent(
    store: &LibsqlCoordination,
    kind: StoreKind,
    candidate: &RaceFixture,
) -> TestResult {
    let absent = match kind {
        StoreKind::Claim => store.read_claims(&candidate.claim_ref).await?.is_none(),
        StoreKind::Review => store
            .read_review(&candidate.left_review_ref)
            .await?
            .is_none(),
        StoreKind::Audit => store.read_audit(&candidate.left_audit_ref).await?.is_none(),
    };
    if !absent {
        return Err("coordination crash candidate survived recovery".into());
    }
    Ok(())
}

async fn verify_row_count(path: &Path, kind: StoreKind) -> TestResult {
    let database = Builder::new_local(path).build().await?;
    let connection = database.connect()?;
    let query = match kind {
        StoreKind::Claim => "SELECT count(*) FROM reakt_coordination_claims",
        StoreKind::Review => "SELECT count(*) FROM reakt_coordination_review_objects",
        StoreKind::Audit => "SELECT count(*) FROM reakt_coordination_audits",
    };
    let mut rows = connection.query(query, ()).await?;
    let count = rows
        .next()
        .await?
        .ok_or("coordination crash recovered row count is missing")?
        .get::<i64>(0)?;
    let expected = i64::try_from(SPILL_ROWS + 1)?;
    if count != expected || rows.next().await?.is_some() {
        return Err("coordination crash recovered row count differs".into());
    }
    Ok(())
}

async fn retry_candidate_twice(
    store: &LibsqlCoordination,
    kind: StoreKind,
    candidate: &RaceFixture,
) -> TestResult {
    let references_match = match kind {
        StoreKind::Claim => {
            store.put_claims(&candidate.claims).await? == candidate.claim_ref
                && store.put_claims(&candidate.claims).await? == candidate.claim_ref
        }
        StoreKind::Review => {
            store.put_review(&candidate.left_review).await? == candidate.left_review_ref
                && store.put_review(&candidate.left_review).await? == candidate.left_review_ref
        }
        StoreKind::Audit => {
            store.put_audit(&candidate.left_audit).await? == candidate.left_audit_ref
                && store.put_audit(&candidate.left_audit).await? == candidate.left_audit_ref
        }
    };
    if !references_match {
        return Err("coordination crash exact retry reference differs".into());
    }
    Ok(())
}

async fn verify_candidate_present(
    store: &LibsqlCoordination,
    kind: StoreKind,
    candidate: &RaceFixture,
) -> TestResult {
    let present = match kind {
        StoreKind::Claim => {
            store.read_claims(&candidate.claim_ref).await? == Some(candidate.claims.clone())
        }
        StoreKind::Review => {
            store.read_review(&candidate.left_review_ref).await?
                == Some(candidate.left_review.clone())
        }
        StoreKind::Audit => {
            store.read_audit(&candidate.left_audit_ref).await? == Some(candidate.left_audit.clone())
        }
    };
    if !present {
        return Err("coordination crash retried metadata differs".into());
    }
    Ok(())
}

fn unique_nonce() -> TestResult<String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!(
        "{:032x}",
        nanos ^ (u128::from(std::process::id()) << 64)
    ))
}

async fn run_crash_case(kind: StoreKind) -> TestResult {
    let directory = tempfile::tempdir()?;
    let nonce = unique_nonce()?;
    let path = directory.path().join(format!("coordination-{nonce}.db"));
    let ready = directory.path().join(format!("crash-{nonce}.ready"));
    let prior = fixture()?;
    let candidate = race_fixture(&prior)?;
    seed(&path, &prior).await?;

    let mut child = OwnedChild::spawn(&path, &ready, &nonce, kind)?;
    wait_until_ready(&mut child, &ready, &nonce, kind)?;
    child.kill_and_reap()?;

    let recovery = LibsqlCoordination::open_local(&path).await?;
    verify_reads(&recovery, &prior).await?;
    verify_candidate_absent(&recovery, kind, &candidate).await?;
    drop(recovery);
    verify_row_count(&path, kind).await?;

    let read_only = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&read_only, &prior).await?;
    verify_candidate_absent(&read_only, kind, &candidate).await?;
    drop(read_only);

    let retry = LibsqlCoordination::open_local(&path).await?;
    retry_candidate_twice(&retry, kind, &candidate).await?;
    drop(retry);
    let final_read = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_reads(&final_read, &prior).await?;
    verify_candidate_present(&final_read, kind, &candidate).await
}

#[tokio::test]
#[ignore = "invoked only as the bounded SIGKILL recovery child"]
async fn local_coordination_sigkill_child() -> TestResult {
    let (path, ready, nonce, kind) = child_configuration()?;
    hold_uncommitted_candidate(&path, &ready, &nonce, kind).await
}

#[tokio::test]
async fn local_sigkill_recovery_rolls_back_each_current_coordination_store() -> TestResult {
    for kind in StoreKind::ALL {
        run_crash_case(kind).await?;
    }
    Ok(())
}
