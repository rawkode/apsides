#![cfg(feature = "sqlite-backend")]
use compiler_core::DeletionPolicy;
use inventory::*;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn scope() -> InventoryScope {
    InventoryScope {
        cluster: "cluster".into(),
        namespace: "test".into(),
        controller: "controller".into(),
    }
}
fn snapshot(generation: &str, count: usize) -> InventorySnapshot {
    InventorySnapshot {
        schema_version: INVENTORY_SCHEMA.into(),
        generation: generation.into(),
        resources: (0..count)
            .map(|i| TrackedResource {
                identity: ResourceKey {
                    api_group: String::new(),
                    resource: "secrets".into(),
                    namespace: "test".into(),
                    name: format!("resource-{i:06}"),
                },
                api_version: "v1".into(),
                kind: "Secret".into(),
                uid: Some(format!("uid-{i}")),
                deletion_policy: DeletionPolicy::Delete,
            })
            .collect(),
    }
}

#[tokio::test]
async fn large_snapshot_survives_reopen_and_preserves_history() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("inventory.db");
    let first = snapshot("first", 12000);
    assert!(first.encode(&scope())?.len() > 1024 * 1024);
    let stored = {
        let store = SqliteInventory::open_local(&path).await?;
        store.publish(&scope(), None, &first).await?
    };
    let store: Box<dyn InventoryStore> = Box::new(SqliteInventory::open_local(&path).await?);
    assert_eq!(
        store
            .load(&scope())
            .await?
            .ok_or("reopened store has no head")?,
        stored
    );
    let second = snapshot("second", 1);
    let next = store
        .publish(&scope(), Some(&stored.revision), &second)
        .await?;
    assert_eq!(
        store.read_snapshot(&scope(), &stored.snapshot_id).await?,
        Some(first.clone())
    );
    assert_eq!(store.publish(&scope(), None, &second).await?, next);
    assert!(matches!(
        store
            .publish(&scope(), Some(&stored.revision), &snapshot("third", 2))
            .await,
        Err(Error::Conflict)
    ));
    let mut other = scope();
    other.controller = "other".into();
    assert!(store.load(&other).await?.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_publishers_cannot_overwrite_a_changed_head() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("inventory.db");
    let a = SqliteInventory::open_local(&path).await?;
    let b = SqliteInventory::open_local(&path).await?;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let other_barrier = barrier.clone();
    let left = tokio::spawn(async move {
        barrier.wait().await;
        a.publish(&scope(), None, &snapshot("first", 1)).await
    });
    let right = tokio::spawn(async move {
        other_barrier.wait().await;
        b.publish(&scope(), None, &snapshot("second", 2)).await
    });
    let (left, right) = (left.await?, right.await?);
    assert_ne!(left.is_ok(), right.is_ok());
    assert!(matches!(left, Err(Error::Conflict)) || matches!(right, Err(Error::Conflict)));
    Ok(())
}

#[tokio::test]
async fn corruption_rejects_reads_and_idempotent_publication() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("inventory.db");
    let store = SqliteInventory::open_local(&path).await?;
    let first = snapshot("first", 1);
    store.publish(&scope(), None, &first).await?;
    let db = rusqlite::Connection::open(&path)?;
    db.execute(
        "UPDATE apsides_inventory_snapshots SET metadata=x'7b7d'",
        (),
    )?;
    assert!(matches!(store.load(&scope()).await, Err(Error::Invalid(_))));
    assert!(matches!(
        store.publish(&scope(), None, &first).await,
        Err(Error::Invalid(_))
    ));
    Ok(())
}

#[test]
fn projection_excludes_private_values_and_rejects_unknown_fields() -> TestResult {
    let resource = compiler_core::Resource {
        api_resource: None,
        force: false,
        id: "private-id-canary".into(),
        api_version: "v1".into(),
        kind: "Secret".into(),
        name: "credential".into(),
        namespace: "test".into(),
        desired: serde_json::json!({"data":{"password":"private-value-canary"},"metadata":{"annotations":{"private":"annotation-canary"}}}),
        lifecycle: compiler_core::Lifecycle {
            deletion_policy: DeletionPolicy::Orphan,
        },
        source: compiler_core::SourceLocation {
            file: "private-path-canary".into(),
            line: 1,
            column: 1,
        },
        handler_output: false,
    };
    let tracked = TrackedResource::from_resource(&resource, "secrets".into(), Some("uid".into()));
    let serialized = serde_json::to_string(&tracked)?;
    assert!(!serialized.contains("canary"));
    let mut value = serde_json::to_value(tracked)?;
    value["data"] = serde_json::json!({"password":"canary"});
    assert!(serde_json::from_value::<TrackedResource>(value).is_err());
    let mut invalid = snapshot("first", 2);
    invalid.resources[1] = invalid.resources[0].clone();
    assert!(invalid.encode(&scope()).is_err());
    Ok(())
}

#[tokio::test]
async fn unsupported_schema_is_not_migrated_or_partially_initialized() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("inventory.db");
    let connection = rusqlite::Connection::open(&path)?;
    connection.execute_batch("CREATE TABLE apsides_inventory_schema (id INTEGER PRIMARY KEY, version INTEGER NOT NULL); INSERT INTO apsides_inventory_schema VALUES (1,99);")?;
    assert!(matches!(
        SqliteInventory::open_local(&path).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        connection.query_one(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('apsides_inventory_heads','apsides_inventory_snapshots')",
            [],
            |row| row.get::<usize, i64>(0),
        )?,
        0
    );
    Ok(())
}

#[test]
fn tracking_preserves_provider_bindings_and_opaque_generation_ids() {
    let mut scope = scope();
    scope.cluster = "arn:aws:eks:region:account:cluster/example".into();
    let mut inventory = snapshot("Generation_42", 1);
    inventory.resources[0].uid = Some("Uid_42".into());
    assert!(inventory.encode(&scope).is_ok());
    scope.cluster.push('\n');
    assert!(inventory.encode(&scope).is_err());
}
