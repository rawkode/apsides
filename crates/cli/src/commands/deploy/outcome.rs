//! What a deployment reports: the review it publishes and its final result.

use crate::{Result, report};
use platform::PlatformClient;
use serde_json::{Value, json};

pub(super) async fn review_transition(
    session: &platform::TransitionSession,
    inventory: &dyn inventory::InventoryStore,
    verified: &release::IntegrityVerifiedCandidate,
    live: &crate::review::LiveReview,
) -> Result<Value> {
    let resolved = session.resolve().await?;
    crate::connection::require_request(inventory, &resolved.plan().request).await?;
    super::surface::verify_surface_binding(verified, resolved.next_review_metadata())?;
    let review = crate::review::transition_review(&resolved, live)?;
    report::note(&review)?;
    Ok(review)
}

pub(super) async fn already_committed_report(
    platform: &PlatformClient,
    verified: &release::IntegrityVerifiedCandidate,
    inventory: &dyn inventory::InventoryStore,
    transition: &str,
) -> Result<Value> {
    let session = platform
        .transition_session(verified, inventory, transition)
        .await?;
    let selected = session.load_registry().await?;
    let tracking =
        crate::connection::publish(&session, inventory, &selected.value, transition).await?;
    let live = crate::review::LiveReview::Unavailable {
        reason: "committed-current-state-preview-not-run",
    };
    let resolved = session.resolve().await?;
    super::surface::verify_surface_binding(verified, resolved.next_review_metadata())?;
    let review = crate::review::transition_review(&resolved, &live)?;
    Ok(json!({
        "status": "already-committed", "transition": transition,
        "artifactTrust": "verified",
        "releaseDigest": verified.release_digest(),
        "review": review,
        "inventory": {"revision": tracking.revision, "snapshotId": tracking.snapshot_id},
    }))
}

pub(super) fn deployment_report(
    resolved: &platform::ResolvedTransition,
    verified: &release::IntegrityVerifiedCandidate,
    compatibility: &Value,
    expires_at: &str,
    tracking: &inventory::VersionedInventory,
    review: &Value,
) -> Value {
    let stored = resolved.stored();
    let transition_id = resolved.transition_id();
    json!({"status":"committed","artifactTrust":"verified","transition":transition_id,"generation":stored.allocation().generation_id,
        "releaseDigest":verified.release_digest(),"irDigest":verified.compiled().ir_digest,"compatibility":compatibility,
        "inventory":{"revision":tracking.revision,"snapshotId":tracking.snapshot_id},
        "credentialExpiresAt":expires_at,"requestDrainQualification":stored.request_drain_qualification,
        "review":review,
        "workloadReadiness":"not-established-by-deployment-commit"})
}
