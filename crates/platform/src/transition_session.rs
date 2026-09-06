//! Release-authorized replay lives only in memory, outside registry state.
use crate::{
    NamespaceRegistry, PlatformClient, Result, StoredTransition, TransitionInputs, Versioned,
    invalid,
};
use deploy::{
    DeploymentRequest, GenerationPlan, PreviousGeneration, RegistrySnapshot, TransitionPlan,
};
use inventory::InventoryStore;
use std::{ops::Deref, sync::Arc};

#[derive(Clone)]
pub(crate) struct Selection {
    inputs: TransitionInputs,
    plan: Arc<TransitionPlan>,
    previous_writer_checkpoint: Option<crate::WriterCheckpoint>,
    trust_policy_digest: Option<String>,
    capacity: crate::CapacityAdmission,
}

/// A selected release capability. Every lifecycle reload revalidates its frozen inputs.
pub struct TransitionSession {
    client: PlatformClient,
    transition_id: String,
}
impl Deref for TransitionSession {
    type Target = PlatformClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

/// Read-only review evidence, bound to an exact registry UID and resource version.
/// This value never authorizes a mutation; session operations load fresh state.
pub struct ResolvedTransition {
    registry: Versioned<NamespaceRegistry>,
    selected: ResolvedStoredTransition,
}
impl ResolvedTransition {
    #[must_use]
    pub fn registry(&self) -> &Versioned<NamespaceRegistry> {
        &self.registry
    }
    #[must_use]
    pub fn plan(&self) -> &TransitionPlan {
        &self.selected.plan
    }
    #[must_use]
    pub fn stored(&self) -> &StoredTransition {
        &self.selected.stored
    }
    #[must_use]
    pub fn transition_id(&self) -> &str {
        &self.selected.stored.allocation().transition_id
    }
}
impl TransitionSession {
    /// Load and resolve review evidence from the authenticated protected registry.
    /// # Errors
    /// Rejects another registry incarnation, modified inputs or invalid record replay.
    pub async fn resolve(&self) -> Result<ResolvedTransition> {
        let registry = self.client.load_registry().await?;
        let selected = self.client.selected(&registry, &self.transition_id)?;
        Ok(ResolvedTransition { registry, selected })
    }
}

#[derive(Clone)]
pub(crate) struct ResolvedStoredTransition {
    pub plan: Arc<TransitionPlan>,
    stored: StoredTransition,
}
impl Deref for ResolvedStoredTransition {
    type Target = StoredTransition;
    fn deref(&self) -> &Self::Target {
        &self.stored
    }
}

impl PlatformClient {
    pub(crate) fn selecting_capacity(&self, capacity: crate::CapacityAdmission) -> Result<Self> {
        let mut selection = self.selection.as_deref().cloned().ok_or_else(|| {
            invalid("capacity resize requires an authenticated transition session")
        })?;
        selection.capacity = capacity;
        let mut client = self.clone();
        client.selection = Some(Arc::new(selection));
        Ok(client)
    }

    /// Reconstruct one transition from a freshly verified release and immutable metadata.
    /// # Errors
    /// Rejects unavailable snapshots, policy drift, changed release or nonexact replay.
    pub async fn transition_session(
        &self,
        release: &release::VerifiedRelease,
        inventory: &dyn InventoryStore,
        transition: &str,
    ) -> Result<TransitionSession> {
        self.bind_release(
            release.compiled(),
            release.release_digest(),
            release.trust_policy_digest(),
            inventory,
            transition,
        )
        .await
    }

    pub(crate) async fn bind_release(
        &self,
        compiled: &compiler_core::CompiledPlan,
        artifact_digest: &str,
        trust_policy_digest: &str,
        inventory: &dyn InventoryStore,
        transition: &str,
    ) -> Result<TransitionSession> {
        let mut client = self.clone();
        client.selection = None;
        let registry = client.load_registry().await?;
        let stored = registry.value.transition(transition)?;
        let inputs = &stored.inputs;
        if inputs.allocation.artifact_digest != artifact_digest
            || inputs.allocation.ir_digest != compiled.ir_digest
        {
            return Err(invalid(
                "selected release differs from protected allocation",
            ));
        }
        client
            .verify_release_trust(stored.domain_id(), trust_policy_digest)
            .await?;
        let policy = client.load_policy(stored.domain_id()).await?;
        if compiler_core::digest(&policy.value)? != inputs.policy_digest {
            return Err(invalid(
                "platform policy changed since transition reservation",
            ));
        }
        let claims = client
            .claims_backend()?
            .read_claims(&inputs.registry.claim_snapshot)
            .await?
            .ok_or_else(|| invalid("frozen planning claims missing; explicit recovery required"))?;
        claims.validate()?;
        if claims.reference()? != inputs.registry.claim_snapshot {
            return Err(invalid(
                "frozen planning claim snapshot commitment mismatch",
            ));
        }
        let previous = read_predecessor(inventory, inputs.previous.as_ref()).await?;
        let next = GenerationPlan {
            generation_id: inputs.allocation.generation_id.clone(),
            artifact_digest: artifact_digest.into(),
            compiled: compiled.clone(),
        };
        let request = DeploymentRequest {
            transition_id: transition.into(),
            previous,
            ownership_intents: deploy::restore_ownership_intents(
                &next,
                transition,
                &inputs.ownership_intents,
            )?,
            next,
            runtime_ceiling: policy.value.runtime_ceiling,
            cleanup_ceiling: policy.value.cleanup_ceiling,
            registry: RegistrySnapshot {
                registry_uid: registry.uid.clone(),
                next_sequence: inputs.registry.next_sequence,
                revision: inputs.registry.revision.clone(),
                claims: claims.claims,
                generation_history: inputs.registry.generation_history.clone(),
            },
            observations: vec![],
        };
        let plan = deploy::plan_transition(&request)?;
        if plan.plan_digest != inputs.plan_digest
            || plan.generation_reservation != inputs.allocation
        {
            return Err(invalid(
                "rehydrated transition differs from protected plan commitment",
            ));
        }
        client.selection = Some(Arc::new(Selection {
            inputs: inputs.clone(),
            plan: Arc::new(plan),
            previous_writer_checkpoint: stored.previous_writer_checkpoint.clone(),
            trust_policy_digest: Some(trust_policy_digest.into()),
            capacity: stored.capacity.clone(),
        }));
        client.selected(&registry, transition)?;
        Ok(TransitionSession {
            client,
            transition_id: transition.into(),
        })
    }

    pub(crate) fn selected(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
    ) -> Result<ResolvedStoredTransition> {
        let selection = self.selection.as_ref().ok_or_else(|| {
            invalid("lifecycle requires a freshly verified release-bound transition session")
        })?;
        let stored = registry.value.transition(transition)?;
        if registry.uid != selection.inputs.allocation.registry_uid
            || registry.resource_version.is_empty()
            || stored.inputs != selection.inputs
            || stored.previous_writer_checkpoint != selection.previous_writer_checkpoint
            || stored.capacity != selection.capacity
            || transition != selection.plan.request.transition_id
            || stored.allocation() != &selection.plan.generation_reservation
            || stored.plan_digest() != selection.plan.plan_digest
        {
            return Err(invalid(
                "selected transition authority changed; resolve release again",
            ));
        }
        deploy::verify_record(&selection.plan, &stored.record)?;
        crate::completion_envelope::validate_selected(&registry.value, &selection.plan)?;
        match (
            selection.plan.request.predecessor()?,
            &stored.previous_writer_checkpoint,
        ) {
            (Some(previous), Some(checkpoint)) => checkpoint.validate(
                previous,
                &self.control_namespace,
                &self.workload_namespace,
                &self.cluster_binding,
            )?,
            (None, None) => (),
            _ => {
                return Err(invalid(
                    "selected predecessor writer checkpoint is missing or unexpected",
                ));
            }
        }
        Ok(ResolvedStoredTransition {
            plan: selection.plan.clone(),
            stored: stored.clone(),
        })
    }

    pub(crate) async fn validate_selection(
        &self,
        registry: &Versioned<NamespaceRegistry>,
    ) -> Result<()> {
        if let Some(selection) = &self.selection {
            if let Some(trust) = &selection.trust_policy_digest {
                self.verify_release_trust(
                    &selection.inputs.registry.generation_history.domain_id,
                    trust,
                )
                .await?;
            }
            self.selected(registry, &selection.plan.request.transition_id)?;
            let policy = self
                .load_policy(&selection.inputs.registry.generation_history.domain_id)
                .await?;
            if compiler_core::digest(&policy.value)? != selection.inputs.policy_digest {
                return Err(invalid("selected transition platform policy changed"));
            }
        }
        Ok(())
    }

    pub(crate) async fn verify_release_trust(&self, domain: &str, expected: &str) -> Result<()> {
        let trust = self.load_trust_policy(domain).await?;
        if !crate::valid_digest(expected) || compiler_core::digest(&trust.value)? != expected {
            return Err(invalid(
                "release verification trust policy is no longer current",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_test_trust(mut self, digest: &str) -> Result<Self> {
        let selection = self
            .selection
            .as_mut()
            .ok_or_else(|| invalid("test selection missing"))?;
        Arc::make_mut(selection).trust_policy_digest = Some(digest.into());
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn bind_test_plan(
        &self,
        plan: &TransitionPlan,
        previous: Option<&crate::WriterCheckpoint>,
        capacity: &crate::CapacityAdmission,
    ) -> Result<Self> {
        let policy = crate::PolicyState {
            schema_version: crate::SCHEMA_VERSION.into(),
            runtime_ceiling: plan.request.runtime_ceiling.clone(),
            cleanup_ceiling: plan.request.cleanup_ceiling.clone(),
        };
        let claims = coordination::ClaimSnapshotV1 {
            schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
            scope: coordination::ClaimScope {
                registry_uid: plan.request.registry.registry_uid.clone(),
                cluster_binding: self.cluster_binding.clone(),
                control_namespace: self.control_namespace.clone(),
                workload_namespace: self.workload_namespace.clone(),
            },
            claims: plan.request.registry.claims.clone(),
        };
        let inputs = TransitionInputs::freeze(plan, claims.reference()?, &policy)?;
        let mut client = self.clone();
        client.selection = Some(Arc::new(Selection {
            inputs,
            plan: Arc::new(plan.clone()),
            previous_writer_checkpoint: previous.cloned(),
            trust_policy_digest: None,
            capacity: capacity.clone(),
        }));
        Ok(client)
    }
}

async fn read_predecessor(
    inventory: &dyn InventoryStore,
    previous: Option<&crate::FrozenPredecessor>,
) -> Result<Option<PreviousGeneration>> {
    Ok(match previous {
        None => None,
        Some(previous) => {
            let snapshot = inventory
                .read_snapshot(&previous.scope, &previous.snapshot_id)
                .await?
                .ok_or_else(|| {
                    invalid("frozen predecessor inventory missing; explicit recovery required")
                })?;
            if inventory::snapshot_id(&previous.scope, &snapshot)? != previous.snapshot_id {
                return Err(invalid("frozen predecessor inventory commitment mismatch"));
            }
            Some(PreviousGeneration::Metadata(Box::new(
                deploy::PredecessorGeneration {
                    allocation: previous.allocation.clone(),
                    scope: previous.scope.clone(),
                    control_namespace: previous.control_namespace.clone(),
                    inventory: snapshot,
                    retirement_role_binding_namespaces: previous
                        .retirement_role_binding_namespaces
                        .clone(),
                },
            )))
        }
    })
}
