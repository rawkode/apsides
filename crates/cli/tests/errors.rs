//! Process-boundary regressions: invalid inputs and closed pipes are errors.
#![cfg(unix)]
use std::os::{
    fd::OwnedFd,
    unix::{ffi::OsStringExt, net::UnixStream},
};
use std::process::{Command, Stdio};

#[test]
fn invalid_utf8_arguments_return_an_error() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
{
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
fn closed_stdout_returns_an_error_without_panicking()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

fn compile_into(output: &std::path::Path) -> std::io::Result<std::process::Output> {
    Command::new(env!("CARGO_BIN_EXE_aps"))
        .args(["compile", "--source", "program.ts", "--output"])
        .arg(output)
        .output()
}

#[test]
fn leftover_outputs_are_named_before_compiling()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("app");
    let stage = directory.path().join("app.source");
    std::fs::create_dir(&stage)?;
    let compiled = compile_into(&output)?;
    assert_eq!(compiled.status.code(), Some(1));
    assert_eq!(
        std::str::from_utf8(&compiled.stderr)?,
        format!(
            "aps: {} already exists (an earlier failed run may have left it); remove it or \
             choose a fresh --output\n",
            stage.display()
        )
    );
    std::fs::remove_dir(&stage)?;
    std::fs::create_dir(&output)?;
    let compiled = compile_into(&output)?;
    assert!(
        std::str::from_utf8(&compiled.stderr)?
            .starts_with(&format!("aps: {} already exists", output.display()))
    );
    Ok(())
}

#[test]
fn an_unwritable_output_parent_is_named_with_a_remedy()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let parent = directory.path().join("locked");
    std::fs::create_dir(&parent)?;
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555))?;
    if std::fs::create_dir(parent.join("probe")).is_ok() {
        // Running with CAP_DAC_OVERRIDE: permissions cannot be exercised.
        return Ok(());
    }
    let compiled = compile_into(&parent.join("app"))?;
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))?;
    assert_eq!(compiled.status.code(), Some(1));
    let diagnostic = std::str::from_utf8(&compiled.stderr)?;
    assert!(
        diagnostic.starts_with(&format!(
            "aps: cannot create the source staging directory {} in {}: ",
            parent.join("app.source").display(),
            parent.display()
        )),
        "{diagnostic}"
    );
    assert!(
        diagnostic.ends_with("; choose an --output whose parent directory is writable\n"),
        "{diagnostic}"
    );
    assert!(!parent.join("app").exists());
    Ok(())
}
