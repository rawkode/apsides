//! Structural checks for unselected history; mutation still requires full release-bound replay.
use crate::{Result, StoredTransition, invalid};
use deploy::{Event, Phase};
use std::collections::BTreeSet;

pub(crate) fn validate(stored: &StoredTransition) -> Result<()> {
    let record = &stored.record;
    if record.schema_version != "apsides.dev/transition-record/v1"
        || record.transition_id != stored.allocation().transition_id
        || record.plan_digest != stored.plan_digest()
        || !valid_record_id(&record.executor_id)
        || record.receipts.len() > 256
        || record.revision != record.receipts.len() as u64
    {
        return Err(invalid("compact transition record header invalid"));
    }
    let mut ids = BTreeSet::new();
    let mut phase = Phase::Planned;
    let mut epoch = 0_u64;
    let mut blocked = None;
    let mut reason = None;
    for receipt in &record.receipts {
        if receipt.event_id != receipt.event.id()
            || !valid_record_id(&receipt.event_id)
            || !ids.insert(&receipt.event_id)
            || compiler_core::digest(&receipt.event)? != receipt.evidence_digest
        {
            return Err(invalid("compact transition receipt commitment invalid"));
        }
        phase = match (&receipt.event, phase) {
            (
                Event::Staged {
                    generation_id,
                    service_account_name,
                    verification_epoch,
                    ..
                },
                Phase::Planned,
            ) if generation_id == &stored.allocation().generation_id
                && service_account_name == &stored.allocation().service_account_name
                && *verification_epoch == epoch =>
            {
                Phase::Staged
            }
            (Event::Quiesced { .. }, Phase::Staged) => Phase::Quiesced,
            (
                Event::Activated {
                    generation_id,
                    verification_epoch,
                    ..
                },
                Phase::Quiesced,
            ) if generation_id == &stored.allocation().generation_id
                && *verification_epoch == epoch =>
            {
                Phase::Activated
            }
            (Event::Cleaning { .. }, Phase::Activated) => Phase::Cleaning,
            (Event::Committed { .. }, Phase::Cleaning) => Phase::Committed,
            (Event::Blocked { reason: why, .. }, current)
                if !matches!(current, Phase::Blocked | Phase::Committed)
                    && !why.is_empty()
                    && why.len() <= 1024 =>
            {
                blocked = Some(current);
                reason = Some(why.clone());
                Phase::Blocked
            }
            (Event::Resume { .. }, Phase::Blocked)
                if matches!(
                    blocked,
                    Some(Phase::Planned | Phase::Staged | Phase::Quiesced)
                ) =>
            {
                epoch = epoch
                    .checked_add(1)
                    .ok_or_else(|| invalid("compact record epoch exhausted"))?;
                blocked = None;
                reason = None;
                Phase::Planned
            }
            _ => return Err(invalid("compact record phase history invalid")),
        };
    }
    if phase != record.phase
        || epoch != record.verification_epoch
        || blocked != record.blocked_from
        || reason != record.blocked_reason
    {
        return Err(invalid("compact record state differs from receipt history"));
    }
    validate_previous(stored)
}

fn valid_record_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_previous(stored: &StoredTransition) -> Result<()> {
    match (&stored.inputs.previous, &stored.previous_writer_checkpoint) {
        (None, None) => (),
        (Some(previous), Some(checkpoint)) => {
            checkpoint.validate_authority(
                &previous.allocation,
                &previous.scope,
                &previous.control_namespace,
            )?;
            if checkpoint.retirement_role_binding_namespaces()?
                != previous.retirement_role_binding_namespaces
            {
                return Err(invalid("compact predecessor retirement identities changed"));
            }
        }
        _ => {
            return Err(invalid(
                "compact predecessor writer authority missing or unexpected",
            ));
        }
    }
    Ok(())
}
