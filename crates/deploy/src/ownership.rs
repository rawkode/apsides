use crate::{
    DeploymentRequest, Error, GenerationPlan, ResourceIdentity, Result, valid_digest, valid_id,
};
use compiler_core::{DeletionPolicy, Resource};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub const OWNERSHIP_INTENT_SCHEMA: &str = "reakt.dev/ownership-intent/v1";

/// Independently administered approval content. A deserialized instance is not
/// authorization: the platform re-reads the exact protected object before use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnershipApproval {
    pub schema_version: String,
    pub transition_id: String,
    pub domain_id: String,
    pub generation_id: String,
    pub release_digest: String,
    pub cluster_binding: String,
    pub control_namespace: String,
    pub identity: ResourceIdentity,
    pub observed_uid: String,
    pub observed_resource_version: String,
    pub prior_managed_fields_digest: String,
    /// The exact statically compiled apply representation, excluding server
    /// fields. This records permitted field values without a custom patch DSL.
    pub permitted_apply: Value,
    /// Only compiler-owned dynamic JSON pointers may appear here.
    pub permitted_runtime_fields: Vec<String>,
    pub deletion_policy: DeletionPolicy,
    /// Non-force adoption accepts an empty conflict list. A real SSA conflict
    /// blocks; it cannot silently become an ownership-forcing permission.
    pub prior_manager_conflicts: Vec<String>,
    pub action: OwnershipAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum OwnershipAction {
    Adopt,
    Transfer {
        source_domain_id: String,
        source_generation_id: String,
        source_transition_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnershipIntent {
    pub object_name: String,
    pub object_uid: String,
    pub object_resource_version: String,
    pub approval: OwnershipApproval,
}

/// Metadata required to reconstruct an ownership intent from an authenticated
/// generation. Desired values and their digests are deliberately excluded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenOwnershipIntent {
    pub object_name: String,
    pub object_uid: String,
    pub object_resource_version: String,
    pub identity: ResourceIdentity,
    pub observed_uid: String,
    pub observed_resource_version: String,
    pub prior_managed_fields_digest: String,
    pub action: OwnershipAction,
}

/// Project validated ownership approvals into metadata-only replay inputs.
/// # Errors
/// Rejects any ownership intent that is not valid for the canonical request.
pub fn freeze_ownership_intents(request: &DeploymentRequest) -> Result<Vec<FrozenOwnershipIntent>> {
    validate_intents(request)?;
    let frozen = request
        .ownership_intents
        .iter()
        .map(|intent| FrozenOwnershipIntent {
            object_name: intent.object_name.clone(),
            object_uid: intent.object_uid.clone(),
            object_resource_version: intent.object_resource_version.clone(),
            identity: intent.approval.identity.clone(),
            observed_uid: intent.approval.observed_uid.clone(),
            observed_resource_version: intent.approval.observed_resource_version.clone(),
            prior_managed_fields_digest: intent.approval.prior_managed_fields_digest.clone(),
            action: intent.approval.action.clone(),
        })
        .collect::<Vec<_>>();
    validate_frozen_ownership_intents(&frozen)?;
    Ok(frozen)
}

/// Validate bounded metadata independently of any selected generation.
/// # Errors
/// Rejects malformed identities, duplicates, oversized metadata or unsupported transfers.
pub fn validate_frozen_ownership_intents(frozen: &[FrozenOwnershipIntent]) -> Result<()> {
    if frozen.len() > 4096 || compiler_core::canonical_bytes(&frozen)?.len() > 1024 * 1024 {
        return Err(Error::Plan(
            "frozen ownership intents exceed their bounded metadata budget".into(),
        ));
    }
    let mut names = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for metadata in frozen {
        if !valid_id(&metadata.object_name)
            || !valid_id(&metadata.object_uid)
            || !valid_version(&metadata.object_resource_version)
            || !valid_id(&metadata.observed_uid)
            || !valid_version(&metadata.observed_resource_version)
            || !valid_digest(&metadata.prior_managed_fields_digest)
            || !bounded_identity(&metadata.identity)
            || metadata.action != OwnershipAction::Adopt
            || !names.insert(&metadata.object_name)
            || !identities.insert(&metadata.identity)
        {
            return Err(Error::Plan(
                "frozen ownership intent metadata is invalid or duplicated".into(),
            ));
        }
    }
    Ok(())
}

/// Reconstruct exact ownership approvals from an authenticated generation.
/// # Errors
/// Rejects unknown, duplicate or malformed metadata and unsupported transfers.
pub fn restore_ownership_intents(
    target: &GenerationPlan,
    transition: &str,
    frozen: &[FrozenOwnershipIntent],
) -> Result<Vec<OwnershipIntent>> {
    validate_frozen_ownership_intents(frozen)?;
    if frozen.len() > target.compiled.ir.resources.len() {
        return Err(Error::Plan("too many ownership intents".into()));
    }
    let domain = &target.compiled.ir.domain;
    let mut names = BTreeSet::new();
    let mut identities = BTreeSet::new();
    let mut restored = Vec::with_capacity(frozen.len());
    for metadata in frozen {
        let resource = target
            .compiled
            .ir
            .resources
            .iter()
            .find(|resource| {
                crate::planner::resource_identity(resource)
                    .is_ok_and(|identity| identity == metadata.identity)
            })
            .ok_or_else(|| {
                Error::Plan("ownership intent target is outside the compiled plan".into())
            })?;
        if !names.insert(&metadata.object_name) || !identities.insert(&metadata.identity) {
            return Err(Error::Plan(
                "duplicate or unsupported frozen ownership intent".into(),
            ));
        }
        let intent = OwnershipIntent {
            object_name: metadata.object_name.clone(),
            object_uid: metadata.object_uid.clone(),
            object_resource_version: metadata.object_resource_version.clone(),
            approval: OwnershipApproval {
                schema_version: OWNERSHIP_INTENT_SCHEMA.into(),
                transition_id: transition.into(),
                domain_id: domain.id.clone(),
                generation_id: target.generation_id.clone(),
                release_digest: target.artifact_digest.clone(),
                cluster_binding: domain.cluster_binding.clone(),
                control_namespace: domain.control_namespace.clone(),
                identity: metadata.identity.clone(),
                observed_uid: metadata.observed_uid.clone(),
                observed_resource_version: metadata.observed_resource_version.clone(),
                prior_managed_fields_digest: metadata.prior_managed_fields_digest.clone(),
                permitted_apply: resource.desired.clone(),
                permitted_runtime_fields: runtime_fields(target, resource),
                deletion_policy: resource.lifecycle.deletion_policy,
                prior_manager_conflicts: Vec::new(),
                action: metadata.action.clone(),
            },
        };
        validate_ownership_intent(&intent, target, transition)?;
        restored.push(intent);
    }
    Ok(restored)
}

fn bounded_identity(identity: &ResourceIdentity) -> bool {
    identity.api_group.len() <= 253
        && identity.api_group.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
        && [&identity.resource, &identity.namespace, &identity.name]
            .iter()
            .all(|value| {
                !value.is_empty()
                    && value.len() <= 253
                    && value.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || byte == b'-'
                            || byte == b'.'
                    })
            })
}

/// Validate the immutable target/field contract; this does not authenticate the
/// approval object or establish live UID/field ownership.
/// # Errors
/// Rejects unbound, ambiguous, oversized, or unsupported ownership intentions.
pub fn validate_ownership_intent(
    intent: &OwnershipIntent,
    target: &GenerationPlan,
    transition: &str,
) -> Result<()> {
    let approval = &intent.approval;
    let domain = &target.compiled.ir.domain;
    let resource = target
        .compiled
        .ir
        .resources
        .iter()
        .find(|resource| {
            crate::planner::resource_identity(resource)
                .is_ok_and(|identity| identity == approval.identity)
        })
        .ok_or_else(|| {
            Error::Plan("ownership intent target is outside the compiled plan".into())
        })?;
    if !valid_id(&intent.object_name)
        || !valid_id(&intent.object_uid)
        || !valid_version(&intent.object_resource_version)
        || !valid_id(&approval.observed_uid)
        || !valid_version(&approval.observed_resource_version)
        || !valid_digest(&approval.prior_managed_fields_digest)
        || approval.schema_version != OWNERSHIP_INTENT_SCHEMA
        || approval.transition_id != transition
        || approval.domain_id != domain.id
        || approval.generation_id != target.generation_id
        || approval.release_digest != target.artifact_digest
        || approval.cluster_binding != domain.cluster_binding
        || approval.control_namespace != domain.control_namespace
        || approval.permitted_apply != resource.desired
        || approval.permitted_runtime_fields != runtime_fields(target, resource)
        || approval.deletion_policy != resource.lifecycle.deletion_policy
        || !approval.prior_manager_conflicts.is_empty()
    {
        return Err(Error::Plan(
            "ownership intent binding, UID, fields, lifecycle or conflict contract differs".into(),
        ));
    }
    Ok(())
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value != "0"
        && value.len() <= 256
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn runtime_fields(target: &GenerationPlan, resource: &Resource) -> Vec<String> {
    if target
        .compiled
        .ir
        .reactions
        .iter()
        .any(|reaction| reaction.target == resource.id)
    {
        vec!["/spec/template/metadata/annotations/reakt.dev~1revision".into()]
    } else {
        Vec::new()
    }
}

pub(crate) fn validate_intents(request: &DeploymentRequest) -> Result<()> {
    let mut names = BTreeSet::new();
    let mut identities = BTreeSet::new();
    let previous = request.predecessor();
    if request.ownership_intents.len() > request.next.compiled.ir.resources.len() {
        return Err(Error::Plan("too many ownership intents".into()));
    }
    for intent in &request.ownership_intents {
        validate_ownership_intent(intent, &request.next, &request.transition_id)?;
        if !names.insert(&intent.object_name) || !identities.insert(&intent.approval.identity) {
            return Err(Error::Plan("duplicate ownership intent identity".into()));
        }
        if previous.is_some_and(|previous| {
            previous.inventory.resources.iter().any(|resource| {
                resource.identity.api_group == intent.approval.identity.api_group
                    && resource.identity.resource == intent.approval.identity.resource
                    && resource.identity.namespace == intent.approval.identity.namespace
                    && resource.identity.name == intent.approval.identity.name
            })
        }) {
            return Err(Error::Plan(
                "ownership intent cannot relabel an already managed resource".into(),
            ));
        }
        if intent.approval.action != OwnershipAction::Adopt {
            return Err(Error::Plan(
                "cross-domain transfer requires the qualified transfer executor".into(),
            ));
        }
    }
    Ok(())
}
