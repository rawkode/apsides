#![cfg(feature = "libsql-backend")]

use coordination::*;
use deploy::{CleanupOutcome, OwnershipAction, ResourceIdentity};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const PRIVATE_CANARY: &str = "private-audit-canary";

fn claim_scope() -> ClaimScope {
    ClaimScope {
        registry_uid: REGISTRY_UID.to_owned(),
        cluster_binding: "cluster-a".to_owned(),
        control_namespace: "control".to_owned(),
        workload_namespace: "workload".to_owned(),
    }
}

fn identity(resource: &str, namespace: &str, name: &str) -> ResourceIdentity {
    ResourceIdentity {
        api_group: String::new(),
        resource: resource.to_owned(),
        namespace: namespace.to_owned(),
        name: name.to_owned(),
    }
}

fn events(resumed: bool) -> Vec<AuditEvent> {
    let mut events = if resumed {
        vec![
            AuditEvent {
                kind: AuditEventKind::Blocked,
                event_id: "blocked-1".to_owned(),
                verification_epoch: 0,
            },
            AuditEvent {
                kind: AuditEventKind::Resume,
                event_id: "resume-1".to_owned(),
                verification_epoch: 1,
            },
        ]
    } else {
        Vec::new()
    };
    let epoch = u64::from(resumed);
    events.extend(
        [
            (AuditEventKind::Staged, "staged"),
            (AuditEventKind::Quiesced, "quiesced"),
            (AuditEventKind::Activated, "activated"),
            (AuditEventKind::Cleaning, "cleaning"),
            (AuditEventKind::Committed, "committed"),
        ]
        .into_iter()
        .map(|(kind, event_id)| AuditEvent {
            kind,
            event_id: event_id.to_owned(),
            verification_epoch: epoch,
        }),
    );
    events
}

fn audit(sequence: u64, previous: Option<AuditRef>) -> TestResult<TransitionAuditV1> {
    let names = deploy::allocation_names(REGISTRY_UID, sequence)?;
    let resumed = previous.is_some();
    let events = events(resumed);
    let retirement = match previous.as_ref() {
        Some(previous) => {
            let prior = deploy::allocation_names(REGISTRY_UID, previous.scope.sequence)?;
            vec![
                AuditRetirement {
                    identity: identity("serviceaccounts", "control", &prior.service_account_name),
                    uid: "service-account-uid".to_owned(),
                },
                AuditRetirement {
                    identity: ResourceIdentity {
                        api_group: "rbac.authorization.k8s.io".to_owned(),
                        resource: "rolebindings".to_owned(),
                        namespace: "workload".to_owned(),
                        name: prior.service_account_name,
                    },
                    uid: "role-binding-uid".to_owned(),
                },
            ]
        }
        None => Vec::new(),
    };
    Ok(TransitionAuditV1 {
        schema_version: TRANSITION_AUDIT_SCHEMA.to_owned(),
        scope: AuditScope {
            claim_scope: claim_scope(),
            domain_id: "controller".to_owned(),
            transition_id: names.transition_id.clone(),
            sequence,
        },
        allocation: AuditAllocation {
            sequence,
            generation_id: names.generation_id,
            transition_id: names.transition_id,
            service_account_name: names.service_account_name,
        },
        executor_id: "ci_job_1".to_owned(),
        final_revision: events.len() as u64,
        final_verification_epoch: u64::from(resumed),
        events,
        cleanup: vec![AuditCleanup {
            identity: identity("configmaps", "workload", "retired-config"),
            expected_uid: None,
            outcome: CleanupOutcome::AlreadyAbsent,
        }],
        ownership: vec![AuditOwnership {
            identity: identity("secrets", "workload", "adopted-secret"),
            observed_uid: "adopted-uid".to_owned(),
            action: OwnershipAction::Adopt,
        }],
        retirement,
        previous,
    })
}

fn missing_reference(sequence: u64) -> TestResult<AuditRef> {
    let names = deploy::allocation_names(REGISTRY_UID, sequence)?;
    Ok(AuditRef {
        scope: AuditScope {
            claim_scope: claim_scope(),
            domain_id: "controller".to_owned(),
            transition_id: names.transition_id,
            sequence,
        },
        digest: format!("sha256:{}", "f".repeat(64)),
    })
}

#[tokio::test]
async fn immutable_audit_chain_survives_real_local_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let first = audit(1, None)?;
    let first_reference = {
        let store = LibsqlCoordination::open_local(&path).await?;
        store.put_audit(&first).await?
    };
    let second = audit(2, Some(first_reference.clone()))?;
    let second_reference = {
        let store = LibsqlCoordination::open_local(&path).await?;
        store.put_audit(&second).await?
    };

    let reopened = LibsqlCoordination::open_local(&path).await?;
    assert_eq!(reopened.read_audit(&first_reference).await?, Some(first));
    assert_eq!(reopened.read_audit(&second_reference).await?, Some(second));
    assert_eq!(reopened.read_audit(&missing_reference(3)?).await?, None);
    Ok(())
}

#[tokio::test]
async fn repeated_write_is_idempotent_but_same_transition_conflict_is_rejected() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = LibsqlCoordination::open_local(&directory.path().join("coordination.db")).await?;
    let original = audit(1, None)?;
    let reference = store.put_audit(&original).await?;
    assert_eq!(store.put_audit(&original).await?, reference);

    let mut conflict = original.clone();
    conflict.executor_id = "different-executor".to_owned();
    assert!(matches!(
        store.put_audit(&conflict).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.read_audit(&reference).await?, Some(original));
    Ok(())
}

#[tokio::test]
async fn corrupt_existing_audit_is_an_error_and_diagnostics_are_private() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let store = LibsqlCoordination::open_local(&path).await?;
    let audit = audit(1, None)?;
    let reference = store.put_audit(&audit).await?;
    let canonical = String::from_utf8(compiler_core::canonical_bytes(&audit)?)?;
    let corrupted = canonical.replace("ci_job_1", PRIVATE_CANARY);
    let database = libsql::Builder::new_local(&path).build().await?;
    database
        .connect()?
        .execute(
            "UPDATE reakt_coordination_audits SET metadata=?1 WHERE transition_id=?2",
            libsql::params![
                corrupted.into_bytes(),
                reference.scope.transition_id.clone()
            ],
        )
        .await?;

    let Err(error) = store.read_audit(&reference).await else {
        return Err("corrupt audit row was accepted".into());
    };
    assert!(!error.to_string().contains(PRIVATE_CANARY));
    let Err(error) = store.put_audit(&audit).await else {
        return Err("immutable publication replaced a corrupt audit row".into());
    };
    assert!(!error.to_string().contains(PRIVATE_CANARY));
    Ok(())
}

#[test]
fn typed_audit_excludes_private_payloads_and_rejects_unknown_fields() -> TestResult {
    let audit = audit(1, None)?;
    let encoded = serde_json::to_string(&audit)?;
    for excluded in [
        "evidenceDigest",
        "blockedReason",
        "priorManagedFieldsDigest",
        "permittedApply",
        "artifactDigest",
        "irDigest",
    ] {
        assert!(!encoded.contains(excluded), "{excluded}");
    }
    let mut value = serde_json::to_value(audit)?;
    value
        .as_object_mut()
        .ok_or("audit fixture is not an object")?
        .insert(
            "evidenceDigest".to_owned(),
            serde_json::json!(PRIVATE_CANARY),
        );
    let error = serde_json::from_value::<TransitionAuditV1>(value)
        .err()
        .ok_or("private audit payload was accepted")?;
    assert!(!error.to_string().contains(PRIVATE_CANARY));
    Ok(())
}

#[test]
fn malformed_order_scope_chain_and_sanitized_metadata_are_rejected() -> TestResult {
    let first = audit(1, None)?;
    let first_reference = first.reference()?;

    let mut changed = audit(2, Some(first_reference.clone()))?;
    changed.allocation.service_account_name = "foreign".to_owned();
    assert!(changed.validate().is_err());

    let mut changed = audit(2, Some(first_reference.clone()))?;
    changed.events.swap(0, 1);
    assert!(changed.validate().is_err());

    let mut changed = audit(2, Some(first_reference.clone()))?;
    changed
        .events
        .first_mut()
        .ok_or("event fixture missing")?
        .event_id = "committed".to_owned();
    assert!(changed.validate().is_err());

    let mut changed = audit(2, Some(first_reference.clone()))?;
    changed
        .previous
        .as_mut()
        .ok_or("previous audit missing")?
        .scope
        .domain_id = "other".to_owned();
    assert!(changed.validate().is_err());

    let mut changed = audit(2, Some(first_reference))?;
    changed
        .ownership
        .first_mut()
        .ok_or("ownership fixture missing")?
        .action = OwnershipAction::Transfer {
        source_domain_id: "other".to_owned(),
        source_generation_id: "foreign-generation".to_owned(),
        source_transition_id: "foreign-transition".to_owned(),
    };
    assert!(changed.validate().is_err());

    let mut changed = audit(1, None)?;
    changed.retirement.push(AuditRetirement {
        identity: identity("secrets", "control", "secret"),
        uid: "secret-uid".to_owned(),
    });
    assert!(changed.validate().is_err());

    let mut changed = audit(1, None)?;
    let duplicate = changed
        .cleanup
        .first()
        .ok_or("cleanup fixture missing")?
        .clone();
    changed.cleanup.push(duplicate);
    assert!(changed.validate().is_err());

    let mut changed = audit(1, None)?;
    changed
        .cleanup
        .first_mut()
        .ok_or("cleanup fixture missing")?
        .outcome = CleanupOutcome::Deleted;
    assert!(changed.validate().is_err());
    Ok(())
}

#[test]
fn audit_event_and_cleanup_counts_are_bounded() -> TestResult {
    let mut too_many_events = audit(1, None)?;
    let repeated = too_many_events
        .events
        .first()
        .ok_or("event fixture missing")?
        .clone();
    too_many_events.events = vec![repeated; MAX_AUDIT_EVENTS + 1];
    too_many_events.final_revision = too_many_events.events.len() as u64;
    assert!(too_many_events.validate().is_err());

    let mut too_much_cleanup = audit(1, None)?;
    let repeated = too_much_cleanup
        .cleanup
        .first()
        .ok_or("cleanup fixture missing")?
        .clone();
    too_much_cleanup.cleanup = vec![repeated; MAX_AUDIT_CLEANUP + 1];
    assert!(too_much_cleanup.validate().is_err());
    Ok(())
}

#[tokio::test]
async fn local_read_only_open_never_creates_or_mutates_coordination_state() -> TestResult {
    let directory = tempfile::tempdir()?;
    let missing_path = directory.path().join("missing.db");
    assert!(
        LibsqlCoordination::open_local_read_only(&missing_path)
            .await
            .is_err()
    );
    assert!(!missing_path.exists());

    let path = directory.path().join("coordination.db");
    let original = audit(1, None)?;
    let reference = {
        let writable = LibsqlCoordination::open_local(&path).await?;
        writable.put_audit(&original).await?
    };
    let read_only = LibsqlCoordination::open_local_read_only(&path).await?;
    assert_eq!(read_only.read_audit(&reference).await?, Some(original));
    assert!(matches!(
        read_only.put_audit(&audit(2, Some(reference))?).await,
        Err(Error::Invalid(_))
    ));
    let claims = ClaimSnapshotV1 {
        schema_version: CLAIM_SNAPSHOT_SCHEMA.to_owned(),
        scope: claim_scope(),
        claims: Vec::new(),
    };
    assert!(matches!(
        read_only.put_claims(&claims).await,
        Err(Error::Invalid(_))
    ));
    Ok(())
}

#[tokio::test]
async fn read_only_open_rejects_schema_or_table_mismatch_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let incomplete_path = directory.path().join("incomplete.db");
    let incomplete_database = libsql::Builder::new_local(&incomplete_path).build().await?;
    incomplete_database
        .connect()?
        .execute_batch(
            "CREATE TABLE reakt_coordination_schema (id INTEGER PRIMARY KEY, version INTEGER NOT NULL);\
             INSERT INTO reakt_coordination_schema VALUES (1,1);\
             CREATE TABLE reakt_coordination_claims (digest TEXT PRIMARY KEY, metadata BLOB NOT NULL);",
        )
        .await?;
    assert!(
        LibsqlCoordination::open_local_read_only(&incomplete_path)
            .await
            .is_err()
    );
    let mut rows = incomplete_database
        .connect()?
        .query(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='reakt_coordination_audits'",
            (),
        )
        .await?;
    let count = rows
        .next()
        .await?
        .ok_or("sqlite_master count row missing")?
        .get::<i64>(0)?;
    assert_eq!(count, 0);

    let mismatch_path = directory.path().join("mismatch.db");
    LibsqlCoordination::open_local(&mismatch_path).await?;
    let mismatch_database = libsql::Builder::new_local(&mismatch_path).build().await?;
    mismatch_database
        .connect()?
        .execute(
            "UPDATE reakt_coordination_schema SET version=2 WHERE id=1",
            (),
        )
        .await?;
    assert!(
        LibsqlCoordination::open_local_read_only(&mismatch_path)
            .await
            .is_err()
    );
    let mut rows = mismatch_database
        .connect()?
        .query(
            "SELECT version FROM reakt_coordination_schema WHERE id=1",
            (),
        )
        .await?;
    let version = rows
        .next()
        .await?
        .ok_or("schema version row missing")?
        .get::<i64>(0)?;
    assert_eq!(version, 2);
    Ok(())
}
