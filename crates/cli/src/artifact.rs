//! The compiled artifact: emission, its source record, and integrity checks.

pub mod input;

use crate::Result;
use compiler_core::{CompiledPlan, Handler, parse_and_validate};
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

pub fn emit(plan: &CompiledPlan, output: &Path, stage: &Path) -> Result<()> {
    let assets = crate::assets::root()?;
    if output.exists() {
        return Err(
            "output already exists; use a fresh directory to avoid replacing an artifact".into(),
        );
    }
    fs::create_dir_all(output.join("controller/src"))?;
    let mut files = BTreeMap::new();
    embed_handlers(plan, output, stage, &mut files)?;
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
    let runtime_path = assets.join("crates/runtime").canonicalize()?;
    let manifest = format!(
        // No `tokio` dependency here: `main.rs` is synchronous (see the
        // Apsides v0.3 plan, Phase 5c) and calls `runtime::start` directly,
        // which builds and owns its own tokio runtime internally, after
        // Landlock lockdown.
        "[package]\nname = \"controller\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\nruntime = {{ path = {} }}\n",
        serde_json::to_string(runtime_path.to_str().ok_or("non UTF-8 runtime path")?)?
    );
    write(
        output,
        "controller/Cargo.toml",
        manifest.as_bytes(),
        &mut files,
    )?;
    let main_rs = format!(
        "//! Synchronous by design (see the Apsides v0.3 plan, \"5c. Filesystem\n\
         //! lockdown\"): `runtime::start` reads every file the controller will\n\
         //! ever need, builds the kube client's in-memory TLS/DNS state, then\n\
         //! applies a deny-all Landlock ruleset to this thread -- before any\n\
         //! other thread in the process exists -- and only then builds the\n\
         //! tokio runtime this generated binary never constructs itself. No\n\
         //! the usual tokio-main attribute macro here: that macro would build the runtime (and its\n\
         //! worker threads) before `main` runs at all, before lockdown had a\n\
         //! chance to happen first.\n\
         use std::io::Write as _;\n\n\
         const HANDLERS: &[(&str, &str)] = &[{handlers}];\n\n\
         fn main() {{\n    \
             if let Err(error) = runtime::start(include_str!(\"ir.json\"), HANDLERS) {{\n        \
                 let _ = writeln!(std::io::stderr().lock(), \"apsides controller: {{error}}\");\n        \
                 std::process::exit(1);\n    \
             }}\n\
         }}\n",
        handlers = handler_consts(&plan.ir.handlers),
    );
    write(
        output,
        "controller/src/main.rs",
        main_rs.as_bytes(),
        &mut files,
    )?;
    let boundary = b"This is a local native compilation artifact. It is not a production OCI bundle.\nDeployment remains blocked until provenance, image identity, platform ceiling, claims, short-lived credentials, and generation transition gates pass.\nThe generated Rust embeds validated IR; it never evaluates TypeScript or loads JavaScript.\nCargo dependencies currently resolve from the prepared local cache. Cross-host reproducibility is not yet established.\n";
    write(output, "BOUNDARY.txt", boundary, &mut files)?;
    let manifest = Manifest {
        schema_version: "apsides.dev/local-artifact/v1".into(),
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

/// Rust source for `main.rs`'s `HANDLERS` static: one `(id, include_str!(...))`
/// tuple per declared handler, in IR order. `handler.id` is already a
/// validated DNS label (compiler-core's `dns_label`), so `{:?}` escaping is
/// defense in depth, not a requirement for correctness.
fn handler_consts(handlers: &[Handler]) -> String {
    handlers
        .iter()
        .map(|handler| {
            format!(
                "({:?}, include_str!({:?}))",
                handler.id,
                format!("handlers/{}.js", handler.id)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Copy each declared handler's bundle (already written by the source
/// compiler at `stage/handlers/<id>.js`) into `controller/src/handlers/`
/// next to `ir.json`, so the generated `main.rs` can `include_str!` it,
/// after checking it matches the IR's own recorded `bundleSha256`.
fn embed_handlers(
    plan: &CompiledPlan,
    output: &Path,
    stage: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<()> {
    if plan.ir.handlers.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(output.join("controller/src/handlers"))?;
    for handler in &plan.ir.handlers {
        let bundle =
            fs::read(stage.join("handlers").join(format!("{}.js", handler.id))).map_err(|_| {
                format!(
                    "handler {} bundle is missing from the source stage",
                    handler.id
                )
            })?;
        if hash(&bundle) != format!("sha256:{}", handler.bundle_sha256) {
            return Err(format!(
                "handler {} bundle does not match its declared bundleSha256",
                handler.id
            )
            .into());
        }
        write(
            output,
            &format!("controller/src/handlers/{}.js", handler.id),
            &bundle,
            files,
        )?;
    }
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
    if manifest.schema_version != "apsides.dev/local-artifact/v1" || manifest.production_ready {
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
        && (!manifest.files.contains_key("apsides-controller")
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
    for handler in &plan.ir.handlers {
        let bundle = fs::read(
            root.join("controller/src/handlers")
                .join(format!("{}.js", handler.id)),
        )?;
        if hash(&bundle) != format!("sha256:{}", handler.bundle_sha256) {
            return Err(format!(
                "handler {} embedded bundle does not match its declared bundleSha256",
                handler.id
            )
            .into());
        }
    }
    if fs::read(root.join("ir.json"))? != fs::read(root.join("controller/src/ir.json"))? {
        return Err("embedded controller IR mismatch".into());
    }
    Ok(
        json!({"status":"integrity-verified","irDigest":plan.ir_digest,"stage":manifest.stage,"productionReady":false,"provenanceVerified":false}),
    )
}

pub fn prepare_lock(root: &Path) -> Result<()> {
    let workspace_lock = crate::assets::root()?.join("Cargo.lock");
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
    fn path_escape_is_rejected_before_file_access() -> Result<()> {
        let root = std::env::temp_dir().join(format!("apsides-escape-{}", std::process::id()));
        fs::create_dir_all(&root)?;
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
            schema_version: "apsides.dev/local-artifact/v1".into(),
            ir_digest: "bad".into(),
            stage: "compiled-source".into(),
            production_ready: false,
            files,
        };
        fs::write(root.join("manifest.json"), serde_json::to_vec(&m)?)?;
        let error = verify(&root).err().ok_or("path escape was accepted")?;
        assert!(
            error.to_string().contains("relative normal path"),
            "{error}"
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
