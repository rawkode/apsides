//! The build context: everything the compile container sees, staged beside
//! the artifact as `<artifact>.native-context` and identified file by file.
//!
//! The context holds the verified controller sources with the artifact's
//! exact lock, the trusted Apsides crates the controller depends on (their
//! `Cargo.toml` and `src/` only), every locked crates.io crate vendored, the
//! cargo configuration that points cargo at that vendor directory, and the
//! pinned `rusty_v8` archive. The compile mounts each of its five top-level
//! directories read-only, so its file manifest is the whole compile input.

use crate::{
    Error, Platform, Result, Stage, Validator,
    cargo::HostCargo,
    command::{Env, Run},
    digest::{HashingReader, digest},
    fs::{copy_tree, file_manifest, read_regular, write_new},
    redact::TAIL_LINES,
    rusty_v8::{self, CONTEXT_PATH, Fetch},
};
use release::RustyV8;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// The context's top-level directories, each mounted under `/build`.
pub const CONTEXT_DIRECTORIES: [&str; 5] =
    [".cargo", "controller", "rusty-v8", "vendor", "workspace"];
/// Points cargo at the vendored crates instead of crates.io.
pub const CARGO_CONFIG: &str = "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"vendor\"\n";
/// The Apsides crates a controller may depend on by path.
pub const ALLOWED_CRATES: [&str; 7] = [
    "compiler-core",
    "coordination",
    "deploy",
    "engine",
    "inventory",
    "platform",
    "runtime",
];
/// The suffix appended to the artifact's name for its context directory.
pub const SUFFIX: &str = ".native-context";
const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
const MAX_LOCK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CRATE_MANIFEST_BYTES: u64 = 1024 * 1024;
const METADATA_TIMEOUT: Duration = Duration::from_secs(300);
const VENDOR_TIMEOUT: Duration = Duration::from_mins(15);
/// Target kinds compiled into the controller, whose sources must be copied.
const COMPILED_KINDS: [&str; 6] = ["lib", "rlib", "proc-macro", "bin", "cdylib", "staticlib"];
/// Target kinds the controller build never compiles.
const IGNORED_KINDS: [&str; 3] = ["test", "bench", "example"];

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::Context, detail)
}

fn io_fail(action: &str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    move |error| fail(format!("cannot {action} {}: {error}", path.display()))
}

/// The generated controller's `Cargo.toml` for a runtime crate at
/// `runtime_path_json`, a path already quoted as a JSON string.
///
/// There is no `tokio` dependency: `main.rs` is synchronous and calls
/// `runtime::start`, which builds its own tokio runtime after Landlock
/// lockdown.
#[must_use]
pub fn controller_manifest(runtime_path_json: &str) -> String {
    format!(
        "[package]\nname = \"controller\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\nruntime = {{ path = {runtime_path_json} }}\n"
    )
}

/// What [`prepare`] needs.
pub struct ContextInputs<'a> {
    pub assets: &'a Path,
    pub artifact: &'a Path,
    pub platform: Platform,
    pub cargo: &'a HostCargo,
    pub validator: &'a dyn Validator,
    /// Where the `rusty_v8` archive is cached.
    pub rusty_v8_cache: &'a Path,
    pub fetcher: &'a dyn Fetch,
}

/// A published build context and its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub root: PathBuf,
    /// Every file, by `/`-separated relative path, mapped to its digest.
    pub files: BTreeMap<String, String>,
    pub ir_digest: String,
    pub controller_lock_sha256: String,
    pub rusty_v8: RustyV8,
    /// The Apsides crates staged under `workspace/crates`.
    pub crates: Vec<String>,
}

impl Context {
    /// Fails unless the context's files are exactly those it was published
    /// with.
    ///
    /// # Errors
    /// Fails in `stage`, naming the first changed, added and removed paths,
    /// and when the context cannot be read.
    pub fn verify_unchanged(&self, stage: Stage, phase: &str) -> Result<()> {
        let current = file_manifest(&self.root).map_err(|error| {
            Error::new(
                stage,
                format!(
                    "the build context {} cannot be read during {phase}: {error}",
                    self.root.display()
                ),
            )
        })?;
        if current == self.files {
            return Ok(());
        }
        let mut parts = Vec::new();
        let mut list = |label: &str, paths: Vec<&String>| {
            if !paths.is_empty() {
                let shown: Vec<&str> = paths.iter().take(5).map(|path| path.as_str()).collect();
                let more = paths.len().saturating_sub(shown.len());
                let suffix = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };
                parts.push(format!("{label} {}{suffix}", shown.join(", ")));
            }
        };
        list(
            "changed",
            self.files
                .iter()
                .filter(|(path, sha)| current.get(*path).is_some_and(|now| now != *sha))
                .map(|(path, _)| path)
                .collect(),
        );
        list(
            "added",
            current
                .keys()
                .filter(|path| !self.files.contains_key(*path))
                .collect(),
        );
        list(
            "removed",
            self.files
                .keys()
                .filter(|path| !current.contains_key(*path))
                .collect(),
        );
        Err(Error::new(
            stage,
            format!(
                "the build context {} changed during {phase}: {}",
                self.root.display(),
                parts.join("; ")
            ),
        ))
    }

    /// The host path of the context directory `directory`.
    #[must_use]
    pub fn mount(&self, directory: &str) -> PathBuf {
        self.root.join(directory)
    }
}

/// The context directory for `artifact`: its name with [`SUFFIX`] appended,
/// beside it.
///
/// # Errors
/// Fails when `artifact` has no file name or parent.
pub fn root_for(artifact: &Path) -> Result<PathBuf> {
    let (Some(parent), Some(name)) = (artifact.parent(), artifact.file_name()) else {
        return Err(fail(format!(
            "the artifact {} has no parent directory or name",
            artifact.display()
        )));
    };
    let mut name = name.to_owned();
    name.push(SUFFIX);
    Ok(parent.join(name))
}

/// Stages and publishes the build context for the verified artifact.
///
/// # Errors
/// Fails when the context directory exists, the artifact fails verification
/// or its lock, manifest or sources differ from what was verified, the
/// dependency closure leaves crates.io and the trusted Apsides crates, the
/// `rusty_v8` archive cannot be resolved, cargo cannot vendor every locked
/// crate offline, or anything changes while it is staged.
pub fn prepare(inputs: &ContextInputs<'_>) -> Result<Context> {
    let artifact = inputs
        .artifact
        .canonicalize()
        .map_err(io_fail("resolve", inputs.artifact))?;
    let assets = inputs
        .assets
        .canonicalize()
        .map_err(io_fail("resolve", inputs.assets))?;
    let root = root_for(&artifact)?;
    if fs::symlink_metadata(&root).is_ok() {
        return Err(fail(format!(
            "{} already exists; choose a fresh --output (aps build never overwrites)",
            root.display()
        )));
    }
    let verified = verify(inputs, &artifact, &assets)?;
    let artifact_metadata = metadata(
        inputs.cargo,
        &verified.manifest_path,
        &assets,
        "the artifact",
    )?;
    let closure = crate_closure(&artifact_metadata, &assets, &verified.manifest_path)?;
    let (archive, pin) = rusty_v8::resolve(
        &assets,
        inputs.platform.rust_target(),
        &closure.v8,
        inputs.rusty_v8_cache,
        inputs.fetcher,
    )?;

    let parent = root
        .parent()
        .ok_or_else(|| fail(format!("{} has no parent directory", root.display())))?;
    let staging = tempfile::Builder::new()
        .prefix(".apsides-context-")
        .tempdir_in(parent)
        .map_err(io_fail("create a staging directory in", parent))?;
    let stage = staging.path();
    stage_sources(stage, &artifact, &assets, &closure.crates, &verified.lock)?;
    check_controller_sources(stage, &verified.recorded)?;
    let staged_archive = stage.join(CONTEXT_PATH);
    copy_file(&archive, &staged_archive)?;
    if file_digest(&staged_archive)? != format!("sha256:{}", pin.sha256_hex) {
        return Err(fail(format!(
            "the staged rusty_v8 archive {} differs from its pin",
            staged_archive.display()
        )));
    }
    vendor(inputs, stage, &verified)?;
    let files = file_manifest(stage)
        .map_err(|error| fail(format!("the staged build context is unusable: {error}")))?;
    if let Some(stray) = files.keys().find(|path| {
        !CONTEXT_DIRECTORIES.iter().any(|directory| {
            path.strip_prefix(directory)
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }) {
        return Err(fail(format!(
            "the staged build context holds {stray}, outside the mounted directories"
        )));
    }

    // Claiming `root` with an exclusive mkdir before the rename keeps the
    // never-overwrite guarantee: rename(2) would silently replace an empty
    // directory that appeared at `root` while the context was staged.
    if let Err(error) = fs::create_dir(&root) {
        return Err(fail(if error.kind() == io::ErrorKind::AlreadyExists {
            format!(
                "{} already exists; choose a fresh --output (aps build never overwrites)",
                root.display()
            )
        } else {
            format!("cannot create {}: {error}", root.display())
        }));
    }
    let staged = staging.keep();
    if let Err(error) = fs::rename(&staged, &root) {
        let _ = fs::remove_dir_all(&staged);
        let _ = fs::remove_dir(&root);
        return Err(fail(format!(
            "cannot publish the build context {}: {error}",
            root.display()
        )));
    }
    seal(&root).map_err(io_fail("make read-only", &root))?;
    Ok(Context {
        root,
        files,
        ir_digest: verified.ir_digest,
        controller_lock_sha256: digest(&verified.lock),
        rusty_v8: RustyV8 {
            archive: rusty_v8::component(&pin),
            binding_file: None,
        },
        crates: closure.crates,
    })
}

/// What [`verify`] established about the artifact.
struct Verified {
    ir_digest: String,
    lock: Vec<u8>,
    recorded: BTreeMap<String, String>,
    manifest_path: PathBuf,
}

/// Verifies the artifact, its recorded lock and its generated manifest.
fn verify(inputs: &ContextInputs<'_>, artifact: &Path, assets: &Path) -> Result<Verified> {
    let ir_digest = inputs
        .validator
        .verify_artifact(artifact)
        .map_err(|error| fail(format!("the artifact failed verification: {error}")))?;
    let lock_path = artifact.join("controller/Cargo.lock");
    let lock = read_regular(&lock_path, MAX_LOCK_BYTES).map_err(io_fail("read", &lock_path))?;
    let recorded = recorded_files(artifact)?;
    if recorded.get("controller/Cargo.lock") != Some(&digest(&lock)) {
        return Err(fail(
            "artifact must contain a recorded exact controller/Cargo.lock; run the native build \
             first",
        ));
    }
    let manifest_path = artifact.join("controller/Cargo.toml");
    let runtime = assets.join("crates/runtime");
    let runtime = runtime
        .canonicalize()
        .map_err(io_fail("resolve", &runtime))?;
    let runtime_json = runtime
        .to_str()
        .and_then(|path| serde_json::to_string(path).ok())
        .ok_or_else(|| fail(format!("{} is not a UTF-8 path", runtime.display())))?;
    let generated = read_regular(&manifest_path, MAX_CRATE_MANIFEST_BYTES)
        .map_err(io_fail("read", &manifest_path))?;
    if generated != controller_manifest(&runtime_json).as_bytes() {
        return Err(fail(
            "generated controller/Cargo.toml differs from the supported runtime dependency \
             contract",
        ));
    }
    Ok(Verified {
        ir_digest,
        lock,
        recorded,
        manifest_path,
    })
}

/// Vendors every locked crate into `stage`, points cargo at them and checks
/// the staged controller still resolves, locked, to the verified IR.
fn vendor(inputs: &ContextInputs<'_>, stage: &Path, verified: &Verified) -> Result<()> {
    Run::new(
        inputs.cargo.program(),
        [
            "vendor",
            "--locked",
            "--offline",
            "--versioned-dirs",
            "--manifest-path",
            "controller/Cargo.toml",
            "vendor",
        ],
    )
    .cwd(stage)
    .env(Env::Inherit(inputs.cargo.environment()))
    .timeout(VENDOR_TIMEOUT)
    .capture()
    .map_err(|failure| {
        fail(format!(
            "cargo vendor could not stage every locked crate offline; populate the host Cargo \
             cache once with `cargo fetch --locked --manifest-path {}` while online\n{}",
            verified.manifest_path.display(),
            failure.describe("cargo vendor", TAIL_LINES)
        ))
    })?;
    // Written only now: cargo vendor itself must still reach the host cache.
    let cargo_config = stage.join(".cargo/config.toml");
    create_parent(&cargo_config)?;
    write_new(&cargo_config, CARGO_CONFIG.as_bytes(), 0o644)
        .map_err(io_fail("write", &cargo_config))?;
    let staged_lock = stage.join("controller/Cargo.lock");
    if read_regular(&staged_lock, MAX_LOCK_BYTES).map_err(io_fail("read", &staged_lock))?
        != verified.lock
    {
        return Err(fail("locked dependency preparation changed Cargo.lock"));
    }
    metadata(
        inputs.cargo,
        &stage.join("controller/Cargo.toml"),
        stage,
        "the staged context",
    )?;
    let staged_ir = inputs
        .validator
        .ir_digest(&stage.join("controller/src/ir.json"))
        .map_err(|error| {
            fail(format!(
                "the staged controller IR does not validate: {error}"
            ))
        })?;
    if staged_ir != verified.ir_digest {
        return Err(fail(
            "the staged controller IR does not validate to the artifact's irDigest",
        ));
    }
    Ok(())
}

/// `manifest.json`'s `files` map.
fn recorded_files(artifact: &Path) -> Result<BTreeMap<String, String>> {
    let path = artifact.join("manifest.json");
    let bytes = read_regular(&path, MAX_MANIFEST_BYTES).map_err(io_fail("read", &path))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| fail(format!("{} is not UTF-8", path.display())))?;
    let manifest = compiler_core::parse_strict(text)
        .map_err(|error| fail(format!("{} is not valid: {error}", path.display())))?;
    manifest["files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect()
        })
        .ok_or_else(|| fail(format!("{} has no files map", path.display())))
}

/// `cargo metadata` of `manifest_path`, locked and offline, run from `cwd`.
fn metadata(cargo: &HostCargo, manifest_path: &Path, cwd: &Path, what: &str) -> Result<Value> {
    let captured = Run::new(
        cargo.program(),
        [
            "metadata".into(),
            "--locked".into(),
            "--offline".into(),
            "--format-version".into(),
            "1".into(),
            "--manifest-path".into(),
            manifest_path.as_os_str().to_owned(),
        ],
    )
    .cwd(cwd)
    .env(Env::Inherit(cargo.environment()))
    .timeout(METADATA_TIMEOUT)
    .capture()
    .map_err(|failure| {
        fail(format!(
            "cargo metadata rejected {what}: {}",
            failure.describe("cargo metadata", TAIL_LINES)
        ))
    })?;
    serde_json::from_slice(&captured.stdout)
        .map_err(|error| fail(format!("cargo metadata of {what} is not JSON: {error}")))
}

/// What the controller's dependency graph needs staged.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Closure {
    /// The trusted Apsides crates, sorted.
    pub crates: Vec<String>,
    /// The locked crates.io `v8` versions.
    pub v8: Vec<String>,
}

/// The crates `metadata` (of the controller at `controller_manifest`) needs
/// staged, after checking that every package comes from crates.io or is a
/// trusted Apsides crate whose compiled targets live under its `src/`.
pub(crate) fn crate_closure(
    metadata: &Value,
    assets: &Path,
    controller_manifest: &Path,
) -> Result<Closure> {
    let controller = controller_manifest
        .canonicalize()
        .map_err(io_fail("resolve", controller_manifest))?;
    let mut crates = Vec::new();
    let mut v8 = Vec::new();
    let packages = metadata["packages"]
        .as_array()
        .ok_or_else(|| fail("cargo metadata listed no packages"))?;
    for package in packages {
        let name = package["name"].as_str().unwrap_or_default();
        match &package["source"] {
            Value::Null => {}
            Value::String(source) if source == CRATES_IO => {
                if let (true, Some(version)) = (name == "v8", package["version"].as_str()) {
                    v8.push(version.to_owned());
                }
                continue;
            }
            other => {
                return Err(fail(format!(
                    "controller/Cargo.lock names source {} for {name}; the network-less build \
                     vendors crates.io only",
                    other
                        .as_str()
                        .map_or_else(|| other.to_string(), str::to_owned)
                )));
            }
        }
        let declared = Path::new(package["manifest_path"].as_str().unwrap_or_default());
        let manifest = declared
            .canonicalize()
            .map_err(io_fail("resolve", declared))?;
        if manifest == controller {
            continue;
        }
        let escapes = || {
            fail(format!(
                "path dependency {} escapes the trusted Apsides crate set",
                manifest.display()
            ))
        };
        if !ALLOWED_CRATES.contains(&name) {
            return Err(escapes());
        }
        let crate_root = assets.join("crates").join(name);
        if crate_root.join("Cargo.toml").canonicalize().ok().as_ref() != Some(&manifest) {
            return Err(escapes());
        }
        let sources = crate_root.join("src");
        let sources = sources
            .canonicalize()
            .map_err(io_fail("resolve", &sources))?;
        for target in package["targets"].as_array().map_or(&[][..], Vec::as_slice) {
            let declared = Path::new(target["src_path"].as_str().unwrap_or_default());
            for kind in target["kind"].as_array().map_or(&[][..], Vec::as_slice) {
                let kind = kind.as_str().unwrap_or_default();
                if IGNORED_KINDS.contains(&kind) {
                    continue;
                }
                if kind == "custom-build" {
                    return Err(fail(format!(
                        "crate {name} has a build script ({}); trusted crates are staged without \
                         one, so the native context copies only Cargo.toml and src/",
                        declared.display()
                    )));
                }
                if !COMPILED_KINDS.contains(&kind) {
                    return Err(fail(format!(
                        "crate {name} has a {kind} target ({}), which the native context does not \
                         support",
                        declared.display()
                    )));
                }
                let inside = declared
                    .canonicalize()
                    .is_ok_and(|path| path.starts_with(&sources));
                if !inside {
                    return Err(fail(format!(
                        "crate {name} has a build target outside src/ ({}); the native context \
                         copies only Cargo.toml and src/",
                        declared.display()
                    )));
                }
            }
        }
        crates.push(name.to_owned());
    }
    crates.sort();
    crates.dedup();
    Ok(Closure { crates, v8 })
}

/// Writes the controller, the workspace manifest and the trusted crates into
/// `stage`.
fn stage_sources(
    stage: &Path,
    artifact: &Path,
    assets: &Path,
    crates: &[String],
    lock: &[u8],
) -> Result<()> {
    let write = |relative: &str, bytes: &[u8]| {
        let path = stage.join(relative);
        create_parent(&path)?;
        write_new(&path, bytes, 0o644).map_err(io_fail("write", &path))
    };
    write(
        "controller/Cargo.toml",
        controller_manifest("\"../workspace/crates/runtime\"").as_bytes(),
    )?;
    write("controller/Cargo.lock", lock)?;
    let tree = |from: &Path, to: &Path| {
        copy_tree(from, to).map_err(|error| {
            fail(format!(
                "cannot stage {} into the build context: {error}",
                from.display()
            ))
        })
    };
    tree(
        &artifact.join("controller/src"),
        &stage.join("controller/src"),
    )?;
    let read =
        |path: &Path| read_regular(path, MAX_CRATE_MANIFEST_BYTES).map_err(io_fail("read", path));
    write("workspace/Cargo.toml", &read(&assets.join("Cargo.toml"))?)?;
    for name in crates {
        let crate_root = assets.join("crates").join(name);
        write(
            &format!("workspace/crates/{name}/Cargo.toml"),
            &read(&crate_root.join("Cargo.toml"))?,
        )?;
        tree(
            &crate_root.join("src"),
            &stage.join("workspace/crates").join(name).join("src"),
        )?;
    }
    Ok(())
}

/// Fails unless the staged controller sources are exactly the ones the
/// artifact's manifest records.
fn check_controller_sources(stage: &Path, recorded: &BTreeMap<String, String>) -> Result<()> {
    let staged = file_manifest(&stage.join("controller/src")).map_err(|error| {
        fail(format!(
            "the artifact's controller sources are unusable: {error}"
        ))
    })?;
    for (relative, sha) in &staged {
        let key = format!("controller/src/{relative}");
        match recorded.get(&key) {
            None => {
                return Err(fail(format!(
                    "artifact file {key} is not recorded in manifest.json"
                )));
            }
            Some(expected) if expected != sha => {
                return Err(fail(format!(
                    "artifact file {key} differs from its manifest.json digest"
                )));
            }
            Some(_) => {}
        }
    }
    if let Some(missing) = recorded.keys().find(|key| {
        key.strip_prefix("controller/src/")
            .is_some_and(|relative| !staged.contains_key(relative))
    }) {
        return Err(fail(format!(
            "manifest.json records {missing}, which the artifact lacks"
        )));
    }
    Ok(())
}

fn create_parent(path: &Path) -> Result<()> {
    path.parent().map_or(Ok(()), |parent| {
        fs::create_dir_all(parent).map_err(io_fail("create", parent))
    })
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    create_parent(to)?;
    let bytes_from = File::open(from).map_err(io_fail("read", from))?;
    if !bytes_from
        .metadata()
        .map_err(io_fail("read", from))?
        .is_file()
    {
        return Err(fail(format!("{} is not a regular file", from.display())));
    }
    let mut reader = bytes_from;
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .map_err(io_fail("create", to))?;
    io::copy(&mut reader, &mut writer).map_err(io_fail("write", to))?;
    writer.sync_all().map_err(io_fail("sync", to))
}

fn file_digest(path: &Path) -> Result<String> {
    let mut reader = HashingReader::new(File::open(path).map_err(io_fail("read", path))?);
    io::copy(&mut reader, &mut io::sink()).map_err(io_fail("read", path))?;
    Ok(reader.finish().0)
}

/// Makes every file under `root` read-only (0444) and every directory 0555.
fn seal(root: &Path) -> io::Result<()> {
    set_modes(root, 0o444, 0o555)
}

/// Restores owner write permission throughout a published context, so it
/// can be removed.
///
/// # Errors
/// Fails when a permission cannot be changed.
pub fn unseal(root: &Path) -> io::Result<()> {
    set_modes(root, 0o644, 0o755)
}

fn set_modes(directory: &Path, file_mode: u32, directory_mode: u32) -> io::Result<()> {
    // Writable while its entries change, then the requested mode.
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            set_modes(&entry.path(), file_mode, directory_mode)?;
        } else if kind.is_file() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(file_mode))?;
        }
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(directory_mode))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tools::fake_tool;
    use serde_json::json;
    use std::{cell::Cell, io::Write, os::unix::fs::symlink};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    pub const IR_DIGEST: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const ARCHIVE: &[u8] = b"rusty v8 archive bytes";
    const LOCK: &str = "version = 4\n\n[[package]]\nname = \"controller\"\nversion = \"0.1.0\"\n";
    const TARGET: &str = "x86_64-unknown-linux-gnu";

    /// Accepts any artifact as `IR_DIGEST`, or rejects every IR.
    pub struct FakeValidator {
        pub ir: &'static str,
    }

    impl Validator for FakeValidator {
        fn verify_artifact(&self, _: &Path) -> std::result::Result<String, String> {
            Ok(IR_DIGEST.to_owned())
        }

        fn ir_digest(&self, ir: &Path) -> std::result::Result<String, String> {
            if fs::read(ir).is_err() {
                return Err("no IR".into());
            }
            Ok(self.ir.to_owned())
        }
    }

    /// Serves the fixture archive and counts calls.
    pub struct FakeFetch {
        pub calls: Cell<usize>,
    }

    impl Fetch for FakeFetch {
        fn fetch(&self, _: &str, sink: &mut dyn Write) -> std::result::Result<u64, String> {
            self.calls.set(self.calls.get() + 1);
            sink.write_all(ARCHIVE).map_err(|error| error.to_string())?;
            Ok(ARCHIVE.len() as u64)
        }
    }

    /// A temporary assets tree, artifact and fake host cargo.
    pub struct Fixture {
        pub directory: tempfile::TempDir,
        pub assets: PathBuf,
        pub artifact: PathBuf,
        pub cache: PathBuf,
        pub cargo: HostCargo,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = unseal(self.directory.path());
        }
    }

    /// The `cargo metadata` JSON for the fixture: the controller, the
    /// runtime crate with a test target, a crates.io `v8`.
    fn metadata_json(assets: &Path, controller: &Path, extra: &Value) -> Value {
        let runtime = assets.join("crates/runtime");
        let mut packages = vec![
            json!({"name": "controller", "version": "0.1.0", "source": null,
                   "manifest_path": controller.join("Cargo.toml"),
                   "targets": [{"kind": ["bin"], "src_path": controller.join("src/main.rs")}]}),
            json!({"name": "runtime", "version": "0.1.0", "source": null,
                   "manifest_path": runtime.join("Cargo.toml"),
                   "targets": [{"kind": ["lib"], "src_path": runtime.join("src/lib.rs")},
                               {"kind": ["test"], "src_path": runtime.join("tests/runtime.rs")}]}),
            json!({"name": "v8", "version": "150.4.0", "source": CRATES_IO,
                   "manifest_path": "/registry/v8/Cargo.toml", "targets": []}),
        ];
        if !extra.is_null() {
            packages.push(extra.clone());
        }
        json!({"packages": packages})
    }

    impl Fixture {
        pub fn new() -> TestResult<Self> {
            Self::with_cargo("")
        }

        /// A fixture whose fake cargo runs `vendor_extra` after vendoring.
        pub fn with_cargo(vendor_extra: &str) -> TestResult<Self> {
            Self::build(vendor_extra, "out")
        }

        /// A fixture whose artifact is `<out>/build` under the temporary
        /// directory.
        pub fn build(vendor_extra: &str, out: &str) -> TestResult<Self> {
            let directory = tempfile::tempdir()?;
            let assets = directory.path().join("assets");
            let runtime = assets.join("crates/runtime");
            fs::create_dir_all(runtime.join("src"))?;
            fs::write(
                assets.join("Cargo.toml"),
                "[workspace]\nmembers = [\"crates/runtime\"]\n",
            )?;
            fs::write(
                runtime.join("Cargo.toml"),
                "[package]\nname = \"runtime\"\n",
            )?;
            fs::write(runtime.join("src/lib.rs"), "pub fn start() {}\n")?;
            let pins = assets.join("scripts/rusty-v8");
            fs::create_dir_all(&pins)?;
            fs::write(
                pins.join("pins.json"),
                serde_json::to_vec(&json!({
                    "v8CrateVersion": "150.4.0",
                    "bindingFile": {"required": false},
                    "archives": {TARGET: {
                        "url": "https://example.invalid/librusty_v8.a.gz",
                        "sha256": crate::digest::sha256_hex(ARCHIVE),
                        "name": "librusty_v8",
                        "version": "150.4.0",
                    }},
                }))?,
            )?;
            let artifact = directory.path().join(out).join("build");
            let controller = artifact.join("controller");
            fs::create_dir_all(controller.join("src"))?;
            let runtime_json =
                serde_json::to_string(runtime.canonicalize()?.to_str().ok_or("non UTF-8 path")?)?;
            fs::write(
                controller.join("Cargo.toml"),
                controller_manifest(&runtime_json),
            )?;
            fs::write(controller.join("Cargo.lock"), LOCK)?;
            fs::write(controller.join("src/main.rs"), "fn main() {}\n")?;
            fs::write(controller.join("src/ir.json"), "{}\n")?;
            let mut files = BTreeMap::new();
            for name in ["Cargo.lock", "Cargo.toml", "src/ir.json", "src/main.rs"] {
                files.insert(
                    format!("controller/{name}"),
                    digest(&fs::read(controller.join(name))?),
                );
            }
            fs::write(
                artifact.join("manifest.json"),
                serde_json::to_vec(&json!({"files": files}))?,
            )?;
            let tools = directory.path().join("tools");
            fs::create_dir_all(&tools)?;
            let metadata = metadata_json(
                &assets.canonicalize()?,
                &controller.canonicalize()?,
                &Value::Null,
            );
            fs::write(tools.join("metadata.json"), serde_json::to_vec(&metadata)?)?;
            let script = format!(
                "case \"$1\" in\n\
                 metadata) cat '{metadata}';;\n\
                 vendor) mkdir -p vendor/itoa-1.0.0 && printf 'pub fn x() {{}}\\n' > vendor/itoa-1.0.0/lib.rs && \
                 printf '[source.crates-io]\\n'; {vendor_extra};;\n\
                 *) exit 64;;\n\
                 esac",
                metadata = tools.join("metadata.json").display(),
            );
            let cargo = fake_tool(&tools, "cargo", &script)?;
            Ok(Self {
                cache: directory.path().join("cache"),
                assets,
                artifact,
                cargo: HostCargo::with_program(cargo, None),
                directory,
            })
        }

        pub fn prepare_with(
            &self,
            validator: &dyn Validator,
            fetch: &FakeFetch,
        ) -> Result<Context> {
            prepare(&ContextInputs {
                assets: &self.assets,
                artifact: &self.artifact,
                platform: Platform::Amd64,
                cargo: &self.cargo,
                validator,
                rusty_v8_cache: &self.cache,
                fetcher: fetch,
            })
        }

        pub fn prepare(&self) -> Result<Context> {
            self.prepare_with(
                &FakeValidator { ir: IR_DIGEST },
                &FakeFetch {
                    calls: Cell::new(0),
                },
            )
        }

        pub fn leftovers(&self) -> TestResult<Vec<String>> {
            let out = self.artifact.parent().ok_or("no output directory")?;
            Ok(fs::read_dir(out)?
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| name != "build")
                .collect())
        }
    }

    #[test]
    fn the_generated_manifest_is_the_emitter_contract() {
        assert_eq!(
            controller_manifest("\"/src/apsides/crates/runtime\""),
            "[package]\nname = \"controller\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n[dependencies]\nruntime = { path = \"/src/apsides/crates/runtime\" }\n"
        );
    }

    #[test]
    fn a_context_is_staged_published_and_sealed() -> TestResult {
        let fixture = Fixture::new()?;
        let context = fixture.prepare()?;
        assert_eq!(context.root, root_for(&fixture.artifact.canonicalize()?)?);
        assert!(context.root.ends_with("out/build.native-context"));
        let paths: Vec<&str> = context.files.keys().map(String::as_str).collect();
        assert_eq!(
            paths,
            [
                ".cargo/config.toml",
                "controller/Cargo.lock",
                "controller/Cargo.toml",
                "controller/src/ir.json",
                "controller/src/main.rs",
                "rusty-v8/librusty_v8.a.gz",
                "vendor/itoa-1.0.0/lib.rs",
                "workspace/Cargo.toml",
                "workspace/crates/runtime/Cargo.toml",
                "workspace/crates/runtime/src/lib.rs",
            ]
        );
        assert_eq!(
            fs::read_to_string(context.mount(".cargo").join("config.toml"))?,
            CARGO_CONFIG
        );
        assert_eq!(
            fs::read_to_string(context.root.join("controller/Cargo.lock"))?,
            LOCK
        );
        let manifest = fs::read_to_string(context.root.join("controller/Cargo.toml"))?;
        assert!(manifest.contains("path = \"../workspace/crates/runtime\""));
        assert!(!manifest.contains(&fixture.directory.path().display().to_string()));
        assert_eq!(context.controller_lock_sha256, digest(LOCK.as_bytes()));
        assert_eq!(context.ir_digest, IR_DIGEST);
        assert_eq!(context.crates, ["runtime"]);
        assert_eq!(context.rusty_v8.archive.path, CONTEXT_PATH);
        assert_eq!(context.rusty_v8.archive.sha256, digest(ARCHIVE));
        assert_eq!(context.files[CONTEXT_PATH], digest(ARCHIVE));
        assert!(context.rusty_v8.binding_file.is_none());
        let mode = |path: &Path| fs::metadata(path).map(|meta| meta.permissions().mode() & 0o777);
        assert_eq!(mode(&context.root.join("controller/src/main.rs"))?, 0o444);
        assert_eq!(mode(&context.root.join("controller/src"))?, 0o555);
        assert!(
            fixture
                .leftovers()?
                .iter()
                .all(|name| name == "build.native-context")
        );
        context.verify_unchanged(Stage::Compile, "the compile")?;
        Ok(())
    }

    #[test]
    fn changes_after_publishing_are_named() -> TestResult {
        let fixture = Fixture::new()?;
        let context = fixture.prepare()?;
        unseal(&context.root)?;
        fs::write(context.root.join("controller/src/ir.json"), "{\"x\":1}\n")?;
        fs::write(context.root.join("vendor/extra.rs"), "")?;
        fs::remove_file(context.root.join("workspace/Cargo.toml"))?;
        let error = context
            .verify_unchanged(Stage::Sbom, "SBOM generation")
            .err()
            .ok_or("accepted")?;
        assert_eq!(error.stage, Stage::Sbom);
        assert_eq!(
            error.detail,
            format!(
                "the build context {} changed during SBOM generation: changed \
                 controller/src/ir.json; added vendor/extra.rs; removed workspace/Cargo.toml",
                context.root.display()
            )
        );
        Ok(())
    }

    #[test]
    fn an_existing_context_is_never_overwritten() -> TestResult {
        let fixture = Fixture::new()?;
        let root = root_for(&fixture.artifact.canonicalize()?)?;
        fs::create_dir(&root)?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert!(
            error
                .detail
                .ends_with("already exists; choose a fresh --output (aps build never overwrites)")
        );
        Ok(())
    }

    #[test]
    fn a_context_appearing_while_staging_is_never_replaced() -> TestResult {
        let fixture = Fixture::with_cargo("mkdir ../build.native-context")?;
        let root = root_for(&fixture.artifact.canonicalize()?)?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert_eq!(
            error.detail,
            format!(
                "{} already exists; choose a fresh --output (aps build never overwrites)",
                root.display()
            )
        );
        assert_eq!(fs::read_dir(&root)?.count(), 0);
        assert_eq!(fixture.leftovers()?, ["build.native-context"]);
        Ok(())
    }

    #[test]
    fn a_lock_changed_while_vendoring_leaves_nothing_behind() -> TestResult {
        let fixture = Fixture::with_cargo("echo '# changed' >> controller/Cargo.lock")?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "locked dependency preparation changed Cargo.lock"
        );
        assert_eq!(fixture.leftovers()?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_failed_vendor_names_the_fetch_remedy() -> TestResult {
        let fixture = Fixture::with_cargo("echo 'no matching package' >&2; exit 101")?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert!(error.detail.starts_with(
            "cargo vendor could not stage every locked crate offline; populate the host Cargo \
             cache once with `cargo fetch --locked --manifest-path"
        ));
        assert!(error.detail.contains("no matching package"));
        assert_eq!(fixture.leftovers()?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn an_altered_controller_manifest_breaks_the_contract() -> TestResult {
        let fixture = Fixture::new()?;
        let manifest = fixture.artifact.join("controller/Cargo.toml");
        let mut text = fs::read_to_string(&manifest)?;
        text.push_str("tokio = \"1\"\n");
        fs::write(&manifest, text)?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "generated controller/Cargo.toml differs from the supported runtime dependency contract"
        );
        Ok(())
    }

    #[test]
    fn unrecorded_or_altered_controller_sources_are_rejected() -> TestResult {
        let fixture = Fixture::new()?;
        fs::write(fixture.artifact.join("controller/src/extra.rs"), "")?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "artifact file controller/src/extra.rs is not recorded in manifest.json"
        );
        assert_eq!(fixture.leftovers()?, Vec::<String>::new());
        let fixture = Fixture::new()?;
        fs::write(
            fixture.artifact.join("controller/src/main.rs"),
            "fn main() { }\n",
        )?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "artifact file controller/src/main.rs differs from its manifest.json digest"
        );
        let fixture = Fixture::new()?;
        fs::write(
            fixture.artifact.join("controller/Cargo.lock"),
            "version = 3\n",
        )?;
        let error = fixture.prepare().err().ok_or("accepted")?;
        assert!(
            error
                .detail
                .starts_with("artifact must contain a recorded exact controller/Cargo.lock")
        );
        Ok(())
    }

    #[test]
    fn symlinked_and_special_sources_are_rejected() -> TestResult {
        for make in [
            |src: &Path| symlink("main.rs", src.join("linked.rs")),
            |src: &Path| symlink(".", src.join("loop")),
            |src: &Path| std::os::unix::net::UnixListener::bind(src.join("socket")).map(drop),
        ] {
            let fixture = Fixture::new()?;
            make(&fixture.artifact.join("controller/src"))?;
            let error = fixture.prepare().err().ok_or("accepted")?;
            assert!(error.detail.starts_with("cannot stage"), "{}", error.detail);
            assert_eq!(fixture.leftovers()?, Vec::<String>::new());
        }
        Ok(())
    }

    #[test]
    fn an_ir_digest_mismatch_is_rejected() -> TestResult {
        let fixture = Fixture::new()?;
        let error = fixture
            .prepare_with(
                &FakeValidator {
                    ir: "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                },
                &FakeFetch {
                    calls: Cell::new(0),
                },
            )
            .err()
            .ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "the staged controller IR does not validate to the artifact's irDigest"
        );
        Ok(())
    }

    #[test]
    fn the_archive_is_fetched_once_into_the_cache() -> TestResult {
        let fixture = Fixture::new()?;
        let fetch = FakeFetch {
            calls: Cell::new(0),
        };
        fixture.prepare_with(&FakeValidator { ir: IR_DIGEST }, &fetch)?;
        assert_eq!(fetch.calls.get(), 1);
        Ok(())
    }

    fn closure_error(extra: &Value) -> TestResult<String> {
        let fixture = Fixture::new()?;
        let assets = fixture.assets.canonicalize()?;
        let controller = fixture.artifact.join("controller").canonicalize()?;
        let metadata = metadata_json(&assets, &controller, extra);
        Ok(
            crate_closure(&metadata, &assets, &controller.join("Cargo.toml"))
                .err()
                .ok_or("accepted")?
                .detail,
        )
    }

    #[test]
    fn test_targets_are_ignored_and_the_closure_is_collected() -> TestResult {
        let fixture = Fixture::new()?;
        let assets = fixture.assets.canonicalize()?;
        let controller = fixture.artifact.join("controller").canonicalize()?;
        let closure = crate_closure(
            &metadata_json(&assets, &controller, &Value::Null),
            &assets,
            &controller.join("Cargo.toml"),
        )?;
        assert_eq!(
            closure,
            Closure {
                crates: vec!["runtime".into()],
                v8: vec!["150.4.0".into()]
            }
        );
        Ok(())
    }

    #[test]
    fn foreign_sources_paths_and_targets_are_rejected() -> TestResult {
        let detail = closure_error(&json!({"name": "left-pad", "version": "1.0.0",
            "source": "git+https://example.invalid/left-pad#abc", "manifest_path": "/x/Cargo.toml", "targets": []}))?;
        assert_eq!(
            detail,
            "controller/Cargo.lock names source git+https://example.invalid/left-pad#abc for left-pad; the network-less build vendors crates.io only"
        );
        let fixture = Fixture::new()?;
        let outside = fixture.directory.path().join("evil");
        fs::create_dir_all(outside.join("src"))?;
        fs::write(outside.join("Cargo.toml"), "")?;
        let detail = closure_error(
            &json!({"name": "release", "version": "0.1.0", "source": null,
            "manifest_path": outside.join("Cargo.toml"), "targets": []}),
        )?;
        assert!(
            detail.starts_with("path dependency")
                && detail.ends_with("escapes the trusted Apsides crate set"),
            "{detail}"
        );

        let fixture = Fixture::new()?;
        let assets = fixture.assets.canonicalize()?;
        let controller = fixture.artifact.join("controller").canonicalize()?;
        let runtime = assets.join("crates/runtime");
        fs::write(runtime.join("build.rs"), "fn main() {}\n")?;
        for (kind, path, expected) in [
            (
                "custom-build",
                runtime.join("build.rs"),
                "has a build script",
            ),
            (
                "lib",
                runtime.join("build.rs"),
                "has a build target outside src/",
            ),
            ("dylib", runtime.join("src/lib.rs"), "has a dylib target"),
        ] {
            let mut metadata = metadata_json(&assets, &controller, &Value::Null);
            metadata["packages"][1]["targets"]
                .as_array_mut()
                .ok_or("no targets")?
                .push(json!({"kind": [kind], "src_path": path}));
            let detail = crate_closure(&metadata, &assets, &controller.join("Cargo.toml"))
                .err()
                .ok_or(kind)?
                .detail;
            assert!(detail.contains(expected), "{detail}");
        }
        Ok(())
    }

    /// The real checkout: the generated controller's path dependencies are
    /// exactly the trusted crates, each with only `src/` targets compiled.
    #[test]
    fn the_checkout_closure_is_the_trusted_crate_set() -> TestResult {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let directory = tempfile::tempdir()?;
        let controller = directory.path().join("controller");
        fs::create_dir_all(controller.join("src"))?;
        let runtime_json =
            serde_json::to_string(assets.join("crates/runtime").to_str().ok_or("non UTF-8")?)?;
        fs::write(
            controller.join("Cargo.toml"),
            controller_manifest(&runtime_json),
        )?;
        fs::write(controller.join("src/main.rs"), "fn main() {}\n")?;
        fs::copy(assets.join("Cargo.lock"), controller.join("Cargo.lock"))?;
        let cargo = HostCargo::locate(&assets)?;
        let output = cargo
            .command()
            .args([
                "metadata",
                "--offline",
                "--format-version",
                "1",
                "--manifest-path",
            ])
            .arg(controller.join("Cargo.toml"))
            .current_dir(&assets)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Value = serde_json::from_slice(&output.stdout)?;
        let closure = crate_closure(&metadata, &assets, &controller.join("Cargo.toml"))?;
        assert_eq!(closure.crates, ALLOWED_CRATES);
        assert_eq!(closure.v8.len(), 1);
        Ok(())
    }
}
