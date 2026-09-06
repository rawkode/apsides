//! Partial assembled reactor consumer check, not signature verification or release promotion.
use crate::{
    ReactorEvidence, Result,
    manifest::{MAX_DOCUMENT, bounded_read, contained, json},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const EVIDENCE: &str = "evidence/reactor-evidence.json";
const CHECKS: [&str; 3] = [
    "offline-reactor-suite",
    "secret-canary-scan",
    "reaction-mutation-check",
];

#[test]
#[ignore = "requires APS_REACTOR_EVIDENCE_RELEASE selecting an assembled release"]
fn assembled_reactor_evidence_matches_production_contract() -> Result<()> {
    let root = PathBuf::from(std::env::var_os("APS_REACTOR_EVIDENCE_RELEASE").ok_or_else(
        || crate::Error::Rejected("reactor consumer requires an explicit release path".into()),
    )?);
    if !root.is_absolute() || !root.is_dir() {
        return crate::reject("reactor consumer requires an existing absolute release directory");
    }
    // Validate real native/OCI payload correspondence and canonical production compilation.
    // Pending coverage is intentionally not promoted or converted to VerifiedRelease.
    let (manifest, production, _) = crate::read_release(&root)?;
    if manifest.mode != crate::manifest::ReleaseMode::Production
        || !manifest.files.contains_key(EVIDENCE)
        || !manifest.files.contains_key("fixture-ir.json")
    {
        return crate::reject("reactor consumer requires bound production and fixture evidence");
    }
    validate_fixture_plan(&root, &production)?;
    let evidence: ReactorEvidence = serde_json::from_value(json(&bounded_read(
        &contained(&root, EVIDENCE)?,
        MAX_DOCUMENT,
    )?)?)?;
    let mut expected = BTreeMap::new();
    for case in production.test_plan.iter().filter(|case| case.required) {
        let key = compiler_core::digest(&serde_json::to_value(case)?)?;
        if expected.insert(key, case).is_some() {
            return crate::reject("production reactor case identity is duplicated");
        }
    }
    let actual: BTreeSet<_> = evidence.cases.keys().collect();
    if actual != expected.keys().collect()
        || evidence
            .checks
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != CHECKS.into_iter().collect()
    {
        return crate::reject(
            "assembled reactor evidence does not exactly cover the required catalogue",
        );
    }
    for (key, case) in expected {
        crate::runner_evidence::validate_record(
            &root,
            &manifest,
            EVIDENCE,
            &key,
            true,
            Some(&case.scenario),
        )?;
    }
    for check in CHECKS {
        crate::runner_evidence::validate_record(&root, &manifest, EVIDENCE, check, false, None)?;
    }
    // Supplemental fixtureCases are not separately accepted here; the three check
    // contracts retain their existing scope and assertion validation.
    Ok(())
}

fn validate_fixture_plan(root: &Path, production: &compiler_core::CompiledPlan) -> Result<()> {
    let fixture_bytes = bounded_read(&contained(root, "fixture-ir.json")?, MAX_DOCUMENT)?;
    let fixture_text = std::str::from_utf8(&fixture_bytes)
        .map_err(|_| crate::Error::Rejected("fixture IR is not UTF-8".into()))?;
    let fixture = compiler_core::parse_and_validate(fixture_text)?;
    // TestCase contains no IR digest. Allowed image/testOnly substitution must
    // preserve exact scenario, source, input, force and applicability metadata.
    if !fixture.ir.test_only || fixture.test_plan != production.test_plan {
        return crate::reject("fixture and production reactor case contracts differ");
    }
    Ok(())
}
