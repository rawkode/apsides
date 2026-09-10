#![cfg(feature = "libsql-backend")]
use compiler_core::DeletionPolicy;
use inventory::*;
use std::path::Path;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn scope() -> InventoryScope {
    InventoryScope {
        cluster: "cluster".to_owned(),
        namespace: "workload".to_owned(),
        controller: "controller".to_owned(),
    }
}

fn snapshot(generation: &str, name: &str) -> InventorySnapshot {
    InventorySnapshot {
        schema_version: INVENTORY_SCHEMA.to_owned(),
        generation: generation.to_owned(),
        resources: vec![TrackedResource {
            identity: ResourceKey {
                api_group: String::new(),
                resource: "configmaps".to_owned(),
                namespace: "workload".to_owned(),
                name: name.to_owned(),
            },
            api_version: "v1".to_owned(),
            kind: "ConfigMap".to_owned(),
            uid: Some(format!("{name}-uid")),
            deletion_policy: DeletionPolicy::Delete,
        }],
    }
}

async fn table_count(path: &Path, table: &str) -> TestResult<i64> {
    let database = libsql::Builder::new_local(path).build().await?;
    let mut rows = database
        .connect()?
        .query(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
        )
        .await?;
    Ok(rows
        .next()
        .await?
        .ok_or("sqlite table count is missing")?
        .get::<i64>(0)?)
}

#[tokio::test]
async fn read_only_open_reads_exact_head_and_history_without_mutation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("inventory.db");
    let first = snapshot("first", "first");
    let second = snapshot("second", "second");
    let (stored_first, stored_second) = {
        let writable = LibsqlInventory::open_local(&path).await?;
        let stored_first = writable.publish(&scope(), None, &first).await?;
        let stored_second = writable
            .publish(&scope(), Some(&stored_first.revision), &second)
            .await?;
        (stored_first, stored_second)
    };
    let before = std::fs::read(&path)?;
    let read_only = LibsqlInventory::open_local_read_only(&path).await?;
    assert_eq!(read_only.load(&scope()).await?, Some(stored_second));
    assert_eq!(
        read_only
            .read_snapshot(&scope(), &stored_first.snapshot_id)
            .await?,
        Some(first)
    );
    assert!(matches!(
        read_only
            .publish(&scope(), Some("2"), &snapshot("third", "third"))
            .await,
        Err(Error::Invalid("read-only inventory store"))
    ));
    let invalid_scope = InventoryScope {
        cluster: String::new(),
        ..scope()
    };
    let invalid_snapshot = InventorySnapshot {
        schema_version: "private-canary".to_owned(),
        generation: String::new(),
        resources: Vec::new(),
    };
    assert!(matches!(
        read_only
            .publish(&invalid_scope, None, &invalid_snapshot)
            .await,
        Err(Error::Invalid("read-only inventory store"))
    ));
    drop(read_only);
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}

#[tokio::test]
async fn read_only_open_rejects_missing_file_and_tables_without_creation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing.db");
    assert!(
        LibsqlInventory::open_local_read_only(&missing)
            .await
            .is_err()
    );
    assert!(!missing.exists());

    let incomplete = directory.path().join("incomplete.db");
    let database = libsql::Builder::new_local(&incomplete).build().await?;
    database
        .connect()?
        .execute_batch(
            "CREATE TABLE apsides_inventory_schema (id INTEGER PRIMARY KEY, version INTEGER NOT NULL);\
             INSERT INTO apsides_inventory_schema VALUES (1,1);\
             CREATE TABLE apsides_inventory_snapshots (scope TEXT,digest TEXT);",
        )
        .await?;
    let before = std::fs::read(&incomplete)?;
    assert!(
        LibsqlInventory::open_local_read_only(&incomplete)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&incomplete)?, before);
    assert_eq!(table_count(&incomplete, "apsides_inventory_heads").await?, 0);
    Ok(())
}

#[tokio::test]
async fn read_only_open_rejects_wrong_version_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("wrong-version.db");
    LibsqlInventory::open_local(&path).await?;
    let database = libsql::Builder::new_local(&path).build().await?;
    database
        .connect()?
        .execute("UPDATE apsides_inventory_schema SET version=2 WHERE id=1", ())
        .await?;
    let before = std::fs::read(&path)?;
    assert!(matches!(
        LibsqlInventory::open_local_read_only(&path).await,
        Err(Error::Invalid("unsupported inventory schema"))
    ));
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}

#[tokio::test]
async fn read_only_load_rejects_corrupt_metadata_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("corrupt.db");
    {
        let writable = LibsqlInventory::open_local(&path).await?;
        writable
            .publish(&scope(), None, &snapshot("first", "first"))
            .await?;
    }
    let database = libsql::Builder::new_local(&path).build().await?;
    database
        .connect()?
        .execute("UPDATE apsides_inventory_snapshots SET metadata=x'7b7d'", ())
        .await?;
    drop(database);
    let before = std::fs::read(&path)?;
    let read_only = LibsqlInventory::open_local_read_only(&path).await?;
    assert!(matches!(
        read_only.load(&scope()).await,
        Err(Error::Invalid("snapshot shape"))
    ));
    drop(read_only);
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}
