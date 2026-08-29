mod canonical;
pub mod generate;
pub mod limits;
pub mod model;
mod strict_json;

use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;

pub use canonical::{canonical_bytes, sha256_hex};
pub use model::{ActionInputV0, ActionOutputV0, RenderInputV0, RenderOutputV0};
pub use strict_json::parse_strict_value;

#[derive(Debug, Clone, PartialEq)]
pub struct Validated<T> {
    pub document: T,
    pub canonical_bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Debug, Error, PartialEq)]
pub enum AbiError {
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("document does not match its ABI type: {0}")]
    InvalidDocument(String),
    #[error("unsupported ABI version `{actual}`; expected `{expected}`")]
    AbiVersion {
        actual: String,
        expected: &'static str,
    },
    #[error("JSON number is not representable as finite binary64")]
    NumberNotBinary64,
    #[error("non-finite JSON numbers are forbidden")]
    NonFiniteNumber,
    #[error("negative zero is forbidden")]
    NegativeZero,
    #[error("integer-valued JSON numbers outside JavaScript's safe range must be strings")]
    UnsafeInteger,
    #[error("string is {actual} UTF-8 bytes; maximum is {maximum}")]
    StringLimit { actual: usize, maximum: usize },
    #[error("JSON depth is {actual}; maximum is {maximum}")]
    DepthLimit { actual: usize, maximum: usize },
    #[error("canonical JSON is {actual} bytes; maximum is {maximum}")]
    CanonicalBytesLimit { actual: usize, maximum: usize },
    #[error("{field} contains {actual} entries; maximum is {maximum}")]
    CardinalityLimit {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("invalid renderer contract: {0}")]
    Contract(String),
}

/// Parse, validate, canonicalize, and hash a render input document.
///
/// # Errors
///
/// Returns [`AbiError`] when JSON, limits, the typed shape, or contract rules
/// are invalid.
pub fn parse_validate_render_input(input: &str) -> Result<Validated<RenderInputV0>, AbiError> {
    parse_and_validate(input, validate_render_input)
}

/// Parse, validate, canonicalize, and hash a render output document.
///
/// # Errors
///
/// Returns [`AbiError`] when JSON, limits, the typed shape, or contract rules
/// are invalid.
pub fn parse_validate_render_output(input: &str) -> Result<Validated<RenderOutputV0>, AbiError> {
    parse_and_validate(input, validate_render_output)
}

/// Parse, validate, canonicalize, and hash an action input document.
///
/// # Errors
///
/// Returns [`AbiError`] when JSON, limits, the typed shape, or contract rules
/// are invalid.
pub fn parse_validate_action_input(input: &str) -> Result<Validated<ActionInputV0>, AbiError> {
    parse_and_validate(input, validate_action_input)
}

/// Parse, validate, canonicalize, and hash an action output document.
///
/// # Errors
///
/// Returns [`AbiError`] when JSON, limits, the typed shape, or contract rules
/// are invalid.
pub fn parse_validate_action_output(input: &str) -> Result<Validated<ActionOutputV0>, AbiError> {
    parse_and_validate(input, validate_action_output)
}

fn parse_and_validate<T>(
    input: &str,
    validate: fn(&T) -> Result<(), AbiError>,
) -> Result<Validated<T>, AbiError>
where
    T: DeserializeOwned,
{
    let value = parse_strict_value(input)?;
    limits::validate_json_limits(&value)?;
    let canonical_bytes = canonical_bytes(&value)?;
    let document = serde_json::from_value(value)
        .map_err(|error| AbiError::InvalidDocument(error.to_string()))?;
    validate(&document)?;
    let sha256 = sha256_hex(&canonical_bytes);
    Ok(Validated {
        document,
        canonical_bytes,
        sha256,
    })
}

fn validate_render_input(input: &RenderInputV0) -> Result<(), AbiError> {
    validate_version(&input.abi_version)?;
    for (field, value) in [
        ("root.apiVersion", input.root.api_version.as_str()),
        ("root.kind", input.root.kind.as_str()),
        ("root.namespace", input.root.namespace.as_str()),
        ("root.name", input.root.name.as_str()),
        ("root.uid", input.root.uid.as_str()),
        ("bundleDigest", input.bundle_digest.as_str()),
        ("cacheEpoch", input.cache_epoch.as_str()),
        ("observationToken", input.observation_token.as_str()),
    ] {
        require_nonempty(field, value)?;
    }
    Ok(())
}

fn validate_render_output(output: &RenderOutputV0) -> Result<(), AbiError> {
    use model::RenderDisposition::{Apply, ExplicitEmpty, Hold};

    validate_version(&output.abi_version)?;
    cardinality("resources", output.resources.len(), limits::MAX_RESOURCES)?;
    cardinality(
        "dependencies",
        output.dependencies.len(),
        limits::MAX_DEPENDENCIES,
    )?;
    cardinality(
        "automations",
        output.automations.len(),
        limits::MAX_AUTOMATIONS,
    )?;
    cardinality(
        "diagnostics",
        output.diagnostics.len(),
        limits::MAX_DIAGNOSTICS,
    )?;

    match output.disposition {
        Apply if output.resources.is_empty() => {
            return Err(AbiError::Contract(
                "an empty desired graph must use disposition `explicitEmpty`".into(),
            ));
        }
        ExplicitEmpty if !output.resources.is_empty() => {
            return Err(AbiError::Contract(
                "disposition `explicitEmpty` cannot contain resources".into(),
            ));
        }
        Hold if !output.resources.is_empty() || !output.automations.is_empty() => {
            return Err(AbiError::Contract(
                "disposition `hold` cannot request resources or automations".into(),
            ));
        }
        Apply | Hold | ExplicitEmpty => {}
    }

    for resource in &output.resources {
        require_nonempty("resource.logicalId", &resource.logical_id)?;
        validate_kubernetes_object(&resource.object)?;
    }
    for dependency in &output.dependencies {
        validate_dependency(dependency)?;
    }
    Ok(())
}

fn validate_dependency(dependency: &model::Dependency) -> Result<(), AbiError> {
    match dependency {
        model::Dependency::Object {
            api_version,
            kind,
            name,
            uid,
            resource_version,
            ..
        } => {
            require_nonempty("dependency.apiVersion", api_version)?;
            require_nonempty("dependency.kind", kind)?;
            require_nonempty("dependency.name", name)?;
            match (uid, resource_version) {
                (Some(uid), Some(resource_version)) => {
                    require_nonempty("dependency.uid", uid)?;
                    require_nonempty("dependency.resourceVersion", resource_version)?;
                }
                (None, None) => {}
                _ => {
                    return Err(AbiError::Contract(
                        "an object dependency must declare both uid and resourceVersion when present, or neither when absent"
                            .into(),
                    ));
                }
            }
        }
        model::Dependency::Collection {
            api_version,
            kind,
            collection_revision,
            ..
        } => {
            require_nonempty("dependency.apiVersion", api_version)?;
            require_nonempty("dependency.kind", kind)?;
            require_nonempty("dependency.collectionRevision", collection_revision)?;
        }
    }
    Ok(())
}

fn validate_action_input(input: &ActionInputV0) -> Result<(), AbiError> {
    validate_version(&input.abi_version)?;
    for (field, value) in [
        ("automationIdentity", input.automation_identity.as_str()),
        ("occurrenceId", input.occurrence_id.as_str()),
        ("sourceUid", input.source_uid.as_str()),
        (
            "sourceResourceVersion",
            input.source_resource_version.as_str(),
        ),
    ] {
        require_nonempty(field, value)?;
    }
    Ok(())
}

fn validate_action_output(output: &ActionOutputV0) -> Result<(), AbiError> {
    validate_version(&output.abi_version)?;
    cardinality("actions", output.actions.len(), limits::MAX_ACTIONS)?;
    cardinality(
        "diagnostics",
        output.diagnostics.len(),
        limits::MAX_DIAGNOSTICS,
    )?;
    for action in &output.actions {
        require_nonempty("action.logicalId", &action.logical_id)?;
        require_nonempty("action.kind", &action.kind)?;
    }
    Ok(())
}

fn validate_version(actual: &str) -> Result<(), AbiError> {
    if actual != model::ABI_VERSION_V0 {
        return Err(AbiError::AbiVersion {
            actual: actual.to_owned(),
            expected: model::ABI_VERSION_V0,
        });
    }
    Ok(())
}

fn cardinality(field: &'static str, actual: usize, maximum: usize) -> Result<(), AbiError> {
    if actual > maximum {
        return Err(AbiError::CardinalityLimit {
            field,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn require_nonempty(field: &str, value: &str) -> Result<(), AbiError> {
    if value.is_empty() {
        return Err(AbiError::Contract(format!("{field} cannot be empty")));
    }
    Ok(())
}

fn validate_kubernetes_object(value: &Value) -> Result<(), AbiError> {
    let object = value
        .as_object()
        .ok_or_else(|| AbiError::Contract("resource.object must be an object".into()))?;
    for field in ["apiVersion", "kind"] {
        let value = object
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default();
        require_nonempty(&format!("resource.object.{field}"), value)?;
    }
    let metadata = object
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| AbiError::Contract("resource.object.metadata must be an object".into()))?;
    let name = metadata
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    require_nonempty("resource.object.metadata.name", name)
}
