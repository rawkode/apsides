use crate::{
    Result,
    manifest::{MAX_DOCUMENT, ReleaseManifest, bounded_read, contained, json},
    reject,
};
use compiler_core::{CompiledPlan, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceResult {
    pub status: String,
    pub evidence_files: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaseResult {
    pub case_digest: String,
    pub result: EvidenceResult,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoverageReport {
    pub schema_version: String,
    pub ir_digest: String,
    pub cases: Vec<CaseResult>,
    pub checks: BTreeMap<String, EvidenceResult>,
}

pub const REQUIRED_CHECKS: &[&str] = &[
    "reproducible-payload",
    "declared-input-isolation",
    "offline-reactor-suite",
    "secret-canary-scan",
    "reaction-mutation-check",
    "scope-matched-benchmark",
    "deployment-ceiling-rejection",
    "ownership-collision",
    "permission-changing-upgrade",
    "phase-interruption-recovery",
    "last-api-type-cleanup",
    "uid-replacement-rejection",
    "finalizer-preservation",
    "uncertain-writer-blocks",
    "fixture-promotion-rejection",
    "api-bound-gap-conformance",
];
fn passed(result: &EvidenceResult, manifest: &ReleaseManifest) -> Result<()> {
    if result.status != "passed"
        || result.evidence_files.is_empty()
        || result.evidence_files.len() > 16
    {
        return reject("required release evidence is failed, unsupported, or absent");
    }
    for name in &result.evidence_files {
        if name == "coverage.json" || !manifest.files.contains_key(name) {
            return reject("coverage evidence is not a separately digest-bound release file");
        }
    }
    Ok(())
}
pub(crate) fn validate(root: &Path, manifest: &ReleaseManifest, plan: &CompiledPlan) -> Result<()> {
    let report: CoverageReport = serde_json::from_value(json(&bounded_read(
        &contained(root, "coverage.json")?,
        MAX_DOCUMENT,
    )?)?)?;
    if report.schema_version != "reakt.dev/release-coverage/v1"
        || report.ir_digest != plan.ir_digest
    {
        return reject("coverage is not bound to the approved production IR");
    }
    if report
        .checks
        .keys()
        .any(|name| !REQUIRED_CHECKS.contains(&name.as_str()))
    {
        return reject("coverage contains an unsupported release check");
    }
    let mut expected = BTreeMap::new();
    for case in plan.test_plan.iter().filter(|case| case.required) {
        expected.insert(digest(&serde_json::to_value(case)?)?, case);
    }
    let mut actual = BTreeSet::new();
    for case in &report.cases {
        if !actual.insert(case.case_digest.clone()) {
            return reject("duplicate release coverage case");
        }
        let expected_case = expected.get(&case.case_digest).ok_or_else(|| {
            crate::Error::Rejected("release coverage contains an unexpected reactor case".into())
        })?;
        passed(&case.result, manifest)?;
        for file in &case.result.evidence_files {
            crate::runner_evidence::validate_record(
                root,
                manifest,
                file,
                &case.case_digest,
                true,
                Some(&expected_case.scenario),
            )?;
        }
    }
    if actual != expected.keys().cloned().collect() {
        return reject(
            "release coverage does not exactly cover the compiler's required reactor cases",
        );
    }
    for name in REQUIRED_CHECKS {
        let Some(result) = report.checks.get(*name) else {
            return reject(format!("missing release check: {name}"));
        };
        passed(result, manifest)?;
        for file in &result.evidence_files {
            if crate::production_evidence::fixture_check(name) {
                crate::runner_evidence::validate_record(root, manifest, file, name, false, None)?;
            } else {
                crate::production_evidence::validate_record(root, manifest, file, name)?;
            }
        }
    }
    Ok(())
}
