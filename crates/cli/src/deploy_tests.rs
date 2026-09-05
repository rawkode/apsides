use super::*;
use deploy::{GenerationHistory, begin, plan_transition};
use platform::{NamespaceRegistry, RuntimeBinding, StoredTransition};

const BUILD: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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
        active_generation_id: None,
        allocations: vec![],
    };
    let request = DeploymentRequest {
        transition_id: "install".into(),
        previous: None,
        next: GenerationPlan {
            generation_id: "g1".into(),
            artifact_digest: BUILD.into(),
            compiled: compiled.clone(),
        },
        runtime_ceiling: ceiling.clone(),
        cleanup_ceiling: ceiling,
        registry: RegistrySnapshot {
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
    history.active_generation_id = Some("g1".into());
    history
        .allocations
        .push(plan.generation_reservation.clone());
    let stored = StoredTransition {
        plan,
        record,
        staged_identity_uids: BTreeMap::new(),
        staged_content_digests: BTreeMap::new(),
        runtime_binding: Some(RuntimeBinding {
            domain_id: "example".into(),
            cluster_binding: "cluster-a".into(),
            generation_id: "g1".into(),
            build_digest: BUILD.into(),
            ir_digest: compiled.ir_digest,
            policy_digest: "policy-1".into(),
            capability_digest: BUILD.into(),
            service_account_name: "sa".into(),
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
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claims: vec![],
        histories: BTreeMap::from([("example".into(), history)]),
        transitions: BTreeMap::from([("install".into(), stored)]),
    }
}

#[test]
fn active_release_is_idempotent_but_changed_policy_allocates_a_generation() {
    let registry = registry();
    assert_eq!(
        default_transition(&registry, "example", BUILD, "policy-1"),
        "install"
    );
    assert!(default_transition(&registry, "example", BUILD, "policy-2").starts_with("deploy-g2-"));
}

#[test]
fn pending_same_build_precedes_old_active_transition_and_rollback_is_fresh() {
    let mut registry = registry();
    let mut next = registry.transitions["install"].clone();
    next.plan.request.transition_id = "pending".into();
    next.plan.request.next.generation_id = "g2".into();
    next.record.phase = Phase::Staged;
    registry.transitions.insert("pending".into(), next);
    assert_eq!(
        default_transition(&registry, "example", BUILD, "policy-1"),
        "pending"
    );
    let next = registry.transitions.get_mut("pending").unwrap();
    next.record.phase = Phase::Committed;
    next.plan.request.next.artifact_digest = OTHER.into();
    next.runtime_binding.as_mut().unwrap().generation_id = "g2".into();
    registry
        .histories
        .get_mut("example")
        .unwrap()
        .active_generation_id = Some("g2".into());
    let mut allocation = registry.histories["example"].allocations[0].clone();
    allocation.generation_id = "g2".into();
    registry
        .histories
        .get_mut("example")
        .unwrap()
        .allocations
        .push(allocation);
    assert!(default_transition(&registry, "example", BUILD, "policy-1").starts_with("deploy-g3-"));
}

#[test]
fn review_uses_metadata_predecessor_but_rejects_unverified_historical_details() -> Result<()> {
    let mut registry = registry();
    let original = registry.transition("install")?.clone();
    let initial = crate::review::transition_review(&registry, "install")?;
    assert!(initial["reactions"]["before"].is_null());

    let mut request = original.plan.request.clone();
    request.transition_id = "upgrade".into();
    request.next.generation_id = "g2".into();
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
    registry.transitions.insert("upgrade".into(), upgrade);
    // The routing fixture marks its historical phase committed without receipts.
    // Review must verify that history before using its old desired fields.
    assert!(crate::review::transition_review(&registry, "upgrade").is_err());
    registry.transitions.remove("install");
    assert!(crate::review::transition_review(&registry, "upgrade").is_err());
    Ok(())
}
