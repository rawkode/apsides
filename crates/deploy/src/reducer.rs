use crate::{
    CleanupOutcome, Error, Event, EventReceipt, GenerationPlan, Phase, Quiescence, Result,
    TransitionPlan, TransitionRecord, plan_transition, valid_digest, valid_id,
};
use compiler_core::{DeletionPolicy, digest};
use std::collections::BTreeSet;

const MAX_EVENT_RECEIPTS: usize = 256;

fn reject(message: &str) -> Error {
    Error::Transition(message.into())
}

/// Compute a stable, generation-specific Kubernetes `ServiceAccount` name.
///
/// # Errors
/// Fails if canonical generation identity serialization fails.
pub fn service_account_name(generation: &GenerationPlan) -> Result<String> {
    crate::allocation::service_account_for_generation(&generation.generation_id)
}

fn initial(plan: &TransitionPlan, executor_id: &str) -> TransitionRecord {
    TransitionRecord {
        schema_version: "apsides.dev/transition-record/v1".into(),
        transition_id: plan.request.transition_id.clone(),
        plan_digest: plan.plan_digest.clone(),
        executor_id: executor_id.into(),
        revision: 0,
        verification_epoch: 0,
        phase: Phase::Planned,
        blocked_from: None,
        blocked_reason: None,
        receipts: vec![],
    }
}

/// Begin a model transition without performing external work.
///
/// # Errors
/// Rejects a forged plan or invalid executor identity.
pub fn begin(plan: &TransitionPlan, executor_id: &str) -> Result<TransitionRecord> {
    require_inventory_checkpoint(plan)?;
    if !valid_id(executor_id) || plan_transition(&plan.request)? != *plan {
        return Err(reject("invalid executor or noncanonical transition plan"));
    }
    Ok(initial(plan, executor_id))
}

/// Verify a canonical plan and replay every durable transition receipt.
/// # Errors
/// Rejects altered plans, forged phases, invalid receipts, or oversized records.
pub fn verify_record(plan: &TransitionPlan, record: &TransitionRecord) -> Result<()> {
    require_inventory_checkpoint(plan)?;
    if compiler_core::canonical_bytes(record)?.len() > 1024 * 1024 {
        return Err(reject(
            "transition record exceeds the 1 MiB control-state budget",
        ));
    }
    if plan_transition(&plan.request)? != *plan
        || !valid_id(&record.executor_id)
        || record.receipts.len() > MAX_EVENT_RECEIPTS
    {
        return Err(reject("invalid plan or oversized transition record"));
    }
    let mut reconstructed = initial(plan, &record.executor_id);
    for receipt in &record.receipts {
        if receipt.event_id != receipt.event.id()
            || digest(&receipt.event)? != receipt.evidence_digest
        {
            return Err(reject("event receipt content digest mismatch"));
        }
        reconstructed = apply(plan, &reconstructed, receipt.event.clone())?;
    }
    if reconstructed != *record {
        return Err(reject(
            "record phase/revision does not match its durable event history",
        ));
    }
    Ok(())
}

/// Verify a transition and reserve enough durable receipt slots to finish the
/// remaining transition and record one failure as `Blocked` before any external work.
///
/// A committed transition needs no further slots. A blocked transition cannot
/// authorize lifecycle side effects until a separate recovery decision resumes
/// it.
/// # Errors
/// Rejects malformed records, blocked transitions, arithmetic overflow, or a
/// record whose bounded receipt history cannot represent the remaining path.
pub fn require_event_capacity(plan: &TransitionPlan, record: &TransitionRecord) -> Result<()> {
    verify_record(plan, record)?;
    let (remaining, blocked_reserve) = match record.phase {
        Phase::Planned => (5, 1),
        Phase::Staged => (4, 1),
        Phase::Quiesced => (3, 1),
        Phase::Activated => (2, 1),
        Phase::Cleaning => (1, 1),
        Phase::Committed => (0, 0),
        Phase::Blocked => {
            return Err(reject(
                "blocked transition cannot authorize lifecycle side effects",
            ));
        }
    };
    let required = record
        .receipts
        .len()
        .checked_add(remaining)
        .and_then(|count| count.checked_add(blocked_reserve))
        .ok_or_else(|| reject("transition event capacity arithmetic overflow"))?;
    if required > MAX_EVENT_RECEIPTS {
        return Err(reject(
            "transition lacks receipt capacity for completion or failure",
        ));
    }
    Ok(())
}

fn require_inventory_checkpoint(plan: &TransitionPlan) -> Result<()> {
    if plan.schema_version != "apsides.dev/transition-plan/v4" {
        return Err(reject("unsupported transition plan schema"));
    }
    crate::validate_allocation(&plan.generation_reservation)?;
    Ok(())
}

/// Advance using an exact record revision and executor ownership predicate.
///
/// # Errors
/// Rejects stale revisions, executor takeover, invalid evidence, phase skipping,
/// record corruption, or evidence reuse with changed content.
pub fn advance(
    plan: &TransitionPlan,
    record: &TransitionRecord,
    expected_revision: u64,
    executor_id: &str,
    event: Event,
) -> Result<TransitionRecord> {
    verify_record(plan, record)?;
    if executor_id != record.executor_id {
        return Err(reject(
            "executor takeover is unsupported without separately verified retirement",
        ));
    }
    if let Some(prior) = record
        .receipts
        .iter()
        .find(|receipt| receipt.event_id == event.id())
    {
        if prior.evidence_digest == digest(&event)? {
            return Ok(record.clone());
        }
        return Err(reject("event identity reused with different evidence"));
    }
    if record.revision != expected_revision {
        return Err(reject("CAS revision mismatch"));
    }
    apply(plan, record, event)
}

fn apply(
    plan: &TransitionPlan,
    record: &TransitionRecord,
    event: Event,
) -> Result<TransitionRecord> {
    if !valid_id(event.id())
        || record.receipts.len() >= MAX_EVENT_RECEIPTS
        || record
            .receipts
            .iter()
            .any(|receipt| receipt.event_id == event.id())
    {
        return Err(reject("invalid, duplicate or excessive transition event"));
    }
    let mut next = record.clone();
    match &event {
        Event::Blocked { reason, .. }
            if !matches!(record.phase, Phase::Committed | Phase::Blocked) =>
        {
            if reason.is_empty() || reason.len() > 1024 {
                return Err(reject("blocked reason must be nonempty and bounded"));
            }
            next.blocked_from = Some(record.phase);
            next.blocked_reason = Some(reason.clone());
            next.phase = Phase::Blocked;
        }
        Event::Resume {
            recovery_receipt_digest,
            ..
        } if record.phase == Phase::Blocked => {
            if !valid_digest(recovery_receipt_digest) {
                return Err(reject("recovery evidence digest required"));
            }
            let blocked_from = record
                .blocked_from
                .ok_or_else(|| reject("blocked record has no resumable phase"))?;
            if !matches!(
                blocked_from,
                Phase::Planned | Phase::Staged | Phase::Quiesced
            ) {
                return Err(reject(
                    "post-activation recovery requires a separate qualified transition; this resume cannot establish writer termination",
                ));
            }
            next.phase = Phase::Planned;
            next.verification_epoch = record
                .verification_epoch
                .checked_add(1)
                .ok_or_else(|| reject("verification epoch exhausted"))?;
            next.blocked_from = None;
            next.blocked_reason = None;
        }
        _ => next.phase = advance_phase(plan, record, &event)?,
    }
    next.revision = record
        .revision
        .checked_add(1)
        .ok_or_else(|| reject("transition revision exhausted"))?;
    next.receipts.push(EventReceipt {
        event_id: event.id().into(),
        evidence_digest: digest(&event)?,
        event,
    });
    Ok(next)
}

fn advance_phase(plan: &TransitionPlan, record: &TransitionRecord, event: &Event) -> Result<Phase> {
    match (record.phase, event) {
        (
            Phase::Planned,
            Event::Staged {
                generation_id,
                service_account_name: name,
                registry_cas_receipt_digest,
                verification_epoch,
                ..
            },
        ) => {
            if generation_id != &plan.request.next.generation_id
                || *name != service_account_name(&plan.request.next)?
                || !valid_digest(registry_cas_receipt_digest)
                || *verification_epoch != record.verification_epoch
            {
                return Err(reject(
                    "staging identity or claim-reservation evidence differs from plan",
                ));
            }
            if record.receipts.iter().any(|receipt| matches!(&receipt.event,
                Event::Staged { registry_cas_receipt_digest: prior, .. } if prior == registry_cas_receipt_digest)) {
                return Err(reject("recovery requires fresh staging and registry reservation evidence"));
            }
            if plan
                .request
                .predecessor()
                .is_some_and(|old| old.allocation.service_account_name == *name)
            {
                return Err(reject(
                    "new generation cannot reuse the old runtime identity",
                ));
            }
            Ok(Phase::Staged)
        }
        (Phase::Staged, Event::Quiesced { evidence, .. }) => {
            verify_quiescence(plan, record, evidence)?;
            Ok(Phase::Quiesced)
        }
        (
            Phase::Quiesced,
            Event::Activated {
                generation_id,
                activation_receipt_digest,
                verification_epoch,
                ..
            },
        ) => {
            if generation_id != &plan.request.next.generation_id
                || !valid_digest(activation_receipt_digest)
                || *verification_epoch != record.verification_epoch
            {
                return Err(reject("activation identity or evidence differs from plan"));
            }
            Ok(Phase::Activated)
        }
        (Phase::Activated, Event::Cleaning { .. }) => Ok(Phase::Cleaning),
        (
            Phase::Cleaning,
            Event::Committed {
                cleanup_receipts,
                obsolete_identity_retirement_digest,
                ..
            },
        ) => {
            if !valid_digest(obsolete_identity_retirement_digest) {
                return Err(reject("obsolete identity retirement evidence required"));
            }
            verify_cleanup(plan, cleanup_receipts)?;
            Ok(Phase::Committed)
        }
        _ => Err(reject(
            "out-of-order transition; activation requires proven quiescence",
        )),
    }
}

fn verify_quiescence(
    plan: &TransitionPlan,
    record: &TransitionRecord,
    evidence: &Quiescence,
) -> Result<()> {
    match (plan.request.predecessor(), evidence) {
        (None, Quiescence::InitialInstallation { verification_epoch })
            if *verification_epoch == record.verification_epoch =>
        {
            Ok(())
        }
        (Some(previous), Quiescence::OldGeneration(evidence)) => {
            if evidence.generation_id != previous.allocation.generation_id
                || evidence.executor_id != record.executor_id
                || evidence.verification_epoch != record.verification_epoch
                || !valid_digest(&evidence.environment_qualification_digest)
                || !valid_digest(&evidence.process_termination_receipt_digest)
                || !valid_digest(&evidence.access_revocation_receipt_digest)
            {
                return Err(reject(
                    "old writer termination and access revocation evidence is incomplete or unbound",
                ));
            }
            if record.receipts.iter().any(|receipt| matches!(&receipt.event,
                Event::Quiesced { evidence: Quiescence::OldGeneration(prior), .. }
                if prior.process_termination_receipt_digest == evidence.process_termination_receipt_digest
                    || prior.access_revocation_receipt_digest == evidence.access_revocation_receipt_digest)) {
                return Err(reject("recovery requires fresh qualified termination and access revocation evidence"));
            }
            Ok(())
        }
        _ => Err(reject("uncertain old writer termination blocks activation")),
    }
}

fn verify_cleanup(plan: &TransitionPlan, receipts: &[crate::CleanupReceipt]) -> Result<()> {
    if receipts.len() != plan.cleanup.len() {
        return Err(reject("not all authorized cleanup items are complete"));
    }
    let mut seen = BTreeSet::new();
    for receipt in receipts {
        if !seen.insert(&receipt.identity) || !valid_digest(&receipt.evidence_digest) {
            return Err(reject("duplicate or invalid cleanup receipt"));
        }
        let item = plan
            .cleanup
            .iter()
            .find(|item| item.identity == receipt.identity)
            .ok_or_else(|| reject("cleanup receipt has an unauthorized target"))?;
        if item.expected_uid != receipt.expected_uid {
            return Err(reject(
                "cleanup receipt UID differs from authorized identity",
            ));
        }
        if !matches!(
            (item.policy, &receipt.outcome),
            (DeletionPolicy::Orphan, CleanupOutcome::OrphanReleased)
                | (
                    DeletionPolicy::Delete,
                    CleanupOutcome::Deleted | CleanupOutcome::AlreadyAbsent
                )
        ) {
            return Err(reject(
                "cleanup result contradicts the prior approved lifecycle policy",
            ));
        }
    }
    Ok(())
}
