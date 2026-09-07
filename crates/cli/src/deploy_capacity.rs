//! Explicit capacity recovery keeps the release and executor checks of normal deploy.
use crate::Result;
use deploy::Phase;
use std::{collections::BTreeMap, io::Write as _};

pub(super) fn requested(options: &BTreeMap<String, String>) -> Result<bool> {
    match options.get("--capacity").map(String::as_str) {
        None => Ok(false),
        Some("resize") if options.get("--transition").is_some_and(|id| !id.is_empty()) => Ok(true),
        Some("resize") => Err("capacity resize requires an explicit --transition ID".into()),
        Some(_) => Err("unsupported capacity action; use --capacity resize".into()),
    }
}

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
    writeln!(
        std::io::stderr().lock(),
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "status": "capacity-resized", "transition": transition,
            "changed": before != after, "before": before, "after": after,
        }))?
    )?;
    Ok(rebound)
}

pub(super) fn required_error(transition: &str) -> Box<dyn std::error::Error> {
    format!("writer topology exceeds admitted capacity; rerun this deploy with --transition {transition} --capacity resize, retaining the same artifact and executor options").into()
}

#[cfg(test)]
mod tests {
    use super::{requested, require_target};
    use std::collections::BTreeMap;

    #[test]
    fn resize_requires_explicit_action_and_transition() -> crate::Result<()> {
        assert!(!requested(&BTreeMap::new())?);
        assert!(requested(&BTreeMap::from([("--capacity".into(), "resize".into())])).is_err());
        assert!(requested(&BTreeMap::from([
            ("--capacity".into(), "resize".into()),
            ("--transition".into(), "t-example".into()),
        ]))?);
        assert!(
            requested(&BTreeMap::from([
                ("--capacity".into(), "cancel".into()),
                ("--transition".into(), "t-example".into()),
            ]))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn resize_cannot_create_or_reopen_committed_transition() {
        assert!(require_target(&super::super::ExistingTransition::Absent, None).is_err());
        assert!(require_target(&super::super::ExistingTransition::Committed, None).is_err());
        assert!(require_target(&super::super::ExistingTransition::Resume, None).is_err());
    }
}
