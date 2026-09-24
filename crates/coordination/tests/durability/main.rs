#![cfg(feature = "sqlite-backend")]
//! Durability of the local SQLite coordination store: reopen, competing
//! writers, backup, returned write failures and process crashes. These tests
//! grant no deployment authority.
use coordination::*;
use deploy::{ClaimState, ProtectedClaim, ResourceIdentity};
use std::path::{Path, PathBuf};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";

mod backup;
#[cfg(unix)]
mod process_crash;
mod write_failure;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

struct Fixture {
    claims: ClaimSnapshotV1,
    claim_ref: ClaimRef,
    review: ReviewMetadataV1,
    review_ref: ReviewRef,
    audit: TransitionAuditV1,
    audit_ref: AuditRef,
}

struct RaceFixture {
    claims: ClaimSnapshotV1,
    claim_ref: ClaimRef,
    left_review: ReviewMetadataV1,
    left_review_ref: ReviewRef,
    right_review: ReviewMetadataV1,
    right_review_ref: ReviewRef,
    left_audit: TransitionAuditV1,
    left_audit_ref: AuditRef,
    right_audit: TransitionAuditV1,
    right_audit_ref: AuditRef,
}

fn scope() -> ClaimScope {
    ClaimScope {
        registry_uid: REGISTRY_UID.to_owned(),
        cluster_binding: "storage-cluster".to_owned(),
        control_namespace: "storage-control".to_owned(),
        workload_namespace: "storage-workload".to_owned(),
    }
}

fn ceiling() -> ReviewCeiling {
    ReviewCeiling {
        policy_ref: "storage-policy".to_owned(),
        domain_id: "storage".to_owned(),
        cluster_binding: "storage-cluster".to_owned(),
        workload_namespace: "storage-workload".to_owned(),
        control_namespace: "storage-control".to_owned(),
        capabilities: Vec::new(),
        accepted_enforcement_gaps: Vec::new(),
        accepted_indirect_risks: Vec::new(),
    }
}

fn review(names: &deploy::AllocationNames, sequence: u64) -> ReviewMetadataV1 {
    ReviewMetadataV1 {
        schema_version: REVIEW_METADATA_SCHEMA.to_owned(),
        scope: ReviewScope {
            claim_scope: scope(),
            domain_id: "storage".to_owned(),
            transition_id: names.transition_id.clone(),
            sequence,
        },
        resources: Vec::new(),
        inputs: Vec::new(),
        reactions: Vec::new(),
        runtime_permissions: Vec::new(),
        cleanup_permissions: Vec::new(),
        deployment_permissions: ReviewDeploymentPermissions {
            required_resources: Vec::new(),
            creation_targets: Vec::new(),
            role_delegation: Vec::new(),
            scope: ReviewDeploymentScope::IssuedDeployerOnly,
            authorization: ReviewAuthorization::RequiredAccessOnly,
            limitations: vec![
                ReviewDeploymentLimitation::CreateNamesRequireAdmission,
                ReviewDeploymentLimitation::GeneratedNamesRequireBroadAccess,
                ReviewDeploymentLimitation::InvokingIdentityBootstrapCredentialsAndDiscoveryExcluded,
                ReviewDeploymentLimitation::DatabaseCredentialsSeparate,
            ],
        },
        compatibility: ReviewCompatibility {
            runtime_budgets: ReviewBudgets {
                max_resources: 1,
                max_inputs: 1,
                max_reactions: 1,
                max_object_bytes: 1024,
                max_queue_depth: 1,
                max_concurrent_requests: 1,
            },
            schema_library: ReviewSchemaLibrary::K8sOpenapi0280,
            schema_kubernetes_minor: ReviewSchemaMinor::V136,
            required_runtime_apis: Vec::new(),
            required_cleanup_apis: Vec::new(),
            required_deployment_apis: Vec::new(),
            deployment_api_scope: ReviewDeploymentScope::IssuedDeployerOnly,
            kubernetes_version: ReviewKubernetesVersion {
                min_version: "1.34".into(),
                qualified_on: ReviewQualifiedServer::V1343,
            },
            field_validation: ReviewFieldValidation::Strict,
            transition_plan_schema: ReviewTransitionSchema::V4,
            runtime_replicas: 1,
        },
        enforcement: ReviewEnforcement {
            profile: ReviewEnforcementProfile::ApiBound,
            gaps: Vec::new(),
            indirect_risks: Vec::new(),
            runtime_ceiling: ceiling(),
            cleanup_ceiling: ceiling(),
            decision: ReviewCeilingDecision::ApprovedByPlanner,
        },
    }
}

fn audit(
    names: &deploy::AllocationNames,
    sequence: u64,
    executor_id: &str,
    previous: Option<AuditRef>,
) -> TestResult<TransitionAuditV1> {
    let events = [
        AuditEventKind::Staged,
        AuditEventKind::Quiesced,
        AuditEventKind::Activated,
        AuditEventKind::Cleaning,
        AuditEventKind::Committed,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, kind)| AuditEvent {
        kind,
        event_id: format!("event-{index}"),
        verification_epoch: 0,
    })
    .collect();
    let retirement = if let Some(reference) = &previous {
        let names = deploy::allocation_names(REGISTRY_UID, reference.scope.sequence)?;
        vec![AuditRetirement {
            identity: ResourceIdentity {
                api_group: String::new(),
                resource: "serviceaccounts".to_owned(),
                namespace: "storage-control".to_owned(),
                name: names.service_account_name,
            },
            uid: "retired-service-account-uid".to_owned(),
        }]
    } else {
        Vec::new()
    };
    Ok(TransitionAuditV1 {
        schema_version: TRANSITION_AUDIT_SCHEMA.to_owned(),
        scope: AuditScope {
            claim_scope: scope(),
            domain_id: "storage".to_owned(),
            transition_id: names.transition_id.clone(),
            sequence,
        },
        allocation: AuditAllocation {
            sequence,
            generation_id: names.generation_id.clone(),
            transition_id: names.transition_id.clone(),
            service_account_name: names.service_account_name.clone(),
        },
        executor_id: executor_id.to_owned(),
        final_revision: 5,
        final_verification_epoch: 0,
        events,
        cleanup: Vec::new(),
        ownership: Vec::new(),
        retirement,
        previous,
    })
}

fn fixture() -> TestResult<Fixture> {
    let names = deploy::allocation_names(REGISTRY_UID, 1)?;
    let claims = ClaimSnapshotV1 {
        schema_version: CLAIM_SNAPSHOT_SCHEMA.to_owned(),
        scope: scope(),
        claims: vec![ProtectedClaim {
            identity: ResourceIdentity {
                api_group: String::new(),
                resource: "configmaps".to_owned(),
                namespace: "storage-workload".to_owned(),
                name: "tracked-object".to_owned(),
            },
            domain_id: "storage".to_owned(),
            generation_id: names.generation_id.clone(),
            transition_id: names.transition_id.clone(),
            state: ClaimState::Active,
            uid: Some("tracked-object-uid".to_owned()),
        }],
    };
    let claim_ref = claims.reference()?;
    let review = review(&names, 1);
    let review_ref = review.reference()?;
    let audit = audit(&names, 1, "storage-probe", None)?;
    let audit_ref = audit.reference()?;
    Ok(Fixture {
        claims,
        claim_ref,
        review,
        review_ref,
        audit,
        audit_ref,
    })
}

fn race_fixture(seed: &Fixture) -> TestResult<RaceFixture> {
    let names = deploy::allocation_names(REGISTRY_UID, 2)?;
    let claims = ClaimSnapshotV1 {
        schema_version: CLAIM_SNAPSHOT_SCHEMA.to_owned(),
        scope: scope(),
        claims: vec![ProtectedClaim {
            identity: ResourceIdentity {
                api_group: String::new(),
                resource: "configmaps".to_owned(),
                namespace: "storage-workload".to_owned(),
                name: "race-object".to_owned(),
            },
            domain_id: "storage".to_owned(),
            generation_id: names.generation_id.clone(),
            transition_id: names.transition_id.clone(),
            state: ClaimState::Active,
            uid: Some("race-object-uid".to_owned()),
        }],
    };
    let claim_ref = claims.reference()?;
    let mut left_review = review(&names, 2);
    "race-left-policy".clone_into(&mut left_review.enforcement.runtime_ceiling.policy_ref);
    "race-left-policy".clone_into(&mut left_review.enforcement.cleanup_ceiling.policy_ref);
    let left_review_ref = left_review.reference()?;
    let mut right_review = review(&names, 2);
    "race-right-policy".clone_into(&mut right_review.enforcement.runtime_ceiling.policy_ref);
    "race-right-policy".clone_into(&mut right_review.enforcement.cleanup_ceiling.policy_ref);
    let right_review_ref = right_review.reference()?;
    let left_audit = audit(&names, 2, "storage-race-left", Some(seed.audit_ref.clone()))?;
    let left_audit_ref = left_audit.reference()?;
    let right_audit = audit(
        &names,
        2,
        "storage-race-right",
        Some(seed.audit_ref.clone()),
    )?;
    let right_audit_ref = right_audit.reference()?;
    Ok(RaceFixture {
        claims,
        claim_ref,
        left_review,
        left_review_ref,
        right_review,
        right_review_ref,
        left_audit,
        left_audit_ref,
        right_audit,
        right_audit_ref,
    })
}

async fn verify_reads(store: &SqliteCoordination, fixture: &Fixture) -> TestResult {
    if store.read_claims(&fixture.claim_ref).await? != Some(fixture.claims.clone())
        || store.read_review(&fixture.review_ref).await? != Some(fixture.review.clone())
        || store.read_audit(&fixture.audit_ref).await? != Some(fixture.audit.clone())
    {
        return Err("coordination metadata differs".into());
    }
    Ok(())
}

async fn seed(path: &Path, fixture: &Fixture) -> TestResult {
    let store = SqliteCoordination::open_local(path).await?;
    if store.put_claims(&fixture.claims).await? != fixture.claim_ref
        || store.put_review(&fixture.review).await? != fixture.review_ref
        || store.put_audit(&fixture.audit).await? != fixture.audit_ref
    {
        return Err("coordination reference differs".into());
    }
    verify_reads(&store, fixture).await
}

async fn verify_read_only(path: &Path, fixture: &Fixture) -> TestResult {
    let store = SqliteCoordination::open_local_read_only(path).await?;
    verify_reads(&store, fixture).await?;
    if !matches!(
        store.put_claims(&fixture.claims).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        store.put_review(&fixture.review).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        store.put_audit(&fixture.audit).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) {
        return Err("read-only coordination write was not rejected".into());
    }
    Ok(())
}

async fn audit_winner(store: &SqliteCoordination, race: &RaceFixture) -> TestResult<bool> {
    match store.read_audit(&race.left_audit_ref).await {
        Ok(Some(value)) if value == race.left_audit => {
            if !matches!(
                store.read_audit(&race.right_audit_ref).await,
                Err(coordination::Error::Invalid("audit reference mismatch"))
            ) {
                return Err("coordination losing audit reference was accepted".into());
            }
            Ok(true)
        }
        Err(coordination::Error::Invalid("audit reference mismatch")) => {
            if store.read_audit(&race.right_audit_ref).await? != Some(race.right_audit.clone()) {
                return Err("coordination winning audit differs".into());
            }
            Ok(false)
        }
        _ => Err("coordination audit winner is invalid".into()),
    }
}

async fn verify_race_absent(
    store: &SqliteCoordination,
    seed: &Fixture,
    race: &RaceFixture,
) -> TestResult {
    verify_reads(store, seed).await?;
    if store.read_claims(&race.claim_ref).await?.is_some()
        || store.read_review(&race.left_review_ref).await?.is_some()
        || store.read_review(&race.right_review_ref).await?.is_some()
        || store.read_audit(&race.left_audit_ref).await?.is_some()
        || store.read_audit(&race.right_audit_ref).await?.is_some()
    {
        return Err("coordination race metadata already exists".into());
    }
    Ok(())
}

async fn publish_race(path: &Path, side: Side) -> TestResult<(&'static str, bool)> {
    let seed = fixture()?;
    let race = race_fixture(&seed)?;
    let store = SqliteCoordination::open_local(path).await?;
    if store.put_claims(&race.claims).await? != race.claim_ref {
        return Err("coordination race claim reference differs".into());
    }
    let (review, review_ref, audit, audit_ref) = match side {
        Side::Left => (
            &race.left_review,
            &race.left_review_ref,
            &race.left_audit,
            &race.left_audit_ref,
        ),
        Side::Right => (
            &race.right_review,
            &race.right_review_ref,
            &race.right_audit,
            &race.right_audit_ref,
        ),
    };
    if store.put_review(review).await? != *review_ref {
        return Err("coordination race review reference differs".into());
    }
    let published = match store.put_audit(audit).await {
        Ok(reference) if reference == *audit_ref => true,
        Err(coordination::Error::Invalid("immutable audit mismatch")) => false,
        _ => return Err("coordination race audit result differs".into()),
    };
    let winner_left = audit_winner(&store, &race).await?;
    if published != (winner_left == (side == Side::Left)) {
        return Err("coordination race outcome disagrees with stored audit".into());
    }
    Ok((
        if published { "published" } else { "conflict" },
        winner_left,
    ))
}

async fn verify_race_read_only(path: &Path) -> TestResult<bool> {
    let seed = fixture()?;
    let race = race_fixture(&seed)?;
    let store = SqliteCoordination::open_local_read_only(path).await?;
    verify_reads(&store, &seed).await?;
    if store.read_claims(&race.claim_ref).await? != Some(race.claims.clone())
        || store.read_review(&race.left_review_ref).await? != Some(race.left_review.clone())
        || store.read_review(&race.right_review_ref).await? != Some(race.right_review.clone())
    {
        return Err("coordination race metadata differs".into());
    }
    let winner_left = audit_winner(&store, &race).await?;
    let winner = if winner_left {
        &race.left_audit
    } else {
        &race.right_audit
    };
    if !matches!(
        store.put_claims(&race.claims).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        store.put_review(&race.left_review).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) || !matches!(
        store.put_audit(winner).await,
        Err(coordination::Error::Invalid("read-only coordination store"))
    ) {
        return Err("read-only coordination race write was not rejected".into());
    }
    Ok(winner_left)
}

#[tokio::test]
async fn seed_then_read_only_reopen_verifies_all_current_stores() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let fixture = fixture()?;
    seed(&path, &fixture).await?;
    verify_read_only(&path, &fixture).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_writers_preserve_both_content_addressed_reviews() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let seed_fixture = fixture()?;
    seed(&path, &seed_fixture).await?;
    let race = race_fixture(&seed_fixture)?;
    let before = SqliteCoordination::open_local_read_only(&path).await?;
    verify_race_absent(&before, &seed_fixture, &race).await?;
    drop(before);
    let (left, right) = tokio::join!(
        publish_race(&path, Side::Left),
        publish_race(&path, Side::Right),
    );
    let (left_outcome, left_winner) = left?;
    let (right_outcome, right_winner) = right?;
    if left_winner != right_winner
        || !matches!(
            (left_outcome, right_outcome),
            ("published", "conflict") | ("conflict", "published")
        )
    {
        return Err("coordination race did not produce exactly one winner".into());
    }
    if verify_race_read_only(&path).await? != left_winner {
        return Err("coordination read-only winner differs".into());
    }
    Ok(())
}
