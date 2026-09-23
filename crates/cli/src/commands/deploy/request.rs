//! Building the deployment request for a fresh transition, and reserving it.

use crate::Result;
use deploy::{DeploymentRequest, GenerationPlan, RegistrySnapshot};
use platform::{IssuedDeployment, PlatformClient};
use std::collections::BTreeSet;

pub(super) async fn reserve_new_transition(
    issued: &IssuedDeployment,
    inventory: &dyn inventory::InventoryStore,
    mut registry: platform::Versioned<platform::NamespaceRegistry>,
    verified: &release::IntegrityVerifiedCandidate,
    transition_id: &str,
    executor: &str,
    ownership_names: &[String],
) -> Result<crate::review::LiveReview> {
    let platform = issued.platform();
    registry
        .value
        .next_sequence
        .checked_add(1)
        .ok_or("registry allocation counter exhausted")?;
    let names = deploy::allocation_names(&registry.uid, registry.value.next_sequence)?;
    if transition_id != names.transition_id {
        return Err("fresh transition must use the canonical registry allocation identity".into());
    }
    let domain = &verified.compiled().ir.domain;

    let active = registry
        .value
        .histories
        .get(&domain.id)
        .and_then(|history| {
            history
                .active_allocation
                .as_ref()
                .map(|a| a.generation_id.as_str())
        });
    crate::connection::require_head(inventory, domain, active).await?;
    if registry
        .value
        .histories
        .get(&domain.id)
        .is_some_and(|h| h.active_allocation.is_some())
    {
        platform.ensure_active_writer_checkpoint(&domain.id).await?;
        // Plan on the captured UIDs before they are written, so the ceilings
        // and the declaration review refuse a release without changing the
        // registry, claims, audit or inventory.
        registry = crate::connection::capture_active(
            platform,
            inventory,
            domain,
            |captured, head| async move {
                plan_request(
                    platform,
                    &captured,
                    Some(&head),
                    verified,
                    transition_id,
                    ownership_names,
                )
                .await
                .map(drop)
            },
        )
        .await?;
    }
    let head = crate::connection::load(inventory, domain).await?;
    let Planned {
        request,
        plan,
        previous_review,
    } = plan_request(
        platform,
        &registry,
        head.as_ref().map(|head| &head.snapshot),
        verified,
        transition_id,
        ownership_names,
    )
    .await?;
    crate::connection::require_request(inventory, &request).await?;
    let live = if issued.identity().preview.is_some() {
        let report = issued.preview_fresh(&request, verified, inventory).await?;
        crate::review::LiveReview::fresh(&plan, previous_review.as_ref(), report)?
    } else {
        crate::review::LiveReview::not_approved()
    };
    crate::review::preflight(&plan, previous_review.as_ref(), &live)?;
    platform
        .reserve_transition(&request, executor, verified, inventory)
        .await?;
    Ok(live)
}

struct Planned {
    request: DeploymentRequest,
    plan: deploy::TransitionPlan,
    previous_review: Option<coordination::ReviewMetadataV1>,
}

/// Every refusal that needs no protected write: request preparation, the
/// ceilings and inventory checks in the planner, and the declaration review.
async fn plan_request(
    platform: &PlatformClient,
    registry: &platform::Versioned<platform::NamespaceRegistry>,
    head: Option<&inventory::InventorySnapshot>,
    verified: &release::IntegrityVerifiedCandidate,
    transition_id: &str,
    ownership_names: &[String],
) -> Result<Planned> {
    let request = prepare_request(
        platform,
        head,
        registry,
        verified,
        transition_id,
        ownership_names,
    )
    .await?;
    deploy::validate_transition_inventory(&request, head)?;
    let previous_review = if request.predecessor().is_some() {
        platform
            .load_active_review_metadata(
                &verified.compiled().ir.domain.id,
                &request.registry.registry_uid,
                &request.registry.revision,
            )
            .await?
    } else {
        None
    };
    let plan = deploy::plan_transition(&request)?;
    // Refuse an incomplete declaration review before issuing any preview request.
    let pending = crate::review::LiveReview::Unavailable {
        reason: "fresh-preview-not-yet-captured",
    };
    crate::review::preflight(&plan, previous_review.as_ref(), &pending)?;
    Ok(Planned {
        request,
        plan,
        previous_review,
    })
}

async fn prepare_request(
    platform: &PlatformClient,
    head: Option<&inventory::InventorySnapshot>,
    registry: &platform::Versioned<platform::NamespaceRegistry>,
    verified: &release::IntegrityVerifiedCandidate,
    transition_id: &str,
    ownership_names: &[String],
) -> Result<DeploymentRequest> {
    let domain = &verified.compiled().ir.domain;
    let policy = platform.load_policy(&domain.id).await?;
    let mut history = registry
        .value
        .histories
        .get(&domain.id)
        .cloned()
        .ok_or("platform has not provisioned this domain history")?;
    history.revision.clone_from(&registry.resource_version);
    let previous = prepare_predecessor(head, &registry.value, domain, &history)?;
    let names = deploy::allocation_names(&registry.uid, registry.value.next_sequence)?;
    if transition_id != names.transition_id {
        return Err("fresh transition must use the canonical registry allocation identity".into());
    }
    let generation_id = names.generation_id;
    let next = GenerationPlan {
        generation_id,
        artifact_digest: verified.release_digest().into(),
        compiled: verified.compiled().clone(),
    };
    if ownership_names.iter().collect::<BTreeSet<_>>().len() != ownership_names.len() {
        return Err("ownership intent names must be unique".into());
    }
    let ownership_intents = if ownership_names.is_empty() {
        vec![]
    } else {
        platform
            .load_ownership_intents(ownership_names, &next, transition_id)
            .await?
    };
    let request = DeploymentRequest {
        transition_id: transition_id.into(),
        previous,
        next,
        runtime_ceiling: policy.value.runtime_ceiling,
        cleanup_ceiling: policy.value.cleanup_ceiling,
        registry: RegistrySnapshot {
            registry_uid: registry.uid.clone(),
            next_sequence: registry.value.next_sequence,
            revision: registry.resource_version.clone(),
            claims: registry.value.claims.clone(),
            generation_history: history,
        },
        observations: vec![],
        ownership_intents,
    };
    Ok(request)
}

fn prepare_predecessor(
    head: Option<&inventory::InventorySnapshot>,
    registry: &platform::NamespaceRegistry,
    domain: &compiler_core::Domain,
    history: &deploy::GenerationHistory,
) -> Result<Option<Box<deploy::PredecessorGeneration>>> {
    let Some(allocation) = &history.active_allocation else {
        return Ok(None);
    };
    let checkpoint = registry
        .active_writer_checkpoints
        .get(&domain.id)
        .ok_or("active writer checkpoint missing; explicit recovery required")?;
    if &checkpoint.allocation != allocation
        || checkpoint.control_namespace != domain.control_namespace
    {
        return Err("active writer checkpoint differs from protected allocation".into());
    }

    let head = head.ok_or("active inventory missing; explicit recovery required")?;
    let scope = inventory::InventoryScope {
        cluster: domain.cluster_binding.clone(),
        namespace: domain.namespace.clone(),
        controller: domain.id.clone(),
    };
    let predecessor = checkpoint.predecessor_metadata(&scope, head)?;
    Ok(Some(Box::new(predecessor)))
}
