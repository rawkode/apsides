use crate::{NamespaceRegistry, Result, Versioned, invalid};
use compiler_core::CompiledPlan;
use deploy::{
    ClaimState, DeploymentRequest, GenerationAllocation, GenerationPlan, ProtectedClaim,
    ResourceIdentity,
};
use release::IntegrityVerifiedCandidate;
use std::collections::BTreeSet;

const CLAIM_COLLISION: &str =
    "new target collides with a protected claim; adoption and transfer are unsupported";
const INVALID_STATE: &str = "candidate qualification collision state is invalid";

pub(super) fn is_claim_collision(error: &deploy::Error) -> bool {
    matches!(error, deploy::Error::Plan(message) if message == CLAIM_COLLISION)
}

pub(super) fn validate_collision_state(
    current: &Versioned<NamespaceRegistry>,
    request: &DeploymentRequest,
    owner: &IntegrityVerifiedCandidate,
    contender: &IntegrityVerifiedCandidate,
) -> Result<()> {
    validate_compiled_collision_state(
        current,
        request,
        owner.compiled(),
        owner.release_digest(),
        contender.compiled(),
        contender.release_digest(),
    )
}

fn validate_compiled_collision_state(
    current: &Versioned<NamespaceRegistry>,
    request: &DeploymentRequest,
    owner: &CompiledPlan,
    owner_release: &str,
    contender: &CompiledPlan,
    contender_release: &str,
) -> Result<()> {
    let owner_domain = &owner.ir.domain;
    let contender_domain = &contender.ir.domain;
    let owner_identities = identities(owner)?;
    let contender_identities = identities(contender)?;
    if compiler_core::validate(&owner.ir).ok().as_ref() != Some(owner)
        || compiler_core::validate(&contender.ir).ok().as_ref() != Some(contender)
        || owner_domain.id == contender_domain.id
        || owner_release == contender_release
        || !same_scope(owner, contender)
        || owner_identities.intersection(&contender_identities).count() != 1
    {
        return Err(collision_invalid());
    }
    validate_contender_request(current, request, contender, contender_release)?;
    validate_owner_registry(current, owner, owner_release, contender, &owner_identities)
}

fn same_scope(owner: &CompiledPlan, contender: &CompiledPlan) -> bool {
    let owner = &owner.ir.domain;
    let contender = &contender.ir.domain;
    owner.cluster_binding == contender.cluster_binding
        && owner.control_namespace == contender.control_namespace
        && owner.namespace == contender.namespace
}

fn identities(compiled: &CompiledPlan) -> Result<BTreeSet<ResourceIdentity>> {
    compiled
        .ir
        .resources
        .iter()
        .map(deploy::resource_identity)
        .collect::<deploy::Result<_>>()
        .map_err(|_| collision_invalid())
}

fn validate_contender_request(
    current: &Versioned<NamespaceRegistry>,
    request: &DeploymentRequest,
    contender: &CompiledPlan,
    contender_release: &str,
) -> Result<()> {
    let names = deploy::allocation_names(&current.uid, 2).map_err(|_| collision_invalid())?;
    if request.previous.is_some()
        || !request.observations.is_empty()
        || !request.ownership_intents.is_empty()
        || request.registry.next_sequence != 2
        || request.next.compiled != *contender
        || request.next.artifact_digest != contender_release
        || request.next.generation_id != names.generation_id
        || request.transition_id != names.transition_id
    {
        return Err(collision_invalid());
    }
    Ok(())
}

fn validate_owner_registry(
    current: &Versioned<NamespaceRegistry>,
    owner: &CompiledPlan,
    owner_release: &str,
    contender: &CompiledPlan,
    owner_identities: &BTreeSet<ResourceIdentity>,
) -> Result<()> {
    let registry = &current.value;
    let owner_domain = &owner.ir.domain;
    let names = deploy::allocation_names(&current.uid, 1).map_err(|_| collision_invalid())?;
    let claims = owner_claims(owner_identities, owner_domain.id.as_str(), &names);
    if registry.registry_uid != current.uid
        || registry.next_sequence != 2
        || registry.schema_version != crate::REGISTRY_SCHEMA_VERSION
        || registry.cluster_binding != owner_domain.cluster_binding
        || registry.workload_namespace != owner_domain.namespace
        || registry.claims != claims
        || registry.histories.len() != 2
        || !registry.active_writer_checkpoints.is_empty()
        || !registry.audit_heads.is_empty()
        || registry.transitions.len() != 1
    {
        return Err(collision_invalid());
    }
    let stored = registry
        .transitions
        .get(&names.transition_id)
        .ok_or_else(collision_invalid)?;
    validate_histories(registry, owner, contender, stored)?;
    validate_owner_transition(current, stored, owner, owner_release, &names)
}

fn owner_claims(
    identities: &BTreeSet<ResourceIdentity>,
    domain: &str,
    names: &deploy::AllocationNames,
) -> Vec<ProtectedClaim> {
    identities
        .iter()
        .map(|identity| ProtectedClaim {
            identity: identity.clone(),
            domain_id: domain.into(),
            generation_id: names.generation_id.clone(),
            transition_id: names.transition_id.clone(),
            state: ClaimState::Reserved,
            uid: None,
        })
        .collect()
}

fn validate_histories(
    registry: &NamespaceRegistry,
    owner: &CompiledPlan,
    contender: &CompiledPlan,
    stored: &crate::StoredTransition,
) -> Result<()> {
    let owner_domain = &owner.ir.domain;
    let contender_domain = &contender.ir.domain;
    let Some(owner_history) = registry.histories.get(&owner_domain.id) else {
        return Err(collision_invalid());
    };
    let Some(contender_history) = registry.histories.get(&contender_domain.id) else {
        return Err(collision_invalid());
    };
    let frozen = &stored.inputs.registry.generation_history;
    if registry.histories.keys().collect::<BTreeSet<_>>()
        != BTreeSet::from([&owner_domain.id, &contender_domain.id])
        || registry.histories.values().any(|history| {
            history.domain_id.is_empty()
                || history.cluster_binding != owner_domain.cluster_binding
                || history.revision.is_empty()
                || history.revision == "0"
                || history.active_allocation.is_some()
        })
        || owner_history.domain_id != owner_domain.id
        || contender_history.domain_id != contender_domain.id
        || owner_history.revision != contender_history.revision
        || frozen.domain_id != owner_domain.id
        || frozen.cluster_binding != owner_domain.cluster_binding
        || frozen.active_allocation.is_some()
        || frozen.revision != stored.inputs.registry.revision
    {
        return Err(collision_invalid());
    }
    Ok(())
}

fn validate_owner_transition(
    current: &Versioned<NamespaceRegistry>,
    stored: &crate::StoredTransition,
    owner: &CompiledPlan,
    owner_release: &str,
    names: &deploy::AllocationNames,
) -> Result<()> {
    let generation = GenerationPlan {
        generation_id: names.generation_id.clone(),
        artifact_digest: owner_release.into(),
        compiled: owner.clone(),
    };
    let (scope, inventory) =
        deploy::generation_inventory(&generation, &[]).map_err(|_| collision_invalid())?;
    let expected = GenerationAllocation {
        registry_uid: current.uid.clone(),
        sequence: 1,
        generation_id: names.generation_id.clone(),
        service_account_name: names.service_account_name.clone(),
        transition_id: names.transition_id.clone(),
        artifact_digest: owner_release.into(),
        ir_digest: owner.ir_digest.clone(),
        inventory_metadata_digest: deploy::inventory_metadata_digest(&scope, &inventory)
            .map_err(|_| collision_invalid())?,
    };
    let record = &stored.record;
    let current_claims = current
        .value
        .claim_snapshot()
        .map_err(|_| collision_invalid())?;
    let empty_claims = coordination::ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
        scope: current_claims.scope,
        claims: vec![],
    }
    .reference()
    .map_err(|_| collision_invalid())?;
    if stored.inputs.allocation != expected
        || stored.inputs.registry.next_sequence != 1
        || stored.inputs.registry.claim_snapshot != empty_claims
        || stored.inputs.previous.is_some()
        || !stored.inputs.ownership_intents.is_empty()
        || stored.previous_review_metadata.is_some()
        || record.schema_version != "reakt.dev/transition-record/v1"
        || record.transition_id != names.transition_id
        || record.plan_digest != stored.inputs.plan_digest
        || !crate::valid_digest(&record.plan_digest)
        || record.executor_id.is_empty()
        || record.revision != 0
        || record.verification_epoch != 0
        || record.phase != deploy::Phase::Planned
        || record.blocked_from.is_some()
        || record.blocked_reason.is_some()
        || !record.receipts.is_empty()
        || !stored.staged_identity_uids.is_empty()
        || !stored.staged_content_digests.is_empty()
        || stored.runtime_binding.is_some()
        || stored.staged_trust_policy_digest.is_some()
        || stored.staged_provenance_digest.is_some()
        || stored.request_drain_qualification.is_some()
        || stored.proxy_identity.is_some()
        || stored.upgrade_drain.is_some()
        || stored.previous_writer_checkpoint.is_some()
    {
        return Err(collision_invalid());
    }
    Ok(())
}

fn collision_invalid() -> crate::Error {
    invalid(INVALID_STATE)
}

#[cfg(test)]
mod tests;
