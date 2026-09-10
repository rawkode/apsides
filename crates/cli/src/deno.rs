use std::{env, path::Path, process::Command};

/// Environment the Deno host needs to resolve its own module and package caches.
const FORWARDED: [&str; 3] = ["HOME", "DENO_DIR", "XDG_CACHE_HOME"];

/// Builds a Deno invocation for one compiler asset script.
///
/// The host runtime holds unrestricted authority. The constrained boundary is
/// the inner evaluation Deno that the compiler spawns with every permission
/// denied; that process, not this one, bounds author code.
pub fn script(path: &Path) -> Command {
    let mut command = Command::new("deno");
    command.args(["run", "--allow-all"]).arg(path);
    command
}

/// Restricts the child environment to PATH and the Deno cache locations.
pub fn isolate(command: &mut Command) -> &mut Command {
    command.env_clear();
    command.env("PATH", env::var_os("PATH").unwrap_or_default());
    for name in FORWARDED {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}
