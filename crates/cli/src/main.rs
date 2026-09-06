#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]
mod artifact;
mod artifact_input;
mod audit_command;
mod crds_command;
mod deploy_command;
mod evidence_command;
mod init_command;
mod inventory_connection;
mod release_command;
mod review;
mod source_build;
mod source_lock_command;

use compiler_core::{PermissionCeiling, parse_and_validate, validate_ceiling};
use serde_json::json;
use source_build::{source_mode, source_plan};
use std::fmt::Write as _;
use std::io::Write as _;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(error) = run() {
        let _ = writeln!(std::io::stderr().lock(), "aps: {error}");
        std::process::exit(1);
    }
}

fn arguments() -> Result<Vec<String>> {
    let arguments = env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| "arguments must be valid UTF-8")
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(arguments)
}

fn run() -> Result<()> {
    let arguments = arguments()?;
    let mut args = arguments.into_iter();
    let command = args.next().unwrap_or_else(|| "help".to_owned());
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        writeln!(
            std::io::stdout().lock(),
            "Apsides — compiled reactive GitOps for Kubernetes\n\n  aps init [--directory DIR]\n  aps lock [--source FILE] [--modules FILE,FILE]\n  aps crds fetch --url URL --kind KIND --output FILE [--version VERSION]\n  aps check --ir FILE\n  aps plan --ir FILE --ceiling FILE\n  aps compile (--source FILE | --ir FILE) --output DIR\n  aps build --source FILE --output DIR [--profile FILE] [--mode production|fixture]\n  aps test --ir FILE --output DIR\n  aps verify --artifact DIR\n  aps verify-release --artifact DIR --trust-policy FILE\n  aps merge-evidence --artifact DIR --fragments FILE --output DIR\n  aps deploy [--artifact DIR | --artifact-reference FILE] [--transition ID --capacity resize]\n  aps audit --domain NAME --transition ID --cluster-binding NAME --namespace NAME --control-namespace NAME\n\ncompile emits validated plans and a Rust controller project.\nbuild produces a pinned Linux OCI image, native binary, SBOM and compiler closure.\ndeploy reads dist/reakt by default and uses protected Kubernetes policy and short-lived identity.\n--capacity resize expands an existing untouched upgrade, then reviews and resumes it with a fresh session."
        )?;
        return Ok(());
    }
    if matches!(command.as_str(), "--version" | "version") {
        writeln!(
            std::io::stdout().lock(),
            "aps {}",
            env!("CARGO_PKG_VERSION")
        )?;
        return Ok(());
    }
    let command = if command == "crds" {
        if args.next().as_deref() != Some("fetch") {
            return Err("expected aps crds fetch".into());
        }
        "crds-fetch".to_owned()
    } else {
        command
    };
    let options = parse_options(&args.collect::<Vec<_>>())?;
    match command.as_str() {
        "init" => init_command::command(&options)?,
        "lock" => source_lock_command::command(&options)?,
        "crds-fetch" => crds_command::fetch(&options)?,
        "test" => {
            only(&options, &["--ir", "--output"])?;
            let plan = read_plan(required(&options, "--ir")?)?;
            let report = compiler_core::run_semantic_tests(&plan)?;
            let output = PathBuf::from(required(&options, "--output")?);
            if output.exists() {
                return Err("test output already exists; choose a fresh directory".into());
            }
            fs::create_dir_all(&output)?;
            let value = serde_json::to_value(&report)?;
            fs::write(
                output.join("results.json"),
                serde_json::to_vec_pretty(&value)?,
            )?;
            fs::write(output.join("junit.xml"), junit(&value)?)?;
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string_pretty(&value)?
            )?;
            if value["results"]
                .as_array()
                .is_some_and(|results| results.iter().any(|row| row["status"] != "passed"))
            {
                return Err("required reactor coverage is failed or unsupported; see results.json (semantic checks alone cannot pass the release gate)".into());
            }
        }
        "check" => {
            only(&options, &["--ir"])?;
            let plan = read_plan(required(&options, "--ir")?)?;
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string_pretty(
                    &json!({"status":"valid","irDigest":plan.ir_digest,"productionReady":false})
                )?
            )?;
        }
        "plan" => {
            only(&options, &["--ir", "--ceiling"])?;
            let plan = read_plan(required(&options, "--ir")?)?;
            let ceiling: PermissionCeiling = serde_json::from_value(compiler_core::parse_strict(
                &fs::read_to_string(required(&options, "--ceiling")?)?,
            )?)?;
            validate_ceiling(&plan, &ceiling)?;
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string_pretty(&plan)?
            )?;
        }
        "compile" | "build" => compile_command(&options, &command)?,
        "verify" => {
            only(&options, &["--artifact"])?;
            let report = artifact::verify(Path::new(required(&options, "--artifact")?))?;
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string_pretty(&report)?
            )?;
        }
        "verify-release" => verify_release_command(&options)?,
        "deploy" => deploy_command::command(&options)?,
        "audit" => audit_command::command(&options)?,
        "release" => release_command::command(&options)?,
        "merge-evidence" => evidence_command::command(&options)?,
        _ => return Err(format!("unknown command {command}; run aps --help").into()),
    }
    Ok(())
}

fn junit(report: &serde_json::Value) -> Result<String> {
    fn escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
    let rows = report["results"]
        .as_array()
        .ok_or("semantic report missing results")?;
    let failures = rows.iter().filter(|row| row["status"] != "passed").count();
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><testsuite name=\"reakt-required-reactor-coverage\" tests=\"{}\" failures=\"{failures}\">",
        rows.len()
    );
    for row in rows {
        let case = &row["case"];
        write!(
            xml,
            "<testcase classname=\"{}\" name=\"{}\">",
            escape(case["reactionId"].as_str().unwrap_or("unknown")),
            escape(case["scenario"].as_str().unwrap_or("unknown"))
        )?;
        if row["status"] != "passed" {
            write!(
                xml,
                "<failure type=\"{}\" message=\"{}\"/>",
                escape(row["status"].as_str().unwrap_or("failed")),
                escape(row["detail"].as_str().unwrap_or("required coverage absent"))
            )?;
        }
        xml.push_str("</testcase>");
    }
    xml.push_str("</testsuite>\n");
    Ok(xml)
}

fn read_plan(path: &str) -> Result<compiler_core::CompiledPlan> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > 8 * 1024 * 1024 {
        return Err("IR exceeds 8 MiB input limit".into());
    }
    Ok(parse_and_validate(&fs::read_to_string(path)?)?)
}

fn parse_options(args: &[String]) -> Result<BTreeMap<String, String>> {
    if !args.len().is_multiple_of(2) {
        return Err("every option requires a value".into());
    }
    let mut options = BTreeMap::new();
    for pair in args.as_chunks::<2>().0 {
        if !pair[0].starts_with("--") || pair[1].starts_with("--") {
            return Err("expected --option VALUE".into());
        }
        if options.insert(pair[0].clone(), pair[1].clone()).is_some() {
            return Err(format!("duplicate option {}", pair[0]).into());
        }
    }
    Ok(options)
}
fn required<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    options
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {key}").into())
}
fn only(options: &BTreeMap<String, String>, allowed: &[&str]) -> Result<()> {
    if let Some(key) = options.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("unknown option {key}").into());
    }
    Ok(())
}

fn verify_release_command(options: &BTreeMap<String, String>) -> Result<()> {
    only(options, &["--artifact", "--trust-policy"])?;
    let policy: release::TrustPolicy = serde_json::from_value(compiler_core::parse_strict(
        &fs::read_to_string(required(options, "--trust-policy")?)?,
    )?)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let verified = runtime.block_on(release::verify_release(
        Path::new(required(options, "--artifact")?),
        &policy,
    ))?;
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(&json!({
            "status":"verified-against-supplied-policy",
            "releaseDigest":verified.release_digest(),
            "irDigest":verified.compiled().ir_digest,
            "provenanceDigest":verified.provenance_digest(),
            "trustPolicyDigest":verified.trust_policy_digest()
        }))?
    )?;
    Ok(())
}

fn compile_command(options: &BTreeMap<String, String>, command: &str) -> Result<()> {
    only(
        options,
        &[
            "--ir",
            "--source",
            "--output",
            "--profile",
            "--cache",
            "--mode",
        ],
    )?;
    let source_mode = source_mode(options, command)?;
    let no_cache = match options.get("--cache").map(String::as_str) {
        None => false,
        Some("disabled") if command == "build" => true,
        Some("enabled") if command == "build" => false,
        _ => return Err("--cache requires build and either enabled or disabled".into()),
    };
    if command == "compile" && options.contains_key("--profile") {
        return Err("--profile applies only to image builds".into());
    }
    if command == "build" && !options.contains_key("--source") {
        return Err("build requires --source to bind the source compiler closure; use compile for raw IR inspection".into());
    }
    let output = PathBuf::from(required(options, "--output")?);
    if options.contains_key("--ir") == options.contains_key("--source") {
        return Err("supply exactly one of --ir or --source".into());
    }
    let source_stage = compile_source_stage(options.get("--source").map(String::as_str), &output)?;
    let plan = if let Some(stage) = &source_stage {
        source_plan(stage, source_mode)?
    } else {
        read_plan(required(options, "--ir")?)?
    };
    artifact::emit(&plan, &output)?;
    artifact::prepare_lock(&output)?;
    if let Some(stage) = &source_stage {
        artifact::record_source(&output, stage)?;
    }
    if command == "build" {
        build_native(
            options,
            source_stage.as_ref().ok_or("source output missing")?,
            &output,
            no_cache,
        )?;
    }
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(
            &json!({"irDigest":plan.ir_digest,"output":output,"productionReady":false,"stage":command})
        )?
    )?;
    Ok(())
}

fn compile_source_stage(source: Option<&str>, output: &Path) -> Result<Option<PathBuf>> {
    let Some(source) = source else {
        return Ok(None);
    };
    let stage = output.with_extension("source");
    if stage.exists() {
        return Err("source staging directory already exists; choose a fresh output".into());
    }
    if let Some(parent) = stage.parent() {
        fs::create_dir_all(parent)?;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let status = Command::new("node")
        .arg(root.join("packages/compiler/src/compiler.ts"))
        .arg(fs::canonicalize(source)?)
        .arg(&stage)
        .env_clear()
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .status()?;
    if !status.success() {
        return Err("Oxc/Deno source compilation failed".into());
    }
    Ok(Some(stage))
}

fn build_native(
    options: &BTreeMap<String, String>,
    source_stage: &Path,
    output: &Path,
    no_cache: bool,
) -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let mut builder = Command::new("python3");
    builder
        .arg(root.join("scripts/reakt/build.py"))
        .env("REAKT_CLI", env::current_exe()?)
        .arg("--artifact")
        .arg(output)
        .arg("--source-output")
        .arg(source_stage)
        .arg("--profile")
        .arg(
            options
                .get("--profile")
                .map_or("reakt.build.json", String::as_str),
        );
    if no_cache {
        builder.arg("--no-cache");
    }
    if !builder.status()?.success() {
        return Err("controller image build failed; no deployable release was produced".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_arguments_fail_closed() {
        assert!(parse_options(&["--ir".into()]).is_err());
        assert!(parse_options(&["--ir".into(), "a".into(), "--ir".into(), "b".into()]).is_err());
        assert!(parse_options(&["--ir".into(), "--output".into()]).is_err());
    }
}
