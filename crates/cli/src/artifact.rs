use crate::Result;
use compiler_core::{CompiledPlan, parse_and_validate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    schema_version: String,
    ir_digest: String,
    stage: String,
    production_ready: bool,
    files: BTreeMap<String, String>,
}

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn emit(plan: &CompiledPlan, output: &Path) -> Result<()> {
    if output.exists() {
        return Err(
            "output already exists; use a fresh directory to avoid replacing an artifact".into(),
        );
    }
    fs::create_dir_all(output.join("controller/src"))?;
    let mut files = BTreeMap::new();
    for (name, value) in compiler_core::review_documents(plan) {
        write(
            output,
            name,
            &compiler_core::canonical_bytes(&value)?,
            &mut files,
        )?;
    }
    write(
        output,
        "ir.json",
        &serde_json::to_vec_pretty(&plan.ir)?,
        &mut files,
    )?;
    write(
        output,
        "plan.json",
        &serde_json::to_vec_pretty(plan)?,
        &mut files,
    )?;
    write(
        output,
        "test-plan.json",
        &serde_json::to_vec_pretty(&plan.test_plan)?,
        &mut files,
    )?;
    write(
        output,
        "controller/src/ir.json",
        &serde_json::to_vec_pretty(&plan.ir)?,
        &mut files,
    )?;
    let runtime_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../runtime")
        .canonicalize()?;
    let manifest = format!(
        "[package]\nname = \"controller\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\nruntime = {{ path = {} }}\ntokio = {{ version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }}\n",
        serde_json::to_string(runtime_path.to_str().ok_or("non UTF-8 runtime path")?)?
    );
    write(
        output,
        "controller/Cargo.toml",
        manifest.as_bytes(),
        &mut files,
    )?;
    write(output, "controller/src/main.rs", b"use std::io::Write as _;\n\n#[tokio::main]\nasync fn main() {\n    if let Err(error) = runtime::run(include_str!(\"ir.json\")).await {\n        let _ = writeln!(std::io::stderr().lock(), \"reakt controller: {error}\");\n        std::process::exit(1);\n    }\n}\n", &mut files)?;
    let boundary = b"This is a local native compilation artifact. It is not a production OCI bundle.\nDeployment remains blocked until provenance, image identity, platform ceiling, claims, short-lived credentials, and generation transition gates pass.\nThe generated Rust embeds validated IR; it never evaluates TypeScript or loads JavaScript.\nCargo dependencies currently resolve from the prepared local cache. Cross-host reproducibility is not yet established.\n";
    write(output, "BOUNDARY.txt", boundary, &mut files)?;
    let manifest = Manifest {
        schema_version: "reakt.dev/local-artifact/v1".into(),
        ir_digest: plan.ir_digest.clone(),
        stage: "compiled-source".into(),
        production_ready: false,
        files,
    };
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

fn write(
    root: &Path,
    name: &str,
    bytes: &[u8],
    files: &mut BTreeMap<String, String>,
) -> Result<()> {
    fs::write(root.join(name), bytes)?;
    files.insert(name.into(), hash(bytes));
    Ok(())
}

pub fn record_source(root: &Path, stage: &Path) -> Result<()> {
    let mut manifest: Manifest = serde_json::from_value(compiler_core::parse_strict(
        &fs::read_to_string(root.join("manifest.json"))?,
    )?)?;
    fs::create_dir(root.join("source"))?;
    for name in [
        "program.mjs",
        "program.mjs.map",
        "evaluate.mjs",
        "source-map.json",
        "build-inputs.json",
        "fixtures.json",
        "fixture-ir.json",
        "fixture-overlay.json",
        "fixture-catalogue.json",
        "fixture-coverage.json",
    ] {
        write(
            root,
            &format!("source/{name}"),
            &fs::read(stage.join(name))?,
            &mut manifest.files,
        )?;
    }
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

pub fn verify(root: &Path) -> Result<Value> {
    let manifest: Manifest = serde_json::from_value(compiler_core::parse_strict(
        &fs::read_to_string(root.join("manifest.json"))?,
    )?)?;
    if manifest.schema_version != "reakt.dev/local-artifact/v1" || manifest.production_ready {
        return Err("unsupported artifact or false production readiness claim".into());
    }
    if !matches!(manifest.stage.as_str(), "compiled-source" | "native-binary") {
        return Err("unknown artifact stage".into());
    }
    for required in [
        "ir.json",
        "plan.json",
        "test-plan.json",
        "controller/src/ir.json",
        "controller/Cargo.toml",
        "controller/src/main.rs",
        "BOUNDARY.txt",
    ] {
        if !manifest.files.contains_key(required) {
            return Err(format!("artifact missing required file {required}").into());
        }
    }
    if manifest.stage == "native-binary"
        && (!manifest.files.contains_key("reakt-controller")
            || !manifest.files.contains_key("controller/Cargo.lock"))
    {
        return Err("native artifact missing binary or dependency lock".into());
    }
    let root = root.canonicalize()?;
    for (name, digest) in &manifest.files {
        let path = Path::new(name);
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err("artifact path must be a relative normal path".into());
        }
        let resolved = root.join(path).canonicalize()?;
        if !resolved.starts_with(&root) {
            return Err("artifact symlink escapes bundle".into());
        }
        if hash(&fs::read(&resolved)?) != *digest {
            return Err(format!("artifact digest mismatch: {name}").into());
        }
    }
    let plan = parse_and_validate(&fs::read_to_string(root.join("ir.json"))?)?;
    if plan.ir_digest != manifest.ir_digest {
        return Err("IR digest mismatch".into());
    }
    let actual: Value = compiler_core::parse_strict(&fs::read_to_string(root.join("plan.json"))?)?;
    if serde_json::to_value(&plan)? != actual {
        return Err("plan does not match trusted validator output".into());
    }
    let test_plan: Value =
        compiler_core::parse_strict(&fs::read_to_string(root.join("test-plan.json"))?)?;
    if serde_json::to_value(&plan.test_plan)? != test_plan {
        return Err("test plan does not match trusted validator output".into());
    }
    if fs::read(root.join("ir.json"))? != fs::read(root.join("controller/src/ir.json"))? {
        return Err("embedded controller IR mismatch".into());
    }
    Ok(
        json!({"status":"integrity-verified","irDigest":plan.ir_digest,"stage":manifest.stage,"productionReady":false,"provenanceVerified":false}),
    )
}

pub fn prepare_lock(root: &Path) -> Result<()> {
    let workspace_lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    fs::copy(workspace_lock, root.join("controller/Cargo.lock"))?;
    // Start from the compiler's own lock. Cargo adds the generated root package
    // without upgrading versions already selected in that lock; resolution is
    // preparation, and all subsequent native builds require --locked.
    let output = std::process::Command::new("cargo")
        .args([
            "metadata",
            "--offline",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(root.join("controller/Cargo.toml"))
        .output()?;
    if !output.status.success() {
        return Err("prepared native dependencies are incomplete; Cargo metadata could not lock the generated controller offline".into());
    }
    let mut manifest: Manifest = serde_json::from_value(compiler_core::parse_strict(
        &fs::read_to_string(root.join("manifest.json"))?,
    )?)?;
    manifest.files.insert(
        "controller/Cargo.lock".into(),
        hash(&fs::read(root.join("controller/Cargo.lock"))?),
    );
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_escape_is_rejected_before_file_access() {
        let root = std::env::temp_dir().join(format!("reakt-escape-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mut files = BTreeMap::from([("../escape".into(), "sha256:bad".into())]);
        for path in [
            "ir.json",
            "plan.json",
            "test-plan.json",
            "controller/src/ir.json",
            "controller/Cargo.toml",
            "controller/src/main.rs",
            "BOUNDARY.txt",
        ] {
            files.insert(path.into(), "bad".into());
        }
        let m = Manifest {
            schema_version: "reakt.dev/local-artifact/v1".into(),
            ir_digest: "bad".into(),
            stage: "compiled-source".into(),
            production_ready: false,
            files,
        };
        fs::write(root.join("manifest.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(
            verify(&root)
                .unwrap_err()
                .to_string()
                .contains("relative normal path")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
