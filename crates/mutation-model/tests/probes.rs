use std::fs;
use std::path::Path;

use mutation_model::{
    CONTROLLER_FIELD_MANAGER, MutationDecision, MutationIntent, ProbeCaseV0, plan,
};

#[test]
fn every_probe_fixture_matches_the_model() {
    let fixtures =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../probes/conditional-mutation/v0/fixtures");
    let mut paths: Vec<_> = fs::read_dir(fixtures)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty());

    for path in paths {
        let case: ProbeCaseV0 = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(plan(&case.input).unwrap(), case.expected, "{}", case.name);
    }
}

#[test]
fn conditional_apply_is_always_forced_by_the_controller_manager() {
    let fixture =
        include_str!("../../../probes/conditional-mutation/v0/fixtures/conditional-apply.json");
    let case: ProbeCaseV0 = serde_json::from_str(fixture).unwrap();
    let MutationDecision::ConditionalApply {
        force,
        field_manager,
        body,
        ..
    } = plan(&case.input).unwrap()
    else {
        panic!("fixture did not produce conditional apply")
    };
    assert!(force);
    assert_eq!(field_manager, CONTROLLER_FIELD_MANAGER);
    assert_eq!(body["metadata"]["uid"], "uid-1");
    assert_eq!(body["metadata"]["resourceVersion"], "17");
}

#[test]
fn there_is_no_adoption_decision() {
    let fixture =
        include_str!("../../../probes/conditional-mutation/v0/fixtures/create-collision.json");
    let case: ProbeCaseV0 = serde_json::from_str(fixture).unwrap();
    assert!(matches!(
        case.input.intent,
        MutationIntent::CreateConflict { .. }
    ));
    assert!(matches!(
        plan(&case.input).unwrap(),
        MutationDecision::RefetchNoAdopt { .. }
    ));
}
