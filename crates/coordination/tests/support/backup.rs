use super::*;
use compiler_core::DeletionPolicy;
use inventory::{
    InventoryScope, InventorySnapshot, InventoryStore, LibsqlInventory, ResourceKey,
    TrackedResource,
};
use libsql::{Builder, OpenFlags, params};
use std::{fs::File, path::Path};

fn inventory_scope() -> InventoryScope {
    InventoryScope {
        cluster: "storage-cluster".to_owned(),
        namespace: "storage-workload".to_owned(),
        controller: "storage".to_owned(),
    }
}

fn inventory_snapshot(generation: &str, name: &str) -> InventorySnapshot {
    InventorySnapshot {
        schema_version: inventory::INVENTORY_SCHEMA.to_owned(),
        generation: generation.to_owned(),
        resources: vec![TrackedResource {
            identity: ResourceKey {
                api_group: String::new(),
                resource: "configmaps".to_owned(),
                namespace: "storage-workload".to_owned(),
                name: name.to_owned(),
            },
            api_version: "v1".to_owned(),
            kind: "ConfigMap".to_owned(),
            uid: Some(format!("{name}-uid")),
            deletion_policy: DeletionPolicy::Delete,
        }],
    }
}

async fn vacuum_into(source: &Path, destination: &Path) -> TestResult {
    if !source.is_absolute() || !destination.is_absolute() {
        return Err("coordination backup paths must be absolute".into());
    }
    let source_metadata = source.symlink_metadata()?;
    if !source_metadata.file_type().is_file() || source_metadata.file_type().is_symlink() {
        return Err("coordination backup source must be a regular file".into());
    }
    match destination.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Err("coordination backup destination must be absent".into()),
    }
    let destination_text = destination
        .to_str()
        .ok_or("coordination backup destination must be UTF-8")?;
    let database = Builder::new_local(source)
        .flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
        .build()
        .await?;
    let connection = database.connect()?;
    connection
        .execute("VACUUM INTO ?1", params![destination_text])
        .await?;
    drop(connection);
    drop(database);
    let backup = File::open(destination)?;
    backup.sync_all()?;
    drop(backup);
    let parent = destination
        .parent()
        .ok_or("coordination backup destination has no parent")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

async fn seed_inventory(
    path: &Path,
) -> TestResult<(
    InventorySnapshot,
    InventorySnapshot,
    inventory::VersionedInventory,
)> {
    let scope = inventory_scope();
    let first = inventory_snapshot("inventory-first", "inventory-first");
    let second = inventory_snapshot("inventory-second", "inventory-second");
    let store = LibsqlInventory::open_local(path).await?;
    let first_version = store.publish(&scope, None, &first).await?;
    let second_version = store
        .publish(&scope, Some(&first_version.revision), &second)
        .await?;
    if store
        .read_snapshot(&scope, &first_version.snapshot_id)
        .await?
        != Some(first.clone())
    {
        return Err("coordination backup inventory history differs".into());
    }
    Ok((first, second, second_version))
}

async fn verify_inventory_backup(
    path: &Path,
    first: &InventorySnapshot,
    second: &InventorySnapshot,
    second_version: &inventory::VersionedInventory,
) -> TestResult {
    let scope = inventory_scope();
    let store = LibsqlInventory::open_local_read_only(path).await?;
    if store.load(&scope).await? != Some(second_version.clone())
        || store
            .read_snapshot(&scope, &second_version.snapshot_id)
            .await?
            != Some(second.clone())
    {
        return Err("coordination backup inventory head differs".into());
    }
    let first_id = inventory::snapshot_id(&scope, first)?;
    if store.read_snapshot(&scope, &first_id).await? != Some(first.clone()) {
        return Err("coordination backup inventory history is missing".into());
    }
    if !matches!(
        store
            .publish(&scope, Some(&second_version.revision), second)
            .await,
        Err(inventory::Error::Invalid("read-only inventory store"))
    ) {
        return Err("read-only backup inventory write was not rejected".into());
    }
    Ok(())
}

#[tokio::test]
async fn local_vacuum_backup_reopens_all_four_stores_and_isolates_later_source_writes() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("all-stores-source.db");
    let backup = directory.path().join("all-stores-'backup'.db");

    let seed_fixture = fixture()?;
    seed(&source, &seed_fixture).await?;
    let (first_inventory, second_inventory, second_version) = seed_inventory(&source).await?;

    LibsqlCoordination::open_local_read_only(&source).await?;
    LibsqlInventory::open_local_read_only(&source).await?;
    vacuum_into(&source, &backup).await?;

    let race = race_fixture(&seed_fixture)?;
    let source_coordination = LibsqlCoordination::open_local(&source).await?;
    if source_coordination.put_claims(&race.claims).await? != race.claim_ref
        || source_coordination.put_review(&race.left_review).await? != race.left_review_ref
        || source_coordination.put_audit(&race.left_audit).await? != race.left_audit_ref
    {
        return Err("coordination backup source mutation differs".into());
    }
    let source_inventory = LibsqlInventory::open_local(&source).await?;
    let third_inventory = inventory_snapshot("inventory-third", "inventory-third");
    let third_version = source_inventory
        .publish(
            &inventory_scope(),
            Some(&second_version.revision),
            &third_inventory,
        )
        .await?;
    if source_inventory.load(&inventory_scope()).await? != Some(third_version) {
        return Err("coordination backup inventory source mutation differs".into());
    }

    let backup_coordination = LibsqlCoordination::open_local_read_only(&backup).await?;
    verify_reads(&backup_coordination, &seed_fixture).await?;
    if backup_coordination
        .read_claims(&race.claim_ref)
        .await?
        .is_some()
        || backup_coordination
            .read_review(&race.left_review_ref)
            .await?
            .is_some()
        || backup_coordination
            .read_audit(&race.left_audit_ref)
            .await?
            .is_some()
    {
        return Err("coordination backup contains later source metadata".into());
    }
    if !matches!(
        backup_coordination.put_claims(&seed_fixture.claims).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        backup_coordination.put_review(&seed_fixture.review).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        backup_coordination.put_audit(&seed_fixture.audit).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) {
        return Err("read-only coordination backup write was not rejected".into());
    }
    verify_inventory_backup(
        &backup,
        &first_inventory,
        &second_inventory,
        &second_version,
    )
    .await
}

#[tokio::test]
async fn local_vacuum_backup_rejects_a_nonempty_destination_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("coordination-source.db");
    let destination = directory.path().join("existing.db");
    seed(&source, &fixture()?).await?;
    let original = b"existing-backup-canary";
    std::fs::write(&destination, original)?;
    if vacuum_into(&source, &destination).await.is_ok() || std::fs::read(&destination)? != original
    {
        return Err("coordination backup replaced a nonempty destination".into());
    }
    Ok(())
}

#[tokio::test]
async fn read_only_backup_open_rejects_missing_and_truncated_files_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing.db");
    if LibsqlCoordination::open_local_read_only(&missing)
        .await
        .is_ok()
        || missing.exists()
        || LibsqlInventory::open_local_read_only(&missing)
            .await
            .is_ok()
        || missing.exists()
    {
        return Err("read-only backup open created a missing database".into());
    }

    let truncated_coordination = directory.path().join("truncated-coordination.db");
    let truncated_inventory = directory.path().join("truncated-inventory.db");
    let truncated = b"SQLite format 3\0truncated";
    std::fs::write(&truncated_coordination, truncated)?;
    std::fs::write(&truncated_inventory, truncated)?;
    if LibsqlCoordination::open_local_read_only(&truncated_coordination)
        .await
        .is_ok()
        || std::fs::read(&truncated_coordination)? != truncated
        || LibsqlInventory::open_local_read_only(&truncated_inventory)
            .await
            .is_ok()
        || std::fs::read(&truncated_inventory)? != truncated
    {
        return Err("read-only backup open repaired a truncated database".into());
    }

    let missing_schema = directory.path().join("missing-schema.db");
    let database = Builder::new_local(&missing_schema).build().await?;
    database
        .connect()?
        .execute("CREATE TABLE unrelated(value TEXT)", ())
        .await?;
    drop(database);
    let missing_schema_bytes = std::fs::read(&missing_schema)?;
    if LibsqlCoordination::open_local_read_only(&missing_schema)
        .await
        .is_ok()
        || std::fs::read(&missing_schema)? != missing_schema_bytes
        || LibsqlInventory::open_local_read_only(&missing_schema)
            .await
            .is_ok()
        || std::fs::read(&missing_schema)? != missing_schema_bytes
    {
        return Err("read-only backup open repaired a database without a schema".into());
    }
    Ok(())
}
