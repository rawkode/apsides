use super::*;
use http::{Request, Response};
use kube::{Client, client::Body};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tower::service_fn;

fn review_target(resource: &str, namespace: &str, name: &str) -> ReviewIdentity {
    ReviewIdentity {
        api_group: String::new(),
        resource: resource.into(),
        namespace: namespace.into(),
        name: name.into(),
    }
}

fn deployment_target(namespace: &str, name: &str) -> ReviewIdentity {
    ReviewIdentity {
        api_group: "apps".into(),
        resource: "deployments".into(),
        namespace: namespace.into(),
        name: name.into(),
    }
}

fn preview(targets: Vec<ReviewIdentity>) -> DeploymentPreviewApproval {
    DeploymentPreviewApproval {
        profile: DeploymentPreviewProfile::StaticSsaIssuedDeployerV1,
        targets,
    }
}

fn identity(
    schema_version: &str,
    preview: Option<DeploymentPreviewApproval>,
) -> DeploymentIdentity {
    DeploymentIdentity {
        schema_version: schema_version.into(),
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        service_account_name: "deployer".into(),
        service_account_uid: "service-account-uid".into(),
        binding_secret_name: "binding".into(),
        binding_secret_uid: "binding-secret-uid".into(),
        audience: "https://kubernetes.default.svc".into(),
        lifetime_seconds: 600,
        preview,
    }
}

fn validate(identity: &DeploymentIdentity) -> Result<()> {
    identity.validate("example", "cluster-a", "workload")
}

#[test]
fn v2_opt_out_or_known_profile_are_valid_and_other_schemas_are_not() -> Result<()> {
    assert!(validate(&identity(crate::SCHEMA_VERSION, None)).is_err());
    assert!(validate(&identity(DEPLOYMENT_IDENTITY_SCHEMA_V2, None)).is_ok());
    let empty_approval = identity(DEPLOYMENT_IDENTITY_SCHEMA_V2, Some(preview(Vec::new())));
    assert!(validate(&empty_approval).is_ok());
    let empty_wire = serde_json::to_value(empty_approval)
        .map_err(|_| invalid("deployment identity test value is invalid"))?;
    assert_eq!(
        empty_wire
            .pointer("/preview/targets")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    let all_managed_targets = preview(vec![
        review_target("configmaps", "workload", "settings"),
        review_target("services", "workload", "service"),
        deployment_target("workload", "controller"),
    ]);
    assert!(
        validate(&identity(
            DEPLOYMENT_IDENTITY_SCHEMA_V2,
            Some(all_managed_targets),
        ))
        .is_ok()
    );

    let mut unknown_profile = serde_json::to_value(identity(
        DEPLOYMENT_IDENTITY_SCHEMA_V2,
        Some(preview(vec![review_target(
            "configmaps",
            "workload",
            "settings",
        )])),
    ))
    .map_err(|_| invalid("deployment identity test value is invalid"))?;
    assert_eq!(
        unknown_profile
            .pointer("/preview/profile")
            .and_then(Value::as_str),
        Some("static-ssa-issued-deployer/v1")
    );
    let profile = unknown_profile
        .pointer_mut("/preview/profile")
        .ok_or_else(|| invalid("deployment identity test profile is missing"))?;
    *profile = json!("otherProfile");
    assert!(serde_json::from_value::<DeploymentIdentity>(unknown_profile).is_err());

    let mut missing_profile = serde_json::to_value(identity(
        DEPLOYMENT_IDENTITY_SCHEMA_V2,
        Some(preview(vec![review_target(
            "configmaps",
            "workload",
            "settings",
        )])),
    ))
    .map_err(|_| invalid("deployment identity test value is invalid"))?;
    let approval = missing_profile
        .get_mut("preview")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("deployment identity test approval is missing"))?;
    approval.remove("profile");
    assert!(serde_json::from_value::<DeploymentIdentity>(missing_profile).is_err());
    Ok(())
}

#[test]
fn preview_targets_are_closed_scoped_sorted_unique_and_bounded() {
    let valid = identity(
        DEPLOYMENT_IDENTITY_SCHEMA_V2,
        Some(preview(vec![review_target(
            "configmaps",
            "workload",
            "settings",
        )])),
    );
    assert!(valid.validate("other", "cluster-a", "workload").is_err());
    assert!(
        valid
            .validate("example", "other-cluster", "workload")
            .is_err()
    );
    assert!(valid.validate("example", "cluster-a", "other").is_err());

    let invalid_targets = [
        vec![review_target("configmaps", "other", "settings")],
        vec![review_target("secrets", "workload", "settings")],
        vec![review_target("configmaps", "workload", "Invalid")],
        vec![ReviewIdentity {
            api_group: "apps".into(),
            resource: "configmaps".into(),
            namespace: "workload".into(),
            name: "settings".into(),
        }],
        vec![
            review_target("configmaps", "workload", "settings"),
            review_target("configmaps", "workload", "settings"),
        ],
        vec![
            review_target("services", "workload", "service"),
            review_target("configmaps", "workload", "settings"),
        ],
    ];
    for targets in invalid_targets {
        assert!(
            validate(&identity(
                DEPLOYMENT_IDENTITY_SCHEMA_V2,
                Some(preview(targets)),
            ))
            .is_err()
        );
    }

    let targets = (0..=MAX_PREVIEW_TARGETS)
        .map(|index| review_target("configmaps", "workload", &format!("target-{index:03}")))
        .collect::<Vec<_>>();
    assert!(
        validate(&identity(
            DEPLOYMENT_IDENTITY_SCHEMA_V2,
            Some(preview(targets.clone())),
        ))
        .is_err()
    );
    let bounded = targets.into_iter().take(MAX_PREVIEW_TARGETS).collect();
    assert!(
        validate(&identity(
            DEPLOYMENT_IDENTITY_SCHEMA_V2,
            Some(preview(bounded)),
        ))
        .is_ok()
    );
}

#[test]
fn preview_wire_rejects_unknown_fields() -> Result<()> {
    let mut value = serde_json::to_value(identity(
        DEPLOYMENT_IDENTITY_SCHEMA_V2,
        Some(preview(vec![review_target(
            "configmaps",
            "workload",
            "settings",
        )])),
    ))
    .map_err(|_| invalid("deployment identity test value is invalid"))?;
    let approval = value
        .get_mut("preview")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("deployment identity test approval is missing"))?;
    approval.insert("privateCanary".into(), json!("must-not-be-accepted"));
    assert!(serde_json::from_value::<DeploymentIdentity>(value).is_err());
    Ok(())
}

fn protected_state(identity: &DeploymentIdentity, revision: &str) -> Result<Value> {
    let state = serde_json::to_string(identity)
        .map_err(|_| invalid("deployment identity test state is invalid"))?;
    Ok(json!({
        "apiVersion":"v1",
        "kind":"ConfigMap",
        "metadata":{
            "name":"apsides-deploy-identity-example",
            "namespace":"control",
            "uid":"identity-uid",
            "resourceVersion":revision
        },
        "data":{"state.json":state}
    }))
}

fn test_client(replies: Vec<Value>) -> Result<PlatformClient> {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let service = service_fn(move |_: Request<Body>| {
        let replies = replies.clone();
        async move {
            let reply = replies
                .lock()
                .map_err(|_| std::io::Error::other("deployment identity reply lock poisoned"))?
                .pop_front()
                .ok_or_else(|| std::io::Error::other("deployment identity reply missing"))?;
            let bytes = serde_json::to_vec(&reply).map_err(std::io::Error::other)?;
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(bytes))
                .map_err(std::io::Error::other)
        }
    });
    PlatformClient::new(
        Client::new(service, "control"),
        "control",
        "workload",
        "cluster-a",
    )
}

#[tokio::test]
async fn identity_change_while_minting_still_rejects() -> Result<()> {
    let approval = preview(vec![review_target("configmaps", "workload", "settings")]);
    let before = identity(DEPLOYMENT_IDENTITY_SCHEMA_V2, Some(approval.clone()));
    let mut after = identity(DEPLOYMENT_IDENTITY_SCHEMA_V2, Some(approval));
    after.audience = "other-audience".into();
    let expires = k8s_openapi::jiff::Timestamp::from_second(
        k8s_openapi::jiff::Timestamp::now().as_second() + 600,
    )
    .map_err(|_| invalid("deployment identity test expiration is invalid"))?;
    let replies = vec![
        protected_state(&before, "1")?,
        json!({"metadata":{"uid":"service-account-uid"}}),
        json!({"metadata":{"uid":"binding-secret-uid"}}),
        json!({"status":{
            "token":"synthetic-test-token",
            "expirationTimestamp":expires.to_string()
        }}),
        protected_state(&after, "2")?,
    ];
    let error = test_client(replies)?
        .approved_deployment_credential("example")
        .await
        .err()
        .ok_or_else(|| invalid("changed deployment identity was accepted"))?;
    assert!(error.to_string().contains("changed while minting"));
    Ok(())
}
