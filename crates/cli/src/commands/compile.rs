//! `aps compile` and `aps build`.
//!
//! Both stages emit the same validated artifact; `build` additionally produces
//! the pinned Linux image, native binary, SBOM and compiler closure. Neither
//! establishes production readiness.

use crate::{Result, artifact, assets, cli, deno, ir, report, source};
use compiler_core::CompiledPlan;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
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
/// Returns an error for any [`compile`] failure, or if the native build
/// fails — in which case no deployable release was produced.
pub fn build(arguments: &cli::Build) -> Result<()> {
    let output = &arguments.output;
    let stage = stage_source(&arguments.source, output)?;
    let plan = source::plan(&stage, arguments.mode)?;
    emit(&plan, output, &stage)?;
    let built = build_native(arguments, &stage, output)?;
    let mut summary = summary(&plan, output, "build");
    summary["imageDigest"] = json!(built.image_digest);
    report::emit(&summary)
}

fn emit(plan: &CompiledPlan, output: &Path, stage: &Path) -> Result<()> {
    artifact::emit(plan, output, stage)?;
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
    for path in [output, stage.as_path()] {
        if fs::symlink_metadata(path).is_ok() {
            return Err(format!(
                "{} already exists (an earlier failed run may have left it); remove it or \
                 choose a fresh --output",
                path.display()
            )
            .into());
        }
    }
    let parent = stage
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let unwritable = |error: std::io::Error| {
        format!(
            "cannot create the source staging directory {} in {}: {error}; choose an --output \
             whose parent directory is writable",
            stage.display(),
            parent.display()
        )
    };
    fs::create_dir_all(parent).map_err(unwritable)?;
    // The compiler creates the staging directory itself; proving it can be
    // created here turns a permission problem into an actionable message
    // instead of a generic source compilation failure.
    fs::create_dir(&stage).map_err(unwritable)?;
    fs::remove_dir(&stage).map_err(unwritable)?;
    let mut command = deno::script(&assets::root()?.join("packages/compiler/src/compiler.ts"));
    command.arg(fs::canonicalize(source)?).arg(&stage);
    if !deno::isolate(&mut command).status()?.success() {
        return Err("Oxc/Deno source compilation failed".into());
    }
    Ok(stage)
}

fn build_native(
    arguments: &cli::Build,
    stage: &Path,
    output: &Path,
) -> Result<packaging::BuildResult> {
    let assets = assets::root()?;
    packaging::build(&packaging::Request {
        assets: &assets,
        artifact: output,
        stage,
        profile: &arguments.profile,
        cache: match arguments.cache {
            cli::Cache::Enabled => packaging::Cache::Enabled,
            cli::Cache::Disabled => packaging::Cache::Disabled,
        },
        validator: &ArtifactValidator,
    })
    .map_err(|error| {
        format!("native build failed during {error}; no deployable release was produced").into()
    })
}

/// The native build's view of `aps`'s own artifact and IR checks.
struct ArtifactValidator;

impl packaging::Validator for ArtifactValidator {
    fn verify_artifact(&self, artifact: &Path) -> std::result::Result<String, String> {
        let verified = artifact::verify(artifact).map_err(|error| error.to_string())?;
        verified["irDigest"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "artifact verification returned no irDigest".to_owned())
    }

    fn ir_digest(&self, ir_json: &Path) -> std::result::Result<String, String> {
        ir::read_plan(ir_json)
            .map(|plan| plan.ir_digest)
            .map_err(|error| error.to_string())
    }
}
