#![cfg(feature = "sqlite-backend")]
//! Durability of the local SQLite inventory store: large retained history,
//! conditional publication and rollback of an uncommitted transaction. These
//! tests grant no deployment authority.
use compiler_core::{DeletionPolicy, Lifecycle, Resource, SourceLocation};
use inventory::{
    INVENTORY_SCHEMA, InventoryScope, InventorySnapshot, InventoryStore, MAX_SNAPSHOT_BYTES,
    SqliteInventory, TrackedResource, VersionedInventory, snapshot_id,
};
use rusqlite::{Connection, TransactionBehavior, params};
use std::path::Path;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const PRIVATE: &str = "durability-private-desired-canary";
const LARGE_COUNT: usize = 12_000;
const CRASH_COUNT: usize = 12_001;

fn scope() -> InventoryScope {
    InventoryScope {
        cluster: "durability".into(),
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

async fn required_head(store: &SqliteInventory) -> TestResult<VersionedInventory> {
    store
        .load(&scope())
        .await?
        .ok_or_else(|| "inventory inventory head missing".into())
}

async fn verify_history(store: &SqliteInventory, raced: bool) -> TestResult<VersionedInventory> {
    let head = required_head(store).await?;
    let expected_revision = if raced { "3" } else { "2" };
    if head.revision != expected_revision {
        return Err("inventory inventory revision differs".into());
    }
    for expected in [snapshot("first", LARGE_COUNT), snapshot("second", 1)] {
        let id = snapshot_id(&scope(), &expected)?;
        if store.read_snapshot(&scope(), &id).await? != Some(expected) {
            return Err("inventory retained snapshot differs".into());
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
            return Err("inventory winner or crash rollback differs".into());
        }
    } else if head.snapshot != snapshot("second", 1) {
        return Err("inventory current snapshot differs".into());
    }
    Ok(head)
}

async fn seed(path: &Path) -> TestResult<VersionedInventory> {
    let store = SqliteInventory::open_local(path).await?;
    if store.load(&scope()).await?.is_some() {
        return Err("inventory seed requires an empty scope".into());
    }
    let first = store
        .publish(&scope(), None, &snapshot("first", LARGE_COUNT))
        .await?;
    store
        .publish(&scope(), Some(&first.revision), &snapshot("second", 1))
        .await?;
    verify_history(&store, false).await
}

async fn race(path: &Path, generation: &str) -> TestResult<&'static str> {
    let store = SqliteInventory::open_local(path).await?;
    match store
        .publish(&scope(), Some("2"), &snapshot(generation, 2))
        .await
    {
        Ok(_) => Ok("published"),
        Err(inventory::Error::Conflict) => Ok("conflict"),
        Err(_) => Err("inventory CAS publisher backend failed".into()),
    }
}

// Deliberately exercise OSS SQLite rollback against the current schema. This is
// not an interruption inside InventoryStore::publish or a production fault hook.
fn mutate_uncommitted(connection: &Connection, head: &VersionedInventory) -> TestResult {
    let candidate = snapshot("crash-uncommitted", CRASH_COUNT);
    let candidate_id = snapshot_id(&scope(), &candidate)?;
    let bytes = candidate.encode(&scope())?;
    let mut selected_statement = connection
        .prepare("SELECT scope FROM apsides_inventory_heads WHERE revision=3 AND digest=?1")?;
    let mut selected = selected_statement.query(params![head.snapshot_id.clone()])?;
    let scope_key = selected
        .next()?
        .ok_or("inventory crash source head missing")?
        .get::<usize, String>(0)?;
    if selected.next()?.is_some() {
        return Err("inventory crash source head ambiguous".into());
    }
    drop(selected);
    connection.execute(
        "INSERT INTO apsides_inventory_snapshots(scope,digest,metadata) VALUES (?1,?2,?3)",
        params![scope_key, candidate_id, bytes],
    )?;
    let count = connection.execute(
        "UPDATE apsides_inventory_heads SET revision=4,digest=?1 WHERE scope=?2 AND revision=3 AND digest=?3",
        params![candidate_id, scope_key, head.snapshot_id],
    )?;
    if count != 1 {
        return Err("inventory crash mutation did not select exact head".into());
    }
    let mut statement = connection.prepare(
        "SELECT h.revision,h.digest,s.metadata FROM apsides_inventory_heads h JOIN apsides_inventory_snapshots s ON h.scope=s.scope AND h.digest=s.digest WHERE h.scope=?1",
    )?;
    let mut rows = statement.query(params![scope_key])?;
    let row = rows
        .next()?
        .ok_or("inventory crash transaction head missing")?;
    if row.get::<usize, i64>(0)? != 4
        || row.get::<usize, String>(1)? != candidate_id
        || row.get::<usize, Vec<u8>>(2)? != bytes
    {
        return Err("inventory crash transaction mutation not observed".into());
    }
    Ok(())
}

fn pragma_integer(connection: &Connection, query: &'static str) -> TestResult<i64> {
    Ok(connection.query_one(query, [], |row| row.get::<usize, i64>(0))?)
}

fn configure_crash(connection: &Connection) -> TestResult {
    connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=8; PRAGMA cache_spill=8;")?;
    let journal =
        connection.query_one("PRAGMA journal_mode", [], |row| row.get::<usize, String>(0))?;
    if journal != "delete"
        || pragma_integer(connection, "PRAGMA synchronous")? != 2
        || pragma_integer(connection, "PRAGMA cache_size")? != 8
        || pragma_integer(connection, "PRAGMA cache_spill")? != 8
    {
        return Err("inventory crash durability profile differs".into());
    }
    Ok(())
}

fn journal_bytes(path: &Path) -> TestResult<u64> {
    let metadata = std::fs::symlink_metadata(path.with_extension("db-journal"))?;
    if !metadata.is_file() || metadata.len() <= 512 {
        return Err("inventory crash requires a populated rollback journal".into());
    }
    Ok(metadata.len())
}

fn ensure(condition: bool, message: &'static str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

#[test]
fn large_snapshot_projection_is_bounded_and_metadata_only() -> TestResult {
    let bytes = snapshot("first", LARGE_COUNT).encode(&scope())?;
    ensure(
        bytes.len() > 1024 * 1024 && bytes.len() <= MAX_SNAPSHOT_BYTES,
        "large inventory fixture is outside its size bounds",
    )?;
    let text = String::from_utf8(bytes)?;
    for forbidden in [
        PRIVATE,
        "private-source-canary",
        "desired",
        "forceConflicts",
    ] {
        ensure(
            !text.contains(forbidden),
            "inventory snapshot retained non-metadata content",
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn history_conditional_publication_and_uncommitted_rollback_survive_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("inventory.db");
    ensure(
        seed(&path).await?.revision == "2",
        "seeded inventory revision differs",
    )?;
    ensure(
        seed(&path).await.is_err(),
        "inventory reseeded a used scope",
    )?;
    verify_history(&SqliteInventory::open_local(&path).await?, false).await?;
    ensure(
        race(&path, "race-left").await? == "published",
        "first conditional publisher did not win",
    )?;
    ensure(
        race(&path, "race-right").await? == "conflict",
        "second conditional publisher did not conflict",
    )?;
    let store = SqliteInventory::open_local(&path).await?;
    let head = verify_history(&store, true).await?;
    let mut connection = rusqlite::Connection::open(&path)?;
    configure_crash(&connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    mutate_uncommitted(&transaction, &head)?;
    ensure(
        journal_bytes(&path)? > 512,
        "rollback journal was not populated",
    )?;
    transaction.rollback()?;
    let recovered = verify_history(&SqliteInventory::open_local(&path).await?, true).await?;
    ensure(
        recovered.snapshot_id == head.snapshot_id,
        "recovered inventory head differs",
    )?;
    ensure(
        SqliteInventory::open_local(&directory.path().join("missing/inventory.db"))
            .await
            .is_err(),
        "inventory opened a database under a missing directory",
    )
}
