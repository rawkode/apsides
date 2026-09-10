//! `aps lock` — refresh the source lock for an authoring program.

use crate::{Result, assets, cli, deno};

/// # Errors
/// Returns an error if the compiler assets are unavailable or the update fails.
pub fn command(arguments: &cli::Lock) -> Result<()> {
    let script = assets::root()?.join("packages/compiler/src/source-lock.ts");
    if !script.is_file() {
        return Err("source lock assets are unavailable".into());
    }
    let mut command = deno::script(&script);
    command.arg(&arguments.source).args(&arguments.modules);
    if !command.status()?.success() {
        return Err("source lock update failed".into());
    }
    Ok(())
}
