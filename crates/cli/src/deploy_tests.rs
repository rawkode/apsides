#[path = "review_adoption_tests.rs"]
mod review_adoption;
#[path = "review_contract_tests.rs"]
mod review_contract;

use super::*;
use deploy::{GenerationHistory, begin, plan_transition};
use platform::{NamespaceRegistry, RuntimeBinding, StoredTransition};

const BUILD: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const GENERATION_1: &str = "gceirceircfardairceirceirce-1";
const GENERATION_2: &str = "gceirceircfardairceirceirce-2";
const TRANSITION_1: &str = "tceirceircfardairceirceirce-1";
const TRANSITION_2: &str = "tceirceircfardairceirceirce-2";
const SERVICE_ACCOUNT_1: &str = "rceirceircfardairceirceirce-1";

fn routing_capacity() -> platform::CapacityAdmission {
    platform::CapacityAdmission {
        schema_version: "apsides.dev/capacity-admission/v1".into(),
        revision: 0,
        profile: platform::EvidenceProfile {
            schema_version: "apsides.dev/evidence-profile/v1".into(),
            max_controller_pods: 1,
            max_regular_containers: 1,
            max_init_containers: 0,
            max_ephemeral_containers: 0,
            max_uid_json_bytes: 128,
            max_resource_version_json_bytes: 128,
            max_container_id_json_bytes: 256,
            max_drain_bytes: 131_072,
        },
        domain_bytes: 131_072,
        activation_bytes: 16_384,
        inventory_bytes: 262_144,
    }
}

fn routing_executor_binding(scope: coordination::ClaimScope) -> platform::ExecutorBinding {
    platform::ExecutorBinding {
        schema_version: "apsides.dev/executor-binding/v1".into(),
        scope,
        domain_id: "example".into(),
        transition_id: TRANSITION_1.into(),
        sequence: 1,
        executor_id: "job-1".into(),
        identity_approval_uid: "identity-uid".into(),
        identity_approval_resource_version: "1".into(),
        identity_approval_digest: BUILD.into(),
        service_account_name: "deployer".into(),
        service_account_uid: "deployer-uid".into(),
        binding_secret_name: "deployment-token-binding".into(),
        binding_secret_uid: "binding-uid".into(),
        audience: "kubernetes".into(),
        reservation_credential_expires_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn registry() -> Result<(NamespaceRegistry, deploy::TransitionPlan)> {
    let compiled = compiler_core::parse_and_validate(&json!({
        "testOnly":false,"schemaVersion":"apsides.dev/ir/v1",
        "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound",
        "budgets":{"maxResources":16,"maxInputs":16,"maxReactions":16,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":[],"inputs":[],"reactions":[],"provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    }).to_string())?;
    let ceiling = compiler_core::PermissionCeiling {
        policy_ref: "platform/example".into(),
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        control_namespace: "control".into(),
        capabilities: compiled.capabilities.clone(),
        accepted_enforcement_gaps: compiled.enforcement_gaps.clone(),
        accepted_indirect_risks: compiled.indirect_risks.clone(),
    };
    let mut history = GenerationHistory {
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        revision: "1".into(),
        active_allocation: None,
    };
    let request = DeploymentRequest {
        transition_id: TRANSITION_1.into(),
        previous: None,
        next: GenerationPlan {
            generation_id: GENERATION_1.into(),
            artifact_digest: BUILD.into(),
            compiled: compiled.clone(),
        },
        runtime_ceiling: ceiling.clone(),
        cleanup_ceiling: ceiling,
        registry: RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 1,
            revision: "1".into(),
            claims: vec![],
            generation_history: history.clone(),
        },
        observations: vec![],
        ownership_intents: vec![],
    };
    let plan = plan_transition(&request)?;
    let mut record = begin(&plan, "job-1")?;
    // Routing fixtures do not purport to be authenticated/replayable cluster records.
    record.phase = Phase::Committed;
    history.active_allocation = Some(plan.generation_reservation.clone());
    let inputs = frozen_inputs(&plan)?;
    let stored = StoredTransition {
        executor_binding: routing_executor_binding(inputs.registry.claim_snapshot.scope.clone()),
        next_review_metadata: platform::project_review_metadata(&plan)?.reference()?,
        previous_review_metadata: None,
        capacity: routing_capacity(),
        inputs,
        record,
        staged_identity_uids: BTreeMap::new(),
        staged_content_digests: BTreeMap::new(),
        runtime_binding: Some(RuntimeBinding {
            domain_id: "example".into(),
            cluster_binding: "cluster-a".into(),
            generation_id: GENERATION_1.into(),
            build_digest: BUILD.into(),
            ir_digest: compiled.ir_digest,
            policy_digest: "policy-1".into(),
            capability_digest: BUILD.into(),
            service_account_name: SERVICE_ACCOUNT_1.into(),
            controller_image: "image".into(),
        }),
        staged_trust_policy_digest: None,
        staged_provenance_digest: None,
        request_drain_qualification: None,
        proxy_identity: None,
        upgrade_drain: None,
        previous_writer_checkpoint: None,
    };
    let snapshot = coordination::ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
        scope: coordination::ClaimScope {
            registry_uid: REGISTRY_UID.into(),
            cluster_binding: "cluster-a".into(),
            control_namespace: "control".into(),
            workload_namespace: "workload".into(),
        },
        claims: vec![],
    };
    let registry = platform::NamespaceRegistryState {
        audit_heads: BTreeMap::new(),
        active_writer_checkpoints: BTreeMap::new(),
        schema_version: platform::REGISTRY_SCHEMA_VERSION.into(),
        registry_uid: REGISTRY_UID.into(),
        next_sequence: 2,
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claim_snapshot: snapshot.reference()?,
        histories: BTreeMap::from([("example".into(), history)]),
        transitions: BTreeMap::from([(TRANSITION_1.into(), stored)]),
    }
    .hydrate(&snapshot)?;
    Ok((registry, plan))
}

#[test]
fn active_release_is_idempotent_but_changed_policy_allocates_a_generation() -> Result<()> {
    let (registry, _) = registry()?;
    assert_eq!(
        default_transition(&registry, "example", BUILD, "policy-1")?,
        TRANSITION_1
    );
    assert_eq!(
        default_transition(&registry, "example", BUILD, "policy-2")?,
        TRANSITION_2
    );
    Ok(())
}

#[test]
fn pending_same_build_precedes_old_active_transition_and_rollback_is_fresh() -> Result<()> {
    let (mut registry, _) = registry()?;
    let mut next = registry.transition(TRANSITION_1)?.clone();
    next.inputs.allocation.transition_id = TRANSITION_2.into();
    next.inputs.allocation.generation_id = GENERATION_2.into();
    next.record.phase = Phase::Staged;
    registry.transitions.insert(TRANSITION_2.into(), next);
    assert_eq!(
        default_transition(&registry, "example", BUILD, "policy-1")?,
        TRANSITION_2
    );
    let next = registry
        .transitions
        .get_mut(TRANSITION_2)
        .ok_or("pending transition missing")?;
    next.record.phase = Phase::Committed;
    next.inputs.allocation.artifact_digest = OTHER.into();
    next.runtime_binding
        .as_mut()
        .ok_or("pending runtime binding missing")?
        .generation_id = GENERATION_2.into();
    let mut allocation = next.inputs.allocation.clone();
    allocation.sequence = 2;
    allocation.generation_id = GENERATION_2.into();
    allocation.transition_id = TRANSITION_2.into();
    allocation.service_account_name = "rceirceircfardairceirceirce-2".into();
    registry
        .histories
        .get_mut("example")
        .ok_or("domain history missing")?
        .active_allocation = Some(allocation);
    registry.next_sequence = 3;
    assert_eq!(
        default_transition(&registry, "example", BUILD, "policy-1")?,
        "tceirceircfardairceirceirce-3"
    );
    Ok(())
}

fn frozen_inputs(plan: &deploy::TransitionPlan) -> Result<platform::TransitionInputs> {
    let request = &plan.request;
    let domain = &request.next.compiled.ir.domain;
    let snapshot = coordination::ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
        scope: coordination::ClaimScope {
            registry_uid: request.registry.registry_uid.clone(),
            cluster_binding: domain.cluster_binding.clone(),
            control_namespace: domain.control_namespace.clone(),
            workload_namespace: domain.namespace.clone(),
        },
        claims: request.registry.claims.clone(),
    };
    Ok(platform::TransitionInputs::freeze(
        plan,
        snapshot.reference()?,
        &platform::PolicyState {
            image_pull_secrets: vec![],
            schema_version: "apsides.dev/platform-policy/v1".into(),
            runtime_ceiling: request.runtime_ceiling.clone(),
            cleanup_ceiling: request.cleanup_ceiling.clone(),
        },
    )?)
}

#[test]
fn review_requires_exact_replay_and_predecessor_checkpoint() -> Result<()> {
    let (registry, original_plan) = registry()?;
    let mut original = registry.transition(TRANSITION_1)?.clone();
    // The routing fixture marks Committed without receipts. The report renderer
    // must reject it, even though routing itself only reads identity metadata.
    assert!(
        crate::review::transition_review_fixture(&original, &original_plan, TRANSITION_1, None)
            .is_err()
    );
    original.record = begin(&original_plan, "job-1")?;
    let initial =
        crate::review::transition_review_fixture(&original, &original_plan, TRANSITION_1, None)?;
    assert_eq!(initial["reactions"]["before"], json!([]));

    let mut request = original_plan.request.clone();
    request.transition_id = TRANSITION_2.into();
    request.next.generation_id = GENERATION_2.into();
    request.registry.next_sequence = registry.next_sequence;
    request.registry.generation_history = registry
        .histories
        .get("example")
        .ok_or("history missing")?
        .clone();
    request.previous = Some(Box::new(deploy::predecessor_metadata(
        &original_plan.request.next,
        &original_plan.generation_reservation,
        &request.registry.claims,
    )?));
    let plan = plan_transition(&request)?;
    let previous_review = platform::project_review_metadata(&original_plan)?;
    let mut upgrade = original;
    upgrade.next_review_metadata = platform::project_review_metadata(&plan)?.reference()?;
    upgrade.previous_review_metadata = Some(previous_review.reference()?);
    upgrade.record = begin(&plan, "job-2")?;
    upgrade.inputs = frozen_inputs(&plan)?;
    assert!(
        crate::review::transition_review_fixture(
            &upgrade,
            &plan,
            TRANSITION_2,
            Some(&previous_review)
        )
        .is_err()
    );
    let predecessor = plan.request.predecessor().ok_or("predecessor missing")?;
    upgrade.previous_writer_checkpoint = Some(review_checkpoint(predecessor)?);
    let review = crate::review::transition_review_fixture(
        &upgrade,
        &plan,
        TRANSITION_2,
        Some(&previous_review),
    )?;
    assert_eq!(review["schemaVersion"], "apsides.dev/deployment-review/v4");
    assert_eq!(
        review["detailAvailability"],
        "authenticated-declaration-metadata"
    );
    assert_eq!(
        review["permissions"]["runtime"]["after"],
        serde_json::to_value(&plan.request.next.compiled.capabilities)?
    );
    assert_eq!(
        review["permissions"]["runtime"]["before"],
        serde_json::to_value(&previous_review.runtime_permissions)?
    );
    assert_eq!(review["permissions"]["runtime"]["added"], json!([]));
    let checkpoint = upgrade
        .previous_writer_checkpoint
        .as_mut()
        .ok_or("checkpoint missing")?;
    checkpoint.allocation.artifact_digest = OTHER.into();
    assert!(
        crate::review::transition_review_fixture(
            &upgrade,
            &plan,
            TRANSITION_2,
            Some(&previous_review)
        )
        .is_err()
    );
    Ok(())
}

fn review_checkpoint(
    predecessor: &deploy::PredecessorGeneration,
) -> Result<platform::WriterCheckpoint> {
    let allocation = &predecessor.allocation;
    let name = &allocation.service_account_name;
    let generation = &allocation.generation_id;
    let control = &predecessor.control_namespace;
    let mut retirement = BTreeMap::from([(
        format!("v1/ServiceAccount/{control}/{name}"),
        "credential-uid".into(),
    )]);
    for namespace in &predecessor.retirement_role_binding_namespaces {
        retirement.insert(
            format!("rbac.authorization.k8s.io/v1/RoleBinding/{namespace}/{name}"),
            format!("{namespace}-role-uid"),
        );
    }
    let checkpoint = platform::WriterCheckpoint {
        schema_version: "apsides.dev/writer-checkpoint/v1".into(),
        allocation: allocation.clone(),
        control_namespace: control.clone(),
        runtime_binding: RuntimeBinding {
            domain_id: predecessor.scope.controller.clone(),
            cluster_binding: predecessor.scope.cluster.clone(),
            generation_id: generation.clone(),
            build_digest: allocation.artifact_digest.clone(),
            ir_digest: allocation.ir_digest.clone(),
            policy_digest: BUILD.into(),
            capability_digest: BUILD.into(),
            service_account_name: name.clone(),
            controller_image: format!("registry.example/controller@{BUILD}"),
        },
        request_drain_qualification: platform::RequestDrainQualification::IsolatedEnvoy {
            qualification_digest: BUILD.into(),
            node_image_digest: OTHER.into(),
        },
        proxy_identity: platform::ProxyIdentity {
            policy_digest: BUILD.into(),
            deployment_name: format!("apsides-proxy-{generation}"),
            deployment_uid: "proxy-deployment-uid".into(),
            node_name: "worker".into(),
            node_uid: "node-uid".into(),
            pod_name: "proxy-pod".into(),
            pod_uid: "pod-uid".into(),
            pod_spec_digest: BUILD.into(),
            envoy_container_id: "containerd://envoy".into(),
            admin_container_id: "containerd://admin".into(),
            config_map_name: format!("apsides-proxy-{generation}-config"),
            config_map_uid: "config-uid".into(),
            config_digest: BUILD.into(),
            qualification_digest: BUILD.into(),
        },
        controller_deployment_uid: "controller-uid".into(),
        activation_uid: "activation-uid".into(),
        inventory_uid: "inventory-uid".into(),
        retirement_identity_uids: retirement,
    };
    checkpoint.validate(
        predecessor,
        control,
        &predecessor.scope.namespace,
        &predecessor.scope.cluster,
    )?;
    Ok(checkpoint)
}

#[test]
fn capacity_recovery_targets_only_untouched_upgrade_phases() -> Result<()> {
    let (registry, _) = registry()?;
    let mut stored = registry.transition(TRANSITION_1)?.clone();
    stored.record.phase = Phase::Planned;
    assert!(capacity::require_target(&ExistingTransition::Resume, Some(&stored)).is_err());
    // Routing metadata is deliberately independent of release authentication.
    stored.inputs.previous = Some(platform::FrozenPredecessor {
        allocation: stored.allocation().clone(),
        scope: inventory::InventoryScope {
            cluster: "cluster-a".into(),
            namespace: "workload".into(),
            controller: "example".into(),
        },
        control_namespace: "control".into(),
        snapshot_id: BUILD.into(),
        retirement_role_binding_namespaces: vec!["control".into()],
    });
    for phase in [Phase::Planned, Phase::Staged] {
        stored.record.phase = phase;
        capacity::require_target(&ExistingTransition::Resume, Some(&stored))?;
    }
    for phase in [
        Phase::Quiesced,
        Phase::Activated,
        Phase::Cleaning,
        Phase::Committed,
        Phase::Blocked,
    ] {
        stored.record.phase = phase;
        assert!(capacity::require_target(&ExistingTransition::Resume, Some(&stored)).is_err());
    }
    Ok(())
}
