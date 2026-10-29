//! The native build records `aps` among its host tools by the version
//! `aps --version` reports, computed in process rather than by running it.
use std::process::Command;

#[test]
fn the_recorded_apsides_version_is_the_first_line_of_aps_version()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let output = Command::new(env!("CARGO_BIN_EXE_aps"))
        .arg("--version")
        .output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert_eq!(
        stdout.lines().next(),
        Some(packaging::tools::apsides_version().as_str())
    );
    Ok(())
}
