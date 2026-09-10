#![cfg(feature = "libsql-backend")]
//! Current coordination-store reopen probe; it grants no deployment authority.
use coordination::*;
use deploy::{ClaimState, ProtectedClaim, ResourceIdentity};
use serde::Serialize;
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const LIVE_PATH: &str = "/store/coordination.db";
const LIVE_RACE_BARRIER: &str = "/store/coordination-race-go";
const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const TEST_NONCE: &str = "0123456789abcdef0123456789abcdef";

#[path = "support/backup.rs"]
mod backup;
#[cfg(unix)]
#[path = "support/process_crash.rs"]
mod process_crash;
#[path = "support/write_failure.rs"]
mod write_failure;

#[derive(Clone, Copy)]
enum Phase {
    Seed,
    Verify,
    RaceLeft,
    RaceRight,
    VerifyRace,
    BackupSeed,
    BackupCreate,
    BackupMutate,
    BackupVerify,
}

impl Phase {
    fn parse(value: &str) -> TestResult<Self> {
        match value {
            "seed" => Ok(Self::Seed),
            "verify" => Ok(Self::Verify),
            "race-left" => Ok(Self::RaceLeft),
            "race-right" => Ok(Self::RaceRight),
            "verify-race" => Ok(Self::VerifyRace),
            "backup-seed" => Ok(Self::BackupSeed),
            "backup-create" => Ok(Self::BackupCreate),
            "backup-mutate" => Ok(Self::BackupMutate),
            "backup-verify" => Ok(Self::BackupVerify),
            _ => Err("unsupported coordination PVC probe phase".into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Verify => "verify",
            Self::RaceLeft => "race-left",
            Self::RaceRight => "race-right",
            Self::VerifyRace => "verify-race",
            Self::BackupSeed => "backup-seed",
            Self::BackupCreate => "backup-create",
            Self::BackupMutate => "backup-mutate",
            Self::BackupVerify => "backup-verify",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn phase(self) -> Phase {
        match self {
            Self::Left => Phase::RaceLeft,
            Self::Right => Phase::RaceRight,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a> {
    schema_version: &'static str,
    nonce: &'a str,
    phase: &'static str,
    outcome: &'static str,
    claim_digest: &'a str,
    review_digest: &'a str,
    audit_digest: &'a str,
    claim_count: usize,
    review_resource_count: usize,
    audit_event_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RaceReady<'a> {
    schema_version: &'static str,
    nonce: &'a str,
    phase: &'static str,
    outcome: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RaceReport<'a> {
    schema_version: &'static str,
    nonce: &'a str,
    phase: &'static str,
    outcome: &'static str,
    claim_digest: &'a str,
    left_review_digest: &'a str,
    right_review_digest: &'a str,
    winner_audit_digest: &'a str,
    loser_audit_digest: &'a str,
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
            qualified_create_precondition_servers: vec![ReviewQualifiedServer::V1343],
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

fn validate_nonce(value: &str) -> TestResult {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("coordination PVC probe requires a lowercase 32-hex nonce".into());
    }
    Ok(())
}

fn emit(phase: Phase, outcome: &'static str, nonce: &str, fixture: &Fixture) -> TestResult {
    validate_nonce(nonce)?;
    let report = Report {
        schema_version: "apsides.dev/pvc-coordination-probe/v1",
        nonce,
        phase: phase.name(),
        outcome,
        claim_digest: &fixture.claim_ref.digest,
        review_digest: &fixture.review_ref.digest,
        audit_digest: &fixture.audit_ref.digest,
        claim_count: fixture.claims.claims.len(),
        review_resource_count: fixture.review.resources.len(),
        audit_event_count: fixture.audit.events.len(),
    };
    let bytes = serde_json::to_vec(&report)?;
    if bytes.len() > 4096 {
        return Err("coordination PVC probe report exceeds its bound".into());
    }
    let mut output = std::io::stdout().lock();
    output.write_all(b"APS_COORDINATION_RESULT=")?;
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn write_report<T: Serialize>(prefix: &[u8], report: &T) -> TestResult {
    let bytes = serde_json::to_vec(report)?;
    if bytes.len() > 4096 {
        return Err("coordination PVC report exceeds its bound".into());
    }
    let mut output = std::io::stdout().lock();
    output.write_all(prefix)?;
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn emit_race_ready(side: Side, nonce: &str) -> TestResult {
    validate_nonce(nonce)?;
    write_report(
        b"APS_COORDINATION_RACE_READY=",
        &RaceReady {
            schema_version: "apsides.dev/pvc-coordination-race/v1",
            nonce,
            phase: side.phase().name(),
            outcome: "ready",
        },
    )
}

fn emit_race_result(
    phase: Phase,
    outcome: &'static str,
    nonce: &str,
    race: &RaceFixture,
    winner_left: bool,
) -> TestResult {
    validate_nonce(nonce)?;
    let (winner, loser) = if winner_left {
        (&race.left_audit_ref, &race.right_audit_ref)
    } else {
        (&race.right_audit_ref, &race.left_audit_ref)
    };
    write_report(
        b"APS_COORDINATION_RACE_RESULT=",
        &RaceReport {
            schema_version: "apsides.dev/pvc-coordination-race/v1",
            nonce,
            phase: phase.name(),
            outcome,
            claim_digest: &race.claim_ref.digest,
            left_review_digest: &race.left_review_ref.digest,
            right_review_digest: &race.right_review_ref.digest,
            winner_audit_digest: &winner.digest,
            loser_audit_digest: &loser.digest,
        },
    )
}

fn require_race_barrier_absent(path: &Path) -> TestResult {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err("coordination PVC race barrier must initially be absent".into()),
    }
}

fn wait_for_race_barrier(path: &Path) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match path.symlink_metadata() {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                return Ok(());
            }
            Ok(_) => return Err("coordination PVC race barrier is invalid".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("coordination PVC race barrier cannot be inspected".into()),
        }
        if Instant::now() >= deadline {
            return Err("coordination PVC race barrier timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn verify_reads(store: &LibsqlCoordination, fixture: &Fixture) -> TestResult {
    if store.read_claims(&fixture.claim_ref).await? != Some(fixture.claims.clone())
        || store.read_review(&fixture.review_ref).await? != Some(fixture.review.clone())
        || store.read_audit(&fixture.audit_ref).await? != Some(fixture.audit.clone())
    {
        return Err("coordination PVC metadata differs".into());
    }
    Ok(())
}

async fn seed(path: &Path, fixture: &Fixture) -> TestResult {
    let store = LibsqlCoordination::open_local(path).await?;
    if store.put_claims(&fixture.claims).await? != fixture.claim_ref
        || store.put_review(&fixture.review).await? != fixture.review_ref
        || store.put_audit(&fixture.audit).await? != fixture.audit_ref
    {
        return Err("coordination PVC reference differs".into());
    }
    verify_reads(&store, fixture).await
}

async fn verify_read_only(path: &Path, fixture: &Fixture) -> TestResult {
    let store = LibsqlCoordination::open_local_read_only(path).await?;
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

async fn audit_winner(store: &LibsqlCoordination, race: &RaceFixture) -> TestResult<bool> {
    match store.read_audit(&race.left_audit_ref).await {
        Ok(Some(value)) if value == race.left_audit => {
            if !matches!(
                store.read_audit(&race.right_audit_ref).await,
                Err(coordination::Error::Invalid("audit reference mismatch"))
            ) {
                return Err("coordination PVC losing audit reference was accepted".into());
            }
            Ok(true)
        }
        Err(coordination::Error::Invalid("audit reference mismatch")) => {
            if store.read_audit(&race.right_audit_ref).await? != Some(race.right_audit.clone()) {
                return Err("coordination PVC winning audit differs".into());
            }
            Ok(false)
        }
        _ => Err("coordination PVC audit winner is invalid".into()),
    }
}

async fn verify_race_absent(
    store: &LibsqlCoordination,
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
        return Err("coordination PVC race metadata already exists".into());
    }
    Ok(())
}

async fn publish_race(
    path: &Path,
    side: Side,
    nonce: &str,
    live_barrier: Option<&Path>,
) -> TestResult<(&'static str, bool)> {
    let seed = fixture()?;
    let race = race_fixture(&seed)?;
    let store = LibsqlCoordination::open_local(path).await?;
    if let Some(barrier) = live_barrier {
        verify_race_absent(&store, &seed, &race).await?;
        require_race_barrier_absent(barrier)?;
        emit_race_ready(side, nonce)?;
        wait_for_race_barrier(barrier)?;
    }
    if store.put_claims(&race.claims).await? != race.claim_ref {
        return Err("coordination PVC race claim reference differs".into());
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
        return Err("coordination PVC race review reference differs".into());
    }
    let published = match store.put_audit(audit).await {
        Ok(reference) if reference == *audit_ref => true,
        Err(coordination::Error::Invalid("immutable audit mismatch")) => false,
        _ => return Err("coordination PVC race audit result differs".into()),
    };
    let winner_left = audit_winner(&store, &race).await?;
    if published != (winner_left == (side == Side::Left)) {
        return Err("coordination PVC race outcome disagrees with stored audit".into());
    }
    Ok((
        if published { "published" } else { "conflict" },
        winner_left,
    ))
}

async fn verify_race_read_only(path: &Path) -> TestResult<bool> {
    let seed = fixture()?;
    let race = race_fixture(&seed)?;
    let store = LibsqlCoordination::open_local_read_only(path).await?;
    verify_reads(&store, &seed).await?;
    if store.read_claims(&race.claim_ref).await? != Some(race.claims.clone())
        || store.read_review(&race.left_review_ref).await? != Some(race.left_review.clone())
        || store.read_review(&race.right_review_ref).await? != Some(race.right_review.clone())
    {
        return Err("coordination PVC race metadata differs".into());
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

async fn execute(path: &Path, phase: Phase, nonce: &str) -> TestResult {
    let fixture = fixture()?;
    let outcome = match phase {
        Phase::Seed => {
            seed(path, &fixture).await?;
            "published"
        }
        Phase::Verify => {
            verify_read_only(path, &fixture).await?;
            "verified"
        }
        Phase::RaceLeft
        | Phase::RaceRight
        | Phase::VerifyRace
        | Phase::BackupSeed
        | Phase::BackupCreate
        | Phase::BackupMutate
        | Phase::BackupVerify => {
            return Err("coordination PVC race phase requires its dedicated protocol".into());
        }
    };
    emit(phase, outcome, nonce, &fixture)
}

async fn execute_race(path: &Path, phase: Phase, nonce: &str, live: bool) -> TestResult {
    let seed = fixture()?;
    let race = race_fixture(&seed)?;
    match phase {
        Phase::RaceLeft | Phase::RaceRight => {
            let side = if matches!(phase, Phase::RaceLeft) {
                Side::Left
            } else {
                Side::Right
            };
            let barrier = live.then(|| PathBuf::from(LIVE_RACE_BARRIER));
            let (outcome, winner_left) =
                publish_race(path, side, nonce, barrier.as_deref()).await?;
            emit_race_result(phase, outcome, nonce, &race, winner_left)
        }
        Phase::VerifyRace => {
            let winner_left = verify_race_read_only(path).await?;
            emit_race_result(phase, "verified", nonce, &race, winner_left)
        }
        Phase::Seed
        | Phase::Verify
        | Phase::BackupSeed
        | Phase::BackupCreate
        | Phase::BackupMutate
        | Phase::BackupVerify => {
            Err("coordination PVC non-race phase used with race protocol".into())
        }
    }
}

fn live_configuration(phase: &str, path: &str, nonce: &str) -> TestResult<Phase> {
    validate_nonce(nonce)?;
    if path != LIVE_PATH {
        return Err("coordination PVC probe requires its exact mounted database path".into());
    }
    Phase::parse(phase)
}

#[tokio::test]
#[ignore = "requires an explicitly provisioned disposable Kubernetes PVC"]
async fn pvc_coordination_phase() -> TestResult {
    let phase = std::env::var("APS_COORDINATION_TEST_PHASE")
        .map_err(|_| "coordination PVC phase is required")?;
    let path = std::env::var("APS_COORDINATION_TEST_PATH")
        .map_err(|_| "coordination PVC path is required")?;
    let nonce = std::env::var("APS_COORDINATION_TEST_NONCE")
        .map_err(|_| "coordination PVC nonce is required")?;
    let phase = live_configuration(&phase, &path, &nonce)?;
    match phase {
        Phase::Seed | Phase::Verify => execute(Path::new(&path), phase, &nonce).await,
        Phase::RaceLeft | Phase::RaceRight | Phase::VerifyRace => {
            execute_race(Path::new(&path), phase, &nonce, true).await
        }
        Phase::BackupSeed | Phase::BackupCreate | Phase::BackupMutate | Phase::BackupVerify => {
            backup::execute_live(phase, &nonce).await
        }
    }
    .map_err(|_| "coordination PVC probe failed".into())
}

#[tokio::test]
async fn local_seed_then_read_only_reopen_verifies_all_current_stores() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    execute(&path, Phase::Seed, TEST_NONCE).await?;
    execute(&path, Phase::Verify, TEST_NONCE).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_competing_writers_preserve_both_content_addressed_reviews() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    execute(&path, Phase::Seed, TEST_NONCE).await?;
    let seed = fixture()?;
    let race = race_fixture(&seed)?;
    let before = LibsqlCoordination::open_local_read_only(&path).await?;
    verify_race_absent(&before, &seed, &race).await?;
    drop(before);
    let (left, right) = tokio::join!(
        publish_race(&path, Side::Left, TEST_NONCE, None),
        publish_race(&path, Side::Right, TEST_NONCE, None),
    );
    let (left_outcome, left_winner) = left?;
    let (right_outcome, right_winner) = right?;
    if left_winner != right_winner
        || !matches!(
            (left_outcome, right_outcome),
            ("published", "conflict") | ("conflict", "published")
        )
    {
        return Err("coordination PVC race did not produce exactly one winner".into());
    }
    if verify_race_read_only(&path).await? != left_winner {
        return Err("coordination PVC read-only winner differs".into());
    }
    Ok(())
}

#[test]
fn live_protocol_is_exact_and_bounded() {
    assert!(live_configuration("seed", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("verify", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("race-left", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("race-right", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("verify-race", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("backup-seed", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("backup-create", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("backup-mutate", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("backup-verify", LIVE_PATH, TEST_NONCE).is_ok());
    for phase in ["", "write", "recover", "race"] {
        assert!(live_configuration(phase, LIVE_PATH, TEST_NONCE).is_err());
    }
    for path in [
        "coordination.db",
        "/tmp/coordination.db",
        "/store/../coordination.db",
    ] {
        assert!(live_configuration("seed", path, TEST_NONCE).is_err());
    }
    assert!(live_configuration("seed", LIVE_PATH, "not-a-valid-nonce").is_err());
}
