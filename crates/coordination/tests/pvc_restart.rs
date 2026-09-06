#![cfg(feature = "libsql-backend")]
//! Current coordination-store reopen probe; it grants no deployment authority.
use coordination::*;
use deploy::{ClaimState, ProtectedClaim, ResourceIdentity};
use serde::Serialize;
use std::{io::Write, path::Path};

type TestResult<T = ()> =
    std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const LIVE_PATH: &str = "/store/coordination.db";
const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const TEST_NONCE: &str = "0123456789abcdef0123456789abcdef";

#[derive(Clone, Copy)]
enum Phase {
    Seed,
    Verify,
}

impl Phase {
    fn parse(value: &str) -> TestResult<Self> {
        match value {
            "seed" => Ok(Self::Seed),
            "verify" => Ok(Self::Verify),
            _ => Err("unsupported coordination PVC probe phase".into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Verify => "verify",
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

struct Fixture {
    claims: ClaimSnapshotV1,
    claim_ref: ClaimRef,
    review: ReviewMetadataV1,
    review_ref: ReviewRef,
    audit: TransitionAuditV1,
    audit_ref: AuditRef,
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

fn review(names: &deploy::AllocationNames) -> ReviewMetadataV1 {
    ReviewMetadataV1 {
        schema_version: REVIEW_METADATA_SCHEMA.to_owned(),
        scope: ReviewScope {
            claim_scope: scope(),
            domain_id: "storage".to_owned(),
            transition_id: names.transition_id.clone(),
            sequence: 1,
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

fn audit(names: &deploy::AllocationNames) -> TransitionAuditV1 {
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
    TransitionAuditV1 {
        schema_version: TRANSITION_AUDIT_SCHEMA.to_owned(),
        scope: AuditScope {
            claim_scope: scope(),
            domain_id: "storage".to_owned(),
            transition_id: names.transition_id.clone(),
            sequence: 1,
        },
        allocation: AuditAllocation {
            sequence: 1,
            generation_id: names.generation_id.clone(),
            transition_id: names.transition_id.clone(),
            service_account_name: names.service_account_name.clone(),
        },
        executor_id: "storage-probe".to_owned(),
        final_revision: 5,
        final_verification_epoch: 0,
        events,
        cleanup: Vec::new(),
        ownership: Vec::new(),
        retirement: Vec::new(),
        previous: None,
    }
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
    let review = review(&names);
    let review_ref = review.reference()?;
    let audit = audit(&names);
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
        schema_version: "reakt.dev/pvc-coordination-probe/v1",
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
    };
    emit(phase, outcome, nonce, &fixture)
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
    execute(Path::new(&path), live_configuration(&phase, &path, &nonce)?, &nonce)
        .await
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

#[test]
fn live_protocol_is_exact_and_bounded() {
    assert!(live_configuration("seed", LIVE_PATH, TEST_NONCE).is_ok());
    assert!(live_configuration("verify", LIVE_PATH, TEST_NONCE).is_ok());
    for phase in ["", "write", "recover"] {
        assert!(live_configuration(phase, LIVE_PATH, TEST_NONCE).is_err());
    }
    for path in ["coordination.db", "/tmp/coordination.db", "/store/../coordination.db"] {
        assert!(live_configuration("seed", path, TEST_NONCE).is_err());
    }
    assert!(live_configuration("seed", LIVE_PATH, "not-a-valid-nonce").is_err());
}
