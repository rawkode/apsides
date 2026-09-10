use super::*;
use compiler_core::DeletionPolicy;
use inventory::{
    InventoryScope, InventorySnapshot, InventoryStore, ResourceKey, SqliteInventory,
    TrackedResource, VersionedInventory,
};
use rusqlite::params;
use serde::Serialize;
use std::{fs::File, path::Path};

const LIVE_BACKUP_PATH: &str = "/backup/coordination.db";

struct BackupFixture {
    seed: Fixture,
    later: RaceFixture,
    first_inventory: InventorySnapshot,
    second_inventory: InventorySnapshot,
    third_inventory: InventorySnapshot,
    first_inventory_id: String,
    second_inventory_version: VersionedInventory,
    third_inventory_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupReport<'a> {
    schema_version: &'static str,
    nonce: &'a str,
    phase: &'static str,
    outcome: &'static str,
    seed_claim_digest: &'a str,
    seed_review_digest: &'a str,
    seed_audit_digest: &'a str,
    first_inventory_snapshot_id: &'a str,
    second_inventory_snapshot_id: &'a str,
    expected_inventory_revision: &'static str,
    observed_inventory_revision: &'static str,
    later_claim_digest: &'a str,
    later_review_digest: &'a str,
    later_audit_digest: &'a str,
    third_inventory_snapshot_id: &'a str,
}

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

fn backup_fixture() -> TestResult<BackupFixture> {
    let seed = fixture()?;
    let later = race_fixture(&seed)?;
    let scope = inventory_scope();
    let first_inventory = inventory_snapshot("inventory-first", "inventory-first");
    let second_inventory = inventory_snapshot("inventory-second", "inventory-second");
    let third_inventory = inventory_snapshot("inventory-third", "inventory-third");
    let first_inventory_id = inventory::snapshot_id(&scope, &first_inventory)?;
    let second_inventory_id = inventory::snapshot_id(&scope, &second_inventory)?;
    let third_inventory_id = inventory::snapshot_id(&scope, &third_inventory)?;
    Ok(BackupFixture {
        seed,
        later,
        first_inventory,
        second_inventory: second_inventory.clone(),
        third_inventory,
        first_inventory_id,
        second_inventory_version: VersionedInventory {
            revision: "2".to_owned(),
            snapshot_id: second_inventory_id,
            snapshot: second_inventory,
        },
        third_inventory_id,
    })
}

fn vacuum_into(source: &Path, destination: &Path) -> TestResult {
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
    let connection =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.execute("VACUUM INTO ?1", params![destination_text])?;
    drop(connection);
    let backup = File::open(destination)?;
    backup.sync_all()?;
    drop(backup);
    let parent = destination
        .parent()
        .ok_or("coordination backup destination has no parent")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

async fn seed_all(path: &Path, expected: &BackupFixture) -> TestResult {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Err("coordination backup source must initially be absent".into()),
    }
    seed(path, &expected.seed).await?;
    let scope = inventory_scope();
    let store = SqliteInventory::open_local(path).await?;
    let first = store
        .publish(&scope, None, &expected.first_inventory)
        .await?;
    if first.revision != "1" || first.snapshot_id != expected.first_inventory_id {
        return Err("coordination backup first inventory reference differs".into());
    }
    let second = store
        .publish(&scope, Some(&first.revision), &expected.second_inventory)
        .await?;
    if second != expected.second_inventory_version {
        return Err("coordination backup second inventory reference differs".into());
    }
    verify_seed(path, expected).await
}

async fn verify_seed(path: &Path, expected: &BackupFixture) -> TestResult {
    let coordination = SqliteCoordination::open_local_read_only(path).await?;
    verify_reads(&coordination, &expected.seed).await?;
    if coordination
        .read_claims(&expected.later.claim_ref)
        .await?
        .is_some()
        || coordination
            .read_review(&expected.later.left_review_ref)
            .await?
            .is_some()
        || coordination
            .read_review(&expected.later.right_review_ref)
            .await?
            .is_some()
        || coordination
            .read_audit(&expected.later.left_audit_ref)
            .await?
            .is_some()
        || coordination
            .read_audit(&expected.later.right_audit_ref)
            .await?
            .is_some()
    {
        return Err("coordination backup contains later coordination metadata".into());
    }
    let inventory = SqliteInventory::open_local_read_only(path).await?;
    let scope = inventory_scope();
    if inventory.load(&scope).await? != Some(expected.second_inventory_version.clone())
        || inventory
            .read_snapshot(&scope, &expected.first_inventory_id)
            .await?
            != Some(expected.first_inventory.clone())
        || inventory
            .read_snapshot(&scope, &expected.second_inventory_version.snapshot_id)
            .await?
            != Some(expected.second_inventory.clone())
        || inventory
            .read_snapshot(&scope, &expected.third_inventory_id)
            .await?
            .is_some()
    {
        return Err("coordination backup inventory metadata differs".into());
    }
    Ok(())
}

async fn mutate_source(path: &Path, expected: &BackupFixture) -> TestResult {
    verify_seed(path, expected).await?;
    let coordination = SqliteCoordination::open_local(path).await?;
    if coordination.put_claims(&expected.later.claims).await? != expected.later.claim_ref
        || coordination.put_review(&expected.later.left_review).await?
            != expected.later.left_review_ref
        || coordination.put_audit(&expected.later.left_audit).await?
            != expected.later.left_audit_ref
    {
        return Err("coordination backup source mutation differs".into());
    }
    let inventory = SqliteInventory::open_local(path).await?;
    let third = inventory
        .publish(
            &inventory_scope(),
            Some(&expected.second_inventory_version.revision),
            &expected.third_inventory,
        )
        .await?;
    if third.revision != "3" || third.snapshot_id != expected.third_inventory_id {
        return Err("coordination backup inventory source mutation differs".into());
    }
    Ok(())
}

async fn verify_backup(path: &Path, expected: &BackupFixture) -> TestResult {
    verify_seed(path, expected).await?;
    let coordination = SqliteCoordination::open_local_read_only(path).await?;
    if !matches!(
        coordination.put_claims(&expected.seed.claims).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        coordination.put_review(&expected.seed.review).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        coordination.put_audit(&expected.seed.audit).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) {
        return Err("read-only coordination backup write was not rejected".into());
    }
    let inventory = SqliteInventory::open_local_read_only(path).await?;
    if !matches!(
        inventory
            .publish(
                &inventory_scope(),
                Some(&expected.second_inventory_version.revision),
                &expected.second_inventory,
            )
            .await,
        Err(inventory::Error::Invalid("read-only inventory store"))
    ) {
        return Err("read-only backup inventory write was not rejected".into());
    }
    Ok(())
}

fn emit_backup(
    phase: Phase,
    outcome: &'static str,
    observed_inventory_revision: &'static str,
    nonce: &str,
    expected: &BackupFixture,
) -> TestResult {
    validate_nonce(nonce)?;
    write_report(
        b"APS_COORDINATION_BACKUP_RESULT=",
        &BackupReport {
            schema_version: "apsides.dev/pvc-coordination-backup/v1",
            nonce,
            phase: phase.name(),
            outcome,
            seed_claim_digest: &expected.seed.claim_ref.digest,
            seed_review_digest: &expected.seed.review_ref.digest,
            seed_audit_digest: &expected.seed.audit_ref.digest,
            first_inventory_snapshot_id: &expected.first_inventory_id,
            second_inventory_snapshot_id: &expected.second_inventory_version.snapshot_id,
            expected_inventory_revision: "2",
            observed_inventory_revision,
            later_claim_digest: &expected.later.claim_ref.digest,
            later_review_digest: &expected.later.left_review_ref.digest,
            later_audit_digest: &expected.later.left_audit_ref.digest,
            third_inventory_snapshot_id: &expected.third_inventory_id,
        },
    )
}

pub(super) async fn execute_live(phase: Phase, nonce: &str) -> TestResult {
    let expected = backup_fixture()?;
    let source = Path::new(LIVE_PATH);
    match phase {
        Phase::BackupSeed => {
            seed_all(source, &expected).await?;
            emit_backup(phase, "seeded", "2", nonce, &expected)
        }
        Phase::BackupCreate => {
            verify_seed(source, &expected).await?;
            let destination = Path::new(LIVE_BACKUP_PATH);
            vacuum_into(source, destination)?;
            verify_seed(destination, &expected).await?;
            emit_backup(phase, "created", "2", nonce, &expected)
        }
        Phase::BackupMutate => {
            mutate_source(source, &expected).await?;
            emit_backup(phase, "mutated", "3", nonce, &expected)
        }
        Phase::BackupVerify => {
            // The verifier Pod mounts only the backup PVC at the fixed source path.
            verify_backup(source, &expected).await?;
            emit_backup(phase, "verified", "2", nonce, &expected)
        }
        _ => Err("coordination backup phase is invalid".into()),
    }
}

#[tokio::test]
async fn local_vacuum_backup_reopens_all_four_stores_and_isolates_later_source_writes() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("all-stores-source.db");
    let destination = directory.path().join("all-stores-'backup'.db");
    let expected = backup_fixture()?;
    seed_all(&source, &expected).await?;
    vacuum_into(&source, &destination)?;
    mutate_source(&source, &expected).await?;
    verify_backup(&destination, &expected).await
}

#[tokio::test]
async fn local_vacuum_backup_rejects_a_nonempty_destination_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("coordination-source.db");
    let destination = directory.path().join("existing.db");
    seed_all(&source, &backup_fixture()?).await?;
    let original = b"existing-backup-canary";
    std::fs::write(&destination, original)?;
    if vacuum_into(&source, &destination).is_ok() || std::fs::read(&destination)? != original {
        return Err("coordination backup replaced a nonempty destination".into());
    }
    Ok(())
}

#[tokio::test]
async fn read_only_backup_open_rejects_missing_and_truncated_files_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing.db");
    if SqliteCoordination::open_local_read_only(&missing)
        .await
        .is_ok()
        || missing.exists()
        || SqliteInventory::open_local_read_only(&missing)
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
    if SqliteCoordination::open_local_read_only(&truncated_coordination)
        .await
        .is_ok()
        || std::fs::read(&truncated_coordination)? != truncated
        || SqliteInventory::open_local_read_only(&truncated_inventory)
            .await
            .is_ok()
        || std::fs::read(&truncated_inventory)? != truncated
    {
        return Err("read-only backup open repaired a truncated database".into());
    }
    let missing_schema = directory.path().join("missing-schema.db");
    let database = rusqlite::Connection::open(&missing_schema)?;
    database.execute("CREATE TABLE unrelated(value TEXT)", ())?;
    drop(database);
    let missing_schema_bytes = std::fs::read(&missing_schema)?;
    if SqliteCoordination::open_local_read_only(&missing_schema)
        .await
        .is_ok()
        || std::fs::read(&missing_schema)? != missing_schema_bytes
        || SqliteInventory::open_local_read_only(&missing_schema)
            .await
            .is_ok()
        || std::fs::read(&missing_schema)? != missing_schema_bytes
    {
        return Err("read-only backup open repaired a database without a schema".into());
    }
    Ok(())
}
