//! The machine-readable result of a cluster run: one row per generated case,
//! the suite-level checks, `JUnit` XML, and the secret-canary scan.

use compiler_core::SourceLocation;
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Write as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseStatus {
    Passed,
    Failed,
    /// A required case the runner has no implementation for. It is not
    /// covered, and it fails the run.
    Unsupported,
    /// A case the test plan marks not required. It is not run.
    Excluded,
}

/// One typed check a case made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Assertion {
    pub check_id: String,
    pub expected: Value,
    pub actual: Value,
    pub passed: bool,
}

impl Assertion {
    /// `expected` is compared for equality, except `">=1"`, which accepts any
    /// integer of at least one.
    pub fn new(check_id: &str, expected: impl Into<Value>, actual: impl Into<Value>) -> Self {
        let (expected, actual) = (expected.into(), actual.into());
        let passed = if expected == ">=1" {
            actual.as_i64().is_some_and(|value| value >= 1)
        } else {
            expected == actual
        };
        Self {
            check_id: check_id.into(),
            expected,
            actual,
            passed,
        }
    }
}

/// Where a case's evidence came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Evidence {
    Semantic,
    Cluster,
}

/// The result of one generated case, with everything spec §27.6 asks a case
/// to retain.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseRow {
    pub reaction_id: String,
    pub scenario: String,
    pub layer: String,
    pub required: bool,
    pub source: SourceLocation,
    pub input_ids: Vec<String>,
    /// What the compiler said the case must establish.
    pub assertion: String,
    pub status: CaseStatus,
    pub evidence: Vec<Evidence>,
    pub fixture_digest: String,
    pub case_digest: String,
    pub assertions: Vec<Assertion>,
    pub detail: String,
    pub seconds: f64,
}

/// What exactly was tested.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactIdentity {
    pub mode: String,
    pub release_digest: String,
    pub ir_digest: String,
    pub production_ir_digest: String,
    pub overlay_digest: String,
    pub controller_image: String,
    pub controller_config_digest: String,
    pub fixture_image_config_digest: String,
    pub proxy_image_config_digest: String,
    pub node_image: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Isolation {
    /// Deny-all egress in the workload and control namespaces, with the API
    /// server as the controller's only exception, observed by the canaries.
    pub egress_denied: bool,
    /// The controller authenticated as its own `ServiceAccount` and could not
    /// create objects outside its namespaces.
    pub separate_credential: bool,
    pub images_imported: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryScan {
    pub canaries: usize,
    pub documents_scanned: usize,
    pub occurrences: usize,
    pub controller_log_bytes: usize,
    pub passed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub schema_version: &'static str,
    pub layer: &'static str,
    pub test_only: bool,
    pub status: CaseStatus,
    pub artifact: ArtifactIdentity,
    pub cluster: Value,
    pub isolation: Isolation,
    pub checks: Value,
    pub summary: Value,
    pub cases: Vec<CaseRow>,
    pub error: Option<String>,
    pub limitations: Vec<&'static str>,
}

impl Report {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == CaseStatus::Passed
    }
}

/// Everything the finished run knows, reduced to a report.
pub struct Outcome {
    pub artifact: ArtifactIdentity,
    pub cluster_name: Option<String>,
    pub disposition: crate::cluster::Disposition,
    pub isolation: Isolation,
    pub scan: CanaryScan,
    pub cases: Vec<CaseRow>,
    pub error: Option<String>,
}

pub fn shape(outcome: Outcome) -> Report {
    let required = |row: &&CaseRow| row.required;
    let count = |status| {
        outcome
            .cases
            .iter()
            .filter(|row| row.status == status)
            .count()
    };
    let blocking = outcome
        .cases
        .iter()
        .filter(required)
        .filter(|row| matches!(row.status, CaseStatus::Failed | CaseStatus::Unsupported))
        .count();
    let unsupported = outcome
        .cases
        .iter()
        .filter(required)
        .filter(|row| row.status == CaseStatus::Unsupported)
        .count();
    let mutations = outcome
        .cases
        .iter()
        .filter(|row| row.scenario == crate::contract::MUTATION_SCENARIO)
        .collect::<Vec<_>>();
    let mutations_detected =
        !mutations.is_empty() && mutations.iter().all(|row| row.status == CaseStatus::Passed);
    let offline = outcome.isolation.egress_denied && outcome.isolation.images_imported;
    let status_of = |passed: bool| if passed { "passed" } else { "failed" };
    let checks = json!({
        "offline-reactor-suite": {
            "status": status_of(offline && blocking == 0),
            "assertions": [
                Assertion::new("external-network-disabled", true, outcome.isolation.egress_denied),
                Assertion::new("fixture-images-imported", true, outcome.isolation.images_imported),
                Assertion::new("required-cases-failed-or-unsupported", 0, blocking),
                Assertion::new("required-cases-unsupported", 0, unsupported),
            ],
        },
        "secret-canary-scan": {
            "status": status_of(outcome.scan.passed),
            "assertions": [
                Assertion::new("canary-occurrences", 0, outcome.scan.occurrences),
                Assertion::new("documents-scanned", ">=1", outcome.scan.documents_scanned),
            ],
        },
        "reaction-mutation-check": {
            "status": status_of(mutations_detected),
            "assertions": [
                Assertion::new("removed-reaction-contribution-detected", true, mutations_detected),
            ],
        },
    });
    let passed = outcome.error.is_none()
        && blocking == 0
        && offline
        && outcome.isolation.separate_credential
        && outcome.scan.passed
        && mutations_detected
        && matches!(
            outcome.disposition,
            crate::cluster::Disposition::Deleted | crate::cluster::Disposition::Kept
        );
    Report {
        schema_version: "apsides.dev/reactor-results/v1",
        layer: "containerized-disposable-cluster",
        test_only: true,
        status: if passed {
            CaseStatus::Passed
        } else {
            CaseStatus::Failed
        },
        artifact: outcome.artifact,
        cluster: json!({"name": outcome.cluster_name, "disposition": outcome.disposition}),
        isolation: outcome.isolation,
        checks,
        summary: json!({
            "cases": outcome.cases.len(),
            "passed": count(CaseStatus::Passed),
            "failed": count(CaseStatus::Failed),
            "unsupported": count(CaseStatus::Unsupported),
            "excluded": count(CaseStatus::Excluded),
            "secretCanaryScan": outcome.scan,
        }),
        cases: outcome.cases,
        error: outcome.error,
        limitations: vec![
            "fixture activation is distinct from production protected activation",
            "cooperative drain is not independent cross-generation request fencing",
            "typed unauthorized, unavailable and invalid primitives use semantic evidence; credential-read-denial is the real API proof",
            "fixture images establish rollout and Pod readiness only, not application behaviour",
        ],
    }
}

/// How many times any canary appears in any document. Canaries shorter than
/// eight bytes are ignored: they would match by coincidence.
pub fn canary_occurrences(documents: &[&str], canaries: &BTreeSet<String>) -> usize {
    canaries
        .iter()
        .filter(|canary| canary.len() >= 8)
        .map(|canary| {
            documents
                .iter()
                .map(|document| document.matches(canary.as_str()).count())
                .sum::<usize>()
        })
        .sum()
}

pub fn junit(report: &Report) -> String {
    let failures = report
        .cases
        .iter()
        .filter(|row| matches!(row.status, CaseStatus::Failed | CaseStatus::Unsupported))
        .count();
    let skipped = report
        .cases
        .iter()
        .filter(|row| row.status == CaseStatus::Excluded)
        .count();
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><testsuite name=\"apsides-reactor-cluster\" tests=\"{}\" failures=\"{failures}\" skipped=\"{skipped}\">",
        report.cases.len()
    );
    for row in &report.cases {
        let _ = write!(
            xml,
            "<testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">",
            escape(&row.reaction_id),
            escape(&row.scenario),
            row.seconds
        );
        match row.status {
            CaseStatus::Passed => {}
            CaseStatus::Excluded => xml.push_str("<skipped/>"),
            CaseStatus::Failed | CaseStatus::Unsupported => {
                let kind = if row.status == CaseStatus::Failed {
                    "failed"
                } else {
                    "unsupported"
                };
                let _ = write!(
                    xml,
                    "<failure type=\"{kind}\" message=\"{}\"/>",
                    escape(&row.detail)
                );
            }
        }
        xml.push_str("</testcase>");
    }
    xml.push_str("</testsuite>\n");
    xml
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Disposition;

    fn row(scenario: &str, status: CaseStatus) -> CaseRow {
        CaseRow {
            reaction_id: "rollout-a".into(),
            scenario: scenario.into(),
            layer: "api".into(),
            required: true,
            source: SourceLocation {
                file: "program.tsx".into(),
                line: 1,
                column: 1,
            },
            input_ids: vec!["input-a".into()],
            assertion: "holds".into(),
            status,
            evidence: vec![Evidence::Cluster],
            fixture_digest: "sha256:0".into(),
            case_digest: "sha256:1".into(),
            assertions: Vec::new(),
            detail: "detail <&>".into(),
            seconds: 0.5,
        }
    }

    fn outcome(cases: Vec<CaseRow>) -> Outcome {
        Outcome {
            artifact: ArtifactIdentity::default(),
            cluster_name: Some("aps-test-1".into()),
            disposition: Disposition::Deleted,
            isolation: Isolation {
                egress_denied: true,
                separate_credential: true,
                images_imported: true,
            },
            scan: CanaryScan {
                canaries: 2,
                documents_scanned: 3,
                occurrences: 0,
                controller_log_bytes: 10,
                passed: true,
            },
            cases,
            error: None,
        }
    }

    #[test]
    fn assertions_compare_exactly_or_by_the_at_least_one_rule() {
        assert!(Assertion::new("a", 403, 403).passed);
        assert!(!Assertion::new("a", 403, 200).passed);
        assert!(Assertion::new("a", ">=1", 3).passed);
        assert!(!Assertion::new("a", ">=1", 0).passed);
        assert!(!Assertion::new("a", ">=1", "3").passed);
    }

    #[test]
    fn a_run_passes_only_when_every_required_case_and_check_passes() {
        let passing = vec![
            row("initial-evaluation", CaseStatus::Passed),
            row(crate::contract::MUTATION_SCENARIO, CaseStatus::Passed),
        ];
        assert!(shape(outcome(passing.clone())).passed());

        for status in [CaseStatus::Failed, CaseStatus::Unsupported] {
            let mut cases = passing.clone();
            cases.push(row("field-conflict", status));
            let report = shape(outcome(cases));
            assert!(!report.passed());
            assert_eq!(report.checks["offline-reactor-suite"]["status"], "failed");
        }

        let mut excluded = passing.clone();
        let mut optional = row("field-conflict", CaseStatus::Excluded);
        optional.required = false;
        excluded.push(optional);
        let report = shape(outcome(excluded));
        assert!(report.passed());
        assert_eq!(report.summary["excluded"], 1);

        let mut leaked = outcome(passing.clone());
        leaked.scan.passed = false;
        assert!(!shape(leaked).passed());

        let mut undeleted = outcome(passing);
        undeleted.disposition = Disposition::DeleteFailed;
        assert!(!shape(undeleted).passed());

        let unmutated = vec![row("initial-evaluation", CaseStatus::Passed)];
        let report = shape(outcome(unmutated));
        assert!(!report.passed());
        assert_eq!(report.checks["reaction-mutation-check"]["status"], "failed");
    }

    #[test]
    fn junit_marks_failures_unsupported_and_excluded_cases() {
        let report = shape(outcome(vec![
            row("a", CaseStatus::Passed),
            row("b", CaseStatus::Failed),
            row("c", CaseStatus::Unsupported),
            row("d", CaseStatus::Excluded),
        ]));
        let xml = junit(&report);
        assert!(
            xml.contains("tests=\"4\" failures=\"2\" skipped=\"1\""),
            "{xml}"
        );
        assert!(xml.contains("type=\"unsupported\""));
        assert!(xml.contains("detail &lt;&amp;&gt;"));
    }

    #[test]
    fn canary_scans_count_every_occurrence_of_long_canaries() {
        let canaries = BTreeSet::from(["synthetic-secret-value".to_owned(), "short".to_owned()]);
        assert_eq!(
            canary_occurrences(&["clean log", "short and clean"], &canaries),
            0
        );
        assert_eq!(
            canary_occurrences(
                &["leaked synthetic-secret-value twice synthetic-secret-value"],
                &canaries
            ),
            2
        );
    }
}
