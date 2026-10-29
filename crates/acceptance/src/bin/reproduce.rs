//! `acceptance-reproduce`: build one program twice with Cargo's cache
//! disabled and require identical outputs.
//!
//! ```sh
//! cargo run -p acceptance --bin acceptance-reproduce -- \
//!   --output work/reproducible-build [--source examples/web-application/program.tsx] \
//!   [--aps target/debug/aps] [--sbom-tool <cargo-cyclonedx>] [--engines docker,podman]
//! ```
//!
//! Run from the repository root. `--engines` compiles the first build with
//! the first engine and the second with the second. `aps build` never pulls,
//! so the builder and runtime base images must already be in each engine's
//! store (see docs/reference/ACCEPTANCE.md).

use acceptance::reproduce::{Options, parse_engines, reproduce};
use std::{io::Write as _, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(path) => {
            let _ = writeln!(std::io::stdout().lock(), "{}", path.display());
            ExitCode::SUCCESS
        }
        Err(message) => {
            let _ = writeln!(std::io::stderr().lock(), "acceptance-reproduce: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<PathBuf, String> {
    let mut output = None;
    let mut aps = PathBuf::from("target/debug/aps");
    let mut sbom_tool = None;
    let mut source = PathBuf::from("examples/web-application/program.tsx");
    let mut engines = None;
    let mut arguments = std::env::args_os().skip(1);
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| format!("{} needs a value", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--output") => output = Some(value),
            Some("--aps") => aps = value,
            Some("--sbom-tool") => sbom_tool = Some(value),
            Some("--source") => source = value,
            Some("--engines") => {
                engines = Some(
                    parse_engines(&value.to_string_lossy()).map_err(|error| error.to_string())?,
                );
            }
            _ => return Err(format!("unknown argument {}", flag.to_string_lossy())),
        }
    }
    let output = output.ok_or("--output is required")?;
    let sbom_tool = match sbom_tool {
        Some(path) => path,
        None => acceptance::prepare::on_path("cargo-cyclonedx")
            .ok_or("cargo-cyclonedx is not on PATH; install it or pass --sbom-tool")?,
    };
    let options = Options {
        root: std::env::current_dir().map_err(|error| error.to_string())?,
        output: output.clone(),
        aps,
        sbom_tool,
        source,
        engines,
    };
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime
        .block_on(reproduce(&options))
        .map_err(|error| error.to_string())?;
    Ok(output.join("report.json"))
}
