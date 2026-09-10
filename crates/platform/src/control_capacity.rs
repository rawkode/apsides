//! Canonical byte ceilings for platform-owned activation and inventory state.

#[cfg(feature = "deployer")]
use crate::{ActivationState, RuntimeInventory};
use crate::{Result, invalid};
use compiler_core::canonical_bytes;

/// Runtime processes cannot read registry admission, so control-object metadata
/// uses the smallest supported encoded scalar tier in every admitted profile.
const RUNTIME_CONTROL_SCALAR_JSON_BYTES: usize = 128;

pub(crate) fn validate_runtime_control_metadata(uid: &str, resource_version: &str) -> Result<()> {
    if !bounded_control_scalar(uid)? || !bounded_control_scalar(resource_version)? {
        return Err(invalid(
            "control object UID or resourceVersion exceeds its source bound",
        ));
    }
    Ok(())
}

fn bounded_control_scalar(value: &str) -> Result<bool> {
    Ok(!value.is_empty()
        && value.len() <= 1024
        && value.bytes().all(|byte| byte.is_ascii_graphic())
        && canonical_bytes(&value)?.len() <= RUNTIME_CONTROL_SCALAR_JSON_BYTES)
}

#[cfg(feature = "deployer")]
pub(crate) fn validate_control_metadata(
    stored: &crate::StoredTransition,
    uid: &str,
    resource_version: &str,
) -> Result<()> {
    validate_runtime_control_metadata(uid, resource_version)?;
    stored.capacity.profile.validate_uid(uid)?;
    stored
        .capacity
        .profile
        .validate_resource_version(resource_version)
}

#[cfg(feature = "deployer")]
pub(crate) fn validate_activation(
    stored: &crate::StoredTransition,
    activation: &ActivationState,
) -> Result<()> {
    validate_bytes(
        canonical_bytes(activation)?.len(),
        stored.capacity.activation_bytes,
        "activation exceeds its admitted completion ceiling",
    )
}

#[cfg(feature = "deployer")]
pub(crate) fn validate_inventory(
    stored: &crate::StoredTransition,
    inventory: &RuntimeInventory,
) -> Result<()> {
    validate_control_metadata(
        stored,
        &inventory.activation_uid,
        &inventory.activation_resource_version,
    )?;
    validate_bytes(
        canonical_bytes(inventory)?.len(),
        stored.capacity.inventory_bytes,
        "inventory exceeds its admitted completion ceiling",
    )
}

#[cfg(feature = "deployer")]
fn validate_bytes(actual: usize, admitted: u64, error: &'static str) -> Result<()> {
    let admitted = usize::try_from(admitted)
        .map_err(|_| invalid("control object completion ceiling cannot be represented"))?;
    if actual > admitted {
        return Err(invalid(error));
    }
    Ok(())
}

#[cfg(feature = "deployer")]
pub(crate) fn control_object_ceilings(
    stored: &crate::StoredTransition,
    plan: &deploy::TransitionPlan,
    profile: &crate::EvidenceProfile,
) -> Result<(usize, usize)> {
    profile.validate()?;
    let next_binding = crate::envelope_writer::binding(stored);
    let next_resources = plan
        .request
        .next
        .compiled
        .ir
        .resources
        .iter()
        .map(|resource| {
            Ok((
                crate::runtime::resource_identity(resource)?,
                resource.lifecycle.deletion_policy,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut activation_bytes = canonical_bytes(&maximum_activation(
        stored,
        &next_binding,
        &next_resources,
        crate::ActivationStatus::Inactive,
        true,
    )?)?
    .len();
    let mut inventory_bytes = canonical_bytes(&maximum_inventory(
        &next_binding,
        next_resources.iter().map(|(identity, _)| identity.clone()),
        profile,
    )?)?
    .len();

    if let Some(previous) = plan.request.predecessor() {
        let checkpoint = stored
            .previous_writer_checkpoint
            .as_ref()
            .ok_or_else(|| invalid("predecessor control envelope lacks its writer checkpoint"))?;
        let prior_resources = previous
            .inventory
            .resources
            .iter()
            .map(|resource| {
                (
                    deploy::ResourceIdentity {
                        api_group: resource.identity.api_group.clone(),
                        resource: resource.identity.resource.clone(),
                        namespace: resource.identity.namespace.clone(),
                        name: resource.identity.name.clone(),
                    },
                    resource.deletion_policy,
                )
            })
            .collect::<Vec<_>>();
        activation_bytes = activation_bytes.max(
            canonical_bytes(&maximum_activation(
                stored,
                &checkpoint.runtime_binding,
                &prior_resources,
                crate::ActivationStatus::Draining,
                false,
            )?)?
            .len(),
        );
        inventory_bytes = inventory_bytes.max(
            canonical_bytes(&maximum_inventory(
                &checkpoint.runtime_binding,
                prior_resources.iter().map(|(identity, _)| identity.clone()),
                profile,
            )?)?
            .len(),
        );
    }
    Ok((activation_bytes, inventory_bytes))
}

#[cfg(feature = "deployer")]
fn maximum_activation(
    stored: &crate::StoredTransition,
    binding: &crate::RuntimeBinding,
    resources: &[(deploy::ResourceIdentity, compiler_core::DeletionPolicy)],
    status: crate::ActivationStatus,
    next: bool,
) -> Result<ActivationState> {
    let uid = "\0".repeat(128);
    let allocation = if next {
        stored.allocation()
    } else {
        &stored
            .inputs
            .previous
            .as_ref()
            .ok_or_else(|| invalid("predecessor activation envelope lacks metadata"))?
            .allocation
    };
    let approved_claims = resources
        .iter()
        .map(|(identity, deletion_policy)| crate::ApprovedClaim {
            claim: deploy::ProtectedClaim {
                identity: identity.clone(),
                domain_id: binding.domain_id.clone(),
                generation_id: allocation.generation_id.clone(),
                transition_id: allocation.transition_id.clone(),
                state: deploy::ClaimState::Active,
                uid: Some(uid.clone()),
            },
            deletion_policy: *deletion_policy,
            // `false` is one byte wider than the authoritative `true` value.
            allow_create: false,
        })
        .collect();
    Ok(ActivationState {
        schema_version: crate::SCHEMA_VERSION.into(),
        binding: binding.clone(),
        status,
        approved_claims,
        rollout_key_id: Some("k".repeat(253)),
    })
}

#[cfg(feature = "deployer")]
fn maximum_inventory(
    binding: &crate::RuntimeBinding,
    identities: impl Iterator<Item = deploy::ResourceIdentity>,
    profile: &crate::EvidenceProfile,
) -> Result<RuntimeInventory> {
    let uid = "\0".repeat(128);
    Ok(RuntimeInventory {
        schema_version: crate::SCHEMA_VERSION.into(),
        binding: binding.clone(),
        activation_uid: crate::envelope_writer::scalar(profile.max_uid_json_bytes)?,
        activation_resource_version: crate::envelope_writer::scalar(
            profile.max_resource_version_json_bytes,
        )?,
        observations: identities
            .map(|identity| deploy::RuntimeObservation {
                identity,
                uid: uid.clone(),
            })
            .collect(),
        conditions: vec![
            crate::RuntimeCondition {
                reason_code: "r".repeat(253),
                resource_name: Some("n".repeat(253)),
            };
            256
        ],
        drained: false,
    })
}

#[cfg(test)]
mod tests {
    use super::validate_runtime_control_metadata;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    #[test]
    fn runtime_control_metadata_uses_complete_json_string_boundary() -> TestResult {
        let exact = "\"".repeat(63);
        validate_runtime_control_metadata(&exact, &exact)?;
        let oversized = "\"".repeat(64);
        assert!(validate_runtime_control_metadata(&oversized, &exact).is_err());
        assert!(validate_runtime_control_metadata(&exact, &oversized).is_err());
        assert!(validate_runtime_control_metadata("\0", "1").is_err());
        Ok(())
    }
}
