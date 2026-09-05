//! Authenticate metadata-only predecessor requests against protected history.
use crate::{NamespaceRegistry, Result, StoredTransition, invalid};
use deploy::{Phase, PredecessorGeneration};
use inventory::InventoryScope;
use std::collections::BTreeSet;

pub(crate) fn authenticate<'a>(
    registry: &'a NamespaceRegistry,
    metadata: &PredecessorGeneration,
) -> Result<&'a StoredTransition> {
    let allocation = &metadata.allocation;
    let previous = registry
        .transitions
        .get(&allocation.transition_id)
        .ok_or_else(|| invalid("predecessor allocation has no protected transition"))?;
    let domain = &previous.plan.request.next.compiled.ir.domain;
    let scope = InventoryScope {
        cluster: domain.cluster_binding.clone(),
        namespace: domain.namespace.clone(),
        controller: domain.id.clone(),
    };
    if previous.record.phase != Phase::Committed
        || previous.plan.request.transition_id != allocation.transition_id
        || previous.record.transition_id != allocation.transition_id
        || previous.plan.generation_reservation != *allocation
        || metadata.scope != scope
        || metadata.control_namespace != domain.control_namespace
        || registry
            .transitions
            .values()
            .filter(|stored| {
                stored.record.phase == Phase::Committed
                    && stored.plan.generation_reservation == *allocation
            })
            .count()
            != 1
    {
        return Err(invalid(
            "predecessor does not match a unique committed protected allocation and scope",
        ));
    }
    deploy::verify_record(&previous.plan, &previous.record)?;
    // The inventory metadata digest does not bind retirement permissions. Derive
    // their exact namespaces independently from the authenticated historical plan.
    let namespaces: Vec<_> = previous
        .plan
        .request
        .next
        .compiled
        .capabilities
        .iter()
        .map(|capability| capability.namespace.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if metadata.retirement_role_binding_namespaces != namespaces {
        return Err(invalid(
            "predecessor retirement namespaces differ from protected capabilities",
        ));
    }
    Ok(previous)
}
