//! The host cargo: the one on `PATH`, run with the toolchain the checkout
//! pins, wherever its working directory is.

use crate::{Error, Result, Stage, fs::read_regular};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The toolchain file under the assets root.
pub const TOOLCHAIN_FILE: &str = "rust-toolchain.toml";
const MAX_TOOLCHAIN_BYTES: u64 = 64 * 1024;

/// Host cargo and the toolchain it runs.
///
/// rustup picks a toolchain from the working directory, and the native build
/// runs cargo from directories outside the checkout, where rustup would fall
/// back to a default that may not exist or may differ. So the toolchain is
/// always named explicitly: the caller's own `RUSTUP_TOOLCHAIN` when set,
/// else the channel the checkout's `rust-toolchain.toml` pins. rustup is told
/// never to install anything, and its home is passed on when set, so a run
/// with a cleared environment (SBOM generation) resolves the same cargo.
/// Without rustup these variables are ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCargo {
    program: PathBuf,
    toolchain: Option<String>,
    rustup_home: Option<String>,
}

impl HostCargo {
    /// `cargo` from `PATH`, with `RUSTUP_TOOLCHAIN` or else the toolchain
    /// `<assets>/rust-toolchain.toml` pins.
    ///
    /// # Errors
    /// Fails when cargo is not on `PATH`, or the toolchain file is needed and
    /// exists but does not pin exactly one channel.
    pub fn locate(assets: &Path) -> Result<Self> {
        let program = container::find_on_path("cargo")
            .ok_or_else(|| Error::new(Stage::HostTools, "cargo is not on PATH"))?;
        let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let toolchain = match set("RUSTUP_TOOLCHAIN") {
            Some(chosen) => Some(chosen),
            None => toolchain_channel(&assets.join(TOOLCHAIN_FILE))?,
        };
        Ok(Self {
            program,
            toolchain,
            rustup_home: set("RUSTUP_HOME"),
        })
    }

    /// Cargo at `program`, run with `toolchain` when it is `Some`.
    #[must_use]
    pub(crate) const fn with_program(program: PathBuf, toolchain: Option<String>) -> Self {
        Self {
            program,
            toolchain,
            rustup_home: None,
        }
    }

    /// A command running this cargo with [`HostCargo::environment`] added to
    /// the inherited environment.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.envs(self.environment());
        command
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The variables every cargo run gets: `RUSTUP_AUTO_INSTALL=0`, then
    /// `RUSTUP_TOOLCHAIN` and `RUSTUP_HOME` when known.
    #[must_use]
    pub fn environment(&self) -> Vec<(String, String)> {
        let mut environment = vec![("RUSTUP_AUTO_INSTALL".to_owned(), "0".to_owned())];
        if let Some(toolchain) = &self.toolchain {
            environment.push(("RUSTUP_TOOLCHAIN".to_owned(), toolchain.clone()));
        }
        if let Some(home) = &self.rustup_home {
            environment.push(("RUSTUP_HOME".to_owned(), home.clone()));
        }
        environment
    }
}

/// The channel `path` pins on its single `channel = "<name>"` line, or
/// `None` when there is no such file.
///
/// # Errors
/// Fails when the file is unreadable or does not have exactly one such line.
pub fn toolchain_channel(path: &Path) -> Result<Option<String>> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    let unreadable = || {
        Error::new(
            Stage::HostTools,
            format!("cannot read the toolchain channel from {}", path.display()),
        )
    };
    let bytes = read_regular(path, MAX_TOOLCHAIN_BYTES).map_err(|_| unreadable())?;
    let text = std::str::from_utf8(&bytes).map_err(|_| unreadable())?;
    let channels: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("channel = \"")?.strip_suffix('"'))
        .collect();
    match channels.as_slice() {
        [channel]
            if !channel.is_empty()
                && channel
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)) =>
        {
            Ok(Some((*channel).to_owned()))
        }
        _ => Err(unreadable()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn the_channel_is_read_from_its_exact_line() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(TOOLCHAIN_FILE);
        assert_eq!(toolchain_channel(&path)?, None);
        fs::write(
            &path,
            "[toolchain]\nchannel = \"1.98.0\"\nprofile = \"minimal\"\n",
        )?;
        assert_eq!(toolchain_channel(&path)?, Some("1.98.0".to_owned()));
        for text in [
            "[toolchain]\n",
            "channel = \"1.98.0\"\nchannel = \"1.97.0\"\n",
            "channel = \"1.98 0\"\n",
            "channel = \"\"\n",
            " channel = \"1.98.0\"\n",
            "channel = '1.98.0'\n",
        ] {
            fs::write(&path, text)?;
            let error = toolchain_channel(&path).err().ok_or(text)?;
            assert_eq!(
                error.detail,
                format!("cannot read the toolchain channel from {}", path.display())
            );
        }
        Ok(())
    }

    #[test]
    fn the_checkout_pins_a_channel() -> TestResult {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert!(toolchain_channel(&root.join(TOOLCHAIN_FILE))?.is_some());
        Ok(())
    }

    #[test]
    fn a_located_cargo_always_names_a_toolchain() -> TestResult {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let located = HostCargo::locate(&root)?;
        assert!(
            located
                .environment()
                .iter()
                .any(|(key, value)| key == "RUSTUP_TOOLCHAIN" && !value.is_empty())
        );
        Ok(())
    }

    #[test]
    fn the_environment_names_the_toolchain_and_forbids_installs() {
        let pinned = HostCargo::with_program("/bin/cargo".into(), Some("1.98.0".into()));
        assert_eq!(
            pinned.environment(),
            [
                ("RUSTUP_AUTO_INSTALL".to_owned(), "0".to_owned()),
                ("RUSTUP_TOOLCHAIN".to_owned(), "1.98.0".to_owned()),
            ]
        );
        let command = pinned.command();
        assert_eq!(command.get_program(), "/bin/cargo");
        assert_eq!(command.get_envs().count(), 2);
        let chosen = HostCargo::with_program("/bin/cargo".into(), None);
        assert_eq!(chosen.environment().len(), 1);
    }
}
