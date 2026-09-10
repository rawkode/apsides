#![cfg(feature = "libsql-backend")]
//! Explicit disposable-PVC probe. This is storage evidence, not deployment authority.
use compiler_core::{DeletionPolicy, Lifecycle, Resource, SourceLocation};
use inventory::{
    INVENTORY_SCHEMA, InventoryScope, InventorySnapshot, InventoryStore, LibsqlInventory,
    MAX_SNAPSHOT_BYTES, TrackedResource, VersionedInventory, snapshot_id,
};
use libsql::{Builder, Connection, TransactionBehavior, params};
use serde::Serialize;
use std::{
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const PRIVATE: &str = "pvc-private-desired-canary";
const LIVE_PATH: &str = "/store/inventory.db";
const LARGE_COUNT: usize = 12_000;
const CRASH_COUNT: usize = 12_001;
const TEST_NONCE: &str = "0123456789abcdef0123456789abcdef";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Seed,
    Verify,
    RaceLeft,
    RaceRight,
    CrashReady,
    Recover,
    Unavailable,
}
impl Phase {
    fn parse(value: &str) -> TestResult<Self> {
        match value {
            "seed" => Ok(Self::Seed),
            "verify" => Ok(Self::Verify),
            "race-left" => Ok(Self::RaceLeft),
            "race-right" => Ok(Self::RaceRight),
            "crash-ready" => Ok(Self::CrashReady),
            "recover" => Ok(Self::Recover),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err("unsupported PVC probe phase".into()),
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Verify => "verify",
            Self::RaceLeft => "race-left",
            Self::RaceRight => "race-right",
            Self::CrashReady => "crash-ready",
            Self::Recover => "recover",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    schema_version: &'static str,
    phase: &'static str,
    outcome: &'static str,
    revision: Option<String>,
    snapshot_id: Option<String>,
    first_snapshot_id: String,
    second_snapshot_id: String,
    large_snapshot_bytes: usize,
    journal_bytes: Option<u64>,
}

fn scope() -> InventoryScope {
    InventoryScope {
        cluster: "pvc-qualification".into(),
        namespace: "storage-test".into(),
        controller: "inventory-probe".into(),
    }
}

fn snapshot(generation: &str, count: usize) -> InventorySnapshot {
    let resources = (0..count).map(|index| {
        let name = format!("resource-{index:06}");
        let resource = Resource {
            api_resource: None,
            id: name.clone(), api_version: "v1".into(), kind: "ConfigMap".into(),
            name: name.clone(), namespace: scope().namespace,
            desired: serde_json::json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"storage-test"},"data":{"value":PRIVATE}}),
            lifecycle: Lifecycle { deletion_policy: DeletionPolicy::Delete }, force: false,
            source: SourceLocation { file: "private-source-canary.tsx".into(), line: 1, column: 1 },
        };
        TrackedResource::from_resource(&resource, "configmaps".into(), Some(format!("uid-{index}")))
    }).collect();
    InventorySnapshot {
        schema_version: INVENTORY_SCHEMA.into(),
        generation: generation.into(),
        resources,
    }
}

fn report(
    phase: Phase,
    outcome: &'static str,
    head: Option<&VersionedInventory>,
) -> TestResult<Report> {
    let first = snapshot("first", LARGE_COUNT);
    let bytes = first.encode(&scope())?;
    if bytes.len() <= 1024 * 1024
        || bytes.len() > MAX_SNAPSHOT_BYTES
        || bytes
            .windows(PRIVATE.len())
            .any(|part| part == PRIVATE.as_bytes())
    {
        return Err("large metadata fixture size or privacy invariant failed".into());
    }
    Ok(Report {
        schema_version: "apsides.dev/pvc-inventory-probe/v1",
        phase: phase.name(),
        outcome,
        revision: head.map(|value| value.revision.clone()),
        snapshot_id: head.map(|value| value.snapshot_id.clone()),
        first_snapshot_id: snapshot_id(&scope(), &first)?,
        second_snapshot_id: snapshot_id(&scope(), &snapshot("second", 1))?,
        large_snapshot_bytes: bytes.len(),
        journal_bytes: None,
    })
}

#[derive(Serialize)]
struct Message<'a> {
    nonce: &'a str,
    #[serde(flatten)]
    report: &'a Report,
}
fn validate_nonce(nonce: &str) -> TestResult {
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("PVC probe requires a lowercase 32-hex nonce".into());
    }
    Ok(())
}
fn encoded_report(report: &Report, nonce: &str) -> TestResult<Vec<u8>> {
    validate_nonce(nonce)?;
    let bytes = serde_json::to_vec(&Message { nonce, report })?;
    if bytes.len() > 4096 {
        return Err("PVC probe report exceeds its bound".into());
    }
    Ok(bytes)
}
fn emit(prefix: &str, report: &Report, nonce: &str) -> TestResult {
    let bytes = encoded_report(report, nonce)?;
    let mut output = std::io::stdout().lock();
    output.write_all(prefix.as_bytes())?;
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

async fn required_head(store: &LibsqlInventory) -> TestResult<VersionedInventory> {
    store
        .load(&scope())
        .await?
        .ok_or_else(|| "PVC inventory head missing".into())
}

async fn verify_history(store: &LibsqlInventory, raced: bool) -> TestResult<VersionedInventory> {
    let head = required_head(store).await?;
    let expected_revision = if raced { "3" } else { "2" };
    if head.revision != expected_revision {
        return Err("PVC inventory revision differs".into());
    }
    for expected in [snapshot("first", LARGE_COUNT), snapshot("second", 1)] {
        let id = snapshot_id(&scope(), &expected)?;
        if store.read_snapshot(&scope(), &id).await? != Some(expected) {
            return Err("PVC retained snapshot differs".into());
        }
    }
    if raced {
        let winner = head.snapshot.generation.as_str();
        if !matches!(winner, "race-left" | "race-right")
            || head.snapshot != snapshot(winner, 2)
            || store.read_snapshot(&scope(), &head.snapshot_id).await?
                != Some(head.snapshot.clone())
            || store
                .read_snapshot(
                    &scope(),
                    &snapshot_id(&scope(), &snapshot("crash-uncommitted", CRASH_COUNT))?,
                )
                .await?
                .is_some()
        {
            return Err("PVC winner or crash rollback differs".into());
        }
    } else if head.snapshot != snapshot("second", 1) {
        return Err("PVC current snapshot differs".into());
    }
    Ok(head)
}

async fn seed(path: &Path) -> TestResult<Report> {
    let store = LibsqlInventory::open_local(path).await?;
    if store.load(&scope()).await?.is_some() {
        return Err("PVC seed requires an empty scope".into());
    }
    let first = store
        .publish(&scope(), None, &snapshot("first", LARGE_COUNT))
        .await?;
    store
        .publish(&scope(), Some(&first.revision), &snapshot("second", 1))
        .await?;
    let head = verify_history(&store, false).await?;
    report(Phase::Seed, "published", Some(&head))
}

async fn race(path: &Path, phase: Phase, wait: bool, nonce: &str) -> TestResult<Report> {
    let store = LibsqlInventory::open_local(path).await?;
    if wait {
        let barrier = path.parent().ok_or("PVC parent missing")?.join("race-go");
        if barrier.exists() {
            return Err("PVC CAS barrier must initially be absent".into());
        }
        let head = verify_history(&store, false).await?;
        emit(
            "APS_STORAGE_READY=",
            &report(phase, "ready", Some(&head))?,
            nonce,
        )?;
        let deadline = Instant::now() + Duration::from_secs(90);
        while !barrier.is_file() {
            if Instant::now() >= deadline {
                return Err("PVC CAS barrier deadline exceeded".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    match store
        .publish(&scope(), Some("2"), &snapshot(phase.name(), 2))
        .await
    {
        Ok(head) => report(phase, "published", Some(&head)),
        Err(inventory::Error::Conflict) => {
            report(phase, "conflict", Some(&required_head(&store).await?))
        }
        Err(_) => Err("PVC CAS publisher backend failed".into()),
    }
}

// Deliberately exercise OSS SQLite rollback against the current schema. This is
// not an interruption inside InventoryStore::publish or a production fault hook.
async fn mutate_uncommitted(connection: &Connection, head: &VersionedInventory) -> TestResult {
    let candidate = snapshot("crash-uncommitted", CRASH_COUNT);
    let candidate_id = snapshot_id(&scope(), &candidate)?;
    let bytes = candidate.encode(&scope())?;
    let mut selected = connection
        .query(
            "SELECT scope FROM apsides_inventory_heads WHERE revision=3 AND digest=?1",
            params![head.snapshot_id.clone()],
        )
        .await?;
    let scope_key = selected
        .next()
        .await?
        .ok_or("PVC crash source head missing")?
        .get::<String>(0)?;
    if selected.next().await?.is_some() {
        return Err("PVC crash source head ambiguous".into());
    }
    drop(selected);
    connection
        .execute(
            "INSERT INTO apsides_inventory_snapshots(scope,digest,metadata) VALUES (?1,?2,?3)",
            params![scope_key.clone(), candidate_id.clone(), bytes.clone()],
        )
        .await?;
    let count = connection.execute(
        "UPDATE apsides_inventory_heads SET revision=4,digest=?1 WHERE scope=?2 AND revision=3 AND digest=?3",
        params![candidate_id.clone(), scope_key.clone(), head.snapshot_id.clone()],
    ).await?;
    if count != 1 {
        return Err("PVC crash mutation did not select exact head".into());
    }
    let mut rows = connection.query(
        "SELECT h.revision,h.digest,s.metadata FROM apsides_inventory_heads h JOIN apsides_inventory_snapshots s ON h.scope=s.scope AND h.digest=s.digest WHERE h.scope=?1",
        params![scope_key],
    ).await?;
    let row = rows
        .next()
        .await?
        .ok_or("PVC crash transaction head missing")?;
    if row.get::<i64>(0)? != 4
        || row.get::<String>(1)? != candidate_id
        || row.get::<Vec<u8>>(2)? != bytes
    {
        return Err("PVC crash transaction mutation not observed".into());
    }
    Ok(())
}

async fn pragma_integer(connection: &Connection, query: &'static str) -> TestResult<i64> {
    let mut rows = connection.query(query, ()).await?;
    let row = rows.next().await?.ok_or("PVC durability setting missing")?;
    Ok(row.get::<i64>(0)?)
}

async fn configure_crash(connection: &Connection) -> TestResult {
    connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=8; PRAGMA cache_spill=8;").await?;
    let mut rows = connection.query("PRAGMA journal_mode", ()).await?;
    let row = rows.next().await?.ok_or("PVC journal setting missing")?;
    if row.get::<String>(0)? != "delete"
        || pragma_integer(connection, "PRAGMA synchronous").await? != 2
        || pragma_integer(connection, "PRAGMA cache_size").await? != 8
        || pragma_integer(connection, "PRAGMA cache_spill").await? != 8
    {
        return Err("PVC crash durability profile differs".into());
    }
    Ok(())
}

fn journal_bytes(path: &Path) -> TestResult<u64> {
    let metadata = std::fs::symlink_metadata(path.with_extension("db-journal"))?;
    if !metadata.is_file() || metadata.len() <= 512 {
        return Err("PVC crash requires a populated rollback journal".into());
    }
    Ok(metadata.len())
}

async fn crash_ready(path: &Path, nonce: &str) -> TestResult<Report> {
    let store = LibsqlInventory::open_local(path).await?;
    let head = verify_history(&store, true).await?;
    let database = Builder::new_local(path).build().await?;
    let connection = database.connect()?;
    configure_crash(&connection).await?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    mutate_uncommitted(&transaction, &head).await?;
    let mut checkpoint = report(Phase::CrashReady, "uncommitted", Some(&head))?;
    checkpoint.journal_bytes = Some(journal_bytes(path)?);
    emit("APS_STORAGE_CRASH_READY=", &checkpoint, nonce)?;
    std::future::pending::<()>().await;
    // Retain the live transaction across the park; only process termination ends it.
    transaction.rollback().await?;
    Err("PVC crash checkpoint unexpectedly resumed".into())
}

async fn execute(path: &Path, phase: Phase, nonce: &str) -> TestResult<Report> {
    match phase {
        Phase::Seed => seed(path).await,
        Phase::RaceLeft | Phase::RaceRight => race(path, phase, true, nonce).await,
        Phase::CrashReady => crash_ready(path, nonce).await,
        Phase::Unavailable => {
            if LibsqlInventory::open_local(path).await.is_ok() {
                return Err("unavailable PVC unexpectedly opened".into());
            }
            report(phase, "rejected", None)
        }
        Phase::Verify | Phase::Recover => {
            let store = LibsqlInventory::open_local(path).await?;
            let head = verify_history(&store, phase == Phase::Recover).await?;
            report(phase, "verified", Some(&head))
        }
    }
}

fn live_configuration(phase: &str, path: &str, nonce: &str) -> TestResult<Phase> {
    validate_nonce(nonce)?;
    if path != LIVE_PATH {
        return Err("PVC probe requires its exact mounted database path".into());
    }
    Phase::parse(phase)
}

#[tokio::test]
#[ignore = "requires explicitly provisioned disposable Kubernetes PVC"]
async fn pvc_inventory_phase() -> TestResult {
    let phase = std::env::var("APS_STORAGE_TEST_PHASE").map_err(|_| "PVC phase is required")?;
    let path = std::env::var("APS_STORAGE_TEST_PATH").map_err(|_| "PVC path is required")?;
    let nonce = std::env::var("APS_STORAGE_TEST_NONCE").map_err(|_| "PVC nonce is required")?;
    let phase = live_configuration(&phase, &path, &nonce)?;
    let result = execute(Path::new(&path), phase, &nonce)
        .await
        .map_err(|_| "PVC probe failed")?;
    emit("APS_STORAGE_RESULT=", &result, &nonce)
}

#[test]
fn fixture_projection_and_protocol_are_bounded_and_metadata_only() -> TestResult {
    let metadata = snapshot("first", LARGE_COUNT);
    let bytes = metadata.encode(&scope())?;
    assert!(bytes.len() > 1024 * 1024);
    let text = String::from_utf8(bytes)?;
    for forbidden in [
        PRIVATE,
        "private-source-canary",
        "desired",
        "forceConflicts",
    ] {
        assert!(!text.contains(forbidden));
    }
    let encoded = encoded_report(&report(Phase::Seed, "published", None)?, TEST_NONCE)?;
    assert!(encoded.len() < 4096);
    for nonce in ["", "ABCDEF0123456789ABCDEF0123456789", "../../private"] {
        assert!(live_configuration("seed", LIVE_PATH, nonce).is_err());
    }
    assert!(live_configuration("seed", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("crash-ready", LIVE_PATH, TEST_NONCE).is_ok());
    for phase in ["", "crash", "anything"] {
        assert!(live_configuration(phase, LIVE_PATH, TEST_NONCE).is_err());
    }
    for path in [
        "inventory.db",
        "/tmp/inventory.db",
        "/store/../inventory.db",
    ] {
        assert!(live_configuration("seed", path, TEST_NONCE).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn local_probe_preserves_history_cas_and_uncommitted_rollback() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("inventory.db");
    assert_eq!(seed(&path).await?.revision.as_deref(), Some("2"));
    assert!(seed(&path).await.is_err());
    assert_eq!(
        execute(&path, Phase::Verify, TEST_NONCE).await?.outcome,
        "verified"
    );
    assert_eq!(
        race(&path, Phase::RaceLeft, false, TEST_NONCE)
            .await?
            .outcome,
        "published"
    );
    assert_eq!(
        race(&path, Phase::RaceRight, false, TEST_NONCE)
            .await?
            .outcome,
        "conflict"
    );
    let store = LibsqlInventory::open_local(&path).await?;
    let head = verify_history(&store, true).await?;
    let database = Builder::new_local(&path).build().await?;
    let connection = database.connect()?;
    configure_crash(&connection).await?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await?;
    mutate_uncommitted(&transaction, &head).await?;
    assert!(journal_bytes(&path)? > 512);
    transaction.rollback().await?;
    assert_eq!(
        execute(&path, Phase::Recover, TEST_NONCE)
            .await?
            .snapshot_id
            .as_deref(),
        Some(head.snapshot_id.as_str())
    );
    let missing_parent = directory.path().join("missing/inventory.db");
    assert_eq!(
        execute(&missing_parent, Phase::Unavailable, TEST_NONCE)
            .await?
            .outcome,
        "rejected"
    );
    Ok(())
}
