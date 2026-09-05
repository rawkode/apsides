use crate::{
    Result,
    manifest::{
        MAX_DOCUMENT, ReleaseManifest, bounded_read, contained, is_digest, json, raw_digest,
    },
    reject,
};
use compiler_core::digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Assertion {
    pub check_id: String,
    pub expected: Value,
    pub actual: Value,
    pub passed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceRecord {
    pub status: String,
    pub assertions: Vec<Assertion>,
    pub evidence_files: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerIdentity {
    pub source_sha256: String,
    pub controller_manifest_digest: String,
    pub controller_archive_digest: String,
    pub fixture_archive_digest: String,
    pub node_image: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReactorEvidence {
    pub schema_version: String,
    pub production_ir_digest: String,
    pub fixture_ir_digest: String,
    pub tested_artifact_digest: String,
    pub artifact_manifest_file: String,
    pub overlay_digest: String,
    pub runner_identity: RunnerIdentity,
    pub cases: BTreeMap<String, EvidenceRecord>,
    pub checks: BTreeMap<String, EvidenceRecord>,
    #[serde(default)]
    pub fixture_cases: BTreeMap<String, EvidenceRecord>,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}
fn scalar(value: &Value) -> bool {
    match value {
        Value::Bool(_) | Value::Number(_) => true,
        Value::String(s) => s.len() <= 1024,
        _ => false,
    }
}
fn record(record: &EvidenceRecord, release: &ReleaseManifest) -> Result<()> {
    if record.status != "passed"
        || record.assertions.is_empty()
        || record.assertions.len() > 128
        || record.evidence_files.len() > 128
    {
        return reject("runner evidence is incomplete, failed or unsupported");
    }
    let mut ids = std::collections::BTreeSet::new();
    for assertion in &record.assertions {
        if !valid_id(&assertion.check_id)
            || !ids.insert(&assertion.check_id)
            || !assertion.passed
            || !scalar(&assertion.expected)
            || !scalar(&assertion.actual)
        {
            return reject(
                "runner assertion is invalid, duplicated or does not match its expected outcome",
            );
        }
    }
    for name in &record.evidence_files {
        if !release.files.contains_key(name) {
            return reject("runner diagnostic is not release-digest-bound");
        }
    }
    Ok(())
}

pub(crate) fn validate_record(
    root: &Path,
    release: &ReleaseManifest,
    file: &str,
    key: &str,
    case: bool,
    scenario: Option<&str>,
) -> Result<()> {
    if !release.files.contains_key(file) {
        return reject("runner evidence is not release-digest-bound");
    }
    let evidence: ReactorEvidence =
        serde_json::from_value(json(&bounded_read(&contained(root, file)?, MAX_DOCUMENT)?)?)?;
    if evidence.schema_version != "reakt.dev/reactor-evidence/v1"
        || evidence.production_ir_digest != release.ir_digest
    {
        return reject("runner evidence refers to another production IR");
    }
    for digest in [
        &evidence.fixture_ir_digest,
        &evidence.tested_artifact_digest,
        &evidence.overlay_digest,
        &evidence.runner_identity.source_sha256,
        &evidence.runner_identity.controller_manifest_digest,
        &evidence.runner_identity.controller_archive_digest,
        &evidence.runner_identity.fixture_archive_digest,
    ] {
        if !is_digest(digest) {
            return reject("runner evidence has invalid identity digests");
        }
    }
    if !evidence
        .runner_identity
        .node_image
        .rsplit_once('@')
        .is_some_and(|(name, d)| !name.is_empty() && is_digest(d))
    {
        return reject("runner Kubernetes node image is not immutable");
    }
    let fixture = json(&bounded_read(
        &contained(root, "fixture-ir.json")?,
        MAX_DOCUMENT,
    )?)?;
    let overlay = json(&bounded_read(
        &contained(root, "fixture-overlay.json")?,
        MAX_DOCUMENT,
    )?)?;
    let production = json(&bounded_read(&contained(root, "ir.json")?, MAX_DOCUMENT)?)?;
    validate_fixture_substitution(&production, &fixture)?;
    if fixture["testOnly"] != true
        || digest(&fixture)? != evidence.fixture_ir_digest
        || digest(&overlay)? != evidence.overlay_digest
    {
        return reject("runner fixture IR or overlay differs from the release fixture contract");
    }
    if !release.files.contains_key(&evidence.artifact_manifest_file) {
        return reject("tested artifact manifest is not digest-bound");
    }
    let artifact = bounded_read(
        &contained(root, &evidence.artifact_manifest_file)?,
        MAX_DOCUMENT,
    )?;
    if raw_digest(&artifact) != evidence.tested_artifact_digest
        || json(&artifact)?["irDigest"] != evidence.fixture_ir_digest
    {
        return reject("runner did not test the declared fixture artifact");
    }
    if evidence.cases.len() > 8192
        || evidence.checks.len() > 128
        || evidence.fixture_cases.len() > 8192
    {
        return reject("runner evidence catalogue exceeds budget");
    }
    let records = if case {
        &evidence.cases
    } else {
        &evidence.checks
    };
    let result = records.get(key).ok_or_else(|| {
        crate::Error::Rejected("runner evidence lacks the required case/check".into())
    })?;
    record(result, release)?;
    crate::validate_assertions(key, case, scenario, &result.assertions)
}

fn validate_fixture_substitution(production: &Value, fixture: &Value) -> Result<()> {
    let mut expected = production.clone();
    expected
        .as_object_mut()
        .ok_or_else(|| crate::Error::Rejected("production IR is not an object".into()))?
        .insert("testOnly".into(), Value::Bool(true));
    for resource in expected
        .get_mut("resources")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::Error::Rejected("fixture resources missing".into()))?
    {
        if resource["kind"] == "Deployment" {
            let containers = resource
                .pointer_mut("/desired/spec/template/spec/containers")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| crate::Error::Rejected("fixture containers missing".into()))?;
            for container in containers {
                let container = container.as_object_mut().ok_or_else(|| {
                    crate::Error::Rejected("fixture container is not an object".into())
                })?;
                container.insert("image".into(), Value::String("reakt-fixture:local".into()));
                container.insert("imagePullPolicy".into(), Value::String("Never".into()));
            }
        }
    }
    if &expected != fixture {
        return reject("fixture substitution changes the approved reaction contract");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_fixture_substitution;
    use serde_json::json;

    #[test]
    fn malformed_fixture_structures_return_errors() {
        for production in [
            json!(null),
            json!([]),
            json!({"resources":{}}),
            json!({"resources":[{"kind":"Deployment","desired":{"spec":[]}}]}),
            json!({"resources":[{"kind":"Deployment","desired":{"spec":{"template":{"spec":{"containers":[null]}}}}}]}),
        ] {
            assert!(validate_fixture_substitution(&production, &json!({})).is_err());
        }
    }
}
