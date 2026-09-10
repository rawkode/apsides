//! Command output.
//!
//! Every command reports one pretty JSON document. A command's *result* goes to
//! stdout so it can be piped into the next tool; anything that merely
//! accompanies the result — a review a human is expected to read, a progress
//! note — goes to stderr, so redirecting stdout still captures exactly the
//! result and nothing else.

use crate::Result;
use serde::Serialize;
use std::io::Write;

/// Write a command's result to stdout.
///
/// # Errors
/// Returns an error if the value cannot be serialized or stdout is closed.
pub fn emit<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    write(std::io::stdout().lock(), value)
}

/// Write output that accompanies a command's result to stderr.
///
/// # Errors
/// Returns an error if the value cannot be serialized or stderr is closed.
pub fn note<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    write(std::io::stderr().lock(), value)
}

fn write<T: Serialize + ?Sized>(mut stream: impl Write, value: &T) -> Result<()> {
    writeln!(stream, "{}", serde_json::to_string_pretty(value)?)?;
    Ok(())
}
