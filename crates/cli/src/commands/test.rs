//! `aps test` — run the generated reactor tests.
//!
//! Without `--cluster`, only the semantic cases run and every case that needs
//! Kubernetes is reported unsupported, so a plan with such cases fails. With
//! `--cluster`, a fixture release's semantic cases run as before and every
//! case also runs against its controller on a disposable kind cluster. Any
//! failed or unsupported required case fails the command.

use crate::{Result, cli, ir, report};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;

/// # Errors
/// Returns an error if the plan is unreadable, the output directory already
/// exists, or any required reactor case failed or is unsupported.
pub fn command(arguments: &cli::Test) -> Result<()> {
    if arguments.cluster {
        return cluster(arguments);
    }
    let plan = match (&arguments.ir, &arguments.release) {
        (Some(path), _) => ir::read_plan(path)?,
        (None, Some(path)) => release::read_release(path)?.1,
        (None, None) => return Err("aps test needs --ir or --release".into()),
    };
    let results = serde_json::to_value(compiler_core::run_semantic_tests(&plan)?)?;

    let output = &arguments.output;
    if output.exists() {
        return Err("test output already exists; choose a fresh directory".into());
    }
    fs::create_dir_all(output)?;
    fs::write(
        output.join("results.json"),
        serde_json::to_vec_pretty(&results)?,
    )?;
    fs::write(output.join("junit.xml"), junit(&results)?)?;
    report::emit(&results)?;

    let rows = rows(&results)?;
    if rows.iter().any(|row| row["status"] == "failed") {
        return Err("required reactor cases failed; see results.json".into());
    }
    if rows.iter().any(failed) {
        return Err("required reactor cases need a Kubernetes cluster; build a fixture release and run `aps test --cluster --release <fixture release> --fixture-image <archive>`".into());
    }
    Ok(())
}

/// Exit status when the cluster, its tools or its isolation failed, so the
/// cases prove nothing either way.
const INFRASTRUCTURE_EXIT: i32 = 3;
/// Exit status after SIGINT or SIGTERM, following the shell convention for
/// SIGINT.
const INTERRUPTED_EXIT: i32 = 130;

/// Run every case of a fixture release on a disposable kind cluster.
///
/// Exits 0 when every check passed, 1 when a reactor case, the canary scan or
/// the mutation check failed, 3 on an infrastructure failure and 130 when
/// interrupted. The cluster is cleaned up in every case.
fn cluster(arguments: &cli::Test) -> Result<()> {
    let (Some(release), Some(fixture_image)) = (&arguments.release, &arguments.fixture_image)
    else {
        return Err("aps test --cluster needs --release and --fixture-image".into());
    };
    let options = test_cluster::Options {
        release: release.clone(),
        fixture_image: fixture_image.clone(),
        crds: arguments.crds.clone(),
        output: arguments.output.clone(),
        keep_cluster: arguments.keep_cluster,
        progress: Some(progress),
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let outcome = runtime.block_on(test_cluster::run(&options, test_cluster::termination()))?;
    drop(runtime);
    let results = arguments.output.join("results.json");
    report::emit(&json!({
        "status": outcome.status,
        "clean": outcome.clean,
        "failure": outcome.failure,
        "summary": outcome.summary,
        "checks": outcome.checks.iter().map(|check| json!({"id": check.id, "status": check.status})).collect::<Vec<_>>(),
        "cluster": outcome.cluster,
        "error": outcome.error,
        "results": results,
    }))?;
    let Some(failure) = outcome.failure else {
        if outcome.cluster.disposition == test_cluster::Disposition::Kept {
            progress(&format!(
                "passed, but the cluster {} was kept, so this is not a clean run; inspect it with --kubeconfig {} and remove it with `kind delete cluster --name {}`",
                outcome.cluster.name.as_deref().unwrap_or_default(),
                outcome
                    .cluster
                    .kubeconfig
                    .as_deref()
                    .unwrap_or(&arguments.output)
                    .display(),
                outcome.cluster.name.as_deref().unwrap_or_default(),
            ));
        }
        return Ok(());
    };
    let message = format!("{}; see {}", explain(&outcome), results.display());
    match failure {
        test_cluster::Failure::Cases => Err(message.into()),
        test_cluster::Failure::Infrastructure => exit(INFRASTRUCTURE_EXIT, &message),
        test_cluster::Failure::Interrupted => exit(INTERRUPTED_EXIT, &message),
    }
}

/// What failed: the run's error, else the failing checks and the first
/// failing cases.
fn explain(outcome: &test_cluster::Report) -> String {
    if let Some(error) = &outcome.error {
        return error.clone();
    }
    let checks = outcome
        .checks
        .iter()
        .filter(|check| !check.passed())
        .map(|check| {
            let failed = check
                .assertions
                .iter()
                .filter(|assertion| !assertion.passed)
                .map(|assertion| assertion.check_id.as_str())
                .collect::<Vec<_>>();
            format!("{} ({})", check.id, failed.join(", "))
        })
        .collect::<Vec<_>>();
    let cases = outcome
        .cases
        .iter()
        .filter(|row| {
            row.required
                && matches!(
                    row.status,
                    test_cluster::CaseStatus::Failed | test_cluster::CaseStatus::Unsupported
                )
        })
        .take(5)
        .map(|row| format!("{} {}: {}", row.reaction_id, row.scenario, row.detail))
        .collect::<Vec<_>>();
    let mut message = format!("failed checks: {}", checks.join("; "));
    if !cases.is_empty() {
        write!(message, "; failing cases: {}", cases.join("; ")).ok();
    }
    message
}

/// Report `message` and end the process with `code`. The runtime, the
/// cluster and the report are already finished.
fn exit(code: i32, message: &str) -> ! {
    let _ = writeln!(std::io::stderr().lock(), "aps: {message}");
    std::process::exit(code)
}

/// One line of progress on stderr.
fn progress(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "aps test: {line}");
}

fn rows(report: &Value) -> Result<&Vec<Value>> {
    report["results"]
        .as_array()
        .ok_or_else(|| "semantic report missing results".into())
}

fn failed(row: &Value) -> bool {
    row["status"] != "passed"
}

fn junit(report: &Value) -> Result<String> {
    let rows = rows(report)?;
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><testsuite name=\"apsides-required-reactor-coverage\" tests=\"{}\" failures=\"{}\">",
        rows.len(),
        rows.iter().filter(|row| failed(row)).count()
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

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
