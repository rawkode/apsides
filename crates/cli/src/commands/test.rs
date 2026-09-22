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

/// Run every case of a fixture release on a disposable kind cluster.
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
    };
    let outcome = tokio::runtime::Runtime::new()?.block_on(test_cluster::run(&options))?;
    report::emit(&json!({
        "status": outcome.status,
        "summary": outcome.summary,
        "checks": outcome.checks,
        "cluster": outcome.cluster,
        "error": outcome.error,
        "results": arguments.output.join("results.json"),
    }))?;
    if !outcome.passed() {
        return Err(format!(
            "required reactor cases failed or are unsupported; see {}",
            arguments.output.join("results.json").display()
        )
        .into());
    }
    Ok(())
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
