//! `aps release` — assemble an unsigned release from a verified artifact.

pub mod verify;

use crate::{Result, artifact, cli, report};
use compiler_core::{MAX_BUILD_INPUTS_BYTES, canonical_bytes, digest, parse_bounded, parse_strict};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

/// # Errors
/// Returns an error if the artifact does not verify, if the output directory
/// already exists, if any input is missing, irregular or oversized, or if the
/// build inputs lack a valid compiler closure digest.
pub fn command(arguments: &cli::Release) -> Result<()> {
    let artifact_root = arguments.artifact.as_path();
    artifact::verify(artifact_root)?;
    let output = arguments.output.as_path();
    if output.exists() {
        return Err("release output exists; choose a fresh directory".into());
    }
    // build-inputs.json's own budget, not IR's: its compiler closure's
    // build-context file manifest records every vendored source file
    // (thousands, once crates/engine's deno_core is in the dependency
    // graph), routinely multiple MiB on its own.
    let inputs = read_build_inputs(&arguments.build_inputs)?;
    let ir = read_json(&artifact_root.join("ir.json"))?;
    let mode = if ir["testOnly"] == true {
        "fixture"
    } else {
        "production"
    };
    fs::create_dir_all(output)?;
    let mut files = BTreeMap::new();
    collect_files(arguments, output, &mut files)?;
    let compiler = inputs["compilerClosureDigest"]
        .as_str()
        .ok_or("final build inputs lack a compiler closure digest")?;
    release::validate_compiler_closure(&inputs, compiler)?;
    let input_bytes = canonical_bytes(&inputs)?;
    fs::write(output.join("build-inputs.json"), &input_bytes)?;
    files.insert(
        "build-inputs.json".into(),
        format!("sha256:{:x}", Sha256::digest(&input_bytes)),
    );
    let repository = arguments.repository.as_str();
    let commit = arguments.commit.as_str();
    let manifest = json!({"schemaVersion":"apsides.dev/release/v2","mode":mode,
        "irDigest":digest(&ir)?,"binaryDigest":files.get("apsides-controller").ok_or("release is missing its controller binary")?,
        "image":{"reference":arguments.image,"platform":arguments.platform},
        "source":{"repository":repository,"commit":commit,"inputsDigest":digest(&inputs)?},
        "compilerDigest":compiler,"files":files});
    let bytes = canonical_bytes(&manifest)?;
    fs::write(output.join("release.json"), &bytes)?;
    let release_digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    release::read_release(output)?;
    fs::write(
        output.join("provenance.json"),
        canonical_bytes(&json!({
            "_type":"https://in-toto.io/Statement/v1","predicateType":"https://slsa.dev/provenance/v1",
            "subject":[{"name":"release.json","digest":{"sha256":release_digest.trim_start_matches("sha256:")}}],
            "predicate":{"buildDefinition":{"buildType":"https://apsides.dev/build/v1","externalParameters":{"repository":repository,"commit":commit,"compilerDigest":compiler},"internalParameters":{},"resolvedDependencies":[]},
            "runDetails":{"builder":{"id":"https://apsides.dev/compiler/v1"}}}
        }))?,
    )?;
    report::emit(
        &json!({"status":"assembled-unsigned","releaseDigest":release_digest,"mode":mode,"output":output}),
    )
}

fn copy(
    source: &Path,
    output: &Path,
    name: &str,
    files: &mut BTreeMap<String, String>,
) -> Result<()> {
    if source.symlink_metadata()?.file_type().is_symlink() || !source.is_file() {
        return Err("release inputs must be regular files".into());
    }
    let max = if name == "controller.oci.tar" {
        512 * 1024 * 1024
    } else if name == "apsides-controller" {
        128 * 1024 * 1024
    } else {
        8 * 1024 * 1024
    };
    if source.metadata()?.len() > max {
        return Err("release input exceeds size limit".into());
    }
    let bytes = fs::read(source)?;
    fs::write(output.join(name), &bytes)?;
    files.insert(name.into(), format!("sha256:{:x}", Sha256::digest(&bytes)));
    Ok(())
}
fn read_json(path: &Path) -> Result<Value> {
    Ok(parse_strict(&fs::read_to_string(path)?)?)
}

fn read_build_inputs(path: &Path) -> Result<Value> {
    Ok(parse_bounded(
        &fs::read_to_string(path)?,
        MAX_BUILD_INPUTS_BYTES,
        "build-inputs.json",
    )?)
}

fn collect_files(
    arguments: &cli::Release,
    output: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<()> {
    let artifact = arguments.artifact.as_path();
    for name in [
        "ir.json",
        "plan.json",
        "test-plan.json",
        "graphs.json",
        "capabilities.json",
        "compatibility.json",
        "ir-source-map.json",
        "surface.json",
    ] {
        copy(&artifact.join(name), output, name, files)?;
    }
    for name in [
        "source-map.json",
        "fixtures.json",
        "fixture-ir.json",
        "fixture-overlay.json",
        "fixture-catalogue.json",
        "fixture-coverage.json",
    ] {
        copy(&artifact.join("source").join(name), output, name, files)?;
    }
    for (source, name) in [
        (&arguments.oci, "controller.oci.tar"),
        (&arguments.binary, "apsides-controller"),
        (&arguments.sbom, "sbom.json"),
    ] {
        copy(source, output, name, files)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntactically valid JSON document of at least `bytes` bytes.
    fn oversized_document(bytes: usize) -> String {
        format!("{{\"padding\":\"{}\"}}", "a".repeat(bytes))
    }

    #[test]
    fn read_build_inputs_accepts_a_document_larger_than_the_ir_budget() -> Result<()> {
        // Bigger than compiler_core::MAX_IR_BYTES (4 MiB) -- real ones now
        // routinely are, once crates/engine's deno_core is in the vendored
        // dependency closure -- but within build-inputs.json's own, larger
        // budget.
        let bytes = compiler_core::MAX_IR_BYTES + 1024;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("build-inputs.json");
        fs::write(&path, oversized_document(bytes))?;
        let value = read_build_inputs(&path)?;
        let Some(padding) = value["padding"].as_str() else {
            return Err("expected a string padding field".into());
        };
        assert_eq!(padding.len(), bytes);
        Ok(())
    }

    #[test]
    fn read_build_inputs_rejects_a_document_over_its_own_budget_by_name() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("build-inputs.json");
        fs::write(&path, oversized_document(MAX_BUILD_INPUTS_BYTES))?;
        let error = read_build_inputs(&path)
            .err()
            .ok_or("expected an oversized build-inputs.json to be rejected")?
            .to_string();
        assert!(error.contains("build-inputs.json"), "{error}");
        assert!(!error.contains("IR"), "{error}");
        Ok(())
    }

    #[test]
    fn read_json_still_uses_the_ir_budget_for_ir() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("ir.json");
        fs::write(&path, oversized_document(compiler_core::MAX_IR_BYTES))?;
        let error = read_json(&path)
            .err()
            .ok_or("expected oversized IR to be rejected")?
            .to_string();
        assert!(error.contains("IR"), "{error}");
        assert!(error.contains("4 MiB"), "{error}");
        Ok(())
    }
}
