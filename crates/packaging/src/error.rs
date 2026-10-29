//! The one error type of the native build: the stage that failed and what,
//! exactly, went wrong there.

use std::{fmt, path::Path};

/// The build step a failure belongs to, named in the top-level message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Profile,
    HostTools,
    Outputs,
    Builder,
    Context,
    RustyV8,
    RuntimeBase,
    Compile,
    Image,
    Sbom,
    Closure,
}

impl Stage {
    /// The stage as the failure message names it.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Profile => "build profile",
            Self::HostTools => "host tool identification",
            Self::Outputs => "build outputs",
            Self::Builder => "builder image check",
            Self::Context => "build context preparation",
            Self::RustyV8 => "rusty_v8 archive resolution",
            Self::RuntimeBase => "runtime base verification",
            Self::Compile => "controller compile",
            Self::Image => "image assembly",
            Self::Sbom => "SBOM generation",
            Self::Closure => "compiler closure finalization",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.describe())
    }
}

/// A native build failure. `detail` states the cause, names the object it
/// concerns and, where one exists, the remedy.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{stage}: {detail}")]
pub struct Error {
    pub stage: Stage,
    pub detail: String,
}

impl Error {
    #[must_use]
    pub fn new(stage: Stage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
        }
    }
}

/// Maps an I/O error on `path` to "cannot {action} {path}: {error}".
pub(crate) fn io(stage: Stage, action: &str, path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    move |error| {
        Error::new(
            stage,
            format!("cannot {action} {}: {error}", path.display()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_stage_then_the_detail() {
        let error = Error::new(Stage::RustyV8, "the archive is missing");
        assert_eq!(
            error.to_string(),
            "rusty_v8 archive resolution: the archive is missing"
        );
        let error = io(Stage::Outputs, "create", Path::new("/out/image"))(std::io::Error::other(
            "disk full",
        ));
        assert_eq!(
            error.to_string(),
            "build outputs: cannot create /out/image: disk full"
        );
    }
}
