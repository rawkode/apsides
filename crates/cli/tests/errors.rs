//! Process-boundary regressions: invalid inputs and closed pipes are errors.
#![cfg(unix)]
use std::os::{
    fd::OwnedFd,
    unix::{ffi::OsStringExt, net::UnixStream},
};
use std::process::{Command, Stdio};

#[test]
fn invalid_utf8_arguments_return_an_error() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_aps"))
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::str::from_utf8(&output.stderr)?,
        "aps: arguments must be valid UTF-8\n"
    );
    Ok(())
}

#[test]
fn closed_stdout_returns_an_error_without_panicking() -> Result<(), Box<dyn std::error::Error>> {
    let (writer, reader) = UnixStream::pair()?;
    drop(reader);
    let writer: OwnedFd = writer.into();
    let output = Command::new(env!("CARGO_BIN_EXE_aps"))
        .arg("--help")
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let diagnostic = std::str::from_utf8(&output.stderr)?;
    assert!(diagnostic.starts_with("aps: "));
    assert!(!diagnostic.contains("panicked"));
    Ok(())
}
