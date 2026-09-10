//! Runtime-conditional preview composition.
//!
//! No input values or rollout keys are read, and no future contribution is
//! calculated. Callers authenticate the metadata, canonical desired object and
//! observation interval.
use crate::{Result, invalid, preview_limits};
use compiler_core::REVISION_ANNOTATION;
use coordination::{ReviewIdentity, ReviewMetadataV1, ReviewResource};
use serde_json::{Map, Value};

const MAX_OBJECT_BYTES: usize = 4 * 1024 * 1024;
const ANNOTATION_PARENT: [&str; 4] = ["spec", "template", "metadata", "annotations"];
const FIELD_PATH: [&str; 5] = [
    "f:spec",
    "f:template",
    "f:metadata",
    "f:annotations",
    "f:apsides.dev/revision",
];

/// Sensitive borrowed observation; expected UID comes from authenticated capture.
pub struct PreviewLiveObject<'a> {
    pub object: &'a Value,
    pub expected_uid: &'a str,
}

/// Sensitive in-memory request and preservation contract. Never serialize this
/// carrier or log its payload; only send it through the bounded dry-run transport.
pub struct PreviewComposition {
    payload: Value,
    resource: ReviewResource,
    expected_uid: Option<String>,
    runtime_conditional: bool,
}

impl PreviewComposition {
    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }
    #[must_use]
    pub const fn runtime_conditional(&self) -> bool {
        self.runtime_conditional
    }

    /// Check the bounded dry-run result against the observation identity and any
    /// promised runtime contribution, including promised absence.
    /// # Errors
    /// Rejects identity changes, malformed objects, or admission changes to a
    /// preserved contribution. This is not a future runtime execution guarantee.
    pub fn verify_projected(&self, projected: &Value) -> Result<()> {
        validate_object(projected, &self.resource, MAX_OBJECT_BYTES)?;
        if let Some(uid) = &self.expected_uid {
            validate_uid(projected, uid)?;
        }
        if self.runtime_conditional && annotation(projected)? != annotation(&self.payload)? {
            return Err(invalid(
                "preview admission changed the conditional runtime contribution",
            ));
        }
        Ok(())
    }
}

/// Compose a canonical desired object with an existing, exactly owned runtime
/// contribution, without reading inputs or predicting a new revision.
/// # Errors
/// Rejects mismatched metadata/objects, invalid UIDs, author-supplied reserved
/// annotations, malformed or foreign contributions, and explicit size limits.
pub fn compose_preview(
    metadata: &ReviewMetadataV1,
    identity: &ReviewIdentity,
    desired: &Value,
    before: Option<PreviewLiveObject<'_>>,
) -> Result<PreviewComposition> {
    metadata
        .validate()
        .map_err(|_| invalid("invalid preview composition metadata"))?;
    let resource = metadata
        .resources
        .iter()
        .find(|item| &item.identity == identity)
        .ok_or_else(|| invalid("preview composition target is not declared"))?;
    validate_object(
        desired,
        resource,
        metadata.compatibility.runtime_budgets.max_object_bytes,
    )?;
    if annotation(desired)?.is_some() {
        return Err(invalid(
            "canonical preview desired object contains a reserved contribution",
        ));
    }
    if let Some(live) = &before {
        validate_object(live.object, resource, MAX_OBJECT_BYTES)?;
        validate_uid(live.object, live.expected_uid)?;
    }
    let conditional = metadata
        .reactions
        .iter()
        .any(|item| item.target_resource_id == resource.id);
    let contribution = if conditional {
        match before
            .as_ref()
            .map(|live| annotation(live.object))
            .transpose()?
            .flatten()
        {
            Some(value) => {
                let live = before
                    .as_ref()
                    .ok_or_else(|| invalid("preview observation is missing"))?;
                validate_revision(value)?;
                validate_ownership(live.object, resource, &metadata.scope.domain_id)?;
                Some(value)
            }
            None => None,
        }
    } else {
        None
    };
    let mut payload = desired.clone();
    if let Some(value) = contribution {
        insert_annotation(&mut payload, value.clone())?;
    }
    preview_limits::validate_json(&payload, MAX_OBJECT_BYTES)?;
    Ok(PreviewComposition {
        payload,
        resource: resource.clone(),
        expected_uid: before.map(|live| live.expected_uid.to_owned()),
        runtime_conditional: conditional,
    })
}

fn validate_object(value: &Value, resource: &ReviewResource, limit: usize) -> Result<()> {
    preview_limits::validate_json(value, limit)?;
    if value.get("apiVersion").and_then(Value::as_str) != Some(resource.api_version.as_str())
        || value.get("kind").and_then(Value::as_str) != Some(resource.kind.as_str())
        || value
            .get("metadata")
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            != Some(resource.identity.name.as_str())
        || value
            .get("metadata")
            .and_then(|v| v.get("namespace"))
            .and_then(Value::as_str)
            != Some(resource.identity.namespace.as_str())
    {
        return Err(invalid("preview composition object identity mismatch"));
    }
    Ok(())
}

fn validate_uid(value: &Value, expected: &str) -> Result<()> {
    if expected.is_empty()
        || expected.len() > 1024
        || !expected.bytes().all(|b| b.is_ascii_graphic())
        || value
            .get("metadata")
            .and_then(|v| v.get("uid"))
            .and_then(Value::as_str)
            != Some(expected)
    {
        return Err(invalid("preview composition object UID mismatch"));
    }
    Ok(())
}

fn annotation(value: &Value) -> Result<Option<&Value>> {
    let mut current = value;
    for key in ANNOTATION_PARENT {
        let object = current
            .as_object()
            .ok_or_else(|| invalid("preview contribution structure is invalid"))?;
        let Some(next) = object.get(key) else {
            return Ok(None);
        };
        current = next;
    }
    current
        .as_object()
        .map(|map| map.get(REVISION_ANNOTATION))
        .ok_or_else(|| invalid("preview contribution annotations are invalid"))
}

fn insert_annotation(value: &mut Value, contribution: Value) -> Result<()> {
    let mut current = value;
    for key in ANNOTATION_PARENT {
        let object = current
            .as_object_mut()
            .ok_or_else(|| invalid("preview contribution structure is invalid"))?;
        current = object
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .ok_or_else(|| invalid("preview contribution annotations are invalid"))?
        .insert(REVISION_ANNOTATION.into(), contribution);
    Ok(())
}

fn validate_revision(value: &Value) -> Result<()> {
    let value = value
        .as_str()
        .ok_or_else(|| invalid("preview runtime contribution is malformed"))?;
    let digest = if let Some(digest) = value.strip_prefix("sha256:") {
        digest
    } else if let Some(rest) = value.strip_prefix("hmac-sha256:") {
        let (key, digest) = rest
            .split_once(':')
            .ok_or_else(|| invalid("preview runtime contribution is malformed"))?;
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid("preview runtime contribution is malformed"));
        }
        digest
    } else {
        return Err(invalid("preview runtime contribution is malformed"));
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("preview runtime contribution is malformed"));
    }
    Ok(())
}

fn validate_ownership(value: &Value, resource: &ReviewResource, domain: &str) -> Result<()> {
    let entries = value
        .get("metadata")
        .and_then(|v| v.get("managedFields"))
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("preview runtime contribution ownership is unavailable"))?;
    let manager = format!("apsides-{domain}");
    let mut owned = false;
    for entry in entries {
        let entry_manager = entry
            .get("manager")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("preview runtime contribution ownership is malformed"))?;
        match entry.get("subresource") {
            None => {}
            Some(Value::String(value)) if value.is_empty() => {}
            Some(Value::String(_)) => continue,
            Some(_) => {
                return Err(invalid(
                    "preview runtime contribution ownership is malformed",
                ));
            }
        }
        if entry.get("fieldsType").and_then(Value::as_str) != Some("FieldsV1") {
            return Err(invalid(
                "preview runtime contribution ownership is malformed",
            ));
        }
        let fields = entry
            .get("fieldsV1")
            .ok_or_else(|| invalid("preview runtime contribution ownership is malformed"))?;
        let touches = touches_contribution(fields)?;
        if entry_manager != manager {
            if touches {
                return Err(invalid(
                    "preview runtime contribution has foreign ownership",
                ));
            }
            continue;
        }
        if owned
            || entry.get("operation").and_then(Value::as_str) != Some("Apply")
            || entry.get("apiVersion").and_then(Value::as_str)
                != Some(resource.api_version.as_str())
            || entry.get("fieldsType").and_then(Value::as_str) != Some("FieldsV1")
            || !exact_contribution(fields)
        {
            return Err(invalid(
                "preview runtime contribution does not have exact Apply ownership",
            ));
        }
        owned = true;
    }
    if !owned {
        return Err(invalid(
            "preview runtime contribution ownership is unavailable",
        ));
    }
    Ok(())
}

fn touches_contribution(mut value: &Value) -> Result<bool> {
    for key in FIELD_PATH {
        let fields = value
            .as_object()
            .ok_or_else(|| invalid("preview runtime contribution field set is malformed"))?;
        if fields.is_empty() || fields.contains_key(".") {
            return Ok(true);
        }
        let Some(next) = fields.get(key) else {
            return Ok(false);
        };
        value = next;
    }
    Ok(true)
}

fn exact_contribution(mut value: &Value) -> bool {
    for key in FIELD_PATH {
        let Some(next) = value.as_object().and_then(|fields| fields.get(key)) else {
            return false;
        };
        value = next;
    }
    value.as_object().is_some_and(Map::is_empty)
}

#[cfg(test)]
#[path = "preview_composition_tests.rs"]
mod tests;
