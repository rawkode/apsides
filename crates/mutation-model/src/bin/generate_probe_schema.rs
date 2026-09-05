use std::fs;
use std::path::PathBuf;

use mutation_model::generated_probe_schema;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .map_or(std::env::current_dir()?, PathBuf::from);
    let path = root.join("probes/conditional-mutation/v0/probe.schema.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, generated_probe_schema())?;
    Ok(())
}
