use crate::{Result, only, required};
use std::{collections::BTreeMap, path::Path, process::Command};

pub fn fetch(options: &BTreeMap<String, String>) -> Result<()> {
    only(options, &["--url", "--kind", "--output", "--version"])?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/compiler/src/crds.ts");
    if !script.is_file() {
        return Err("CRD generator assets are unavailable".into());
    }
    let mut command = Command::new("node");
    command
        .arg(script)
        .arg(required(options, "--url")?)
        .arg(required(options, "--kind")?)
        .arg(required(options, "--output")?);
    if let Some(version) = options.get("--version") {
        command.arg(version);
    }
    if !command.status()?.success() {
        return Err("CRD import failed".into());
    }
    Ok(())
}
