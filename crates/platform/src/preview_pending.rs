//! Pending-current-state preview. Current protected claims bind target UIDs;
//! runtime inventory and phase names never supply missing object identity.
use super::{LivePreviewReport, PreviewCondition, PreviewInterval};
use crate::{IssuedDeployment, ResolvedTransition, Result, TransitionSession, invalid};
use coordination::ReviewRef;
use serde::Serialize;

#[path = "preview_pending_authority.rs"]
mod authority;
#[path = "preview_pending_targets.rs"]
mod targets;

pub struct PendingPreview {
    pub session: TransitionSession,
    pub outcome: PendingPreviewOutcome,
    pub binding: PendingPreviewBinding,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "camelCase")]
pub enum PendingPreviewOutcome {
    Available(LivePreviewReport),
    Unavailable(PendingPreviewUnavailable),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PendingPreviewUnavailable {
    #[serde(rename = "pending-target-uid-not-proven")]
    TargetUidNotProven,
    #[serde(rename = "pending-adoption-result-not-proven")]
    AdoptionResultNotProven,
}
impl PendingPreviewUnavailable {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TargetUidNotProven => "pending-target-uid-not-proven",
            Self::AdoptionResultNotProven => "pending-adoption-result-not-proven",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPreviewBinding {
    pub registry_uid: String,
    pub registry_resource_version: String,
    pub transition_id: String,
    pub plan_digest: String,
    pub phase: deploy::Phase,
    pub record_revision: u64,
    pub verification_epoch: u64,
    pub next_review_metadata: ReviewRef,
    pub previous_review_metadata: Option<ReviewRef>,
}
impl PendingPreviewBinding {
    fn capture(resolved: &ResolvedTransition) -> Self {
        let stored = resolved.stored();
        Self {
            registry_uid: resolved.registry().uid.clone(),
            registry_resource_version: resolved.registry().resource_version.clone(),
            transition_id: resolved.transition_id().into(),
            plan_digest: resolved.plan().plan_digest.clone(),
            phase: stored.record.phase,
            record_revision: stored.record.revision,
            verification_epoch: stored.record.verification_epoch,
            next_review_metadata: stored.next_review_metadata.clone(),
            previous_review_metadata: stored.previous_review_metadata.clone(),
        }
    }
    /// Match the exact authenticated selected state immediately before reporting.
    /// # Errors
    /// Rejects any selected registry, record or declaration-reference change.
    pub fn validate(&self, resolved: &ResolvedTransition) -> Result<()> {
        if *self != Self::capture(resolved) {
            return Err(invalid("pending preview binding changed; repeat preview"));
        }
        Ok(())
    }
}

impl IssuedDeployment {
    /// Preview a selected pending transition using current protected UID claims.
    /// Unavailable is limited to the two closed evidence gaps; all other failures
    /// reject the operation. No partial field report is returned.
    /// # Errors
    /// Rejects invalid selection, recovery-only state, denied or changed authority,
    /// transport failure, target drift, or the bounded operation deadline.
    pub async fn preview_transition(
        &self,
        verified: &release::IntegrityVerifiedCandidate,
        inventory: &dyn inventory::InventoryStore,
        transition: &str,
    ) -> Result<PendingPreview> {
        tokio::time::timeout(
            super::DEADLINE,
            Box::pin(async {
                self.require_preview_approval().await?;
                let session = self
                    .platform()
                    .transition_session(verified, inventory, transition)
                    .await?;
                let (outcome, binding) = preview_selected(self, &session).await?;
                Ok(PendingPreview {
                    session,
                    outcome,
                    binding,
                })
            }),
        )
        .await
        .map_err(|_| invalid("pending live preview exceeded its deadline"))?
        .map_err(|_: crate::Error| {
            invalid("pending live preview rejected; no deployment state was changed")
        })
    }
}

async fn preview_selected(
    issued: &IssuedDeployment,
    session: &TransitionSession,
) -> Result<(PendingPreviewOutcome, PendingPreviewBinding)> {
    let started_at = k8s_openapi::jiff::Timestamp::now().to_string();
    let context = authority::PendingContext::authenticate(issued, session).await?;
    let transport =
        crate::preview_transport::PreviewTransport::new(issued.platform().client.clone());
    let mut captured = Vec::with_capacity(context.resolved.next_review_metadata().resources.len());
    let mut unavailable = None;
    for resource in &context.resolved.next_review_metadata().resources {
        context.recheck(issued, session).await?;
        let observation = targets::capture(&transport, &context.resolved, resource).await?;
        if let Some(reason) = observation.unavailable {
            unavailable.get_or_insert(reason);
        }
        captured.push(observation);
        context.recheck(issued, session).await?;
    }
    if let Some(reason) = unavailable {
        final_recheck(issued, session, &context, &transport, &captured).await?;
        return Ok((PendingPreviewOutcome::Unavailable(reason), context.binding));
    }
    let mut resources = Vec::with_capacity(captured.len());
    for observation in &captured {
        context.recheck(issued, session).await?;
        let impact = observation.apply(&transport, &context.resolved).await?;
        context.recheck(issued, session).await?;
        crate::preview_limits::validate_serialized(
            &(&resources, &impact),
            super::MAX_REPORT_BYTES,
        )?;
        resources.push(impact);
    }
    final_recheck(issued, session, &context, &transport, &captured).await?;
    let mut conditions = vec![
        PreviewCondition::IssuedDeployerAdmissionOnly,
        PreviewCondition::RuntimeInputsNotRead,
        PreviewCondition::RuntimeConditionalTargetsNotPredicted,
        PreviewCondition::ObservationIntervalNotAtomic,
        PreviewCondition::LifecycleDeletesNotPreviewed,
        PreviewCondition::NoExecutionAuthority,
    ];
    if !context.resolved.plan().request.ownership_intents.is_empty() {
        conditions.push(PreviewCondition::AdoptionUsesNonForceApply);
    }
    let report = LivePreviewReport {
        schema_version: "reakt.dev/live-preview/v1",
        plan_digest: context.binding.plan_digest.clone(),
        interval: PreviewInterval {
            started_at,
            completed_at: k8s_openapi::jiff::Timestamp::now().to_string(),
        },
        resources,
        conditions,
    };
    crate::preview_limits::validate_serialized(&report, super::MAX_REPORT_BYTES)?;
    Ok((PendingPreviewOutcome::Available(report), context.binding))
}

async fn final_recheck(
    issued: &IssuedDeployment,
    session: &TransitionSession,
    context: &authority::PendingContext,
    transport: &crate::preview_transport::PreviewTransport,
    captured: &[targets::CapturedTarget],
) -> Result<()> {
    for target in captured {
        context.recheck(issued, session).await?;
        target.recheck(transport).await?;
    }
    context.recheck(issued, session).await
}

#[cfg(test)]
#[path = "preview_pending_tests.rs"]
mod tests;
