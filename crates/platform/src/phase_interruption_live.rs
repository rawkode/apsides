//! The killed executable is test-instrumented; no `VerifiedRelease` or takeover claim.
use crate::phase_interruption::{PauseAfterPhase, PhasePause};
use std::{io::Write, sync::Arc};

mod driver {
    use crate as platform_api;
    include!("../tests/support/candidate_driver.rs");
}

#[tokio::test]
#[ignore = "requires an owned disposable cluster, signed candidate and parent process checkpoint handshake"]
async fn candidate_transition_checkpoint() -> driver::Result<()> {
    let outcome = Box::pin(driver::transition_step(|mut platform, settings| {
        let target: PauseAfterPhase = serde_json::from_value(
            settings
                .get("pauseAfterPhase")
                .cloned()
                .ok_or("checkpoint target missing")?,
        )?;
        let transition = settings
            .get("transition")
            .and_then(serde_json::Value::as_str)
            .ok_or("checkpoint transition missing")?;
        platform.phase_pause = Some(Arc::new(PhasePause::new(target, transition)?));
        Ok(platform)
    }))
    .await;
    let pending = outcome
        .as_ref()
        .err()
        .is_some_and(|error| typed_pending(error.as_ref()));
    let kind = if pending { "platform-pending" } else { "other" };
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "APSIDES_TEST_ERROR={{\"pending\":{pending},\"kind\":\"{kind}\"}}"
    )?;
    stdout.flush()?;
    // Only a typed Pending may be retried by the harness. Do not expose raw
    // library diagnostics through the test runner's error rendering.
    Err(if pending {
        "checkpoint action pending"
    } else {
        "checkpoint action failed or requested checkpoint was not reached"
    }
    .into())
}

fn typed_pending(error: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        error.downcast_ref::<crate::Error>(),
        Some(crate::Error::Pending(_))
    )
}

#[test]
fn only_typed_platform_pending_allows_another_checkpoint_attempt() {
    assert!(typed_pending(&crate::Error::Pending(
        "bounded pending".into()
    )));
    assert!(!typed_pending(&crate::Error::Invalid(
        "platform operation pending".into()
    )));
    assert!(!typed_pending(&std::io::Error::other("platform-pending")));
}
