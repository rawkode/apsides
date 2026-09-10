use super::*;
use crate::{PolicyState, SCHEMA_VERSION};
use compiler_core::{Capability, PermissionCeiling};
use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tower::service_fn;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CLUSTER: &str = "apsides-qual-0123456789abcdef0123456789abcdef";
const NODE: &str = "apsides-qual-0123456789abcdef0123456789abcdef-control-plane";
const PROVIDER: &str = "kind://docker/apsides-qual-0123456789abcdef0123456789abcdef/apsides-qual-0123456789abcdef0123456789abcdef-control-plane";
type Calls = Arc<Mutex<Vec<(String, String)>>>;

fn capability(name: &str, verb: &str) -> Capability {
    Capability {
        namespace: "production".into(),
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        verb: verb.into(),
        names: vec![name.into()],
    }
}

fn cluster_target() -> CandidateQualificationTarget {
    CandidateQualificationTarget {
        run_id: "0123456789abcdef0123456789abcdef".into(),
        kube_system_namespace_uid: "system-uid".into(),
        control_namespace_uid: "control-uid".into(),
        workload_namespace_uid: "workload-uid".into(),
        sentinel_namespace_uid: "sentinel-uid".into(),
        marker_uid: "marker-uid".into(),
        marker_resource_version: "17".into(),
        node_name: NODE.into(),
        node_uid: "node-uid".into(),
        node_provider_id: PROVIDER.into(),
        node_image_digest: DIGEST_A.into(),
    }
}

fn target() -> CandidatePairQualificationTarget {
    CandidatePairQualificationTarget {
        purpose: CandidatePairPurpose::PermissionExpansion {},
        cluster: cluster_target(),
        initial_policy_digest: DIGEST_A.into(),
        successor_policy_digest: DIGEST_B.into(),
    }
}

fn marker() -> CandidatePairQualificationMarker {
    let target = target();
    CandidatePairQualificationMarker {
        purpose: CandidatePairPurpose::PermissionExpansion {},
        schema_version: CANDIDATE_PAIR_QUALIFICATION_SCHEMA.into(),
        run_id: target.cluster.run_id.clone(),
        cluster_name: CLUSTER.into(),
        sentinel_namespace: CLUSTER.into(),
        domain_id: "production-domain".into(),
        cluster_binding: "production-cluster".into(),
        control_namespace: "apsides-system".into(),
        workload_namespace: "production".into(),
        kube_system_namespace_uid: target.cluster.kube_system_namespace_uid,
        control_namespace_uid: target.cluster.control_namespace_uid,
        workload_namespace_uid: target.cluster.workload_namespace_uid,
        sentinel_namespace_uid: target.cluster.sentinel_namespace_uid,
        node_name: target.cluster.node_name,
        node_uid: target.cluster.node_uid,
        node_provider_id: target.cluster.node_provider_id,
        node_image_digest: target.cluster.node_image_digest,
        initial_release_digest: DIGEST_A.into(),
        initial_ir_digest: DIGEST_A.into(),
        initial_provenance_digest: DIGEST_A.into(),
        successor_release_digest: DIGEST_B.into(),
        successor_ir_digest: DIGEST_B.into(),
        successor_provenance_digest: DIGEST_B.into(),
        trust_policy_digest: DIGEST_A.into(),
        initial_policy_digest: DIGEST_A.into(),
        successor_policy_digest: DIGEST_B.into(),
    }
}

fn binding() -> CandidateBinding<'static> {
    CandidateBinding {
        domain_id: "production-domain",
        release_digest: DIGEST_B,
        cluster_binding: "production-cluster",
        control_namespace: "apsides-system",
        workload_namespace: "production",
    }
}

fn namespace(name: &str, uid: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":name,"uid":uid,"resourceVersion":"1"}})
}

fn node() -> Value {
    json!({"apiVersion":"v1","kind":"Node","metadata":{"name":NODE,"uid":"node-uid","resourceVersion":"1"},
        "spec":{"providerID":PROVIDER},"status":{"conditions":[{"type":"Ready","status":"True"}]}})
}

fn marker_object(record: &CandidatePairQualificationMarker) -> TestResult<Value> {
    Ok(
        json!({"apiVersion":"v1","kind":"ConfigMap","immutable":true,
        "metadata":{"name":CANDIDATE_PAIR_QUALIFICATION_MARKER_NAME,"namespace":CLUSTER,"uid":"marker-uid","resourceVersion":"17"},
        "data":{"state.json":serde_json::to_string(record)?}}),
    )
}

fn policy_object(value: &PolicyState) -> TestResult<Value> {
    Ok(json!({"apiVersion":"v1","kind":"ConfigMap",
        "metadata":{"name":"apsides-policy-production-domain","namespace":"apsides-system","uid":"policy-uid","resourceVersion":"5"},
        "data":{"state.json":serde_json::to_string(value)?}}))
}

fn policy() -> PolicyState {
    let ceiling = PermissionCeiling {
        policy_ref: "platform/production-domain".into(),
        domain_id: "production-domain".into(),
        cluster_binding: "production-cluster".into(),
        workload_namespace: "production".into(),
        control_namespace: "apsides-system".into(),
        capabilities: vec![],
        accepted_enforcement_gaps: vec![],
        accepted_indirect_risks: vec![],
    };
    PolicyState {
        image_pull_secrets: vec![],
        schema_version: SCHEMA_VERSION.into(),
        runtime_ceiling: ceiling.clone(),
        cleanup_ceiling: ceiling,
    }
}

fn objects(
    target: &CandidatePairQualificationTarget,
    record: &CandidatePairQualificationMarker,
) -> TestResult<BTreeMap<String, Value>> {
    Ok(BTreeMap::from([
        (
            "/api/v1/namespaces/kube-system".into(),
            namespace("kube-system", &target.cluster.kube_system_namespace_uid),
        ),
        (
            "/api/v1/namespaces/apsides-system".into(),
            namespace("apsides-system", &target.cluster.control_namespace_uid),
        ),
        (
            "/api/v1/namespaces/production".into(),
            namespace("production", &target.cluster.workload_namespace_uid),
        ),
        (
            format!("/api/v1/namespaces/{CLUSTER}"),
            namespace(CLUSTER, &target.cluster.sentinel_namespace_uid),
        ),
        (format!("/api/v1/nodes/{NODE}"), node()),
        (
            format!(
                "/api/v1/namespaces/{CLUSTER}/configmaps/{CANDIDATE_PAIR_QUALIFICATION_MARKER_NAME}"
            ),
            marker_object(record)?,
        ),
    ]))
}

fn wire(objects: BTreeMap<String, Value>) -> (Client, Calls) {
    let objects = Arc::new(objects);
    let calls: Calls = Arc::default();
    let service = service_fn({
        let calls = calls.clone();
        move |request: Request<Body>| {
            let objects = objects.clone();
            let calls = calls.clone();
            async move {
                let (parts, body) = request.into_parts();
                let _ = body.collect().await.map_err(std::io::Error::other)?;
                calls
                    .lock()
                    .map_err(|_| std::io::Error::other("calls lock poisoned"))?
                    .push((parts.method.to_string(), parts.uri.path().into()));
                let value = objects
                    .get(parts.uri.path())
                    .cloned()
                    .ok_or_else(|| std::io::Error::other("unexpected Kubernetes request"))?;
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&value).map_err(std::io::Error::other)?,
                    ))
                    .map_err(std::io::Error::other)
            }
        }
    });
    (Client::new(service, "default"), calls)
}

fn mock_platform(objects: BTreeMap<String, Value>) -> TestResult<(PlatformClient, Calls)> {
    let (client, calls) = wire(objects);
    Ok((
        PlatformClient::new(client, "apsides-system", "production", "production-cluster")?,
        calls,
    ))
}

fn assert_read_only(calls: &Calls) -> TestResult {
    let calls = calls
        .lock()
        .map_err(|_| std::io::Error::other("calls lock poisoned"))?;
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|(method, _)| method == "GET"));
    Ok(())
}

#[test]
fn requires_strict_capability_addition() {
    let read = capability("application", "get");
    let write = capability("application", "update");
    assert!(strict_capability_expansion(
        std::slice::from_ref(&read),
        &[read.clone(), write.clone()]
    ));
    assert!(!strict_capability_expansion(
        std::slice::from_ref(&read),
        std::slice::from_ref(&read)
    ));
    assert!(!strict_capability_expansion(
        &[read, write],
        &[capability("application", "update")]
    ));
}

#[test]
fn pair_metadata_schemas_are_closed() -> TestResult {
    let mut target_value = serde_json::to_value(target())?;
    target_value
        .as_object_mut()
        .ok_or("target was not an object")?
        .insert("unexpected".into(), json!(true));
    assert!(serde_json::from_value::<CandidatePairQualificationTarget>(target_value).is_err());

    let mut missing = serde_json::to_value(target())?;
    missing
        .as_object_mut()
        .ok_or("target was not an object")?
        .remove("purpose");
    assert!(serde_json::from_value::<CandidatePairQualificationTarget>(missing).is_err());
    for purpose in [
        json!({"kind":"unknown"}),
        json!({"kind":"last-type-cleanup"}),
        json!({"kind":"last-type-cleanup","identity":{"apiGroup":"","resource":"services","namespace":"production","name":"obsolete","unexpected":true}}),
        json!({"kind":"permission-expansion","identity":{}}),
    ] {
        assert!(serde_json::from_value::<CandidatePairPurpose>(purpose).is_err());
    }

    let mut marker_value = serde_json::to_value(marker())?;
    marker_value
        .as_object_mut()
        .ok_or("marker was not an object")?
        .insert("unexpected".into(), json!(true));
    assert!(serde_json::from_value::<CandidatePairQualificationMarker>(marker_value).is_err());

    for field in [
        "purpose",
        "initialReleaseDigest",
        "successorReleaseDigest",
        "initialPolicyDigest",
        "successorPolicyDigest",
    ] {
        let mut value = serde_json::to_value(marker())?;
        assert!(
            value
                .as_object_mut()
                .ok_or("marker was not an object")?
                .remove(field)
                .is_some()
        );
        assert!(serde_json::from_value::<CandidatePairQualificationMarker>(value).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn exact_pair_marker_and_topology_are_accepted_read_only() -> TestResult {
    let target = target();
    let expected = marker();
    let (platform, calls) = mock_platform(objects(&target, &expected)?)?;
    platform
        .verify_pair_target(&binding(), &target, &expected)
        .await?;
    assert_read_only(&calls)?;
    Ok(())
}

#[tokio::test]
async fn ordered_pair_marker_or_object_identity_drift_is_rejected_read_only() -> TestResult {
    for change in [
        "release-order",
        "ir-order",
        "provenance-order",
        "policy-order",
        "purpose",
        "old-schema",
        "uid",
        "revision",
        "mutable",
    ] {
        let target = target();
        let expected = marker();
        let mut observed = expected.clone();
        match change {
            "release-order" => {
                std::mem::swap(
                    &mut observed.initial_release_digest,
                    &mut observed.successor_release_digest,
                );
            }
            "ir-order" => {
                std::mem::swap(
                    &mut observed.initial_ir_digest,
                    &mut observed.successor_ir_digest,
                );
            }
            "provenance-order" => {
                std::mem::swap(
                    &mut observed.initial_provenance_digest,
                    &mut observed.successor_provenance_digest,
                );
            }
            "policy-order" => {
                std::mem::swap(
                    &mut observed.initial_policy_digest,
                    &mut observed.successor_policy_digest,
                );
            }
            "purpose" => {
                observed.purpose = CandidatePairPurpose::LastTypeCleanup {
                    identity: deploy::ResourceIdentity {
                        api_group: String::new(),
                        resource: "services".into(),
                        namespace: "production".into(),
                        name: "obsolete".into(),
                    },
                }
            }
            "old-schema" => {
                observed.schema_version = "apsides.dev/candidate-pair-qualification/v1".into();
            }
            "uid" | "revision" | "mutable" => {}
            _ => return Err("unknown pair marker mutation".into()),
        }
        let mut values = objects(&target, &observed)?;
        let path = format!(
            "/api/v1/namespaces/{CLUSTER}/configmaps/{CANDIDATE_PAIR_QUALIFICATION_MARKER_NAME}"
        );
        let object = values.get_mut(&path).ok_or("pair marker missing")?;
        match change {
            "uid" => object["metadata"]["uid"] = json!("replacement-marker"),
            "revision" => object["metadata"]["resourceVersion"] = json!("18"),
            "mutable" => object["immutable"] = json!(false),
            _ => {}
        }
        let (platform, calls) = mock_platform(values)?;
        assert!(
            platform
                .verify_pair_target(&binding(), &target, &expected)
                .await
                .is_err(),
            "pair marker change {change} was accepted"
        );
        assert_read_only(&calls)?;
    }
    Ok(())
}

#[tokio::test]
async fn current_policy_must_match_the_marker_digest() -> TestResult {
    let value = policy();
    let path = "/api/v1/namespaces/apsides-system/configmaps/apsides-policy-production-domain";
    let objects = BTreeMap::from([(path.into(), policy_object(&value)?)]);
    let expected = compiler_core::digest(&value)?;
    let (platform, calls) = mock_platform(objects)?;
    platform
        .verify_pair_policy_digest("production-domain", &expected)
        .await?;
    assert_read_only(&calls)?;

    let objects = BTreeMap::from([(path.into(), policy_object(&value)?)]);
    let (platform, calls) = mock_platform(objects)?;
    assert!(
        platform
            .verify_pair_policy_digest("production-domain", DIGEST_A)
            .await
            .is_err()
    );
    assert_read_only(&calls)?;
    Ok(())
}

#[test]
fn lifecycle_operations_select_only_their_fixed_candidate_role() {
    for operation in [
        CandidatePairLifecycleOperation::StageInitial,
        CandidatePairLifecycleOperation::ActivateInitial,
    ] {
        assert_eq!(operation.role(), CandidatePairRole::Initial);
    }
    for operation in [
        CandidatePairLifecycleOperation::StageSuccessor,
        CandidatePairLifecycleOperation::QuiesceUpgrade,
        CandidatePairLifecycleOperation::ActivateUpgrade,
        CandidatePairLifecycleOperation::CommitUpgrade,
    ] {
        assert_eq!(operation.role(), CandidatePairRole::Successor);
    }
    assert_eq!(
        candidate_pair_qualification_executor(),
        CANDIDATE_PAIR_QUALIFICATION_EXECUTOR
    );
}
