use crate::{NamespaceRegistry, PlatformClient, Result, Versioned, invalid};
use deploy::{ClaimState, Phase, generation_inventory, verify_record};
use inventory::{InventorySnapshot, InventoryStore, VersionedInventory};

impl PlatformClient {
    /// Publish tracking only for the latest committed generation. Read the head
    /// before rechecking the registry, then CAS that head so a delayed publisher
    /// cannot overwrite a newer inventory. This operation grants no API authority.
    /// Capture active claim UIDs first when live UID evidence is required.
    /// # Errors
    /// Rejects unfinished transitions, missing upgrade inventory, regressed UIDs,
    /// changed registry state, stale heads, and unavailable or corrupt storage.
    pub async fn publish_committed_inventory(
        &self,
        store: &dyn InventoryStore,
        transition_id: &str,
        executor_id: &str,
    ) -> Result<VersionedInventory> {
        let registry = self.load_registry().await?;
        let stored = registry
            .value
            .transitions
            .get(transition_id)
            .ok_or_else(|| invalid("unknown inventory transition"))?;
        verify_record(&stored.plan, &stored.record)?;
        let generation = &stored.plan.request.next;
        let domain = &generation.compiled.ir.domain;
        if domain.namespace != self.workload_namespace
            || domain.control_namespace != self.control_namespace
            || domain.cluster_binding != self.cluster_binding
            || stored.record.phase != Phase::Committed
            || stored.record.executor_id != executor_id
            || registry
                .value
                .histories
                .get(&domain.id)
                .and_then(|h| h.active_generation_id.as_ref())
                != Some(&generation.generation_id)
            || registry.value.transitions.values().any(|entry| {
                entry.plan.request.next.compiled.ir.domain.id == domain.id
                    && entry.record.phase != Phase::Committed
            })
        {
            return Err(invalid(
                "inventory publication requires the latest committed generation",
            ));
        }
        let claims: Vec<_> = registry
            .value
            .claims
            .iter()
            .filter(|claim| claim.domain_id == domain.id)
            .collect();
        let reserved: std::collections::BTreeSet<_> = stored
            .plan
            .reservations
            .iter()
            .map(|reservation| &reservation.identity)
            .collect();
        if claims.len() != generation.compiled.ir.resources.len()
            || claims.iter().any(|claim| {
                !reserved.contains(&claim.identity)
                    || claim.state != ClaimState::Active
                    || claim.generation_id != generation.generation_id
                    || claim.transition_id != transition_id
            })
        {
            return Err(invalid(
                "inventory publication requires complete active claims",
            ));
        }
        let (scope, snapshot) = generation_inventory(generation, &registry.value.claims)?;
        deploy::validate_inventory_checkpoint(
            &scope,
            &snapshot,
            &stored.plan.generation_reservation,
        )?;
        let previous = store.load(&scope).await?;
        if previous
            .as_ref()
            .is_none_or(|head| head.snapshot.generation != snapshot.generation)
        {
            deploy::validate_transition_inventory(
                &stored.plan.request,
                previous.as_ref().map(|head| &head.snapshot),
            )?;
        }
        validate_predecessor(
            stored
                .plan
                .request
                .predecessor()?
                .map(|generation| generation.allocation.generation_id.as_str()),
            &snapshot,
            previous.as_ref(),
        )?;
        self.recheck_inventory_registry(&registry).await?;
        let published = store
            .publish(
                &scope,
                previous.as_ref().map(|head| head.revision.as_str()),
                &snapshot,
            )
            .await?;
        // The database and Kubernetes do not share a transaction. A changed
        // registry means the caller must reload; it must not report completion.
        self.recheck_inventory_registry(&registry).await?;
        Ok(published)
    }

    pub(crate) async fn recheck_inventory_registry(
        &self,
        expected: &Versioned<NamespaceRegistry>,
    ) -> Result<()> {
        let current = self.load_registry().await?;
        if current.uid != expected.uid || current.resource_version != expected.resource_version {
            return Err(invalid(
                "registry changed during inventory publication; reload before retry",
            ));
        }
        Ok(())
    }
}

fn validate_predecessor(
    previous_generation: Option<&str>,
    snapshot: &InventorySnapshot,
    head: Option<&VersionedInventory>,
) -> Result<()> {
    match head {
        None if previous_generation.is_some() => Err(invalid(
            "upgrade inventory is missing; explicit recovery is required",
        )),
        Some(previous) if previous.snapshot.generation == snapshot.generation => {
            let next_by_identity: std::collections::BTreeMap<_, _> = snapshot
                .resources
                .iter()
                .map(|resource| (&resource.identity, resource))
                .collect();
            let mut completed = previous.snapshot.clone();
            for resource in &mut completed.resources {
                let next = next_by_identity
                    .get(&resource.identity)
                    .ok_or_else(|| invalid("committed generation inventory lost a resource"))?;
                if resource.uid.is_none() {
                    resource.uid.clone_from(&next.uid);
                }
            }
            if &completed != snapshot {
                return Err(invalid(
                    "committed generation inventory metadata or recorded UID changed",
                ));
            }
            Ok(())
        }
        Some(previous) if previous_generation != Some(previous.snapshot.generation.as_str()) => {
            Err(invalid(
                "inventory head is not the preceding committed generation",
            ))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_upgrade_inventory_requires_recovery_but_initial_install_can_start() {
        let next = InventorySnapshot {
            schema_version: inventory::INVENTORY_SCHEMA.into(),
            generation: "generation-b".into(),
            resources: vec![],
        };
        assert!(validate_predecessor(Some("generation-a"), &next, None).is_err());
        assert!(validate_predecessor(None, &next, None).is_ok());
        let prior = InventorySnapshot {
            generation: "generation-a".into(),
            ..next.clone()
        };
        let head = VersionedInventory {
            revision: "7".into(),
            snapshot_id: compiler_core::digest(&prior).unwrap(),
            snapshot: prior,
        };
        assert!(validate_predecessor(Some("generation-a"), &next, Some(&head)).is_ok());
        assert!(validate_predecessor(None, &next, Some(&head)).is_err());
    }

    #[test]
    fn same_generation_can_complete_an_unknown_uid_only()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let old = InventorySnapshot {
            schema_version: inventory::INVENTORY_SCHEMA.into(),
            generation: "g1".into(),
            resources: vec![inventory::TrackedResource {
                identity: inventory::ResourceKey {
                    api_group: String::new(),
                    resource: "configmaps".into(),
                    namespace: "workload".into(),
                    name: "config".into(),
                },
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                uid: None,
                deletion_policy: compiler_core::DeletionPolicy::Orphan,
            }],
        };
        let head = VersionedInventory {
            revision: "1".into(),
            snapshot_id: compiler_core::digest(&old)?,
            snapshot: old.clone(),
        };
        let mut current = old;
        current
            .resources
            .first_mut()
            .ok_or("missing fixture resource")?
            .uid = Some("observed-uid".into());
        validate_predecessor(None, &current, Some(&head))?;
        current
            .resources
            .first_mut()
            .ok_or("missing fixture resource")?
            .deletion_policy = compiler_core::DeletionPolicy::Delete;
        assert!(validate_predecessor(None, &current, Some(&head)).is_err());
        Ok(())
    }
}
