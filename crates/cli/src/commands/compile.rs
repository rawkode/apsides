//! `aps compile` and `aps build`.
//!
//! Both stages emit the same validated artifact; `build` additionally produces
//! the pinned Linux image, native binary, SBOM and compiler closure. Neither
//! establishes production readiness.

use crate::{Result, artifact, assets, cli, deno, report, source};
use compiler_core::CompiledPlan;
use serde_json::json;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

/// # Errors
/// Returns an error if the source is unreadable or invalid, if the output or its
/// source staging directory already exists, or if source compilation fails.
pub fn compile(arguments: &cli::Compile) -> Result<()> {
    let output = &arguments.output;
    let stage = stage_source(&arguments.source, output)?;
    let plan = source::plan(&stage, cli::Mode::Production)?;
    emit(&plan, output, &stage)?;
    report::emit(&summary(&plan, output, "compile"))
}

/// # Errors
/// Returns an error for any [`compile`] failure, or if the controller image
/// build fails — in which case no deployable release was produced.
pub fn build(arguments: &cli::Build) -> Result<()> {
    let output = &arguments.output;
    let stage = stage_source(&arguments.source, output)?;
    let plan = source::plan(&stage, arguments.mode)?;
    emit(&plan, output, &stage)?;
    build_native(arguments, &stage, output)?;
    report::emit(&summary(&plan, output, "build"))
}

fn emit(plan: &CompiledPlan, output: &Path, stage: &Path) -> Result<()> {
    artifact::emit(plan, output)?;
    artifact::prepare_lock(output)?;
    artifact::record_source(output, stage)
}

fn summary(plan: &CompiledPlan, output: &Path, stage: &str) -> serde_json::Value {
    json!({
        "irDigest": plan.ir_digest,
        "output": output,
        "productionReady": false,
        "stage": stage,
    })
}

/// Run the Oxc/Deno source compiler into a staging directory beside the output.
fn stage_source(source: &Path, output: &Path) -> Result<PathBuf> {
    let stage = output.with_extension("source");
    if stage.exists() {
        return Err("source staging directory already exists; choose a fresh output".into());
    }
    if let Some(parent) = stage.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut command = deno::script(&assets::root()?.join("packages/compiler/src/compiler.ts"));
    command.arg(fs::canonicalize(source)?).arg(&stage);
    if !deno::isolate(&mut command).status()?.success() {
        return Err("Oxc/Deno source compilation failed".into());
    }
    Ok(stage)
}

fn build_native(arguments: &cli::Build, stage: &Path, output: &Path) -> Result<()> {
    let mut builder = Command::new("python3");
    builder
        .arg(assets::root()?.join("scripts/build.py"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("APSIDES_CLI", env::current_exe()?)
        .arg("--artifact")
        .arg(output)
        .arg("--source-output")
        .arg(stage)
        .arg("--profile")
        .arg(&arguments.profile);
    if arguments.cache == cli::Cache::Disabled {
        builder.arg("--no-cache");
    }
    if !builder.status()?.success() {
        return Err("controller image build failed; no deployable release was produced".into());
    }
    Ok(())
}
