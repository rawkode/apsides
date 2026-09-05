use crate::{NamespaceRegistry, PlatformClient, Result, Versioned, invalid};
use deploy::{ClaimState, Phase, ProtectedClaim};
use inventory::{InventoryScope, InventorySnapshot, InventoryStore, VersionedInventory};
use std::collections::BTreeMap;

impl PlatformClient {
    /// Complete unknown inventory UIDs from the protected result of live capture.
    /// Preserve the existing inventory records and policies without an old artifact.
    /// # Errors
    /// Rejects changed registry/head, incomplete claims, known UID changes or an
    /// unfinished generation. Database publication remains conditional and separate
    /// from Kubernetes; a post-publication race requires retry.
    pub async fn publish_captured_inventory(
        &self,
        store: &dyn InventoryStore,
        domain: &str,
        captured: &Versioned<NamespaceRegistry>,
        expected_head: &VersionedInventory,
    ) -> Result<VersionedInventory> {
        let registry = self.load_registry().await?;
        if registry.uid != captured.uid || registry.resource_version != captured.resource_version {
            return Err(invalid("registry changed after UID capture"));
        }
        let generation = &expected_head.snapshot.generation;
        if registry
            .value
            .histories
            .get(domain)
            .and_then(|history| history.active_generation_id.as_ref())
            != Some(generation)
            || registry.value.transitions.values().any(|entry| {
                entry.plan.request.next.compiled.ir.domain.id == domain
                    && entry.record.phase != Phase::Committed
            })
        {
            return Err(invalid(
                "UID publication requires the protected active generation",
            ));
        }
        let scope = InventoryScope {
            cluster: self.cluster_binding.clone(),
            namespace: self.workload_namespace.clone(),
            controller: domain.into(),
        };
        let allocation = active_inventory_allocation(&registry.value, domain, generation)?;
        deploy::validate_inventory_checkpoint(&scope, &expected_head.snapshot, allocation)?;
        let current_head = store
            .load(&scope)
            .await?
            .ok_or_else(|| invalid("active inventory disappeared"))?;
        if &current_head != expected_head {
            return Err(invalid("inventory changed after UID capture"));
        }
        let snapshot = complete_uids(
            &scope,
            &current_head.snapshot,
            &registry.value.claims,
            &allocation.transition_id,
        )?;
        self.recheck_inventory_registry(&registry).await?;
        let published = store
            .publish(&scope, Some(&current_head.revision), &snapshot)
            .await?;
        self.recheck_inventory_registry(&registry).await?;
        Ok(published)
    }
}

fn complete_uids(
    scope: &InventoryScope,
    snapshot: &InventorySnapshot,
    claims: &[ProtectedClaim],
    transition_id: &str,
) -> Result<InventorySnapshot> {
    snapshot
        .encode(scope)
        .map_err(|_| invalid("invalid captured inventory metadata"))?;
    let mut active = BTreeMap::new();
    for claim in claims
        .iter()
        .filter(|claim| claim.domain_id == scope.controller)
    {
        if claim.state != ClaimState::Active
            || claim.generation_id != snapshot.generation
            || claim.transition_id != transition_id
            || active
                .insert(
                    inventory::ResourceKey {
                        api_group: claim.identity.api_group.clone(),
                        resource: claim.identity.resource.clone(),
                        namespace: claim.identity.namespace.clone(),
                        name: claim.identity.name.clone(),
                    },
                    claim,
                )
                .is_some()
        {
            return Err(invalid("captured inventory requires unique active claims"));
        }
    }
    if active.len() != snapshot.resources.len() {
        return Err(invalid(
            "captured inventory has incomplete resource membership",
        ));
    }
    let mut next = snapshot.clone();
    for resource in &mut next.resources {
        let claim = active
            .get(&resource.identity)
            .ok_or_else(|| invalid("captured resource has no protected claim"))?;
        if resource
            .uid
            .as_ref()
            .is_some_and(|uid| claim.uid.as_ref() != Some(uid))
        {
            return Err(invalid("captured inventory would change a known UID"));
        }
        resource.uid.clone_from(&claim.uid);
    }
    next.encode(scope)
        .map_err(|_| invalid("invalid captured UID metadata"))?;
    Ok(next)
}

/// Resolve one protected active allocation without historical plan lookup.
pub(crate) fn active_inventory_allocation<'a>(
    registry: &'a NamespaceRegistry,
    domain: &str,
    generation: &str,
) -> Result<&'a deploy::GenerationAllocation> {
    let history = registry
        .histories
        .get(domain)
        .ok_or_else(|| invalid("active inventory history is missing"))?;
    if history.domain_id != domain || history.active_generation_id.as_deref() != Some(generation) {
        return Err(invalid(
            "inventory generation differs from protected active history",
        ));
    }
    let mut matches = history
        .allocations
        .iter()
        .filter(|entry| entry.generation_id == generation);
    let allocation = matches
        .next()
        .ok_or_else(|| invalid("active inventory allocation is missing"))?;
    if matches.next().is_some() {
        return Err(invalid("active inventory allocation is ambiguous"));
    }
    Ok(allocation)
}
