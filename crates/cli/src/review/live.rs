//! Ephemeral live evidence; never added to durable declaration metadata.
use coordination::{ReviewMetadataV1, ReviewRef};
use serde::Serialize;

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(crate) enum LiveReview {
    Unavailable {
        reason: &'static str,
    },
    #[serde(rename = "unavailable")]
    PendingUnavailable {
        reason: &'static str,
        #[serde(rename = "observationContext")]
        observation_context: &'static str,
        #[serde(rename = "observationBinding")]
        binding: Box<platform::PendingPreviewBinding>,
        #[serde(rename = "supplementalPermissions")]
        supplemental_permissions: Box<platform::PreviewPermissions>,
    },
    Available {
        #[serde(rename = "observationContext")]
        observation_context: &'static str,
        report: Box<platform::LivePreviewReport>,
        #[serde(rename = "observationBinding", skip_serializing_if = "Option::is_none")]
        pending_binding: Option<Box<platform::PendingPreviewBinding>>,
        #[serde(rename = "supplementalPermissions")]
        supplemental_permissions: Box<platform::PreviewPermissions>,
        #[serde(skip)]
        binding: Box<CaptureBinding>,
    },
}

pub(crate) struct CaptureBinding {
    next: ReviewRef,
    previous: Option<ReviewRef>,
}

impl LiveReview {
    /// The protected deployment identity does not approve a live preview.
    pub(crate) const fn not_approved() -> Self {
        Self::Unavailable {
            reason: "deployment-identity-preview-not-approved",
        }
    }

    pub(crate) fn fresh(
        plan: &deploy::TransitionPlan,
        previous: Option<&ReviewMetadataV1>,
        report: platform::LivePreviewReport,
    ) -> crate::Result<Self> {
        let next = platform::project_review_metadata(plan)?;
        let result = Self::Available {
            observation_context: "fresh-pre-reservation",
            supplemental_permissions: Box::new(platform::preview_permissions(plan)?),
            report: Box::new(report),
            pending_binding: None,
            binding: Box::new(CaptureBinding {
                next: next.reference()?,
                previous: previous.map(ReviewMetadataV1::reference).transpose()?,
            }),
        };
        result.validate(plan, previous, &next)?;
        Ok(result)
    }

    pub(crate) fn pending(
        resolved: &platform::ResolvedTransition,
        outcome: platform::PendingPreviewOutcome,
        binding: platform::PendingPreviewBinding,
    ) -> crate::Result<Self> {
        binding.validate(resolved)?;
        let result = match outcome {
            platform::PendingPreviewOutcome::Available(report) => {
                let mut value =
                    Self::fresh(resolved.plan(), resolved.previous_review_metadata(), report)?;
                if let Self::Available {
                    observation_context,
                    pending_binding,
                    ..
                } = &mut value
                {
                    *observation_context = "pending-current-state";
                    *pending_binding = Some(Box::new(binding));
                }
                value
            }
            platform::PendingPreviewOutcome::Unavailable(reason) => Self::PendingUnavailable {
                reason: reason.as_str(),
                observation_context: "pending-current-state",
                binding: Box::new(binding),
                supplemental_permissions: Box::new(platform::preview_permissions(resolved.plan())?),
            },
        };
        result.validate_resolved(resolved)?;
        Ok(result)
    }

    pub(super) fn validate_resolved(
        &self,
        resolved: &platform::ResolvedTransition,
    ) -> crate::Result<()> {
        match self {
            Self::Available {
                pending_binding: Some(binding),
                ..
            }
            | Self::PendingUnavailable { binding, .. } => binding.validate(resolved)?,
            Self::Unavailable { .. }
            | Self::Available {
                pending_binding: None,
                ..
            } => {}
        }
        self.validate(
            resolved.plan(),
            resolved.previous_review_metadata(),
            resolved.next_review_metadata(),
        )
    }

    pub(super) fn validate(
        &self,
        plan: &deploy::TransitionPlan,
        previous: Option<&ReviewMetadataV1>,
        next: &ReviewMetadataV1,
    ) -> crate::Result<()> {
        if let Self::Available {
            report, binding, ..
        } = self
            && (report.plan_digest != plan.plan_digest
                || binding.next != next.reference()?
                || binding.previous != previous.map(ReviewMetadataV1::reference).transpose()?)
        {
            return Err("live preview differs from the resolved declaration review".into());
        }
        Ok(())
    }
}
