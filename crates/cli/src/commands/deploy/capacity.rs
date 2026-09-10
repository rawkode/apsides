//! Explicit capacity recovery keeps the release and executor checks of normal deploy.
//!
//! `--capacity resize` is admissible only against an existing untouched
//! upgrade: it can neither create a transition nor reopen a committed one.
use crate::{Result, report};
use deploy::Phase;

pub(super) fn require_target(
    existing: &super::ExistingTransition,
    stored: Option<&platform::StoredTransition>,
) -> Result<()> {
    if !matches!(existing, super::ExistingTransition::Resume) {
        return Err("capacity resize requires an existing untouched upgrade; it cannot create a transition or resize a committed deployment".into());
    }
    let stored = stored.ok_or("capacity resize transition is absent")?;
    if stored.inputs.previous.is_none()
        || !matches!(stored.record.phase, Phase::Planned | Phase::Staged)
        || stored.upgrade_drain.is_some()
    {
        return Err("capacity resize requires an existing Planned or Staged upgrade before quiescence has started".into());
    }
    Ok(())
}

pub(super) async fn resize_and_rebind(
    platform: &platform::PlatformClient,
    session: platform::TransitionSession,
    verified: &release::IntegrityVerifiedCandidate,
    inventory: &dyn inventory::InventoryStore,
    transition: &str,
    executor: &str,
) -> Result<platform::TransitionSession> {
    let before = session.resolve().await?.stored().capacity.clone();
    let resized = Box::pin(session.resize_pre_quiescence_capacity(transition, executor)).await?;
    let after = resized.value.transition(transition)?.capacity.clone();
    // Never keep using a session whose pinned admission may have changed.
    let rebound = platform
        .transition_session(verified, inventory, transition)
        .await?;
    report::note(&serde_json::json!({
        "status": "capacity-resized", "transition": transition,
        "changed": before != after, "before": before, "after": after,
    }))?;
    Ok(rebound)
}

pub(super) fn required_error(transition: &str) -> Box<dyn std::error::Error + Send + Sync> {
    format!("writer topology exceeds admitted capacity; rerun this deploy with --transition {transition} --capacity resize, retaining the same artifact and executor options").into()
}

#[cfg(test)]
mod tests {
    use super::require_target;

    #[test]
    fn resize_cannot_create_or_reopen_committed_transition() {
        assert!(require_target(&super::super::ExistingTransition::Absent, None).is_err());
        assert!(require_target(&super::super::ExistingTransition::Committed, None).is_err());
        assert!(require_target(&super::super::ExistingTransition::Resume, None).is_err());
    }
}
