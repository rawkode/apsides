//! These exercise the production bounded orchestration kernel after the sealed
//! `IntegrityVerifiedCandidate` equality boundary. Signature/OCI verification is covered by release tests.
use super::*;
use crate::{DeploymentIdentity, DeploymentPreviewApproval, DeploymentPreviewProfile, Versioned};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;
struct Fixture {
    issued: IssuedDeployment,
    request: DeploymentRequest,
    trust_digest: String,
    calls: Calls,
}

pub(super) struct NoInventory;
#[async_trait::async_trait]
impl InventoryStore for NoInventory {
    async fn load(
        &self,
        _: &inventory::InventoryScope,
    ) -> inventory::Result<Option<inventory::VersionedInventory>> {
        Err(inventory::Error::Backend("unexpected inventory head read"))
    }
    async fn read_snapshot(
        &self,
        _: &inventory::InventoryScope,
        _: &str,
    ) -> inventory::Result<Option<inventory::InventorySnapshot>> {
        Err(inventory::Error::Backend(
            "unexpected initial snapshot read",
        ))
    }
    async fn publish(
        &self,
        _: &inventory::InventoryScope,
        _: Option<&str>,
        _: &inventory::InventorySnapshot,
    ) -> inventory::Result<inventory::VersionedInventory> {
        Err(inventory::Error::Backend(
            "preview must never publish inventory",
        ))
    }
}

pub(super) fn state(
    name: &str,
    value: &impl serde::Serialize,
    uid: &str,
    rv: &str,
) -> TestResult<Value> {
    Ok(
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"control","uid":uid,"resourceVersion":rv},
        "data":{"state.json":serde_json::to_string(value)?}}),
    )
}
pub(super) fn version() -> Value {
    json!({"major":"1","minor":"34","gitVersion":"v1.34.3","gitCommit":"test","gitTreeState":"clean",
        "buildDate":"2026-01-01T00:00:00Z","goVersion":"go1.24","compiler":"gc","platform":"linux/amd64"})
}
pub(super) fn identity(targets: Vec<coordination::ReviewIdentity>) -> DeploymentIdentity {
    DeploymentIdentity {
        schema_version: crate::DEPLOYMENT_IDENTITY_SCHEMA_V2.into(),
        domain_id: "example".into(),
        cluster_binding: "cluster-a".into(),
        service_account_name: "deployer".into(),
        service_account_uid: "deployer-uid".into(),
        binding_secret_name: "binding".into(),
        binding_secret_uid: "binding-uid".into(),
        audience: "kubernetes".into(),
        lifetime_seconds: 600,
        preview: Some(DeploymentPreviewApproval {
            profile: DeploymentPreviewProfile::StaticSsaIssuedDeployerV1,
            targets,
        }),
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    Success,
    Denied,
    RegistryDrift,
    PolicyDrift,
    TrustDrift,
    FinalTargetDrift,
}

fn fixture(scenario: Scenario) -> TestResult<Fixture> {
    let (request, registry, policy, trust) = crate::tests::preview_fresh_fixture()?;
    let plan = deploy::plan_transition(&request)?;
    let review = crate::project_review_metadata(&plan)?;
    let targets = if matches!(scenario, Scenario::Denied) {
        Vec::new()
    } else {
        review
            .resources
            .iter()
            .map(|item| item.identity.clone())
            .collect()
    };
    let identity = identity(targets);
    let identity_object = state(
        "apsides-deploy-identity-example",
        &identity,
        "identity-uid",
        "1",
    )?;
    let registry_object = crate::tests::registry_object(
        &registry,
        &request.registry.registry_uid,
        &request.registry.revision,
    )?;
    let policy_object = state("apsides-policy-example", &policy, "policy-uid", "40")?;
    let trust_object = state("apsides-trust-example", &trust, "trust-uid", "43")?;
    let base = vec![
        identity_object.clone(),
        registry_object,
        policy_object,
        trust_object,
    ];
    let mut replies = vec![version()];
    for index in 0..6 {
        let mut group = base.clone();
        if index == 3 {
            match scenario {
                Scenario::RegistryDrift => {
                    group[1]["metadata"]["resourceVersion"] = json!("changed-registry");
                }
                Scenario::PolicyDrift => group[2]["metadata"]["uid"] = json!("changed-policy"),
                Scenario::TrustDrift => {
                    group[3]["metadata"]["resourceVersion"] = json!("changed-trust");
                }
                _ => {}
            }
        }
        replies.extend(group);
        if index == 2 {
            replies.extend([empty_list(), dry_run(&request)]);
        }
        if index == 4 {
            replies.push(if matches!(scenario,Scenario::FinalTargetDrift) {
                let mut object = dry_run(&request);
                object["metadata"]["uid"] = json!("appeared-uid");
                json!({"apiVersion":"v1","kind":"ConfigMapList","metadata":{"resourceVersion":"80"},"items":[object]})
            } else {empty_list()});
        }
    }
    replies.retain(|reply| reply["metadata"]["name"] != "apsides-deploy-identity-example");
    let (client, calls) = crate::tests::wire_with_identity(replies, identity_object)?;
    let platform = crate::tests::platform_client(client, "control", "workload", "cluster-a")?;
    let issued = IssuedDeployment::for_preview_test(
        platform,
        Versioned {
            value: identity,
            uid: "identity-uid".into(),
            resource_version: "1".into(),
        },
    )?;
    Ok(Fixture {
        issued,
        request,
        trust_digest: compiler_core::digest(&trust)?,
        calls,
    })
}
pub(super) fn empty_list() -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMapList","metadata":{"resourceVersion":"77"},"items":[]})
}
fn dry_run(request: &DeploymentRequest) -> Value {
    let mut desired = request.next.compiled.ir.resources[0].desired.clone();
    desired["metadata"]["resourceVersion"] = json!("77");
    desired["metadata"]["managedFields"] = json!([{"manager":"apsides-example","operation":"Apply","apiVersion":"v1",
        "fieldsType":"FieldsV1","fieldsV1":{"f:data":{"f:value":{}}}}]);
    desired
}

#[tokio::test]
async fn all_targets_are_approved_before_any_workload_request() -> TestResult {
    let fixture = fixture(Scenario::Denied)?;
    let result = Box::pin(preview_bounded(
        &fixture.issued,
        &fixture.request,
        &fixture.trust_digest,
        &NoInventory,
    ))
    .await;
    assert!(result.is_err());
    let calls = fixture.calls.lock().map_err(|_| "request lock")?;
    assert!(
        calls
            .iter()
            .all(|(method, path, _)| method == "GET" && !path.contains("/namespaces/workload/"))
    );
    assert!(
        calls
            .iter()
            .any(|(_, path, _)| path.ends_with("apsides-deploy-identity-example"))
    );
    Ok(())
}

#[tokio::test]
async fn authenticated_absent_preview_uses_only_conditional_dry_run_and_final_relist() -> TestResult
{
    let fixture = fixture(Scenario::Success)?;
    let report = Box::pin(preview_bounded(
        &fixture.issued,
        &fixture.request,
        &fixture.trust_digest,
        &NoInventory,
    ))
    .await?;
    assert_eq!(report.resources.len(), 1);
    assert_eq!(
        report.plan_digest,
        deploy::plan_transition(&fixture.request)?.plan_digest
    );
    assert!(!serde_json::to_string(&report)?.contains("approved"));
    let calls = fixture.calls.lock().map_err(|_| "request lock")?;
    let target_calls: Vec<_> = calls
        .iter()
        .filter(|(_, path, _)| path.contains("/namespaces/workload/"))
        .collect();
    assert_eq!(target_calls.len(), 3);
    assert_eq!(target_calls[0].0, "GET");
    assert!(
        target_calls[0]
            .1
            .contains("fieldSelector=metadata.name%3Dconfig")
    );
    assert_eq!(target_calls[1].0, "PATCH");
    for query in [
        "dryRun=All",
        "fieldManager=apsides-example",
        "fieldValidation=Strict",
    ] {
        assert!(target_calls[1].1.contains(query));
    }
    assert_eq!(
        target_calls[1].2["metadata"]["resourceVersion"],
        json!("77")
    );
    assert!(target_calls[1].2["metadata"].get("uid").is_none());
    assert_eq!(target_calls[2].0, "GET");
    assert!(
        calls.iter().all(|(method, path, _)| method == "GET"
            || (method == "PATCH" && path.contains("dryRun=All")))
    );
    assert!(!calls.iter().any(|(_, path, _)| path.contains("/secrets")
        || path.contains("/token")
        || path.contains("/exec")));
    Ok(())
}

#[tokio::test]
async fn changed_registry_policy_or_trust_discards_the_preview_after_patch() -> TestResult {
    for scenario in [
        Scenario::RegistryDrift,
        Scenario::PolicyDrift,
        Scenario::TrustDrift,
    ] {
        let fixture = fixture(scenario)?;
        assert!(
            Box::pin(preview_bounded(
                &fixture.issued,
                &fixture.request,
                &fixture.trust_digest,
                &NoInventory
            ))
            .await
            .is_err()
        );
        let calls = fixture.calls.lock().map_err(|_| "request lock")?;
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _, _)| method == "PATCH")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(
                    |(method, path, _)| method == "GET" && path.contains("/namespaces/workload/")
                )
                .count(),
            1
        );
        assert!(calls.iter().all(|(method, path, _)| method == "GET"
            || (method == "PATCH" && path.contains("dryRun=All"))));
    }
    Ok(())
}

#[tokio::test]
async fn final_target_reappearance_discards_all_preview_evidence() -> TestResult {
    let fixture = fixture(Scenario::FinalTargetDrift)?;
    assert!(
        Box::pin(preview_bounded(
            &fixture.issued,
            &fixture.request,
            &fixture.trust_digest,
            &NoInventory
        ))
        .await
        .is_err()
    );
    let calls = fixture.calls.lock().map_err(|_| "request lock")?;
    assert_eq!(
        calls
            .iter()
            .filter(|(method, path, _)| method == "GET" && path.contains("/namespaces/workload/"))
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _, _)| method == "PATCH")
            .count(),
        1
    );
    Ok(())
}
