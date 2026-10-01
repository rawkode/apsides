//! Selecting and reading the compiler's staged output.
//!
//! The source compiler stages both the production IR and, alongside it, the
//! synthetic fixture projection. Choosing the fixture is never implicit: it is
//! read only when it validates against the production document it claims to
//! project.

use crate::{Result, cli::Mode, ir::read_plan};
use compiler_core::parse_and_validate;
use std::{fs, io::Read as _, path::Path};

/// The largest staged compiler document the CLI will read.
const MAX_BYTES: usize = 8 * 1024 * 1024;

const fn ir_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Production => "ir.json",
        Mode::Fixture => "fixture-ir.json",
    }
}

/// Read the staged IR the requested mode selects.
///
/// # Errors
/// Returns an error if the staged documents are missing, oversized, invalid, or
/// if a requested fixture does not project the production IR beside it.
pub(crate) fn plan(stage: &Path, mode: Mode) -> Result<compiler_core::CompiledPlan> {
    if mode == Mode::Production {
        return read_plan(&stage.join(ir_name(mode)));
    }
    let production = document(&stage.join("ir.json"))?;
    let fixture = document(&stage.join(ir_name(mode)))?;
    let overlay = document(&stage.join("fixture-overlay.json"))?;
    release::validate_fixture_projection(&production, &fixture, &overlay)?;
    Ok(parse_and_validate(&serde_json::to_string(&fixture)?)?)
}

fn document(path: &Path) -> Result<serde_json::Value> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_BYTES as u64 {
        return Err("source compiler document is missing or oversized".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err("source compiler document is oversized".into());
    }
    Ok(compiler_core::parse_strict(std::str::from_utf8(&bytes)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_mode_selects_its_exact_staged_document() {
        assert_eq!(ir_name(Mode::Production), "ir.json");
        assert_eq!(ir_name(Mode::Fixture), "fixture-ir.json");
    }

    #[test]
    fn fixture_selection_requires_the_bound_compiler_projection() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let production = production_ir();
        let mut fixture = production.clone();
        fixture["testOnly"] = json!(true);
        let fixture_digest = compiler_core::digest(&fixture)?;
        let overlay = overlay(&production, &fixture)?;
        write_stage(directory.path(), &production, &fixture, &overlay)?;

        let selected = plan(directory.path(), Mode::Fixture)?;
        assert!(selected.ir.test_only);
        assert_eq!(selected.ir_digest, fixture_digest);
        let production = plan(directory.path(), Mode::Production)?;
        assert!(!production.ir.test_only);

        let mut wrong_overlay = overlay;
        wrong_overlay["fixtureIrSha256"] = json!("0".repeat(64));
        fs::write(
            directory.path().join("fixture-overlay.json"),
            wrong_overlay.to_string(),
        )?;
        assert!(plan(directory.path(), Mode::Fixture).is_err());
        Ok(())
    }

    fn production_ir() -> serde_json::Value {
        json!({
            "schemaVersion":"apsides.dev/ir/v2","testOnly":false,
            "domain":{"id":"fixture-selection","namespace":"workload",
                "controlNamespace":"control","clusterBinding":"cluster-a",
                "policyRef":"platform/fixture-selection","enforcement":"api-bound",
                "minKubernetesVersion":"1.34",
                "budgets":{"maxResources":4,"maxInputs":4,"maxReactions":4,
                    "maxObjectBytes":262_144,"maxQueueDepth":4,"maxConcurrentRequests":2}},
            "resources":[{"id":"config","apiVersion":"v1","kind":"ConfigMap",
                "name":"config","namespace":"workload",
                "desired":{"apiVersion":"v1","kind":"ConfigMap",
                    "metadata":{"name":"config","namespace":"workload"},
                    "data":{"value":"production"}},
                "lifecycle":{"deletionPolicy":"orphan"},
                "source":{"file":"program.tsx","line":1,"column":1}}],
            "inputs":[],"reactions":[],
            "handlers":[],"outputs":[],"provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.tsx",
                "sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
        })
    }

    fn overlay(
        production: &serde_json::Value,
        fixture: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let production_digest = compiler_core::digest(production)?;
        let fixture_digest = compiler_core::digest(fixture)?;
        Ok(json!({
            "schemaVersion":"apsides.dev/fixture-overlay/v1",
            "testOnly":true,"productionReady":false,
            "productionIrSha256":production_digest.strip_prefix("sha256:").ok_or("digest prefix missing")?,
            "fixtureIrSha256":fixture_digest.strip_prefix("sha256:").ok_or("digest prefix missing")?
        }))
    }

    fn write_stage(
        directory: &Path,
        production: &serde_json::Value,
        fixture: &serde_json::Value,
        overlay: &serde_json::Value,
    ) -> Result<()> {
        fs::write(directory.join("ir.json"), production.to_string())?;
        fs::write(directory.join("fixture-ir.json"), fixture.to_string())?;
        fs::write(directory.join("fixture-overlay.json"), overlay.to_string())?;
        Ok(())
    }
}
