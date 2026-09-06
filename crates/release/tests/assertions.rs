use release::{Assertion, validate_assertions};
use serde_json::{Value, json};

fn assertion(id: &str, expected: Value, actual: Value) -> Assertion {
    Assertion {
        check_id: id.into(),
        expected,
        actual,
        passed: true,
    }
}

#[test]
fn validates_case_specific_expectations() {
    validate_assertions(
        "case-digest",
        true,
        Some("projected-change"),
        &[
            assertion("revision-changed", json!(true), json!(true)),
            assertion("target-converged", json!(true), json!(true)),
        ],
    )
    .unwrap();
    validate_assertions(
        "case-digest",
        true,
        Some("managed-read-denial"),
        &[
            assertion("http-status", json!(403), json!(403)),
            assertion("permission-expanded", json!(false), json!(false)),
            assertion("revision-unchanged", json!(true), json!(true)),
            assertion("target-mutations", json!(0), json!(0)),
        ],
    )
    .unwrap();
    validate_assertions(
        "case-digest",
        true,
        Some("managed-unavailable"),
        &[
            assertion("transport-failure-observed", json!(true), json!(true)),
            assertion("block-reason", json!("unavailable"), json!("unavailable")),
            assertion("revision-unchanged", json!(true), json!(true)),
            assertion("target-mutations", json!(0), json!(0)),
        ],
    )
    .unwrap();
    validate_assertions(
        "case-digest",
        true,
        Some("managed-drift-repair"),
        &[
            assertion("managed-drift-observed", json!(true), json!(true)),
            assertion("managed-data-restored", json!(true), json!(true)),
            assertion("revision-unchanged", json!(true), json!(true)),
            assertion("template-mutations", json!(0), json!(0)),
        ],
    )
    .unwrap();
    validate_assertions(
        "case-digest",
        true,
        Some("managed-missing"),
        &[
            assertion("block-reason", json!("missing"), json!("missing")),
            assertion("repair-denied", json!(true), json!(true)),
            assertion("revision-unchanged", json!(true), json!(true)),
            assertion("target-mutations", json!(0), json!(0)),
        ],
    )
    .unwrap();
    validate_assertions(
        "case-digest",
        true,
        Some("rollout-readiness"),
        &[
            assertion("ready-pods", json!(">=1"), json!(3)),
            assertion("new-template-ready", json!(true), json!(true)),
        ],
    )
    .unwrap();
}

#[test]
fn rejects_self_selected_or_incomplete_case_expectations() {
    assert!(
        validate_assertions(
            "case-digest",
            true,
            Some("projected-change"),
            &[assertion("anything", json!(true), json!(true))],
        )
        .is_err()
    );
    assert!(
        validate_assertions(
            "case-digest",
            true,
            Some("rollout-readiness"),
            &[
                assertion("ready-pods", json!(0), json!(0)),
                assertion("new-template-ready", json!(true), json!(true)),
            ],
        )
        .is_err()
    );
}

#[test]
fn validates_release_check_specific_expectations() {
    validate_assertions(
        "declared-input-isolation",
        false,
        None,
        &[
            assertion("sandbox-engine", json!("SES"), json!("SES")),
            assertion("undeclared-input-rejected", json!(true), json!(true)),
        ],
    )
    .unwrap();
    validate_assertions(
        "secret-canary-scan",
        false,
        None,
        &[
            assertion("canary-occurrences", json!(0), json!(0)),
            assertion("files-scanned", json!(">=1"), json!(42)),
        ],
    )
    .unwrap();
}

#[test]
fn rejects_generic_status_equalities() {
    assert!(
        validate_assertions(
            "fixture-promotion-rejection",
            false,
            None,
            &[assertion("status", json!("passed"), json!("passed"))],
        )
        .is_err()
    );
    assert!(
        validate_assertions(
            "fixture-promotion-rejection",
            false,
            Some("initial-evaluation"),
            &[assertion(
                "fixture-release-rejected",
                json!(true),
                json!(true),
            )],
        )
        .is_err()
    );
}

fn phase_recovery_assertions() -> Vec<Assertion> {
    let mut assertions: Vec<_> = [
        "planned",
        "staged",
        "quiesced",
        "activated",
        "cleaning",
        "committed",
    ]
    .into_iter()
    .map(|phase| {
        assertion(
            &format!("{phase}-interruption-recovered"),
            json!(true),
            json!(true),
        )
    })
    .collect();
    assertions.push(assertion("unsafe-activation", json!(false), json!(false)));
    assertions
}

#[test]
fn phase_recovery_requires_every_phase_and_safe_activation() {
    let complete = phase_recovery_assertions();
    let validate = |values: &[Assertion]| {
        validate_assertions("phase-interruption-recovery", false, None, values)
    };
    assert!(validate(&complete).is_ok());
    for index in 0..complete.len() {
        let mut missing = complete.clone();
        missing.remove(index);
        assert!(validate(&missing).is_err());

        let mut failed = complete.clone();
        failed[index].actual = json!(index == complete.len() - 1);
        assert!(validate(&failed).is_err());

        let mut duplicate = complete.clone();
        duplicate[index] = complete[(index + 1) % complete.len()].clone();
        assert!(validate(&duplicate).is_err());
    }
}

#[test]
fn phase_recovery_rejects_legacy_aggregate_and_self_selected_expectations() {
    let legacy = vec![
        assertion(
            "all-phase-interruptions-recovered",
            json!(true),
            json!(true),
        ),
        assertion("unsafe-activation", json!(false), json!(false)),
    ];
    assert!(validate_assertions("phase-interruption-recovery", false, None, &legacy).is_err());
    let mut incomplete = phase_recovery_assertions();
    incomplete[0].expected = json!(false);
    incomplete[0].actual = json!(false);
    assert!(validate_assertions("phase-interruption-recovery", false, None, &incomplete).is_err());
}
