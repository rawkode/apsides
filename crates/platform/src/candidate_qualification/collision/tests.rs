use super::*;
use crate::{
    CapacityAdmission, EXECUTOR_BINDING_SCHEMA, ExecutorBinding, NamespaceRegistryState,
    PolicyState, REGISTRY_SCHEMA_VERSION, SCHEMA_VERSION, StoredTransition, TransitionInputs,
};
use compiler_core::{PermissionCeiling, parse_and_validate};
use coordination::{ClaimScope, ClaimSnapshotV1, ReviewRef, ReviewScope};
use serde_json::json;
use std::collections::BTreeMap;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const OWNER_RELEASE: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CONTENDER_RELEASE: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const FIXTURE_DIGEST: &str =
    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn compiled(domain: &str, names: &[&str]) -> TestResult<CompiledPlan> {
    compiled_in_scope(domain, names, "cluster-a", "control", "workload")
}

fn compiled_in_scope(
    domain: &str,
    names: &[&str],
    cluster: &str,
    control_namespace: &str,
    workload_namespace: &str,
) -> TestResult<CompiledPlan> {
    let resources = names
        .iter()
        .map(|name| {
            json!({
                "id": name,
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "name": name,
                "namespace": workload_namespace,
                "desired": {
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": name, "namespace": workload_namespace},
                    "data": {"value": domain}
                },
                "lifecycle": {"deletionPolicy": "orphan"},
                "source": {"file": "program.tsx", "line": 1, "column": 1}
            })
        })
        .collect::<Vec<_>>();
    Ok(parse_and_validate(
        &json!({
            "schemaVersion": "reakt.dev/ir/v1",
            "testOnly": false,
            "domain": {
                "id": domain,
                "namespace": workload_namespace,
                "controlNamespace": control_namespace,
                "clusterBinding": cluster,
                "policyRef": format!("platform/{domain}"),
                "enforcement": "api-bound",
                "budgets": {
                    "maxResources": 8,
                    "maxInputs": 8,
                    "maxReactions": 8,
                    "maxObjectBytes": 262_144,
                    "maxQueueDepth": 8,
                    "maxConcurrentRequests": 4
                }
            },
            "resources": resources,
            "inputs": [],
            "reactions": [],
            "provenance": {
                "compiler": "reakt/0.1",
                "inputs": [{
                    "path": "program.tsx",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }]
            }
        })
        .to_string(),
    )?)
}

fn policy(compiled: &CompiledPlan) -> PolicyState {
    let domain = &compiled.ir.domain;
    let ceiling = PermissionCeiling {
        policy_ref: domain.policy_ref.clone(),
        domain_id: domain.id.clone(),
        cluster_binding: domain.cluster_binding.clone(),
        workload_namespace: domain.namespace.clone(),
        control_namespace: domain.control_namespace.clone(),
        capabilities: compiled.capabilities.clone(),
        accepted_enforcement_gaps: compiled.enforcement_gaps.clone(),
        accepted_indirect_risks: compiled.indirect_risks.clone(),
    };
    PolicyState {
        image_pull_secrets: vec![],
        schema_version: SCHEMA_VERSION.into(),
        runtime_ceiling: ceiling.clone(),
        cleanup_ceiling: ceiling,
    }
}

fn scope() -> ClaimScope {
    ClaimScope {
        registry_uid: REGISTRY_UID.into(),
        cluster_binding: "cluster-a".into(),
        control_namespace: "control".into(),
        workload_namespace: "workload".into(),
    }
}

fn history(domain: &str, revision: &str) -> deploy::GenerationHistory {
    deploy::GenerationHistory {
        domain_id: domain.into(),
        cluster_binding: "cluster-a".into(),
        revision: revision.into(),
        active_allocation: None,
    }
}

fn executor_binding(allocation: &GenerationAllocation, domain: &str) -> ExecutorBinding {
    ExecutorBinding {
        schema_version: EXECUTOR_BINDING_SCHEMA.into(),
        scope: scope(),
        domain_id: domain.into(),
        transition_id: allocation.transition_id.clone(),
        sequence: allocation.sequence,
        executor_id: "collision-owner".into(),
        identity_approval_uid: "identity-uid".into(),
        identity_approval_resource_version: "7".into(),
        identity_approval_digest: FIXTURE_DIGEST.into(),
        service_account_name: allocation.service_account_name.clone(),
        service_account_uid: "service-account-uid".into(),
        binding_secret_name: "binding-secret".into(),
        binding_secret_uid: "binding-secret-uid".into(),
        audience: "https://kubernetes.default.svc".into(),
        reservation_credential_expires_at: "2099-01-01T00:00:00Z".into(),
    }
}

fn fixture() -> TestResult<(
    Versioned<NamespaceRegistry>,
    DeploymentRequest,
    CompiledPlan,
    CompiledPlan,
)> {
    let owner = compiled("owner", &["owner-only", "shared"])?;
    let contender = compiled("contender", &["contender-only", "shared"])?;
    let owner_names = deploy::allocation_names(REGISTRY_UID, 1)?;
    let empty = ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
        scope: scope(),
        claims: vec![],
    };
    let owner_history = history("owner", "41");
    let mut frozen_owner_history = owner_history.clone();
    frozen_owner_history.revision = "42".into();
    let owner_policy = policy(&owner);
    let owner_request = DeploymentRequest {
        transition_id: owner_names.transition_id.clone(),
        previous: None,
        next: GenerationPlan {
            generation_id: owner_names.generation_id.clone(),
            artifact_digest: OWNER_RELEASE.into(),
            compiled: owner.clone(),
        },
        runtime_ceiling: owner_policy.runtime_ceiling.clone(),
        cleanup_ceiling: owner_policy.cleanup_ceiling.clone(),
        registry: deploy::RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 1,
            revision: "42".into(),
            claims: vec![],
            generation_history: frozen_owner_history,
        },
        observations: vec![],
        ownership_intents: vec![],
    };
    let plan = deploy::plan_transition(&owner_request)?;
    let record = deploy::begin(&plan, "collision-owner")?;
    let inputs = TransitionInputs::freeze(&plan, empty.reference()?, &owner_policy)?;
    let allocation = plan.generation_reservation.clone();
    let stored = StoredTransition::reserved(
        inputs,
        record,
        capacity(),
        ReviewRef {
            scope: ReviewScope {
                claim_scope: scope(),
                domain_id: "owner".into(),
                transition_id: owner_names.transition_id.clone(),
                sequence: 1,
            },
            digest: FIXTURE_DIGEST.into(),
        },
        None,
        executor_binding(&allocation, "owner"),
    );
    let owner_identities = identities(&owner)?;
    let claims = owner_claims(&owner_identities, "owner", &owner_names);
    let snapshot = ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
        scope: scope(),
        claims: claims.clone(),
    };
    let state = NamespaceRegistryState {
        registry_uid: REGISTRY_UID.into(),
        next_sequence: 2,
        schema_version: REGISTRY_SCHEMA_VERSION.into(),
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claim_snapshot: snapshot.reference()?,
        histories: BTreeMap::from([
            ("owner".into(), owner_history),
            ("contender".into(), history("contender", "41")),
        ]),
        active_writer_checkpoints: BTreeMap::new(),
        audit_heads: BTreeMap::new(),
        transitions: BTreeMap::from([(owner_names.transition_id, stored)]),
    };
    let current = Versioned {
        value: state.hydrate(&snapshot)?,
        uid: REGISTRY_UID.into(),
        resource_version: "43".into(),
    };
    let request = contender_request(&contender, claims)?;
    Ok((current, request, owner, contender))
}

fn contender_request(
    contender: &CompiledPlan,
    claims: Vec<ProtectedClaim>,
) -> TestResult<DeploymentRequest> {
    let names = deploy::allocation_names(REGISTRY_UID, 2)?;
    let contender_policy = policy(contender);
    let mut contender_history = history("contender", "41");
    contender_history.revision = "43".into();
    Ok(DeploymentRequest {
        transition_id: names.transition_id,
        previous: None,
        next: GenerationPlan {
            generation_id: names.generation_id,
            artifact_digest: CONTENDER_RELEASE.into(),
            compiled: contender.clone(),
        },
        runtime_ceiling: contender_policy.runtime_ceiling,
        cleanup_ceiling: contender_policy.cleanup_ceiling,
        registry: deploy::RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 2,
            revision: "43".into(),
            claims,
            generation_history: contender_history,
        },
        observations: vec![],
        ownership_intents: vec![],
    })
}

fn capacity() -> CapacityAdmission {
    CapacityAdmission {
        schema_version: crate::state_capacity::CAPACITY_SCHEMA.into(),
        revision: 0,
        profile: crate::completion_envelope::initial_profile(),
        domain_bytes: 1,
        activation_bytes: 1,
        inventory_bytes: 1,
    }
}

fn rejected(result: Result<()>) -> bool {
    matches!(result, Err(crate::Error::Invalid(message)) if message == INVALID_STATE)
}

fn validate_fixture(
    current: &Versioned<NamespaceRegistry>,
    request: &DeploymentRequest,
    owner: &CompiledPlan,
    contender: &CompiledPlan,
) -> Result<()> {
    validate_compiled_collision_state(
        current,
        request,
        owner,
        OWNER_RELEASE,
        contender,
        CONTENDER_RELEASE,
    )
}

fn fixture_predecessor(
    current: &Versioned<NamespaceRegistry>,
    owner: &CompiledPlan,
) -> TestResult<Box<deploy::PredecessorGeneration>> {
    let stored = current
        .value
        .transitions
        .values()
        .next()
        .ok_or("fixture transition missing")?;
    let generation = GenerationPlan {
        generation_id: stored.inputs.allocation.generation_id.clone(),
        artifact_digest: OWNER_RELEASE.into(),
        compiled: owner.clone(),
    };
    let (scope, inventory) = deploy::generation_inventory(&generation, &[])?;
    Ok(Box::new(deploy::PredecessorGeneration {
        allocation: stored.inputs.allocation.clone(),
        scope,
        control_namespace: "control".into(),
        inventory,
        retirement_role_binding_namespaces: vec![],
    }))
}

fn fixture_intent(request: &DeploymentRequest) -> TestResult<deploy::OwnershipIntent> {
    let claim = request
        .registry
        .claims
        .first()
        .ok_or("fixture claim missing")?;
    Ok(deploy::OwnershipIntent {
        object_name: "approval".into(),
        object_uid: "approval-uid".into(),
        object_resource_version: "1".into(),
        approval: deploy::OwnershipApproval {
            schema_version: deploy::OWNERSHIP_INTENT_SCHEMA.into(),
            transition_id: request.transition_id.clone(),
            domain_id: request.next.compiled.ir.domain.id.clone(),
            generation_id: request.next.generation_id.clone(),
            release_digest: request.next.artifact_digest.clone(),
            cluster_binding: "cluster-a".into(),
            control_namespace: "control".into(),
            identity: claim.identity.clone(),
            observed_uid: "observed-uid".into(),
            observed_resource_version: "1".into(),
            prior_managed_fields_digest: FIXTURE_DIGEST.into(),
            permitted_apply: json!({}),
            permitted_runtime_fields: vec![],
            deletion_policy: compiler_core::DeletionPolicy::Orphan,
            prior_manager_conflicts: vec![],
            action: deploy::OwnershipAction::Adopt,
        },
    })
}

#[test]
fn accepts_one_exact_planned_owner_and_one_overlap() -> TestResult {
    let (current, request, owner, contender) = fixture()?;
    let owner_history = current
        .value
        .histories
        .get("owner")
        .ok_or("owner history missing")?;
    let stored = current
        .value
        .transitions
        .values()
        .next()
        .ok_or("fixture transition missing")?;
    assert_eq!(owner_history.revision, "41");
    assert_eq!(stored.inputs.registry.revision, "42");
    assert_eq!(stored.inputs.registry.generation_history.revision, "42");
    assert_eq!(current.resource_version, "43");
    assert_eq!(request.registry.revision, "43");
    assert_eq!(request.registry.generation_history.revision, "43");
    validate_fixture(&current, &request, &owner, &contender)?;
    Ok(())
}

#[test]
fn rejects_claim_stage_and_contender_request_drift() -> TestResult {
    let (current, request, owner, contender) = fixture()?;
    let mut changed_claim = current.clone();
    changed_claim.value.claims[0].uid = Some("unexpected-uid".into());
    assert!(rejected(validate_compiled_collision_state(
        &changed_claim,
        &request,
        &owner,
        OWNER_RELEASE,
        &contender,
        CONTENDER_RELEASE,
    )));

    let mut staged = current.clone();
    let transition = staged
        .value
        .transitions
        .values_mut()
        .next()
        .ok_or("fixture transition missing")?;
    transition
        .staged_identity_uids
        .insert("deployment".into(), "uid".into());
    assert!(rejected(validate_compiled_collision_state(
        &staged,
        &request,
        &owner,
        OWNER_RELEASE,
        &contender,
        CONTENDER_RELEASE,
    )));

    let mut changed_request = request.clone();
    changed_request.registry.next_sequence = 3;
    assert!(rejected(validate_compiled_collision_state(
        &current,
        &changed_request,
        &owner,
        OWNER_RELEASE,
        &contender,
        CONTENDER_RELEASE,
    )));
    Ok(())
}

#[test]
fn rejects_non_distinct_or_non_overlapping_candidates() -> TestResult {
    let (current, request, owner, _) = fixture()?;
    assert!(rejected(validate_compiled_collision_state(
        &current,
        &request,
        &owner,
        OWNER_RELEASE,
        &owner,
        OWNER_RELEASE,
    )));
    let disjoint = compiled("contender", &["contender-only", "other"])?;
    assert!(rejected(validate_compiled_collision_state(
        &current,
        &request,
        &owner,
        OWNER_RELEASE,
        &disjoint,
        CONTENDER_RELEASE,
    )));
    assert!(is_claim_collision(&deploy::Error::Plan(
        CLAIM_COLLISION.into()
    )));
    assert!(!is_claim_collision(&deploy::Error::Plan(
        "different rejection".into()
    )));
    Ok(())
}

#[test]
fn rejects_multiple_overlap_or_candidate_scope_distinction_drift() -> TestResult {
    let (current, request, owner, contender) = fixture()?;
    let multiple = compiled("contender", &["owner-only", "shared"])?;
    assert!(rejected(validate_fixture(
        &current, &request, &owner, &multiple,
    )));

    let wrong_scope = compiled_in_scope(
        "contender",
        &["contender-only", "shared"],
        "cluster-a",
        "control",
        "other-workload",
    )?;
    assert!(rejected(validate_fixture(
        &current,
        &request,
        &owner,
        &wrong_scope,
    )));

    let same_domain = compiled("owner", &["contender-only", "shared"])?;
    assert!(rejected(validate_fixture(
        &current,
        &request,
        &owner,
        &same_domain,
    )));
    assert!(rejected(validate_compiled_collision_state(
        &current,
        &request,
        &owner,
        OWNER_RELEASE,
        &contender,
        OWNER_RELEASE,
    )));
    Ok(())
}

#[test]
fn rejects_extra_missing_or_wrong_owner_claims() -> TestResult {
    let (current, request, owner, contender) = fixture()?;
    let mut extra = current.clone();
    let extra_claim = extra
        .value
        .claims
        .first()
        .cloned()
        .ok_or("fixture claim missing")?;
    extra.value.claims.push(extra_claim);
    assert!(rejected(validate_fixture(
        &extra, &request, &owner, &contender,
    )));

    let mut missing = current.clone();
    assert!(missing.value.claims.pop().is_some());
    assert!(rejected(validate_fixture(
        &missing, &request, &owner, &contender,
    )));

    let mut wrong = current.clone();
    let claim = wrong
        .value
        .claims
        .first_mut()
        .ok_or("fixture claim missing")?;
    claim.state = ClaimState::Active;
    assert!(rejected(validate_fixture(
        &wrong, &request, &owner, &contender,
    )));
    Ok(())
}

#[test]
fn rejects_extra_missing_or_wrong_history_and_transition() -> TestResult {
    let (current, request, owner, contender) = fixture()?;
    let mut extra_history = current.clone();
    extra_history
        .value
        .histories
        .insert("extra".into(), history("extra", "41"));
    assert!(rejected(validate_fixture(
        &extra_history,
        &request,
        &owner,
        &contender,
    )));

    let mut missing_history = current.clone();
    assert!(missing_history.value.histories.remove("owner").is_some());
    assert!(rejected(validate_fixture(
        &missing_history,
        &request,
        &owner,
        &contender,
    )));

    let mut wrong_history = current.clone();
    let history = wrong_history
        .value
        .histories
        .get_mut("owner")
        .ok_or("owner history missing")?;
    history.domain_id = "wrong-owner".into();
    assert!(rejected(validate_fixture(
        &wrong_history,
        &request,
        &owner,
        &contender,
    )));

    let mut extra_transition = current.clone();
    let transition = extra_transition
        .value
        .transitions
        .values()
        .next()
        .cloned()
        .ok_or("fixture transition missing")?;
    extra_transition
        .value
        .transitions
        .insert("extra-transition".into(), transition);
    assert!(rejected(validate_fixture(
        &extra_transition,
        &request,
        &owner,
        &contender,
    )));

    let mut missing_transition = current.clone();
    missing_transition.value.transitions.clear();
    assert!(rejected(validate_fixture(
        &missing_transition,
        &request,
        &owner,
        &contender,
    )));
    Ok(())
}

#[test]
fn rejects_non_planned_or_staged_owner_and_contender_predecessor_inputs() -> TestResult {
    let (current, request, owner, contender) = fixture()?;
    let mut non_planned = current.clone();
    let transition = non_planned
        .value
        .transitions
        .values_mut()
        .next()
        .ok_or("fixture transition missing")?;
    transition.record.phase = deploy::Phase::Staged;
    assert!(rejected(validate_fixture(
        &non_planned,
        &request,
        &owner,
        &contender,
    )));

    let mut predecessor = request.clone();
    predecessor.previous = Some(fixture_predecessor(&current, &owner)?);
    assert!(rejected(validate_fixture(
        &current,
        &predecessor,
        &owner,
        &contender,
    )));

    let mut observed = request.clone();
    observed.observations.push(deploy::RuntimeObservation {
        identity: request.registry.claims[0].identity.clone(),
        uid: "observed-uid".into(),
    });
    assert!(rejected(validate_fixture(
        &current, &observed, &owner, &contender,
    )));

    let mut intent = request.clone();
    intent.ownership_intents.push(fixture_intent(&request)?);
    assert!(rejected(validate_fixture(
        &current, &intent, &owner, &contender,
    )));
    Ok(())
}

#[test]
fn owner_plan_replay_accepts_exact_inputs_and_rejects_plan_or_policy_drift() -> TestResult {
    let (current, _, owner, _) = fixture()?;
    let stored = current
        .value
        .transitions
        .values()
        .next()
        .ok_or("fixture transition missing")?;
    let owner_policy = policy(&owner);
    let plan = super::super::collision_owner_plan(stored, &owner, OWNER_RELEASE, &owner_policy)?;
    assert_eq!(plan.generation_reservation, stored.inputs.allocation);
    assert_eq!(plan.plan_digest, stored.inputs.plan_digest);

    let mut changed_plan = stored.clone();
    changed_plan.inputs.plan_digest = FIXTURE_DIGEST.into();
    assert!(
        super::super::collision_owner_plan(&changed_plan, &owner, OWNER_RELEASE, &owner_policy,)
            .is_err()
    );

    let mut changed_policy = owner_policy;
    changed_policy.runtime_ceiling.cluster_binding = "other-cluster".into();
    assert!(
        super::super::collision_owner_plan(stored, &owner, OWNER_RELEASE, &changed_policy,)
            .is_err()
    );
    Ok(())
}
