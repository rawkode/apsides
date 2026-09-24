use compiler_core::{Capability, DeletionPolicy, PermissionCeiling, parse_and_validate};
use deploy::*;
use serde_json::json;
use std::collections::BTreeSet;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const REGISTRY_TOKEN: &str = "ceirceircfardairceirceirce";

fn adoption_intent(request: &DeploymentRequest) -> OwnershipIntent {
    let target = &request.next;
    let resource = &target.compiled.ir.resources[0];
    OwnershipIntent {
        object_name: "operator-approval".into(),
        object_uid: "approval-uid".into(),
        object_resource_version: "17".into(),
        approval: OwnershipApproval {
            schema_version: OWNERSHIP_INTENT_SCHEMA.into(),
            transition_id: request.transition_id.clone(),
            domain_id: target.compiled.ir.domain.id.clone(),
            generation_id: target.generation_id.clone(),
            release_digest: target.artifact_digest.clone(),
            cluster_binding: target.compiled.ir.domain.cluster_binding.clone(),
            control_namespace: target.compiled.ir.domain.control_namespace.clone(),
            identity: identity(&resource.name),
            observed_uid: "existing-object-uid".into(),
            observed_resource_version: "123".into(),
            prior_managed_fields_digest: DIGEST.into(),
            permitted_apply: resource.desired.clone(),
            permitted_runtime_fields: vec![],
            deletion_policy: resource.lifecycle.deletion_policy,
            prior_manager_conflicts: vec![],
            action: OwnershipAction::Adopt,
        },
    }
}

#[test]
fn adoption_requires_exact_bound_fields_and_cannot_expand_authority() -> TestResult {
    let mut request = request(None, generation("g1", &[("existing", "orphan")])?);
    let baseline = plan_transition(&request)?;
    request.ownership_intents.push(adoption_intent(&request));
    let approved = plan_transition(&request)?;
    assert_ne!(approved.plan_digest, baseline.plan_digest);
    assert_eq!(
        approved.request.next.compiled.capabilities,
        baseline.request.next.compiled.capabilities
    );
    let mut changed = request.clone();
    changed.ownership_intents[0].approval.permitted_apply["data"]["value"] = json!("unapproved");
    assert!(plan_transition(&changed).is_err());
    changed = request.clone();
    changed.ownership_intents[0].approval.generation_id = "another-generation".into();
    assert!(plan_transition(&changed).is_err());
    changed = request.clone();
    changed.ownership_intents[0]
        .approval
        .prior_manager_conflicts
        .push("foreign-field-owner".into());
    assert!(plan_transition(&changed).is_err());
    changed = request.clone();
    changed.ownership_intents[0]
        .approval
        .permitted_runtime_fields
        .push("/data".into());
    assert!(plan_transition(&changed).is_err());
    request
        .ownership_intents
        .push(request.ownership_intents[0].clone());
    assert!(plan_transition(&request).is_err());
    Ok(())
}

#[test]
fn frozen_ownership_metadata_reconstructs_exact_approved_fields()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut request = request(None, generation("g1", &[("existing", "orphan")])?);
    request.ownership_intents.push(adoption_intent(&request));
    let frozen = freeze_ownership_intents(&request)?;
    let encoded = serde_json::to_string(&frozen)?;
    assert!(!encoded.contains("permittedApply"));
    assert!(!encoded.contains("permittedRuntimeFields"));
    assert!(!encoded.contains("deletionPolicy"));
    assert!(!encoded.contains("approved"));
    assert_eq!(
        restore_ownership_intents(&request.next, &request.transition_id, &frozen)?,
        request.ownership_intents
    );

    let mut invalid = frozen.clone();
    invalid
        .first_mut()
        .ok_or("frozen ownership intent missing")?
        .observed_uid
        .clear();
    assert!(restore_ownership_intents(&request.next, &request.transition_id, &invalid).is_err());
    invalid = frozen.clone();
    invalid.push(
        frozen
            .first()
            .ok_or("frozen ownership intent missing")?
            .clone(),
    );
    assert!(restore_ownership_intents(&request.next, &request.transition_id, &invalid).is_err());
    Ok(())
}

#[test]
fn empty_ownership_intents_preserve_existing_serialized_plan_identity() -> TestResult {
    let request = request(None, generation("g1", &[("config", "orphan")])?);
    let encoded = serde_json::to_value(&request)?;
    assert!(encoded.get("ownershipIntents").is_none());
    let restored: DeploymentRequest = serde_json::from_value(encoded)?;
    assert_eq!(plan_transition(&restored)?, plan_transition(&request)?);
    Ok(())
}

fn generation(id: &str, names: &[(&str, &str)]) -> TestResult<GenerationPlan> {
    let resources=names.iter().map(|(name,policy)|json!({"id":name,"apiVersion":"v1","kind":"ConfigMap","name":name,"namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"workload"},"data":{"value":"approved"}},"lifecycle":{"deletionPolicy":policy},"source":{"file":"program.ts","line":1,"column":1}})).collect::<Vec<_>>();
    let value = json!({"schemaVersion":"apsides.dev/ir/v1","domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":64,"maxInputs":64,"maxReactions":64,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},"resources":resources,"inputs":[],"reactions":[],"provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}});
    Ok(GenerationPlan {
        generation_id: id.into(),
        artifact_digest: DIGEST.into(),
        compiled: parse_and_validate(&value.to_string())?,
    })
}

fn ceiling(generation: &GenerationPlan) -> PermissionCeiling {
    let plan = &generation.compiled;
    PermissionCeiling {
        policy_ref: plan.ir.domain.policy_ref.clone(),
        domain_id: plan.ir.domain.id.clone(),
        cluster_binding: plan.ir.domain.cluster_binding.clone(),
        workload_namespace: plan.ir.domain.namespace.clone(),
        control_namespace: plan.ir.domain.control_namespace.clone(),
        capabilities: plan.capabilities.clone(),
        accepted_enforcement_gaps: plan.enforcement_gaps.clone(),
        accepted_indirect_risks: plan.indirect_risks.clone(),
    }
}

fn identity(name: &str) -> ResourceIdentity {
    ResourceIdentity {
        api_group: String::new(),
        resource: "configmaps".into(),
        namespace: "workload".into(),
        name: name.into(),
    }
}

fn fixture_names(sequence: u64) -> (String, String, String) {
    let suffix = format!("{REGISTRY_TOKEN}-{sequence}");
    (
        format!("g{suffix}"),
        format!("t{suffix}"),
        format!("r{suffix}"),
    )
}

fn allocation(generation: &GenerationPlan, sequence: u64) -> GenerationAllocation {
    let (_, transition_id, service_account_name) = fixture_names(sequence);
    GenerationAllocation {
        registry_uid: REGISTRY_UID.into(),
        sequence,
        generation_id: generation.generation_id.clone(),
        service_account_name,
        transition_id,
        artifact_digest: generation.artifact_digest.clone(),
        ir_digest: generation.compiled.ir_digest.clone(),
        inventory_metadata_digest: match generation_inventory(generation, &[])
            .and_then(|(scope, snapshot)| inventory_metadata_digest(&scope, &snapshot))
        {
            Ok(digest) => digest,
            Err(error) => format!("invalid-fixture:{error}"),
        },
    }
}

fn request(previous: Option<GenerationPlan>, next: GenerationPlan) -> DeploymentRequest {
    let next_sequence = if previous.is_some() { 2 } else { 1 };
    request_at_sequence(previous, next, next_sequence)
}

fn request_at_sequence(
    mut previous: Option<GenerationPlan>,
    mut next: GenerationPlan,
    next_sequence: u64,
) -> DeploymentRequest {
    let (generation_id, transition_id, _) = fixture_names(next_sequence);
    next.generation_id = generation_id;
    let previous_sequence = next_sequence.saturating_sub(1);
    let (_, previous_transition_id, _) = fixture_names(previous_sequence);
    if let Some(previous) = &mut previous {
        previous.generation_id = fixture_names(previous_sequence).0;
    }
    let runtime_ceiling = ceiling(&next);
    let mut cleanup_ceiling = ceiling(&next);
    let claims = previous.as_ref().map_or_else(Vec::new, |previous| {
        previous
            .compiled
            .ir
            .resources
            .iter()
            .map(|resource| ProtectedClaim {
                identity: identity(&resource.name),
                domain_id: "example".into(),
                generation_id: previous.generation_id.clone(),
                transition_id: previous_transition_id.clone(),
                state: ClaimState::Active,
                uid: Some(format!("uid-{}", resource.name)),
            })
            .collect()
    });
    for claim in &claims {
        for verb in ["get", "delete"] {
            cleanup_ceiling.capabilities.push(Capability {
                namespace: "workload".into(),
                api_group: String::new(),
                resource: "configmaps".into(),
                subresource: None,
                verb: verb.into(),
                names: vec![claim.identity.name.clone()],
            });
        }
    }
    let previous_allocation = previous
        .as_ref()
        .map(|previous| allocation(previous, previous_sequence));
    let generation_history = GenerationHistory {
        domain_id: next.compiled.ir.domain.id.clone(),
        cluster_binding: next.compiled.ir.domain.cluster_binding.clone(),
        revision: "rv-7".into(),
        active_allocation: previous_allocation.clone(),
    };
    let previous = previous
        .zip(previous_allocation)
        .map(|(generation, allocation)| {
            Box::new(fixture_predecessor(&generation, &allocation, &claims))
        });
    DeploymentRequest {
        transition_id,
        previous,
        next,
        runtime_ceiling,
        cleanup_ceiling,
        registry: RegistrySnapshot {
            registry_uid: REGISTRY_UID.into(),
            next_sequence,
            revision: "rv-7".into(),
            claims,
            generation_history,
        },
        observations: vec![],
        ownership_intents: vec![],
    }
}

fn fixture_predecessor(
    generation: &GenerationPlan,
    allocation: &GenerationAllocation,
    claims: &[ProtectedClaim],
) -> PredecessorGeneration {
    let domain = &generation.compiled.ir.domain;
    let mut resources = generation
        .compiled
        .ir
        .resources
        .iter()
        .map(|resource| {
            let uid = claims
                .iter()
                .find(|claim| claim.identity.name == resource.name)
                .and_then(|claim| claim.uid.clone());
            inventory::TrackedResource::from_resource(resource, "configmaps".into(), uid)
        })
        .collect::<Vec<_>>();
    resources.sort_by(|a, b| a.identity.cmp(&b.identity));
    PredecessorGeneration {
        allocation: allocation.clone(),
        scope: inventory::InventoryScope {
            cluster: domain.cluster_binding.clone(),
            namespace: domain.namespace.clone(),
            controller: domain.id.clone(),
        },
        control_namespace: domain.control_namespace.clone(),
        inventory: inventory::InventorySnapshot {
            schema_version: inventory::INVENTORY_SCHEMA.into(),
            generation: generation.generation_id.clone(),
            resources,
        },
        retirement_role_binding_namespaces: generation
            .compiled
            .capabilities
            .iter()
            .map(|capability| capability.namespace.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    }
}

fn predecessor_mut(
    request: &mut DeploymentRequest,
) -> std::result::Result<&mut PredecessorGeneration, &'static str> {
    request
        .previous
        .as_deref_mut()
        .ok_or("missing fixture predecessor")
}

fn events(plan: &TransitionPlan) -> Vec<Event> {
    let quiescence = plan.request.previous.as_ref().map_or(
        Quiescence::InitialInstallation {
            verification_epoch: 0,
        },
        |old| {
            Quiescence::OldGeneration(TerminationEvidence {
                generation_id: old.allocation.generation_id.clone(),
                executor_id: "executor".into(),
                verification_epoch: 0,
                environment_qualification_digest: DIGEST.into(),
                process_termination_receipt_digest: DIGEST.into(),
                access_revocation_receipt_digest: DIGEST.into(),
            })
        },
    );
    vec![
        Event::Staged {
            event_id: "staged".into(),
            generation_id: plan.request.next.generation_id.clone(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: DIGEST.into(),
            verification_epoch: 0,
        },
        Event::Quiesced {
            event_id: "quiesced".into(),
            evidence: quiescence,
        },
        Event::Activated {
            event_id: "activated".into(),
            generation_id: plan.request.next.generation_id.clone(),
            activation_receipt_digest: DIGEST.into(),
            verification_epoch: 0,
        },
        Event::Cleaning {
            event_id: "cleaning".into(),
        },
        Event::Committed {
            event_id: "committed".into(),
            cleanup_receipts: plan
                .cleanup
                .iter()
                .map(|item| CleanupReceipt {
                    identity: item.identity.clone(),
                    expected_uid: item.expected_uid.clone(),
                    outcome: if item.policy == DeletionPolicy::Orphan {
                        CleanupOutcome::OrphanReleased
                    } else {
                        CleanupOutcome::Deleted
                    },
                    evidence_digest: DIGEST.into(),
                })
                .collect(),
            obsolete_identity_retirement_digest: DIGEST.into(),
        },
    ]
}

fn planned_recovery_cycles(
    plan: &TransitionPlan,
    cycles: usize,
) -> std::result::Result<TransitionRecord, Box<dyn std::error::Error + Send + Sync>> {
    let mut record = begin(plan, "executor")?;
    for cycle in 0..cycles {
        record = advance(
            plan,
            &record,
            record.revision,
            "executor",
            Event::Blocked {
                event_id: format!("capacity-block-{cycle}"),
                reason: "bounded test failure".into(),
            },
        )?;
        record = advance(
            plan,
            &record,
            record.revision,
            "executor",
            Event::Resume {
                event_id: format!("capacity-resume-{cycle}"),
                recovery_receipt_digest: DIGEST.into(),
            },
        )?;
    }
    Ok(record)
}

fn stage_for_capacity(
    plan: &TransitionPlan,
    record: &TransitionRecord,
    event_id: &str,
) -> std::result::Result<TransitionRecord, Box<dyn std::error::Error + Send + Sync>> {
    Ok(advance(
        plan,
        record,
        record.revision,
        "executor",
        Event::Staged {
            event_id: event_id.into(),
            generation_id: plan.request.next.generation_id.clone(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: compiler_core::digest(&event_id)?,
            verification_epoch: record.verification_epoch,
        },
    )?)
}

fn completion_after_staging(
    plan: &TransitionPlan,
    record: &TransitionRecord,
) -> std::result::Result<(TransitionRecord, Event), Box<dyn std::error::Error + Send + Sync>> {
    let mut current = record.clone();
    let mut events = capacity_completion_events(plan, record.verification_epoch).into_iter();
    for event in events.by_ref().take(3) {
        current = advance(plan, &current, current.revision, "executor", event)?;
    }
    let committed = events.next().ok_or("capacity commit event missing")?;
    current = advance(
        plan,
        &current,
        current.revision,
        "executor",
        committed.clone(),
    )?;
    Ok((current, committed))
}

fn capacity_completion_events(plan: &TransitionPlan, epoch: u64) -> [Event; 4] {
    [
        Event::Quiesced {
            event_id: "capacity-quiesced".into(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: epoch,
            },
        },
        Event::Activated {
            event_id: "capacity-activated".into(),
            generation_id: plan.request.next.generation_id.clone(),
            activation_receipt_digest: DIGEST.into(),
            verification_epoch: epoch,
        },
        Event::Cleaning {
            event_id: "capacity-cleaning".into(),
        },
        Event::Committed {
            event_id: "capacity-committed".into(),
            cleanup_receipts: vec![],
            obsolete_identity_retirement_digest: DIGEST.into(),
        },
    ]
}

fn insufficient_staged(
    plan: &TransitionPlan,
) -> std::result::Result<TransitionRecord, Box<dyn std::error::Error + Send + Sync>> {
    let mut record = planned_recovery_cycles(plan, 124)?;
    record = stage_for_capacity(plan, &record, "capacity-stage-before-retry")?;
    for event in [
        Event::Blocked {
            event_id: "capacity-retry-block".into(),
            reason: "retry fixture".into(),
        },
        Event::Resume {
            event_id: "capacity-retry-resume".into(),
            recovery_receipt_digest: DIGEST.into(),
        },
    ] {
        record = advance(plan, &record, record.revision, "executor", event)?;
    }
    stage_for_capacity(plan, &record, "capacity-stage-insufficient")
}

fn assert_capacity(
    plan: &TransitionPlan,
    record: &TransitionRecord,
    expected: bool,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if require_event_capacity(plan, record).is_ok() == expected {
        Ok(())
    } else {
        Err("event capacity result differed from boundary expectation".into())
    }
}

#[test]
fn event_capacity_reserves_completion_and_failure_at_canonical_boundary()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let plan = plan_transition(&request(
        None,
        generation("capacity", &[("config", "orphan")])?,
    ))?;
    let mut exact = planned_recovery_cycles(&plan, 125)?;
    assert_eq!(exact.receipts.len(), 250);
    assert_capacity(&plan, &exact, true)?;
    exact = stage_for_capacity(&plan, &exact, "capacity-stage-exact")?;
    assert_eq!(exact.receipts.len(), 251);
    assert_capacity(&plan, &exact, true)?;
    for event in capacity_completion_events(&plan, exact.verification_epoch) {
        exact = advance(&plan, &exact, exact.revision, "executor", event)?;
        assert_capacity(&plan, &exact, true)?;
    }
    assert_eq!(exact.receipts.len(), 255);
    assert_eq!(exact.phase, Phase::Committed);

    let planned = planned_recovery_cycles(&plan, 125)?;
    let blocked = advance(
        &plan,
        &planned,
        planned.revision,
        "executor",
        Event::Blocked {
            event_id: "capacity-final-block".into(),
            reason: "last reserved failure receipt".into(),
        },
    )?;
    assert_eq!(blocked.receipts.len(), 251);
    assert_capacity(&plan, &blocked, false)?;
    let resumed = advance(
        &plan,
        &blocked,
        blocked.revision,
        "executor",
        Event::Resume {
            event_id: "capacity-final-resume".into(),
            recovery_receipt_digest: DIGEST.into(),
        },
    )?;
    assert_eq!(resumed.phase, Phase::Planned);
    assert_eq!(resumed.receipts.len(), 252);
    assert_capacity(&plan, &resumed, false)?;

    let mut insufficient = insufficient_staged(&plan)?;
    for event in capacity_completion_events(&plan, insufficient.verification_epoch) {
        assert_capacity(&plan, &insufficient, false)?;
        insufficient = advance(
            &plan,
            &insufficient,
            insufficient.revision,
            "executor",
            event,
        )?;
    }
    assert_eq!(insufficient.receipts.len(), 256);
    assert_eq!(insufficient.phase, Phase::Committed);
    assert_capacity(&plan, &insufficient, true)?;

    let mut corrupted = planned;
    corrupted
        .receipts
        .last_mut()
        .ok_or("capacity receipt missing")?
        .event_id
        .push_str("-corrupt");
    assert_capacity(&plan, &corrupted, false)?;
    Ok(())
}

#[test]
fn full_committed_history_remains_valid_and_idempotent()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let plan = plan_transition(&request(
        None,
        generation("capacity", &[("config", "orphan")])?,
    ))?;
    let staged = insufficient_staged(&plan)?;
    assert_eq!(staged.receipts.len(), 252);
    let (committed, event) = completion_after_staging(&plan, &staged)?;
    assert_eq!(committed.receipts.len(), 256);
    assert_eq!(committed.phase, Phase::Committed);
    require_event_capacity(&plan, &committed)?;
    assert_eq!(
        advance(&plan, &committed, 255, "executor", event)?,
        committed
    );
    Ok(())
}

#[test]
fn interruption_after_every_phase_resumes_from_serialized_durable_state() -> TestResult {
    let request = request(
        Some(generation(
            "generation-1",
            &[("remove", "delete"), ("keep", "orphan")],
        )?),
        generation("generation-2", &[("keep", "orphan")])?,
    );
    let plan = plan_transition(&request)?;
    let mut record = begin(&plan, "executor")?;
    let expected = [
        Phase::Staged,
        Phase::Quiesced,
        Phase::Activated,
        Phase::Cleaning,
        Phase::Committed,
    ];
    for (event, phase) in events(&plan).into_iter().zip(expected) {
        let before = record.revision;
        record = advance(&plan, &record, before, "executor", event.clone())?;
        assert_eq!(record.phase, phase);
        record = serde_json::from_slice(&serde_json::to_vec(&record)?)?;
        assert_eq!(
            advance(&plan, &record, before, "executor", event)?,
            record,
            "response-loss replay must not repeat external intent"
        );
    }
    assert_eq!(record.revision, 5);
    Ok(())
}

#[test]
fn rollback_reuses_build_only_with_new_allocation_identity()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let previous = generation("generation-1", &[("config", "orphan")])?;
    let next = generation("generation-2", &[("config", "orphan")])?;
    assert_eq!(previous.artifact_digest, next.artifact_digest);
    let plan = plan_transition(&request(Some(previous), next))?;
    let predecessor = plan.request.predecessor().ok_or("predecessor missing")?;
    assert_ne!(
        predecessor.allocation.generation_id,
        plan.generation_reservation.generation_id
    );
    assert_ne!(
        predecessor.allocation.service_account_name,
        plan.generation_reservation.service_account_name
    );
    assert_eq!(
        predecessor.allocation.artifact_digest,
        plan.generation_reservation.artifact_digest
    );
    Ok(())
}

#[test]
fn stale_cas_wrong_executor_and_evidence_reuse_cannot_advance() -> TestResult {
    let plan = plan_transition(&request(
        None,
        generation("gen-1", &[("config", "orphan")])?,
    ))?;
    let record = begin(&plan, "executor")?;
    let staged = events(&plan)[0].clone();
    assert!(advance(&plan, &record, 1, "executor", staged.clone()).is_err());
    assert!(advance(&plan, &record, 0, "other-executor", staged.clone()).is_err());
    let record = advance(&plan, &record, 0, "executor", staged)?;
    let altered = Event::Staged {
        event_id: "staged".into(),
        generation_id: "different".into(),
        service_account_name: "wrong".into(),
        registry_cas_receipt_digest: DIGEST.into(),
        verification_epoch: 0,
    };
    assert!(advance(&plan, &record, record.revision, "executor", altered).is_err());
    Ok(())
}

#[test]
fn old_writer_uncertainty_blocks_activation_and_cannot_be_skipped() -> TestResult {
    let plan = plan_transition(&request(
        Some(generation("gen-1", &[("config", "orphan")])?),
        generation("gen-2", &[("config", "orphan")])?,
    ))?;
    let record = begin(&plan, "executor")?;
    let events = events(&plan);
    let record = advance(&plan, &record, 0, "executor", events[0].clone())?;
    assert!(advance(&plan, &record, 1, "executor", events[2].clone()).is_err());
    assert!(
        advance(
            &plan,
            &record,
            1,
            "executor",
            Event::Quiesced {
                event_id: "unknown".into(),
                evidence: Quiescence::InitialInstallation {
                    verification_epoch: 0
                }
            }
        )
        .is_err()
    );
    let Event::Quiesced {
        evidence: Quiescence::OldGeneration(evidence),
        ..
    } = &events[1]
    else {
        return Err("quiesce step carries no old-generation evidence".into());
    };
    let mut evidence = evidence.clone();
    evidence.process_termination_receipt_digest.clear();
    assert!(
        advance(
            &plan,
            &record,
            1,
            "executor",
            Event::Quiesced {
                event_id: "missing-proof".into(),
                evidence: Quiescence::OldGeneration(evidence)
            }
        )
        .is_err()
    );
    let blocked = advance(
        &plan,
        &record,
        1,
        "executor",
        Event::Blocked {
            event_id: "blocked".into(),
            reason: "node termination uncertain".into(),
        },
    )?;
    assert_eq!(blocked.phase, Phase::Blocked);
    assert!(
        advance(
            &plan,
            &blocked,
            blocked.revision,
            "executor",
            events[2].clone()
        )
        .is_err()
    );
    let resumed = advance(
        &plan,
        &blocked,
        blocked.revision,
        "executor",
        Event::Resume {
            event_id: "resume".into(),
            recovery_receipt_digest: DIGEST.into(),
        },
    )?;
    assert_eq!(resumed.phase, Phase::Planned);
    assert!(
        advance(
            &plan,
            &resumed,
            resumed.revision,
            "executor",
            events[2].clone()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn previous_policy_and_protected_uid_govern_removal_not_runtime_inventory() -> TestResult {
    let mut request = request(
        Some(generation(
            "gen-1",
            &[("remove", "delete"), ("retain", "orphan")],
        )?),
        generation("gen-2", &[])?,
    );
    request.observations.push(RuntimeObservation {
        identity: identity("remove"),
        uid: "forged-runtime-uid".into(),
    });
    let plan = plan_transition(&request)?;
    let item = plan
        .cleanup
        .iter()
        .find(|item| item.identity.name == "remove")
        .ok_or("plan has no cleanup item for the removed resource")?;
    assert_eq!(item.expected_uid.as_deref(), Some("uid-remove"));
    assert_eq!(item.policy, DeletionPolicy::Delete);
    assert_eq!(
        plan.cleanup
            .iter()
            .find(|item| item.identity.name == "retain")
            .ok_or("plan has no cleanup item for the retained resource")?
            .policy,
        DeletionPolicy::Orphan
    );
    request
        .registry
        .claims
        .iter_mut()
        .find(|claim| claim.identity.name == "remove")
        .ok_or("registry has no claim for the removed resource")?
        .uid = None;
    if let Some(previous) = request.previous.as_mut() {
        for resource in previous
            .inventory
            .resources
            .iter_mut()
            .filter(|resource| resource.identity.name == "remove")
        {
            resource.uid = None;
        }
    }
    let absence_only = plan_transition(&request)?;
    assert!(
        absence_only
            .cleanup_capabilities
            .iter()
            .all(|cap| cap.verb == "get")
    );
    let item = absence_only
        .cleanup
        .iter()
        .find(|item| item.identity.name == "remove")
        .ok_or("plan has no cleanup item for the removed resource")?;
    assert!(
        item.expected_uid.is_none(),
        "runtime observation cannot grant deletion authority"
    );
    let record = cleaning_record(&absence_only)?;
    let mut claim = request
        .registry
        .claims
        .iter()
        .find(|claim| claim.identity.name == "remove")
        .ok_or("registry has no claim for the removed resource")?
        .clone();
    claim.transition_id = absence_only.request.transition_id.clone();
    claim.state = ClaimState::Releasing;
    assert_eq!(
        cleanup_decision(
            &absence_only,
            &record,
            &item.identity,
            &claim,
            &LiveCleanupObject::Absent
        )?,
        CleanupDecision::AlreadyAbsent
    );
    assert!(
        cleanup_decision(
            &absence_only,
            &record,
            &item.identity,
            &claim,
            &LiveCleanupObject::Present {
                uid: "forged-runtime-uid".into(),
                finalizers: vec![],
                deletion_started: false
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn cleanup_authorization_is_separate_and_old_resource_type_remains_in_plan() -> TestResult {
    let mut request = request(
        Some(generation("gen-1", &[("old-type", "delete")])?),
        generation("gen-2", &[])?,
    );
    let plan = plan_transition(&request)?;
    assert!(
        plan.cleanup_capabilities
            .iter()
            .any(|cap| cap.verb == "delete"
                && cap.resource == "configmaps"
                && cap.names == ["old-type"])
    );
    assert!(
        !plan
            .request
            .next
            .compiled
            .capabilities
            .iter()
            .any(|cap| cap.verb == "delete")
    );
    request
        .cleanup_ceiling
        .capabilities
        .retain(|cap| cap.verb != "delete");
    assert!(plan_transition(&request).is_err());
    Ok(())
}

#[test]
fn cleanup_refuses_replacement_uid_and_preserves_unknown_finalizers() -> TestResult {
    let request = request(
        Some(generation("gen-1", &[("remove", "delete")])?),
        generation("gen-2", &[])?,
    );
    let plan = plan_transition(&request)?;
    let item = &plan.cleanup[0];
    let record = cleaning_record(&plan)?;
    let mut claim = request.registry.claims[0].clone();
    claim.transition_id = plan.request.transition_id.clone();
    claim.state = ClaimState::Releasing;
    assert_eq!(
        cleanup_decision(
            &plan,
            &record,
            &item.identity,
            &claim,
            &LiveCleanupObject::Present {
                uid: "replacement".into(),
                finalizers: vec![],
                deletion_started: false
            }
        ),
        Err(Error::CleanupUidMismatch)
    );
    assert_eq!(
        cleanup_decision(
            &plan,
            &record,
            &item.identity,
            &claim,
            &LiveCleanupObject::Absent
        )?,
        CleanupDecision::AlreadyAbsent
    );
    let pending = LiveCleanupObject::Present {
        uid: "uid-remove".into(),
        finalizers: vec!["external/retain".into()],
        deletion_started: true,
    };
    assert_eq!(
        cleanup_decision(&plan, &record, &item.identity, &claim, &pending)?,
        CleanupDecision::WaitForFinalizers
    );
    let before_delete = LiveCleanupObject::Present {
        uid: "uid-remove".into(),
        finalizers: vec!["external/retain".into()],
        deletion_started: false,
    };
    assert_eq!(
        cleanup_decision(&plan, &record, &item.identity, &claim, &before_delete)?,
        CleanupDecision::DeleteWithUidPrecondition {
            uid: "uid-remove".into()
        }
    );
    Ok(())
}

#[test]
fn foreign_and_unfinished_claims_and_preexisting_targets_fail_closed() -> TestResult {
    let mut req = request(None, generation("gen-1", &[("config", "orphan")])?);
    req.registry.claims.push(ProtectedClaim {
        identity: identity("config"),
        domain_id: "foreign".into(),
        generation_id: "foreign-gen".into(),
        transition_id: "foreign-transition".into(),
        state: ClaimState::Active,
        uid: Some("foreign-uid".into()),
    });
    assert!(plan_transition(&req).is_err());
    req.registry.claims.clear();
    req.observations.push(RuntimeObservation {
        identity: identity("config"),
        uid: "existing".into(),
    });
    assert!(plan_transition(&req).is_err());
    let mut req = request(
        Some(generation("gen-1", &[("config", "orphan")])?),
        generation("gen-2", &[("config", "orphan")])?,
    );
    req.registry.claims[0].state = ClaimState::Reserved;
    assert!(plan_transition(&req).is_err());
    Ok(())
}

#[test]
fn forged_phase_without_evidence_history_and_incomplete_cleanup_cannot_commit() -> TestResult {
    let plan = plan_transition(&request(
        Some(generation("gen-1", &[("remove", "delete")])?),
        generation("gen-2", &[])?,
    ))?;
    let mut record = begin(&plan, "executor")?;
    record.phase = Phase::Cleaning;
    let mut steps = events(&plan);
    assert!(advance(&plan, &record, 0, "executor", steps[4].clone()).is_err());
    record = begin(&plan, "executor")?;
    for step in steps.drain(..4) {
        record = advance(&plan, &record, record.revision, "executor", step)?;
    }
    let missing = Event::Committed {
        event_id: "commit-missing".into(),
        cleanup_receipts: vec![],
        obsolete_identity_retirement_digest: DIGEST.into(),
    };
    assert!(advance(&plan, &record, record.revision, "executor", missing).is_err());
    Ok(())
}

fn cleaning_record(plan: &TransitionPlan) -> TestResult<TransitionRecord> {
    let mut record = begin(plan, "executor")?;
    for event in events(plan).into_iter().take(4) {
        record = advance(plan, &record, record.revision, "executor", event)?;
    }
    Ok(record)
}

#[test]
fn canonical_plan_replays_after_counter_advances_and_failed_identity_is_not_reused()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let first_request =
        request_at_sequence(None, generation("ignored", &[("config", "orphan")])?, 1);
    let first = plan_transition(&first_request)?;
    let first_names = allocation_names(REGISTRY_UID, 1)?;
    assert!(
        allocation_names(REGISTRY_UID, 36)?
            .generation_id
            .ends_with("-10")
    );
    assert_eq!(
        first.generation_reservation.generation_id,
        first_names.generation_id
    );
    assert_eq!(
        first.generation_reservation.transition_id,
        first_names.transition_id
    );
    assert_eq!(
        first.generation_reservation.service_account_name,
        first_names.service_account_name
    );

    // Sequence one may fail before commit, but the protected counter still moves.
    let retry_request =
        request_at_sequence(None, generation("ignored", &[("config", "orphan")])?, 2);
    let retry = plan_transition(&retry_request)?;
    assert_ne!(
        first.generation_reservation.generation_id,
        retry.generation_reservation.generation_id
    );
    assert_ne!(
        first.generation_reservation.transition_id,
        retry.generation_reservation.transition_id
    );
    assert_ne!(
        first.generation_reservation.service_account_name,
        retry.generation_reservation.service_account_name
    );

    // Advancing the external registry does not make the immutable first plan noncanonical.
    begin(&first, "executor")?;
    let mut reused = retry_request;
    reused.next.generation_id = first.generation_reservation.generation_id;
    assert!(plan_transition(&reused).is_err());
    Ok(())
}

#[test]
fn malformed_registry_and_exhausted_counter_are_rejected()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let baseline = request_at_sequence(None, generation("ignored", &[])?, 3);
    plan_transition(&baseline)?;

    let mut malformed = baseline.clone();
    malformed.registry.registry_uid = "not-a-registry-uuid".into();
    assert!(plan_transition(&malformed).is_err());
    let mut numeric_counter = serde_json::to_value(&baseline)?;
    numeric_counter
        .get_mut("registry")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("registry encoding missing")?
        .insert("nextSequence".into(), json!(3));
    assert!(serde_json::from_value::<DeploymentRequest>(numeric_counter).is_err());
    let mut missing_counter = serde_json::to_value(&baseline)?;
    missing_counter
        .get_mut("registry")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("registry encoding missing")?
        .remove("nextSequence");
    assert!(serde_json::from_value::<DeploymentRequest>(missing_counter).is_err());
    let mut zero = baseline.clone();
    zero.registry.next_sequence = 0;
    assert!(plan_transition(&zero).is_err());
    let mut exhausted = baseline;
    exhausted.registry.next_sequence = u64::MAX;
    let error = plan_transition(&exhausted)
        .err()
        .ok_or("exhausted allocation counter was accepted")?;
    assert!(error.to_string().contains("exhausted"));
    Ok(())
}

#[test]
fn missing_mismatched_or_scoped_active_allocation_is_rejected()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let previous = generation("generation-a", &[("config", "orphan")])?;
    let next = generation("generation-b", &[("config", "orphan")])?;
    let base = request(Some(previous), next);
    plan_transition(&base)?;
    let mut missing = base.clone();
    missing.registry.generation_history.active_allocation = None;
    assert!(plan_transition(&missing).is_err());
    let mut reused_transition = base.clone();
    reused_transition.transition_id = reused_transition
        .registry
        .generation_history
        .active_allocation
        .as_ref()
        .ok_or("active allocation missing")?
        .transition_id
        .clone();
    assert!(
        plan_transition(&reused_transition).is_err(),
        "a new generation cannot reuse a historical transition record identity"
    );
    let mut foreign = base.clone();
    foreign.registry.generation_history.domain_id = "foreign".into();
    assert!(plan_transition(&foreign).is_err());
    let mut wrong_cluster = base.clone();
    wrong_cluster.registry.generation_history.cluster_binding = "foreign-cluster".into();
    assert!(plan_transition(&wrong_cluster).is_err());
    let mut stale_previous = base.clone();
    stale_previous
        .registry
        .generation_history
        .active_allocation
        .as_mut()
        .ok_or("active allocation missing")?
        .artifact_digest =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
    assert!(
        plan_transition(&stale_previous).is_err(),
        "an approved historical build is not authority to ignore the active writer"
    );
    let mut wrong_incarnation = base.clone();
    predecessor_mut(&mut wrong_incarnation)?
        .allocation
        .registry_uid = "22222222-2222-4222-8222-222222222222".into();
    assert!(plan_transition(&wrong_incarnation).is_err());
    let mut wrong_revision = base.clone();
    wrong_revision.registry.generation_history.revision = "stale".into();
    assert!(plan_transition(&wrong_revision).is_err());

    let mut omitted = serde_json::to_value(&base)?;
    omitted["registry"]
        .as_object_mut()
        .ok_or("registry encoding missing")?
        .remove("generationHistory");
    assert!(serde_json::from_value::<DeploymentRequest>(omitted).is_err());
    let mut missing_active = serde_json::to_value(&base)?;
    missing_active["registry"]["generationHistory"]
        .as_object_mut()
        .ok_or("history encoding missing")?
        .remove("activeAllocation");
    assert!(
        serde_json::from_value::<DeploymentRequest>(missing_active).is_err(),
        "an absent active pointer must not imply explicit uninstall authority"
    );
    Ok(())
}

#[test]
fn resumed_quiesced_transition_needs_fresh_staging_and_termination_evidence() -> TestResult {
    let plan = plan_transition(&request(
        Some(generation("gen-a", &[("config", "orphan")])?),
        generation("gen-b", &[("config", "orphan")])?,
    ))?;
    let sequence = events(&plan);
    let mut record = block_and_resume_from_quiescence(&plan)?;
    assert_eq!(record.phase, Phase::Planned);
    assert_eq!(record.verification_epoch, 1);
    assert!(
        advance(
            &plan,
            &record,
            record.revision,
            "executor",
            sequence[2].clone()
        )
        .is_err()
    );
    let fresh_digest = format!("sha256:{}", "b".repeat(64));
    let mut staged = match sequence[0].clone() {
        Event::Staged {
            generation_id,
            service_account_name,
            registry_cas_receipt_digest,
            ..
        } => Event::Staged {
            event_id: "staged-again".into(),
            generation_id,
            service_account_name,
            registry_cas_receipt_digest,
            verification_epoch: 1,
        },
        _ => return Err("recorded step is not a staging event".into()),
    };
    assert!(
        advance(&plan, &record, record.revision, "executor", staged.clone()).is_err(),
        "old staging proof cannot be relabeled with a new epoch"
    );
    if let Event::Staged {
        registry_cas_receipt_digest,
        ..
    } = &mut staged
    {
        *registry_cas_receipt_digest = fresh_digest.clone();
    }
    record = advance(&plan, &record, record.revision, "executor", staged)?;
    let Event::Quiesced {
        evidence: Quiescence::OldGeneration(mut evidence),
        ..
    } = sequence[1].clone()
    else {
        return Err("recorded step is not an old-generation quiesce".into());
    };
    let old_epoch = Event::Quiesced {
        event_id: "quiesced-again".into(),
        evidence: Quiescence::OldGeneration(evidence.clone()),
    };
    assert!(advance(&plan, &record, record.revision, "executor", old_epoch).is_err());
    evidence.verification_epoch = 1;
    let reused_proofs = Event::Quiesced {
        event_id: "quiesced-again".into(),
        evidence: Quiescence::OldGeneration(evidence.clone()),
    };
    assert!(
        advance(&plan, &record, record.revision, "executor", reused_proofs).is_err(),
        "old termination receipts cannot qualify a fresh epoch"
    );
    evidence.process_termination_receipt_digest = fresh_digest.clone();
    evidence.access_revocation_receipt_digest = fresh_digest.clone();
    record = advance(
        &plan,
        &record,
        record.revision,
        "executor",
        Event::Quiesced {
            event_id: "quiesced-again".into(),
            evidence: Quiescence::OldGeneration(evidence),
        },
    )?;
    record = advance(
        &plan,
        &record,
        record.revision,
        "executor",
        Event::Activated {
            event_id: "activated-again".into(),
            generation_id: plan.request.next.generation_id.clone(),
            activation_receipt_digest: fresh_digest,
            verification_epoch: 1,
        },
    )?;
    assert_eq!(record.phase, Phase::Activated);
    Ok(())
}

#[test]
fn post_activation_block_cannot_restart_protocol_under_same_generation() -> TestResult {
    let plan = plan_transition(&request(
        None,
        generation("gen-a", &[("config", "orphan")])?,
    ))?;
    let mut record = begin(&plan, "executor")?;
    for event in events(&plan).into_iter().take(3) {
        record = advance(&plan, &record, record.revision, "executor", event)?;
    }
    record = advance(
        &plan,
        &record,
        record.revision,
        "executor",
        Event::Blocked {
            event_id: "blocked-after-activation".into(),
            reason: "new writer state uncertain".into(),
        },
    )?;
    assert!(
        advance(
            &plan,
            &record,
            record.revision,
            "executor",
            Event::Resume {
                event_id: "unsafe-resume".into(),
                recovery_receipt_digest: DIGEST.into()
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn cleanup_is_bound_to_validated_plan_phase_domain_and_current_transition() -> TestResult {
    let req = request(
        Some(generation("gen-a", &[("remove", "delete")])?),
        generation("gen-b", &[])?,
    );
    let plan = plan_transition(&req)?;
    let record = cleaning_record(&plan)?;
    let target = identity("remove");
    let live = LiveCleanupObject::Present {
        uid: "uid-remove".into(),
        finalizers: vec![],
        deletion_started: false,
    };
    let mut claim = req.registry.claims[0].clone();
    claim.state = ClaimState::Releasing;
    claim.transition_id = req.transition_id;
    assert!(cleanup_decision(&plan, &record, &target, &claim, &live).is_ok());
    let mut foreign = claim.clone();
    foreign.domain_id = "foreign-domain".into();
    assert!(cleanup_decision(&plan, &record, &target, &foreign, &live).is_err());
    let mut wrong_transition = claim.clone();
    wrong_transition.transition_id = "unrelated-transition".into();
    assert!(cleanup_decision(&plan, &record, &target, &wrong_transition, &live).is_err());
    let mut active = claim.clone();
    active.state = ClaimState::Active;
    assert!(cleanup_decision(&plan, &record, &target, &active, &live).is_err());
    assert!(cleanup_decision(&plan, &record, &identity("unplanned"), &claim, &live).is_err());
    let initial = begin(&plan, "executor")?;
    assert!(cleanup_decision(&plan, &initial, &target, &claim, &live).is_err());
    let mut forged = plan;
    forged.cleanup[0].expected_uid = Some("forged".into());
    assert!(cleanup_decision(&forged, &record, &target, &claim, &live).is_err());
    Ok(())
}

fn block_and_resume_from_quiescence(plan: &TransitionPlan) -> TestResult<TransitionRecord> {
    let mut record = begin(plan, "executor")?;
    for event in events(plan).iter().take(2) {
        record = advance(plan, &record, record.revision, "executor", event.clone())?;
    }
    assert_eq!(record.phase, Phase::Quiesced);
    record = advance(
        plan,
        &record,
        record.revision,
        "executor",
        Event::Blocked {
            event_id: "lost-certainty".into(),
            reason: "termination evidence no longer reliable".into(),
        },
    )?;
    record = advance(
        plan,
        &record,
        record.revision,
        "executor",
        Event::Resume {
            event_id: "recovery-1".into(),
            recovery_receipt_digest: DIGEST.into(),
        },
    )?;
    Ok(record)
}

#[test]
fn cleanup_and_inventory_projection_exclude_resource_payloads() -> TestResult {
    let mut prior = generation("g1", &[("removed", "delete"), ("retained", "orphan")])?;
    prior.generation_id = fixture_names(1).0;
    prior.compiled.ir.resources[0].desired["data"]["value"] = json!("private-value-canary");
    for resource in &mut prior.compiled.ir.resources {
        resource.source.file = "private-path-canary.ts".into();
    }
    prior.compiled.ir.provenance.inputs[0].path = "private-path-canary.ts".into();
    prior.compiled = compiler_core::validate(&prior.compiled.ir)?;
    let request = request(
        Some(prior.clone()),
        generation("g2", &[("retained", "delete")])?,
    );
    let (scope, inventory) = generation_inventory(&prior, &request.registry.claims)?;
    let encoded = String::from_utf8(inventory.encode(&scope)?)?;
    assert!(!encoded.contains("canary"));
    assert!(!encoded.contains(&prior.compiled.ir_digest));
    assert_eq!(inventory.resources[0].uid.as_deref(), Some("uid-removed"));
    let plan = plan_transition(&request)?;
    assert_eq!(plan.cleanup.len(), 1);
    let item = &plan.cleanup[0];
    assert_eq!((&*item.api_version, &*item.kind), ("v1", "ConfigMap"));
    assert_eq!(item.expected_uid.as_deref(), Some("uid-removed"));
    assert_eq!(item.policy, DeletionPolicy::Delete);
    assert!(!serde_json::to_string(&plan.cleanup)?.contains("canary"));
    let mut tampered = plan.clone();
    tampered.cleanup[0].kind = "Secret".into();
    assert!(begin(&tampered, "executor").is_err());
    let mut old_schema = plan.clone();
    old_schema.schema_version = "apsides.dev/transition-plan/v1".into();
    assert!(begin(&old_schema, "executor").is_err());
    let mut wrong_owner = request.registry.claims.clone();
    wrong_owner[0].generation_id = "foreign".into();
    assert!(generation_inventory(&prior, &wrong_owner).is_err());
    let mut duplicates = request.registry.claims;
    duplicates.push(duplicates[0].clone());
    assert!(generation_inventory(&prior, &duplicates).is_err());
    Ok(())
}

#[test]
fn persisted_inventory_must_match_approved_resource_metadata()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let initial = request(None, generation("g1", &[("old", "orphan")])?);
    validate_transition_inventory(&initial, None)?;
    let upgrade = request(
        Some(initial.next.clone()),
        generation("g2", &[("new", "delete")])?,
    );
    let snapshot = upgrade
        .predecessor()
        .ok_or("missing fixture predecessor")?
        .inventory
        .clone();
    validate_transition_inventory(&upgrade, Some(&snapshot))?;
    assert!(validate_transition_inventory(&upgrade, None).is_err());
    assert!(validate_transition_inventory(&initial, Some(&snapshot)).is_err());
    let mutations: [fn(&mut inventory::TrackedResource); 6] = [
        |resource| resource.deletion_policy = DeletionPolicy::Delete,
        |resource| resource.uid = None,
        |resource| resource.uid = Some("replacement-uid".into()),
        |resource| resource.kind = "Secret".into(),
        |resource| resource.api_version = "other/v1".into(),
        |resource| resource.identity.name = "unapproved".into(),
    ];
    for mutate in mutations {
        let mut changed = snapshot.clone();
        mutate(
            changed
                .resources
                .first_mut()
                .ok_or("missing fixture resource")?,
        );
        assert!(validate_transition_inventory(&upgrade, Some(&changed)).is_err());
    }
    let mut missing = snapshot.clone();
    missing.resources.clear();
    assert!(validate_transition_inventory(&upgrade, Some(&missing)).is_err());
    let mut extra = snapshot;
    let mut resource = extra
        .resources
        .first()
        .ok_or("missing fixture resource")?
        .clone();
    resource.identity.name = "extra".into();
    extra.resources.push(resource);
    assert!(validate_transition_inventory(&upgrade, Some(&extra)).is_err());
    Ok(())
}

#[test]
fn protected_metadata_commitment_excludes_desired_values_and_rejects_invalid_authority()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let original = generation("g1", &[("config", "delete")])?;
    let original_plan = plan_transition(&request(None, original.clone()))?;
    let mut changed_ir = original.compiled.ir.clone();
    changed_ir
        .resources
        .first_mut()
        .ok_or("resource missing")?
        .desired["data"] =
        serde_json::json!({"private-value-canary": "not-part-of-inventory-commitment"});
    let changed = GenerationPlan {
        compiled: compiler_core::validate(&changed_ir)?,
        ..original
    };
    let changed_plan = plan_transition(&request(None, changed))?;
    assert_ne!(
        original_plan.generation_reservation.ir_digest,
        changed_plan.generation_reservation.ir_digest
    );
    assert_eq!(
        original_plan
            .generation_reservation
            .inventory_metadata_digest,
        changed_plan
            .generation_reservation
            .inventory_metadata_digest
    );
    let record = begin(&original_plan, "executor")?;
    for malformed in ["schema", "digest"] {
        let mut plan = original_plan.clone();
        if malformed == "schema" {
            plan.schema_version = "unsupported".into();
        } else {
            plan.generation_reservation.inventory_metadata_digest = "invalid".into();
        }
        assert!(begin(&plan, "executor").is_err());
        assert!(verify_record(&plan, &record).is_err());
    }
    for absent in [false, true] {
        let mut encoded = serde_json::to_value(&original_plan.generation_reservation)?;
        let object = encoded.as_object_mut().ok_or("allocation object missing")?;
        if absent {
            object.remove("inventoryMetadataDigest");
        } else {
            object.insert("inventoryMetadataDigest".into(), serde_json::Value::Null);
        }
        assert!(serde_json::from_value::<GenerationAllocation>(encoded).is_err());
    }
    Ok(())
}

#[test]
fn predecessor_metadata_rejects_forged_allocation_policy_uid_transition_and_membership()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut prior = generation("g1", &[("old", "orphan")])?;
    prior.generation_id = fixture_names(1).0;
    let baseline = request(Some(prior.clone()), generation("g2", &[])?);
    plan_transition(&baseline)?;
    let allocation = baseline
        .registry
        .generation_history
        .active_allocation
        .as_ref()
        .ok_or("missing fixture allocation")?;
    assert_eq!(
        predecessor_metadata(&prior, allocation, &baseline.registry.claims)?,
        baseline
            .predecessor()
            .ok_or("missing fixture predecessor")?
            .clone()
    );

    let mut forged_allocation = baseline.clone();
    predecessor_mut(&mut forged_allocation)?
        .allocation
        .artifact_digest =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
    assert!(plan_transition(&forged_allocation).is_err());

    let mut forged_policy = baseline.clone();
    predecessor_mut(&mut forged_policy)?
        .inventory
        .resources
        .first_mut()
        .ok_or("missing fixture resource")?
        .deletion_policy = DeletionPolicy::Delete;
    assert!(plan_transition(&forged_policy).is_err());

    let mut forged_uid = baseline.clone();
    forged_uid
        .registry
        .claims
        .first_mut()
        .ok_or("missing fixture claim")?
        .uid = Some("replacement-uid".into());
    assert!(plan_transition(&forged_uid).is_err());

    let mut forged_transition = baseline.clone();
    forged_transition
        .registry
        .claims
        .first_mut()
        .ok_or("missing fixture claim")?
        .transition_id = "other-transition".into();
    assert!(plan_transition(&forged_transition).is_err());

    let mut missing_claim = baseline.clone();
    missing_claim.registry.claims.clear();
    assert!(plan_transition(&missing_claim).is_err());

    let mut extra_claim = baseline.clone();
    let mut claim = extra_claim
        .registry
        .claims
        .first()
        .ok_or("missing fixture claim")?
        .clone();
    claim.identity.name = "extra".into();
    extra_claim.registry.claims.push(claim);
    assert!(plan_transition(&extra_claim).is_err());
    Ok(())
}

#[test]
fn predecessor_metadata_validates_scope_retirement_namespaces_and_absent_uids()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let baseline = request(
        Some(generation("g1", &[("old", "delete")])?),
        generation("g2", &[])?,
    );

    let mut absent = baseline.clone();
    absent
        .registry
        .claims
        .first_mut()
        .ok_or("missing fixture claim")?
        .uid = None;
    predecessor_mut(&mut absent)?
        .inventory
        .resources
        .first_mut()
        .ok_or("missing fixture resource")?
        .uid = None;
    plan_transition(&absent)?;

    let mut wrong_scope = baseline.clone();
    predecessor_mut(&mut wrong_scope)?.scope.namespace = "other".into();
    assert!(plan_transition(&wrong_scope).is_err());

    let mut duplicate_namespace = baseline.clone();
    predecessor_mut(&mut duplicate_namespace)?.retirement_role_binding_namespaces =
        vec!["workload".into(), "workload".into()];
    assert!(plan_transition(&duplicate_namespace).is_err());

    let mut malformed_namespace = baseline;
    predecessor_mut(&mut malformed_namespace)?.retirement_role_binding_namespaces =
        vec!["Not-A-Namespace".into()];
    assert!(plan_transition(&malformed_namespace).is_err());
    Ok(())
}

#[test]
fn full_generation_is_rejected_as_predecessor_metadata()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let predecessor = generation("g1", &[("old", "delete")])?;
    let request = request(Some(predecessor.clone()), generation("g2", &[])?);
    let mut encoded = serde_json::to_value(request)?;
    encoded["previous"] = serde_json::to_value(predecessor)?;
    assert!(serde_json::from_value::<DeploymentRequest>(encoded).is_err());
    Ok(())
}
