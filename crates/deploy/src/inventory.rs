use crate::{Error, GenerationPlan, ProtectedClaim, Result};
use inventory::{INVENTORY_SCHEMA, InventoryScope, InventorySnapshot, TrackedResource};
use std::collections::BTreeMap;

/// Project a verified generation and matching claims into metadata-only tracking.
/// This projection grants no ownership or deletion authority.
/// # Errors
/// Rejects altered compiler output, duplicate claims, or mismatched claim owners.
pub fn generation_inventory(
    generation: &GenerationPlan,
    claims: &[ProtectedClaim],
) -> Result<(InventoryScope, InventorySnapshot)> {
    if compiler_core::validate(&generation.compiled.ir)? != generation.compiled {
        return Err(Error::Plan(
            "inventory requires verified compiler output".into(),
        ));
    }
    let domain = &generation.compiled.ir.domain;
    let scope = InventoryScope {
        cluster: domain.cluster_binding.clone(),
        namespace: domain.namespace.clone(),
        controller: domain.id.clone(),
    };
    let mut by_identity = BTreeMap::new();
    for claim in claims {
        if by_identity.insert(&claim.identity, claim).is_some() {
            return Err(Error::Plan(
                "inventory projection rejects duplicate claims".into(),
            ));
        }
    }
    let mut resources = Vec::new();
    for resource in &generation.compiled.ir.resources {
        let identity = crate::planner::identity(resource)?;
        let uid = match by_identity.get(&identity) {
            Some(claim) => {
                if claim.domain_id != domain.id || claim.generation_id != generation.generation_id {
                    return Err(Error::Plan(
                        "inventory claim belongs to another generation".into(),
                    ));
                }
                claim.uid.clone()
            }
            None => None,
        };
        resources.push(TrackedResource::from_resource(
            resource,
            identity.resource,
            uid,
        ));
    }
    resources.sort_by(|a, b| a.identity.cmp(&b.identity));
    let snapshot = InventorySnapshot {
        schema_version: INVENTORY_SCHEMA.into(),
        generation: generation.generation_id.clone(),
        resources,
    };
    snapshot
        .encode(&scope)
        .map_err(|_| Error::Plan("invalid generation inventory metadata".into()))?;
    Ok((scope, snapshot))
}

/// Validate persisted tracking against the approved predecessor and protected
/// claims. A generation name alone cannot establish inventory completeness.
/// # Errors
/// Rejects invalid plans, missing/extra inventory, altered lifecycle metadata,
/// or UIDs that differ from the protected planning snapshot.
pub fn validate_transition_inventory(
    request: &crate::DeploymentRequest,
    persisted: Option<&InventorySnapshot>,
) -> Result<()> {
    crate::plan_transition(request)?;
    match (&request.previous, persisted) {
        (None, None) => Ok(()),
        (Some(previous), Some(persisted)) => {
            let (_, expected) = generation_inventory(previous, &request.registry.claims)?;
            if &expected != persisted {
                return Err(Error::Plan("persisted inventory differs from the approved predecessor and protected claims".into()));
            }
            Ok(())
        }
        _ => Err(Error::Plan(
            "persisted inventory is missing or unexpected for this transition".into(),
        )),
    }
}
