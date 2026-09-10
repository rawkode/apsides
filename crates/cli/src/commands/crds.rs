//! `aps crds fetch` — import a custom resource definition into the model.

use crate::{Result, assets, cli, deno};

/// # Errors
/// Returns an error if the compiler assets are unavailable or the import fails.
pub fn fetch(arguments: &cli::CrdsFetch) -> Result<()> {
    let script = assets::root()?.join("packages/compiler/src/crds.ts");
    if !script.is_file() {
        return Err("CRD generator assets are unavailable".into());
    }
    let mut command = deno::script(&script);
    command
        .arg(&arguments.url)
        .arg(&arguments.kind)
        .arg(&arguments.output);
    if let Some(version) = &arguments.version {
        command.arg(version);
    }
    if !command.status()?.success() {
        return Err("CRD import failed".into());
    }
    Ok(())
}
