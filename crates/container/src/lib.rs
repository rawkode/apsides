#![forbid(unsafe_code)]
//! What aps knows about the local container engine: which one to use, the
//! argv of every engine command it runs, and how to read their output.
//!
//! The crate never starts a process. The kind test runner drives engines
//! asynchronously and the native build drives them synchronously, so each
//! runs the argv built here with its own runner.
//!
//! podman is used when it is on `PATH` and Docker otherwise.
//! [`OVERRIDE`] (`podman` or `docker`) overrides the choice, so a host with
//! both can keep the images it already pulled into one engine's store.

mod info;
mod inspect;
mod run;
mod save;

pub use info::{Security, client_version_args, parse_security, security_args, server_version_args};
pub use inspect::{Inspection, inspect_args, parse_inspection};
pub use run::{Mount, RunError, RunSpec, User, remove_args, run_args};
pub use save::save_args;

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Names the engine explicitly instead of detecting it.
pub const OVERRIDE: &str = "APSIDES_CONTAINER_ENGINE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Podman,
    Docker,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("{OVERRIDE} must be podman or docker, not {0:?}")]
    Override(String),
    #[error("{engine} reported output aps cannot read ({what}): {detail}")]
    Unreadable {
        engine: &'static str,
        what: &'static str,
        detail: String,
    },
}

impl Engine {
    /// The engine this process uses, chosen once.
    ///
    /// With no override and neither engine on `PATH` this is Docker; running
    /// it then fails in the caller, which says that neither is installed.
    ///
    /// # Errors
    /// Fails when [`OVERRIDE`] names something other than `podman` or
    /// `docker`.
    pub fn current() -> Result<Self, Error> {
        static CHOSEN: OnceLock<Result<Engine, Error>> = OnceLock::new();
        CHOSEN
            .get_or_init(|| Self::select(std::env::var_os(OVERRIDE).as_deref(), on_path("podman")))
            .clone()
    }

    /// The engine for an override value `requested` (the value of
    /// [`OVERRIDE`], if set) on a host that does or does not have podman.
    ///
    /// # Errors
    /// Fails when `requested` is neither `podman` nor `docker`.
    pub fn select(requested: Option<&OsStr>, podman_on_path: bool) -> Result<Self, Error> {
        match requested.map(OsStr::to_str) {
            None => Ok(if podman_on_path {
                Self::Podman
            } else {
                Self::Docker
            }),
            Some(Some("podman")) => Ok(Self::Podman),
            Some(Some("docker")) => Ok(Self::Docker),
            Some(_) => Err(Error::Override(
                requested.unwrap_or_default().to_string_lossy().into_owned(),
            )),
        }
    }

    /// The engine's CLI, which is also kind's provider name.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }

    /// The engine's name in messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Podman => "Podman",
            Self::Docker => "Docker",
        }
    }

    /// `version --format` for the engine's own version. Local podman has no
    /// separate server, so it reports a single version.
    #[must_use]
    pub const fn version_format(self) -> &'static str {
        match self {
            Self::Podman => "{{.Version}}",
            Self::Docker => "{{.Server.Version}}",
        }
    }
}

/// The first executable regular file named `program` in a `PATH` directory.
/// Relative `PATH` entries are skipped, so the answer never depends on the
/// working directory.
#[must_use]
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

/// Whether [`find_on_path`] finds `program`.
#[must_use]
pub fn on_path(program: &str) -> bool {
    find_on_path(program).is_some()
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn podman_wins_when_present_and_docker_is_the_fallback() {
        assert_eq!(Engine::select(None, true), Ok(Engine::Podman));
        assert_eq!(Engine::select(None, false), Ok(Engine::Docker));
    }

    #[test]
    fn the_override_decides_whatever_is_installed() {
        assert_eq!(
            Engine::select(Some(OsStr::new("docker")), true),
            Ok(Engine::Docker)
        );
        assert_eq!(
            Engine::select(Some(OsStr::new("podman")), false),
            Ok(Engine::Podman)
        );
    }

    #[test]
    fn an_invalid_override_is_named_in_the_error() {
        let error = Engine::select(Some(OsStr::new("nerdctl")), true);
        assert_eq!(error, Err(Error::Override("nerdctl".to_owned())));
        assert_eq!(
            error.err().map(|error| error.to_string()).as_deref(),
            Some("APSIDES_CONTAINER_ENGINE must be podman or docker, not \"nerdctl\"")
        );
    }

    #[cfg(unix)]
    #[test]
    fn executables_are_regular_files_with_an_execute_bit() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let program = dir.path().join("tool");
        std::fs::write(&program, "#!/bin/sh\n")?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644))?;
        assert!(!executable(&program));
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        assert!(executable(&program));
        assert!(!executable(dir.path()));
        assert!(!executable(&dir.path().join("missing")));
        Ok(())
    }
}
