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
            crate::planner::identity(resource).is_ok_and(|identity| identity == approval.identity)
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
    if request.ownership_intents.len() > request.next.compiled.ir.resources.len() {
        return Err(Error::Plan("too many ownership intents".into()));
    }
    for intent in &request.ownership_intents {
        validate_ownership_intent(intent, &request.next, &request.transition_id)?;
        if !names.insert(&intent.object_name) || !identities.insert(&intent.approval.identity) {
            return Err(Error::Plan("duplicate ownership intent identity".into()));
        }
        if request.previous.as_ref().is_some_and(|previous| {
            previous.compiled.ir.resources.iter().any(|resource| {
                crate::planner::identity(resource)
                    .is_ok_and(|identity| identity == intent.approval.identity)
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
