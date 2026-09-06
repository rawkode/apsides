use super::super::execution::{Declarations, Observation, apply_observed, require_apply_owner};
use super::PendingPreviewUnavailable;
use crate::preview_transport::{NamedPreview, PreviewTarget, PreviewTransport};
use crate::{ResolvedTransition, Result, invalid};
use coordination::ReviewResource;
use deploy::{ClaimState, OwnershipAction, ProtectedClaim};

pub(super) struct CapturedTarget {
    observation: Observation,
    resource: ReviewResource,
    force: bool,
    pub unavailable: Option<PendingPreviewUnavailable>,
}
impl CapturedTarget {
    pub async fn recheck(&self, transport: &PreviewTransport) -> Result<()> {
        self.observation.recheck(transport).await
    }
    pub async fn apply(
        &self,
        transport: &PreviewTransport,
        resolved: &ResolvedTransition,
    ) -> Result<crate::preview_fields::PreviewFieldImpact> {
        let listed = self.observation.read_unchanged(transport).await?;
        let evidence = classify(resolved, &self.resource, &listed)?;
        if evidence.unavailable.is_some() || evidence.force != self.force {
            return Err(invalid("pending target evidence changed before preview"));
        }
        let declarations = Declarations {
            request: &resolved.plan().request,
            next: resolved.next_review_metadata(),
            previous: resolved.previous_review_metadata(),
        };
        apply_observed(
            transport,
            &declarations,
            &self.resource,
            self.observation.target(),
            &listed,
            self.force,
        )
        .await
    }
}
struct Evidence {
    force: bool,
    unavailable: Option<PendingPreviewUnavailable>,
}

pub(super) async fn capture(
    transport: &PreviewTransport,
    resolved: &ResolvedTransition,
    resource: &ReviewResource,
) -> Result<CapturedTarget> {
    let target = PreviewTarget::new(
        &resource.api_version,
        &resource.kind,
        &resource.identity.api_group,
        &resource.identity.resource,
        &resource.identity.namespace,
        &resource.identity.name,
    )?;
    let listed = transport.list_named(&target).await?;
    let evidence = classify(resolved, resource, &listed)?;
    if evidence.unavailable.is_none() {
        validate_composition(resolved, resource, &listed)?;
    }
    Ok(CapturedTarget {
        observation: Observation::capture(target, &listed),
        resource: resource.clone(),
        force: evidence.force,
        unavailable: evidence.unavailable,
    })
}

fn validate_composition(
    resolved: &ResolvedTransition,
    metadata: &ReviewResource,
    listed: &NamedPreview,
) -> Result<()> {
    let resource = resolved
        .plan()
        .request
        .next
        .compiled
        .ir
        .resources
        .iter()
        .find(|resource| resource.id == metadata.id)
        .ok_or_else(|| invalid("pending resource missing"))?;
    let before = listed
        .object()
        .map(super::super::execution::object_value)
        .transpose()?;
    let uid = listed
        .object()
        .and_then(|object| object.metadata.uid.as_deref());
    let live = before.as_ref().zip(uid).map(|(object, expected_uid)| {
        crate::preview_composition::PreviewLiveObject {
            object,
            expected_uid,
        }
    });
    crate::preview_composition::compose_preview(
        resolved.next_review_metadata(),
        &metadata.identity,
        &resource.desired,
        live,
    )?;
    Ok(())
}

fn current_claim<'a>(
    resolved: &'a ResolvedTransition,
    resource: &ReviewResource,
) -> Result<&'a ProtectedClaim> {
    let matches: Vec<_> = resolved
        .registry()
        .value
        .claims
        .iter()
        .filter(|claim| {
            claim.identity.api_group == resource.identity.api_group
                && claim.identity.resource == resource.identity.resource
                && claim.identity.namespace == resource.identity.namespace
                && claim.identity.name == resource.identity.name
        })
        .collect();
    let [claim] = matches.as_slice() else {
        return Err(invalid(
            "pending preview requires one exact protected claim",
        ));
    };
    let allocation = resolved.stored().allocation();
    if claim.domain_id != resolved.stored().domain_id()
        || claim.generation_id != allocation.generation_id
        || claim.transition_id != allocation.transition_id
        || !matches!(claim.state, ClaimState::Reserved | ClaimState::Active)
    {
        return Err(invalid(
            "pending preview claim differs from selected allocation",
        ));
    }
    Ok(claim)
}

fn classify(
    resolved: &ResolvedTransition,
    metadata: &ReviewResource,
    listed: &NamedPreview,
) -> Result<Evidence> {
    let request = &resolved.plan().request;
    let resource = request
        .next
        .compiled
        .ir
        .resources
        .iter()
        .find(|item| item.id == metadata.id)
        .ok_or_else(|| invalid("pending resource missing"))?;
    let claim = current_claim(resolved, metadata)?;
    let previous = request.predecessor().and_then(|previous| {
        previous.inventory.resources.iter().find(|old| {
            old.identity.api_group == claim.identity.api_group
                && old.identity.resource == claim.identity.resource
                && old.identity.namespace == claim.identity.namespace
                && old.identity.name == claim.identity.name
        })
    });
    if previous
        .and_then(|item| item.uid.as_deref())
        .is_some_and(|uid| claim.uid.as_deref() != Some(uid))
    {
        return Err(invalid("pending claim differs from predecessor UID"));
    }
    let intent = request
        .ownership_intents
        .iter()
        .find(|intent| intent.approval.identity == claim.identity);
    if let Some(intent) = intent
        && (intent.approval.action != OwnershipAction::Adopt
            || intent.approval.permitted_apply != resource.desired
            || claim.uid.as_deref() != Some(intent.approval.observed_uid.as_str()))
    {
        return Err(invalid(
            "pending adoption differs from protected claim or desired object",
        ));
    }
    let Some(object) = listed.object() else {
        if claim.uid.is_some()
            || intent.is_some()
            || !request.next.compiled.capabilities.iter().any(|cap| {
                cap.verb == "create"
                    && cap.namespace == claim.identity.namespace
                    && cap.api_group == claim.identity.api_group
                    && cap.resource == claim.identity.resource
                    && cap.subresource.is_none()
            })
        {
            return Err(invalid("pending claim forbids absent target creation"));
        }
        return Ok(Evidence {
            force: resource.force,
            unavailable: None,
        });
    };
    if object.metadata.deletion_timestamp.is_some() {
        return Err(invalid("pending target is being deleted"));
    }
    let Some(uid) = claim.uid.as_deref() else {
        return Ok(Evidence {
            force: resource.force,
            unavailable: Some(PendingPreviewUnavailable::TargetUidNotProven),
        });
    };
    if object.metadata.uid.as_deref() != Some(uid) {
        return Err(invalid("pending target UID was replaced"));
    }
    if let Some(intent) = intent {
        if object.metadata.resource_version.as_deref()
            != Some(intent.approval.observed_resource_version.as_str())
            || compiler_core::digest(&object.metadata.managed_fields)?
                != intent.approval.prior_managed_fields_digest
        {
            return Ok(Evidence {
                force: false,
                unavailable: Some(PendingPreviewUnavailable::AdoptionResultNotProven),
            });
        }
        return Ok(Evidence {
            force: false,
            unavailable: None,
        });
    }
    require_apply_owner(object, resource, &request.next.compiled.ir.domain.id)?;
    Ok(Evidence {
        force: resource.force,
        unavailable: None,
    })
}
