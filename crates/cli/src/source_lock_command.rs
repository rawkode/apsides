use crate::{Result, only};
use std::collections::BTreeMap;

pub fn command(options: &BTreeMap<String, String>) -> Result<()> {
    only(options, &["--source", "--modules"])?;
    let script = crate::assets::root()?.join("packages/compiler/src/source-lock.ts");
    if !script.is_file() {
        return Err("source lock assets are unavailable".into());
    }
    let mut command = crate::deno::script(&script);
    command.arg(
        options
            .get("--source")
            .map_or("program.tsx", String::as_str),
    );
    if let Some(modules) = options.get("--modules") {
        for module in modules.split(',') {
            if module.is_empty() {
                return Err("source modules must be nonempty paths".into());
            }
            command.arg(module);
        }
    }
    if !command.status()?.success() {
        return Err("source lock update failed".into());
    }
    Ok(())
}
