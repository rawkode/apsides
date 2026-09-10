//! Driving the resumable transition protocol to a committed generation.

use super::capacity;
use crate::Result;
use deploy::Phase;
use platform::PlatformClient;
use std::{io::Write as _, time::Duration};

pub(super) async fn complete_transition(
    session: &platform::TransitionSession,
    verified: &release::IntegrityVerifiedCandidate,
    inventory: &dyn inventory::InventoryStore,
    transition: &str,
    executor: &str,
) -> Result<inventory::VersionedInventory> {
    let committed = Box::pin(drive_transition(session, verified, transition, executor)).await?;
    crate::connection::publish(session, inventory, &committed.value, transition).await
}

pub(super) async fn selected_session(
    platform: &PlatformClient,
    verified: &release::IntegrityVerifiedCandidate,
    inventory: &dyn inventory::InventoryStore,
    transition: &str,
    executor: &str,
    resize_capacity: bool,
) -> Result<platform::TransitionSession> {
    let session = platform
        .transition_session(verified, inventory, transition)
        .await?;
    if resize_capacity {
        Box::pin(capacity::resize_and_rebind(
            platform, session, verified, inventory, transition, executor,
        ))
        .await
    } else {
        Ok(session)
    }
}

async fn drive_transition(
    platform: &PlatformClient,
    verified: &release::IntegrityVerifiedCandidate,
    transition: &str,
    executor: &str,
) -> Result<platform::Versioned<platform::NamespaceRegistry>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let registry = platform.load_registry().await?;
        let stored = registry
            .value
            .transitions
            .get(transition)
            .ok_or("transition disappeared")?;
        let upgrade = stored.inputs.previous.is_some();
        let result = match stored.record.phase {
            Phase::Committed => return Ok(registry),
            Phase::Blocked => {
                return Err(format!(
                    "transition blocked: {}",
                    stored
                        .record
                        .blocked_reason
                        .as_deref()
                        .unwrap_or("protected recovery required")
                )
                .into());
            }
            Phase::Planned => {
                platform
                    .stage_proxy_controller(transition, executor, verified)
                    .await
            }
            Phase::Staged if upgrade => platform.quiesce_previous(transition, executor).await,
            Phase::Staged | Phase::Quiesced | Phase::Activated if !upgrade => {
                Box::pin(platform.activate_initial(transition, executor)).await
            }
            Phase::Quiesced | Phase::Activated => {
                platform.activate_upgrade(transition, executor).await
            }
            Phase::Cleaning if upgrade => platform.commit_upgrade(transition, executor).await,
            Phase::Cleaning => Box::pin(platform.activate_initial(transition, executor)).await,
            Phase::Staged => return Err("unsupported protected transition phase".into()),
        };
        match result {
            Ok(_) => {}
            Err(platform::Error::CapacityResizeRequired) => {
                return Err(capacity::required_error(transition));
            }
            Err(platform::Error::Pending(reason)) if tokio::time::Instant::now() < deadline => {
                let _ = writeln!(std::io::stderr().lock(), "aps: {reason}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}
