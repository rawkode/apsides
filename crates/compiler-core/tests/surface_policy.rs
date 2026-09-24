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

    // `egress.added` is accepted but never matches (no `egress` section
    // exists yet); `resources.removed` does not match either.
    assert_eq!(
        triggered,
        vec![PolicyRule::KubernetesChanged, PolicyRule::PermissionsAdded]
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
