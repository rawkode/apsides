use crate::{
    ActivationState, ActivationStatus, CapacityAdmission, EvidenceProfile, NamespaceRegistry,
    PlatformClient, Result, Versioned, invalid,
};
use deploy::Phase;

impl PlatformClient {
    /// Enlarge an untouched upgrade's capacity after bounded topology drift.
    /// The caller must construct a fresh release session after this CAS succeeds.
    /// # Errors
    /// Rejects shrinking, quiescence already started, changed authority or insufficient
    /// global capacity. No old-writer mutation or automatic cancellation is performed.
    pub async fn resize_pre_quiescence_capacity(
        &self,
        transition: &str,
        executor: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let registry = self.load_registry().await?;
        let stored = self.selected(&registry, transition)?;
        if stored.record.executor_id != executor
            || !matches!(stored.record.phase, Phase::Planned | Phase::Staged)
            || stored.upgrade_drain.is_some()
        {
            return Err(invalid(
                "capacity resize requires the owning untouched Planned or Staged upgrade",
            ));
        }
        let previous = self.previous_writer(&stored)?;
        self.require_active_predecessor(previous).await?;
        let observed = self.select_evidence_profile(Some(previous)).await?;
        let profile = expanded_profile(&stored.capacity.profile, &observed);
        profile.validate()?;
        let mut capacity =
            crate::completion_envelope::calculate(&registry.value, &stored.plan, &profile)?;
        capacity.domain_bytes = capacity.domain_bytes.max(stored.capacity.domain_bytes);
        capacity.activation_bytes = capacity
            .activation_bytes
            .max(stored.capacity.activation_bytes);
        capacity.inventory_bytes = capacity
            .inventory_bytes
            .max(stored.capacity.inventory_bytes);
        if capacity.profile == stored.capacity.profile
            && capacity.domain_bytes == stored.capacity.domain_bytes
            && capacity.activation_bytes == stored.capacity.activation_bytes
            && capacity.inventory_bytes == stored.capacity.inventory_bytes
        {
            return Ok(registry);
        }
        capacity.revision =
            stored.capacity.revision.checked_add(1).ok_or_else(|| {
                invalid("capacity revision exhausted; explicit recovery required")
            })?;
        require_expansion(&stored.capacity, &capacity)?;
        let mut next = registry.value.clone();
        next.transitions
            .get_mut(transition)
            .ok_or_else(|| invalid("capacity transition disappeared"))?
            .capacity = capacity.clone();
        let reference = next.claim_snapshot()?.reference()?;
        crate::state_capacity::validate_domain_candidate(
            &next.state(&reference)?,
            stored.domain_id(),
            usize::try_from(capacity.domain_bytes)
                .map_err(|_| invalid("capacity byte ceiling overflow"))?,
        )?;
        self.require_active_predecessor(previous).await?;
        self.selecting_capacity(capacity)?
            .persist_registry(&registry, &next)
            .await
    }

    pub(crate) async fn require_active_predecessor(
        &self,
        previous: &crate::WriterCheckpoint,
    ) -> Result<()> {
        let activation: Versioned<ActivationState> = crate::store::read_state(
            &self.client,
            &self.control_namespace,
            &format!("reakt-activation-{}", previous.runtime_binding.domain_id),
        )
        .await?;
        if activation.uid != previous.activation_uid
            || activation.value.binding != previous.runtime_binding
            || activation.value.status != ActivationStatus::Active
        {
            return Err(invalid(
                "untouched predecessor requires its exact Active activation",
            ));
        }
        Ok(())
    }
}

fn expanded_profile(old: &EvidenceProfile, observed: &EvidenceProfile) -> EvidenceProfile {
    EvidenceProfile {
        schema_version: old.schema_version.clone(),
        max_controller_pods: old.max_controller_pods.max(observed.max_controller_pods),
        max_regular_containers: old
            .max_regular_containers
            .max(observed.max_regular_containers),
        max_init_containers: old.max_init_containers.max(observed.max_init_containers),
        max_ephemeral_containers: old
            .max_ephemeral_containers
            .max(observed.max_ephemeral_containers),
        max_uid_json_bytes: old.max_uid_json_bytes.max(observed.max_uid_json_bytes),
        max_resource_version_json_bytes: old
            .max_resource_version_json_bytes
            .max(observed.max_resource_version_json_bytes),
        max_container_id_json_bytes: old
            .max_container_id_json_bytes
            .max(observed.max_container_id_json_bytes),
        max_drain_bytes: old.max_drain_bytes.max(observed.max_drain_bytes),
    }
}

fn require_expansion(old: &CapacityAdmission, next: &CapacityAdmission) -> Result<()> {
    next.validate()?;
    let revision = old
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("capacity revision exhausted; explicit recovery required"))?;
    if next.revision != revision
        || !next.profile.contains(&old.profile)
        || next.domain_bytes < old.domain_bytes
        || next.activation_bytes < old.activation_bytes
        || next.inventory_bytes < old.inventory_bytes
    {
        return Err(invalid(
            "capacity resize cannot shrink protected obligations",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expanded_profile, require_expansion};
    use crate::{CapacityAdmission, Result};

    fn admission() -> CapacityAdmission {
        CapacityAdmission {
            schema_version: crate::state_capacity::CAPACITY_SCHEMA.into(),
            revision: 4,
            profile: crate::completion_envelope::initial_profile(),
            domain_bytes: 300_000,
            activation_bytes: 20_000,
            inventory_bytes: 200_000,
        }
    }

    #[test]
    fn expansion_preserves_unobserved_old_allowances() -> Result<()> {
        let mut old = admission();
        old.profile.max_controller_pods = 4;
        old.profile.max_regular_containers = 4;
        old.profile.max_init_containers = 2;
        let mut observed = crate::completion_envelope::initial_profile();
        observed.max_ephemeral_containers = 1;
        observed.max_uid_json_bytes = 512;
        let combined = expanded_profile(&old.profile, &observed);
        combined.validate()?;
        assert!(combined.contains(&old.profile));
        assert!(combined.contains(&observed));
        assert_eq!(combined.max_controller_pods, 4);
        assert_eq!(combined.max_init_containers, 2);
        assert_eq!(combined.max_ephemeral_containers, 1);
        Ok(())
    }

    #[test]
    fn resize_rejects_each_shrinking_ceiling_and_profile() -> Result<()> {
        let old = admission();
        let mut next = old.clone();
        next.revision += 1;
        require_expansion(&old, &next)?;
        for field in 0..3 {
            let mut shrinking = next.clone();
            match field {
                0 => shrinking.domain_bytes -= 1,
                1 => shrinking.activation_bytes -= 1,
                _ => shrinking.inventory_bytes -= 1,
            }
            assert!(require_expansion(&old, &shrinking).is_err());
        }
        let mut wider = old.clone();
        wider.profile.max_uid_json_bytes = 512;
        assert!(require_expansion(&wider, &next).is_err());
        Ok(())
    }

    #[test]
    fn resize_revision_is_exact_and_cannot_wrap() -> Result<()> {
        let old = admission();
        let mut next = old.clone();
        assert!(require_expansion(&old, &next).is_err());
        next.revision = old.revision + 2;
        assert!(require_expansion(&old, &next).is_err());
        next.revision = old.revision + 1;
        require_expansion(&old, &next)?;
        let mut exhausted = old;
        exhausted.revision = u64::MAX;
        next.revision = 0;
        assert!(require_expansion(&exhausted, &next).is_err());
        Ok(())
    }
}
