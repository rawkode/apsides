use super::{
    MAX_REVIEW_DECLARATIONS, REVIEW_METADATA_SCHEMA, REVIEW_ROLLOUT_FIELD,
    ReviewApiResourceMapping, ReviewApiResourceScope, ReviewMetadata, ReviewOwnershipPlan,
    ReviewResource,
};
use super::{ReviewInputMode, ReviewSource};
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const fn require(ok: bool, reason: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(reason))
    }
}
pub(super) fn namespace(value: &str) -> bool {
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
pub(super) fn name(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(namespace)
}
pub(super) fn id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
pub(super) fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
pub(super) fn sorted<T: Ord>(items: &[T]) -> bool {
    items.windows(2).all(|pair| pair[0] < pair[1])
}
fn source(location: &ReviewSource) -> bool {
    let path = &location.file;
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && location.line > 0
        && location.column > 0
}
fn pointer(value: &str) -> bool {
    if !value.starts_with('/')
        || matches!(
            value,
            "/apiVersion" | "/kind" | "/metadata/name" | "/metadata/namespace"
        )
    {
        return false;
    }
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return false;
        }
    }
    true
}

fn resource_api(resource: &ReviewResource) -> Result<compiler_core::ResolvedApiResource> {
    if resource.kind == "Secret" {
        return Err(Error::Invalid("review resource API"));
    }
    let builtin = match resource.kind.as_str() {
        "ConfigMap" => Some(("v1", "", "configmaps")),
        "Service" => Some(("v1", "", "services")),
        "Deployment" => Some(("apps/v1", "apps", "deployments")),
        _ => None,
    };
    let (api_group, plural) = match (builtin, &resource.api_resource) {
        (Some((api_version, api_group, plural)), None) if resource.api_version == api_version => {
            (api_group, plural)
        }
        (
            None,
            Some(ReviewApiResourceMapping {
                plural,
                scope: ReviewApiResourceScope::Namespaced,
                schema_sha256,
            }),
        ) if super::encoding::valid_digest(schema_sha256) => {
            let api_group = resource
                .api_version
                .split_once('/')
                .map_or("", |(group, _)| group);
            (api_group, plural.as_str())
        }
        _ => return Err(Error::Invalid("review resource API")),
    };
    compiler_core::resolve_namespaced_api(&resource.api_version, &resource.kind, api_group, plural)
        .map_err(|_| Error::Invalid("review resource API"))
}

pub(super) fn validate(value: &ReviewMetadata) -> Result<()> {
    require(
        value.schema_version == REVIEW_METADATA_SCHEMA
            && value.resources.len() <= MAX_REVIEW_DECLARATIONS
            && value.inputs.len() <= MAX_REVIEW_DECLARATIONS
            && value.reactions.len() <= MAX_REVIEW_DECLARATIONS
            && value.handlers.len() <= MAX_REVIEW_DECLARATIONS
            && value.egress.len() <= MAX_REVIEW_DECLARATIONS
            && value.outputs.len() <= MAX_REVIEW_DECLARATIONS
            && super::encoding::valid_digest(&value.surface_digest),
        "review schema, declaration count or surface digest",
    )?;
    let scope = &value.scope.claim_scope;
    let mut ids = BTreeMap::new();
    let mut previous_identity = None;
    let mut approval_names = BTreeSet::new();
    for resource in &value.resources {
        let identity = &resource.identity;
        let mapping = resource_api(resource)?;
        require(
            namespace(&resource.id)
                && ids.insert(&resource.id, resource).is_none()
                && previous_identity.is_none_or(|old| old < identity)
                && identity.api_group == mapping.api_group
                && identity.resource == mapping.plural
                && identity.namespace == scope.workload_namespace
                && name(&identity.name)
                && source(&resource.source)
                && sorted(&resource.apply_fields)
                && resource.apply_fields.iter().all(|path| pointer(path)),
            "review resource identity, fields or source",
        )?;
        previous_identity = Some(identity);
        if let ReviewOwnershipPlan::AdoptionApproved {
            intent_object_name,
            object_uid,
            object_resource_version,
            observed_uid,
            observed_resource_version,
        } = &resource.ownership_plan
        {
            require(
                id(intent_object_name)
                    && approval_names.insert(intent_object_name)
                    && id(object_uid)
                    && id(observed_uid)
                    && version(object_resource_version)
                    && version(observed_resource_version),
                "review ownership approval",
            )?;
        }
    }
    validate_inputs(value, &ids)?;
    validate_reactions(value, &ids)?;
    let input_ids: BTreeSet<_> = value.inputs.iter().map(|input| &input.id).collect();
    let output_ids = validate_outputs(value)?;
    let handler_ids = validate_handlers(value, &input_ids, &output_ids)?;
    validate_egress(value, &handler_ids)
}
fn validate_outputs(value: &ReviewMetadata) -> Result<BTreeSet<&String>> {
    let mut previous = None;
    let mut ids = BTreeSet::new();
    for output in &value.outputs {
        require(
            namespace(&output.id)
                && previous.is_none_or(|old| old < &output.id)
                && ids.insert(&output.id)
                && name(&output.name)
                && output.namespace == value.scope.claim_scope.workload_namespace,
            "review output identity or order",
        )?;
        previous = Some(&output.id);
    }
    Ok(ids)
}
fn validate_handlers<'a>(
    value: &'a ReviewMetadata,
    input_ids: &BTreeSet<&String>,
    output_ids: &BTreeSet<&String>,
) -> Result<BTreeSet<&'a String>> {
    let mut previous = None;
    let mut ids = BTreeSet::new();
    for handler in &value.handlers {
        require(
            namespace(&handler.id)
                && previous.is_none_or(|old| old < &handler.id)
                && ids.insert(&handler.id)
                && !handler.inputs.is_empty()
                && sorted(&handler.inputs)
                && handler.inputs.iter().all(|id| input_ids.contains(id))
                && handler.inputs.contains(&handler.trigger)
                && sorted(&handler.outputs)
                && handler.outputs.iter().all(|id| output_ids.contains(id)),
            "review handler links or order",
        )?;
        previous = Some(&handler.id);
    }
    Ok(ids)
}
fn validate_egress(value: &ReviewMetadata, handler_ids: &BTreeSet<&String>) -> Result<()> {
    require(sorted(&value.egress), "review egress order")?;
    for egress in &value.egress {
        require(
            name(&egress.host) && egress.port >= 1 && handler_ids.contains(&egress.handler),
            "review egress host, port or handler link",
        )?;
    }
    Ok(())
}
fn version(value: &str) -> bool {
    !value.is_empty()
        && value != "0"
        && value.len() <= 256
        && value.bytes().all(|b| b.is_ascii_graphic())
}
fn validate_inputs(
    value: &ReviewMetadata,
    resources: &BTreeMap<&String, &super::ReviewResource>,
) -> Result<()> {
    let mut previous = None;
    for input in &value.inputs {
        require(
            namespace(&input.id)
                && previous.is_none_or(|old| old < &input.id)
                && input.api_version == "v1"
                && matches!(input.kind.as_str(), "ConfigMap" | "Secret")
                && name(&input.name)
                && input.namespace == value.scope.claim_scope.workload_namespace
                && input.projection == ["data"]
                && input.missing == "block-dependent-mutations"
                && input.sensitive == (input.kind == "Secret")
                && source(&input.source),
            "review input authority or source",
        )?;
        previous = Some(&input.id);
        match input.mode {
            ReviewInputMode::Managed => {
                let resource = input
                    .managed_resource_id
                    .as_ref()
                    .and_then(|id| resources.get(id))
                    .ok_or(Error::Invalid("review managed input association"))?;
                require(
                    input.kind == "ConfigMap"
                        && resource.kind == input.kind
                        && resource.identity.name == input.name
                        && resource.identity.namespace == input.namespace,
                    "review managed input association",
                )?;
            }
            ReviewInputMode::Observed => require(
                input.managed_resource_id.is_none()
                    && !resources
                        .values()
                        .any(|r| r.kind == input.kind && r.identity.name == input.name),
                "review observed input aliases managed identity",
            )?,
            ReviewInputMode::Output => {
                let resource = input
                    .managed_resource_id
                    .as_ref()
                    .and_then(|id| resources.get(id))
                    .ok_or(Error::Invalid("review output input association"))?;
                require(
                    input.kind == "ConfigMap"
                        && resource.kind == input.kind
                        && resource.identity.name == input.name
                        && resource.identity.namespace == input.namespace,
                    "review output input association",
                )?;
            }
        }
    }
    Ok(())
}
fn validate_reactions(
    value: &ReviewMetadata,
    resources: &BTreeMap<&String, &super::ReviewResource>,
) -> Result<()> {
    let input_ids: BTreeSet<_> = value.inputs.iter().map(|input| &input.id).collect();
    let mut previous = None;
    for reaction in &value.reactions {
        require(
            namespace(&reaction.id)
                && previous.is_none_or(|old| old < &reaction.id)
                && !reaction.input_ids.is_empty()
                && reaction.input_ids.len() <= MAX_REVIEW_DECLARATIONS
                && sorted(&reaction.input_ids)
                && reaction.input_ids.iter().all(|id| input_ids.contains(id))
                && resources
                    .get(&reaction.target_resource_id)
                    .is_some_and(|target| target.kind == "Deployment")
                && reaction.effect_fields == [REVIEW_ROLLOUT_FIELD]
                && source(&reaction.source),
            "review reaction links, effect or source",
        )?;
        previous = Some(&reaction.id);
    }
    Ok(())
}
