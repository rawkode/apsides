//! `run`: one network-less container from a local image.

use crate::Engine;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

/// A bind mount of the host path `source` at `target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub source: PathBuf,
    pub target: &'static str,
    pub read_only: bool,
}

/// Who the container's process runs as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum User {
    /// The given host ids. Rootless podman maps them to the same ids inside
    /// the container (`--userns=keep-id`).
    HostIds { uid: u32, gid: u32 },
    /// uid and gid 0, which a rootless Docker daemon maps to the host user.
    Root,
}

/// Everything [`run_args`] needs for one container.
#[derive(Clone, Copy, Debug)]
pub struct RunSpec<'a> {
    pub name: &'a str,
    pub image: &'a str,
    /// `os/architecture`, such as `linux/amd64`.
    pub platform: &'a str,
    pub hostname: &'a str,
    pub workdir: &'a str,
    pub user: User,
    /// Whether the engine runs rootless, from [`crate::parse_security`].
    pub rootless: bool,
    pub mounts: &'a [Mount],
    /// The container's whole environment. podman's own additions and the
    /// image's variables are removed, so both engines pass exactly this.
    pub env: &'a BTreeMap<String, String>,
    pub entrypoint: &'a str,
    pub args: &'a [&'a str],
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("mount source {0} cannot be passed to --mount: it contains a comma or is not UTF-8")]
    MountPath(PathBuf),
}

/// `run` for `spec`: removed on exit, never pulling, on no network, with
/// every capability dropped and no privilege gain.
///
/// # Errors
/// Fails when a mount source is not UTF-8 or contains a comma, which
/// `--mount` would split.
pub fn run_args(engine: Engine, spec: &RunSpec<'_>) -> Result<Vec<OsString>, RunError> {
    let mut argv: Vec<String> = [
        "run",
        "--rm",
        "--name",
        spec.name,
        "--pull=never",
        "--platform",
        spec.platform,
        "--network=none",
        "--hostname",
        spec.hostname,
        "--workdir",
        spec.workdir,
    ]
    .map(str::to_owned)
    .into();
    if engine == Engine::Podman && spec.rootless {
        argv.push("--userns=keep-id".to_owned());
    }
    let (uid, gid) = match spec.user {
        User::HostIds { uid, gid } => (uid, gid),
        User::Root => (0, 0),
    };
    argv.extend(["--user".to_owned(), format!("{uid}:{gid}")]);
    if engine == Engine::Podman {
        argv.push("--unsetenv-all".to_owned());
    }
    argv.extend(
        [
            "--security-opt",
            "label=disable",
            "--security-opt",
            "no-new-privileges",
            "--cap-drop=ALL",
        ]
        .map(str::to_owned),
    );
    for mount in spec.mounts {
        let source = mount_source(&mount.source)?;
        let mut value = format!("type=bind,source={source},target={}", mount.target);
        if mount.read_only {
            value.push_str(",readonly");
        }
        argv.extend(["--mount".to_owned(), value]);
    }
    for (key, value) in spec.env {
        argv.extend(["--env".to_owned(), format!("{key}={value}")]);
    }
    argv.extend(["--entrypoint", spec.entrypoint, spec.image].map(str::to_owned));
    argv.extend(spec.args.iter().map(|&arg| arg.to_owned()));
    Ok(argv.into_iter().map(OsString::from).collect())
}

fn mount_source(source: &Path) -> Result<&str, RunError> {
    source
        .to_str()
        .filter(|text| !text.contains(','))
        .ok_or_else(|| RunError::MountPath(source.to_path_buf()))
}

/// `rm --force <name>`: removes the container whether or not it still runs.
#[must_use]
pub fn remove_args(name: &str) -> Vec<OsString> {
    ["rm", "--force", name]
        .into_iter()
        .map(OsString::from)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const CONTEXT: [&str; 5] = [".cargo", "controller", "rusty-v8", "vendor", "workspace"];

    fn mounts() -> Vec<Mount> {
        let mut mounts: Vec<Mount> = CONTEXT
            .iter()
            .zip([
                "/build/.cargo",
                "/build/controller",
                "/build/rusty-v8",
                "/build/vendor",
                "/build/workspace",
            ])
            .map(|(directory, target)| Mount {
                source: Path::new("/out/.context").join(directory),
                target,
                read_only: true,
            })
            .collect();
        mounts.push(Mount {
            source: "/cache/target".into(),
            target: "/build/target",
            read_only: false,
        });
        mounts.push(Mount {
            source: "/out/.cargo-home".into(),
            target: "/opt/apsides-cargo",
            read_only: false,
        });
        mounts
    }

    /// The build contract's environment for the pinned builder (spec §3.2).
    fn env() -> BTreeMap<String, String> {
        [
            ("CARGO_HOME", "/opt/apsides-cargo"),
            ("CARGO_NET_OFFLINE", "true"),
            ("CARGO_TARGET_DIR", "/build/target"),
            ("HOME", "/opt/apsides-cargo"),
            (
                "PATH",
                "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            ),
            ("RUSTUP_HOME", "/usr/local/rustup"),
            ("RUSTY_V8_ARCHIVE", "/build/rusty-v8/librusty_v8.a.gz"),
            ("RUST_VERSION", "1.98.0"),
            ("SOURCE_DATE_EPOCH", "0"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
    }

    fn compile(engine: Engine, user: User, rootless: bool) -> Result<Vec<OsString>, RunError> {
        let mounts = mounts();
        let env = env();
        run_args(
            engine,
            &RunSpec {
                name: "apsides-build-7-1f",
                image: "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922",
                platform: "linux/amd64",
                hostname: "apsides-build",
                workdir: "/build",
                user,
                rootless,
                mounts: &mounts,
                env: &env,
                entrypoint: "cargo",
                args: &[
                    "build",
                    "--frozen",
                    "--release",
                    "--manifest-path",
                    "controller/Cargo.toml",
                ],
            },
        )
    }

    /// The full argv with `identity` (user namespace, user and env reset)
    /// in its place.
    fn golden(identity: &[&str]) -> Vec<OsString> {
        let head = [
            "run",
            "--rm",
            "--name",
            "apsides-build-7-1f",
            "--pull=never",
            "--platform",
            "linux/amd64",
            "--network=none",
            "--hostname",
            "apsides-build",
            "--workdir",
            "/build",
        ];
        let tail = [
            "--security-opt",
            "label=disable",
            "--security-opt",
            "no-new-privileges",
            "--cap-drop=ALL",
            "--mount",
            "type=bind,source=/out/.context/.cargo,target=/build/.cargo,readonly",
            "--mount",
            "type=bind,source=/out/.context/controller,target=/build/controller,readonly",
            "--mount",
            "type=bind,source=/out/.context/rusty-v8,target=/build/rusty-v8,readonly",
            "--mount",
            "type=bind,source=/out/.context/vendor,target=/build/vendor,readonly",
            "--mount",
            "type=bind,source=/out/.context/workspace,target=/build/workspace,readonly",
            "--mount",
            "type=bind,source=/cache/target,target=/build/target",
            "--mount",
            "type=bind,source=/out/.cargo-home,target=/opt/apsides-cargo",
            "--env",
            "CARGO_HOME=/opt/apsides-cargo",
            "--env",
            "CARGO_NET_OFFLINE=true",
            "--env",
            "CARGO_TARGET_DIR=/build/target",
            "--env",
            "HOME=/opt/apsides-cargo",
            "--env",
            "PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "--env",
            "RUSTUP_HOME=/usr/local/rustup",
            "--env",
            "RUSTY_V8_ARCHIVE=/build/rusty-v8/librusty_v8.a.gz",
            "--env",
            "RUST_VERSION=1.98.0",
            "--env",
            "SOURCE_DATE_EPOCH=0",
            "--entrypoint",
            "cargo",
            "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922",
            "build",
            "--frozen",
            "--release",
            "--manifest-path",
            "controller/Cargo.toml",
        ];
        head.iter()
            .chain(identity)
            .chain(&tail)
            .map(|&word| OsString::from(word))
            .collect()
    }

    #[test]
    fn podman_rootless_keeps_the_host_ids_and_resets_the_environment() -> Result<(), RunError> {
        assert_eq!(
            compile(
                Engine::Podman,
                User::HostIds {
                    uid: 1000,
                    gid: 100
                },
                true
            )?,
            golden(&["--userns=keep-id", "--user", "1000:100", "--unsetenv-all"])
        );
        Ok(())
    }

    #[test]
    fn podman_rootful_passes_the_ids_without_a_user_namespace() -> Result<(), RunError> {
        assert_eq!(
            compile(
                Engine::Podman,
                User::HostIds {
                    uid: 1000,
                    gid: 100
                },
                false
            )?,
            golden(&["--user", "1000:100", "--unsetenv-all"])
        );
        Ok(())
    }

    #[test]
    fn docker_rootful_passes_the_host_ids() -> Result<(), RunError> {
        assert_eq!(
            compile(
                Engine::Docker,
                User::HostIds {
                    uid: 1000,
                    gid: 100
                },
                false
            )?,
            golden(&["--user", "1000:100"])
        );
        Ok(())
    }

    #[test]
    fn docker_rootless_runs_as_root_inside_the_daemon_namespace() -> Result<(), RunError> {
        assert_eq!(
            compile(Engine::Docker, User::Root, true)?,
            golden(&["--user", "0:0"])
        );
        Ok(())
    }

    #[test]
    fn the_build_is_offline_and_never_writes_its_context() -> Result<(), RunError> {
        let argv: Vec<String> = compile(Engine::Docker, User::Root, true)?
            .into_iter()
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        assert!(argv.iter().any(|word| word == "--network=none"));
        assert!(argv.iter().any(|word| word == "--pull=never"));
        assert!(
            argv.iter()
                .any(|word| word == "RUSTY_V8_ARCHIVE=/build/rusty-v8/librusty_v8.a.gz")
        );
        assert!(argv.iter().any(|word| word
            == "type=bind,source=/out/.context/rusty-v8,target=/build/rusty-v8,readonly"));
        for directory in CONTEXT {
            let source = format!("source=/out/.context/{directory},");
            assert!(
                argv.iter()
                    .filter(|word| word.contains(&source))
                    .all(|word| word.ends_with(",readonly")),
                "{directory} is mounted writable"
            );
        }
        Ok(())
    }

    #[test]
    fn a_mount_source_with_a_comma_is_rejected() {
        let mounts = [Mount {
            source: "/out/a,b/.cargo".into(),
            target: "/build/.cargo",
            read_only: true,
        }];
        let result = run_args(
            Engine::Podman,
            &RunSpec {
                name: "n",
                image: "i",
                platform: "linux/amd64",
                hostname: "h",
                workdir: "/build",
                user: User::Root,
                rootless: false,
                mounts: &mounts,
                env: &BTreeMap::new(),
                entrypoint: "/usr/bin/env",
                args: &["-0"],
            },
        );
        assert!(
            matches!(&result, Err(RunError::MountPath(path)) if path == Path::new("/out/a,b/.cargo")),
            "{result:?}"
        );
    }

    #[test]
    fn a_mount_source_that_is_not_utf8_is_rejected() {
        use std::os::unix::ffi::OsStrExt as _;
        let source = PathBuf::from(std::ffi::OsStr::from_bytes(b"/out/\xff"));
        assert!(matches!(
            mount_source(&source),
            Err(RunError::MountPath(path)) if path == source
        ));
    }

    #[test]
    fn remove_forces_the_named_container() {
        assert_eq!(
            remove_args("apsides-build-7-1f"),
            ["rm", "--force", "apsides-build-7-1f"]
        );
    }
}
