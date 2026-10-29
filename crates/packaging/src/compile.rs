//! The controller compile: one network-less container of the pinned builder
//! image, which sees only the build context, a target directory and an empty
//! cargo home.
//!
//! Its argv, environment and mounts are the build's recipe. They are fixed
//! here and in `container::run_args`, so the packaging sources the compiler
//! closure hashes identify them. An environment probe in the same container
//! configuration fails the build when an engine adds anything to it.

use crate::{
    Cache, Error, Platform, Result, Stage,
    builder::{self, Builder},
    command::Run,
    context::{CARGO_CONFIG, CONTEXT_DIRECTORIES, Context},
    fs::read_regular,
    redact::{TAIL_LINES, redact},
    rusty_v8::resolved,
};
use container::{Engine, Mount, RunSpec, User};
use serde_json::json;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The compile recipe's version. Bump it whenever the argv, environment or
/// mounts change, so cached targets from the old recipe are never reused.
pub const RECIPE: &str = "apsides.dev/native-compile/v1";
pub const HOSTNAME: &str = "apsides-build";
pub const WORKDIR: &str = "/build";
/// The cargo home inside the container: always a fresh, empty directory.
pub const CARGO_HOME: &str = "/opt/apsides-cargo";
const TARGET: &str = "/build/target";
const COMPILE_TIMEOUT: Duration = Duration::from_hours(4);
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
const INFO_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const COMPILE_TAIL_LINES: usize = 40;
/// The probe prints its own environment, then the mounted cargo config, so
/// one run proves both the environment and that the engine sees the context.
const PROBE_ENTRYPOINT: &str = "/usr/bin/cat";
const PROBE_ARGS: [&str; 2] = ["/proc/self/environ", "/build/.cargo/config.toml"];
const COMPILE_ARGS: [&str; 5] = [
    "build",
    "--frozen",
    "--release",
    "--manifest-path",
    "controller/Cargo.toml",
];

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::Compile, detail)
}

/// What [`compile`] needs.
pub struct CompileInputs<'a> {
    pub engine: Engine,
    /// The engine CLI identified among the host tools.
    pub engine_program: &'a Path,
    pub builder: &'a Builder,
    pub context: &'a Context,
    pub cache: Cache,
    /// The assets root, which a build cache must stay out of.
    pub assets: &'a Path,
    pub epoch: u64,
    pub platform: Platform,
    /// Where temporary directories are created: the output's parent.
    pub scratch: &'a Path,
}

/// Compiles the controller in the builder image and returns the binary.
///
/// # Errors
/// Fails when the engine's mode cannot be read or is unsupported, a mount
/// path is unusable, the container environment differs from the contract,
/// the compile fails, the context or builder image changes, or the result is
/// not a Linux ELF binary for the platform.
pub fn compile(inputs: &CompileInputs<'_>) -> Result<Vec<u8>> {
    let cache_root = match inputs.cache {
        Cache::Enabled => Some(build_cache_dir(|name| std::env::var_os(name))?),
        Cache::Disabled => None,
    };
    compile_in(inputs, cache_root.as_deref(), COMPILE_TIMEOUT)
}

/// The build cache directory: `APSIDES_BUILD_CACHE`, else
/// `$XDG_CACHE_HOME/apsides/build`, else `$HOME/.cache/apsides/build`.
/// `lookup` reads the environment; an empty value counts as unset.
///
/// # Errors
/// Fails when none of the three is set.
pub fn build_cache_dir(lookup: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let set = |name: &str| {
        lookup(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    set("APSIDES_BUILD_CACHE")
        .or_else(|| set("XDG_CACHE_HOME").map(|cache| cache.join("apsides/build")))
        .or_else(|| set("HOME").map(|home| home.join(".cache/apsides/build")))
        .ok_or_else(|| {
            fail(
                "cannot determine a build cache directory: set APSIDES_BUILD_CACHE (or \
                 XDG_CACHE_HOME, or HOME), or pass --cache disabled",
            )
        })
}

/// The hex key of a cached target directory: everything that decides what
/// cargo compiles, apart from the controller sources.
///
/// Programs with the same dependency closure share a key; the controller
/// itself is recompiled on every build (see `forget_controller`), so only its
/// dependencies are reused.
///
/// # Errors
/// Fails when the key cannot be serialised.
pub fn cache_key(builder: &Builder, epoch: u64, context: &Context) -> Result<String> {
    let trusted: BTreeMap<&str, &str> = context
        .files
        .iter()
        .filter(|(path, _)| path.starts_with("workspace/") || *path == ".cargo/config.toml")
        .map(|(path, sha)| (path.as_str(), sha.as_str()))
        .collect();
    let serialise = |value: &serde_json::Value| {
        compiler_core::canonical_bytes(value)
            .map_err(|error| fail(format!("cannot serialise the build cache key: {error}")))
    };
    let trusted = crate::digest::digest(&serialise(&json!(trusted))?);
    let key = json!({
        "recipe": RECIPE,
        "reference": builder.reference,
        "platform": builder.platform.as_str(),
        "epoch": epoch,
        "cargoLockSha256": context.controller_lock_sha256,
        "rustyV8Sha256": context.rusty_v8.archive.sha256,
        "trustedSources": trusted,
    });
    Ok(crate::digest::sha256_hex(&serialise(&key)?))
}

/// The container environment for a builder image whose `Config.Env` is
/// `builder_env`: the image's own variables, with cargo pointed at the
/// mounted directories, offline, and the source date epoch set.
///
/// # Errors
/// Fails on a malformed or duplicate image variable.
pub fn expected_environment(
    builder_env: &[String],
    epoch: u64,
) -> Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    for entry in builder_env {
        let Some((key, value)) = entry.split_once('=').filter(|(key, _)| !key.is_empty()) else {
            return Err(fail(format!(
                "the builder image declares a malformed environment entry {:?}",
                redact(entry)
            )));
        };
        if environment
            .insert(key.to_owned(), value.to_owned())
            .is_some()
        {
            return Err(fail(format!(
                "the builder image declares {key} more than once"
            )));
        }
    }
    environment.remove("CARGO_HOME");
    for (key, value) in [
        ("CARGO_HOME", CARGO_HOME.to_owned()),
        ("CARGO_NET_OFFLINE", "true".to_owned()),
        ("CARGO_TARGET_DIR", TARGET.to_owned()),
        ("HOME", CARGO_HOME.to_owned()),
        (
            "RUSTY_V8_ARCHIVE",
            format!("/build/{}", crate::rusty_v8::CONTEXT_PATH),
        ),
        ("SOURCE_DATE_EPOCH", epoch.to_string()),
    ] {
        environment.insert(key.to_owned(), value);
    }
    Ok(environment)
}

/// The target directory a compile writes into, held for the whole compile.
enum Target {
    /// A fresh directory, removed afterwards.
    Fresh(tempfile::TempDir),
    /// A cached directory, locked against concurrent builds of the same key.
    Cached { path: PathBuf, _lock: File },
}

impl Target {
    fn path(&self) -> &Path {
        match self {
            Self::Fresh(directory) => directory.path(),
            Self::Cached { path, .. } => path,
        }
    }

    /// Removes a fresh directory and releases a cached one's lock.
    fn close(self) {
        if let Self::Fresh(directory) = self
            && let Err(error) = directory.close()
        {
            eprintln!("aps: warning: cannot remove the build's target directory: {error}");
        }
    }
}

/// [`compile`] with an explicit cache root (`None` for a fresh target) and
/// compile deadline.
pub(crate) fn compile_in(
    inputs: &CompileInputs<'_>,
    cache_root: Option<&Path>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let engine = inputs.engine;
    let program = inputs.engine_program;
    if inputs.builder.platform != inputs.platform {
        return Err(fail(format!(
            "builder image {} is {} but the build profile declares {}",
            inputs.builder.reference,
            inputs.builder.platform.as_str(),
            inputs.platform.as_str()
        )));
    }
    let security = security(engine, program)?;
    let user = if engine == Engine::Docker && security.rootless {
        User::Root
    } else {
        let (uid, gid) = host_ids()?;
        User::HostIds { uid, gid }
    };
    let target = match cache_root {
        None => Target::Fresh(temporary(
            inputs.scratch,
            ".apsides-target-",
            "a target directory",
        )?),
        Some(root) => cached_target(root, inputs)?,
    };
    let cargo_home = temporary(inputs.scratch, ".apsides-cargo-home-", "a cargo home")?;
    let environment = expected_environment(&inputs.builder.env, inputs.epoch)?;
    let mounts = mounts(inputs.context, target.path(), cargo_home.path())?;
    let run = |name: &str, entrypoint: &str, args: &[&str]| -> Result<Vec<OsString>> {
        check_mounts(engine, &mounts)?;
        container::run_args(
            engine,
            &RunSpec {
                name,
                image: &inputs.builder.id,
                platform: inputs.platform.as_str(),
                hostname: HOSTNAME,
                workdir: WORKDIR,
                user,
                rootless: security.rootless,
                mounts: &mounts,
                env: &environment,
                entrypoint,
                args,
            },
        )
        .map_err(|container::RunError::MountPath(path)| unmountable(engine, &path))
    };

    let probe_name = container_name();
    probe(
        inputs,
        Run::new(program, run(&probe_name, PROBE_ENTRYPOINT, &PROBE_ARGS)?),
        &probe_name,
        &environment,
    )?;
    inputs
        .context
        .verify_unchanged(Stage::Compile, "the compile preparation")?;

    let compile_name = container_name();
    let compiled = Run::new(program, run(&compile_name, "cargo", &COMPILE_ARGS)?)
        .timeout(timeout)
        .on_timeout(container::remove_args(&compile_name))
        .stream();
    if let Err(failure) = compiled {
        return Err(fail(compile_failure(
            &failure,
            &compile_name,
            target.path(),
            timeout,
        )));
    }
    inputs
        .context
        .verify_unchanged(Stage::Compile, "the controller compile")?;
    let after = builder::inspect(engine, program, &inputs.builder.reference)
        .map_err(|error| fail(error.detail))?;
    if &after != inputs.builder {
        return Err(fail(format!(
            "the builder image changed in the {} store during the build",
            engine.name()
        )));
    }
    let binary = collect(&target.path().join("release/controller"), inputs.platform);
    target.close();
    if let Err(error) = cargo_home.close() {
        eprintln!("aps: warning: cannot remove the build's cargo home: {error}");
    }
    binary
}

/// A fresh directory in `scratch` named with `prefix`.
fn temporary(scratch: &Path, prefix: &str, what: &str) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(scratch)
        .map_err(|error| {
            fail(format!(
                "cannot create {what} in {}: {error}",
                scratch.display()
            ))
        })
}

/// Runs the environment probe and checks what it printed.
fn probe(
    inputs: &CompileInputs<'_>,
    run: Run,
    name: &str,
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let probed = run
        .timeout(PROBE_TIMEOUT)
        .on_timeout(container::remove_args(name))
        .capture()
        .map_err(|failure| {
            if failure.timed_out() {
                return fail(format!(
                    "the build environment probe did not finish within {} s; container {name} \
                     was removed",
                    PROBE_TIMEOUT.as_secs()
                ));
            }
            if failure.tail.contains(".cargo/config.toml") {
                return unseen(inputs.engine, inputs.context);
            }
            fail(failure.describe("the build environment probe", TAIL_LINES))
        })?;
    check_probe(inputs.engine, inputs.context, &probed.stdout, environment)
}

/// The compiled controller at `path`, which must be a Linux ELF binary for
/// `platform`.
fn collect(path: &Path, platform: Platform) -> Result<Vec<u8>> {
    let binary = match read_regular(path, MAX_BINARY_BYTES) {
        Ok(binary) => binary,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(fail(format!(
                "cargo reported success but {} does not exist",
                path.display()
            )));
        }
        Err(error) => return Err(fail(format!("cannot read {}: {error}", path.display()))),
    };
    release::validate_controller_binary(&binary, platform.as_str()).map_err(|_| {
        fail(format!(
            "the compiled controller is not a Linux ELF binary for {}",
            platform.as_str()
        ))
    })?;
    Ok(binary)
}

fn security(engine: Engine, program: &Path) -> Result<container::Security> {
    let captured = Run::new(program, container::security_args(engine))
        .timeout(INFO_TIMEOUT)
        .capture()
        .map_err(|failure| {
            fail(failure.describe(&format!("{} info", program.display()), TAIL_LINES))
        })?;
    let security = container::parse_security(engine, &String::from_utf8_lossy(&captured.stdout))
        .map_err(|error| fail(error.to_string()))?;
    if security.userns_remap {
        return Err(fail(format!(
            "the {} daemon remaps user namespaces (userns-remap), so the build's files would \
             belong to another host user; aps build needs a daemon without userns-remap",
            engine.name()
        )));
    }
    Ok(security)
}

/// This process's effective uid and gid: the owner of a file it just
/// created.
fn host_ids() -> Result<(u32, u32)> {
    let file = tempfile::tempfile()
        .map_err(|error| fail(format!("cannot create a temporary file: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| fail(format!("cannot inspect a temporary file: {error}")))?;
    Ok((metadata.uid(), metadata.gid()))
}

/// The cached target directory for this build's key, locked exclusively
/// until the binary has been read.
fn cached_target(root: &Path, inputs: &CompileInputs<'_>) -> Result<Target> {
    let unusable = |path: &Path, error: &dyn std::fmt::Display| {
        fail(format!(
            "cannot use the build cache {}: {error}; set APSIDES_BUILD_CACHE to a writable \
             directory, or pass --cache disabled",
            path.display()
        ))
    };
    let root = resolved(root).map_err(|error| unusable(root, &error))?;
    let assets = inputs
        .assets
        .canonicalize()
        .map_err(|error| unusable(inputs.assets, &error))?;
    if root.starts_with(&assets) {
        return Err(fail(format!(
            "the build cache {} is inside {}; set APSIDES_BUILD_CACHE to a directory outside \
             any installed Apsides CLI distribution prefix",
            root.display(),
            assets.display()
        )));
    }
    let key = cache_key(inputs.builder, inputs.epoch, inputs.context)?;
    let targets = root.join("cargo-target");
    let path = targets.join(&key);
    fs::create_dir_all(&path).map_err(|error| unusable(&path, &error))?;
    let lock_path = targets.join(format!("{key}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| unusable(&lock_path, &error))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            eprintln!(
                "aps: waiting for another aps build using build cache key {key} at {} (pass \
                 --cache disabled to build without it)",
                path.display()
            );
            lock.lock().map_err(|error| unusable(&lock_path, &error))?;
        }
        Err(TryLockError::Error(error)) => return Err(unusable(&lock_path, &error)),
    }
    forget_controller(&path.join("release")).map_err(|error| unusable(&path, &error))?;
    Ok(Target::Cached { path, _lock: lock })
}

/// Removes the controller's binary, dep-info and cargo fingerprint from a
/// cached `release` directory, so cargo always recompiles it.
///
/// The cache key leaves out the controller sources, and every program's
/// controller has the same package id and container paths. Cargo decides
/// freshness of a path package by mtime, so a build whose context was staged
/// before another build of the same key compiled would otherwise see that
/// build's controller as fresh and reuse a binary embedding another program's
/// IR. Its dependencies stay cached.
fn forget_controller(release: &Path) -> io::Result<()> {
    let is_controller = |name: &str| {
        let name = name.strip_suffix(".d").unwrap_or(name);
        name == "controller"
            || name.strip_prefix("controller-").is_some_and(|hash| {
                !hash.is_empty() && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    };
    for directory in [
        release.to_path_buf(),
        release.join(".fingerprint"),
        release.join("deps"),
    ] {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_name().to_str().is_some_and(is_controller) {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

/// The bind mounts: each context directory read-only, then the target and
/// cargo home writable.
fn mounts(context: &Context, target: &Path, cargo_home: &Path) -> Result<Vec<Mount>> {
    let canonical = |path: &Path| {
        path.canonicalize()
            .map_err(|error| fail(format!("cannot resolve {}: {error}", path.display())))
    };
    let mut mounts = Vec::with_capacity(CONTEXT_DIRECTORIES.len() + 2);
    for (directory, mount_target) in CONTEXT_DIRECTORIES.into_iter().zip([
        "/build/.cargo",
        "/build/controller",
        "/build/rusty-v8",
        "/build/vendor",
        "/build/workspace",
    ]) {
        mounts.push(Mount {
            source: canonical(&context.mount(directory))?,
            target: mount_target,
            read_only: true,
        });
    }
    mounts.push(Mount {
        source: canonical(target)?,
        target: TARGET,
        read_only: false,
    });
    mounts.push(Mount {
        source: canonical(cargo_home)?,
        target: CARGO_HOME,
        read_only: false,
    });
    Ok(mounts)
}

/// Right before a run: every mount source is still a real directory, not a
/// symlink swapped in.
fn check_mounts(engine: Engine, mounts: &[Mount]) -> Result<()> {
    for mount in mounts {
        let is_directory = fs::symlink_metadata(&mount.source).is_ok_and(|meta| meta.is_dir());
        if !is_directory {
            return Err(fail(format!(
                "{} is no longer a directory, so {} cannot mount it at {}",
                mount.source.display(),
                engine.name(),
                mount.target
            )));
        }
    }
    Ok(())
}

fn unmountable(engine: Engine, path: &Path) -> Error {
    fail(format!(
        "the path {} cannot be bind-mounted by {} --mount (it contains a comma or is not UTF-8); \
         choose an output path without commas",
        path.display(),
        engine.program()
    ))
}

fn unseen(engine: Engine, context: &Context) -> Error {
    fail(format!(
        "the {} daemon cannot see {}; aps build needs a local engine",
        engine.name(),
        context.mount(".cargo").display()
    ))
}

/// A container name unique to this process and moment. It is never visible
/// inside the container.
fn container_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("apsides-build-{}-{nanos:x}", std::process::id())
}

/// Fails unless the probe printed exactly the expected environment and the
/// mounted cargo config.
fn check_probe(
    engine: Engine,
    context: &Context,
    output: &[u8],
    environment: &BTreeMap<String, String>,
) -> Result<()> {
    let text = String::from_utf8_lossy(output);
    let mut parts: Vec<&str> = text.split('\0').collect();
    let config = parts.pop().unwrap_or_default();
    if config != CARGO_CONFIG {
        return Err(unseen(engine, context));
    }
    let mut expected = environment.clone();
    expected.insert("HOSTNAME".to_owned(), HOSTNAME.to_owned());
    let show = |key: &str, value: &str| {
        if key == "HOME" || key == "PATH" {
            format!("{key}={value}")
        } else {
            format!("{key}=[REDACTED]")
        }
    };
    let mut seen = BTreeMap::new();
    let mut unexpected = Vec::new();
    for entry in parts.into_iter().filter(|entry| !entry.is_empty()) {
        let (key, value) = entry.split_once('=').unwrap_or((entry, ""));
        seen.insert(key, value);
        if expected.get(key).is_none_or(|wanted| wanted != value) {
            unexpected.push(show(key, value));
        }
    }
    let missing: Vec<&str> = expected
        .keys()
        .map(String::as_str)
        .filter(|key| !seen.contains_key(key))
        .collect();
    if unexpected.is_empty() && missing.is_empty() {
        return Ok(());
    }
    let mut differences = Vec::new();
    if !unexpected.is_empty() {
        differences.push(format!("unexpected {}", unexpected.join(", ")));
    }
    if !missing.is_empty() {
        differences.push(format!("missing {}", missing.join(", ")));
    }
    Err(fail(format!(
        "the {} container environment differs from the build contract: {}; remove engine or \
         containers.conf defaults that inject variables",
        engine.name(),
        differences.join("; ")
    )))
}

fn compile_failure(
    failure: &crate::command::Failure,
    name: &str,
    target: &Path,
    timeout: Duration,
) -> String {
    let mut message = if failure.timed_out() {
        format!(
            "the controller compile did not finish within {} s; container {name} was removed",
            timeout.as_secs()
        )
    } else {
        failure.describe("the controller compile", COMPILE_TAIL_LINES)
    };
    let hint = if failure.tail.contains("Read-only file system") {
        Some(
            "a build script tried to write into its source tree, which the network-less build \
             mounts read-only"
                .to_owned(),
        )
    } else if failure.tail.contains("No space left on device") {
        Some(format!(
            "free disk space under {} (or pass --cache disabled / set APSIDES_BUILD_CACHE)",
            target.display()
        ))
    } else if failure.tail.contains("could not find `Cargo.toml`")
        || failure.tail.contains("failed to read")
    {
        Some("the staged context is incomplete; rerun with a fresh output".to_owned())
    } else {
        None
    };
    if let Some(hint) = hint {
        message.push('\n');
        message.push_str(&hint);
    }
    message.push_str("\n(full compile output is above on stderr)");
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{context::tests::Fixture, tools::fake_tool};
    use std::{sync::Arc, thread, time::Instant};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    const REFERENCE: &str =
        "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922";

    fn pinned_env() -> Vec<String> {
        [
            "PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "RUSTUP_HOME=/usr/local/rustup",
            "CARGO_HOME=/usr/local/cargo",
            "RUST_VERSION=1.98.0",
        ]
        .map(str::to_owned)
        .into()
    }

    fn builder() -> Builder {
        Builder {
            reference: REFERENCE.to_owned(),
            platform: Platform::Amd64,
            id: "a3535ab4".to_owned(),
            repo_digests: vec![format!("docker.io/library/{REFERENCE}")],
            env: pinned_env(),
        }
    }

    #[test]
    fn the_pinned_builder_gets_the_contract_environment() -> TestResult {
        let environment = expected_environment(&pinned_env(), 0)?;
        let rendered: Vec<String> = environment
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        assert_eq!(
            rendered,
            [
                "CARGO_HOME=/opt/apsides-cargo",
                "CARGO_NET_OFFLINE=true",
                "CARGO_TARGET_DIR=/build/target",
                "HOME=/opt/apsides-cargo",
                "PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "RUSTUP_HOME=/usr/local/rustup",
                "RUSTY_V8_ARCHIVE=/build/rusty-v8/librusty_v8.a.gz",
                "RUST_VERSION=1.98.0",
                "SOURCE_DATE_EPOCH=0",
            ]
        );
        assert!(expected_environment(&["NOEQUALS".to_owned()], 0).is_err());
        assert!(expected_environment(&["A=1".to_owned(), "A=2".to_owned()], 0).is_err());
        Ok(())
    }

    #[test]
    fn the_build_cache_follows_the_environment() -> TestResult {
        let lookup = |values: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                values
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            build_cache_dir(lookup(&[("APSIDES_BUILD_CACHE", "/c"), ("HOME", "/h")]))?,
            PathBuf::from("/c")
        );
        assert_eq!(
            build_cache_dir(lookup(&[
                ("APSIDES_BUILD_CACHE", ""),
                ("XDG_CACHE_HOME", "/x")
            ]))?,
            PathBuf::from("/x/apsides/build")
        );
        assert_eq!(
            build_cache_dir(lookup(&[("HOME", "/h")]))?,
            PathBuf::from("/h/.cache/apsides/build")
        );
        let error = build_cache_dir(lookup(&[])).err().ok_or("accepted")?;
        assert!(
            error
                .detail
                .starts_with("cannot determine a build cache directory")
        );
        Ok(())
    }

    #[test]
    fn the_cache_key_is_stable_and_covers_its_inputs() -> TestResult {
        let fixture = Fixture::new()?;
        let context = fixture.prepare()?;
        let key = cache_key(&builder(), 0, &context)?;
        assert_eq!(key, cache_key(&builder(), 0, &context)?);
        assert_eq!(key.len(), 64);
        assert_ne!(key, cache_key(&builder(), 1, &context)?);
        let mut changed = context.clone();
        changed
            .files
            .insert("workspace/Cargo.toml".into(), crate::digest::digest(b"x"));
        assert_ne!(key, cache_key(&builder(), 0, &changed)?);
        let mut controller_only = context;
        controller_only
            .files
            .insert("controller/src/main.rs".into(), crate::digest::digest(b"x"));
        assert_eq!(key, cache_key(&builder(), 0, &controller_only)?);
        Ok(())
    }

    fn probe_output(environment: &BTreeMap<String, String>, extra: &[&str]) -> Vec<u8> {
        let mut output = Vec::new();
        for (key, value) in environment {
            output.extend_from_slice(format!("{key}={value}\0").as_bytes());
        }
        output.extend_from_slice(b"HOSTNAME=apsides-build\0");
        for entry in extra {
            output.extend_from_slice(entry.as_bytes());
            output.push(0);
        }
        output.extend_from_slice(CARGO_CONFIG.as_bytes());
        output
    }

    #[test]
    fn the_probe_rejects_injected_missing_or_unseen_state() -> TestResult {
        let fixture = Fixture::new()?;
        let context = fixture.prepare()?;
        let environment = expected_environment(&pinned_env(), 0)?;
        check_probe(
            Engine::Podman,
            &context,
            &probe_output(&environment, &[]),
            &environment,
        )?;
        let error = check_probe(
            Engine::Podman,
            &context,
            &probe_output(&environment, &["container=podman"]),
            &environment,
        )
        .err()
        .ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "the Podman container environment differs from the build contract: unexpected \
             container=[REDACTED]; remove engine or containers.conf defaults that inject variables"
        );
        let mut fewer = environment.clone();
        fewer.remove("RUST_VERSION");
        fewer.insert("HOME".into(), "/root".into());
        let error = check_probe(
            Engine::Docker,
            &context,
            &probe_output(&fewer, &[]),
            &environment,
        )
        .err()
        .ok_or("accepted")?;
        assert!(
            error
                .detail
                .contains("unexpected HOME=/root; missing RUST_VERSION"),
            "{}",
            error.detail
        );
        let mut unseen = probe_output(&environment, &[]);
        unseen.truncate(unseen.len() - CARGO_CONFIG.len());
        let error = check_probe(Engine::Docker, &context, &unseen, &environment)
            .err()
            .ok_or("accepted")?;
        assert!(error.detail.starts_with("the Docker daemon cannot see"));
        Ok(())
    }

    /// A fake podman: `info` says rootless, `image inspect` describes the
    /// builder, and `run` acts out the probe or the compile. The compile
    /// writes an ELF header followed by `marker` into the target mount,
    /// after running `compile_extra`.
    fn engine(
        directory: &Path,
        name: &str,
        marker: &str,
        compile_extra: &str,
    ) -> std::io::Result<PathBuf> {
        let script = format!(
            r#"echo "$*" >> "$0.log"
case "$1 $2" in
  "info --format") echo true; exit 0;;
  "image inspect") echo '[{{"Id":"a3535ab4","Os":"linux","Architecture":"amd64","RepoDigests":["docker.io/library/{REFERENCE}"],"Config":{{"Env":["PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin","RUSTUP_HOME=/usr/local/rustup","CARGO_HOME=/usr/local/cargo","RUST_VERSION=1.98.0"]}}}}]'; exit 0;;
  "rm --force") exit 0;;
esac
entry=; target=; cargo=; prev=; envs=
for arg in "$@"; do
  case "$prev" in
    --entrypoint) entry=$arg;;
    --env) envs="$envs$arg
";;
    --mount) case "$arg" in
      *target=/build/target) target=${{arg#type=bind,source=}}; target=${{target%,target=/build/target}};;
      *target=/build/.cargo,readonly) cargo=${{arg#type=bind,source=}}; cargo=${{cargo%,target=/build/.cargo,readonly}};;
    esac;;
  esac
  prev=$arg
done
if [ "$entry" = /usr/bin/cat ]; then
  printf '%s' "$envs" | while IFS= read -r line; do printf '%s\0' "$line"; done
  printf 'HOSTNAME=apsides-build\0'
  cat "$cargo/config.toml"
  exit 0
fi
{compile_extra}
mkdir -p "$target/release"
{{ printf '\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\002\000\076\000'; head -c 44 /dev/zero; printf '{marker}'; }} > "$target/release/controller"
"#
        );
        fake_tool(directory, name, &script)
    }

    struct Setup {
        fixture: Fixture,
        context: Context,
        builder: Builder,
    }

    fn setup() -> TestResult<Setup> {
        let fixture = Fixture::new()?;
        let context = fixture.prepare()?;
        Ok(Setup {
            fixture,
            context,
            builder: builder(),
        })
    }

    impl Setup {
        fn inputs<'a>(&'a self, program: &'a Path, cache: Cache) -> CompileInputs<'a> {
            CompileInputs {
                engine: Engine::Podman,
                engine_program: program,
                builder: &self.builder,
                context: &self.context,
                cache,
                assets: &self.fixture.assets,
                epoch: 0,
                platform: Platform::Amd64,
                scratch: self
                    .fixture
                    .artifact
                    .parent()
                    .unwrap_or_else(|| Path::new("/")),
            }
        }
    }

    #[test]
    fn a_compile_probes_runs_and_returns_the_binary() -> TestResult {
        let setup = setup()?;
        let program = engine(setup.fixture.directory.path(), "podman", "one", "")?;
        let binary = compile_in(
            &setup.inputs(&program, Cache::Disabled),
            None,
            COMPILE_TIMEOUT,
        )?;
        assert!(binary.starts_with(b"\x7fELF") && binary.ends_with(b"one"));
        let log = fs::read_to_string(program.with_extension("log"))?;
        let runs: Vec<&str> = log
            .lines()
            .filter(|line| line.starts_with("run "))
            .collect();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].contains("--userns=keep-id") && runs[0].contains("--unsetenv-all"));
        assert!(runs[0].ends_with(
            "--entrypoint /usr/bin/cat a3535ab4 /proc/self/environ /build/.cargo/config.toml"
        ));
        assert!(runs[1].ends_with(
            "--entrypoint cargo a3535ab4 build --frozen --release --manifest-path controller/Cargo.toml"
        ));
        assert!(runs[1].contains("--network=none") && runs[1].contains("--pull=never"));
        assert!(
            setup
                .fixture
                .leftovers()?
                .iter()
                .all(|name| name == "build.native-context")
        );
        Ok(())
    }

    #[test]
    fn a_context_changed_during_the_compile_is_detected() -> TestResult {
        let setup = setup()?;
        let ir = setup.context.root.join("controller/src/ir.json");
        let program = engine(
            setup.fixture.directory.path(),
            "podman",
            "x",
            &format!("chmod u+w '{0}' && echo changed > '{0}'", ir.display()),
        )?;
        let error = compile_in(
            &setup.inputs(&program, Cache::Disabled),
            None,
            COMPILE_TIMEOUT,
        )
        .err()
        .ok_or("accepted")?;
        assert!(
            error
                .detail
                .contains("changed during the controller compile: changed controller/src/ir.json"),
            "{}",
            error.detail
        );
        Ok(())
    }

    #[test]
    fn a_missing_binary_or_failed_compile_is_explained() -> TestResult {
        let setup = setup()?;
        let program = engine(setup.fixture.directory.path(), "podman", "x", "exit 0")?;
        let error = compile_in(
            &setup.inputs(&program, Cache::Disabled),
            None,
            COMPILE_TIMEOUT,
        )
        .err()
        .ok_or("accepted")?;
        assert!(
            error.detail.starts_with("cargo reported success but"),
            "{}",
            error.detail
        );
        assert!(error.detail.ends_with("release/controller does not exist"));
        let program = engine(
            setup.fixture.directory.path(),
            "podman2",
            "x",
            "echo 'error: Read-only file system (os error 30)'; exit 101",
        )?;
        let error = compile_in(
            &setup.inputs(&program, Cache::Disabled),
            None,
            COMPILE_TIMEOUT,
        )
        .err()
        .ok_or("accepted")?;
        assert!(error.detail.contains("exited with status 101"));
        assert!(
            error
                .detail
                .contains("a build script tried to write into its source tree")
        );
        assert!(
            error
                .detail
                .ends_with("(full compile output is above on stderr)")
        );
        Ok(())
    }

    #[test]
    fn a_timed_out_compile_removes_its_container() -> TestResult {
        let setup = setup()?;
        let program = engine(setup.fixture.directory.path(), "podman", "x", "sleep 20")?;
        let started = Instant::now();
        let error = compile_in(
            &setup.inputs(&program, Cache::Disabled),
            None,
            Duration::from_millis(500),
        )
        .err()
        .ok_or("accepted")?;
        assert!(started.elapsed() < Duration::from_secs(15));
        assert!(
            error
                .detail
                .contains("did not finish within 0 s; container apsides-build-")
        );
        let log = fs::read_to_string(program.with_extension("log"))?;
        assert!(
            log.lines()
                .any(|line| line.starts_with("rm --force apsides-build-"))
        );
        Ok(())
    }

    /// Two builds of one cache key: the second waits for the first to read
    /// its binary before its own compile can overwrite it.
    #[test]
    fn a_cached_target_is_locked_until_the_binary_is_read() -> TestResult {
        let setup = Arc::new(setup()?);
        let cache = setup.fixture.directory.path().join("cache-root");
        // The first compile writes its binary, then lingers before exiting.
        let lingering = engine(setup.fixture.directory.path(), "lingering", "first", "")?;
        let script = fs::read_to_string(&lingering)?;
        fs::write(&lingering, format!("{script}\nsleep 2\n"))?;
        let second = engine(setup.fixture.directory.path(), "second", "second", "")?;
        let holder = {
            let setup = Arc::clone(&setup);
            let cache = cache.clone();
            thread::spawn(move || {
                compile_in(
                    &setup.inputs(&lingering, Cache::Enabled),
                    Some(&cache),
                    COMPILE_TIMEOUT,
                )
            })
        };
        thread::sleep(Duration::from_millis(700));
        let waiter = compile_in(
            &setup.inputs(&second, Cache::Enabled),
            Some(&cache),
            COMPILE_TIMEOUT,
        )?;
        let held = holder.join().map_err(|_| "the first build panicked")??;
        assert!(
            held.ends_with(b"first"),
            "the first build read another build's binary"
        );
        assert!(waiter.ends_with(b"second"));
        Ok(())
    }

    /// A fake cargo that, like cargo's mtime check on a context staged
    /// before another build of the key compiled, treats an existing
    /// controller fingerprint as fresh and leaves the old binary in place.
    #[test]
    fn a_cached_controller_is_always_recompiled() -> TestResult {
        let setup = setup()?;
        let cache = setup.fixture.directory.path().join("cache-root");
        let fresh = "fingerprint=\"$target/release/.fingerprint/controller-0123456789abcdef\"\n\
                     [ -d \"$fingerprint\" ] && exit 0\n\
                     mkdir -p \"$fingerprint\"";
        let mut binaries = Vec::new();
        for marker in ["first", "second"] {
            let program = engine(setup.fixture.directory.path(), marker, marker, fresh)?;
            binaries.push(compile_in(
                &setup.inputs(&program, Cache::Enabled),
                Some(&cache),
                COMPILE_TIMEOUT,
            )?);
        }
        assert!(binaries[0].ends_with(b"first"));
        assert!(
            binaries[1].ends_with(b"second"),
            "the second build reused the first build's controller"
        );
        Ok(())
    }

    #[test]
    fn only_the_controller_is_forgotten() -> TestResult {
        let directory = tempfile::tempdir()?;
        let release = directory.path().join("release");
        let removed = [
            "controller",
            "controller.d",
            ".fingerprint/controller-0123456789abcdef/bin-controller",
            "deps/controller-0123456789abcdef",
            "deps/controller-0123456789abcdef.d",
        ];
        let kept = [
            ".fingerprint/controller-macros-0123456789abcdef/lib",
            ".fingerprint/serde-0123456789abcdef/lib",
            "deps/libserde-0123456789abcdef.rlib",
            "deps/controller_extra-0123456789abcdef.d",
            "build/controller-0123456789abcdef.txt",
        ];
        for path in removed.iter().chain(&kept) {
            let path = release.join(path);
            fs::create_dir_all(path.parent().ok_or("no parent")?)?;
            fs::write(path, "")?;
        }
        forget_controller(&release)?;
        forget_controller(&directory.path().join("absent"))?;
        for path in removed {
            assert!(!release.join(path).exists(), "{path} survived");
        }
        assert!(
            !release
                .join(".fingerprint/controller-0123456789abcdef")
                .exists()
        );
        for path in kept {
            assert!(release.join(path).exists(), "{path} was removed");
        }
        Ok(())
    }

    #[test]
    fn a_cache_inside_the_assets_is_rejected() -> TestResult {
        let setup = setup()?;
        let program = engine(setup.fixture.directory.path(), "podman", "x", "")?;
        let error = compile_in(
            &setup.inputs(&program, Cache::Enabled),
            Some(&setup.fixture.assets.join("cache")),
            COMPILE_TIMEOUT,
        )
        .err()
        .ok_or("accepted")?;
        assert!(error.detail.contains("is inside"), "{}", error.detail);
        Ok(())
    }

    /// Compiles a dependency-free crate in the pinned builder through real
    /// podman with the build's argv, twice, and expects identical binaries.
    ///
    /// Run with `cargo test --locked -p packaging -- --ignored
    /// real_podman_compile` on a host with podman and the pinned builder
    /// image in its store.
    #[test]
    #[ignore = "needs podman and the pinned builder image in its store"]
    fn real_podman_compile() -> TestResult {
        let program = container::find_on_path("podman").ok_or("podman is not on PATH")?;
        let reference = format!("docker.io/library/{REFERENCE}");
        let builder = builder::inspect(Engine::Podman, &program, &reference)?;
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("build.native-context");
        for (path, contents) in [
            (".cargo/config.toml", CARGO_CONFIG),
            (
                "controller/Cargo.toml",
                "[package]\nname = \"controller\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
            ),
            (
                "controller/Cargo.lock",
                "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"controller\"\nversion = \"0.1.0\"\n",
            ),
            (
                "controller/src/main.rs",
                "fn main() {\n    println!(\"{}\", env!(\"CARGO_PKG_NAME\"));\n}\n",
            ),
            ("rusty-v8/librusty_v8.a.gz", "unused"),
            ("workspace/Cargo.toml", "[workspace]\n"),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().ok_or("no parent")?)?;
            fs::write(path, contents)?;
        }
        fs::create_dir_all(root.join("vendor"))?;
        let context = Context {
            files: crate::fs::file_manifest(&root)?,
            root,
            ir_digest: crate::digest::digest(b"ir"),
            controller_lock_sha256: crate::digest::digest(b"lock"),
            rusty_v8: release::RustyV8 {
                archive: release::RustyV8Component {
                    name: "librusty_v8".into(),
                    version: "0".into(),
                    target: Platform::Amd64.rust_target().into(),
                    url: "https://example.invalid".into(),
                    sha256: crate::digest::digest(b"unused"),
                    path: crate::rusty_v8::CONTEXT_PATH.into(),
                },
                binding_file: None,
            },
            crates: Vec::new(),
        };
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let inputs = CompileInputs {
            engine: Engine::Podman,
            engine_program: &program,
            builder: &builder,
            context: &context,
            cache: Cache::Disabled,
            assets: &assets,
            epoch: 0,
            platform: builder.platform,
            scratch: directory.path(),
        };
        let first = compile_in(&inputs, None, COMPILE_TIMEOUT)?;
        let second = compile_in(&inputs, None, COMPILE_TIMEOUT)?;
        assert!(first.starts_with(b"\x7fELF"));
        assert_eq!(
            crate::digest::digest(&first),
            crate::digest::digest(&second)
        );
        eprintln!(
            "real_podman_compile: {} bytes, {}",
            first.len(),
            crate::digest::digest(&first)
        );
        Ok(())
    }
}
