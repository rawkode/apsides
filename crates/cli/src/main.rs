//! `aps` — the Apsides command-line interface.
//!
//! This file is deliberately only the entry point: the accepted surface lives
//! in [`cli`], and each command's behaviour lives in its own module under
//! [`commands`].

mod artifact;
mod assets;
mod cli;
mod commands;
mod connection;
mod deno;
mod ir;
mod report;
mod review;
mod source;

use cli::{Command, Crds, Invocation};
use std::io::Write as _;
use std::process::ExitCode;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Exit status for a usage error, following the convention clap and the shells
/// expect: a misuse is distinguishable from a command that ran and failed.
const USAGE: u8 = 2;

fn main() -> ExitCode {
    match cli::parse() {
        Ok(Invocation::Run(command)) => finish(dispatch(*command)),
        Ok(Invocation::Print(text)) => finish(print(&text)),
        Ok(Invocation::Usage(text)) => {
            let _ = write!(std::io::stderr().lock(), "{text}");
            ExitCode::from(USAGE)
        }
        Err(error) => finish(Err(error)),
    }
}

fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init(arguments) => commands::init::command(&arguments),
        Command::Lock(arguments) => commands::lock::command(&arguments),
        Command::Crds {
            command: Crds::Fetch(arguments),
        } => commands::crds::fetch(&arguments),
        Command::Check(arguments) => commands::check::command(&arguments),
        Command::Compile(arguments) => commands::compile::compile(&arguments),
        Command::Build(arguments) => commands::compile::build(&arguments),
        Command::Test(arguments) => commands::test::command(&arguments),
        Command::Verify(arguments) => commands::verify::command(&arguments),
        Command::VerifyRelease(arguments) => commands::release::verify::command(&arguments),
        Command::MergeEvidence(arguments) => commands::evidence::command(&arguments),
        Command::Release(arguments) => commands::release::command(&arguments),
        Command::Deploy(arguments) => commands::deploy::command(&arguments),
        Command::Audit(arguments) => commands::audit::command(&arguments),
    }
}

fn print(text: &str) -> Result<()> {
    write!(std::io::stdout().lock(), "{text}")?;
    Ok(())
}

fn finish(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "aps: {error}");
            ExitCode::FAILURE
        }
    }
}
