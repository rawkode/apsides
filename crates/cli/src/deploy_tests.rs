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

fn registry() -> NamespaceRegistry {
    let compiled = compiler_core::parse_and_validate(&json!({
        "testOnly":false,"schemaVersion":"reakt.dev/ir/v1",
        "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound",
        "budgets":{"maxResources":16,"maxInputs":16,"maxReactions":16,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":[],"inputs":[],"reactions":[],"provenance":{"compiler":"reakt/0.1","inputs":[{"path":"program.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    }).to_string()).unwrap();
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
    let plan = plan_transition(&request).unwrap();
    let mut record = begin(&plan, "job-1").unwrap();
    // Routing fixtures do not purport to be authenticated/replayable cluster records.
    record.phase = Phase::Committed;
    history.active_allocation = Some(plan.generation_reservation.clone());
    let stored = StoredTransition {
        plan,
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
    NamespaceRegistry {
        schema_version: platform::SCHEMA_VERSION.into(),
        registry_uid: REGISTRY_UID.into(),
        next_sequence: 2,
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claims: vec![],
        histories: BTreeMap::from([("example".into(), history)]),
        transitions: BTreeMap::from([(TRANSITION_1.into(), stored)]),
    }
}

#[test]
fn active_release_is_idempotent_but_changed_policy_allocates_a_generation() -> Result<()> {
    let registry = registry();
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
    let mut registry = registry();
    let mut next = registry.transitions[TRANSITION_1].clone();
    next.plan.request.transition_id = TRANSITION_2.into();
    next.plan.request.next.generation_id = GENERATION_2.into();
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
    next.plan.request.next.artifact_digest = OTHER.into();
    next.runtime_binding
        .as_mut()
        .ok_or("pending runtime binding missing")?
        .generation_id = GENERATION_2.into();
    let mut allocation = next.plan.generation_reservation.clone();
    allocation.sequence = 2;
    allocation.generation_id = GENERATION_2.into();
    allocation.transition_id = TRANSITION_2.into();
    allocation.service_account_name = "rceirceircfardairceirceirce-2".into();
    registry
        .histories
        .get_mut("example")
        .unwrap()
        .active_allocation = Some(allocation);
    registry.next_sequence = 3;
    assert_eq!(
        default_transition(&registry, "example", BUILD, "policy-1")?,
        "tceirceircfardairceirceirce-3"
    );
    Ok(())
}

#[test]
fn review_uses_metadata_predecessor_but_rejects_unverified_historical_details() -> Result<()> {
    let mut registry = registry();
    let original = registry.transition(TRANSITION_1)?.clone();
    let initial = crate::review::transition_review(&registry, TRANSITION_1)?;
    assert!(initial["reactions"]["before"].is_null());

    let mut request = original.plan.request.clone();
    request.transition_id = TRANSITION_2.into();
    request.next.generation_id = GENERATION_2.into();
    request.registry.registry_uid = registry.registry_uid.clone();
    request.registry.next_sequence = registry.next_sequence;
    request.registry.revision = "1".into();
    request.registry.generation_history = registry
        .histories
        .get("example")
        .ok_or("history missing")?
        .clone();
    request.previous = Some(deploy::PreviousGeneration::Metadata(Box::new(
        deploy::predecessor_metadata(
            &original.plan.request.next,
            &original.plan.generation_reservation,
            &request.registry.claims,
        )?,
    )));
    let plan = plan_transition(&request)?;
    let mut upgrade = original;
    upgrade.record = begin(&plan, "job-2")?;
    upgrade.plan = plan;
    registry.transitions.insert(TRANSITION_2.into(), upgrade);
    // The routing fixture marks its historical phase committed without receipts.
    // Review must verify that history before using its old desired fields.
    assert!(crate::review::transition_review(&registry, TRANSITION_2).is_err());
    registry.transitions.remove(TRANSITION_1);
    assert!(crate::review::transition_review(&registry, TRANSITION_2).is_err());
    Ok(())
}
