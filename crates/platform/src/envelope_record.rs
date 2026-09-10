//! Finite receipt-history sizing. Synthetic receipts are never accepted as authority.
use crate::{Result, StoredTransition, envelope_writer::commitment, invalid};
use deploy::{
    CleanupOutcome, CleanupReceipt, Event, EventReceipt, Phase, Quiescence, TerminationEvidence,
    TransitionPlan, TransitionRecord,
};

pub(crate) const MAX_RESUMES: usize = 8;
pub(crate) const MAX_RECEIPTS: usize = 40;

pub(crate) fn validate(stored: &StoredTransition) -> Result<()> {
    if stored.record.receipts.len() > MAX_RECEIPTS
        || stored
            .record
            .receipts
            .iter()
            .filter(|receipt| matches!(receipt.event, Event::Resume { .. }))
            .count()
            > MAX_RESUMES
    {
        return Err(invalid(
            "transition exceeds its admitted recovery allowance; explicit recovery required",
        ));
    }
    Ok(())
}

fn id(index: usize) -> String {
    let prefix = format!("e{index}-");
    format!("{prefix}{}", "x".repeat(128 - prefix.len()))
}

fn quiescence(stored: &StoredTransition, epoch: u64) -> Quiescence {
    stored.inputs.previous.as_ref().map_or(
        Quiescence::InitialInstallation {
            verification_epoch: epoch,
        },
        |previous| {
            Quiescence::OldGeneration(TerminationEvidence {
                generation_id: previous.allocation.generation_id.clone(),
                executor_id: stored.record.executor_id.clone(),
                verification_epoch: epoch,
                environment_qualification_digest: commitment(),
                process_termination_receipt_digest: commitment(),
                access_revocation_receipt_digest: commitment(),
            })
        },
    )
}

fn pre_activation(events: &mut Vec<Event>, stored: &StoredTransition, epoch: u64) {
    events.push(Event::Staged {
        event_id: id(events.len()),
        generation_id: stored.allocation().generation_id.clone(),
        service_account_name: stored.allocation().service_account_name.clone(),
        registry_cas_receipt_digest: commitment(),
        verification_epoch: epoch,
    });
    events.push(Event::Quiesced {
        event_id: id(events.len()),
        evidence: quiescence(stored, epoch),
    });
}

pub(crate) fn maximum(
    stored: &StoredTransition,
    plan: &TransitionPlan,
) -> Result<TransitionRecord> {
    let mut events = Vec::new();
    for epoch in 0..MAX_RESUMES {
        pre_activation(&mut events, stored, epoch as u64);
        events.push(Event::Blocked {
            event_id: id(events.len()),
            reason: "\0".repeat(1024),
        });
        events.push(Event::Resume {
            event_id: id(events.len()),
            recovery_receipt_digest: commitment(),
        });
    }
    pre_activation(&mut events, stored, MAX_RESUMES as u64);
    events.push(Event::Activated {
        event_id: id(events.len()),
        generation_id: stored.allocation().generation_id.clone(),
        activation_receipt_digest: commitment(),
        verification_epoch: MAX_RESUMES as u64,
    });
    events.push(Event::Cleaning {
        event_id: id(events.len()),
    });
    let cleanup_receipts = plan
        .cleanup
        .iter()
        .map(|item| CleanupReceipt {
            identity: item.identity.clone(),
            expected_uid: item.expected_uid.clone(),
            outcome: CleanupOutcome::OrphanReleased,
            evidence_digest: commitment(),
        })
        .collect();
    events.push(Event::Committed {
        event_id: id(events.len()),
        cleanup_receipts,
        obsolete_identity_retirement_digest: commitment(),
    });
    // Include the largest terminal Blocked receipt as well as Committed. Their
    // coexistence is deliberately conservative and remains within 40 slots.
    events.push(Event::Blocked {
        event_id: id(events.len()),
        reason: "\0".repeat(1024),
    });
    let receipts = events
        .into_iter()
        .map(|event| EventReceipt {
            event_id: event.id().into(),
            evidence_digest: commitment(),
            event,
        })
        .collect::<Vec<_>>();
    if receipts.len() > MAX_RECEIPTS {
        return Err(invalid("receipt sizing template exceeds admitted slots"));
    }
    Ok(TransitionRecord {
        schema_version: "apsides.dev/transition-record/v1".into(),
        transition_id: stored.record.transition_id.clone(),
        plan_digest: stored.record.plan_digest.clone(),
        executor_id: stored.record.executor_id.clone(),
        revision: MAX_RECEIPTS as u64,
        verification_epoch: MAX_RESUMES as u64,
        phase: Phase::Committed,
        blocked_from: Some(Phase::Committed),
        blocked_reason: Some("\0".repeat(1024)),
        receipts,
    })
}

pub(crate) fn require_continuation(stored: &StoredTransition, plan: &TransitionPlan) -> Result<()> {
    validate(stored)?;
    deploy::require_event_capacity(plan, &stored.record)?;
    let remaining = match stored.record.phase {
        Phase::Planned => 6,
        Phase::Staged => 5,
        Phase::Quiesced => 4,
        Phase::Activated => 3,
        Phase::Cleaning => 2,
        Phase::Committed => 0,
        Phase::Blocked => 7,
    };
    let resumes = stored
        .record
        .receipts
        .iter()
        .filter(|receipt| matches!(receipt.event, Event::Resume { .. }))
        .count();
    if stored.record.receipts.len() + remaining > MAX_RECEIPTS
        || (stored.record.phase == Phase::Blocked && resumes >= MAX_RESUMES)
    {
        return Err(invalid(
            "transition recovery allowance cannot fit completion; explicit recovery required",
        ));
    }
    Ok(())
}
