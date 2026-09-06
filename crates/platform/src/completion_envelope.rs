//! Admission is computed by serializing typed maxima, never by granting evidence authority.
use crate::{
    CapacityAdmission, EvidenceProfile, NamespaceRegistry, Result, envelope_record,
    envelope_writer, invalid,
};
use compiler_core::canonical_bytes;
use deploy::TransitionPlan;

pub(crate) fn initial_profile() -> EvidenceProfile {
    EvidenceProfile {
        schema_version: crate::evidence_profile::EVIDENCE_PROFILE_SCHEMA.into(),
        max_controller_pods: 1,
        max_regular_containers: 1,
        max_init_containers: 0,
        max_ephemeral_containers: 0,
        max_uid_json_bytes: 128,
        max_resource_version_json_bytes: 128,
        max_container_id_json_bytes: 256,
        max_drain_bytes: crate::evidence_profile::MAX_DRAIN_BYTES,
    }
}

pub(crate) fn calculate(
    registry: &NamespaceRegistry,
    plan: &TransitionPlan,
    profile: &EvidenceProfile,
) -> Result<CapacityAdmission> {
    profile.validate()?;
    let transition = &plan.request.transition_id;
    let current = registry.transition(transition)?;
    let mut maximum = current.clone();
    maximum.capacity = CapacityAdmission::projection_placeholder(profile.clone());
    maximum.record = envelope_record::maximum(current, plan)?;
    let checkpoint = envelope_writer::populate(&mut maximum, profile)?;
    if let Some(drain) = &maximum.upgrade_drain
        && canonical_bytes(drain)?.len() > profile.max_drain_bytes as usize
    {
        return Err(invalid(
            "complete admitted writer evidence cannot fit the drain byte ceiling",
        ));
    }
    let reference = registry.claim_snapshot()?.reference()?;
    let mut state = registry.state(&reference)?;
    if current.record.phase == deploy::Phase::Committed {
        // A committed generation does not own a successor's separate reservation.
        state.transitions.retain(|id, stored| {
            stored.domain_id() != current.domain_id()
                || id == transition
                || stored.record.phase == deploy::Phase::Committed
        });
    }
    state
        .transitions
        .insert(transition.clone(), maximum.clone());
    let domain = current.domain_id();
    let before = crate::state_capacity::domain_bytes(&state, domain)?;
    if let Some(previous) = &current.inputs.previous {
        state.transitions.remove(&previous.allocation.transition_id);
        state.audit_heads.insert(
            domain.into(),
            coordination::AuditRef {
                scope: coordination::AuditScope {
                    claim_scope: current.inputs.registry.claim_snapshot.scope.clone(),
                    domain_id: domain.into(),
                    transition_id: previous.allocation.transition_id.clone(),
                    sequence: previous.allocation.sequence,
                },
                digest: envelope_writer::commitment(),
            },
        );
    }
    state
        .histories
        .get_mut(domain)
        .ok_or_else(|| invalid("capacity domain history missing"))?
        .active_allocation = Some(current.allocation().clone());
    state
        .active_writer_checkpoints
        .insert(domain.into(), checkpoint);
    let committed = crate::state_capacity::domain_bytes(&state, domain)?;
    let (activation_bytes, inventory_bytes) =
        crate::control_capacity::control_object_ceilings(&maximum, plan, profile)?;
    let admission = CapacityAdmission {
        schema_version: crate::state_capacity::CAPACITY_SCHEMA.into(),
        revision: 0,
        profile: profile.clone(),
        domain_bytes: before.max(committed) as u64,
        activation_bytes: activation_bytes as u64,
        inventory_bytes: inventory_bytes as u64,
    };
    admission.validate()?;
    Ok(admission)
}

pub(crate) fn validate_selected(registry: &NamespaceRegistry, plan: &TransitionPlan) -> Result<()> {
    let stored = registry.transition(&plan.request.transition_id)?;
    stored.capacity.validate()?;
    envelope_record::validate(stored)?;
    if stored.record.phase != deploy::Phase::Committed {
        envelope_record::require_continuation(stored, plan)?;
    }
    let required = calculate(registry, plan, &stored.capacity.profile)?;
    if required.domain_bytes > stored.capacity.domain_bytes
        || required.activation_bytes > stored.capacity.activation_bytes
        || required.inventory_bytes > stored.capacity.inventory_bytes
    {
        return Err(invalid(
            "admitted capacity does not cover remaining lifecycle obligations",
        ));
    }
    Ok(())
}
