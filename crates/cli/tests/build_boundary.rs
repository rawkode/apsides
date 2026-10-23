//! `aps build` reports a failed native build as one stage-qualified line on
//! stderr and writes nothing to stdout.
//!
//! The source compile runs under Deno with the repository's installed
//! modules, as every `aps compile` does (`cuenv task install`).
use std::{path::Path, process::Command};

#[test]
fn a_missing_build_profile_fails_the_native_build_at_its_stage()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/web-application/program.tsx")
        .canonicalize()?;
    let directory = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_aps"))
        .arg("build")
        .arg("--source")
        .arg(&source)
        .arg("--output")
        .arg(directory.path().join("build"))
        .arg("--profile")
        .arg(directory.path().join("missing.json"))
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8(output.stdout)?, "");
    let stderr = String::from_utf8(output.stderr)?;
    let last = stderr.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("aps: native build failed during build profile: cannot read "),
        "{stderr}"
    );
    assert!(
        last.ends_with("; no deployable release was produced"),
        "{stderr}"
    );
    assert!(!directory.path().join("build/image").exists());
    Ok(())
}
