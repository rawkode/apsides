use crate::{
    EvidenceRecord, Result,
    manifest::{
        Image, MAX_DOCUMENT, ReleaseManifest, ReleaseMode, bounded_read, contained, json,
        raw_digest,
    },
    reject,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Signer-attested measurements of the exact production payload, not a fixture.
/// The enclosing release signature authenticates these assertions; this record
/// alone is neither a signature nor proof of runner execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductionEvidence {
    pub schema_version: String,
    pub ir_digest: String,
    pub compiler_digest: String,
    pub binary_digest: String,
    pub image: Image,
    pub source_inputs_digest: String,
    pub payload_files: BTreeMap<String, String>,
    pub runner_source_file: String,
    pub runner_source_digest: String,
    pub checks: BTreeMap<String, EvidenceRecord>,
}

pub(crate) fn fixture_check(key: &str) -> bool {
    matches!(
        key,
        "offline-reactor-suite" | "secret-canary-scan" | "reaction-mutation-check"
    )
}

pub(crate) fn validate_record(
    root: &Path,
    release: &ReleaseManifest,
    file: &str,
    key: &str,
) -> Result<()> {
    if !release.files.contains_key(file) {
        return reject("production evidence is not release-digest-bound");
    }
    let evidence: ProductionEvidence =
        serde_json::from_value(json(&bounded_read(&contained(root, file)?, MAX_DOCUMENT)?)?)?;
    if evidence.schema_version != "reakt.dev/production-evidence/v1"
        || release.mode != ReleaseMode::Production
        || evidence.ir_digest != release.ir_digest
        || evidence.compiler_digest != release.compiler_digest
        || evidence.binary_digest != release.binary_digest
        || evidence.image.reference != release.image.reference
        || evidence.image.platform != release.image.platform
        || evidence.source_inputs_digest != release.source.inputs_digest
    {
        return reject("production evidence refers to another production payload");
    }
    let payloads = [
        "build-inputs.json",
        "controller.oci.tar",
        "reakt-controller",
    ];
    if evidence.payload_files.len() != payloads.len()
        || payloads.iter().any(|name| {
            !evidence.payload_files.contains_key(*name)
                || evidence.payload_files.get(*name) != release.files.get(*name)
        })
    {
        return reject("production evidence payload files differ from the release");
    }
    if release.files.get(&evidence.runner_source_file) != Some(&evidence.runner_source_digest)
        || raw_digest(&bounded_read(
            &contained(root, &evidence.runner_source_file)?,
            MAX_DOCUMENT,
        )?) != evidence.runner_source_digest
    {
        return reject("production evidence runner source is not authenticated");
    }
    if evidence.checks.is_empty()
        || evidence
            .checks
            .keys()
            .any(|name| fixture_check(name) || !crate::REQUIRED_CHECKS.contains(&name.as_str()))
    {
        return reject("production evidence contains an unsupported check");
    }
    // Validate every included record, even when coverage selects only one.
    for (name, record) in &evidence.checks {
        crate::runner_evidence::record(record, release)?;
        crate::validate_assertions(name, false, None, &record.assertions)?;
    }
    if !evidence.checks.contains_key(key) {
        return reject("production evidence lacks the required check");
    }
    Ok(())
}
