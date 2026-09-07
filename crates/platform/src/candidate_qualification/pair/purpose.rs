use super::{CandidatePairPurpose, strict_capability_expansion};
use crate::{Result, invalid};
use compiler_core::{CompiledPlan, DeletionPolicy};
use std::collections::BTreeSet;

#[cfg(test)]
mod tests;

pub(super) fn validate_relation(
    purpose: &CandidatePairPurpose,
    initial: &CompiledPlan,
    successor: &CompiledPlan,
    initial_policy: &str,
    successor_policy: &str,
) -> Result<()> {
    if !crate::valid_digest(initial_policy)
        || !crate::valid_digest(successor_policy)
        || initial.ir.domain != successor.ir.domain
    {
        return Err(invalid(
            "candidate pair purpose has incompatible domain or policy identities",
        ));
    }
    match purpose {
        CandidatePairPurpose::PermissionExpansion {} => {
            if initial_policy == successor_policy
                || !strict_capability_expansion(&initial.capabilities, &successor.capabilities)
            {
                return Err(invalid(
                    "candidate pair is not an exact permission expansion",
                ));
            }
        }
        CandidatePairPurpose::LastTypeCleanup { identity } => {
            if initial_policy != successor_policy
                || !strict_capability_expansion(&successor.capabilities, &initial.capabilities)
                || successor.capabilities.iter().any(|capability| {
                    capability.api_group == identity.api_group
                        && capability.resource == identity.resource
                })
            {
                return Err(invalid(
                    "cleanup pair requires a stable policy and removal of the API type",
                ));
            }
            let initial_identities = initial
                .ir
                .resources
                .iter()
                .map(deploy::resource_identity)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let successor_identities = successor
                .ir
                .resources
                .iter()
                .map(deploy::resource_identity)
                .collect::<std::result::Result<BTreeSet<_>, _>>()?;
            let selected: Vec<_> = initial_identities
                .iter()
                .zip(&initial.ir.resources)
                .filter(|(candidate, _)| *candidate == identity)
                .collect();
            if selected.len() != 1
                || selected.iter().any(|(_, resource)| {
                    resource.lifecycle.deletion_policy != DeletionPolicy::Delete
                })
                || successor_identities.iter().any(|candidate| {
                    candidate.api_group == identity.api_group
                        && candidate.resource == identity.resource
                })
                || initial_identities.len() != successor_identities.len() + 1
                || successor_identities.len() != successor.ir.resources.len()
                || initial_identities
                    .iter()
                    .filter(|candidate| *candidate != identity)
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    != successor_identities
            {
                return Err(invalid(
                    "cleanup pair must remove exactly its selected prior-delete resource",
                ));
            }
            validate_cleanup_projection(initial, successor, identity)?;
        }
    }
    Ok(())
}

fn validate_cleanup_projection(
    initial: &CompiledPlan,
    successor: &CompiledPlan,
    identity: &deploy::ResourceIdentity,
) -> Result<()> {
    let mut projected = initial.ir.clone();
    projected.resources.clear();
    for resource in &initial.ir.resources {
        if deploy::resource_identity(resource)? != *identity {
            projected.resources.push(resource.clone());
        }
    }
    // Source coordinates and provenance describe the new source, not a change
    // to the surviving declarations. Recompile everything else unchanged.
    projected.provenance = successor.ir.provenance.clone();
    for resource in &mut projected.resources {
        resource.source = successor
            .ir
            .resources
            .iter()
            .find(|next| next.id == resource.id)
            .ok_or_else(|| invalid("cleanup projection changed a surviving resource ID"))?
            .source
            .clone();
    }
    for input in &mut projected.inputs {
        input.source = successor
            .ir
            .inputs
            .iter()
            .find(|next| next.id == input.id)
            .ok_or_else(|| invalid("cleanup projection removed an input"))?
            .source
            .clone();
    }
    for reaction in &mut projected.reactions {
        reaction.source = successor
            .ir
            .reactions
            .iter()
            .find(|next| next.id == reaction.id)
            .ok_or_else(|| invalid("cleanup projection removed a reaction"))?
            .source
            .clone();
    }
    let compiled = compiler_core::validate(&projected)
        .map_err(|_| invalid("cleanup projection is not a valid compiled declaration"))?;
    if compiled != *successor {
        return Err(invalid(
            "cleanup pair changes declarations beyond its selected resource",
        ));
    }
    Ok(())
}

pub(super) fn validate_cleanup_plan(
    purpose: &CandidatePairPurpose,
    plan: &deploy::TransitionPlan,
) -> Result<()> {
    let CandidatePairPurpose::LastTypeCleanup { identity } = purpose else {
        return Ok(());
    };
    if deploy::plan_transition(&plan.request)? != *plan {
        return Err(invalid(
            "cleanup qualification requires its canonical frozen plan",
        ));
    }
    let previous = plan
        .request
        .previous
        .as_deref()
        .ok_or_else(|| invalid("cleanup qualification requires a predecessor"))?;
    let [item] = plan.cleanup.as_slice() else {
        return Err(invalid(
            "cleanup qualification requires exactly one cleanup item",
        ));
    };
    if &item.identity != identity
        || item.policy != DeletionPolicy::Delete
        || item.expected_uid.as_ref().is_none_or(String::is_empty)
        || item.prior_generation_id != previous.allocation.generation_id
        || plan.cleanup_capabilities.len() != 2
        || ["get", "delete"].iter().any(|verb| {
            !plan.cleanup_capabilities.iter().any(|capability| {
                capability.namespace == identity.namespace
                    && capability.api_group == identity.api_group
                    && capability.resource == identity.resource
                    && capability.subresource.is_none()
                    && capability.verb == *verb
                    && capability.names == [identity.name.clone()]
            })
        })
    {
        return Err(invalid(
            "cleanup qualification plan differs from its exact UID-bound authority",
        ));
    }
    Ok(())
}
