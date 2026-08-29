use std::fs;
use std::path::PathBuf;

use oxideloop_renderer_abi::generate::generated_artifacts;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .map_or(std::env::current_dir()?, PathBuf::from);

    for artifact in generated_artifacts() {
        let path = root.join(artifact.relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, artifact.contents)?;
    }
    Ok(())
}
