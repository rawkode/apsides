//! The container engine the kind nodes run on.
//!
//! The choice itself is [`container::Engine::current`]: podman when it is on
//! `PATH`, Docker otherwise, and [`OVERRIDE`] decides when set. kind is told
//! the same engine through `KIND_EXPERIMENTAL_PROVIDER`, so the cluster, its
//! node containers and the local image store always belong to one engine.

use crate::{Result, process::Invocation};

pub use container::{Engine, OVERRIDE};

/// The engine this process uses, chosen once.
///
/// # Errors
/// Fails when [`OVERRIDE`] names something other than `podman` or `docker`.
pub fn current() -> Result<Engine> {
    Ok(Engine::current()?)
}

/// The engine's commands, run by the suite's async process runner.
pub trait EngineCommands {
    /// A command running the engine's CLI.
    #[must_use]
    fn command(self) -> Invocation;

    /// A command running `kind` against this engine.
    #[must_use]
    fn kind(self) -> Invocation;
}

impl EngineCommands for Engine {
    fn command(self) -> Invocation {
        Invocation::new(self.program())
    }

    fn kind(self) -> Invocation {
        Invocation::new("kind").env("KIND_EXPERIMENTAL_PROVIDER", self.program())
    }
}
