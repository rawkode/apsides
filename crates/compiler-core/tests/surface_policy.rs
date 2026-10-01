//! `surface_policy.rs`: strict parsing of `apsides.surface-policy.json` and
//! evaluating its rules against a `SurfaceDiff`.
use compiler_core::*;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn domain() -> SurfaceDomain {
    SurfaceDomain {
        id: "team-api-production".into(),
        namespace: "production".into(),
        control_namespace: "apsides-system".into(),
    }
}

fn kubernetes() -> SurfaceKubernetes {
    SurfaceKubernetes {
        min_version: "1.34".into(),
        qualified_on: "v1.34.3".into(),
    }
}

fn empty_surface() -> Surface {
    Surface {
        schema_version: SURFACE_SCHEMA.into(),
        domain: domain(),
        kubernetes: kubernetes(),
        permissions: Vec::new(),
        secrets_read: Vec::new(),
        resources: Vec::new(),
        inputs: Vec::new(),
        reactions: Vec::new(),
        images: Vec::new(),
        handlers: Vec::new(),
        egress: Vec::new(),
        outputs: Vec::new(),
        risks: Vec::new(),
        enforcement_gaps: Vec::new(),
    }
}

#[test]
fn parse_policy_accepts_the_documented_shape() -> TestResult {
    let policy = parse_policy(
        r#"{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":["permissions.added","images.unpinned"]}"#,
    )?;
    assert_eq!(
        policy.require_approval,
        vec![PolicyRule::PermissionsAdded, PolicyRule::ImagesUnpinned]
    );
    Ok(())
}

#[test]
fn parse_policy_rejects_an_unknown_rule_name() {
    let result = parse_policy(
        r#"{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":["resources.renamed"]}"#,
    );
    assert!(matches!(result, Err(Error::Invalid(_))));
}

#[test]
fn parse_policy_rejects_unknown_fields_and_wrong_schema() {
    assert!(
        parse_policy(
            r#"{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":[],"extra":true}"#
        )
        .is_err()
    );
    assert!(
        parse_policy(r#"{"schemaVersion":"apsides.dev/surface-policy/v0","requireApproval":[]}"#)
            .is_err()
    );
}

#[test]
fn evaluate_policy_reports_triggered_rules_in_declared_order() {
    let policy = SurfacePolicy {
        schema_version: SURFACE_POLICY_SCHEMA.into(),
        require_approval: vec![
            PolicyRule::KubernetesChanged,
            PolicyRule::EgressAdded,
            PolicyRule::PermissionsAdded,
            PolicyRule::ResourcesRemoved,
        ],
    };

    let base = empty_surface();
    let mut head = empty_surface();
    head.kubernetes.min_version = "1.35".into();
    head.permissions = vec![SurfacePermission {
        namespace: "production".into(),
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        verbs: vec![SurfaceVerb {
            verb: "get".into(),
            all_names: true,
            names: None,
        }],
    }];

    let diff = diff_surfaces(&base, &head);
    let triggered = evaluate_policy(&policy, &diff);

    // Neither surface declares a handler, so `egress.added` does not match
    // in this scenario; `resources.removed` does not match either.
    assert_eq!(
        triggered,
        vec![PolicyRule::KubernetesChanged, PolicyRule::PermissionsAdded]
    );
}

fn empty_diff() -> SurfaceDiff {
    SurfaceDiff {
        schema_version: SURFACE_DIFF_SCHEMA.into(),
        domain: None,
        kubernetes: None,
        permissions: PermissionsDiff::default(),
        secrets_read: SecretsReadDiff::default(),
        resources: ResourcesDiff::default(),
        inputs: InputsDiff::default(),
        reactions: ReactionsDiff::default(),
        images: ImagesDiff::default(),
        handlers: HandlersDiff::default(),
        egress: EgressDiff::default(),
        outputs: OutputsDiff::default(),
        risks: StringSetDiff::default(),
        enforcement_gaps: StringSetDiff::default(),
    }
}

fn permission_change(
    verbs_added: Vec<&str>,
    verbs_removed: Vec<&str>,
    verbs_changed: Vec<PermissionVerbChange>,
) -> PermissionChange {
    PermissionChange {
        namespace: "production".into(),
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        verbs_added: verbs_added.into_iter().map(str::to_owned).collect(),
        verbs_removed: verbs_removed.into_iter().map(str::to_owned).collect(),
        verbs_changed,
    }
}

fn verb_change(
    verb: &str,
    names_added: Vec<&str>,
    names_removed: Vec<&str>,
    widened_to_all_names: bool,
    narrowed_from_all_names: bool,
) -> PermissionVerbChange {
    PermissionVerbChange {
        verb: verb.into(),
        names_added: names_added.into_iter().map(str::to_owned).collect(),
        names_removed: names_removed.into_iter().map(str::to_owned).collect(),
        widened_to_all_names,
        narrowed_from_all_names,
    }
}

fn require(rule: PolicyRule) -> SurfacePolicy {
    SurfacePolicy {
        schema_version: SURFACE_POLICY_SCHEMA.into(),
        require_approval: vec![rule],
    }
}

#[test]
fn permissions_added_matches_a_new_verb_on_an_existing_row() {
    let mut diff = empty_diff();
    diff.permissions.changed = vec![permission_change(vec!["watch"], vec![], vec![])];
    assert_eq!(
        evaluate_policy(&require(PolicyRule::PermissionsAdded), &diff),
        vec![PolicyRule::PermissionsAdded]
    );
}

#[test]
fn permissions_added_matches_a_name_added_to_an_existing_verb() {
    let mut diff = empty_diff();
    diff.permissions.changed = vec![permission_change(
        vec![],
        vec![],
        vec![verb_change("get", vec!["new-config"], vec![], false, false)],
    )];
    assert_eq!(
        evaluate_policy(&require(PolicyRule::PermissionsAdded), &diff),
        vec![PolicyRule::PermissionsAdded]
    );
}

#[test]
fn permissions_added_matches_a_verb_widened_to_all_names() {
    let mut diff = empty_diff();
    diff.permissions.changed = vec![permission_change(
        vec![],
        vec![],
        vec![verb_change("get", vec![], vec![], true, false)],
    )];
    assert_eq!(
        evaluate_policy(&require(PolicyRule::PermissionsAdded), &diff),
        vec![PolicyRule::PermissionsAdded]
    );
}

#[test]
fn permissions_added_does_not_match_a_pure_narrowing() {
    // Each of `diff_verb`'s own narrowing outputs, separately: a dropped
    // verb, a name dropped from an already name-restricted verb (neither
    // side `allNames`, so no widening/narrowing flag applies), and a verb
    // narrowed from `allNames` back to specific names (which never carries
    // `namesRemoved` itself — see `diff_verb` — so it is its own case, not
    // combined with the one above).
    let mut diff = empty_diff();
    diff.permissions.changed = vec![permission_change(
        vec![],
        vec!["delete"],
        vec![
            verb_change("list", vec![], vec!["old-config"], false, false),
            verb_change("watch", vec![], vec![], false, true),
        ],
    )];

    assert!(evaluate_policy(&require(PolicyRule::PermissionsAdded), &diff).is_empty());
    // A narrowing is still a change, just never a widening: `permissions.changed`
    // must still see it.
    assert_eq!(
        evaluate_policy(&require(PolicyRule::PermissionsChanged), &diff),
        vec![PolicyRule::PermissionsChanged]
    );
}

#[test]
fn evaluate_policy_is_empty_when_nothing_matches() {
    let policy = SurfacePolicy {
        schema_version: SURFACE_POLICY_SCHEMA.into(),
        require_approval: vec![PolicyRule::SecretsReadAdded, PolicyRule::RisksAdded],
    };
    let surface = empty_surface();
    let diff = diff_surfaces(&surface, &surface.clone());
    assert!(evaluate_policy(&policy, &diff).is_empty());
}

#[test]
fn handlers_added_matches_a_new_handler() {
    let mut diff = empty_diff();
    diff.handlers.added = vec![SurfaceHandler {
        id: "sync-flags".into(),
        trigger: "settings".into(),
        inputs: vec!["settings".into()],
        outputs: vec!["flags".into()],
    }];
    assert_eq!(
        evaluate_policy(&require(PolicyRule::HandlersAdded), &diff),
        vec![PolicyRule::HandlersAdded]
    );
}

#[test]
fn egress_added_matches_a_new_egress_edge() {
    let mut diff = empty_diff();
    diff.egress.added = vec![SurfaceEgress {
        host: "api.example.com".into(),
        port: 443,
        handler: "sync-flags".into(),
    }];
    assert_eq!(
        evaluate_policy(&require(PolicyRule::EgressAdded), &diff),
        vec![PolicyRule::EgressAdded]
    );
}

#[test]
fn egress_added_does_not_match_a_removal() {
    let mut diff = empty_diff();
    diff.egress.removed = vec![SurfaceEgress {
        host: "api.example.com".into(),
        port: 443,
        handler: "sync-flags".into(),
    }];
    assert!(evaluate_policy(&require(PolicyRule::EgressAdded), &diff).is_empty());
}
