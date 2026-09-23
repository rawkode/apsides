//! `acceptance-prepare`: build and sign the releases the acceptance
//! scenarios deploy, and write the inputs file they read.
//!
//! ```sh
//! cargo run -p acceptance --bin acceptance-prepare -- \
//!   --output work/acceptance [--aps target/debug/aps] [--sbom-tool <cargo-cyclonedx>]
//! ```
//!
//! Run from the repository root. `aps build` never pulls, so the builder and
//! runtime base images must already be local (see docs/reference/ACCEPTANCE.md).

use acceptance::prepare::{Options, prepare};
use std::{io::Write as _, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(path) => {
            let _ = writeln!(std::io::stdout().lock(), "{}", path.display());
            ExitCode::SUCCESS
        }
        Err(message) => {
            let _ = writeln!(std::io::stderr().lock(), "acceptance-prepare: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<PathBuf, String> {
    let mut output = None;
    let mut aps = PathBuf::from("target/debug/aps");
    let mut sbom_tool = None;
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
        output,
        aps,
        sbom_tool,
    };
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let inputs = runtime
        .block_on(prepare(&options))
        .map_err(|error| error.to_string())?;
    Ok(inputs.results.parent().map_or_else(
        || PathBuf::from("inputs.json"),
        |directory| directory.join("inputs.json"),
    ))
}
