use crate::{
    ClaimState, Error, GenerationAllocation, GenerationPlan, PredecessorGeneration, ProtectedClaim,
    ResourceIdentity, Result, service_account_name, valid_digest, valid_id,
};
use inventory::{INVENTORY_SCHEMA, InventoryScope, InventorySnapshot, TrackedResource};
use std::collections::{BTreeMap, BTreeSet};

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
        let identity = crate::planner::resource_identity(resource)?;
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

/// Project a trusted historical generation into the complete metadata needed by
/// a future planner. The allocation remains the protected authority for the
/// immutable inventory metadata; active claims bind its captured UIDs.
/// # Errors
/// Rejects altered compiler output, allocation mismatches, incomplete claims,
/// or malformed inventory and identity metadata.
pub fn predecessor_metadata(
    generation: &GenerationPlan,
    allocation: &GenerationAllocation,
    claims: &[ProtectedClaim],
) -> Result<PredecessorGeneration> {
    if !valid_id(&generation.generation_id)
        || !valid_digest(&generation.artifact_digest)
        || !valid_id(&allocation.transition_id)
        || !valid_id(&allocation.service_account_name)
        || allocation.generation_id != generation.generation_id
        || allocation.artifact_digest != generation.artifact_digest
        || allocation.ir_digest != generation.compiled.ir_digest
        || allocation.service_account_name != service_account_name(generation)?
    {
        return Err(Error::Plan(
            "predecessor generation differs from its protected allocation".into(),
        ));
    }
    let (scope, inventory) = generation_inventory(generation, claims)?;
    validate_inventory_checkpoint(&scope, &inventory, allocation)?;

    let resources: BTreeMap<_, _> = inventory
        .resources
        .iter()
        .map(|resource| {
            (
                ResourceIdentity {
                    api_group: resource.identity.api_group.clone(),
                    resource: resource.identity.resource.clone(),
                    namespace: resource.identity.namespace.clone(),
                    name: resource.identity.name.clone(),
                },
                resource,
            )
        })
        .collect();
    let mut seen = BTreeSet::new();
    for claim in claims {
        if claim.domain_id != scope.controller {
            continue;
        }
        let tracked = resources.get(&claim.identity).ok_or_else(|| {
            Error::Plan("protected predecessor claim is absent from its inventory".into())
        })?;
        if !seen.insert(&claim.identity)
            || claim.state != ClaimState::Active
            || claim.generation_id != allocation.generation_id
            || claim.transition_id != allocation.transition_id
            || claim.uid != tracked.uid
        {
            return Err(Error::Plan(
                "protected predecessor claim differs from its allocation or inventory".into(),
            ));
        }
    }
    if resources.keys().any(|identity| !seen.contains(identity)) {
        return Err(Error::Plan(
            "predecessor inventory lacks complete active claims".into(),
        ));
    }

    let retirement_role_binding_namespaces = generation
        .compiled
        .capabilities
        .iter()
        .map(|capability| capability.namespace.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(PredecessorGeneration {
        allocation: allocation.clone(),
        scope,
        control_namespace: generation.compiled.ir.domain.control_namespace.clone(),
        inventory,
        retirement_role_binding_namespaces,
    })
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
    match (request.predecessor(), persisted) {
        (None, None) => Ok(()),
        (Some(previous), Some(persisted)) => {
            if &previous.inventory != persisted {
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
        "schemaVersion": "apsides.dev/inventory-metadata-checkpoint/v1",
        "scope": scope,
        "snapshot": metadata,
    }))?)
}

/// Verify metadata against protected allocation authority without any compiled
/// predecessor, manifest or source. This does not validate live UID ownership.
/// # Errors
/// Rejects invalid commitments and changed generation or metadata.
pub fn validate_inventory_checkpoint(
    scope: &InventoryScope,
    snapshot: &InventorySnapshot,
    allocation: &crate::GenerationAllocation,
) -> Result<()> {
    crate::validate_allocation(allocation)?;
    let expected = &allocation.inventory_metadata_digest;
    if snapshot.generation != allocation.generation_id
        || &inventory_metadata_digest(scope, snapshot)? != expected
    {
        return Err(Error::Plan(
            "inventory metadata differs from its protected allocation".into(),
        ));
    }
    Ok(())
}
