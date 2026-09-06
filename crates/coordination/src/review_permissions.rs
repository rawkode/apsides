use super::validation::{name, namespace, require, sorted, text};
use super::{
    ReviewApiRequirement, ReviewDeploymentLimitation, ReviewMetadataV1, ReviewQualifiedServer,
};
use super::{ReviewCapability, ReviewCeiling};
use crate::Result;
use std::collections::BTreeSet;

fn verb(value: &str) -> bool {
    matches!(
        value,
        "get"
            | "list"
            | "watch"
            | "create"
            | "update"
            | "patch"
            | "delete"
            | "deletecollection"
            | "bind"
            | "escalate"
    )
}
fn group(value: &str) -> bool {
    value.is_empty() || name(value)
}
fn resource(value: &str) -> bool {
    let mut parts = value.split('/');
    parts.next().is_some_and(namespace)
        && parts.next().is_none_or(namespace)
        && parts.next().is_none()
}
#[derive(Clone, Copy)]
enum CapabilityUse {
    Requested,
    Ceiling,
}

fn capabilities(values: &[ReviewCapability], usage: CapabilityUse) -> Result<()> {
    require(
        sorted(values)
            && values.iter().all(|c| {
                namespace(&c.namespace)
                    && group(&c.api_group)
                    && namespace(&c.resource)
                    && c.subresource.as_deref().is_none_or(namespace)
                    && (matches!(usage, CapabilityUse::Ceiling)
                        || c.names.is_empty() == (c.verb == "create"))
                    && verb(&c.verb)
                    && sorted(&c.names)
                    && c.names.iter().all(|n| name(n))
            }),
        "review capability shape or order",
    )
}
fn covers(requested: &ReviewCapability, allowed: &ReviewCapability) -> bool {
    requested.namespace == allowed.namespace
        && requested.api_group == allowed.api_group
        && requested.resource == allowed.resource
        && requested.subresource == allowed.subresource
        && requested.verb == allowed.verb
        && if requested.names.is_empty() {
            allowed.names.is_empty()
        } else {
            !allowed.names.is_empty()
                && requested
                    .names
                    .iter()
                    .all(|name| allowed.names.contains(name))
        }
}
fn gap(value: &str) -> bool {
    matches!(
        value,
        "rbac-create-not-name-restricted"
            | "rbac-patch-not-field-restricted"
            | "rbac-update-not-field-restricted"
            | "sensitive-revision-change-observable"
    )
}
fn risk(value: &str) -> bool {
    if matches!(
        value,
        "workload-template-modification"
            | "workload-service-account-authority"
            | "mutable-workload-image"
            | "workload-secret-reference"
            | "external-input-writers-influence-rollout"
            | "secret-read-authority"
            | "rollout-key-read-authority"
            | "opaque-custom-resource-effects"
    ) {
        return true;
    }
    value.strip_prefix("force-apply:").is_some_and(|identity| {
        let mut parts = identity.split('/');
        parts.next().is_some_and(|kind| {
            kind != "Secret"
                && kind.len() <= 63
                && kind.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
                && kind.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }) && parts.next().is_some_and(namespace)
            && parts.next().is_some_and(name)
            && parts.next().is_none()
    })
}
fn ceiling(value: &ReviewCeiling, metadata: &ReviewMetadataV1) -> Result<()> {
    let scope = &metadata.scope;
    require(
        text(&value.policy_ref, 256)
            && value.domain_id == scope.domain_id
            && value.cluster_binding == scope.claim_scope.cluster_binding
            && value.control_namespace == scope.claim_scope.control_namespace
            && value.workload_namespace == scope.claim_scope.workload_namespace,
        "review ceiling scope",
    )?;
    capabilities(&value.capabilities, CapabilityUse::Ceiling)?;
    require(
        sorted(&value.accepted_enforcement_gaps)
            && value.accepted_enforcement_gaps.iter().all(|v| gap(v))
            && sorted(&value.accepted_indirect_risks)
            && value.accepted_indirect_risks.iter().all(|v| risk(v)),
        "review ceiling risk vocabulary",
    )
}

pub(super) fn validate(value: &ReviewMetadataV1) -> Result<()> {
    capabilities(&value.runtime_permissions, CapabilityUse::Requested)?;
    capabilities(&value.cleanup_permissions, CapabilityUse::Requested)?;
    let scope = &value.scope.claim_scope;
    require(
        value.runtime_permissions.iter().all(|c| {
            c.namespace == scope.workload_namespace || c.namespace == scope.control_namespace
        }) && value
            .cleanup_permissions
            .iter()
            .all(|c| c.namespace == scope.workload_namespace),
        "review requested permission scope",
    )?;
    let e = &value.enforcement;
    ceiling(&e.runtime_ceiling, value)?;
    ceiling(&e.cleanup_ceiling, value)?;
    require(
        e.runtime_ceiling.policy_ref == e.cleanup_ceiling.policy_ref
            && sorted(&e.gaps)
            && e.gaps
                .iter()
                .all(|v| gap(v) && e.runtime_ceiling.accepted_enforcement_gaps.contains(v))
            && sorted(&e.indirect_risks)
            && e.indirect_risks
                .iter()
                .all(|v| risk(v) && e.runtime_ceiling.accepted_indirect_risks.contains(v))
            && value
                .runtime_permissions
                .iter()
                .all(|c| e.runtime_ceiling.capabilities.iter().any(|a| covers(c, a)))
            && value
                .cleanup_permissions
                .iter()
                .all(|c| e.cleanup_ceiling.capabilities.iter().any(|a| covers(c, a))),
        "review ceiling decision",
    )?;
    deployment(value)?;
    compatibility(value)
}
fn deployment(value: &ReviewMetadataV1) -> Result<()> {
    let d = &value.deployment_permissions;
    require(sorted(&d.required_resources) && sorted(&d.creation_targets)
        && d.limitations == [ReviewDeploymentLimitation::CreateNamesRequireAdmission,
            ReviewDeploymentLimitation::GeneratedNamesRequireBroadAccess,
            ReviewDeploymentLimitation::InvokingIdentityBootstrapCredentialsAndDiscoveryExcluded,
            ReviewDeploymentLimitation::DatabaseCredentialsSeparate], "review deployment order or limitations")?;
    for access in &d.required_resources {
        require(
            access.namespace.as_deref().is_none_or(namespace)
                && group(&access.api_group)
                && resource(&access.resource)
                && verb(&access.verb)
                && sorted(&access.resource_names)
                && access.resource_names.iter().all(|n| name(n)),
            "review deployment access",
        )?;
    }
    for target in &d.creation_targets {
        require(
            namespace(&target.namespace)
                && group(&target.api_group)
                && namespace(&target.resource)
                && name(&target.name),
            "review creation target",
        )?;
    }
    let mut previous = None;
    for delegation in &d.role_delegation {
        let key = (&delegation.namespace, &delegation.role_name);
        require(
            namespace(&delegation.namespace)
                && name(&delegation.role_name)
                && previous.is_none_or(|old| old < key),
            "review role delegation identity or order",
        )?;
        capabilities(&delegation.granted_capabilities, CapabilityUse::Requested)?;
        require(
            delegation
                .granted_capabilities
                .iter()
                .all(|c| c.namespace == delegation.namespace),
            "review role delegation scope",
        )?;
        previous = Some(key);
    }
    Ok(())
}
fn capability_apis(capabilities: &[ReviewCapability]) -> Vec<ReviewApiRequirement> {
    capabilities
        .iter()
        .map(|c| ReviewApiRequirement {
            api_group: c.api_group.clone(),
            resource: c.resource.clone(),
            subresource: c.subresource.clone(),
            verb: c.verb.clone(),
            namespaced: true,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn deployment_apis(value: &ReviewMetadataV1) -> Vec<ReviewApiRequirement> {
    value
        .deployment_permissions
        .required_resources
        .iter()
        .map(|a| {
            let (resource, subresource) = a
                .resource
                .split_once('/')
                .map_or((a.resource.as_str(), None), |(r, s)| {
                    (r, Some(s.to_owned()))
                });
            ReviewApiRequirement {
                api_group: a.api_group.clone(),
                resource: resource.into(),
                subresource,
                verb: a.verb.clone(),
                namespaced: a.namespace.is_some(),
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn compatibility(value: &ReviewMetadataV1) -> Result<()> {
    let c = &value.compatibility;
    let b = &c.runtime_budgets;
    require(
        c.runtime_replicas == 1
            && c.qualified_create_precondition_servers == [ReviewQualifiedServer::V1343]
            && c.required_runtime_apis == capability_apis(&value.runtime_permissions)
            && c.required_cleanup_apis == capability_apis(&value.cleanup_permissions)
            && c.required_deployment_apis == deployment_apis(value),
        "review compatibility requirements",
    )?;
    require(
        (1..=256).contains(&b.max_resources)
            && (1..=256).contains(&b.max_inputs)
            && (1..=256).contains(&b.max_reactions)
            && (1024..=262_144).contains(&b.max_object_bytes)
            && (1..=1024).contains(&b.max_queue_depth)
            && (1..=32).contains(&b.max_concurrent_requests)
            && value.resources.len() <= b.max_resources
            && value.inputs.len() <= b.max_inputs
            && value.reactions.len() <= b.max_reactions
            && value.resources.len() <= b.max_queue_depth,
        "review runtime budgets",
    )
}
