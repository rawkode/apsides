//! The packaging sources: the files whose bytes decide what the native build
//! does.
//!
//! `build.rs` embeds their digests into this crate at compile time and the
//! compiler closure records them, so a build can show which packaging code
//! produced it.
//!
//! This module is also compiled into `build.rs`, so it uses only `std` and
//! `sha2`.

use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io,
    path::Path,
};

/// The crates whose `Cargo.toml` and `src/` are packaging sources.
pub const CRATES: [&str; 5] = ["cli", "compiler-core", "container", "packaging", "release"];
/// The workspace files that are packaging sources.
pub const ROOT_FILES: [&str; 2] = ["Cargo.lock", "Cargo.toml"];

/// Every packaging source under the workspace `root`, as
/// `(relative path, "sha256:<hex>")` sorted by path bytes.
///
/// # Errors
/// Fails on a symlink or other non-regular entry, a non-UTF-8 path and an
/// unreadable file.
pub fn collect(root: &Path) -> io::Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for name in ROOT_FILES {
        add(root, name, &mut files)?;
    }
    for name in CRATES {
        add(root, &format!("crates/{name}/Cargo.toml"), &mut files)?;
        walk(root, &format!("crates/{name}/src"), &mut files)?;
    }
    files.sort();
    Ok(files)
}

fn walk(root: &Path, directory: &str, files: &mut Vec<(String, String)>) -> io::Result<()> {
    let path = root.join(directory);
    if !fs::symlink_metadata(&path)?.is_dir() {
        return Err(invalid(directory, "is not a directory"));
    }
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid(directory, "holds a non-UTF-8 name"))?;
        let relative = format!("{directory}/{name}");
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk(root, &relative, files)?;
        } else {
            add(root, &relative, files)?;
        }
    }
    Ok(())
}

fn add(root: &Path, relative: &str, files: &mut Vec<(String, String)>) -> io::Result<()> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() {
        return Err(invalid(relative, "is a symlink"));
    }
    if !metadata.is_file() {
        return Err(invalid(relative, "is not a regular file"));
    }
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(&path)?, &mut hasher)?;
    files.push((
        relative.to_owned(),
        format!("sha256:{:x}", hasher.finalize()),
    ));
    Ok(())
}

fn invalid(path: &str, problem: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("packaging source {path} {problem}"),
    )
}
