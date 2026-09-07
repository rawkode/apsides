//! Immutable replay inputs. Release manifests and desired values are never persisted here.
use crate::{Result, invalid};
use coordination::ClaimRef;
use deploy::{GenerationAllocation, GenerationHistory};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenRegistrySnapshot {
    pub claim_snapshot: ClaimRef,
    pub revision: String,
    #[serde(with = "deploy::allocation_counter")]
    pub next_sequence: u64,
    pub generation_history: GenerationHistory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenPredecessor {
    pub allocation: GenerationAllocation,
    pub scope: inventory::InventoryScope,
    pub control_namespace: String,
    pub snapshot_id: String,
    pub retirement_role_binding_namespaces: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransitionInputs {
    pub schema_version: String,
    pub registry: FrozenRegistrySnapshot,
    pub previous: Option<FrozenPredecessor>,
    pub allocation: GenerationAllocation,
    pub plan_digest: String,
    pub policy_digest: String,
    pub ownership_intents: Vec<deploy::FrozenOwnershipIntent>,
}

impl TransitionInputs {
    /// Freeze only metadata after the caller has authenticated the immutable stores.
    /// # Errors
    /// Rejects observations and noncanonical plans, or mismatched claim commitments.
    pub fn freeze(
        plan: &deploy::TransitionPlan,
        claims: ClaimRef,
        policy: &crate::PolicyState,
    ) -> Result<Self> {
        policy.validate_image_pull_secrets()?;
        if !plan.request.observations.is_empty()
            || deploy::plan_transition(&plan.request)? != *plan
            || policy.runtime_ceiling != plan.request.runtime_ceiling
            || policy.cleanup_ceiling != plan.request.cleanup_ceiling
        {
            return Err(invalid(
                "compact transition requires a canonical plan with no observations",
            ));
        }
        let snapshot = coordination::ClaimSnapshotV1 {
            schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
            scope: claims.scope.clone(),
            claims: plan.request.registry.claims.clone(),
        };
        if snapshot.reference()? != claims
            || claims.scope.registry_uid != plan.request.registry.registry_uid
        {
            return Err(invalid(
                "frozen claim reference differs from original planning inputs",
            ));
        }
        let previous = plan
            .request
            .predecessor()
            .map(|previous| {
                Ok::<_, crate::Error>(FrozenPredecessor {
                    allocation: previous.allocation.clone(),
                    scope: previous.scope.clone(),
                    control_namespace: previous.control_namespace.clone(),
                    snapshot_id: inventory::snapshot_id(&previous.scope, &previous.inventory)?,
                    retirement_role_binding_namespaces: previous
                        .retirement_role_binding_namespaces
                        .clone(),
                })
            })
            .transpose()?;
        Ok(Self {
            schema_version: "reakt.dev/transition-inputs/v1".into(),
            registry: FrozenRegistrySnapshot {
                claim_snapshot: claims,
                revision: plan.request.registry.revision.clone(),
                next_sequence: plan.request.registry.next_sequence,
                generation_history: plan.request.registry.generation_history.clone(),
            },
            previous,
            allocation: plan.generation_reservation.clone(),
            plan_digest: plan.plan_digest.clone(),
            policy_digest: compiler_core::digest(policy)?,
            ownership_intents: deploy::freeze_ownership_intents(&plan.request)?,
        })
    }

    pub(crate) fn validate(
        &self,
        transition: &str,
        scope: &coordination::ClaimScope,
    ) -> Result<()> {
        self.registry.claim_snapshot.validate()?;
        deploy::validate_frozen_ownership_intents(&self.ownership_intents)?;
        deploy::validate_allocation(&self.allocation)?;
        let history = &self.registry.generation_history;
        if self.schema_version != "reakt.dev/transition-inputs/v1"
            || self.registry.claim_snapshot.scope != *scope
            || self.allocation.registry_uid != scope.registry_uid
            || self.allocation.transition_id != transition
            || self.registry.next_sequence != self.allocation.sequence
            || self.registry.revision.is_empty()
            || self.registry.revision.len() > 1024
            || history.revision != self.registry.revision
            || history.cluster_binding != scope.cluster_binding
            || !crate::valid_namespace(&history.domain_id)
            || !crate::valid_digest(&self.plan_digest)
            || !crate::valid_digest(&self.policy_digest)
            || history.active_allocation.as_ref() != self.previous.as_ref().map(|p| &p.allocation)
        {
            return Err(invalid("compact transition replay authority invalid"));
        }
        if let Some(previous) = &self.previous {
            deploy::validate_allocation(&previous.allocation)?;
            if previous.allocation.registry_uid != scope.registry_uid
                || previous.allocation.sequence >= self.allocation.sequence
                || previous.scope.cluster != scope.cluster_binding
                || previous.scope.namespace != scope.workload_namespace
                || previous.scope.controller != history.domain_id
                || previous.control_namespace != scope.control_namespace
                || !crate::valid_digest(&previous.snapshot_id)
            {
                return Err(invalid("compact predecessor authority invalid"));
            }
        }
        Ok(())
    }
}
