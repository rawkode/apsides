//! `aps release` — assemble an unsigned release from a verified artifact.

pub mod verify;

use crate::{Result, artifact, cli, report};
use compiler_core::{canonical_bytes, digest, parse_strict};
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
    let inputs = read_json(&arguments.build_inputs)?;
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
    let manifest = json!({"schemaVersion":"apsides.dev/release/v1","mode":mode,
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
        (&arguments.coverage, "coverage.json"),
    ] {
        copy(source, output, name, files)?;
    }
    let mut entries =
        fs::read_dir(&arguments.evidence)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    fs::create_dir(output.join("evidence"))?;
    for entry in entries {
        if !entry.file_type()?.is_file() {
            return Err("release evidence must be regular files in one directory".into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "evidence name is not UTF-8")?;
        copy(&entry.path(), output, &format!("evidence/{name}"), files)?;
    }
    Ok(())
}
