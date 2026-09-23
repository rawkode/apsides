//! `aps deploy` — the CI interface, independent of the Kubernetes provider.
//!
//! Apsides owns the artifact and policy checks and the resumable deployment
//! protocol; authentication comes from the invoking environment's normal
//! Kubernetes configuration, never from the release.

mod capacity;
mod cluster;
mod outcome;
mod protocol;
mod request;
mod transition;

use crate::{Result, cli, report};
use cluster::{DeploymentContext, connect};
use outcome::{already_committed_report, deployment_report, review_transition};
use protocol::{complete_transition, selected_session};
use request::reserve_new_transition;
use serde_json::Value;
use transition::{ExistingTransition, check_existing, default_transition, verify_resumed_intents};

/// # Errors
/// Returns an error if the release does not verify against protected trust
/// policy, if the transition belongs to another artifact or executor, or if any
/// stage of the protected transition protocol is blocked.
pub fn command(arguments: &cli::Deploy) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let outcome = runtime.block_on(deploy(arguments))?;
    report::emit(&outcome)
}

async fn deploy(arguments: &cli::Deploy) -> Result<Value> {
    let resize_capacity = arguments.capacity == Some(cli::Capacity::Resize);
    platform::install_rustls_ring_provider()?;
    let inventory = crate::connection::connect().await?;
    let artifact = crate::artifact::input::open(arguments).await?;
    let DeploymentContext {
        issued,
        verified,
        compatibility,
    } = connect(artifact.path(), artifact.expected_digest()).await?;
    let platform = issued.platform().clone();
    let domain = &verified.compiled().ir.domain;
    let executor = executor_id(arguments)?;
    let registry = platform.load_registry().await?;
    let policy_digest = compiler_core::digest(&platform.load_policy(&domain.id).await?.value)?;
    let transition_id = match &arguments.transition {
        Some(transition) => transition.clone(),
        None => default_transition(
            &registry.value,
            &domain.id,
            verified.release_digest(),
            &policy_digest,
        )?,
    };
    verify_resumed_intents(
        &arguments.ownership_intents,
        registry.value.transitions.get(&transition_id),
    )?;
    let existing = check_existing(
        &registry.value,
        &transition_id,
        &(&verified).into(),
        &executor,
        &policy_digest,
    )?;
    if resize_capacity {
        capacity::require_target(&existing, registry.value.transitions.get(&transition_id))?;
    }
    let resumed = matches!(&existing, ExistingTransition::Resume);
    let reserved = match existing {
        ExistingTransition::Committed => {
            return already_committed_report(
                &platform,
                &verified,
                inventory.as_ref(),
                &transition_id,
            )
            .await;
        }
        ExistingTransition::Resume => crate::review::LiveReview::not_approved(),
        ExistingTransition::Absent => {
            reserve_new_transition(
                &issued,
                inventory.as_ref(),
                registry,
                &verified,
                &transition_id,
                &executor,
                &arguments.ownership_intents,
            )
            .await?
        }
    };
    // Always taken: this is where an approved capacity resize happens, and a
    // resumed preview replaces the session it produces rather than skipping it.
    let reserved_session = selected_session(
        &platform,
        &verified,
        inventory.as_ref(),
        &transition_id,
        &executor,
        resize_capacity,
    )
    .await?;
    let (live, session) = if resumed && issued.identity().preview.is_some() {
        preview_resumed(&issued, &verified, inventory.as_ref(), &transition_id).await?
    } else {
        (reserved, reserved_session)
    };
    let review = review_transition(&session, inventory.as_ref(), &live).await?;
    let tracking = complete_transition(
        &session,
        &verified,
        inventory.as_ref(),
        &transition_id,
        &executor,
    )
    .await?;
    Ok(deployment_report(
        &session.resolve().await?,
        &verified,
        &compatibility,
        issued.expires_at(),
        &tracking,
        &review,
    ))
}

/// Re-preview a resumed transition, replacing both the review and the session
/// with the pair bound to that preview.
async fn preview_resumed(
    issued: &platform::IssuedDeployment,
    verified: &release::IntegrityVerifiedCandidate,
    inventory: &dyn inventory::InventoryStore,
    transition: &str,
) -> Result<(crate::review::LiveReview, platform::TransitionSession)> {
    let pending = issued
        .preview_transition(verified, inventory, transition)
        .await?;
    let resolved = pending.session.resolve().await?;
    let live = crate::review::LiveReview::pending(&resolved, pending.outcome, pending.binding)?;
    Ok((live, pending.session))
}

fn executor_id(arguments: &cli::Deploy) -> Result<String> {
    if let Some(value) = &arguments.executor {
        return Ok(value.clone());
    }
    if let Ok(value) = std::env::var("APSIDES_EXECUTOR_ID") {
        return Ok(value);
    }
    if let (Ok(run), Ok(attempt)) = (
        std::env::var("GITHUB_RUN_ID"),
        std::env::var("GITHUB_RUN_ATTEMPT"),
    ) {
        return Ok(format!("github-{run}-{attempt}"));
    }
    Err("set APSIDES_EXECUTOR_ID for the trusted local deployment job (GitHub Actions supplies its run identity automatically)".into())
}

#[cfg(test)]
mod tests;
