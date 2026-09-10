//! Complete metadata projection; canonicality alone is not release or registry authority.
use crate::{Result, invalid};
use coordination::{ClaimScope, REVIEW_METADATA_SCHEMA, ReviewMetadataV1, ReviewScope};
use deploy::TransitionPlan;

#[path = "review_projection_declarations.rs"]
mod declarations;
#[path = "review_projection_permissions.rs"]
mod permissions;

/// Project the entire supported declaration review from a canonical transition.
///
/// The caller must separately authenticate the verified release, predecessor and
/// protected allocation before publishing or binding the returned metadata.
/// # Errors
/// Rejects noncanonical plans, unsupported semantics or incomplete/oversized metadata.
pub fn project_review_metadata(plan: &TransitionPlan) -> Result<ReviewMetadataV1> {
    let canonical = deploy::plan_transition(&plan.request)
        .map_err(|_| invalid("review projection requires a canonical transition plan"))?;
    if canonical != *plan {
        return Err(invalid(
            "review projection requires a canonical transition plan",
        ));
    }
    let allocation = &plan.generation_reservation;
    if allocation.registry_uid != plan.request.registry.registry_uid
        || allocation.transition_id != plan.request.transition_id
    {
        return Err(invalid(
            "review projection allocation differs from its request",
        ));
    }
    let domain = &plan.request.next.compiled.ir.domain;
    let scope = ReviewScope {
        claim_scope: ClaimScope {
            registry_uid: allocation.registry_uid.clone(),
            cluster_binding: domain.cluster_binding.clone(),
            control_namespace: domain.control_namespace.clone(),
            workload_namespace: domain.namespace.clone(),
        },
        domain_id: domain.id.clone(),
        transition_id: allocation.transition_id.clone(),
        sequence: allocation.sequence,
    };
    let catalogs = declarations::project(plan)?;
    let derived = crate::deployment_permissions(plan)
        .map_err(|_| invalid("review projection deployment requirements are invalid"))?;
    let deployment_permissions = permissions::deployment(&derived)?;
    let runtime_permissions = permissions::capabilities(&plan.request.next.compiled.capabilities);
    let cleanup_permissions = permissions::capabilities(&plan.cleanup_capabilities);
    let metadata = ReviewMetadataV1 {
        schema_version: REVIEW_METADATA_SCHEMA.into(),
        scope,
        resources: catalogs.resources,
        inputs: catalogs.inputs,
        reactions: catalogs.reactions,
        compatibility: permissions::compatibility(
            &domain.budgets,
            &runtime_permissions,
            &cleanup_permissions,
            &deployment_permissions,
        ),
        runtime_permissions,
        cleanup_permissions,
        deployment_permissions,
        enforcement: permissions::enforcement(plan)?,
    };
    metadata
        .validate()
        .map_err(|_| invalid("review projection exceeds the supported metadata contract"))?;
    Ok(metadata)
}
