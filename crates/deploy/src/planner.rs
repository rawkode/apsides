use crate::{
    ClaimReservation, ClaimState, CleanupDecision, CleanupItem, DeploymentRequest, Error,
    GenerationAllocation, GenerationPlan, LiveCleanupObject, Phase, ProtectedClaim,
    ResourceIdentity, Result, TransitionPlan, TransitionRecord, service_account_name, valid_digest,
    valid_id,
};
use compiler_core::{
    Capability, DeletionPolicy, PermissionCeiling, Resource, digest, validate, validate_ceiling,
};
use std::collections::{BTreeMap, BTreeSet};

/// Project a compiler resource into its canonical lifecycle identity.
///
/// # Errors
/// Rejects resource types outside the supported lifecycle set.
pub fn resource_identity(resource: &Resource) -> Result<ResourceIdentity> {
    let api = compiler_core::resolve_resource_api(resource)
        .map_err(|_| Error::Plan("unsupported lifecycle resource".into()))?;
    Ok(ResourceIdentity {
        api_group: api.api_group,
        resource: api.plural,
        namespace: resource.namespace.clone(),
        name: resource.name.clone(),
    })
}

fn generation_valid(generation: &GenerationPlan) -> Result<()> {
    if !valid_id(&generation.generation_id) || !valid_digest(&generation.artifact_digest) {
        return Err(Error::Plan(
            "invalid generation or artifact identity".into(),
        ));
    }
    if validate(&generation.compiled.ir)? != generation.compiled {
        return Err(Error::Plan(
            "generation plan differs from trusted compiler derivation".into(),
        ));
    }
    Ok(())
}

fn resources(generation: &GenerationPlan) -> Result<BTreeMap<ResourceIdentity, &Resource>> {
    generation
        .compiled
        .ir
        .resources
        .iter()
        .map(|resource| Ok((resource_identity(resource)?, resource)))
        .collect()
}

fn inventory_resources(
    previous: &inventory::InventorySnapshot,
) -> BTreeMap<ResourceIdentity, &inventory::TrackedResource> {
    previous
        .resources
        .iter()
        .map(|resource| {
            (
                ResourceIdentity {
                    api_group: resource.identity.api_group.clone(),
                    resource: resource.identity.resource.clone(),
                    namespace: resource.identity.namespace.clone(),
                    name: resource.identity.name.clone(),
                },
                resource,
            )
        })
        .collect()
}

fn valid_request(request: &DeploymentRequest) -> Result<()> {
    if compiler_core::canonical_bytes(request)?.len() > 1024 * 1024 {
        return Err(Error::Plan(
            "transition request exceeds the 1 MiB control-state budget".into(),
        ));
    }
    if !valid_id(&request.transition_id)
        || request.registry.revision.is_empty()
        || request.registry.revision.len() > 256
        || request.registry.claims.len() > 4096
        || request.observations.len() > 4096
    {
        return Err(Error::Plan(
            "invalid transition identity or oversized control snapshot".into(),
        ));
    }
    generation_valid(&request.next)?;
    crate::ownership::validate_intents(request)?;
    validate_generation_history(request)?;
    validate_ceiling(&request.next.compiled, &request.runtime_ceiling)?;
    let next = &request.next.compiled.ir.domain;
    if let Some(previous) = request.predecessor() {
        if previous.allocation.generation_id == request.next.generation_id {
            return Err(Error::Plan(
                "activation must allocate a new generation even for rollback".into(),
            ));
        }
        if previous.scope.controller != next.id
            || previous.scope.cluster != next.cluster_binding
            || previous.scope.namespace != next.namespace
            || previous.control_namespace != next.control_namespace
        {
            return Err(Error::Plan(
                "domain, cluster and namespace migration is unsupported".into(),
            ));
        }
    }
    let ceiling = &request.cleanup_ceiling;
    if ceiling.policy_ref != next.policy_ref
        || ceiling.domain_id != next.id
        || ceiling.cluster_binding != next.cluster_binding
        || ceiling.workload_namespace != next.namespace
        || ceiling.control_namespace != next.control_namespace
    {
        return Err(Error::Plan(
            "cleanup ceiling binding differs from the domain".into(),
        ));
    }
    Ok(())
}

fn allocation(request: &DeploymentRequest) -> Result<GenerationAllocation> {
    let generation = &request.next;
    let transition_id = &request.transition_id;
    let (scope, snapshot) = crate::generation_inventory(generation, &[])?;
    Ok(GenerationAllocation {
        registry_uid: request.registry.registry_uid.clone(),
        sequence: request.registry.next_sequence,
        generation_id: generation.generation_id.clone(),
        service_account_name: service_account_name(generation)?,
        transition_id: transition_id.clone(),
        artifact_digest: generation.artifact_digest.clone(),
        ir_digest: generation.compiled.ir_digest.clone(),
        inventory_metadata_digest: crate::inventory_metadata_digest(&scope, &snapshot)?,
    })
}

fn validate_generation_history(request: &DeploymentRequest) -> Result<()> {
    let history = &request.registry.generation_history;
    let domain = &request.next.compiled.ir.domain;
    let previous = request.predecessor();
    request
        .registry
        .next_sequence
        .checked_add(1)
        .ok_or_else(|| Error::Plan("registry allocation counter exhausted".into()))?;
    let names = crate::allocation_names(
        &request.registry.registry_uid,
        request.registry.next_sequence,
    )?;
    if request.next.generation_id != names.generation_id
        || request.transition_id != names.transition_id
    {
        return Err(Error::Plan(
            "generation and transition must use the protected registry allocation sequence".into(),
        ));
    }
    if history.active_allocation.as_ref() != previous.map(|p| &p.allocation) {
        return Err(Error::Plan(
            "previous generation does not match protected active allocation".into(),
        ));
    }
    if history.domain_id != domain.id
        || history.cluster_binding != domain.cluster_binding
        || history.revision != request.registry.revision
    {
        return Err(Error::Plan(
            "active allocation scope or revision mismatch".into(),
        ));
    }
    if let Some(previous) = previous {
        let entry = &previous.allocation;
        crate::validate_allocation(entry)?;
        if entry.registry_uid != request.registry.registry_uid
            || entry.sequence >= request.registry.next_sequence
        {
            return Err(Error::Plan(
                "predecessor allocation is outside the registry incarnation or sequence".into(),
            ));
        }
        crate::validate_inventory_checkpoint(&previous.scope, &previous.inventory, entry)?;
        previous
            .inventory
            .encode(&previous.scope)
            .map_err(|_| Error::Plan("invalid predecessor inventory metadata".into()))?;
        if previous.retirement_role_binding_namespaces.len() > 4096 {
            return Err(Error::Plan(
                "too many predecessor retirement namespaces".into(),
            ));
        }
        let mut prior_namespace: Option<&str> = None;
        for namespace in &previous.retirement_role_binding_namespaces {
            if !valid_namespace(namespace)
                || prior_namespace.is_some_and(|prior| prior >= namespace.as_str())
            {
                return Err(Error::Plan(
                    "predecessor retirement namespaces must be sorted and unique".into(),
                ));
            }
            prior_namespace = Some(namespace);
        }
    }
    Ok(())
}

fn claims<'a>(
    request: &'a DeploymentRequest,
    previous: &BTreeMap<ResourceIdentity, &inventory::TrackedResource>,
) -> Result<BTreeMap<ResourceIdentity, &'a ProtectedClaim>> {
    let predecessor = request.predecessor();
    let mut claims = BTreeMap::new();
    for claim in &request.registry.claims {
        if !bounded_identity(&claim.identity) {
            return Err(Error::Plan(
                "invalid protected registry resource identity".into(),
            ));
        }
        if claims.insert(claim.identity.clone(), claim).is_some() {
            return Err(Error::Plan("duplicate protected registry claim".into()));
        }
        if !valid_id(&claim.domain_id)
            || !valid_id(&claim.generation_id)
            || !valid_id(&claim.transition_id)
            || claim.uid.as_ref().is_some_and(|uid| !valid_id(uid))
        {
            return Err(Error::Plan("invalid protected claim identity".into()));
        }
        if claim.domain_id == request.next.compiled.ir.domain.id
            && !previous.contains_key(&claim.identity)
        {
            return Err(Error::Plan(
                "unexpected existing domain claim requires recovery of its owning transition"
                    .into(),
            ));
        }
    }
    for (id, tracked) in previous {
        let claim = claims
            .get(id)
            .ok_or_else(|| Error::Plan("prior approved resource lacks a protected claim".into()))?;
        let allocation = &predecessor
            .ok_or_else(|| Error::Plan("predecessor claim lacks predecessor metadata".into()))?
            .allocation;
        if claim.state != ClaimState::Active
            || claim.domain_id != request.next.compiled.ir.domain.id
            || claim.generation_id != allocation.generation_id
            || claim.transition_id != allocation.transition_id
            || claim.uid != tracked.uid
        {
            return Err(Error::Plan(
                "prior resource claim has a conflicting owner, generation or unfinished transition"
                    .into(),
            ));
        }
    }
    Ok(claims)
}

fn valid_namespace(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn bounded_identity(identity: &ResourceIdentity) -> bool {
    identity.api_group.len() <= 253
        && identity
            .api_group
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'))
        && [&identity.resource, &identity.namespace, &identity.name]
            .iter()
            .all(|value| {
                !value.is_empty()
                    && value.len() <= 253
                    && value.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.')
                    })
            })
}

fn cleanup_capability(id: &ResourceIdentity, verb: &str) -> Capability {
    Capability {
        namespace: id.namespace.clone(),
        api_group: id.api_group.clone(),
        resource: id.resource.clone(),
        subresource: None,
        verb: verb.into(),
        names: vec![id.name.clone()],
    }
}

fn cleanup_allowed(capabilities: &[Capability], ceiling: &PermissionCeiling) -> Result<()> {
    for requested in capabilities {
        let covered = ceiling.capabilities.iter().any(|allowed| {
            allowed.namespace == requested.namespace
                && allowed.api_group == requested.api_group
                && allowed.resource == requested.resource
                && allowed.subresource == requested.subresource
                && allowed.verb == requested.verb
                && !allowed.names.is_empty()
                && requested
                    .names
                    .iter()
                    .all(|name| allowed.names.contains(name))
        });
        if !covered {
            return Err(Error::Plan(format!(
                "cleanup {} on {}/{} is outside its separate ceiling",
                requested.verb, requested.api_group, requested.resource
            )));
        }
    }
    Ok(())
}

/// Recompute an immutable lifecycle plan from approved generations and protected claims.
///
/// # Errors
/// Rejects conflicting claims, generation reuse, mismatched UID authority, unsupported
/// migrations, or permissions outside independently supplied platform ceilings.
pub fn plan_transition(request: &DeploymentRequest) -> Result<TransitionPlan> {
    valid_request(request)?;
    let predecessor = request.predecessor();
    let previous = predecessor
        .map(|previous| inventory_resources(&previous.inventory))
        .unwrap_or_default();
    let next = resources(&request.next)?;
    let claims = claims(request, &previous)?;
    let mut reservations = Vec::new();
    for id in next.keys() {
        if !previous.contains_key(id) && claims.contains_key(id) {
            return Err(Error::Plan(
                "new target collides with a protected claim; adoption and transfer are unsupported"
                    .into(),
            ));
        }
        if !previous.contains_key(id)
            && !request
                .ownership_intents
                .iter()
                .any(|intent| &intent.approval.identity == id)
            && request
                .observations
                .iter()
                .any(|observation| &observation.identity == id)
        {
            return Err(Error::Plan(
                "a newly managed target is already observed; explicit adoption is unsupported"
                    .into(),
            ));
        }
        reservations.push(ClaimReservation {
            identity: id.clone(),
            prior_generation_id: if previous.contains_key(id) {
                predecessor.map(|prior| prior.allocation.generation_id.clone())
            } else {
                None
            },
            next_generation_id: request.next.generation_id.clone(),
        });
    }
    let (cleanup, capabilities) = match predecessor {
        Some(previous) => plan_inventory_cleanup(
            &previous.scope,
            &previous.inventory,
            &next.keys().cloned().collect(),
            &request.registry.claims,
            &request.cleanup_ceiling,
        )?,
        None => (Vec::new(), Vec::new()),
    };
    cleanup_allowed(&capabilities, &request.cleanup_ceiling)?;
    let generation_reservation = allocation(request)?;
    let mut plan = TransitionPlan {
        schema_version: "apsides.dev/transition-plan/v4".into(),
        plan_digest: String::new(), request: request.clone(), reservations,
        generation_reservation, cleanup, cleanup_capabilities: capabilities,
        warnings: vec!["uncertain old-writer termination blocks activation; short-lived credentials do not undo durable changes".into()],
    };
    plan.plan_digest = digest(&plan)?;
    Ok(plan)
}

/// Plan omitted-resource cleanup from tracking metadata and protected claims.
/// No previous compiled artifact, manifest or source is required by this boundary.
/// The caller must supply the approved persisted snapshot; this does not make
/// arbitrary database contents authoritative.
/// # Errors
/// Rejects malformed tracking, incomplete/mismatched claims and excess permissions.
pub fn plan_inventory_cleanup(
    scope: &inventory::InventoryScope,
    previous: &inventory::InventorySnapshot,
    next: &BTreeSet<ResourceIdentity>,
    protected: &[ProtectedClaim],
    ceiling: &PermissionCeiling,
) -> Result<(Vec<CleanupItem>, Vec<Capability>)> {
    previous
        .encode(scope)
        .map_err(|_| Error::Plan("invalid prior inventory metadata".into()))?;
    if ceiling.domain_id != scope.controller
        || ceiling.cluster_binding != scope.cluster
        || ceiling.workload_namespace != scope.namespace
    {
        return Err(Error::Plan(
            "inventory cleanup ceiling scope mismatch".into(),
        ));
    }
    let mut claims = BTreeMap::new();
    for claim in protected {
        if claims.insert(claim.identity.clone(), claim).is_some() {
            return Err(Error::Plan("duplicate protected cleanup identity".into()));
        }
    }
    let mut identities = BTreeSet::new();
    for resource in &previous.resources {
        let id = ResourceIdentity {
            api_group: resource.identity.api_group.clone(),
            resource: resource.identity.resource.clone(),
            namespace: resource.identity.namespace.clone(),
            name: resource.identity.name.clone(),
        };
        let claim = claims
            .get(&id)
            .ok_or_else(|| Error::Plan("inventory resource lacks a protected claim".into()))?;
        if claim.domain_id != scope.controller
            || claim.generation_id != previous.generation
            || claim.state != ClaimState::Active
            || claim.uid != resource.uid
        {
            return Err(Error::Plan(
                "inventory resource differs from protected predecessor".into(),
            ));
        }
        identities.insert(id);
    }
    if protected.iter().any(|claim| {
        claim.domain_id == scope.controller
            && claim.generation_id == previous.generation
            && !identities.contains(&claim.identity)
    }) {
        return Err(Error::Plan(
            "inventory omits a protected predecessor resource".into(),
        ));
    }
    let (cleanup, capabilities) = cleanup_plan(previous, next, &claims)?;
    cleanup_allowed(&capabilities, ceiling)?;
    Ok((cleanup, capabilities))
}

fn cleanup_plan(
    previous: &inventory::InventorySnapshot,
    next: &BTreeSet<ResourceIdentity>,
    claims: &BTreeMap<ResourceIdentity, &ProtectedClaim>,
) -> Result<(Vec<CleanupItem>, Vec<Capability>)> {
    let mut cleanup = Vec::new();
    let mut capabilities = BTreeSet::new();
    for resource in &previous.resources {
        let id = &ResourceIdentity {
            api_group: resource.identity.api_group.clone(),
            resource: resource.identity.resource.clone(),
            namespace: resource.identity.namespace.clone(),
            name: resource.identity.name.clone(),
        };
        if next.contains(id) {
            continue;
        }
        let claim = claims
            .get(id)
            .ok_or_else(|| Error::Plan("cleanup target lacks a protected claim".into()))?;
        if resource.deletion_policy == DeletionPolicy::Delete {
            capabilities.insert(cleanup_capability(id, "get"));
            if claim.uid.is_some() {
                capabilities.insert(cleanup_capability(id, "delete"));
            }
        }
        cleanup.push(CleanupItem {
            identity: id.clone(),
            api_version: resource.api_version.clone(),
            kind: resource.kind.clone(),
            policy: resource.deletion_policy,
            expected_uid: claim.uid.clone(),
            prior_generation_id: claim.generation_id.clone(),
        });
    }
    // The current source has no ability to override an omitted resource's
    // prior deletion policy. Explicit overrides require a separate contract.
    Ok((cleanup, capabilities.into_iter().collect()))
}

/// Decide one cleanup operation immediately before a trusted executor acts.
///
/// # Errors
/// Rejects mismatched ownership, generation or UID, including replacement objects.
pub fn cleanup_decision(
    plan: &TransitionPlan,
    record: &TransitionRecord,
    identity: &ResourceIdentity,
    claim: &ProtectedClaim,
    live: &LiveCleanupObject,
) -> Result<CleanupDecision> {
    crate::reducer::verify_record(plan, record)?;
    if record.phase != Phase::Cleaning {
        return Err(Error::Plan(
            "cleanup requires the validated transition to be in Cleaning".into(),
        ));
    }
    let item = plan
        .cleanup
        .iter()
        .find(|item| &item.identity == identity)
        .ok_or_else(|| {
            Error::Plan("cleanup target is absent from the validated transition plan".into())
        })?;
    if claim.identity != item.identity
        || claim.domain_id != plan.request.next.compiled.ir.domain.id
        || claim.transition_id != plan.request.transition_id
        || claim.generation_id != item.prior_generation_id
        || claim.uid != item.expected_uid
        || claim.state != ClaimState::Releasing
    {
        return Err(Error::Plan(
            "live protected claim differs from cleanup authority".into(),
        ));
    }
    if item.policy == DeletionPolicy::Orphan {
        return Ok(CleanupDecision::ReleaseOrphanClaim);
    }
    if matches!(live, LiveCleanupObject::Absent) {
        return Ok(CleanupDecision::AlreadyAbsent);
    }
    let expected = item
        .expected_uid
        .as_ref()
        .ok_or_else(|| Error::Plan("cleanup lacks a trusted UID".into()))?;
    match live {
        LiveCleanupObject::Absent => Ok(CleanupDecision::AlreadyAbsent),
        LiveCleanupObject::Present { uid, .. } if uid != expected => Err(Error::CleanupUidMismatch),
        LiveCleanupObject::Present {
            finalizers,
            deletion_started: true,
            ..
        } if !finalizers.is_empty() => Ok(CleanupDecision::WaitForFinalizers),
        LiveCleanupObject::Present { uid, .. } => {
            Ok(CleanupDecision::DeleteWithUidPrecondition { uid: uid.clone() })
        }
    }
}
