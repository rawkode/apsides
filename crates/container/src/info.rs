//! `info` and `version`: the engine's mode and versions.

use crate::{Engine, Error};
use std::ffi::OsString;

fn args<const N: usize>(words: [&str; N]) -> Vec<OsString> {
    words.into_iter().map(OsString::from).collect()
}

/// `info` reporting the engine's security mode: podman's rootless flag, or
/// Docker's security options, which name `rootless` for a rootless daemon
/// and `userns` for one remapping user namespaces.
#[must_use]
pub fn security_args(engine: Engine) -> Vec<OsString> {
    match engine {
        Engine::Podman => args(["info", "--format", "{{.Host.Security.Rootless}}"]),
        Engine::Docker => args(["info", "--format", "{{json .SecurityOptions}}"]),
    }
}

/// How the engine isolates containers, as [`security_args`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Security {
    /// The engine runs without root on the host.
    pub rootless: bool,
    /// A Docker daemon remaps container user IDs (`userns-remap`), so a
    /// container uid is not the host uid of the same number.
    pub userns_remap: bool,
}

/// The security mode in the output of [`security_args`].
///
/// # Errors
/// Fails when podman prints something other than `true` or `false`, or
/// Docker prints something other than a JSON array of strings (or `null`).
pub fn parse_security(engine: Engine, stdout: &str) -> Result<Security, Error> {
    let unreadable = |detail: String| Error::Unreadable {
        engine: engine.name(),
        what: "info",
        detail,
    };
    let stdout = stdout.trim();
    match engine {
        Engine::Podman => match stdout {
            "true" | "false" => Ok(Security {
                rootless: stdout == "true",
                userns_remap: false,
            }),
            other => Err(unreadable(format!("expected true or false, got {other:?}"))),
        },
        Engine::Docker => serde_json::from_str::<Option<Vec<String>>>(stdout)
            .map(|options| {
                let options = options.unwrap_or_default();
                let named = |name: &str| {
                    options.iter().any(|option| {
                        option
                            .split(',')
                            .any(|field| field == format!("name={name}"))
                    })
                };
                Security {
                    rootless: named("rootless"),
                    userns_remap: named("userns"),
                }
            })
            .map_err(|error| unreadable(error.to_string())),
    }
}

/// `--version`: the client's version line.
#[must_use]
pub fn client_version_args() -> Vec<OsString> {
    args(["--version"])
}

/// `version --format` printing the engine's own version.
#[must_use]
pub fn server_version_args(engine: Engine) -> Vec<OsString> {
    args(["version", "--format", engine.version_format()])
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn security(rootless: bool, userns_remap: bool) -> Security {
        Security {
            rootless,
            userns_remap,
        }
    }

    #[test]
    fn podman_reports_its_rootless_flag() {
        assert_eq!(
            parse_security(Engine::Podman, "true\n"),
            Ok(security(true, false))
        );
        assert_eq!(
            parse_security(Engine::Podman, "false\n"),
            Ok(security(false, false))
        );
        assert!(matches!(
            parse_security(Engine::Podman, "yes"),
            Err(Error::Unreadable {
                engine: "Podman",
                what: "info",
                ..
            })
        ));
    }

    #[test]
    fn docker_security_options_name_rootless_and_userns_remap() {
        assert_eq!(
            parse_security(
                Engine::Docker,
                r#"["name=seccomp,profile=builtin","name=rootless","name=cgroupns"]"#
            ),
            Ok(security(true, false))
        );
        assert_eq!(
            parse_security(
                Engine::Docker,
                "[\"name=apparmor\",\"name=seccomp,profile=builtin\"]\n"
            ),
            Ok(security(false, false))
        );
        assert_eq!(
            parse_security(
                Engine::Docker,
                r#"["name=seccomp,profile=builtin","name=userns"]"#
            ),
            Ok(security(false, true))
        );
        assert_eq!(
            parse_security(Engine::Docker, "null"),
            Ok(security(false, false))
        );
        assert!(matches!(
            parse_security(Engine::Docker, "true"),
            Err(Error::Unreadable {
                engine: "Docker",
                what: "info",
                ..
            })
        ));
    }

    #[test]
    fn argv_per_engine() {
        assert_eq!(
            security_args(Engine::Podman),
            ["info", "--format", "{{.Host.Security.Rootless}}"]
        );
        assert_eq!(
            security_args(Engine::Docker),
            ["info", "--format", "{{json .SecurityOptions}}"]
        );
        assert_eq!(client_version_args(), ["--version"]);
        assert_eq!(
            server_version_args(Engine::Podman),
            ["version", "--format", "{{.Version}}"]
        );
        assert_eq!(
            server_version_args(Engine::Docker),
            ["version", "--format", "{{.Server.Version}}"]
        );
    }
}
