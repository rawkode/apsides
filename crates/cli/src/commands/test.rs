//! `aps test` — run the generated semantic scenario plan.
//!
//! Required cases that need a cluster are reported as unsupported, and any
//! unsupported or failed required case fails the command. A semantic failure
//! is conclusive; a semantic pass covers only the rows the runner executed.

use crate::{Result, cli, ir, report};
use serde_json::Value;
use std::fmt::Write as _;
use std::fs;

/// # Errors
/// Returns an error if the IR is unreadable, the output directory already
/// exists, or any required reactor coverage is failed or unsupported.
pub fn command(arguments: &cli::Test) -> Result<()> {
    let plan = ir::read_plan(&arguments.ir)?;
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

    if rows(&results)?.iter().any(failed) {
        return Err("required reactor cases failed or need a cluster the semantic runner does not have; see results.json".into());
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
