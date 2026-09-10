use crate::{Result, read_plan};
use compiler_core::parse_and_validate;
use std::{collections::BTreeMap, fs, io::Read as _, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceMode {
    Production,
    Fixture,
}

impl SourceMode {
    fn ir_name(self) -> &'static str {
        match self {
            Self::Production => "ir.json",
            Self::Fixture => "fixture-ir.json",
        }
    }
}

pub(crate) fn source_mode(options: &BTreeMap<String, String>, command: &str) -> Result<SourceMode> {
    let Some(mode) = options.get("--mode") else {
        return Ok(SourceMode::Production);
    };
    if command != "build" || !options.contains_key("--source") || options.contains_key("--ir") {
        return Err("--mode applies only to build --source".into());
    }
    match mode.as_str() {
        "production" => Ok(SourceMode::Production),
        "fixture" => Ok(SourceMode::Fixture),
        _ => Err("--mode must be production or fixture".into()),
    }
}

pub(crate) fn source_plan(stage: &Path, mode: SourceMode) -> Result<compiler_core::CompiledPlan> {
    if mode == SourceMode::Production {
        return read_plan(
            stage
                .join(mode.ir_name())
                .to_str()
                .ok_or("non UTF-8 source output path")?,
        );
    }
    let production = source_document(&stage.join("ir.json"))?;
    let fixture = source_document(&stage.join(mode.ir_name()))?;
    let overlay = source_document(&stage.join("fixture-overlay.json"))?;
    release::validate_fixture_projection(&production, &fixture, &overlay)?;
    Ok(parse_and_validate(&serde_json::to_string(&fixture)?)?)
}

fn source_document(path: &Path) -> Result<serde_json::Value> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > 8 * 1024 * 1024 {
        return Err("source compiler document is missing or oversized".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("source compiler document is oversized".into());
    }
    Ok(compiler_core::parse_strict(std::str::from_utf8(&bytes)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mode_defaults_to_production_and_selects_exact_ir() -> Result<()> {
        let default = BTreeMap::from([
            ("--source".into(), "program.tsx".into()),
            ("--output".into(), "output".into()),
        ]);
        assert_eq!(source_mode(&default, "build")?, SourceMode::Production);
        assert_eq!(SourceMode::Production.ir_name(), "ir.json");

        let fixture = BTreeMap::from([
            ("--source".into(), "program.tsx".into()),
            ("--output".into(), "output".into()),
            ("--mode".into(), "fixture".into()),
        ]);
        assert_eq!(source_mode(&fixture, "build")?, SourceMode::Fixture);
        assert_eq!(SourceMode::Fixture.ir_name(), "fixture-ir.json");
        Ok(())
    }

    #[test]
    fn explicit_mode_rejects_compile_raw_ir_and_unknown_values() -> Result<()> {
        let mut options = BTreeMap::from([
            ("--source".into(), "program.tsx".into()),
            ("--output".into(), "output".into()),
            ("--mode".into(), "production".into()),
        ]);
        assert_eq!(source_mode(&options, "build")?, SourceMode::Production);
        assert!(source_mode(&options, "compile").is_err());

        options.remove("--source");
        options.insert("--ir".into(), "ir.json".into());
        assert!(source_mode(&options, "build").is_err());

        options.remove("--ir");
        options.insert("--source".into(), "program.tsx".into());
        options.insert("--mode".into(), "other".into());
        assert!(source_mode(&options, "build").is_err());
        Ok(())
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

        let selected = source_plan(directory.path(), SourceMode::Fixture)?;
        assert!(selected.ir.test_only);
        assert_eq!(selected.ir_digest, fixture_digest);
        let production = source_plan(directory.path(), SourceMode::Production)?;
        assert!(!production.ir.test_only);

        let mut wrong_overlay = overlay;
        wrong_overlay["fixtureIrSha256"] = json!("0".repeat(64));
        fs::write(
            directory.path().join("fixture-overlay.json"),
            wrong_overlay.to_string(),
        )?;
        assert!(source_plan(directory.path(), SourceMode::Fixture).is_err());
        Ok(())
    }

    fn production_ir() -> serde_json::Value {
        json!({
            "schemaVersion":"apsides.dev/ir/v1","testOnly":false,
            "domain":{"id":"fixture-selection","namespace":"workload",
                "controlNamespace":"control","clusterBinding":"cluster-a",
                "policyRef":"platform/fixture-selection","enforcement":"api-bound",
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
            "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.tsx",
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
