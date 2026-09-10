use super::*;
use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tower::service_fn;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Calls = Arc<Mutex<Vec<(String, String)>>>;

const RUN_ID: &str = "0123456789abcdef0123456789abcdef";
const CLUSTER: &str = "apsides-qual-0123456789abcdef0123456789abcdef";
const NODE: &str = "apsides-qual-0123456789abcdef0123456789abcdef-control-plane";
const PROVIDER: &str = "kind://docker/apsides-qual-0123456789abcdef0123456789abcdef/apsides-qual-0123456789abcdef0123456789abcdef-control-plane";
const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn target() -> CandidateQualificationTarget {
    CandidateQualificationTarget {
        run_id: RUN_ID.into(),
        kube_system_namespace_uid: "uid-system".into(),
        control_namespace_uid: "uid-control".into(),
        workload_namespace_uid: "uid-workload".into(),
        sentinel_namespace_uid: "uid-sentinel".into(),
        marker_uid: "uid-marker".into(),
        marker_resource_version: "17".into(),
        node_name: NODE.into(),
        node_uid: "uid-node".into(),
        node_provider_id: PROVIDER.into(),
        node_image_digest: DIGEST_B.into(),
    }
}

fn binding() -> CandidateBinding<'static> {
    CandidateBinding {
        domain_id: "production-domain",
        release_digest: DIGEST_A,
        cluster_binding: "production-cluster-binding",
        control_namespace: "production-control",
        workload_namespace: "production-workload",
    }
}

fn marker_record(target: &CandidateQualificationTarget) -> CandidateQualificationMarker {
    CandidateQualificationMarker {
        schema_version: CANDIDATE_QUALIFICATION_SCHEMA.into(),
        run_id: target.run_id.clone(),
        cluster_name: CLUSTER.into(),
        sentinel_namespace: CLUSTER.into(),
        domain_id: "production-domain".into(),
        release_digest: DIGEST_A.into(),
        cluster_binding: "production-cluster-binding".into(),
        control_namespace: "production-control".into(),
        workload_namespace: "production-workload".into(),
        kube_system_namespace_uid: target.kube_system_namespace_uid.clone(),
        control_namespace_uid: target.control_namespace_uid.clone(),
        workload_namespace_uid: target.workload_namespace_uid.clone(),
        sentinel_namespace_uid: target.sentinel_namespace_uid.clone(),
        node_name: target.node_name.clone(),
        node_uid: target.node_uid.clone(),
        node_provider_id: target.node_provider_id.clone(),
        node_image_digest: target.node_image_digest.clone(),
    }
}

fn namespace(name: &str, uid: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":name,"uid":uid,"resourceVersion":"1"}})
}

fn node(conditions: &Value) -> Value {
    json!({"apiVersion":"v1","kind":"Node","metadata":{"name":NODE,"uid":"uid-node","resourceVersion":"1"},
        "spec":{"providerID":PROVIDER},"status":{"conditions":conditions}})
}

fn marker(target: &CandidateQualificationTarget) -> TestResult<Value> {
    Ok(
        json!({"apiVersion":"v1","kind":"ConfigMap","immutable":true,
        "metadata":{"name":CANDIDATE_QUALIFICATION_MARKER_NAME,"namespace":CLUSTER,"uid":"uid-marker","resourceVersion":"17"},
        "data":{"state.json":serde_json::to_string(&marker_record(target))?}}),
    )
}

fn objects(target: &CandidateQualificationTarget) -> TestResult<BTreeMap<String, Value>> {
    Ok(BTreeMap::from([
        (
            format!("/api/v1/namespaces/{KUBE_SYSTEM_NAMESPACE}"),
            namespace(KUBE_SYSTEM_NAMESPACE, &target.kube_system_namespace_uid),
        ),
        (
            "/api/v1/namespaces/production-control".into(),
            namespace("production-control", &target.control_namespace_uid),
        ),
        (
            "/api/v1/namespaces/production-workload".into(),
            namespace("production-workload", &target.workload_namespace_uid),
        ),
        (
            format!("/api/v1/namespaces/{CLUSTER}"),
            namespace(CLUSTER, &target.sentinel_namespace_uid),
        ),
        (
            format!("/api/v1/nodes/{NODE}"),
            node(&json!([{"type":"Ready","status":"True"}])),
        ),
        (
            format!(
                "/api/v1/namespaces/{CLUSTER}/configmaps/{CANDIDATE_QUALIFICATION_MARKER_NAME}"
            ),
            marker(target)?,
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

fn platform(objects: BTreeMap<String, Value>) -> TestResult<(PlatformClient, Calls)> {
    let (client, calls) = wire(objects);
    let platform = PlatformClient::new(
        client,
        "production-control",
        "production-workload",
        "production-cluster-binding",
    )?;
    Ok((platform, calls))
}

async fn verify(
    target: &CandidateQualificationTarget,
    objects: BTreeMap<String, Value>,
) -> TestResult<(crate::Result<()>, Calls)> {
    let (platform, calls) = platform(objects)?;
    let continued = AtomicBool::new(false);
    let outcome = platform
        .after_candidate_qualification(&binding(), target, || async {
            continued.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await;
    assert_eq!(continued.load(Ordering::Relaxed), outcome.is_ok());
    Ok((outcome, calls))
}

fn assert_read_only(calls: &Calls) -> TestResult {
    let calls = calls
        .lock()
        .map_err(|_| std::io::Error::other("calls lock poisoned"))?;
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|(method, _)| method == "GET"));
    Ok(())
}

#[tokio::test]
async fn exact_arbitrary_candidate_scope_reaches_the_reservation_boundary() -> TestResult {
    let target = target();
    let (platform, calls) = platform(objects(&target)?)?;
    let continued = AtomicBool::new(false);
    let outcome: crate::Result<()> = platform
        .after_candidate_qualification(&binding(), &target, || async {
            continued.store(true, Ordering::Relaxed);
            Err(invalid("reservation boundary sentinel"))
        })
        .await;
    assert!(outcome.is_err());
    assert!(continued.load(Ordering::Relaxed));
    assert_read_only(&calls)?;
    let paths = calls
        .lock()
        .map_err(|_| std::io::Error::other("calls lock poisoned"))?
        .iter()
        .map(|(_, path)| path.clone())
        .collect::<Vec<_>>();
    assert_eq!(paths.len(), 6);
    assert!(paths.contains(&format!("/api/v1/namespaces/{CLUSTER}")));
    assert!(paths.contains(&format!("/api/v1/nodes/{NODE}")));
    Ok(())
}

#[tokio::test]
async fn target_rejection_never_enters_the_reservation_continuation() -> TestResult {
    let target = target();
    let mut values = objects(&target)?;
    let path =
        format!("/api/v1/namespaces/{CLUSTER}/configmaps/{CANDIDATE_QUALIFICATION_MARKER_NAME}");
    values.get_mut(&path).ok_or("marker fixture missing")?["immutable"] = json!(false);
    let (platform, calls) = platform(values)?;
    let continued = AtomicBool::new(false);
    let outcome = platform
        .after_candidate_qualification(&binding(), &target, || async {
            continued.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await;
    assert!(outcome.is_err());
    assert!(!continued.load(Ordering::Relaxed));
    assert_read_only(&calls)?;
    Ok(())
}

#[tokio::test]
async fn namespace_replacement_or_deletion_stops_before_any_write() -> TestResult {
    for path in [
        "/api/v1/namespaces/kube-system",
        "/api/v1/namespaces/production-control",
        "/api/v1/namespaces/production-workload",
        "/api/v1/namespaces/apsides-qual-0123456789abcdef0123456789abcdef",
    ] {
        for change in ["replacement", "deleting"] {
            let target = target();
            let mut values = objects(&target)?;
            let object = values.get_mut(path).ok_or("namespace fixture missing")?;
            if change == "replacement" {
                object["metadata"]["uid"] = json!("replacement-uid");
            } else {
                object["metadata"]["deletionTimestamp"] = json!("2026-09-06T00:00:00Z");
            }
            let (outcome, calls) = verify(&target, values).await?;
            assert!(outcome.is_err(), "{path} {change} was accepted");
            assert_read_only(&calls)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn marker_identity_immutability_and_strict_content_are_required() -> TestResult {
    for change in [
        "uid",
        "revision",
        "deleting",
        "mutable",
        "schema",
        "binding",
        "control",
        "workload",
        "namespace-uid",
        "extra",
    ] {
        let target = target();
        let mut values = objects(&target)?;
        let path = format!(
            "/api/v1/namespaces/{CLUSTER}/configmaps/{CANDIDATE_QUALIFICATION_MARKER_NAME}"
        );
        let object = values.get_mut(&path).ok_or("marker fixture missing")?;
        match change {
            "uid" => object["metadata"]["uid"] = json!("replacement-marker"),
            "revision" => object["metadata"]["resourceVersion"] = json!("18"),
            "deleting" => object["metadata"]["deletionTimestamp"] = json!("2026-09-06T00:00:00Z"),
            "mutable" => object["immutable"] = json!(false),
            "schema" | "binding" | "control" | "workload" | "namespace-uid" | "extra" => {
                let encoded = object["data"]["state.json"]
                    .as_str()
                    .ok_or("marker state missing")?;
                let mut state: Value = serde_json::from_str(encoded)?;
                match change {
                    "schema" => state["schemaVersion"] = json!("other-schema"),
                    "binding" => state["clusterBinding"] = json!("other-binding"),
                    "control" => state["controlNamespace"] = json!("other-control"),
                    "workload" => state["workloadNamespace"] = json!("other-workload"),
                    "namespace-uid" => state["sentinelNamespaceUid"] = json!("other-sentinel-uid"),
                    "extra" => state["unrecognized"] = json!(true),
                    _ => return Err("unknown marker content mutation".into()),
                }
                object["data"]["state.json"] = json!(serde_json::to_string(&state)?);
            }
            _ => return Err("unknown marker mutation".into()),
        }
        let (outcome, calls) = verify(&target, values).await?;
        assert!(outcome.is_err(), "marker {change} was accepted");
        assert_read_only(&calls)?;
    }
    Ok(())
}

#[tokio::test]
async fn exact_kind_node_identity_provider_and_single_ready_condition_are_required() -> TestResult {
    for change in [
        "uid",
        "provider",
        "not-ready",
        "missing-ready",
        "malformed-ready",
        "multiple-ready",
        "deleting",
    ] {
        let target = target();
        let mut values = objects(&target)?;
        let path = format!("/api/v1/nodes/{NODE}");
        let object = values.get_mut(&path).ok_or("node fixture missing")?;
        match change {
            "uid" => object["metadata"]["uid"] = json!("replacement-node"),
            "provider" => object["spec"]["providerID"] = json!("kind://docker/other/other"),
            "not-ready" => {
                object["status"]["conditions"] = json!([{"type":"Ready","status":"False"}]);
            }
            "missing-ready" => {
                object["status"]["conditions"] =
                    json!([{"type":"MemoryPressure","status":"False"}]);
            }
            "malformed-ready" => {
                object["status"]["conditions"] = json!([{"type":"Ready","status":"true"}]);
            }
            "multiple-ready" => {
                object["status"]["conditions"] =
                    json!([{"type":"Ready","status":"True"},{"type":"Ready","status":"True"}]);
            }
            "deleting" => object["metadata"]["deletionTimestamp"] = json!("2026-09-06T00:00:00Z"),
            _ => return Err("unknown node mutation".into()),
        }
        let (outcome, calls) = verify(&target, values).await?;
        assert!(outcome.is_err(), "node {change} was accepted");
        assert_read_only(&calls)?;
    }
    Ok(())
}

#[tokio::test]
async fn malformed_target_or_marker_binding_is_rejected_without_api_mutation() -> TestResult {
    for change in [
        "run",
        "node-name",
        "provider",
        "image",
        "marker-release",
        "marker-domain",
    ] {
        let mut target = target();
        let mut values = objects(&target)?;
        match change {
            "run" => target.run_id = "ABCDEF0123456789ABCDEF0123456789".into(),
            "node-name" => target.node_name = "other-control-plane".into(),
            "provider" => target.node_provider_id = "kind://docker/other/other".into(),
            "image" => target.node_image_digest = "sha256:short".into(),
            "marker-release" | "marker-domain" => {
                let path = format!(
                    "/api/v1/namespaces/{CLUSTER}/configmaps/{CANDIDATE_QUALIFICATION_MARKER_NAME}"
                );
                let object = values.get_mut(&path).ok_or("marker fixture missing")?;
                let encoded = object["data"]["state.json"]
                    .as_str()
                    .ok_or("marker state missing")?;
                let mut state: Value = serde_json::from_str(encoded)?;
                if change == "marker-release" {
                    state["releaseDigest"] = json!(DIGEST_B);
                } else {
                    state["domainId"] = json!("other-domain");
                }
                object["data"]["state.json"] = json!(serde_json::to_string(&state)?);
            }
            _ => return Err("unknown binding mutation".into()),
        }
        let (outcome, calls) = verify(&target, values).await?;
        assert!(outcome.is_err(), "target {change} was accepted");
        if !calls
            .lock()
            .map_err(|_| std::io::Error::other("calls lock poisoned"))?
            .is_empty()
        {
            assert_read_only(&calls)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn every_marker_binding_field_is_required_and_exact() -> TestResult {
    let target = target();
    let original = serde_json::to_value(marker_record(&target))?;
    let fields = original.as_object().ok_or("marker is not an object")?;
    let path =
        format!("/api/v1/namespaces/{CLUSTER}/configmaps/{CANDIDATE_QUALIFICATION_MARKER_NAME}");
    for field in fields.keys() {
        for remove in [false, true] {
            let mut state = original.clone();
            let state_object = state.as_object_mut().ok_or("marker is not an object")?;
            if remove {
                state_object.remove(field);
            } else {
                state_object.insert(field.clone(), json!("different"));
            }
            let mut values = objects(&target)?;
            values.get_mut(&path).ok_or("marker missing")?["data"]["state.json"] =
                json!(serde_json::to_string(&state)?);
            let (outcome, calls) = verify(&target, values).await?;
            assert!(
                outcome.is_err(),
                "marker field {field}, removed={remove} was accepted"
            );
            assert_read_only(&calls)?;
        }
    }
    Ok(())
}

#[test]
fn version_checks_reject_replacement_and_restored_content() {
    let expected = Versioned {
        uid: "original".into(),
        resource_version: "1".into(),
        value: json!({"approved": true}),
    };
    assert!(same_version(&expected, &expected));
    let mut replaced = expected.clone();
    replaced.uid = "replacement".into();
    assert!(!same_version(&replaced, &expected));
    let mut restored = expected.clone();
    restored.resource_version = "3".into();
    assert!(!same_version(&restored, &expected));
    let mut altered = expected.clone();
    altered.value = json!({"approved": false});
    assert!(!same_version(&altered, &expected));
}
