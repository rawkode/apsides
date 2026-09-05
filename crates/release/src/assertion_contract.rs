use crate::{Assertion, Result, reject};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
enum Expected {
    Bool(bool),
    Number(i64),
    Text(&'static str),
    AtLeastOne,
}

#[derive(Clone, Copy)]
struct Requirement {
    id: &'static str,
    expected: Expected,
}

const fn r(id: &'static str, expected: Expected) -> Requirement {
    Requirement { id, expected }
}

const INITIAL: &[Requirement] = &[
    r("revision-present", Expected::Bool(true)),
    r("target-converged", Expected::Bool(true)),
];
const PROJECTED: &[Requirement] = &[
    r("revision-changed", Expected::Bool(true)),
    r("target-converged", Expected::Bool(true)),
];
const STABLE: &[Requirement] = &[
    r("revision-unchanged", Expected::Bool(true)),
    r("template-mutations", Expected::Number(0)),
];
const COALESCED: &[Requirement] = &[
    r("latest-input-set-used", Expected::Bool(true)),
    r("target-converged", Expected::Bool(true)),
];
const BLOCKED_MISSING: &[Requirement] = &[
    r("block-reason", Expected::Text("missing")),
    r("target-mutations", Expected::Number(0)),
    r("target-preserved", Expected::Bool(true)),
];
const RETURNED: &[Requirement] = &[
    r("dependency-recovered", Expected::Bool(true)),
    r("target-converged", Expected::Bool(true)),
];
const BLOCKED_UNAUTHORIZED: &[Requirement] = &[
    r("block-reason", Expected::Text("unauthorized")),
    r("target-mutations", Expected::Number(0)),
];
const CREDENTIAL_DENIAL: &[Requirement] = &[
    r("http-status", Expected::Number(403)),
    r("permission-expanded", Expected::Bool(false)),
    r("target-mutations", Expected::Number(0)),
];
const BLOCKED_UNAVAILABLE: &[Requirement] = &[
    r("block-reason", Expected::Text("unavailable")),
    r("target-mutations", Expected::Number(0)),
];
const BLOCKED_INVALID: &[Requirement] = &[
    r("block-reason", Expected::Text("invalid")),
    r("sensitive-data-exposed", Expected::Bool(false)),
    r("target-mutations", Expected::Number(0)),
];
const MANAGED_DRIFT: &[Requirement] = &[
    r("managed-drift-observed", Expected::Bool(true)),
    r("managed-data-restored", Expected::Bool(true)),
    r("revision-unchanged", Expected::Bool(true)),
    r("template-mutations", Expected::Number(0)),
];
const MANAGED_MISSING: &[Requirement] = &[
    r("block-reason", Expected::Text("missing")),
    r("repair-denied", Expected::Bool(true)),
    r("revision-unchanged", Expected::Bool(true)),
    r("target-mutations", Expected::Number(0)),
];
const MANAGED_READ_DENIAL: &[Requirement] = &[
    r("http-status", Expected::Number(403)),
    r("permission-expanded", Expected::Bool(false)),
    r("revision-unchanged", Expected::Bool(true)),
    r("target-mutations", Expected::Number(0)),
];
const MANAGED_UNAVAILABLE: &[Requirement] = &[
    r("transport-failure-observed", Expected::Bool(true)),
    r("block-reason", Expected::Text("unavailable")),
    r("revision-unchanged", Expected::Bool(true)),
    r("target-mutations", Expected::Number(0)),
];
const RESTART_RELIST: &[Requirement] = &[
    r("latest-input-set-used", Expected::Bool(true)),
    r("target-converged", Expected::Bool(true)),
];
const FIELD_CONFLICT: &[Requirement] = &[
    r("conflict-reported", Expected::Bool(true)),
    r("force-applied", Expected::Bool(false)),
    r("foreign-field-preserved", Expected::Bool(true)),
];
const FORCED_FIELD_CONFLICT: &[Requirement] = &[
    r("force-applied", Expected::Bool(true)),
    r("target-converged", Expected::Bool(true)),
    r("uid-preserved", Expected::Bool(true)),
];
const OUT_OF_SCOPE: &[Requirement] = &[
    r("http-status", Expected::Number(403)),
    r("out-of-scope-mutations", Expected::Number(0)),
];
const READINESS: &[Requirement] = &[
    r("ready-pods", Expected::AtLeastOne),
    r("new-template-ready", Expected::Bool(true)),
];

fn case_requirements(scenario: &str) -> Option<&'static [Requirement]> {
    match scenario {
        "initial-evaluation" => Some(INITIAL),
        "projected-change" => Some(PROJECTED),
        "irrelevant-change" | "same-value-replay" | "accepted-write-restart" => Some(STABLE),
        "coalesced-inputs" => Some(COALESCED),
        "missing-input" => Some(BLOCKED_MISSING),
        "input-returned" => Some(RETURNED),
        "unauthorized-input" => Some(BLOCKED_UNAUTHORIZED),
        "credential-read-denial" => Some(CREDENTIAL_DENIAL),
        "unavailable-input" => Some(BLOCKED_UNAVAILABLE),
        "invalid-input" => Some(BLOCKED_INVALID),
        "managed-drift-repair" => Some(MANAGED_DRIFT),
        "managed-missing" => Some(MANAGED_MISSING),
        "managed-read-denial" => Some(MANAGED_READ_DENIAL),
        "managed-unavailable" => Some(MANAGED_UNAVAILABLE),
        "restart-and-relist" => Some(RESTART_RELIST),
        "field-conflict" => Some(FIELD_CONFLICT),
        "forced-field-conflict" => Some(FORCED_FIELD_CONFLICT),
        "out-of-scope-change" => Some(OUT_OF_SCOPE),
        "rollout-readiness" => Some(READINESS),
        _ => None,
    }
}

const REPRODUCIBLE: &[Requirement] = &[
    r("independent-builds", Expected::Number(2)),
    r("payload-digests-equal", Expected::Bool(true)),
];
const INPUT_ISOLATION: &[Requirement] = &[
    r("sandbox-engine", Expected::Text("SES")),
    r("undeclared-input-rejected", Expected::Bool(true)),
];
const OFFLINE: &[Requirement] = &[
    r("external-network-disabled", Expected::Bool(true)),
    r("required-cases-failed", Expected::Number(0)),
    r("required-cases-unsupported", Expected::Number(0)),
];
const CANARY: &[Requirement] = &[
    r("canary-occurrences", Expected::Number(0)),
    r("files-scanned", Expected::AtLeastOne),
];
const MUTATION: &[Requirement] = &[r(
    "removed-reaction-contribution-detected",
    Expected::Bool(true),
)];
const BENCHMARK: &[Requirement] = &[
    r("measurements-present", Expected::Bool(true)),
    r("scope-matched", Expected::Bool(true)),
];
const CEILING: &[Requirement] = &[
    r("above-ceiling-rejected", Expected::Bool(true)),
    r("next-writer-started", Expected::Bool(false)),
];
const COLLISION: &[Requirement] = &[
    r("claim-collision-rejected", Expected::Bool(true)),
    r("next-writer-started", Expected::Bool(false)),
];
const PERMISSION_UPGRADE: &[Requirement] = &[
    r("permission-change-reviewed", Expected::Bool(true)),
    r("unapproved-writer-started", Expected::Bool(false)),
];
const PHASE_RECOVERY: &[Requirement] = &[
    r("all-phase-interruptions-recovered", Expected::Bool(true)),
    r("unsafe-activation", Expected::Bool(false)),
];
const LAST_TYPE: &[Requirement] = &[
    r("last-api-type-cleanup-complete", Expected::Bool(true)),
    r("wrong-object-deleted", Expected::Bool(false)),
];
const UID: &[Requirement] = &[
    r("replacement-deleted", Expected::Bool(false)),
    r("uid-mismatch-rejected", Expected::Bool(true)),
];
const FINALIZER: &[Requirement] = &[
    r("finalizer-block-reported", Expected::Bool(true)),
    r("finalizers-forced", Expected::Bool(false)),
];
const UNCERTAIN_WRITER: &[Requirement] = &[
    r("activation-blocked", Expected::Bool(true)),
    r("unsafe-activation", Expected::Bool(false)),
];
const FIXTURE_PROMOTION: &[Requirement] = &[r("fixture-release-rejected", Expected::Bool(true))];
const API_GAP: &[Requirement] = &[
    r("actual-credential-tests-passed", Expected::Bool(true)),
    r("undisclosed-gap", Expected::Bool(false)),
];

fn check_requirements(key: &str) -> Option<&'static [Requirement]> {
    match key {
        "reproducible-payload" => Some(REPRODUCIBLE),
        "declared-input-isolation" => Some(INPUT_ISOLATION),
        "offline-reactor-suite" => Some(OFFLINE),
        "secret-canary-scan" => Some(CANARY),
        "reaction-mutation-check" => Some(MUTATION),
        "scope-matched-benchmark" => Some(BENCHMARK),
        "deployment-ceiling-rejection" => Some(CEILING),
        "ownership-collision" => Some(COLLISION),
        "permission-changing-upgrade" => Some(PERMISSION_UPGRADE),
        "phase-interruption-recovery" => Some(PHASE_RECOVERY),
        "last-api-type-cleanup" => Some(LAST_TYPE),
        "uid-replacement-rejection" => Some(UID),
        "finalizer-preservation" => Some(FINALIZER),
        "uncertain-writer-blocks" => Some(UNCERTAIN_WRITER),
        "fixture-promotion-rejection" => Some(FIXTURE_PROMOTION),
        "api-bound-gap-conformance" => Some(API_GAP),
        _ => None,
    }
}

fn matches(assertion: &Assertion, expected: Expected) -> bool {
    if !assertion.passed {
        return false;
    }
    match expected {
        Expected::Bool(value) => {
            assertion.expected == Value::Bool(value) && assertion.actual == Value::Bool(value)
        }
        Expected::Number(value) => {
            assertion.expected == Value::Number(value.into())
                && assertion.actual == Value::Number(value.into())
        }
        Expected::Text(value) => {
            assertion.expected == Value::String(value.into())
                && assertion.actual == Value::String(value.into())
        }
        Expected::AtLeastOne => {
            assertion.expected == Value::String(">=1".into())
                && assertion.actual.as_u64().is_some_and(|value| value >= 1)
        }
    }
}

/// Validate scenario-specific or release-check-specific independent expectations.
///
/// Callers supply actual observations. This contract fixes the required assertion
/// identifiers and expected values so a runner cannot make its own observed value
/// the expected value.
///
/// # Errors
/// Rejects unknown contracts, missing/extra assertions, duplicates, or mismatches.
pub fn validate_assertions(
    key: &str,
    is_case: bool,
    scenario: Option<&str>,
    assertions: &[Assertion],
) -> Result<()> {
    let requirements = if is_case {
        let scenario = scenario
            .ok_or_else(|| crate::Error::Rejected("case evidence lacks scenario".into()))?;
        case_requirements(scenario).ok_or_else(|| {
            crate::Error::Rejected("unknown reactor case assertion contract".into())
        })?
    } else {
        if scenario.is_some() {
            return reject("release check evidence must not supply a case scenario");
        }
        check_requirements(key).ok_or_else(|| {
            crate::Error::Rejected("unknown release-check assertion contract".into())
        })?
    };
    if assertions.len() != requirements.len() {
        return reject("evidence does not contain the exact required assertion set");
    }
    let mut seen = BTreeSet::new();
    for requirement in requirements {
        let Some(assertion) = assertions
            .iter()
            .find(|assertion| assertion.check_id == requirement.id)
        else {
            return reject("evidence is missing a required assertion");
        };
        if !seen.insert(assertion.check_id.as_str()) || !matches(assertion, requirement.expected) {
            return reject("evidence assertion does not satisfy its independent expected contract");
        }
    }
    Ok(())
}
