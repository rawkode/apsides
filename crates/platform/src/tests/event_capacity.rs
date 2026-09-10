use super::*;
use deploy::{Event, Phase, TransitionRecord};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn only_gets(calls: &Calls) -> TestResult<bool> {
    Ok(calls
        .lock()
        .map_err(|_| "calls lock poisoned")?
        .iter()
        .all(|call| call.0 == "GET"))
}

fn exhausted_planned(plan: &deploy::TransitionPlan) -> TestResult<TransitionRecord> {
    let mut record = deploy::begin(plan, "executor-a")?;
    for attempt in 0..126 {
        for event in [
            Event::Blocked {
                event_id: format!("blocked-{attempt}"),
                reason: "fixture-recovery".into(),
            },
            Event::Resume {
                event_id: format!("resume-{attempt}"),
                recovery_receipt_digest: digest(&attempt)?,
            },
        ] {
            record = deploy::advance(plan, &record, record.revision, "executor-a", event)?;
        }
    }
    assert_eq!(record.phase, Phase::Planned);
    assert_eq!(record.receipts.len(), 252);
    deploy::verify_record(plan, &record)?;
    Ok(record)
}

#[tokio::test]
async fn staging_and_activation_refuse_exhausted_receipts_before_writes() -> TestResult {
    let (mut registry, _, policy, _, _) = staged_initial_fixture()?;
    let plan = registry.plan(TRANSITION_A)?.clone();
    let record = exhausted_planned(&plan)?;
    registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("transition missing")?
        .record = record;

    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("apsides-policy-example", &policy, "policy-uid", "40")?,
    ])?;
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let error = platform
        .stage_runtime_identity(TRANSITION_A, "executor-a")
        .await
        .err()
        .ok_or("exhausted staging unexpectedly succeeded")?;
    assert!(
        error
            .to_string()
            .contains("transition exceeds its admitted recovery allowance"),
        "{error}"
    );
    assert!(only_gets(&calls)?);

    let stored = registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("transition missing")?;
    stored.record = deploy::advance(
        &plan,
        &stored.record,
        stored.record.revision,
        "executor-a",
        Event::Staged {
            event_id: "stage-exhausted".into(),
            generation_id: plan.request.next.generation_id.clone(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: digest(&"stage-exhausted")?,
            verification_epoch: stored.record.verification_epoch,
        },
    )?;
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "43")?,
        object("apsides-policy-example", &policy, "policy-uid", "40")?,
    ])?;
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let error = Box::pin(platform.activate_initial(TRANSITION_A, "executor-a"))
        .await
        .err()
        .ok_or("exhausted activation unexpectedly succeeded")?;
    assert!(
        error
            .to_string()
            .contains("transition exceeds its admitted recovery allowance"),
        "{error}"
    );
    assert!(only_gets(&calls)?);
    Ok(())
}

#[tokio::test]
async fn quiescence_and_cleanup_refuse_exhausted_receipts_before_writes() -> TestResult {
    let mut registry = qualified_upgrade_fixture()?;
    let plan = registry.plan(TRANSITION_B)?.clone();
    let policy = policy(&plan.request.next.compiled);
    let mut record = exhausted_planned(&plan)?;
    let epoch = record.verification_epoch;
    let stage = Event::Staged {
        event_id: "stage-exhausted".into(),
        generation_id: plan.request.next.generation_id.clone(),
        service_account_name: plan.generation_reservation.service_account_name.clone(),
        registry_cas_receipt_digest: digest(&"stage-exhausted")?,
        verification_epoch: epoch,
    };
    record = deploy::advance(&plan, &record, record.revision, "executor-a", stage)?;
    let stored = registry
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("transition missing")?;
    stored.record = record.clone();
    stored.executor_binding = fixture_executor_binding(&plan.generation_reservation, "executor-a")?;
    qualified_writer::qualify(&mut registry, TRANSITION_B)?;
    let control = registry_object(&registry, REGISTRY_UID, "42")?;
    let ceiling = object("apsides-policy-example", &policy, "policy-uid", "40")?;
    let (client, calls) = wire(vec![
        control.clone(),
        ceiling.clone(),
        control,
        ceiling.clone(),
    ])?;
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    let error = platform
        .quiesce_previous(TRANSITION_B, "executor-a")
        .await
        .err()
        .ok_or("exhausted quiescence unexpectedly succeeded")?;
    assert!(
        error
            .to_string()
            .contains("transition exceeds its admitted recovery allowance"),
        "{error}"
    );
    assert!(only_gets(&calls)?);

    for event in [
        Event::Quiesced {
            event_id: "quiesce-exhausted".into(),
            evidence: deploy::Quiescence::OldGeneration(deploy::TerminationEvidence {
                generation_id: GENERATION_A.into(),
                executor_id: "executor-a".into(),
                verification_epoch: epoch,
                environment_qualification_digest: digest(&"qualified")?,
                process_termination_receipt_digest: digest(&"terminated")?,
                access_revocation_receipt_digest: digest(&"revoked")?,
            }),
        },
        Event::Activated {
            event_id: "activate-exhausted".into(),
            generation_id: plan.request.next.generation_id.clone(),
            activation_receipt_digest: digest(&"activated")?,
            verification_epoch: epoch,
        },
        Event::Cleaning {
            event_id: "clean-exhausted".into(),
        },
    ] {
        record = deploy::advance(&plan, &record, record.revision, "executor-a", event)?;
    }
    assert_eq!(record.receipts.len(), 256);
    registry
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("transition missing")?
        .record = record;
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "43")?,
        ceiling,
    ])?;
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_B)?;
    let error = platform
        .execute_cleanup(TRANSITION_B, "executor-a")
        .await
        .err()
        .ok_or("exhausted cleanup unexpectedly succeeded")?;
    assert!(
        error
            .to_string()
            .contains("transition exceeds its admitted recovery allowance"),
        "{error}"
    );
    assert!(only_gets(&calls)?);
    Ok(())
}
