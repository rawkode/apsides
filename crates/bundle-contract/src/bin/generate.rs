use std::fs;
use std::path::PathBuf;

use oxideloop_bundle_contract::generate::generated_artifacts;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let check = matches!(args.next().as_deref(), Some(value) if value == "--check");
    let root = if check {
        std::env::current_dir()?
    } else {
        args.next().map_or(std::env::current_dir()?, PathBuf::from)
    };
    let mut stale = false;
    for artifact in generated_artifacts() {
        let path = root.join(artifact.relative_path);
        if check {
            stale |= fs::read_to_string(&path).ok().as_deref() != Some(artifact.contents.as_str());
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, artifact.contents)?;
        }
    }
    if stale {
        Err("generated bundle artifacts are stale".into())
    } else {
        Ok(())
    }
}
