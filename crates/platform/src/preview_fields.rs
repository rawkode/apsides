//! Pure, bounded projection comparison.
//!
//! Callers must authenticate metadata and bind both objects to their
//! observation/dry-run interval before using evidence. This module neither
//! performs SSA nor establishes transport or ownership authority.
use crate::{Result, invalid, preview_limits};
use coordination::{REVIEW_ROLLOUT_FIELD, ReviewIdentity, ReviewMetadata, ReviewResource};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_PREVIEW_OBJECT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_PREVIEW_PATHS: usize = 100_000;
pub const MAX_PREVIEW_PATH_BYTES: usize = 1024 * 1024;
pub const MAX_PREVIEW_FIELD_REPORT_BYTES: usize = 8 * 1024 * 1024;

/// Sensitive transient objects; deliberately not serializable or debuggable.
#[derive(Clone, Copy)]
pub struct PreviewObjects<'a> {
    pub before: Option<&'a Value>,
    pub projected: &'a Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProjectionChange {
    Added,
    Removed,
    Changed,
    Unchanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CoarseDifference {
    Changed,
    Unchanged,
    Unknown,
}

/// Opaque serialized field-set evidence, not semantic SSA ownership changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldSetEvidence {
    Unchanged,
    SerializedFieldSetChanged,
    Unknown,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewFieldChange {
    pub path: String,
    pub change: ProjectionChange,
    pub runtime_contribution: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewFieldImpact {
    pub identity: ReviewIdentity,
    pub fields: Vec<PreviewFieldChange>,
    /// The next target apply depends on runtime inputs; this is no value prediction.
    pub runtime_conditional: bool,
    pub undeclared_server_difference: CoarseDifference,
    pub ownership_field_set_evidence: FieldSetEvidence,
}

/// Compare only authenticated declaration projection paths and the fixed rollout
/// contribution. Arrays are aggregate values, never positional ownership paths.
///
/// # Errors
/// Rejects invalid metadata/context, mismatched objects, paths crossing arrays,
/// or explicit object/path/output limits. Authentication is the caller's duty.
pub fn classify_preview_fields(
    previous: Option<&ReviewMetadata>,
    next: &ReviewMetadata,
    identity: &ReviewIdentity,
    objects: PreviewObjects<'_>,
) -> Result<PreviewFieldImpact> {
    next.validate()
        .map_err(|_| invalid("invalid preview metadata"))?;
    if let Some(old) = previous {
        old.validate()
            .map_err(|_| invalid("invalid preview metadata"))?;
        if old.scope.claim_scope != next.scope.claim_scope
            || old.scope.domain_id != next.scope.domain_id
            || old.scope.sequence >= next.scope.sequence
        {
            return Err(invalid("preview metadata scope mismatch"));
        }
    }
    let resource = next
        .resources
        .iter()
        .find(|item| &item.identity == identity)
        .ok_or_else(|| invalid("preview requires a proposed resource"))?;
    let old = previous.and_then(|metadata| {
        metadata
            .resources
            .iter()
            .find(|item| &item.identity == identity)
    });
    if old.is_some_and(|old| old.api_version != resource.api_version || old.kind != resource.kind) {
        return Err(invalid("preview requires one exact resource API"));
    }
    validate_object(objects.projected, resource)?;
    if let Some(before) = objects.before {
        validate_object(before, resource)?;
    }
    let mut paths = BTreeSet::new();
    paths.extend(resource.apply_fields.iter().map(String::as_str));
    if let Some(old) = old {
        paths.extend(old.apply_fields.iter().map(String::as_str));
    }
    let runtime_conditional = has_reaction(next, resource);
    let old_runtime = previous
        .zip(old)
        .is_some_and(|(metadata, item)| has_reaction(metadata, item));
    if runtime_conditional || old_runtime {
        paths.insert(REVIEW_ROLLOUT_FIELD);
    }
    classify_paths(
        identity,
        &resource.api_version,
        paths,
        runtime_conditional,
        objects,
    )
}

fn has_reaction(metadata: &ReviewMetadata, resource: &ReviewResource) -> bool {
    metadata
        .reactions
        .iter()
        .any(|item| item.target_resource_id == resource.id)
}

fn validate_object(value: &Value, resource: &ReviewResource) -> Result<()> {
    preview_limits::validate_json(value, MAX_PREVIEW_OBJECT_BYTES)?;
    if value.get("apiVersion").and_then(Value::as_str) != Some(&resource.api_version)
        || value.get("kind").and_then(Value::as_str) != Some(&resource.kind)
        || value
            .get("metadata")
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            != Some(&resource.identity.name)
        || value
            .get("metadata")
            .and_then(|v| v.get("namespace"))
            .and_then(Value::as_str)
            != Some(&resource.identity.namespace)
    {
        return Err(invalid("preview object identity mismatch"));
    }
    Ok(())
}

fn classify_paths(
    identity: &ReviewIdentity,
    api_version: &str,
    paths: BTreeSet<&str>,
    runtime_conditional: bool,
    objects: PreviewObjects<'_>,
) -> Result<PreviewFieldImpact> {
    let bytes = paths
        .iter()
        .try_fold(0_usize, |total, path| total.checked_add(path.len()))
        .ok_or_else(|| invalid("preview path limit exceeded"))?;
    if paths.len() > MAX_PREVIEW_PATHS || bytes > MAX_PREVIEW_PATH_BYTES {
        return Err(invalid("preview path limit exceeded"));
    }
    let mut ignored = Ignore::default();
    let mut fields = Vec::with_capacity(paths.len());
    for path in paths {
        let tokens = tokens(path)?;
        let before = lookup(objects.before, &tokens)?;
        let after = lookup(Some(objects.projected), &tokens)?;
        fields.push(PreviewFieldChange {
            path: path.to_owned(),
            change: change(before, after),
            runtime_contribution: path == REVIEW_ROLLOUT_FIELD,
        });
        ignored.insert(&tokens);
    }
    for path in [
        "/metadata/uid",
        "/metadata/resourceVersion",
        "/metadata/generation",
        "/metadata/creationTimestamp",
        "/metadata/managedFields",
        "/metadata/selfLink",
        "/status",
    ] {
        ignored.insert(&tokens(path)?);
    }
    let result = PreviewFieldImpact {
        identity: identity.clone(),
        fields,
        runtime_conditional,
        undeclared_server_difference: equal_except(
            objects.before,
            Some(objects.projected),
            &ignored,
        ),
        ownership_field_set_evidence: field_sets(objects.before, objects.projected, api_version),
    };
    preview_limits::validate_serialized(&result, MAX_PREVIEW_FIELD_REPORT_BYTES)?;
    Ok(result)
}

fn tokens(path: &str) -> Result<Vec<String>> {
    if !path.starts_with('/') {
        return Err(invalid("invalid preview projection path"));
    }
    let parts: Vec<_> = path[1..].split('/').take(65).collect();
    if parts.len() > 64 {
        return Err(invalid("preview projection path exceeds depth limit"));
    }
    parts
        .into_iter()
        .map(|part| {
            let mut output = String::new();
            let mut chars = part.chars();
            while let Some(value) = chars.next() {
                if value != '~' {
                    output.push(value);
                    continue;
                }
                match chars.next() {
                    Some('0') => output.push('~'),
                    Some('1') => output.push('/'),
                    _ => return Err(invalid("invalid preview projection path")),
                }
            }
            Ok(output)
        })
        .collect()
}

fn lookup<'a>(mut value: Option<&'a Value>, tokens: &[String]) -> Result<Option<&'a Value>> {
    for token in tokens {
        value = match value {
            Some(Value::Array(_)) => {
                return Err(invalid("preview projection path crosses an array"));
            }
            Some(Value::Object(values)) => values.get(token),
            _ => None,
        };
    }
    Ok(value)
}

fn change(before: Option<&Value>, after: Option<&Value>) -> ProjectionChange {
    match (before, after) {
        (None, Some(_)) => ProjectionChange::Added,
        (Some(_), None) => ProjectionChange::Removed,
        (Some(a), Some(b)) if a != b => ProjectionChange::Changed,
        _ => ProjectionChange::Unchanged,
    }
}

#[derive(Default)]
struct Ignore {
    whole: bool,
    children: BTreeMap<String, Self>,
}
impl Ignore {
    fn insert(&mut self, path: &[String]) {
        let Some((first, rest)) = path.split_first() else {
            self.whole = true;
            return;
        };
        self.children.entry(first.clone()).or_default().insert(rest);
    }
}

fn equal_except(
    before: Option<&Value>,
    after: Option<&Value>,
    ignored: &Ignore,
) -> CoarseDifference {
    if ignored.whole || before == after {
        return CoarseDifference::Unchanged;
    }
    if ignored.children.is_empty() {
        return CoarseDifference::Changed;
    }
    let (Some(Value::Object(left)), Some(Value::Object(right))) = (before, after) else {
        return CoarseDifference::Unknown;
    };
    let empty = Ignore::default();
    let mut result = CoarseDifference::Unchanged;
    for key in left
        .keys()
        .chain(right.keys().filter(|key| !left.contains_key(*key)))
    {
        let child = equal_except(
            left.get(key),
            right.get(key),
            ignored.children.get(key).map_or(&empty, |value| value),
        );
        if child == CoarseDifference::Changed {
            return child;
        }
        if child == CoarseDifference::Unknown {
            result = child;
        }
    }
    result
}

fn field_sets(before: Option<&Value>, after: &Value, api: &str) -> FieldSetEvidence {
    let Some(before) = before else {
        return FieldSetEvidence::Unknown;
    };
    match (
        applicable_fields(before, api),
        applicable_fields(after, api),
    ) {
        (Some(left), Some(right)) if left == right => FieldSetEvidence::Unchanged,
        (Some(_), Some(_)) => FieldSetEvidence::SerializedFieldSetChanged,
        _ => FieldSetEvidence::Unknown,
    }
}

fn applicable_fields<'a>(value: &'a Value, api: &str) -> Option<BTreeMap<&'a str, &'a Value>> {
    let entries = value.get("metadata")?.get("managedFields")?.as_array()?;
    let mut fields = BTreeMap::new();
    for entry in entries {
        let object = entry.as_object()?;
        if object.keys().any(|key| {
            ![
                "manager",
                "operation",
                "apiVersion",
                "time",
                "fieldsType",
                "fieldsV1",
                "subresource",
            ]
            .contains(&key.as_str())
        }) {
            return None;
        }
        if entry.get("operation")?.as_str()? != "Apply" {
            return None;
        }
        if entry.get("apiVersion")?.as_str()? != api {
            return None;
        }
        if entry
            .get("subresource")
            .is_some_and(|v| v.as_str() != Some(""))
        {
            return None;
        }
        if entry.get("fieldsType")?.as_str()? != "FieldsV1" {
            return None;
        }
        let manager = entry.get("manager")?.as_str()?;
        let tree = entry.get("fieldsV1")?;
        if manager.is_empty() || !field_tree(tree) || fields.insert(manager, tree).is_some() {
            return None;
        }
    }
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

fn field_tree(value: &Value) -> bool {
    let Some(entries) = value.as_object() else {
        return false;
    };
    entries.iter().all(|(key, child)| {
        if key == "." {
            return child.as_object().is_some_and(serde_json::Map::is_empty);
        }
        valid_field_key(key) && field_tree(child)
    })
}

// Validate encoding only. Selectors remain opaque: no SSA ownership evaluation,
// selector expansion, or path generation is performed here.
fn valid_field_key(key: &str) -> bool {
    if let Some(field) = key.strip_prefix("f:") {
        return !field.is_empty();
    }
    if let Some(index) = key.strip_prefix("i:") {
        return index
            .parse::<u32>()
            .is_ok_and(|value| value.to_string() == index);
    }
    let (encoded, object) = if let Some(encoded) = key.strip_prefix("k:") {
        (encoded, true)
    } else if let Some(encoded) = key.strip_prefix("v:") {
        (encoded, false)
    } else {
        return false;
    };
    // This is a conservative syntactic-evidence profile, not a Kubernetes limit.
    if encoded.len() > 262_144 {
        return false;
    }
    let Ok(value) = compiler_core::parse_strict(encoded) else {
        return false;
    };
    (!object || value.is_object()) && preview_limits::validate_json(&value, 262_144).is_ok()
}

#[cfg(test)]
#[path = "preview_fields_tests.rs"]
mod tests;
