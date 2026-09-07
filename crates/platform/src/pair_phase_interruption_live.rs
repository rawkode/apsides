//! Killed test process recovery for an exact signed pair; no executor takeover claim.
use crate::phase_interruption::{PauseAfterPhase, PhasePause};
use deploy::Phase;
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};

mod driver {
    use crate as platform_api;
    include!("../tests/support/permission_upgrade_driver.rs");
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PauseSettings {
    registry_uid: String,
    phase: Phase,
    transition: String,
}

fn settings<T: serde::de::DeserializeOwned>(environment: &str) -> Result<T> {
    let path =
        PathBuf::from(std::env::var(environment).map_err(|_| "checkpoint settings missing")?);
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|_| "checkpoint settings unreadable")?;
    if !metadata.is_file() || metadata.len() > 8192 {
        return Err("checkpoint settings must be a bounded regular file".into());
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|_| "checkpoint settings unreadable")?)
        .map_err(|_| "checkpoint settings invalid".into())
}

fn action_matches(phase: Phase, action: &str) -> bool {
    matches!(
        (phase, action),
        (Phase::Planned, "reserve-successor")
            | (Phase::Staged, "stage-successor")
            | (Phase::Quiesced, "quiesce-upgrade")
            | (Phase::Activated | Phase::Cleaning, "activate-upgrade")
            | (Phase::Committed, "commit-upgrade")
    )
}

#[tokio::test]
#[ignore = "requires an owned disposable cluster, exact signed pair and parent checkpoint handshake"]
async fn candidate_pair_transition_checkpoint() -> Result<()> {
    let pause: PauseSettings = settings("APS_PERMISSION_UPGRADE_PAUSE")?;
    let step: serde_json::Value = settings("APS_PERMISSION_UPGRADE_STEP")?;
    if !step
        .get("action")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|action| action_matches(pause.phase, action))
    {
        return Err("checkpoint phase and signed pair action differ".into());
    }
    let hook = Arc::new(
        PhasePause::new(
            PauseAfterPhase {
                registry_uid: pause.registry_uid,
                phase: pause.phase,
            },
            &pause.transition,
        )
        .map_err(|_| "checkpoint target invalid")?,
    );
    // A reached hook emits the existing post-CAS marker and never returns.
    // The ordinary driver still emits typed Pending outcomes for bounded retries.
    let _ = Box::pin(driver::run_configured(|mut platform| {
        platform.phase_pause = Some(hook);
        platform
    }))
    .await;
    Err("signed pair action returned without reaching requested checkpoint".into())
}

#[test]
fn checkpoint_actions_are_successor_phase_specific() {
    for (phase, action) in [
        (Phase::Planned, "reserve-successor"),
        (Phase::Staged, "stage-successor"),
        (Phase::Quiesced, "quiesce-upgrade"),
        (Phase::Activated, "activate-upgrade"),
        (Phase::Cleaning, "activate-upgrade"),
        (Phase::Committed, "commit-upgrade"),
    ] {
        assert!(action_matches(phase, action));
        assert!(!action_matches(phase, "activate-initial"));
        assert!(!action_matches(phase, "resolve-successor"));
    }
    assert!(!action_matches(Phase::Blocked, "activate-upgrade"));
    assert!(!action_matches(Phase::Staged, "reserve-successor"));
}
