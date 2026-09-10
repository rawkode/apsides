//! The complete command-line surface.
//!
//! Every accepted command, option and value lives here, so the parser is the
//! single description of what `aps` accepts. Mutually exclusive and dependent
//! options are expressed as clap relationships rather than as checks scattered
//! through the command bodies.

use crate::Result;
use clap::{
    ArgGroup, Args, CommandFactory as _, Parser, Subcommand, ValueEnum,
    builder::NonEmptyStringValueParser,
};
use std::{collections::BTreeSet, env, path::PathBuf};

/// What the parser concluded about this invocation.
pub enum Invocation {
    /// Run a command.
    Run(Box<Command>),
    /// Write terminal output — help or the version — to stdout and stop.
    Print(String),
    /// Write a usage diagnostic to stderr and stop with a nonzero status.
    Usage(String),
}

/// Parse the process arguments into an invocation.
///
/// # Errors
/// Returns an error for arguments that are not valid UTF-8, or for a repeated
/// option. Usage problems clap reports are returned as [`Invocation::Usage`],
/// not as errors, so the caller can honour clap's exit convention.
pub fn parse() -> Result<Invocation> {
    let arguments = arguments()?;
    reject_repeated_options(&arguments)?;
    match Cli::try_parse_from(std::iter::once("aps".to_owned()).chain(arguments)) {
        Ok(Cli { command: None }) => Ok(Invocation::Print(
            Cli::command().render_long_help().to_string(),
        )),
        Ok(Cli {
            command: Some(command),
        }) => Ok(Invocation::Run(Box::new(command))),
        Err(error) if error.use_stderr() => Ok(Invocation::Usage(error.render().to_string())),
        Err(error) => Ok(Invocation::Print(error.render().to_string())),
    }
}

/// The version line printed by both `aps version` and `aps --version`.
pub fn version() -> String {
    format!("aps {}\n", env!("CARGO_PKG_VERSION"))
}

fn arguments() -> Result<Vec<String>> {
    env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| "arguments must be valid UTF-8".into())
        })
        .collect()
}

/// Refuse a repeated option instead of letting the last occurrence quietly win.
///
/// Every option this CLI accepts is single-valued or comma-separated, so a
/// repeat is always a mistake — and a deployment is not the place to guess
/// which of two conflicting values the caller meant.
fn reject_repeated_options(arguments: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for argument in arguments {
        if argument == "--" {
            break;
        }
        let Some(name) = argument.strip_prefix("--") else {
            continue;
        };
        let name = name.split('=').next().unwrap_or(name);
        if !name.is_empty() && !seen.insert(name) {
            return Err(format!("duplicate option --{name}").into());
        }
    }
    Ok(())
}

/// Apsides — compiled reactive GitOps for Kubernetes.
///
/// compile emits validated plans and a Rust controller project.
/// build produces a pinned Linux OCI image, native binary, SBOM and compiler closure.
/// deploy reads dist/apsides by default and uses protected Kubernetes policy
/// and short-lived identity.
// This comment is the text `aps --help` prints, so it is prose for a reader at
// a terminal rather than rustdoc; backticks would show up verbatim.
#[allow(clippy::doc_markdown)]
#[derive(Parser, Debug)]
#[command(name = "aps", version, verbatim_doc_comment)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a TypeScript authoring project.
    Init(Init),
    /// Refresh the source lock for an authoring program.
    Lock(Lock),
    /// Custom resource definition tooling.
    Crds {
        #[command(subcommand)]
        command: Crds,
    },
    /// Validate compiled IR.
    Check(Check),
    /// Validate compiled IR against a permission ceiling.
    Plan(Plan),
    /// Compile a program or raw IR into a validated artifact.
    Compile(Compile),
    /// Compile a program and build its native controller image.
    Build(Build),
    /// Run the generated semantic scenario plan.
    Test(Test),
    /// Check artifact integrity and recompute its plan.
    Verify(Verify),
    /// Verify a release against a supplied trust policy.
    VerifyRelease(VerifyRelease),
    /// Merge evidence receipts into a release candidate.
    MergeEvidence(MergeEvidence),
    /// Assemble an unsigned release from a verified artifact.
    Release(Release),
    /// Deploy a release through the resumable transition protocol.
    Deploy(Deploy),
    /// Read transition history. Audit output never grants lifecycle authority.
    Audit(Audit),
    /// Print the CLI version.
    #[command(hide = true)]
    Version,
}

#[derive(Args, Debug)]
pub struct Init {
    /// Directory to initialize.
    #[arg(long, default_value = ".")]
    pub directory: PathBuf,
}

#[derive(Args, Debug)]
pub struct Lock {
    /// Authoring entry point.
    #[arg(long, default_value = "program.tsx")]
    pub source: PathBuf,
    /// Additional modules to lock.
    #[arg(long, value_delimiter = ',', value_parser = NonEmptyStringValueParser::new())]
    pub modules: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Crds {
    /// Import a custom resource definition into the authoring model.
    Fetch(CrdsFetch),
}

#[derive(Args, Debug)]
pub struct CrdsFetch {
    /// Definition source URL.
    #[arg(long)]
    pub url: String,
    /// Resource kind to import.
    #[arg(long)]
    pub kind: String,
    /// Generated model destination.
    #[arg(long)]
    pub output: PathBuf,
    /// Definition version to select.
    #[arg(long)]
    pub version: Option<String>,
}

#[derive(Args, Debug)]
pub struct Check {
    /// Compiled IR to validate.
    #[arg(long)]
    pub ir: PathBuf,
}

#[derive(Args, Debug)]
pub struct Plan {
    /// Compiled IR to validate.
    #[arg(long)]
    pub ir: PathBuf,
    /// Permission ceiling the plan must fit inside.
    #[arg(long)]
    pub ceiling: PathBuf,
}

#[derive(Args, Debug)]
#[command(group = ArgGroup::new("input").required(true))]
pub struct Compile {
    /// Authoring entry point to compile.
    #[arg(long, group = "input")]
    pub source: Option<PathBuf>,
    /// Pre-compiled IR to read instead of a source program.
    #[arg(long, group = "input")]
    pub ir: Option<PathBuf>,
    /// Fresh artifact directory.
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Args, Debug)]
pub struct Build {
    /// Authoring entry point. Build always binds the source compiler closure.
    #[arg(long)]
    pub source: PathBuf,
    /// Fresh artifact directory.
    #[arg(long)]
    pub output: PathBuf,
    /// Pinned build profile.
    #[arg(long, default_value = "apsides.build.json")]
    pub profile: PathBuf,
    /// Cargo cache reuse for the native build.
    #[arg(long, value_enum, default_value_t = Cache::Enabled)]
    pub cache: Cache,
    /// IR projection to build.
    #[arg(long, value_enum, default_value_t = Mode::Production)]
    pub mode: Mode,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cache {
    Enabled,
    Disabled,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Production,
    Fixture,
}

#[derive(Args, Debug)]
pub struct Test {
    /// Compiled IR whose scenario plan to run.
    #[arg(long)]
    pub ir: PathBuf,
    /// Fresh directory for `results.json` and `junit.xml`.
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Args, Debug)]
pub struct Verify {
    /// Artifact directory to check.
    #[arg(long)]
    pub artifact: PathBuf,
}

#[derive(Args, Debug)]
pub struct VerifyRelease {
    /// Release directory to verify.
    #[arg(long)]
    pub artifact: PathBuf,
    /// Trust policy to verify against.
    #[arg(long)]
    pub trust_policy: PathBuf,
}

#[derive(Args, Debug)]
pub struct MergeEvidence {
    /// Release candidate to merge into.
    #[arg(long)]
    pub artifact: PathBuf,
    /// JSON list of receipt paths, relative to the list itself.
    #[arg(long)]
    pub fragments: PathBuf,
    /// Fresh output directory.
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Args, Debug)]
pub struct Release {
    /// Verified artifact directory.
    #[arg(long)]
    pub artifact: PathBuf,
    /// Native controller binary.
    #[arg(long)]
    pub binary: PathBuf,
    /// Controller image tarball.
    #[arg(long)]
    pub oci: PathBuf,
    /// Image reference recorded in the release manifest.
    #[arg(long)]
    pub image: String,
    /// Image platform recorded in the release manifest.
    #[arg(long)]
    pub platform: String,
    /// Source repository recorded in provenance.
    #[arg(long)]
    pub repository: String,
    /// Source commit recorded in provenance.
    #[arg(long)]
    pub commit: String,
    /// Final build inputs, including the compiler closure digest.
    #[arg(long)]
    pub build_inputs: PathBuf,
    /// Software bill of materials.
    #[arg(long)]
    pub sbom: PathBuf,
    /// Coverage report.
    #[arg(long)]
    pub coverage: PathBuf,
    /// Directory of evidence receipts.
    #[arg(long)]
    pub evidence: PathBuf,
    /// Fresh release directory.
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Args, Debug)]
pub struct Deploy {
    /// Local release directory.
    #[arg(long, conflicts_with = "artifact_reference")]
    pub artifact: Option<PathBuf>,
    /// File holding an immutable OCI release reference.
    #[arg(long)]
    pub artifact_reference: Option<PathBuf>,
    /// Transition to create or resume.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    pub transition: Option<String>,
    /// Executor identity. Defaults to `APSIDES_EXECUTOR_ID` or the CI run identity.
    #[arg(long, value_parser = NonEmptyStringValueParser::new())]
    pub executor: Option<String>,
    /// Resources to adopt, by object name.
    #[arg(long, value_delimiter = ',', value_parser = NonEmptyStringValueParser::new())]
    pub ownership_intents: Vec<String>,
    /// Expand an existing untouched upgrade that outgrew its admitted capacity,
    /// then review and resume it with a fresh session.
    #[arg(long, value_enum, requires = "transition")]
    pub capacity: Option<Capacity>,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capacity {
    Resize,
}

#[derive(Args, Debug)]
pub struct Audit {
    /// Domain to look up.
    #[arg(long)]
    pub domain: String,
    /// Transition to look up.
    #[arg(long)]
    pub transition: String,
    /// Cluster binding of the protected registry.
    #[arg(long)]
    pub cluster_binding: String,
    /// Workload namespace.
    #[arg(long)]
    pub namespace: String,
    /// Control namespace.
    #[arg(long)]
    pub control_namespace: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("aps").chain(arguments.iter().copied()))
    }

    #[test]
    fn the_declared_surface_matches_the_documented_commands() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ambiguous_arguments_fail_closed() {
        assert!(
            reject_repeated_options(&["--ir".into(), "a".into(), "--ir".into(), "b".into()])
                .is_err()
        );
        assert!(reject_repeated_options(&["--ir=a".into(), "--ir".into(), "b".into()]).is_err());
        assert!(
            reject_repeated_options(&["--ir".into(), "a".into(), "--output".into(), "b".into()])
                .is_ok()
        );
        // A value after `--` is data, not an option.
        assert!(
            reject_repeated_options(&["--ir".into(), "a".into(), "--".into(), "--ir".into()])
                .is_ok()
        );

        assert!(parse(&["check", "--ir"]).is_err(), "option without a value");
        assert!(parse(&["compile", "--output", "out"]).is_err(), "no input");
        assert!(
            parse(&["compile", "--ir", "a", "--source", "b", "--output", "out"]).is_err(),
            "--ir and --source are mutually exclusive"
        );
        assert!(parse(&["nonesuch"]).is_err(), "unknown command");
    }

    #[test]
    fn compile_and_build_do_not_share_each_others_options() {
        assert!(parse(&["compile", "--source", "a", "--output", "o"]).is_ok());
        assert!(parse(&["compile", "--ir", "a", "--output", "o"]).is_ok());
        // Image-build options belong to build alone.
        assert!(
            parse(&[
                "compile",
                "--source",
                "a",
                "--output",
                "o",
                "--profile",
                "p"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "compile", "--source", "a", "--output", "o", "--cache", "disabled"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "compile", "--source", "a", "--output", "o", "--mode", "fixture"
            ])
            .is_err()
        );
        // Build binds the source compiler closure, so raw IR is not an input.
        assert!(parse(&["build", "--ir", "a", "--output", "o"]).is_err());
        assert!(parse(&["build", "--source", "a", "--output", "o"]).is_ok());
    }

    #[test]
    fn capacity_resize_requires_an_explicit_transition() -> Result<()> {
        assert!(parse(&["deploy", "--capacity", "resize"]).is_err());
        assert!(parse(&["deploy", "--capacity", "cancel", "--transition", "t"]).is_err());
        let Some(Command::Deploy(deploy)) =
            parse(&["deploy", "--capacity", "resize", "--transition", "t"])?.command
        else {
            return Err("deploy did not parse".into());
        };
        assert_eq!(deploy.capacity, Some(Capacity::Resize));
        assert_eq!(deploy.transition.as_deref(), Some("t"));
        Ok(())
    }

    #[test]
    fn a_local_artifact_and_an_oci_reference_are_mutually_exclusive() {
        assert!(
            parse(&[
                "deploy",
                "--artifact",
                "dist",
                "--artifact-reference",
                "ref.json"
            ])
            .is_err()
        );
        assert!(parse(&["deploy", "--artifact", "dist"]).is_ok());
        assert!(parse(&["deploy", "--artifact-reference", "ref.json"]).is_ok());
    }

    #[test]
    fn comma_separated_lists_reject_empty_entries() -> Result<()> {
        assert!(parse(&["lock", "--modules", "a,,b"]).is_err());
        assert!(parse(&["deploy", "--ownership-intents", "a,,b"]).is_err());
        let Some(Command::Lock(lock)) = parse(&["lock", "--modules", "a,b"])?.command else {
            return Err("lock did not parse".into());
        };
        assert_eq!(lock.modules, ["a", "b"]);
        assert_eq!(lock.source, PathBuf::from("program.tsx"));
        Ok(())
    }

    #[test]
    fn crds_import_is_reachable_only_through_its_subcommand() {
        assert!(parse(&["crds"]).is_err());
        assert!(parse(&["crds", "--url", "u", "--kind", "k", "--output", "o"]).is_err());
        assert!(
            parse(&[
                "crds", "fetch", "--url", "u", "--kind", "k", "--output", "o"
            ])
            .is_ok()
        );
    }
}
