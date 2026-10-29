//! Filesystem helpers that never follow a symlink.

use crate::digest::HashingReader;
use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// The bytes of the regular file at `path`, which must not be a symlink and
/// must hold at most `max` bytes. Hard links are allowed.
///
/// # Errors
/// Fails with [`io::ErrorKind::InvalidData`] when `path` is a symlink, is not
/// a regular file or holds more than `max` bytes, and with the underlying
/// error when it cannot be read.
pub fn read_regular(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected a regular non-symlink file of at most {max} bytes"),
        )
    };
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid());
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(invalid());
    }
    Ok(bytes)
}

/// Why [`file_manifest`] refused a tree.
#[derive(Debug)]
pub enum ManifestError {
    Io {
        path: PathBuf,
        error: io::Error,
    },
    /// A symlink, to a file or a directory, at this relative path.
    Symlink(String),
    /// A FIFO, socket, device or other non-regular entry.
    NotRegular(String),
    /// A path that is not UTF-8.
    NotUtf8(PathBuf),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, error } => {
                write!(formatter, "cannot read {}: {error}", path.display())
            }
            Self::Symlink(path) => write!(formatter, "{path} is a symlink"),
            Self::NotRegular(path) => write!(formatter, "{path} is not a regular file"),
            Self::NotUtf8(path) => write!(formatter, "{} is not a UTF-8 path", path.display()),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Every regular file under `root`, keyed by its `/`-separated relative path,
/// mapped to its digest. Keys sort in byte order, which is the order release
/// validation requires.
///
/// # Errors
/// Fails on a symlink anywhere in the tree, on any other non-regular entry,
/// on a non-UTF-8 path and on a read failure.
pub fn file_manifest(root: &Path) -> Result<BTreeMap<String, String>, ManifestError> {
    let mut files = BTreeMap::new();
    walk_manifest(root, "", &mut files)?;
    Ok(files)
}

fn walk_manifest(
    directory: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, String>,
) -> Result<(), ManifestError> {
    let failed = |path: &Path| {
        let path = path.to_path_buf();
        move |error| ManifestError::Io { path, error }
    };
    for entry in fs::read_dir(directory).map_err(failed(directory))? {
        let entry = entry.map_err(failed(directory))?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| ManifestError::NotUtf8(path.clone()))?;
        let relative = format!("{prefix}{name}");
        let kind = entry.file_type().map_err(failed(&path))?;
        if kind.is_symlink() {
            return Err(ManifestError::Symlink(relative));
        }
        if kind.is_dir() {
            walk_manifest(&path, &format!("{relative}/"), files)?;
        } else if kind.is_file() {
            let file = File::open(&path).map_err(failed(&path))?;
            if !file.metadata().map_err(failed(&path))?.is_file() {
                return Err(ManifestError::NotRegular(relative));
            }
            let mut reader = HashingReader::new(file);
            io::copy(&mut reader, &mut io::sink()).map_err(failed(&path))?;
            files.insert(relative, reader.finish().0);
        } else {
            return Err(ManifestError::NotRegular(relative));
        }
    }
    Ok(())
}

/// Copies the regular files under `source` into `destination`, creating
/// directories as needed. Copies get fresh timestamps.
///
/// # Errors
/// Fails with [`io::ErrorKind::InvalidData`] naming the path on a symlink or
/// other non-regular entry, and with the underlying error otherwise.
pub fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    mirror(source, destination, &|from, to| {
        fs::copy(from, to).map(|_| ())
    })
}

/// [`copy_tree`] by hard link, falling back to a copy for a file on another
/// filesystem.
///
/// # Errors
/// As [`copy_tree`].
pub fn link_tree(source: &Path, destination: &Path) -> io::Result<()> {
    mirror(
        source,
        destination,
        &|from, to| match fs::hard_link(from, to) {
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                fs::copy(from, to).map(|_| ())
            }
            linked => linked,
        },
    )
}

fn mirror(
    source: &Path,
    destination: &Path,
    place: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if !fs::symlink_metadata(source)?.is_dir() {
        return Err(not_regular(source));
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            mirror(&from, &to, place)?;
        } else if kind.is_file() {
            place(&from, &to)?;
        } else {
            return Err(not_regular(&from));
        }
    }
    Ok(())
}

fn not_regular(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} is a symlink or not a regular file or directory",
            path.display()
        ),
    )
}

/// Creates `path`, which must not exist, with exactly `mode`, writes `bytes`
/// and syncs it to disk.
///
/// # Errors
/// Fails when `path` exists or cannot be created, written or synced.
pub fn write_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::digest;
    use std::os::unix::fs::symlink;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn tree() -> io::Result<tempfile::TempDir> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("controller/src"))?;
        fs::write(root.path().join("controller/src/main.rs"), "fn main() {}\n")?;
        fs::write(root.path().join(".cargo"), "x")?;
        Ok(root)
    }

    #[test]
    fn read_regular_rejects_links_directories_and_oversized_files() -> TestResult {
        let root = tree()?;
        let file = root.path().join("controller/src/main.rs");
        assert_eq!(read_regular(&file, 13)?, b"fn main() {}\n");
        let link = root.path().join("link");
        symlink(&file, &link)?;
        for (path, max) in [(&file, 12), (&link, 100), (&root.path().to_path_buf(), 100)] {
            let error = read_regular(path, max).err().ok_or("accepted")?;
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        let hard = root.path().join("hard");
        fs::hard_link(&file, &hard)?;
        assert_eq!(read_regular(&hard, 100)?, b"fn main() {}\n");
        Ok(())
    }

    #[test]
    fn the_manifest_maps_relative_paths_to_digests() -> TestResult {
        let root = tree()?;
        let manifest = file_manifest(root.path())?;
        assert_eq!(
            manifest.into_iter().collect::<Vec<_>>(),
            [
                (".cargo".to_owned(), digest(b"x")),
                (
                    "controller/src/main.rs".to_owned(),
                    digest(b"fn main() {}\n")
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn the_manifest_rejects_symlinked_files_and_directories() -> TestResult {
        let root = tree()?;
        symlink("main.rs", root.path().join("controller/src/escape.rs"))?;
        assert!(matches!(
            file_manifest(root.path()),
            Err(ManifestError::Symlink(path)) if path == "controller/src/escape.rs"
        ));
        let root = tree()?;
        symlink("src", root.path().join("controller/linked"))?;
        assert!(matches!(
            file_manifest(root.path()),
            Err(ManifestError::Symlink(path)) if path == "controller/linked"
        ));
        Ok(())
    }

    #[test]
    fn the_manifest_rejects_non_regular_entries() -> TestResult {
        let root = tree()?;
        let _socket = std::os::unix::net::UnixListener::bind(root.path().join("socket"))?;
        assert!(matches!(
            file_manifest(root.path()),
            Err(ManifestError::NotRegular(path)) if path == "socket"
        ));
        Ok(())
    }

    #[test]
    fn trees_copy_and_link_regular_files_only() -> TestResult {
        let root = tree()?;
        let copied = root.path().join("copied");
        copy_tree(&root.path().join("controller"), &copied)?;
        let linked = root.path().join("linked");
        link_tree(&root.path().join("controller"), &linked)?;
        for tree in [&copied, &linked] {
            assert_eq!(fs::read(tree.join("src/main.rs"))?, b"fn main() {}\n");
        }
        symlink("main.rs", root.path().join("controller/src/escape.rs"))?;
        let error = copy_tree(&root.path().join("controller"), &root.path().join("again"))
            .err()
            .ok_or("a symlink was copied")?;
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = link_tree(
            &root.path().join("controller"),
            &root.path().join("relinked"),
        )
        .err()
        .ok_or("a symlink was linked")?;
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        Ok(())
    }

    #[test]
    fn write_new_sets_the_mode_and_never_overwrites() -> TestResult {
        let root = tempfile::tempdir()?;
        let path = root.path().join("apsides-controller");
        write_new(&path, b"binary", 0o755)?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o7777, 0o755);
        assert_eq!(
            write_new(&path, b"other", 0o755)
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(&path)?, b"binary");
        Ok(())
    }
}
