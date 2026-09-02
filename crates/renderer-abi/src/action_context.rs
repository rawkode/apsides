use serde::Deserialize;
use serde_json::Value;

use crate::AbiError;

pub const ACTION_CONTEXT_KEY_V1: &str = "$oxideloop";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionContextSourceV1 {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub resource_version: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionContextV1 {
    pub kind: String,
    pub version: String,
    pub source: ActionContextSourceV1,
    pub metadata: Value,
}

/// Parses the v1 authored action-context envelope when it is explicitly
/// tagged. Untagged metadata is legacy data and remains valid.
///
/// # Errors
///
/// A malformed tagged envelope is never interpreted as legacy metadata.
pub fn action_context_v1(value: &Value) -> Result<Option<ActionContextV1>, AbiError> {
    let Some(top) = value.as_object() else {
        return Ok(None);
    };
    let Some(tagged) = top.get(ACTION_CONTEXT_KEY_V1) else {
        return Ok(None);
    };
    if top.len() != 1 {
        return Err(AbiError::Contract(
            "v1 action-context metadata must contain only $oxideloop".into(),
        ));
    }
    let context: ActionContextV1 = serde_json::from_value(tagged.clone())
        .map_err(|error| AbiError::Contract(format!("invalid v1 action-context: {error}")))?;
    if context.kind != "action-context" || context.version != "v1" {
        return Err(AbiError::Contract(
            "v1 action-context kind/version is invalid".into(),
        ));
    }
    for (field, value) in [
        ("apiVersion", context.source.api_version.as_str()),
        ("kind", context.source.kind.as_str()),
        ("namespace", context.source.namespace.as_str()),
        ("name", context.source.name.as_str()),
        ("uid", context.source.uid.as_str()),
        ("resourceVersion", context.source.resource_version.as_str()),
    ] {
        if value.is_empty() {
            return Err(AbiError::Contract(format!(
                "v1 action-context source.{field} must be nonempty"
            )));
        }
    }
    Ok(Some(context))
}

#[must_use]
pub fn action_context_source_matches(
    context: &ActionContextV1,
    api_version: &str,
    kind: &str,
    namespace: &str,
    name: &str,
    uid: &str,
    resource_version: &str,
) -> bool {
    context.source.api_version == api_version
        && context.source.kind == kind
        && context.source.namespace == namespace
        && context.source.name == name
        && context.source.uid == uid
        && context.source.resource_version == resource_version
}
