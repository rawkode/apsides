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

pub(crate) fn identity(resource: &Resource) -> Result<ResourceIdentity> {
    let (api_group, plural) = match (resource.api_version.as_str(), resource.kind.as_str()) {
        ("v1", "ConfigMap") => ("", "configmaps"),
        ("v1", "Service") => ("", "services"),
        ("apps/v1", "Deployment") => ("apps", "deployments"),
        _ => return Err(Error::Plan("unsupported lifecycle resource".into())),
    };
    Ok(ResourceIdentity {
        api_group: api_group.into(),
        resource: plural.into(),
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
        .map(|resource| Ok((identity(resource)?, resource)))
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
    if let Some(previous) = &request.previous {
        generation_valid(previous)?;
        let old = &previous.compiled.ir.domain;
        if previous.generation_id == request.next.generation_id {
            return Err(Error::Plan(
                "activation must allocate a new generation even for rollback".into(),
            ));
        }
        if old.id != next.id
            || old.cluster_binding != next.cluster_binding
            || old.namespace != next.namespace
            || old.control_namespace != next.control_namespace
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

fn allocation(generation: &GenerationPlan, transition_id: &str) -> Result<GenerationAllocation> {
    Ok(GenerationAllocation {
        generation_id: generation.generation_id.clone(),
        service_account_name: service_account_name(generation)?,
        transition_id: transition_id.into(),
        artifact_digest: generation.artifact_digest.clone(),
        ir_digest: generation.compiled.ir_digest.clone(),
    })
}

fn validate_generation_history(request: &DeploymentRequest) -> Result<()> {
    let history = &request.registry.generation_history;
    let domain = &request.next.compiled.ir.domain;
    if history.active_generation_id.as_ref()
        != request
            .previous
            .as_ref()
            .map(|previous| &previous.generation_id)
    {
        return Err(Error::Plan(
            "previous generation does not match protected active-generation authority".into(),
        ));
    }
    if history.domain_id != domain.id
        || history.cluster_binding != domain.cluster_binding
        || history.revision.is_empty()
        || history.revision.len() > 256
        || history.allocations.len() > 4096
    {
        return Err(Error::Plan(
            "generation history must be complete, bounded, and bound to this domain and cluster"
                .into(),
        ));
    }
    let mut ids = BTreeSet::new();
    let mut accounts = BTreeSet::new();
    let mut transitions = BTreeSet::new();
    let next_account = service_account_name(&request.next)?;
    for entry in &history.allocations {
        if !valid_id(&entry.generation_id)
            || !valid_id(&entry.transition_id)
            || !valid_id(&entry.service_account_name)
            || !valid_digest(&entry.artifact_digest)
            || !valid_digest(&entry.ir_digest)
            || !ids.insert(&entry.generation_id)
            || !accounts.insert(&entry.service_account_name)
            || !transitions.insert(&entry.transition_id)
        {
            return Err(Error::Plan(
                "generation history contains invalid or repeated allocations".into(),
            ));
        }
        if entry.generation_id == request.next.generation_id
            || entry.service_account_name == next_account
            || entry.transition_id == request.transition_id
        {
            return Err(Error::Plan("generation, runtime identity, or transition ID was allocated previously; rollback requires fresh activation identities".into()));
        }
    }
    if let Some(previous) = &request.previous {
        let entry = history
            .allocations
            .iter()
            .find(|entry| entry.generation_id == previous.generation_id)
            .ok_or_else(|| {
                Error::Plan("prior generation is absent from protected generation history".into())
            })?;
        if entry.artifact_digest != previous.artifact_digest
            || entry.ir_digest != previous.compiled.ir_digest
            || entry.service_account_name != service_account_name(previous)?
        {
            return Err(Error::Plan(
                "prior generation differs from its protected allocation history".into(),
            ));
        }
    }
    Ok(())
}

fn claims<'a>(
    request: &'a DeploymentRequest,
    previous: &BTreeMap<ResourceIdentity, &Resource>,
) -> Result<BTreeMap<ResourceIdentity, &'a ProtectedClaim>> {
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
    for id in previous.keys() {
        let claim = claims
            .get(id)
            .ok_or_else(|| Error::Plan("prior approved resource lacks a protected claim".into()))?;
        if claim.state != ClaimState::Active
            || claim.domain_id != request.next.compiled.ir.domain.id
            || Some(&claim.generation_id)
                != request.previous.as_ref().map(|prior| &prior.generation_id)
            || !request
                .registry
                .generation_history
                .allocations
                .iter()
                .any(|allocation| {
                    allocation.generation_id == claim.generation_id
                        && allocation.transition_id == claim.transition_id
                })
        {
            return Err(Error::Plan(
                "prior resource claim has a conflicting owner, generation or unfinished transition"
                    .into(),
            ));
        }
    }
    Ok(claims)
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
/// Rejects conflicting claims, generation reuse, missing UID authority, unsupported
/// migrations, or permissions outside independently supplied platform ceilings.
pub fn plan_transition(request: &DeploymentRequest) -> Result<TransitionPlan> {
    valid_request(request)?;
    let previous = request
        .previous
        .as_ref()
        .map(resources)
        .transpose()?
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
                request
                    .previous
                    .as_ref()
                    .map(|prior| prior.generation_id.clone())
            } else {
                None
            },
            next_generation_id: request.next.generation_id.clone(),
        });
    }
    let prior_inventory = request
        .previous
        .as_ref()
        .map(|generation| crate::generation_inventory(generation, &request.registry.claims))
        .transpose()?;
    let (cleanup, capabilities) = match &prior_inventory {
        Some((scope, snapshot)) => plan_inventory_cleanup(
            scope,
            snapshot,
            &next.keys().cloned().collect(),
            &request.registry.claims,
            &request.cleanup_ceiling,
        )?,
        None => (Vec::new(), Vec::new()),
    };
    cleanup_allowed(&capabilities, &request.cleanup_ceiling)?;
    let generation_reservation = allocation(&request.next, &request.transition_id)?;
    let mut plan = TransitionPlan {
        schema_version: "reakt.dev/transition-plan/v2".into(),
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
        LiveCleanupObject::Present { uid, .. } if uid != expected => Err(Error::Plan(
            "cleanup refuses to delete a replacement UID".into(),
        )),
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
