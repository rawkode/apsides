mod files;

use crate::{Result, cli, report};
use files::{PROJECT_FILES, SDK_FILES};
use serde_json::json;
use std::{
    env, fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
};

/// Create one modest TypeScript authoring project in an empty file namespace.
///
/// # Errors
/// Rejects existing generated paths, unavailable compiler assets, source-lock
/// failures, symlinks and filesystem races without overwriting files.
pub fn command(arguments: &cli::Init) -> Result<()> {
    let destination = destination(&arguments.directory)?;
    initialize(&destination)?;
    report::emit(&json!({
        "status": "initialized",
        "directory": destination,
        "entry": "program.tsx",
        "sourceLock": "apsides.lock.json"
    }))
}

fn destination(requested: &Path) -> Result<PathBuf> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        env::current_dir()?.join(requested)
    };
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("init directory must be a real directory".into());
            }
            Ok(absolute.canonicalize()?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = absolute
                .parent()
                .ok_or("init directory must have an existing parent")?
                .canonicalize()?;
            let name = absolute
                .file_name()
                .ok_or("init directory name is invalid")?;
            Ok(parent.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

fn initialize(destination: &Path) -> Result<()> {
    reject_existing_outputs(destination)?;
    let parent = destination
        .parent()
        .ok_or("init directory must have an existing parent")?;
    let stage = create_stage(parent)?;
    let result = (|| {
        write_stage(&stage)?;
        generate_source_lock(&stage)?;
        publish(&stage, destination)
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

fn reject_existing_outputs(destination: &Path) -> Result<()> {
    for name in PROJECT_FILES
        .iter()
        .map(|(name, _)| *name)
        .chain(["apsides.lock.json", ".aps"])
    {
        if fs::symlink_metadata(destination.join(name)).is_ok() {
            return Err(format!("init refuses to overwrite {name}").into());
        }
    }
    Ok(())
}

fn create_stage(parent: &Path) -> Result<PathBuf> {
    for attempt in 0_u8..100 {
        let path = parent.join(format!(".aps-init-{}-{attempt}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("unable to allocate an init staging directory".into())
}

fn write_stage(stage: &Path) -> Result<()> {
    for (name, contents) in PROJECT_FILES {
        fs::write(stage.join(name), contents)?;
    }
    let sdk = stage.join(".aps/sdk");
    fs::create_dir_all(&sdk)?;
    for (name, contents) in SDK_FILES {
        fs::write(sdk.join(name), contents)?;
    }
    Ok(())
}

fn generate_source_lock(stage: &Path) -> Result<()> {
    let script = crate::assets::root()?.join("packages/compiler/src/source-lock.ts");
    let metadata =
        fs::symlink_metadata(&script).map_err(|_| "Apsides compiler assets are unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Apsides compiler assets are invalid".into());
    }
    let mut command = crate::deno::script(&script.canonicalize()?);
    command.arg(stage.join("program.tsx")).stdout(Stdio::null());
    let status = crate::deno::isolate(&mut command).status()?;
    if !status.success() {
        return Err("source lock generation failed".into());
    }
    Ok(())
}

fn publish(stage: &Path, destination: &Path) -> Result<()> {
    reject_existing_outputs(destination)?;
    let destination_created = match fs::create_dir(destination) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let mut created = Vec::new();
    let result = (|| {
        let generated = destination.join(".aps");
        fs::create_dir(&generated)?;
        created.push(generated.clone());
        let sdk = generated.join("sdk");
        fs::create_dir(&sdk)?;
        for (name, _) in SDK_FILES {
            copy_exclusive(&stage.join(".aps/sdk").join(name), &sdk.join(name))?;
        }
        for name in PROJECT_FILES
            .iter()
            .map(|(name, _)| *name)
            .chain(["apsides.lock.json"])
        {
            let target = destination.join(name);
            copy_exclusive(&stage.join(name), &target)?;
            created.push(target);
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in created.iter().rev() {
            if path.is_dir() {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
        if destination_created {
            let _ = fs::remove_dir(destination);
        }
        return Err(error);
    }
    Ok(())
}

fn copy_exclusive(source: &Path, destination: &Path) -> Result<()> {
    let contents = fs::read(source)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    if let Err(error) = output.write_all(&contents) {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sha2::{Digest as _, Sha256};
    use std::process::Command;

    #[test]
    fn scaffold_is_locked_and_typechecks_outside_the_repository() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let project = temporary.path().join("hello");
        initialize(&project)?;

        let lock: Value = serde_json::from_slice(&fs::read(project.join("apsides.lock.json"))?)?;
        let program = fs::read(project.join("program.tsx"))?;
        assert_eq!(lock["entry"], "program.tsx");
        assert_eq!(lock["inputs"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            lock["inputs"][0]["sha256"],
            format!("{:x}", Sha256::digest(program))
        );
        assert!(project.join(".aps/sdk/index.ts").is_file());
        assert_eq!(fs::read_to_string(project.join(".gitignore"))?, ".aps/\n");

        let status = Command::new("deno")
            .args([
                "check",
                "--config",
                "deno.json",
                "--no-lock",
                "--no-npm",
                "--no-remote",
                "--deny-import",
                "program.tsx",
            ])
            .current_dir(&project)
            .status()?;
        assert!(status.success());
        Ok(())
    }

    #[test]
    fn existing_project_file_is_never_overwritten() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let project = temporary.path().join("hello");
        fs::create_dir(&project)?;
        fs::write(project.join("program.tsx"), "keep me")?;

        assert!(initialize(&project).is_err());
        assert_eq!(fs::read_to_string(project.join("program.tsx"))?, "keep me");
        assert!(!project.join(".aps").exists());
        assert!(!project.join("apsides.lock.json").exists());
        Ok(())
    }
}
