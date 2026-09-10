use crate::store::{read_state, replace_state};
use crate::{
    ActivationState, ActivationStatus, DomainKey, PolicyState, Result, RuntimeBinding,
    RuntimeInventory, SCHEMA_VERSION, VerifiedActivation, Versioned, invalid, valid_digest,
    valid_name,
};
use compiler_core::{CompiledPlan, InputMode, digest, validate, validate_ceiling};
use deploy::{ClaimState, ResourceIdentity};
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use std::collections::BTreeSet;

#[derive(Clone)]
pub struct RuntimeAuthority {
    client: Client,
    control_namespace: String,
    binding: RuntimeBinding,
}

impl RuntimeAuthority {
    /// The supplied client must use the qualified Kubernetes transport. The
    /// binding is a trusted Deployment input, not proof of bearer identity.
    /// # Errors
    /// Rejects malformed fixed control names or content identities.
    pub fn new(client: Client, control_namespace: &str, binding: RuntimeBinding) -> Result<Self> {
        validate_binding(&binding)?;
        if !valid_name(control_namespace) {
            return Err(invalid("invalid control namespace"));
        }
        Ok(Self {
            client,
            control_namespace: control_namespace.into(),
            binding,
        })
    }

    /// Read cooperative activation status, including Draining or Inactive.
    /// # Errors
    /// Rejects missing, oversized, malformed or differently bound records.
    pub async fn read_activation(&self) -> Result<Versioned<ActivationState>> {
        let state: Versioned<ActivationState> = read_state(
            &self.client,
            &self.control_namespace,
            &format!("apsides-activation-{}", self.binding.domain_id),
        )
        .await?;
        crate::control_capacity::validate_runtime_control_metadata(
            &state.uid,
            &state.resource_version,
        )?;
        if state.value.schema_version != SCHEMA_VERSION || state.value.binding != self.binding {
            return Err(invalid("activation binding differs from this controller"));
        }
        Ok(state)
    }

    /// Recheck before each mutation. This is cooperative admission, not fencing
    /// of a compromised process or already submitted requests.
    /// # Errors
    /// Rejects inactive state, forged plans, policy mismatch or incomplete claims.
    pub async fn verify(&self, plan: &CompiledPlan) -> Result<VerifiedActivation> {
        if plan.ir.test_only
            || validate(&plan.ir)? != *plan
            || plan.ir_digest != self.binding.ir_digest
            || plan.ir.domain.id != self.binding.domain_id
            || plan.ir.domain.cluster_binding != self.binding.cluster_binding
            || plan.ir.domain.control_namespace != self.control_namespace
            || digest(&plan.capabilities)? != self.binding.capability_digest
        {
            return Err(invalid("compiled plan differs from activation binding"));
        }
        let activation = self.read_activation().await?;
        if activation.value.status != ActivationStatus::Active {
            return Err(crate::Error::Inactive);
        }
        let policy: Versioned<PolicyState> = read_state(
            &self.client,
            &self.control_namespace,
            &format!("apsides-policy-{}", self.binding.domain_id),
        )
        .await?;
        if policy.value.schema_version != SCHEMA_VERSION
            || digest(&policy.value)? != self.binding.policy_digest
        {
            return Err(invalid("protected policy identity mismatch"));
        }
        validate_ceiling(plan, &policy.value.runtime_ceiling)?;
        verify_claims(plan, &activation.value)?;
        let sensitive = plan
            .ir
            .inputs
            .iter()
            .any(|input| input.mode == InputMode::Observed && input.sensitive);
        if sensitive != activation.value.rollout_key_id.is_some()
            || activation
                .value
                .rollout_key_id
                .as_ref()
                .is_some_and(|id| !valid_name(id))
        {
            return Err(invalid(
                "rollout key identity differs from sensitive input requirements",
            ));
        }
        Ok(VerifiedActivation { activation })
    }

    /// # Errors
    /// Rejects absent, corrupt, oversized or differently bound inventory.
    pub async fn read_inventory(&self) -> Result<Versioned<RuntimeInventory>> {
        let inventory: Versioned<RuntimeInventory> = read_state(
            &self.client,
            &self.control_namespace,
            &format!("apsides-inventory-{}", self.binding.domain_id),
        )
        .await?;
        crate::control_capacity::validate_runtime_control_metadata(
            &inventory.value.activation_uid,
            &inventory.value.activation_resource_version,
        )?;
        if inventory.value.schema_version != SCHEMA_VERSION
            || inventory.value.binding != self.binding
        {
            return Err(invalid("inventory generation or build binding mismatch"));
        }
        Ok(inventory)
    }

    /// Inventory is preprovisioned; runtime receives no create permission.
    /// # Errors
    /// Rejects changed object UID/RV, undeclared identities and unbounded content.
    pub async fn publish_inventory(
        &self,
        previous: &Versioned<RuntimeInventory>,
        authority: &VerifiedActivation,
        next: RuntimeInventory,
    ) -> Result<Versioned<RuntimeInventory>> {
        let current = self.read_activation().await?;
        if next.drained
            || current.value.status != ActivationStatus::Active
            || current.uid != authority.uid()
            || current.resource_version != authority.resource_version()
        {
            return Err(invalid(
                "active inventory publication requires current activation",
            ));
        }
        self.publish_checked_inventory(previous, authority, next)
            .await
    }

    /// Publish a cooperative drain acknowledgement using a freshly read,
    /// correctly bound Draining activation. It grants no workload write authority.
    /// # Errors
    /// Rejects non-draining activation, a false drain flag and inventory CAS races.
    pub async fn publish_drained_inventory(
        &self,
        previous: &Versioned<RuntimeInventory>,
        next: RuntimeInventory,
    ) -> Result<Versioned<RuntimeInventory>> {
        let activation = self.read_activation().await?;
        if !next.drained || activation.value.status != ActivationStatus::Draining {
            return Err(invalid(
                "drain acknowledgement requires current Draining activation",
            ));
        }
        self.publish_checked_inventory(previous, &VerifiedActivation { activation }, next)
            .await
    }

    async fn publish_checked_inventory(
        &self,
        previous: &Versioned<RuntimeInventory>,
        authority: &VerifiedActivation,
        next: RuntimeInventory,
    ) -> Result<Versioned<RuntimeInventory>> {
        crate::control_capacity::validate_runtime_control_metadata(
            &next.activation_uid,
            &next.activation_resource_version,
        )?;
        if previous.value.binding != self.binding
            || next.binding != self.binding
            || authority.state().binding != self.binding
            || next.schema_version != SCHEMA_VERSION
            || next.activation_uid != authority.uid()
            || next.activation_resource_version != authority.resource_version()
            || next.observations.len() > authority.state().approved_claims.len()
            || next.conditions.len() > 256
        {
            return Err(invalid(
                "inventory binding, activation or cardinality mismatch",
            ));
        }
        let approved: BTreeSet<_> = authority
            .state()
            .approved_claims
            .iter()
            .map(|c| &c.claim.identity)
            .collect();
        let mut observed = BTreeSet::new();
        for item in &next.observations {
            if !approved.contains(&item.identity)
                || !observed.insert(&item.identity)
                || item.uid.is_empty()
                || item.uid.len() > 128
            {
                return Err(invalid(
                    "inventory contains unauthorized or duplicate identity",
                ));
            }
        }
        if next.conditions.iter().any(|condition| {
            !valid_name(&condition.reason_code)
                || condition
                    .resource_name
                    .as_ref()
                    .is_some_and(|name| !valid_name(name))
        }) {
            return Err(invalid(
                "inventory condition must contain bounded identifiers only",
            ));
        }
        replace_state(
            &self.client,
            &self.control_namespace,
            &format!("apsides-inventory-{}", self.binding.domain_id),
            previous,
            &next,
        )
        .await
    }

    /// # Errors
    /// Rejects unexpected key access, identity mismatch or invalid key lengths.
    /// Errors contain no key data; the returned type has no Debug implementation.
    pub async fn load_rollout_key(&self, authority: &VerifiedActivation) -> Result<DomainKey> {
        if authority.state().binding != self.binding {
            return Err(invalid("key request binding mismatch"));
        }
        let expected = authority
            .state()
            .rollout_key_id
            .as_ref()
            .ok_or_else(|| invalid("this activation has no rollout key grant"))?;
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.control_namespace);
        // Avoid reflecting API response text containing Secret material.
        let secret = api
            .get(&format!("apsides-rollout-key-{}", self.binding.domain_id))
            .await
            .map_err(|_| invalid("protected rollout key lookup failed"))?;
        let data = secret
            .data
            .ok_or_else(|| invalid("protected rollout key is malformed"))?;
        let id = data
            .get("keyId")
            .and_then(|value| std::str::from_utf8(&value.0).ok())
            .ok_or_else(|| invalid("protected rollout key identifier is malformed"))?;
        let bytes = data
            .get("key")
            .ok_or_else(|| invalid("protected rollout key is malformed"))?;
        if id != expected || bytes.0.len() != 32 || data.len() != 2 {
            return Err(invalid("protected rollout key binding or length mismatch"));
        }
        Ok(DomainKey {
            id: id.into(),
            bytes: secrecy::SecretBox::new(bytes.0.clone().into_boxed_slice()),
        })
    }
}

pub(crate) fn validate_binding(binding: &RuntimeBinding) -> Result<()> {
    if !valid_name(&binding.domain_id)
        || !valid_name(&binding.service_account_name)
        || binding.generation_id.is_empty()
        || binding.generation_id.len() > 128
        || binding.cluster_binding.is_empty()
        || binding.cluster_binding.len() > 256
        || [
            &binding.build_digest,
            &binding.ir_digest,
            &binding.policy_digest,
            &binding.capability_digest,
        ]
        .iter()
        .any(|value| !valid_digest(value))
        || !binding
            .controller_image
            .rsplit_once('@')
            .is_some_and(|(name, hash)| !name.is_empty() && valid_digest(hash))
        || binding.controller_image.len() > 4096
        || binding.controller_image.chars().any(char::is_whitespace)
    {
        return Err(invalid(
            "invalid runtime binding or unpinned controller image",
        ));
    }
    Ok(())
}

pub(crate) fn resource_identity(resource: &compiler_core::Resource) -> Result<ResourceIdentity> {
    deploy::resource_identity(resource).map_err(|_| invalid("unsupported resource identity"))
}

fn verify_claims(plan: &CompiledPlan, activation: &ActivationState) -> Result<()> {
    if activation.approved_claims.len() != plan.ir.resources.len() {
        return Err(invalid(
            "approved claims do not cover exactly the managed resource set",
        ));
    }
    let mut seen = BTreeSet::new();
    for approved in &activation.approved_claims {
        let claim = &approved.claim;
        if !seen.insert(&claim.identity)
            || claim.domain_id != activation.binding.domain_id
            || claim.generation_id != activation.binding.generation_id
            || claim.state != ClaimState::Active
            || claim.transition_id.is_empty()
            || claim
                .uid
                .as_ref()
                .is_some_and(|uid| uid.is_empty() || uid.len() > 128)
            || (claim.uid.is_none() && !approved.allow_create)
        {
            return Err(invalid(
                "approved claim has invalid ownership or create authority",
            ));
        }
        let resource = plan
            .ir
            .resources
            .iter()
            .find(|r| resource_identity(r).is_ok_and(|identity| identity == claim.identity))
            .ok_or_else(|| invalid("approved claim names an undeclared target"))?;
        if resource.lifecycle.deletion_policy != approved.deletion_policy {
            return Err(invalid(
                "approved claim lifecycle policy differs from compiled policy",
            ));
        }
        if approved.allow_create
            && !plan.capabilities.iter().any(|c| {
                c.verb == "create"
                    && c.api_group == claim.identity.api_group
                    && c.resource == claim.identity.resource
                    && c.namespace == claim.identity.namespace
                    && c.names.is_empty()
            })
        {
            return Err(invalid(
                "claim create authority exceeds actual runtime capability",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod binding_tests {
    use super::validate_binding;
    use crate::RuntimeBinding;

    fn binding(image: String) -> RuntimeBinding {
        let digest = format!("sha256:{}", "a".repeat(64));
        RuntimeBinding {
            domain_id: "example".into(),
            cluster_binding: "cluster-a".into(),
            generation_id: "generation-a".into(),
            build_digest: digest.clone(),
            ir_digest: digest.clone(),
            policy_digest: digest.clone(),
            capability_digest: digest,
            service_account_name: "controller".into(),
            controller_image: image,
        }
    }

    #[test]
    fn controller_image_accepts_exact_bound_and_rejects_larger_or_whitespace() {
        let suffix = format!("@sha256:{}", "b".repeat(64));
        let exact = format!("{}{}", "a".repeat(4096 - suffix.len()), suffix);
        assert!(validate_binding(&binding(exact.clone())).is_ok());
        assert!(validate_binding(&binding(format!("a{exact}"))).is_err());
        assert!(validate_binding(&binding(format!("a b{suffix}"))).is_err());
    }
}
