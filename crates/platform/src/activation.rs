use crate::controller::{approved_claims, stable_data};
use crate::store::{read_state, replace_state};
use crate::{
    ActivationState, ActivationStatus, NamespaceRegistry, PlatformClient, Result, Versioned,
    invalid,
};
use compiler_core::digest;
use deploy::{ClaimState, Event, Phase, Quiescence, advance};
use kube::{
    Api,
    api::{ApiResource, DynamicObject, PostParams},
    core::GroupVersionKind,
};
use serde_json::{Value, json};

impl PlatformClient {
    /// Activate only a staged initial installation. The method resumes from
    /// durable CAS states under the same executor; it never treats a lease,
    /// process observation or receipt hash as proof of old request termination.
    /// # Errors
    /// Any previous generation, changed authority or ambiguous object blocks.
    pub async fn activate_initial(
        &self,
        transition_id: &str,
        executor_id: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let mut registry = self.load_registry().await?;
        let stored = registry
            .value
            .transitions
            .get(transition_id)
            .cloned()
            .ok_or_else(|| invalid("unknown transition"))?;
        if stored.record.executor_id != executor_id
            || stored.plan.request.previous.is_some()
            || !stored.plan.cleanup.is_empty()
            || !matches!(
                stored.record.phase,
                Phase::Staged
                    | Phase::Quiesced
                    | Phase::Activated
                    | Phase::Cleaning
                    | Phase::Committed
            )
        {
            return Err(invalid(
                "initial activation requires the owning staged initial transition",
            ));
        }
        let binding = stored
            .runtime_binding
            .clone()
            .ok_or_else(|| invalid("controller not staged"))?;
        let trust = self.load_trust_policy(&binding.domain_id).await?;
        if stored.staged_trust_policy_digest.as_ref() != Some(&digest(&trust.value)?) {
            return Err(invalid("artifact trust policy changed before activation"));
        }
        let policy = self.load_policy(&binding.domain_id).await?;
        if digest(&policy.value)? != binding.policy_digest {
            return Err(invalid("platform policy changed before activation"));
        }
        if stored.record.phase == Phase::Committed {
            return Ok(registry);
        }
        self.verify_staged_proxy(&stored).await?;
        self.verify_staged_controller(&registry, transition_id)
            .await?;
        if stored.record.phase == Phase::Staged {
            registry = self
                .transition_event(
                    &registry,
                    transition_id,
                    executor_id,
                    Event::Quiesced {
                        event_id: format!("initial-quiesce-{}", stored.record.verification_epoch),
                        evidence: Quiescence::InitialInstallation {
                            verification_epoch: stored.record.verification_epoch,
                        },
                    },
                )
                .await?;
        }
        if registry.value.transition(transition_id)?.record.phase == Phase::Quiesced {
            registry = self
                .publish_initial_activation(&registry, transition_id, executor_id, &binding)
                .await?;
        }

        if registry.value.transition(transition_id)?.record.phase == Phase::Activated {
            self.start_initial_controller(&registry, transition_id)
                .await?;
            registry = self
                .transition_event(
                    &registry,
                    transition_id,
                    executor_id,
                    Event::Cleaning {
                        event_id: "initial-cleaning".into(),
                    },
                )
                .await?;
        }
        if registry.value.transition(transition_id)?.record.phase == Phase::Cleaning {
            let no_previous = digest(&(
                "initial-installation-no-obsolete-identity",
                &registry.value.transition(transition_id)?.plan.plan_digest,
            ))?;
            registry = self
                .transition_event(
                    &registry,
                    transition_id,
                    executor_id,
                    Event::Committed {
                        event_id: "initial-committed".into(),
                        cleanup_receipts: vec![],
                        obsolete_identity_retirement_digest: no_previous,
                    },
                )
                .await?;
        }
        Ok(registry)
    }

    async fn publish_initial_activation(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition_id: &str,
        executor_id: &str,
        binding: &crate::RuntimeBinding,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let registry = self
            .activate_initial_claims(registry, transition_id)
            .await?;
        let name = format!("reakt-activation-{}", binding.domain_id);
        let activation: Versioned<ActivationState> =
            read_state(&self.client, &self.control_namespace, &name).await?;
        let entry = registry.value.transition(transition_id)?;
        let key = format!("v1/ConfigMap/{}/{name}", self.control_namespace);
        let claims = approved_claims(&registry.value, entry)?;
        if entry.staged_identity_uids.get(&key) != Some(&activation.uid)
            || &activation.value.binding != binding
            || activation.value.approved_claims != claims
            || !matches!(
                activation.value.status,
                ActivationStatus::Inactive | ActivationStatus::Active
            )
        {
            return Err(invalid("staged activation UID, binding or claims changed"));
        }
        let active = if activation.value.status == ActivationStatus::Active {
            activation
        } else {
            let mut next = activation.value.clone();
            next.status = ActivationStatus::Active;
            replace_state(
                &self.client,
                &self.control_namespace,
                &name,
                &activation,
                &next,
            )
            .await?
        };
        let epoch = registry
            .value
            .transition(transition_id)?
            .record
            .verification_epoch;
        self.transition_event(
            &registry,
            transition_id,
            executor_id,
            Event::Activated {
                event_id: format!("activate-{epoch}"),
                generation_id: binding.generation_id.clone(),
                activation_receipt_digest: digest(&(
                    active.uid,
                    active.resource_version,
                    active.value,
                ))?,
                verification_epoch: epoch,
            },
        )
        .await
    }

    async fn activate_initial_claims(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition_id: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let entry = registry.value.transition(transition_id)?;
        let claims = approved_claims(&registry.value, entry)?;
        let domain = &entry.plan.request.next.compiled.ir.domain.id;
        let generation = &entry.plan.request.next.generation_id;
        let history = registry
            .value
            .histories
            .get(domain)
            .ok_or_else(|| invalid("domain history missing"))?;
        if history
            .active_generation_id
            .as_ref()
            .is_some_and(|active| active != generation)
        {
            return Err(invalid(
                "an old generation is active; initial activation forbidden",
            ));
        }
        let mut next = registry.value.clone();
        for approved in claims {
            let claim = next
                .claims
                .iter_mut()
                .find(|c| c.identity == approved.claim.identity)
                .ok_or_else(|| invalid("claim vanished"))?;
            claim.state = ClaimState::Active;
        }
        next.histories
            .get_mut(domain)
            .ok_or_else(|| invalid("history vanished"))?
            .active_generation_id = Some(generation.clone());
        self.persist_registry(registry, &next).await
    }

    pub(crate) async fn transition_event(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition_id: &str,
        executor_id: &str,
        event: Event,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let mut next = registry.value.clone();
        let entry = next
            .transitions
            .get_mut(transition_id)
            .ok_or_else(|| invalid("unknown transition"))?;
        entry.record = advance(
            &entry.plan,
            &entry.record,
            entry.record.revision,
            executor_id,
            event,
        )?;
        self.persist_registry(registry, &next).await
    }

    pub(crate) async fn verify_staged_controller(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition_id: &str,
    ) -> Result<DynamicObject> {
        let entry = registry.value.transition(transition_id)?;
        let binding = entry
            .runtime_binding
            .as_ref()
            .ok_or_else(|| invalid("controller binding absent"))?;
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("apps", "v1", "Deployment"));
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.control_namespace, &ar);
        let object = api.get(&binding.service_account_name).await?;
        let key = format!(
            "apps/v1/Deployment/{}/{}",
            self.control_namespace, binding.service_account_name
        );
        let mut normalized = stable_data(&object);
        let replicas = normalized
            .pointer("/spec/replicas")
            .and_then(serde_json::Value::as_u64);
        if !matches!(replicas, Some(0 | 1))
            || (replicas == Some(1)
                && matches!(entry.record.phase, Phase::Staged | Phase::Quiesced))
        {
            return Err(invalid("staged controller has unexpected replicas"));
        }
        normalized["spec"]["replicas"] = json!(0);
        if entry.staged_identity_uids.get(&key) != object.metadata.uid.as_ref()
            || object.metadata.uid.is_none()
            || object.metadata.deletion_timestamp.is_some()
            || entry.staged_content_digests.get(&key) != Some(&digest(&normalized)?)
        {
            return Err(invalid("staged controller UID or content changed"));
        }
        Ok(object)
    }

    pub(crate) async fn start_initial_controller(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition_id: &str,
    ) -> Result<()> {
        let mut object = self
            .verify_staged_controller(registry, transition_id)
            .await?;
        let entry = registry.value.transition(transition_id)?;
        self.verify_runtime_identity(entry).await?;
        self.verify_controller_support(entry).await?;
        let binding = entry
            .runtime_binding
            .as_ref()
            .ok_or_else(|| invalid("controller binding missing"))?;
        let authority = crate::RuntimeAuthority::new(
            self.client.clone(),
            &self.control_namespace,
            binding.clone(),
        )?;
        authority.verify(&entry.plan.request.next.compiled).await?;
        if object.data.pointer("/spec/replicas") == Some(&json!(1)) {
            return Ok(());
        }
        object
            .data
            .get_mut("spec")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid("controller Deployment has no spec"))?
            .insert("replicas".into(), json!(1));
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("apps", "v1", "Deployment"));
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.control_namespace, &ar);
        let result = api
            .replace(
                &binding.service_account_name,
                &PostParams::default(),
                &object,
            )
            .await?;
        if result.metadata.uid != object.metadata.uid {
            return Err(invalid("controller UID changed on activation"));
        }
        Ok(())
    }
}
