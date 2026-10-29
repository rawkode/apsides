//! The build's output tree: reserved before anything is written, removed
//! again unless the build completes.

use crate::{Error, Result, Stage, context::root_for, fs::write_new};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The outputs a build writes, relative to the output directory.
pub const OUTPUTS: [&str; 5] = [
    "image",
    "linux-binary",
    "sbom",
    "build-inputs.json",
    "build-result.json",
];

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::Outputs, detail)
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// A claim on a build's outputs. Dropping it without [`Reservation::commit`]
/// removes every output this build wrote; the build context is kept.
#[derive(Debug)]
#[must_use = "dropping a reservation removes the build's outputs"]
pub struct Reservation {
    artifact: PathBuf,
    committed: bool,
}

/// Reserves the outputs of a build into `artifact` and creates
/// `<artifact>/image`.
///
/// # Errors
/// Fails, naming the path, when any output or the build context directory
/// already exists, and when the image directory cannot be created.
pub fn reserve(artifact: &Path) -> Result<Reservation> {
    let mut taken: Vec<PathBuf> = OUTPUTS.iter().map(|name| artifact.join(name)).collect();
    taken.push(root_for(artifact).map_err(|error| fail(error.detail))?);
    if let Some(path) = taken.iter().find(|path| exists(path)) {
        return Err(fail(format!(
            "{} already exists; choose a fresh --output (aps build never overwrites)",
            path.display()
        )));
    }
    let image = artifact.join("image");
    fs::create_dir(&image)
        .map_err(|error| fail(format!("cannot create {}: {error}", image.display())))?;
    Ok(Reservation {
        artifact: artifact.to_path_buf(),
        committed: false,
    })
}

impl Reservation {
    /// Keeps the outputs.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for name in OUTPUTS {
            let path = self.artifact.join(name);
            let removed = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path),
                Ok(_) => fs::remove_file(&path),
                Err(_) => continue,
            };
            if let Err(error) = removed {
                eprintln!(
                    "aps: warning: cannot remove the unfinished output {}: {error}",
                    path.display()
                );
            }
        }
    }
}

/// Writes the controller binary to `path`, creating its directory, as a new
/// executable file.
///
/// # Errors
/// Fails when the directory or file exists or cannot be written.
pub fn write_binary(path: &Path, binary: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir(parent)
            .map_err(|error| fail(format!("cannot create {}: {error}", parent.display())))?;
    }
    write_new(path, binary, 0o755)
        .map_err(|error| fail(format!("cannot write {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn existing_outputs_or_contexts_are_never_overwritten() -> TestResult {
        let directory = tempfile::tempdir()?;
        let artifact = directory.path().join("build");
        fs::create_dir(&artifact)?;
        for taken in ["sbom", "build-result.json"] {
            let path = artifact.join(taken);
            fs::write(&path, "")?;
            let error = reserve(&artifact).err().ok_or("accepted")?;
            assert_eq!(
                error.detail,
                format!(
                    "{} already exists; choose a fresh --output (aps build never overwrites)",
                    path.display()
                )
            );
            fs::remove_file(&path)?;
        }
        let context = directory.path().join("build.native-context");
        fs::create_dir(&context)?;
        let error = reserve(&artifact).err().ok_or("accepted")?;
        assert!(error.detail.starts_with(&context.display().to_string()));
        Ok(())
    }

    #[test]
    fn an_uncommitted_reservation_removes_what_it_wrote() -> TestResult {
        let directory = tempfile::tempdir()?;
        let artifact = directory.path().join("build");
        fs::create_dir(&artifact)?;
        fs::write(artifact.join("manifest.json"), "{}")?;
        let reservation = reserve(&artifact)?;
        assert!(artifact.join("image").is_dir());
        write_binary(
            &artifact.join("linux-binary/apsides-controller"),
            b"\x7fELF",
        )?;
        let mode = fs::metadata(artifact.join("linux-binary/apsides-controller"))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        fs::write(artifact.join("build-inputs.json"), "{}")?;
        drop(reservation);
        let mut left: Vec<String> = fs::read_dir(&artifact)?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        left.sort();
        assert_eq!(left, ["manifest.json"]);
        let reservation = reserve(&artifact)?;
        reservation.commit();
        assert!(artifact.join("image").is_dir());
        Ok(())
    }
}
