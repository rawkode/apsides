//! The machine-readable result of a cluster run: one row per generated case,
//! the suite-level checks, `JUnit` XML, and the secret-canary scan.

use crate::cluster::Disposition;
use compiler_core::SourceLocation;
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Write as _, path::PathBuf};

/// The report's own schema. It is not the semantic runner's results schema.
pub const SCHEMA: &str = "apsides.dev/reactor-report/v1";

/// Replaces every leaked canary in written output.
pub const REDACTED: &str = "[REDACTED]";

/// Canaries shorter than this would match by coincidence, so they are
/// counted in the scan but not searched for.
const MIN_CANARY_BYTES: usize = 8;

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

/// How the whole run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Passed,
    Failed,
    /// A signal stopped the run; the cluster was still cleaned up.
    Interrupted,
}

/// Why a run that did not pass failed, which the CLI maps to an exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Failure {
    /// A reactor case, the canary scan or the mutation check failed.
    Cases,
    /// The cluster, its tools, its isolation or its cleanup failed, so the
    /// cases prove nothing either way.
    Infrastructure,
    Interrupted,
}

/// One typed check a case or the suite made.
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

    /// Passes when `actual` is any of `allowed`.
    #[must_use]
    pub fn one_of(check_id: &str, allowed: &[&str], actual: Option<&str>) -> Self {
        Self {
            check_id: check_id.into(),
            expected: json!({"oneOf": allowed}),
            actual: json!(actual),
            passed: actual.is_some_and(|actual| allowed.contains(&actual)),
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
    /// The tested artifact (`release.json`) and fixture overlay.
    pub release_digest: String,
    pub overlay_digest: String,
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
}

/// What ran the test: the runner and every tool and image it locked.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Runner {
    pub aps_version: String,
    pub kind_version: String,
    /// The container engine the kind nodes ran on (`podman` or `docker`) and
    /// its version.
    pub container_engine: String,
    pub container_engine_version: String,
    /// The pinned node image reference, and the image ID the engine holds for it.
    pub node_image: String,
    pub node_image_id: String,
    pub proxy_image: String,
    pub proxy_image_config_digest: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterReport {
    pub name: Option<String>,
    pub disposition: Disposition,
    pub pod_subnet: &'static str,
    pub service_subnet: &'static str,
    /// The admin kubeconfig copied into the output directory, when the
    /// cluster was kept.
    pub kubeconfig: Option<PathBuf>,
}

/// Whether a connection could be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reach {
    Reachable,
    Blocked,
    /// The connection failed, but not at the runner's egress rule, so the
    /// failure does not show the rule works.
    BlockedElsewhere,
    NotRun,
}

/// One connection attempt the offline gate made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkProbe {
    pub id: String,
    /// `node`, a namespace's probe Pod, or the controller Pod.
    pub origin: String,
    pub target: String,
    pub expected: Reach,
    pub observed: Reach,
    pub detail: String,
}

impl NetworkProbe {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.expected == self.observed
    }
}

/// Probes every complete offline gate makes. A run that stops before one of
/// them fails the gate rather than passing it by omission.
pub const REQUIRED_PROBES: &[&str] = &[
    "node-external-tcp",
    "node-external-dns",
    "node-registry-pull",
    "node-api-server-service",
    "pod-sentinel-before-policies",
    "pod-sentinel-by-dns",
    "pod-api-server-service",
    "pod-external-tcp",
    "pod-external-dns",
    "workload-pod-sentinel",
    "workload-pod-external-tcp",
    "controller-sentinel",
    "controller-external-tcp",
    "node-external-tcp-after-cases",
];

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Isolation {
    /// The egress rule sits first in every node's `OUTPUT` and `FORWARD`
    /// chains, for IPv4 and IPv6, and Docker's embedded DNS is refused.
    pub node_egress_blocked: bool,
    pub probes: Vec<NetworkProbe>,
    /// The controller authenticated as its own `ServiceAccount` and could not
    /// create objects outside its namespaces.
    pub separate_credential: bool,
    pub images_imported: bool,
    /// Lifetime of the controller's token; the run is bounded by it.
    pub controller_token_seconds: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanaryScan {
    /// Canaries searched for.
    pub canaries: usize,
    /// Canaries too short to search for without coincidental matches.
    pub canaries_too_short: usize,
    pub documents_scanned: usize,
    /// Occurrences found, all of which were redacted before writing.
    pub occurrences: usize,
    pub controller_log_bytes: usize,
    pub passed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Passed,
    Failed,
}

/// A suite-level check.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub id: &'static str,
    pub status: CheckStatus,
    pub assertions: Vec<Assertion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

impl Check {
    fn new(id: &'static str, assertions: Vec<Assertion>, note: Option<&'static str>) -> Self {
        let passed = !assertions.is_empty() && assertions.iter().all(|item| item.passed);
        Self {
            id,
            status: if passed {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            },
            assertions,
            note,
        }
    }

    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == CheckStatus::Passed
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub cases: usize,
    pub passed: usize,
    pub failed: usize,
    pub unsupported: usize,
    pub excluded: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub schema_version: &'static str,
    pub layer: &'static str,
    pub test_only: bool,
    pub status: RunStatus,
    /// Passed, and the cluster is gone. A pass with `--keep-cluster` is not
    /// clean.
    pub clean: bool,
    pub failure: Option<Failure>,
    pub runner: Runner,
    pub artifact: ArtifactIdentity,
    pub cluster: ClusterReport,
    pub isolation: Isolation,
    pub checks: Vec<Check>,
    pub summary: Summary,
    pub secret_canary_scan: CanaryScan,
    pub cases: Vec<CaseRow>,
    pub error: Option<String>,
    pub limitations: Vec<&'static str>,
}

impl Report {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == RunStatus::Passed
    }
}

/// Everything the finished run knows, reduced to a report.
pub struct Outcome {
    pub runner: Runner,
    pub artifact: ArtifactIdentity,
    pub cluster: ClusterReport,
    pub isolation: Isolation,
    pub scan: CanaryScan,
    pub cases: Vec<CaseRow>,
    pub error: Option<String>,
    pub interrupted: bool,
}

pub fn shape(outcome: Outcome) -> Report {
    let count = |status| {
        outcome
            .cases
            .iter()
            .filter(|row| row.status == status)
            .count()
    };
    let infrastructure = infrastructure_checks(&outcome);
    let results = result_checks(&outcome);
    let failure = if outcome.interrupted {
        Some(Failure::Interrupted)
    } else if outcome.error.is_some() || infrastructure.iter().any(|check| !check.passed()) {
        Some(Failure::Infrastructure)
    } else if results.iter().any(|check| !check.passed()) {
        Some(Failure::Cases)
    } else {
        None
    };
    let status = match failure {
        None => RunStatus::Passed,
        Some(Failure::Interrupted) => RunStatus::Interrupted,
        Some(_) => RunStatus::Failed,
    };
    Report {
        schema_version: SCHEMA,
        layer: "containerized-disposable-cluster",
        test_only: true,
        status,
        clean: status == RunStatus::Passed && outcome.cluster.disposition == Disposition::Deleted,
        failure,
        runner: outcome.runner,
        artifact: outcome.artifact,
        cluster: outcome.cluster,
        isolation: outcome.isolation,
        checks: infrastructure.into_iter().chain(results).collect(),
        summary: Summary {
            cases: outcome.cases.len(),
            passed: count(CaseStatus::Passed),
            failed: count(CaseStatus::Failed),
            unsupported: count(CaseStatus::Unsupported),
            excluded: count(CaseStatus::Excluded),
        },
        secret_canary_scan: outcome.scan,
        cases: outcome.cases,
        error: outcome.error,
        limitations: vec![
            "fixture activation is distinct from production protected activation",
            "cooperative drain is not independent cross-generation request fencing",
            "typed unauthorized, unavailable and invalid primitives use semantic evidence; credential-read-denial is the real API proof",
            "fixture images establish rollout and Pod readiness only, not application behaviour",
            "the reaction-mutation check mutates the oracle's model of the revision, not the controller",
            "the offline gate blocks egress inside the kind node containers; the Docker host itself is not isolated",
        ],
    }
}

/// Checks whose failure means the cases prove nothing either way.
fn infrastructure_checks(outcome: &Outcome) -> [Check; 3] {
    [
        offline_gate(&outcome.isolation),
        Check::new(
            "controller-credential",
            vec![Assertion::new(
                "separate-credential",
                true,
                outcome.isolation.separate_credential,
            )],
            None,
        ),
        Check::new(
            "cluster-cleanup",
            vec![Assertion::one_of(
                "disposition",
                &["deleted", "kept"],
                serde_json::to_value(outcome.cluster.disposition)
                    .ok()
                    .as_ref()
                    .and_then(Value::as_str),
            )],
            None,
        ),
    ]
}

/// Checks on what the cases established.
fn result_checks(outcome: &Outcome) -> [Check; 3] {
    let required = outcome.cases.iter().filter(|row| row.required);
    let blocking = required
        .clone()
        .filter(|row| matches!(row.status, CaseStatus::Failed | CaseStatus::Unsupported))
        .count();
    let unsupported = required
        .filter(|row| row.status == CaseStatus::Unsupported)
        .count();
    let mutations = outcome
        .cases
        .iter()
        .filter(|row| row.scenario == crate::contract::MUTATION_SCENARIO)
        .collect::<Vec<_>>();
    [
        Check::new(
            "reactor-cases",
            vec![
                Assertion::new("cases-run", ">=1", outcome.cases.len()),
                Assertion::new("required-cases-failed-or-unsupported", 0, blocking),
                Assertion::new("required-cases-unsupported", 0, unsupported),
            ],
            None,
        ),
        Check::new(
            "secret-canary-scan",
            vec![
                Assertion::new("canary-occurrences", 0, outcome.scan.occurrences),
                Assertion::new("documents-scanned", ">=1", outcome.scan.documents_scanned),
                Assertion::new(
                    "controller-log-captured",
                    true,
                    outcome.scan.controller_log_bytes > 0,
                ),
            ],
            None,
        ),
        Check::new(
            "reaction-mutation-check",
            vec![
                Assertion::new("mutation-cases", ">=1", mutations.len()),
                Assertion::new(
                    "mutations-distinguished-from-live-revision",
                    true,
                    mutations.iter().all(|row| row.status == CaseStatus::Passed),
                ),
            ],
            Some(
                "mutations are applied to the independent revision oracle's model, not to the controller binary",
            ),
        ),
    ]
}

/// External access disabled and internal networking preserved, each shown
/// by a named probe.
fn offline_gate(isolation: &Isolation) -> Check {
    let mut assertions = vec![
        Assertion::new("fixture-images-imported", true, isolation.images_imported),
        Assertion::new(
            "node-egress-rules-installed",
            true,
            isolation.node_egress_blocked,
        ),
    ];
    for id in REQUIRED_PROBES {
        if !isolation.probes.iter().any(|probe| probe.id == *id) {
            assertions.push(Assertion::new(id, "run", "not-run"));
        }
    }
    for probe in &isolation.probes {
        assertions.push(Assertion {
            check_id: probe.id.clone(),
            expected: json!(probe.expected),
            actual: json!(probe.observed),
            passed: probe.passed(),
        });
    }
    let (external, internal): (Vec<_>, Vec<_>) = isolation
        .probes
        .iter()
        .partition(|probe| probe.expected == Reach::Blocked);
    let complete = REQUIRED_PROBES
        .iter()
        .all(|id| isolation.probes.iter().any(|probe| probe.id == *id));
    assertions.push(Assertion::new(
        "external-network-disabled",
        true,
        complete && isolation.node_egress_blocked && external.iter().all(|probe| probe.passed()),
    ));
    assertions.push(Assertion::new(
        "internal-networking-preserved",
        true,
        complete && !internal.is_empty() && internal.iter().all(|probe| probe.passed()),
    ));
    Check::new("offline-gate", assertions, None)
}

/// Splits canaries into those long enough to search for and a count of the
/// rest.
pub fn searchable(canaries: &BTreeSet<String>) -> (Vec<&str>, usize) {
    let (long, short): (Vec<_>, Vec<_>) = canaries
        .iter()
        .map(String::as_str)
        .partition(|canary| canary.len() >= MIN_CANARY_BYTES);
    (long, short.len())
}

/// How many times any canary appears in any document.
pub fn canary_occurrences(documents: &[&str], canaries: &[&str]) -> usize {
    canaries
        .iter()
        .map(|canary| {
            documents
                .iter()
                .map(|document| document.matches(canary).count())
                .sum::<usize>()
        })
        .sum()
}

/// `text` with every canary replaced, longest first so a canary containing
/// another is removed whole.
pub fn redact(text: &str, canaries: &[&str]) -> String {
    let mut ordered = canaries.to_vec();
    ordered.sort_by_key(|canary| std::cmp::Reverse(canary.len()));
    ordered.iter().fold(text.to_owned(), |text, canary| {
        text.replace(canary, REDACTED)
    })
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
            release_digest: "sha256:2".into(),
            overlay_digest: "sha256:3".into(),
            case_digest: "sha256:1".into(),
            assertions: Vec::new(),
            detail: "detail <&>".into(),
            seconds: 0.5,
        }
    }

    fn probes() -> Vec<NetworkProbe> {
        REQUIRED_PROBES
            .iter()
            .map(|id| {
                let reach = if id.contains("sentinel-before")
                    || id.contains("by-dns")
                    || id.contains("api-server")
                {
                    Reach::Reachable
                } else {
                    Reach::Blocked
                };
                NetworkProbe {
                    id: (*id).into(),
                    origin: "node".into(),
                    target: "x".into(),
                    expected: reach,
                    observed: reach,
                    detail: String::new(),
                }
            })
            .collect()
    }

    fn outcome(cases: Vec<CaseRow>) -> Outcome {
        Outcome {
            runner: Runner::default(),
            artifact: ArtifactIdentity::default(),
            cluster: ClusterReport {
                name: Some("aps-test-1".into()),
                disposition: Disposition::Deleted,
                pod_subnet: crate::cluster::POD_SUBNET,
                service_subnet: crate::cluster::SERVICE_SUBNET,
                kubeconfig: None,
            },
            isolation: Isolation {
                node_egress_blocked: true,
                probes: probes(),
                separate_credential: true,
                images_imported: true,
                controller_token_seconds: 600,
            },
            scan: CanaryScan {
                canaries: 2,
                canaries_too_short: 0,
                documents_scanned: 3,
                occurrences: 0,
                controller_log_bytes: 10,
                passed: true,
            },
            cases,
            error: None,
            interrupted: false,
        }
    }

    fn check<'a>(report: &'a Report, id: &str) -> Option<&'a Check> {
        report.checks.iter().find(|check| check.id == id)
    }

    #[test]
    fn assertions_compare_exactly_or_by_the_at_least_one_rule() {
        assert!(Assertion::new("a", 403, 403).passed);
        assert!(!Assertion::new("a", 403, 200).passed);
        assert!(Assertion::new("a", ">=1", 3).passed);
        assert!(!Assertion::new("a", ">=1", 0).passed);
        assert!(!Assertion::new("a", ">=1", "3").passed);
        assert!(Assertion::one_of("a", &["x", "y"], Some("y")).passed);
        assert!(!Assertion::one_of("a", &["x", "y"], None).passed);
    }

    #[test]
    fn a_run_passes_only_when_every_required_case_and_check_passes() {
        let passing = vec![
            row("initial-evaluation", CaseStatus::Passed),
            row(crate::contract::MUTATION_SCENARIO, CaseStatus::Passed),
        ];
        let report = shape(outcome(passing.clone()));
        assert!(report.passed() && report.clean, "{:?}", report.checks);
        assert_eq!(report.schema_version, SCHEMA);

        for status in [CaseStatus::Failed, CaseStatus::Unsupported] {
            let mut cases = passing.clone();
            cases.push(row("field-conflict", status));
            let report = shape(outcome(cases));
            assert!(!report.passed());
            assert_eq!(report.failure, Some(Failure::Cases));
            assert!(check(&report, "reactor-cases").is_some_and(|check| !check.passed()));
        }

        let mut excluded = passing.clone();
        let mut optional = row("field-conflict", CaseStatus::Excluded);
        optional.required = false;
        excluded.push(optional);
        let report = shape(outcome(excluded));
        assert!(report.passed());
        assert_eq!(report.summary.excluded, 1);

        let mut leaked = outcome(passing.clone());
        leaked.scan.occurrences = 1;
        assert_eq!(shape(leaked).failure, Some(Failure::Cases));

        let mut undeleted = outcome(passing.clone());
        undeleted.cluster.disposition = Disposition::DeleteFailed;
        assert_eq!(shape(undeleted).failure, Some(Failure::Infrastructure));

        let mut kept = outcome(passing.clone());
        kept.cluster.disposition = Disposition::Kept;
        let kept = shape(kept);
        assert!(kept.passed() && !kept.clean);

        let mut interrupted = outcome(passing);
        interrupted.interrupted = true;
        interrupted.error = Some("interrupted by SIGINT".into());
        let interrupted = shape(interrupted);
        assert_eq!(interrupted.status, RunStatus::Interrupted);
        assert_eq!(interrupted.failure, Some(Failure::Interrupted));

        let unmutated = vec![row("initial-evaluation", CaseStatus::Passed)];
        let report = shape(outcome(unmutated));
        assert!(!report.passed());
        assert!(check(&report, "reaction-mutation-check").is_some_and(|check| !check.passed()));
    }

    #[test]
    fn the_offline_gate_needs_every_probe_to_behave() {
        let passing = vec![
            row("initial-evaluation", CaseStatus::Passed),
            row(crate::contract::MUTATION_SCENARIO, CaseStatus::Passed),
        ];
        let mut missing = outcome(passing.clone());
        missing.isolation.probes.pop();
        let report = shape(missing);
        assert_eq!(report.failure, Some(Failure::Infrastructure));
        assert!(check(&report, "offline-gate").is_some_and(|check| !check.passed()));

        let mut unattributed = outcome(passing.clone());
        unattributed.isolation.probes[0].observed = Reach::BlockedElsewhere;
        assert_eq!(shape(unattributed).failure, Some(Failure::Infrastructure));

        let mut open = outcome(passing.clone());
        if let Some(probe) = open
            .isolation
            .probes
            .iter_mut()
            .find(|probe| probe.expected == Reach::Reachable)
        {
            probe.observed = Reach::Blocked;
        }
        assert_eq!(shape(open).failure, Some(Failure::Infrastructure));

        let mut unblocked = outcome(passing);
        unblocked.isolation.node_egress_blocked = false;
        assert_eq!(shape(unblocked).failure, Some(Failure::Infrastructure));
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
    fn canary_scans_count_short_canaries_and_redact_long_ones() {
        let canaries = BTreeSet::from([
            "synthetic-secret-value".to_owned(),
            "synthetic-secret-value-longer".to_owned(),
            "short".to_owned(),
        ]);
        let (long, short) = searchable(&canaries);
        assert_eq!(short, 1);
        assert_eq!(
            canary_occurrences(&["clean log", "short and clean"], &long),
            0
        );
        let leaked = "leaked synthetic-secret-value and synthetic-secret-value-longer";
        assert_eq!(canary_occurrences(&[leaked], &long), 3);
        assert_eq!(redact(leaked, &long), "leaked [REDACTED] and [REDACTED]");
    }
}
