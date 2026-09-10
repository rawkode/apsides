use super::authority::Context;
use crate::preview_composition::{PreviewLiveObject, compose_preview};
use crate::preview_fields::{PreviewFieldImpact, PreviewObjects, classify_preview_fields};
use crate::preview_transport::{NamedPreview, PreviewTarget, PreviewTransport};
use crate::{ApplyPrecondition, Result, invalid};
use compiler_core::Resource;
use coordination::ReviewResource;
use deploy::{DeploymentRequest, OwnershipAction, ResourceIdentity};
use kube::api::DynamicObject;

pub(super) struct Observation {
    target: PreviewTarget,
    uid: Option<String>,
    resource_version: Option<String>,
}
impl Observation {
    pub fn capture(target: PreviewTarget, listed: &NamedPreview) -> Self {
        Self {
            target,
            uid: listed
                .object()
                .and_then(|object| object.metadata.uid.clone()),
            resource_version: listed
                .object()
                .and_then(|object| object.metadata.resource_version.clone()),
        }
    }
    pub fn target(&self) -> &PreviewTarget {
        &self.target
    }
    pub async fn read_unchanged(&self, transport: &PreviewTransport) -> Result<NamedPreview> {
        let current = transport.list_named(&self.target).await?;
        let uid = current
            .object()
            .and_then(|object| object.metadata.uid.as_ref());
        let version = current
            .object()
            .and_then(|object| object.metadata.resource_version.as_ref());
        if uid != self.uid.as_ref() || version != self.resource_version.as_ref() {
            return Err(invalid(
                "preview target changed during the observation interval",
            ));
        }
        Ok(current)
    }
    pub async fn recheck(&self, transport: &PreviewTransport) -> Result<()> {
        self.read_unchanged(transport).await.map(|_| ())
    }
}

pub(super) struct Declarations<'a> {
    pub request: &'a DeploymentRequest,
    pub next: &'a coordination::ReviewMetadataV1,
    pub previous: Option<&'a coordination::ReviewMetadataV1>,
}

pub(super) async fn preview_target(
    transport: &PreviewTransport,
    context: &Context,
    request: &DeploymentRequest,
    metadata: &ReviewResource,
) -> Result<(PreviewFieldImpact, Observation)> {
    let resource = request
        .next
        .compiled
        .ir
        .resources
        .iter()
        .find(|resource| resource.id == metadata.id)
        .ok_or_else(|| invalid("preview canonical target is missing"))?;
    let identity = ResourceIdentity {
        api_group: metadata.identity.api_group.clone(),
        resource: metadata.identity.resource.clone(),
        namespace: metadata.identity.namespace.clone(),
        name: metadata.identity.name.clone(),
    };
    let target = PreviewTarget::new(
        &resource.api_version,
        &resource.kind,
        &identity.api_group,
        &identity.resource,
        &identity.namespace,
        &identity.name,
    )?;
    let listed = transport.list_named(&target).await?;
    let force = require_target_authority(request, resource, &identity, &listed)?;
    let declarations = Declarations {
        request,
        next: &context.next,
        previous: context.previous.as_ref(),
    };
    let impact =
        apply_observed(transport, &declarations, metadata, &target, &listed, force).await?;
    Ok((impact, Observation::capture(target, &listed)))
}

pub(super) async fn apply_observed(
    transport: &PreviewTransport,
    context: &Declarations<'_>,
    metadata: &ReviewResource,
    target: &PreviewTarget,
    listed: &NamedPreview,
    force: bool,
) -> Result<PreviewFieldImpact> {
    let request = context.request;
    let resource = request
        .next
        .compiled
        .ir
        .resources
        .iter()
        .find(|item| item.id == metadata.id)
        .ok_or_else(|| invalid("preview canonical resource missing"))?;
    let before = listed.object().map(object_value).transpose()?;
    let uid = listed
        .object()
        .and_then(|object| object.metadata.uid.as_deref());
    let live = before
        .as_ref()
        .zip(uid)
        .map(|(object, expected_uid)| PreviewLiveObject {
            object,
            expected_uid,
        });
    let composition = compose_preview(context.next, &metadata.identity, &resource.desired, live)?;
    let precondition = match listed.object() {
        Some(object) => ApplyPrecondition::Existing {
            uid: object
                .metadata
                .uid
                .as_deref()
                .ok_or_else(|| invalid("preview existing UID missing"))?,
            resource_version: object
                .metadata
                .resource_version
                .as_deref()
                .ok_or_else(|| invalid("preview existing version missing"))?,
        },
        None => ApplyPrecondition::Absent {
            list_resource_version: listed.resource_version(),
        },
    };
    let apply = crate::conditional_apply_request(
        composition.payload(),
        &resource.name,
        &resource.namespace,
        &format!("apsides-{}", request.next.compiled.ir.domain.id),
        force,
        precondition,
    )?;
    let projected = transport.dry_run_apply(target, &apply).await?;
    require_apply_owner(
        projected.object(),
        resource,
        &request.next.compiled.ir.domain.id,
    )?;
    let projected_value = object_value(projected.object())?;
    composition.verify_projected(&projected_value)?;
    let impact = classify_preview_fields(
        context.previous,
        context.next,
        &metadata.identity,
        PreviewObjects {
            before: before.as_ref(),
            projected: &projected_value,
        },
    )?;
    Ok(impact)
}

fn require_target_authority(
    request: &DeploymentRequest,
    resource: &Resource,
    identity: &ResourceIdentity,
    listed: &NamedPreview,
) -> Result<bool> {
    let intent = request
        .ownership_intents
        .iter()
        .find(|intent| &intent.approval.identity == identity);
    let previous = request.predecessor().and_then(|previous| {
        previous.inventory.resources.iter().find(|old| {
            old.identity.api_group == identity.api_group
                && old.identity.resource == identity.resource
                && old.identity.namespace == identity.namespace
                && old.identity.name == identity.name
        })
    });
    let claim = request
        .registry
        .claims
        .iter()
        .find(|claim| &claim.identity == identity);
    let prior_uid = previous.and_then(|old| old.uid.as_deref());
    let claim_uid = claim.and_then(|claim| claim.uid.as_deref());
    if prior_uid
        .zip(claim_uid)
        .is_some_and(|(old, claimed)| old != claimed)
    {
        return Err(invalid(
            "preview predecessor and protected claim UIDs disagree",
        ));
    }
    let Some(object) = listed.object() else {
        if intent.is_some()
            || prior_uid.is_some()
            || claim_uid.is_some()
            || !request.next.compiled.capabilities.iter().any(|capability| {
                capability.verb == "create"
                    && capability.namespace == identity.namespace
                    && capability.api_group == identity.api_group
                    && capability.resource == identity.resource
                    && capability.subresource.is_none()
            })
        {
            return Err(invalid("preview claim forbids creating an absent target"));
        }
        return Ok(resource.force);
    };
    if object.metadata.deletion_timestamp.is_some() {
        return Err(invalid("preview target is being deleted"));
    }
    if let Some(intent) = intent {
        let approval = &intent.approval;
        if approval.action != OwnershipAction::Adopt
            || approval.permitted_apply != resource.desired
            || object.metadata.uid.as_deref() != Some(approval.observed_uid.as_str())
            || object.metadata.resource_version.as_deref()
                != Some(approval.observed_resource_version.as_str())
            || compiler_core::digest(&object.metadata.managed_fields)?
                != approval.prior_managed_fields_digest
        {
            return Err(invalid(
                "preview adoption differs from its exact protected approval",
            ));
        }
        return Ok(false);
    }
    let uid = prior_uid
        .or(claim_uid)
        .filter(|_| previous.is_some())
        .ok_or_else(|| invalid("preview cannot adopt an unapproved existing target"))?;
    if object.metadata.uid.as_deref() != Some(uid) {
        return Err(invalid("preview target UID differs from predecessor"));
    }
    require_apply_owner(object, resource, &request.next.compiled.ir.domain.id)?;
    Ok(resource.force)
}

pub(super) fn require_apply_owner(
    object: &DynamicObject,
    resource: &Resource,
    domain: &str,
) -> Result<()> {
    let manager = format!("apsides-{domain}");
    let mut apply = 0;
    for entry in object
        .metadata
        .managed_fields
        .iter()
        .flatten()
        .filter(|entry| {
            entry.manager.as_deref() == Some(manager.as_str())
                && entry.subresource.as_deref().is_none_or(str::is_empty)
        })
    {
        if entry.operation.as_deref() != Some("Apply")
            || entry.api_version.as_deref() != Some(resource.api_version.as_str())
            || entry.fields_type.as_deref() != Some("FieldsV1")
            || entry.fields_v1.is_none()
        {
            return Err(invalid(
                "preview did not establish exact stable-manager Apply evidence",
            ));
        }
        apply += 1;
    }
    if apply != 1 {
        return Err(invalid(
            "preview stable-manager Apply evidence is missing or ambiguous",
        ));
    }
    Ok(())
}

pub(super) fn object_value(object: &DynamicObject) -> Result<serde_json::Value> {
    crate::preview_limits::validate_serialized(object, 4 * 1024 * 1024)?;
    serde_json::to_value(object).map_err(|_| invalid("preview object projection failed"))
}

#[cfg(test)]
#[path = "preview_execution_tests.rs"]
mod tests;
