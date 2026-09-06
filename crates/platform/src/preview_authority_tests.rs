use super::*;
use coordination::{ReviewIdentity, ReviewResource};

fn resource(name: &str) -> ReviewResource {
    ReviewResource {
        id: name.into(),
        identity: ReviewIdentity {
            api_group: String::new(),
            resource: "configmaps".into(),
            namespace: "workload".into(),
            name: name.into(),
        },
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        apply_fields: vec!["/data/key".into()],
        lifecycle: coordination::ReviewLifecycle {
            deletion_policy: coordination::ReviewDeletionPolicy::Orphan,
        },
        force: false,
        source: coordination::ReviewSource {
            file: "source.tsx".into(),
            line: 1,
            column: 1,
        },
        ownership_plan: coordination::ReviewOwnershipPlan::CreateIfAbsent,
    }
}

#[test]
fn empty_approval_is_deny_all_and_every_target_must_be_approved() {
    let mut approval = crate::DeploymentPreviewApproval {
        profile: crate::DeploymentPreviewProfile::StaticSsaIssuedDeployerV1,
        targets: vec![],
    };
    assert!(require_targets(&approval, &[]).is_ok());
    assert!(require_targets(&approval, &[resource("a")]).is_err());
    approval.targets.push(resource("a").identity);
    assert!(require_targets(&approval, &[resource("a")]).is_ok());
    assert!(require_targets(&approval, &[resource("a"), resource("b")]).is_err());
    let mut foreign = resource("a");
    foreign.identity.namespace = "other".into();
    assert!(require_targets(&approval, &[foreign]).is_err());
}

#[test]
fn authority_pins_uid_resource_version_and_contents() {
    let before = Versioned {
        uid: "uid".into(),
        resource_version: "1".into(),
        value: 42,
    };
    assert!(same(&before, &before));
    let mut after = before.clone();
    after.uid = "replacement".into();
    assert!(!same(&before, &after));
    after = before.clone();
    after.resource_version = "2".into();
    assert!(!same(&before, &after));
    after = before.clone();
    after.value = 43;
    assert!(!same(&before, &after));
}

#[test]
fn conditional_create_preview_requires_exact_qualified_server() {
    assert!(require_qualified_server("v1.34.3").is_ok());
    for version in ["v1.34.2", "v1.34.3+unqualified", "v1.35.0", ""] {
        assert!(require_qualified_server(version).is_err());
    }
}
