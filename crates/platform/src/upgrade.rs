use crate::{
    ActivationState, ActivationStatus, Error, NamespaceRegistry, PlatformClient, Result,
    RuntimeInventory, SCHEMA_VERSION, StoredTransition, Versioned, invalid,
};
use crate::{
    controller::approved_claims,
    store::{read_state, replace_state},
};
use compiler_core::digest;
use deploy::{ClaimState, Event, Phase, advance};
use kube::{
    Api,
    api::{ApiResource, DynamicObject},
    core::GroupVersionKind,
};

impl PlatformClient {
    /// Publish a staged generation only after this executor's live quiescence
    /// operations produced protected complete old-writer checkpoints.
    /// # Errors
    /// Rejects changed policy, incomplete retirement, replaced objects or stale CAS.
    pub async fn activate_upgrade(
        &self,
        transition: &str,
        executor: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let mut registry = self
            .ensure_previous_writer_checkpoint(transition, executor)
            .await?;
        let stored = self
            .owning_upgrade(
                &registry,
                transition,
                executor,
                &[
                    Phase::Quiesced,
                    Phase::Activated,
                    Phase::Cleaning,
                    Phase::Committed,
                ],
            )?
            .clone();
        if matches!(stored.record.phase, Phase::Cleaning | Phase::Committed) {
            return Ok(registry);
        }
        self.verify_upgrade_authority(&stored).await?;
        self.verify_staged_controller(&registry, transition).await?;
        if stored.record.phase == Phase::Quiesced {
            registry = self.transfer_upgrade_claims(&registry, transition).await?;
            registry = self
                .publish_upgrade_activation(&registry, transition, executor)
                .await?;
        }
        self.start_initial_controller(&registry, transition).await?;
        self.transition_event(
            &registry,
            transition,
            executor,
            Event::Cleaning {
                event_id: format!("cleaning-{}", stored.record.verification_epoch),
            },
        )
        .await
    }

    /// Execute cleanup and atomically release only completed plan-authorized
    /// claims. Receipts are obtained internally from real Kubernetes operations.
    /// # Errors
    /// Pending finalizers, changed authority or missing retirement blocks commit.
    pub async fn commit_upgrade(
        &self,
        transition: &str,
        executor: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let registry = self
            .ensure_previous_writer_checkpoint(transition, executor)
            .await?;
        let stored = self.owning_upgrade(
            &registry,
            transition,
            executor,
            &[Phase::Cleaning, Phase::Committed],
        )?;
        if stored.record.phase == Phase::Committed {
            return Ok(registry);
        }
        self.verify_upgrade_authority(stored).await?;
        let progress = self.execute_cleanup(transition, executor).await?;
        if !progress.pending.is_empty() {
            return Err(Error::Pending(
                "cleanup remains pending; claims retained".into(),
            ));
        }
        let fresh = self.load_registry().await?;
        if fresh.uid != registry.uid || fresh.resource_version != registry.resource_version {
            return Err(invalid(
                "registry changed during cleanup; repeat verified cleanup",
            ));
        }
        let mut next = registry.value.clone();
        let drain = stored
            .upgrade_drain
            .as_ref()
            .ok_or_else(|| invalid("retirement checkpoint missing"))?;
        let event = Event::Committed {
            event_id: format!("commit-{}", stored.record.verification_epoch),
            cleanup_receipts: progress.completed,
            obsolete_identity_retirement_digest: digest(&drain.retired_identity_uids)?,
        };
        let updated = advance(
            &stored.plan,
            &stored.record,
            stored.record.revision,
            executor,
            event,
        )?;
        for item in &stored.plan.cleanup {
            let matching: Vec<_> = next
                .claims
                .iter()
                .filter(|c| c.identity == item.identity)
                .collect();
            if !matches!(matching.as_slice(), [claim]
                if claim.transition_id == transition
                && claim.state == ClaimState::Releasing
                && claim.uid == item.expected_uid)
            {
                return Err(invalid("cleanup claim changed before release"));
            }
            next.claims.retain(|c| c.identity != item.identity);
        }
        next.transitions
            .get_mut(transition)
            .ok_or_else(|| invalid("transition missing"))?
            .record = updated;
        self.persist_registry(&registry, &next).await
    }

    async fn verify_upgrade_authority(&self, stored: &StoredTransition) -> Result<()> {
        let binding = stored
            .runtime_binding
            .as_ref()
            .ok_or_else(|| invalid("new binding missing"))?;
        let trust = self.load_trust_policy(&binding.domain_id).await?;
        let policy = self.load_policy(&binding.domain_id).await?;
        if stored.staged_trust_policy_digest.as_ref() != Some(&digest(&trust.value)?)
            || binding.policy_digest != digest(&policy.value)?
        {
            return Err(invalid("approved trust or ceiling changed"));
        }
        self.verify_staged_proxy(stored).await?;
        let previous = self.previous_writer(stored)?;
        let drain = stored
            .upgrade_drain
            .as_ref()
            .ok_or_else(|| invalid("upgrade lacks live drain checkpoint"))?;
        if drain.previous_transition_id != previous.allocation.transition_id
            || drain.runtime_termination.is_none()
            || drain.final_proxy_ledger.is_none()
            || drain.proxy_termination.is_none()
            || &drain.retired_identity_uids != crate::retirement::identities(previous)
        {
            return Err(invalid("upgrade lacks complete source evidence"));
        }
        self.verify_proxy_stop_epoch(&previous.proxy_identity, true)
            .await?;
        self.observe_writer_termination(&drain.runtime_writers)
            .await?;
        self.observe_writer_termination(&drain.proxy_writers)
            .await?;
        for key in drain.retired_identity_uids.keys() {
            let parts: Vec<_> = key.split('/').collect();
            let (group, version, kind, namespace, name) = match parts.as_slice() {
                ["v1", kind, ns, name] => ("", "v1", *kind, *ns, *name),
                [group, version, kind, ns, name] => (*group, *version, *kind, *ns, *name),
                _ => return Err(invalid("retired identity key invalid")),
            };
            let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
            let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), namespace, &ar);
            if api.get_opt(name).await?.is_some() {
                return Err(invalid("obsolete credential identity reappeared"));
            }
        }
        Ok(())
    }

    async fn transfer_upgrade_claims(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let stored = registry.value.transition(transition)?;
        let previous = stored
            .plan
            .request
            .predecessor()?
            .ok_or_else(|| invalid("previous generation missing"))?;
        let domain = &stored.plan.request.next.compiled.ir.domain.id;
        let generation = &stored.plan.request.next.generation_id;
        let mut next = registry.value.clone();
        let history = next
            .histories
            .get_mut(domain)
            .ok_or_else(|| invalid("domain history missing"))?;
        if !history.active_generation_id.as_ref().is_some_and(|active| {
            active == &previous.allocation.generation_id || active == generation
        }) {
            return Err(invalid("active generation differs from transition"));
        }
        for approved in approved_claims(&registry.value, stored)? {
            next.claims
                .iter_mut()
                .find(|c| c.identity == approved.claim.identity)
                .ok_or_else(|| invalid("reserved claim missing"))?
                .state = ClaimState::Active;
        }
        for item in &stored.plan.cleanup {
            let claim = next
                .claims
                .iter_mut()
                .find(|c| c.identity == item.identity)
                .ok_or_else(|| invalid("prior cleanup claim missing"))?;
            if claim.domain_id != *domain
                || claim.generation_id != previous.allocation.generation_id
                || claim.uid != item.expected_uid
            {
                return Err(invalid("prior cleanup claim replaced"));
            }
            claim.state = ClaimState::Releasing;
            claim.transition_id = transition.into();
        }
        history.active_generation_id = Some(generation.clone());
        self.persist_registry(registry, &next).await
    }

    async fn publish_upgrade_activation(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
        executor: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let stored = registry.value.transition(transition)?;
        let previous = self.previous_writer(stored)?;
        let binding = stored
            .runtime_binding
            .as_ref()
            .ok_or_else(|| invalid("new binding missing"))?;
        let old_binding = &previous.runtime_binding;
        let activation_name = format!("reakt-activation-{}", binding.domain_id);
        let current: Versioned<ActivationState> =
            read_state(&self.client, &self.control_namespace, &activation_name).await?;
        let key = format!("v1/ConfigMap/{}/{activation_name}", self.control_namespace);
        if previous.activation_uid != current.uid
            || !((current.value.binding == *old_binding
                && current.value.status == ActivationStatus::Draining)
                || (current.value.binding == *binding
                    && current.value.status == ActivationStatus::Active))
        {
            return Err(invalid("activation has unrelated UID or generation"));
        }
        let next = ActivationState {
            schema_version: SCHEMA_VERSION.into(),
            binding: binding.clone(),
            status: ActivationStatus::Active,
            approved_claims: approved_claims(&registry.value, stored)?,
            rollout_key_id: self.preprovisioned_key_id(stored).await?,
        };
        let active = if current.value == next {
            current
        } else {
            replace_state(
                &self.client,
                &self.control_namespace,
                &activation_name,
                &current,
                &next,
            )
            .await?
        };
        let inventory_name = format!("reakt-inventory-{}", binding.domain_id);
        let inventory: Versioned<RuntimeInventory> =
            read_state(&self.client, &self.control_namespace, &inventory_name).await?;
        let inventory_key = format!("v1/ConfigMap/{}/{inventory_name}", self.control_namespace);
        if previous.inventory_uid != inventory.uid
            || (inventory.value.binding != *old_binding && inventory.value.binding != *binding)
        {
            return Err(invalid("inventory identity changed across quiescence"));
        }
        let fresh_inventory = RuntimeInventory {
            schema_version: SCHEMA_VERSION.into(),
            binding: binding.clone(),
            activation_uid: active.uid.clone(),
            activation_resource_version: active.resource_version.clone(),
            observations: vec![],
            conditions: vec![],
            drained: false,
        };
        replace_state(
            &self.client,
            &self.control_namespace,
            &inventory_name,
            &inventory,
            &fresh_inventory,
        )
        .await?;
        let mut next_registry = registry.value.clone();
        let entry = next_registry
            .transitions
            .get_mut(transition)
            .ok_or_else(|| invalid("transition missing"))?;
        entry.staged_identity_uids.insert(key, active.uid.clone());
        entry
            .staged_identity_uids
            .insert(inventory_key, inventory.uid);
        entry.record = advance(
            &entry.plan,
            &entry.record,
            entry.record.revision,
            executor,
            Event::Activated {
                event_id: format!("activate-{}", entry.record.verification_epoch),
                generation_id: binding.generation_id.clone(),
                activation_receipt_digest: digest(&(
                    active.uid,
                    active.resource_version,
                    active.value,
                ))?,
                verification_epoch: entry.record.verification_epoch,
            },
        )?;
        self.persist_registry(registry, &next_registry).await
    }
}
