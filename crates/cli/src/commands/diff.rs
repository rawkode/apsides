//! `aps diff` — compare two deployment surfaces and evaluate an optional
//! review policy.
//!
//! Unlike every other command, a clean diff (exit 0) and a diff whose policy
//! requires approval (exit 1) are both *successful* invocations: the command
//! did exactly what it was asked, it just has two different things to report.
//! This module writes its output and calls `std::process::exit` directly for
//! both outcomes, rather than returning an `Err` that `main`'s generic
//! `finish` would collapse onto one failure exit code. An unreadable or
//! malformed `--base`, `--head` or `--policy` is a third, distinct outcome
//! (exit 2, the same status clap's own usage errors already use) for the
//! same reason.

use crate::{Result, cli, report};
use compiler_core::{
    Surface, SurfacePolicy, diff_surfaces, evaluate_policy, parse_policy, parse_strict,
    render_markdown,
};
use serde_json::json;
use std::{fs, path::Path};

/// The largest surface or policy document this command will read.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Exit status for an unreadable or malformed input, alongside clap's own
/// usage errors.
const USAGE_EXIT: i32 = 2;
/// Exit status when the policy names at least one triggered rule.
const APPROVAL_EXIT: i32 = 1;

fn read_bounded(path: &Path) -> Result<String> {
    if fs::metadata(path)?.len() > MAX_BYTES {
        return Err("input exceeds 8 MiB limit".into());
    }
    Ok(fs::read_to_string(path)?)
}

/// Read a deployment surface from `path`: an artifact directory, in which
/// case its `surface.json` is read, or a `surface.json` file directly.
fn read_surface(path: &Path) -> Result<Surface> {
    let target = if fs::metadata(path)?.is_dir() {
        path.join("surface.json")
    } else {
        path.to_path_buf()
    };
    Ok(serde_json::from_value(parse_strict(&read_bounded(
        &target,
    )?)?)?)
}

fn read_policy(path: &Path) -> Result<SurfacePolicy> {
    Ok(parse_policy(&read_bounded(path)?)?)
}

/// Report an unreadable or malformed input and exit with [`USAGE_EXIT`].
fn fail(context: &str, error: impl std::fmt::Display) -> ! {
    eprintln!("aps: {context}: {error}");
    std::process::exit(USAGE_EXIT);
}

/// # Errors
/// Returns an error only if writing the result fails. An unreadable or
/// malformed `--base`, `--head` or `--policy` exits the process directly
/// with status 2; a policy match exits with status 1 (see the module
/// documentation).
pub fn command(arguments: &cli::Diff) -> Result<()> {
    let base = read_surface(&arguments.base).unwrap_or_else(|error| fail("--base", error));
    let head = read_surface(&arguments.head).unwrap_or_else(|error| fail("--head", error));
    let policy = arguments
        .policy
        .as_ref()
        .map(|path| read_policy(path).unwrap_or_else(|error| fail("--policy", error)));

    let diff = diff_surfaces(&base, &head);
    let triggered = policy.as_ref().map(|policy| evaluate_policy(policy, &diff));

    match arguments.format {
        cli::DiffFormat::Markdown => print!("{}", render_markdown(&diff)),
        cli::DiffFormat::Json => {
            let triggered_rules = triggered
                .as_ref()
                .map(|rules| rules.iter().map(|rule| rule.as_str()).collect::<Vec<_>>());
            report::emit(&json!({"diff": diff, "triggeredRules": triggered_rules}))?;
        }
    }

    if let Some(triggered) = &triggered
        && !triggered.is_empty()
    {
        let names = triggered
            .iter()
            .map(|rule| rule.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("aps diff: policy requires approval: {names}");
        std::process::exit(APPROVAL_EXIT);
    }
    Ok(())
}
