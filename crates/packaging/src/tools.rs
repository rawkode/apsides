//! The host tools a build ran, identified by their executable bytes and the
//! version each reports, and rechecked before the build's outputs are kept.

use crate::{
    Error, Result, Stage,
    cargo::HostCargo,
    command::{Env, Run},
    digest::HashingReader,
    redact::bounded,
};
use container::Engine;
use release::HostTool;
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

/// The roles, in the order the compiler closure lists them.
pub const ROLES: [&str; 4] = ["engine", "cargo", "apsides", "sbom"];
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_VERSION_BYTES: usize = 4096;
const FAILURE_LINES: usize = 20;

/// The identified tools, with the path each was invoked by and the file that
/// path resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    tools: Vec<HostTool>,
    resolved: [PathBuf; 4],
    invoked: [PathBuf; 4],
}

/// Identifies the engine CLI at `engine_program`, host cargo, this `aps`
/// and the SBOM tool at `sbom`.
///
/// # Errors
/// Fails when a tool cannot be resolved, hashed or asked for its version.
pub fn capture(
    engine: Engine,
    engine_program: &Path,
    cargo: &HostCargo,
    sbom: &Path,
) -> Result<Snapshot> {
    let apsides = std::env::current_exe().map_err(|error| {
        Error::new(
            Stage::HostTools,
            format!("cannot locate the running aps executable: {error}"),
        )
    })?;
    capture_with(engine, engine_program, cargo, &apsides, sbom)
}

/// [`capture`] with an explicit `aps` executable.
pub(crate) fn capture_with(
    engine: Engine,
    engine_program: &Path,
    cargo: &HostCargo,
    apsides: &Path,
    sbom: &Path,
) -> Result<Snapshot> {
    let invoked = [
        engine_program.to_path_buf(),
        cargo.program().to_path_buf(),
        apsides.to_path_buf(),
        sbom.to_path_buf(),
    ];
    let mut tools = Vec::with_capacity(ROLES.len());
    let mut resolved = Vec::with_capacity(ROLES.len());
    for (role, path) in ROLES.into_iter().zip(&invoked) {
        let identity = path.canonicalize().map_err(|error| {
            Error::new(
                Stage::HostTools,
                format!(
                    "cannot resolve host tool {role} ({}): {error}",
                    path.display()
                ),
            )
        })?;
        let sha256 = file_digest(&identity).map_err(|error| {
            Error::new(
                Stage::HostTools,
                format!(
                    "cannot hash host tool {role} ({}): {error}",
                    identity.display()
                ),
            )
        })?;
        let version = match role {
            "engine" => {
                let client = version(role, path, Run::new(path, container::client_version_args()))?;
                let server = version(
                    role,
                    path,
                    Run::new(path, container::server_version_args(engine)),
                )?;
                format!("{client}; server {server}")
            }
            "cargo" => version(
                role,
                path,
                Run::new(path, ["--version"]).env(Env::Inherit(cargo.environment())),
            )?,
            "apsides" => apsides_version(),
            _ => version(role, path, Run::new(path, ["cyclonedx", "--version"]))?,
        };
        tools.push(HostTool {
            role: role.to_owned(),
            sha256,
            version,
        });
        resolved.push(identity);
    }
    let resolved: [PathBuf; 4] = resolved
        .try_into()
        .map_err(|_| Error::new(Stage::HostTools, "host tool identification is incomplete"))?;
    Ok(Snapshot {
        tools,
        resolved,
        invoked,
    })
}

/// The version `aps --version` prints first: clap's `aps <version>`.
#[must_use]
pub fn apsides_version() -> String {
    format!("aps {}", env!("CARGO_PKG_VERSION"))
}

impl Snapshot {
    /// The identities the compiler closure records.
    #[must_use]
    pub fn public(&self) -> Vec<HostTool> {
        self.tools.clone()
    }

    /// The SBOM tool as it was invoked.
    #[must_use]
    pub fn sbom_path(&self) -> &Path {
        &self.invoked[3]
    }

    /// The identified SBOM tool's digest.
    #[must_use]
    pub fn sbom_sha256(&self) -> &str {
        &self.tools[3].sha256
    }

    /// Identifies the same tools again and fails unless every byte, version
    /// and path is unchanged.
    ///
    /// # Errors
    /// Fails, naming the roles, when anything changed, and as [`capture`].
    pub fn verify_unchanged(
        &self,
        engine: Engine,
        engine_program: &Path,
        cargo: &HostCargo,
        sbom: &Path,
    ) -> Result<()> {
        let current = capture_with(engine, engine_program, cargo, &self.invoked[2], sbom)?;
        self.compare(&current)
    }

    fn compare(&self, current: &Self) -> Result<()> {
        let changed: Vec<&str> = ROLES
            .iter()
            .enumerate()
            .filter(|&(index, _)| {
                self.tools[index] != current.tools[index]
                    || self.resolved[index] != current.resolved[index]
                    || self.invoked[index] != current.invoked[index]
            })
            .map(|(_, role)| *role)
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        Err(Error::new(
            Stage::HostTools,
            format!(
                "host tools changed during the build: {}; rerun the build with stable tools",
                changed.join(", ")
            ),
        ))
    }
}

fn file_digest(path: &Path) -> io::Result<String> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut reader = HashingReader::new(File::open(path)?);
    io::copy(&mut reader, &mut io::sink())?;
    Ok(reader.finish().0)
}

/// The whitespace-collapsed stdout of `run`, which must be a usable version.
fn version(role: &str, path: &Path, run: Run) -> Result<String> {
    let captured = run.timeout(VERSION_TIMEOUT).capture().map_err(|failure| {
        let mut detail = format!(
            "cannot identify host tool {role} ({}): {}",
            path.display(),
            bounded(
                &failure.describe("the version query", FAILURE_LINES),
                FAILURE_LINES + 1
            )
        );
        if role == "cargo" && failure.tail.contains("rustup") {
            detail.push_str(
                "; set RUSTUP_TOOLCHAIN or install the toolchain named in rust-toolchain.toml",
            );
        }
        Error::new(Stage::HostTools, detail)
    })?;
    let text = String::from_utf8_lossy(&captured.stdout);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty()
        || collapsed.len() > MAX_VERSION_BYTES
        || collapsed.chars().any(char::is_control)
    {
        return Err(Error::new(
            Stage::HostTools,
            format!(
                "host tool {role} ({}) reported an unusable version: it must be 1 to \
                 {MAX_VERSION_BYTES} bytes without control characters",
                path.display()
            ),
        ));
    }
    Ok(collapsed)
}

/// Writes an executable `/bin/sh` script, a fake tool for tests across the
/// crate.
#[cfg(test)]
pub(crate) fn fake_tool(directory: &Path, name: &str, script: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\n{script}\n"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::digest;
    use std::os::unix::fs::symlink;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    struct Fakes {
        _directory: tempfile::TempDir,
        engine: PathBuf,
        cargo: HostCargo,
        apsides: PathBuf,
        sbom: PathBuf,
    }

    fn fakes() -> io::Result<Fakes> {
        let directory = tempfile::tempdir()?;
        let engine = fake_tool(
            directory.path(),
            "podman",
            "if [ \"$1\" = --version ]; then echo 'podman   version\n 5.8.9'; else echo 5.8.9; fi",
        )?;
        let cargo = fake_tool(
            directory.path(),
            "cargo",
            "echo \"cargo 1.98.0 ($RUSTUP_TOOLCHAIN)\"",
        )?;
        let apsides = fake_tool(directory.path(), "aps", "exit 0")?;
        let sbom = fake_tool(
            directory.path(),
            "cargo-cyclonedx",
            "[ \"$1 $2\" = 'cyclonedx --version' ] && echo 'cargo-cyclonedx 0.5.9'",
        )?;
        Ok(Fakes {
            _directory: directory,
            engine,
            cargo: HostCargo::with_program(cargo, Some("1.98.0".into())),
            apsides,
            sbom,
        })
    }

    fn snapshot(fakes: &Fakes) -> Result<Snapshot> {
        capture_with(
            Engine::Podman,
            &fakes.engine,
            &fakes.cargo,
            &fakes.apsides,
            &fakes.sbom,
        )
    }

    #[test]
    fn versions_are_collapsed_and_bytes_hashed() -> TestResult {
        let fakes = fakes()?;
        let snapshot = snapshot(&fakes)?;
        let tools = snapshot.public();
        let roles: Vec<&str> = tools.iter().map(|tool| tool.role.as_str()).collect();
        assert_eq!(roles, ROLES);
        assert_eq!(tools[0].version, "podman version 5.8.9; server 5.8.9");
        assert_eq!(tools[1].version, "cargo 1.98.0 (1.98.0)");
        assert_eq!(tools[2].version, apsides_version());
        assert_eq!(tools[3].version, "cargo-cyclonedx 0.5.9");
        assert_eq!(tools[3].sha256, digest(&fs::read(&fakes.sbom)?));
        assert_eq!(snapshot.sbom_path(), fakes.sbom);
        assert_eq!(snapshot.sbom_sha256(), tools[3].sha256);
        Ok(())
    }

    #[test]
    fn a_changed_byte_or_version_names_the_role() -> TestResult {
        let fakes = fakes()?;
        let before = snapshot(&fakes)?;
        fake_tool(
            fakes.sbom.parent().ok_or("no parent")?,
            "cargo-cyclonedx",
            "echo 'cargo-cyclonedx 0.6.0'",
        )?;
        let error = before.compare(&snapshot(&fakes)?).err().ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "host tools changed during the build: sbom; rerun the build with stable tools"
        );
        let mut appended = fs::read(&fakes.engine)?;
        appended.extend_from_slice(b"\n# changed\n");
        fs::write(&fakes.engine, appended)?;
        let error = before.compare(&snapshot(&fakes)?).err().ok_or("accepted")?;
        assert!(error.detail.contains(": engine, sbom;"), "{}", error.detail);
        Ok(())
    }

    #[test]
    fn a_symlinked_invocation_recaptures_equal() -> TestResult {
        let fakes = fakes()?;
        let link = fakes.engine.with_file_name("podman-link");
        symlink(&fakes.engine, &link)?;
        let first = capture_with(
            Engine::Podman,
            &link,
            &fakes.cargo,
            &fakes.apsides,
            &fakes.sbom,
        )?;
        first.verify_unchanged(Engine::Podman, &link, &fakes.cargo, &fakes.sbom)?;
        assert_eq!(first.resolved[0], fakes.engine.canonicalize()?);
        assert_eq!(first.invoked[0], link);
        Ok(())
    }

    #[test]
    fn a_failing_or_empty_version_is_rejected() -> TestResult {
        let fakes = fakes()?;
        let directory = fakes.sbom.parent().ok_or("no parent")?;
        fake_tool(
            directory,
            "cargo-cyclonedx",
            "echo 'token=abc123 broke' >&2; exit 2",
        )?;
        let error = snapshot(&fakes).err().ok_or("accepted")?;
        assert!(error.detail.starts_with(&format!(
            "cannot identify host tool sbom ({}): the version query failed",
            fakes.sbom.display()
        )));
        assert!(error.detail.contains("token=[REDACTED]"));
        fake_tool(directory, "cargo-cyclonedx", "printf ' \\n'")?;
        let error = snapshot(&fakes).err().ok_or("accepted")?;
        assert!(error.detail.contains("reported an unusable version"));
        Ok(())
    }
}
