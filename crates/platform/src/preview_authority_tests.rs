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
        api_resource: None,
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

fn info(
    major: &str,
    minor: &str,
    git_version: &str,
) -> k8s_openapi::apimachinery::pkg::version::Info {
    k8s_openapi::apimachinery::pkg::version::Info {
        major: major.into(),
        minor: minor.into(),
        git_version: git_version.into(),
        ..Default::default()
    }
}

#[test]
fn conditional_create_preview_requires_the_declared_minimum_server() {
    assert!(require_qualified_server(&info("1", "34", "v1.34.3"), "1.34").is_ok());
    assert!(require_qualified_server(&info("1", "35", "v1.35.0"), "1.34").is_ok());
    assert!(require_qualified_server(&info("1", "35+", "v1.35.2"), "1.34").is_ok());
    assert!(require_qualified_server(&info("2", "0", "v2.0.0"), "1.34").is_ok());
    assert!(require_qualified_server(&info("1", "33", "v1.33.9"), "1.34").is_err());
    assert!(require_qualified_server(&info("1", "34", "v1.34.3"), "1.35").is_err());
    assert!(require_qualified_server(&info("garbage", "garbage", ""), "1.34").is_err());
    assert!(require_qualified_server(&info("1", "34", "v1.34.3"), "garbage").is_err());
}
