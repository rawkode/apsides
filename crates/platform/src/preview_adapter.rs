//! Fresh-request live preview through an issued deployment identity. Preview
//! evidence is never a reservation, writer authorization or execution receipt.
use crate::{IssuedDeployment, Result, invalid, preview_fields::PreviewFieldImpact};
use deploy::DeploymentRequest;
use inventory::InventoryStore;
use release::VerifiedRelease;
use serde::Serialize;
use std::time::Duration;

#[path = "preview_authority.rs"]
mod authority;
#[path = "preview_execution.rs"]
mod execution;

#[path = "preview_pending.rs"]
mod pending;
pub use pending::{
    PendingPreview, PendingPreviewBinding, PendingPreviewOutcome, PendingPreviewUnavailable,
};

const DEADLINE: Duration = Duration::from_secs(120);
const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePreviewReport {
    pub schema_version: &'static str,
    pub plan_digest: String,
    pub interval: PreviewInterval,
    pub resources: Vec<PreviewFieldImpact>,
    pub conditions: Vec<PreviewCondition>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewInterval {
    pub started_at: String,
    pub completed_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewCondition {
    IssuedDeployerAdmissionOnly,
    RuntimeInputsNotRead,
    RuntimeConditionalTargetsNotPredicted,
    ObservationIntervalNotAtomic,
    LifecycleDeletesNotPreviewed,
    NoExecutionAuthority,
    AdoptionUsesNonForceApply,
}

impl IssuedDeployment {
    /// Authenticate and preview a fresh, unreserved request. All targets must be
    /// approved before any target API request. No failed or partial report escapes.
    /// # Errors
    /// Rejects expired/changed authority, mismatched release or snapshots, denied
    /// targets, unsafe ownership, dry-run failure, drift, size limits or timeout.
    pub async fn preview_fresh(
        &self,
        request: &DeploymentRequest,
        verified: &VerifiedRelease,
        inventory: &dyn InventoryStore,
    ) -> Result<LivePreviewReport> {
        if request.next.compiled != *verified.compiled()
            || request.next.artifact_digest != verified.release_digest()
            || verified.manifest().image.reference.len() > 4096
        {
            return Err(invalid(
                "preview requires an exact freshly verified release",
            ));
        }
        preview_bounded(self, request, verified.trust_policy_digest(), inventory).await
    }
}

async fn preview_bounded(
    issued: &IssuedDeployment,
    request: &DeploymentRequest,
    trust_digest: &str,
    inventory: &dyn InventoryStore,
) -> Result<LivePreviewReport> {
    tokio::time::timeout(
        DEADLINE,
        Box::pin(preview(issued, request, trust_digest, inventory)),
    )
    .await
    .map_err(|_| invalid("authenticated live preview exceeded its deadline"))?
    .map_err(|_| invalid("authenticated live preview rejected; no deployment state was published"))
}

async fn preview(
    issued: &IssuedDeployment,
    request: &DeploymentRequest,
    trust_digest: &str,
    inventory: &dyn InventoryStore,
) -> Result<LivePreviewReport> {
    let started_at = k8s_openapi::jiff::Timestamp::now().to_string();
    let context =
        authority::Context::authenticate(issued, request, trust_digest, inventory).await?;
    let transport =
        crate::preview_transport::PreviewTransport::new(issued.platform().client.clone());
    let mut resources = Vec::with_capacity(context.next.resources.len());
    let mut observations = Vec::with_capacity(context.next.resources.len());
    for resource in &context.next.resources {
        context.recheck(issued, request, inventory).await?;
        let (impact, observation) =
            execution::preview_target(&transport, &context, request, resource).await?;
        context.recheck(issued, request, inventory).await?;
        crate::preview_limits::validate_serialized(&(&resources, &impact), MAX_REPORT_BYTES)?;
        resources.push(impact);
        observations.push(observation);
    }
    for observation in observations {
        context.recheck(issued, request, inventory).await?;
        observation.recheck(&transport).await?;
    }
    context.recheck(issued, request, inventory).await?;
    let mut conditions = vec![
        PreviewCondition::IssuedDeployerAdmissionOnly,
        PreviewCondition::RuntimeInputsNotRead,
        PreviewCondition::RuntimeConditionalTargetsNotPredicted,
        PreviewCondition::ObservationIntervalNotAtomic,
        PreviewCondition::LifecycleDeletesNotPreviewed,
        PreviewCondition::NoExecutionAuthority,
    ];
    if !request.ownership_intents.is_empty() {
        conditions.push(PreviewCondition::AdoptionUsesNonForceApply);
    }
    let report = LivePreviewReport {
        schema_version: "reakt.dev/live-preview/v1",
        plan_digest: context.plan.plan_digest,
        interval: PreviewInterval {
            started_at,
            completed_at: k8s_openapi::jiff::Timestamp::now().to_string(),
        },
        resources,
        conditions,
    };
    crate::preview_limits::validate_serialized(&report, MAX_REPORT_BYTES)?;
    Ok(report)
}

#[cfg(test)]
#[path = "preview_adapter_tests.rs"]
mod tests;
