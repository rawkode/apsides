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

/// Commit only to approved inventory metadata. UIDs are captured later and are
/// authenticated separately against protected claims; their completion must not
/// change this immutable allocation commitment.
/// # Errors
/// Rejects malformed, unsorted or foreign-scope tracking metadata.
pub fn inventory_metadata_digest(
    scope: &InventoryScope,
    snapshot: &InventorySnapshot,
) -> Result<String> {
    snapshot
        .encode(scope)
        .map_err(|_| Error::Plan("invalid inventory checkpoint metadata".into()))?;
    let mut metadata = snapshot.clone();
    for resource in &mut metadata.resources {
        resource.uid = None;
    }
    Ok(compiler_core::digest(&serde_json::json!({
        "schemaVersion": "reakt.dev/inventory-metadata-checkpoint/v1",
        "scope": scope,
        "snapshot": metadata,
    }))?)
}

/// Verify metadata against protected allocation authority without any compiled
/// predecessor, manifest or source. This does not validate live UID ownership.
/// # Errors
/// Rejects missing legacy checkpoints and changed generation or metadata.
pub fn validate_inventory_checkpoint(
    scope: &InventoryScope,
    snapshot: &InventorySnapshot,
    allocation: &crate::GenerationAllocation,
) -> Result<()> {
    let expected = allocation
        .inventory_metadata_digest
        .as_ref()
        .ok_or_else(|| {
            Error::Plan(
                "protected inventory checkpoint is missing; explicit recovery is required".into(),
            )
        })?;
    if snapshot.generation != allocation.generation_id
        || &inventory_metadata_digest(scope, snapshot)? != expected
    {
        return Err(Error::Plan(
            "inventory metadata differs from its protected allocation".into(),
        ));
    }
    Ok(())
}
