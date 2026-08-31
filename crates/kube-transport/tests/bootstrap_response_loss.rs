#![cfg(feature = "test-support")]

use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use http::{Method, Request, Response, StatusCode};
use kube::{Client, client::Body};
use oxideloop_controller_api::{OxideLoopStateSpec, RootBinding, StateBody, bootstrap_id};
use oxideloop_controller_core::CommandV0;
use oxideloop_kube_transport::{
    CommandOutcomeV0, KubeTransportV0, RootAddressV0, TransportErrorV0,
};
use serde_json::{Value, json};
use tower::service_fn;

const ROOT_GET: &str =
    "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a";
const ROOT_STATUS: &str =
    "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a/status?";
const STATE_GET: &str =
    "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a";
const CONTROLLER_FIELD_MANAGER_QUERY: &str = "&fieldManager=oxideloop-controller";

fn response(value: impl serde::Serialize) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("response JSON"),
        ))
        .expect("response")
}
fn not_found() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(Body::from(
            br#"{"kind":"Status","apiVersion":"v1","reason":"NotFound","code":404}"#.to_vec(),
        ))
        .expect("404")
}
fn lost() -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(b"{".to_vec()))
        .expect("lost response")
}
fn root(metadata: bool, link: bool, rv: &str, divergent: bool) -> Value {
    let mut annotations = serde_json::Map::new();
    if metadata {
        annotations.insert("runtime.oxideloop.dev/root-uid".into(), json!("root-uid"));
        annotations.insert(
            "runtime.oxideloop.dev/transaction-id".into(),
            json!("b".repeat(64)),
        );
        annotations.insert(
            "runtime.oxideloop.dev/command-id".into(),
            json!("b".repeat(64)),
        );
    }
    let status = if link {
        json!({"state":{"rootUid":"root-uid","name":"state-a","uid":"state-uid"}})
    } else {
        json!({})
    };
    json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication",
        "metadata":{"name":"app-a","namespace":"tenant-a","uid":"root-uid","resourceVersion":rv,"generation":1,
          "finalizers":if metadata { json!(["demo.oxideloop.dev/finalizer"]) } else { json!([]) },"annotations":annotations},
        "spec":if divergent { json!({"changed":true}) } else { json!({"message":"hello","expose":false,"program":{"configMapName":"bundle","key":"bundle","digest":"b".repeat(64)}}) },"status":status})
}

fn apply_managed_fields() -> Value {
    json!([{
        "manager":"kubectl-client-side-apply",
        "operation":"Apply",
        "apiVersion":"demo.oxideloop.dev/v1alpha1",
        "time":"2026-08-31T00:00:00Z",
        "fieldsType":"FieldsV1",
        "fieldsV1":{"f:spec":{}},
    }])
}

fn state_apply_managed_fields() -> Value {
    json!([{
        "manager":"kubectl-client-side-apply",
        "operation":"Apply",
        "apiVersion":"runtime.oxideloop.dev/v1alpha1",
        "time":"2026-08-31T00:00:00Z",
        "fieldsType":"FieldsV1",
        "fieldsV1":{"f:spec":{}},
    }])
}

fn controller_metadata_update_managed_fields() -> Value {
    json!([{
        "manager":"oxideloop-controller",
        "operation":"Update",
        "apiVersion":"demo.oxideloop.dev/v1alpha1",
        "time":"2026-08-31T00:00:01Z",
        "fieldsType":"FieldsV1",
        "fieldsV1":{"f:metadata":{"f:annotations":{},"f:finalizers":{}}},
    }])
}

fn controller_state_finalizer_update_managed_fields() -> Value {
    json!([{
        "manager":"oxideloop-controller",
        "operation":"Update",
        "apiVersion":"runtime.oxideloop.dev/v1alpha1",
        "time":"2026-08-31T00:00:01Z",
        "fieldsType":"FieldsV1",
        "fieldsV1":{"f:metadata":{"f:finalizers":{}}},
    }])
}

fn controller_status_update_managed_fields() -> Value {
    json!([{
        "manager":"oxideloop-controller",
        "operation":"Update",
        "apiVersion":"demo.oxideloop.dev/v1alpha1",
        "subresource":"status",
        "time":"2026-08-31T00:00:02Z",
        "fieldsType":"FieldsV1",
        "fieldsV1":{"f:status":{"f:state":{}}},
    }])
}

fn root_with_apply_managed_fields(metadata: bool, link: bool, rv: &str) -> Value {
    let mut root = root(metadata, link, rv, false);
    root["metadata"]["managedFields"] = apply_managed_fields();
    root
}

fn root_with_controller_managed_fields(metadata: bool, link: bool, rv: &str) -> Value {
    let mut root = root(metadata, link, rv, false);
    root["metadata"]["managedFields"] = controller_metadata_update_managed_fields();
    root
}

fn root_with_controller_status_managed_fields(metadata: bool, link: bool, rv: &str) -> Value {
    let mut root = root(metadata, link, rv, false);
    root["metadata"]["managedFields"] = controller_status_update_managed_fields();
    root
}

fn root_with_foreign_metadata(mutation: &str) -> Value {
    let mut root = root(true, false, "2", false);
    match mutation {
        "annotation" => {
            root["metadata"]["annotations"]["foreign"] = json!("changed");
        }
        "finalizer" => {
            root["metadata"]["finalizers"] = json!(["demo.oxideloop.dev/finalizer", "foreign"]);
        }
        "label" => {
            root["metadata"]["labels"] = json!({"foreign":"changed"});
        }
        "deletion metadata" => {
            root["metadata"]["deletionTimestamp"] = json!("2026-08-31T00:00:00Z");
        }
        "owner" => {
            root["metadata"]["ownerReferences"] = json!([{
                "apiVersion":"v1", "kind":"ConfigMap", "name":"foreign",
                "uid":"foreign-uid",
            }]);
        }
        "spec" => {
            root["spec"]["message"] = json!("changed");
        }
        "status" => {
            root["status"]["observedGeneration"] = json!(1);
        }
        "identity" => {
            root["metadata"]["uid"] = json!("foreign-root-uid");
        }
        _ => unreachable!("foreign-metadata fixture is exhaustive"),
    }
    root
}

fn root_with_foreign_state_link_mutation(mutation: &str) -> Value {
    let mut root = root_with_controller_status_managed_fields(true, true, "3");
    match mutation {
        "annotation" => {
            root["metadata"]["annotations"]["foreign"] = json!("changed");
        }
        "finalizer" => {
            root["metadata"]["finalizers"] = json!(["demo.oxideloop.dev/finalizer", "foreign"]);
        }
        "label" => {
            root["metadata"]["labels"] = json!({"foreign":"changed"});
        }
        "deletion metadata" => {
            root["metadata"]["deletionTimestamp"] = json!("2026-08-31T00:00:00Z");
        }
        "owner" => {
            root["metadata"]["ownerReferences"] = json!([{
                "apiVersion":"v1", "kind":"ConfigMap", "name":"foreign",
                "uid":"foreign-uid",
            }]);
        }
        "spec" => {
            root["spec"]["message"] = json!("changed");
        }
        "status" => {
            root["status"]["observedGeneration"] = json!(1);
        }
        "identity" => {
            root["metadata"]["uid"] = json!("foreign-root-uid");
        }
        _ => unreachable!("foreign State-link fixture is exhaustive"),
    }
    root
}
fn state(finalizer: bool, rv: &str, divergent: bool) -> Value {
    let transaction_id = if divergent {
        "changed".to_owned()
    } else {
        "b".repeat(64)
    };
    let spec = OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: "root-uid".into(),
            bootstrap_id: bootstrap_id("root-uid").expect("bootstrap id"),
            bundle_digest: "bundle".into(),
        },
        state: if divergent {
            StateBody {
                committed_dependencies: vec![],
                ..StateBody::default()
            }
        } else {
            StateBody::default()
        },
    };
    json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"OxideLoopState",
      "metadata":{"name":"state-a","namespace":"tenant-a","uid":"state-uid","resourceVersion":rv,
        "finalizers":if finalizer { json!(["runtime.oxideloop.dev/state-finalizer"]) } else { json!([]) },
        "annotations":{"runtime.oxideloop.dev/root-uid":"root-uid","runtime.oxideloop.dev/transaction-id":transaction_id,"runtime.oxideloop.dev/command-id":"b".repeat(64)},
        "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},"spec":spec})
}

fn state_with_apply_managed_fields(finalizer: bool, rv: &str, divergent: bool) -> Value {
    let mut state = state(finalizer, rv, divergent);
    state["metadata"]["managedFields"] = state_apply_managed_fields();
    state
}

fn state_with_controller_finalizer_managed_fields(
    finalizer: bool,
    rv: &str,
    divergent: bool,
) -> Value {
    let mut state = state(finalizer, rv, divergent);
    state["metadata"]["managedFields"] = controller_state_finalizer_update_managed_fields();
    state
}

fn state_with_foreign_finalizer_mutation(mutation: &str) -> Value {
    let mut state = state_with_controller_finalizer_managed_fields(true, "3", false);
    match mutation {
        "annotation" => {
            state["metadata"]["annotations"]["foreign"] = json!("changed");
        }
        "finalizer" => {
            state["metadata"]["finalizers"] =
                json!(["runtime.oxideloop.dev/state-finalizer", "foreign"]);
        }
        "label" => {
            state["metadata"]["labels"] = json!({"foreign":"changed"});
        }
        "deletion metadata" => {
            state["metadata"]["deletionTimestamp"] = json!("2026-08-31T00:00:00Z");
        }
        "owner" => {
            state["metadata"]["ownerReferences"] = json!([{
                "apiVersion":"v1", "kind":"ConfigMap", "name":"foreign",
                "uid":"foreign-uid",
            }]);
        }
        "spec" => {
            state["spec"]["stateVersion"] = json!(2);
        }
        "status" => {
            state["status"] = json!({"observedGeneration":1});
        }
        "identity" => {
            state["metadata"]["uid"] = json!("foreign-state-uid");
        }
        _ => unreachable!("foreign State-finalizer fixture is exhaustive"),
    }
    state
}

fn root_status_patch_count(calls: &[(Method, String)]) -> usize {
    calls
        .iter()
        .filter(|(method, path)| *method == Method::PATCH && path.starts_with(ROOT_STATUS))
        .count()
}

fn state_finalizer_patch_count(calls: &[(Method, String)]) -> usize {
    calls
        .iter()
        .filter(|(method, path)| *method == Method::PATCH && path.starts_with(STATE_GET))
        .count()
}
fn command() -> CommandV0 {
    let state = OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: "root-uid".into(),
            bootstrap_id: bootstrap_id("root-uid").expect("bootstrap id"),
            bundle_digest: "bundle".into(),
        },
        state: StateBody::default(),
    };
    CommandV0::EnsureBootstrap {
        token: "b".repeat(64),
        state,
        root_uid: "root-uid".into(),
        root_generation: 1,
        root_resource_version: "1".into(),
        state_name: "state-a".into(),
    }
}
async fn run_async(
    responses: Vec<Response<Body>>,
) -> (
    Result<CommandOutcomeV0, TransportErrorV0>,
    Vec<(Method, String)>,
) {
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let next = Arc::clone(&responses);
    let seen = Arc::clone(&calls);
    let service = service_fn(move |request: Request<Body>| {
        let next = Arc::clone(&next);
        let seen = Arc::clone(&seen);
        async move {
            seen.lock().expect("calls").push((
                request.method().clone(),
                request.uri().path_and_query().expect("URI").to_string(),
            ));
            Ok::<_, Infallible>(
                next.lock()
                    .expect("responses")
                    .pop_front()
                    .expect("unexpected request"),
            )
        }
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let result = transport.dispatch(&address, &command()).await;
    (result, calls.lock().expect("calls").clone())
}

#[tokio::test]
async fn state_create_response_loss_holds_without_name_adoption_or_proof_get() {
    let (result, calls) = run_async(vec![
        response(root(false, false, "1", false)),
        not_found(),
        response(root(false, false, "1", false)),
        lost(),
    ])
    .await;
    assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
    assert_eq!(
        calls
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        vec![
            ROOT_GET,
            STATE_GET,
            ROOT_GET,
            "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates?"
        ]
    );
}

#[tokio::test]
async fn state_finalizer_direct_response_normalizes_controller_managed_fields() {
    let root = root_with_apply_managed_fields(true, true, "1");
    let before = state_with_apply_managed_fields(false, "2", false);
    let written = state_with_controller_finalizer_managed_fields(true, "3", false);
    let (result, calls) = run_async(vec![
        response(root.clone()),
        response(before),
        response(root.clone()),
        response(written.clone()),
        response(root.clone()),
        response(written),
        response(root),
    ])
    .await;

    assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
    assert_eq!(state_finalizer_patch_count(&calls), 1);
    assert!(calls.iter().any(|(method, path)| {
        *method == Method::PATCH
            && path.starts_with(STATE_GET)
            && path.ends_with(CONTROLLER_FIELD_MANAGER_QUERY)
    }));
    assert_eq!(root_status_patch_count(&calls), 0);
}

#[tokio::test]
async fn state_finalizer_recovery_normalizes_controller_managed_fields() {
    let root = root_with_apply_managed_fields(true, true, "1");
    let before = state_with_apply_managed_fields(false, "2", false);
    let proof = state_with_controller_finalizer_managed_fields(true, "3", false);
    let (result, calls) = run_async(vec![
        response(root.clone()),
        response(before),
        response(root.clone()),
        lost(),
        response(proof.clone()),
        response(root.clone()),
        response(proof),
        response(root),
    ])
    .await;

    assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
    assert_eq!(state_finalizer_patch_count(&calls), 1);
    assert!(calls.iter().any(|(method, path)| {
        *method == Method::PATCH
            && path.starts_with(STATE_GET)
            && path.ends_with(CONTROLLER_FIELD_MANAGER_QUERY)
    }));
    assert_eq!(root_status_patch_count(&calls), 0);
}

#[tokio::test]
async fn state_finalizer_recovery_rejects_any_foreign_semantic_mutation() {
    for mutation in [
        "annotation",
        "finalizer",
        "label",
        "owner",
        "deletion metadata",
        "spec",
        "status",
        "identity",
    ] {
        let root = root_with_apply_managed_fields(true, true, "1");
        let (result, calls) = run_async(vec![
            response(root.clone()),
            response(state_with_apply_managed_fields(false, "2", false)),
            response(root),
            lost(),
            response(state_with_foreign_finalizer_mutation(mutation)),
        ])
        .await;

        assert_eq!(
            result,
            Err(TransportErrorV0::ManualRecoveryHold),
            "a foreign {mutation} must not become an owned State-finalizer response"
        );
        assert_eq!(state_finalizer_patch_count(&calls), 1);
        assert_eq!(root_status_patch_count(&calls), 0);
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| *method == Method::PATCH)
                .count(),
            1,
            "a rejected State-finalizer proof must make no later bootstrap write for {mutation}"
        );
    }
}

#[tokio::test]
async fn state_finalizer_loss_uses_one_exact_proof_and_holds_on_divergence() {
    for divergent in [false, true] {
        let (result, calls) = run_async(vec![
            response(root(true, true, "1", false)),
            response(state(false, "2", false)),
            response(root(true, true, "1", false)),
            lost(),
            response(state(true, "3", divergent)),
            response(root(true, true, "2", false)),
            response(state(true, "3", false)),
            response(root(true, true, "2", false)),
        ])
        .await;
        if divergent {
            assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
        } else {
            assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
        }
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| *method == Method::PATCH)
                .count(),
            1
        );
        assert_eq!(
            calls.iter().filter(|(_, path)| path == STATE_GET).count(),
            if divergent { 2 } else { 3 }
        );
    }
}

#[tokio::test]
async fn root_metadata_loss_uses_exact_proof_and_holds_on_divergence() {
    for divergent in [false, true] {
        let (result, calls) = run_async(vec![
            response(root(false, true, "1", false)),
            response(state(true, "2", false)),
            response(root(false, true, "1", false)),
            response(root(false, true, "1", false)),
            lost(),
            response(root(true, true, "2", divergent)),
            response(state(true, "2", false)),
            response(root(true, true, "2", false)),
        ])
        .await;
        if divergent {
            assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
        } else {
            assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
        }
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| *method == Method::PATCH)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn root_metadata_direct_response_normalizes_controller_managed_fields() {
    let (result, calls) = run_async(vec![
        response(root_with_apply_managed_fields(false, false, "1")),
        not_found(),
        response(root_with_apply_managed_fields(false, false, "1")),
        response(state(false, "2", false)),
        response(root_with_apply_managed_fields(false, false, "1")),
        response(state(true, "3", false)),
        response(root_with_apply_managed_fields(false, false, "1")),
        response(root_with_apply_managed_fields(false, false, "1")),
        response(root_with_controller_managed_fields(true, false, "2")),
        response(root_with_controller_managed_fields(true, false, "2")),
        response(root_with_controller_managed_fields(true, true, "3")),
        response(state(true, "3", false)),
        response(root_with_controller_managed_fields(true, true, "3")),
    ])
    .await;
    assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
    assert_eq!(
        root_status_patch_count(&calls),
        1,
        "the direct metadata response permits exactly one State-link PATCH"
    );
    assert!(
        calls.iter().any(|(method, path)| {
            *method == Method::PATCH
                && path.starts_with(ROOT_GET)
                && path.ends_with(CONTROLLER_FIELD_MANAGER_QUERY)
        }),
        "the JSON metadata patch names the controller field manager"
    );
}

#[tokio::test]
async fn root_metadata_recovery_normalizes_controller_managed_fields() {
    let (result, calls) = run_async(vec![
        response(root_with_apply_managed_fields(false, false, "1")),
        not_found(),
        response(root_with_apply_managed_fields(false, false, "1")),
        response(state(false, "2", false)),
        response(root_with_apply_managed_fields(false, false, "1")),
        response(state(true, "3", false)),
        response(root_with_apply_managed_fields(false, false, "1")),
        response(root_with_apply_managed_fields(false, false, "1")),
        lost(),
        response(root_with_controller_managed_fields(true, false, "2")),
        response(root_with_controller_managed_fields(true, false, "2")),
        response(root_with_controller_managed_fields(true, true, "3")),
        response(state(true, "3", false)),
        response(root_with_controller_managed_fields(true, true, "3")),
    ])
    .await;
    assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
    assert_eq!(
        root_status_patch_count(&calls),
        1,
        "the exact response-loss proof permits exactly one State-link PATCH"
    );
    assert!(
        calls.iter().any(|(method, path)| {
            *method == Method::PATCH
                && path.starts_with(ROOT_GET)
                && path.ends_with(CONTROLLER_FIELD_MANAGER_QUERY)
        }),
        "the ambiguous JSON metadata patch names the controller field manager"
    );
}

#[tokio::test]
async fn root_metadata_loss_rejects_any_foreign_metadata_before_state_link() {
    for mutation in [
        "annotation",
        "finalizer",
        "label",
        "owner",
        "deletion metadata",
        "spec",
        "status",
        "identity",
    ] {
        let foreign = root_with_foreign_metadata(mutation);
        let (result, calls) = run_async(vec![
            response(root(false, false, "1", false)),
            not_found(),
            response(root(false, false, "1", false)),
            response(state(false, "2", false)),
            response(root(false, false, "1", false)),
            response(state(true, "3", false)),
            response(root(false, false, "1", false)),
            response(root(false, false, "1", false)),
            lost(),
            response(foreign.clone()),
            // These replies make an accidental admission observable as a
            // later status PATCH, rather than hiding it behind fixture EOF.
            response(foreign),
            response(root(true, true, "3", false)),
            response(state(true, "2", false)),
            response(root(true, true, "3", false)),
        ])
        .await;
        assert_eq!(
            result,
            Err(TransportErrorV0::ManualRecoveryHold),
            "a foreign {mutation} must not become an owned metadata response"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| *method == Method::PATCH)
                .count(),
            2,
            "only State finalization and the ambiguous metadata PATCH may be polled for {mutation}"
        );
        assert_eq!(
            root_status_patch_count(&calls),
            0,
            "a foreign {mutation} must produce zero later State-link/status PATCHes"
        );
    }
}

#[tokio::test]
async fn root_state_link_direct_response_normalizes_controller_managed_fields() {
    let root_before = root_with_apply_managed_fields(false, false, "1");
    let state_before = state_with_apply_managed_fields(false, "2", false);
    let state_after = state_with_controller_finalizer_managed_fields(true, "3", false);
    let root_after_metadata = root_with_controller_managed_fields(true, false, "2");
    let root_after_link = root_with_controller_status_managed_fields(true, true, "3");
    let (result, calls) = run_async(vec![
        response(root_before.clone()),
        not_found(),
        response(root_before.clone()),
        response(state_before),
        response(root_before.clone()),
        response(state_after.clone()),
        response(root_before.clone()),
        response(root_before),
        response(root_after_metadata.clone()),
        response(root_after_metadata),
        response(root_after_link.clone()),
        response(state_after),
        response(root_after_link),
    ])
    .await;

    assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
    assert_eq!(root_status_patch_count(&calls), 1);
    assert!(calls.iter().any(|(method, path)| {
        *method == Method::PATCH
            && path.starts_with(ROOT_STATUS)
            && path.ends_with(CONTROLLER_FIELD_MANAGER_QUERY)
    }));
}

#[tokio::test]
async fn root_state_link_recovery_normalizes_controller_managed_fields() {
    let root_before = root_with_apply_managed_fields(false, false, "1");
    let state_before = state_with_apply_managed_fields(false, "2", false);
    let state_after = state_with_controller_finalizer_managed_fields(true, "3", false);
    let root_after_metadata = root_with_controller_managed_fields(true, false, "2");
    let root_after_link = root_with_controller_status_managed_fields(true, true, "3");
    let (result, calls) = run_async(vec![
        response(root_before.clone()),
        not_found(),
        response(root_before.clone()),
        response(state_before),
        response(root_before.clone()),
        response(state_after.clone()),
        response(root_before.clone()),
        response(root_before),
        response(root_after_metadata.clone()),
        response(root_after_metadata),
        lost(),
        response(root_after_link.clone()),
        response(state_after),
        response(root_after_link),
    ])
    .await;

    assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
    assert_eq!(root_status_patch_count(&calls), 1);
    assert!(calls.iter().any(|(method, path)| {
        *method == Method::PATCH
            && path.starts_with(ROOT_STATUS)
            && path.ends_with(CONTROLLER_FIELD_MANAGER_QUERY)
    }));
}

#[tokio::test]
async fn root_state_link_recovery_rejects_any_foreign_semantic_mutation() {
    for mutation in [
        "annotation",
        "finalizer",
        "label",
        "owner",
        "deletion metadata",
        "spec",
        "status",
        "identity",
    ] {
        let root_before = root_with_apply_managed_fields(false, false, "1");
        let state_before = state_with_apply_managed_fields(false, "2", false);
        let state_after = state_with_controller_finalizer_managed_fields(true, "3", false);
        let root_after_metadata = root_with_controller_managed_fields(true, false, "2");
        let (result, calls) = run_async(vec![
            response(root_before.clone()),
            not_found(),
            response(root_before.clone()),
            response(state_before),
            response(root_before.clone()),
            response(state_after),
            response(root_before.clone()),
            response(root_before),
            response(root_after_metadata.clone()),
            response(root_after_metadata),
            lost(),
            response(root_with_foreign_state_link_mutation(mutation)),
        ])
        .await;

        assert_eq!(
            result,
            Err(TransportErrorV0::ManualRecoveryHold),
            "a foreign {mutation} must not become an owned State-link response"
        );
        assert_eq!(root_status_patch_count(&calls), 1);
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| *method == Method::PATCH)
                .count(),
            3,
            "a rejected State-link proof must make no later bootstrap write for {mutation}"
        );
    }
}

#[tokio::test]
async fn root_link_loss_uses_exact_proof_and_holds_on_divergence() {
    for divergent in [false, true] {
        let (result, calls) = run_async(vec![
            response(root(false, false, "1", false)),
            not_found(),
            response(root(false, false, "1", false)),
            response(state(false, "2", false)),
            response(root(false, false, "1", false)),
            response(state(true, "3", false)),
            response(root(false, false, "1", false)),
            response(root(false, false, "1", false)),
            response(root(true, false, "2", false)),
            response(root(true, false, "2", false)),
            lost(),
            response(root(true, !divergent, "3", false)),
            response(state(true, "3", false)),
            response(root(true, true, "3", false)),
        ])
        .await;
        if divergent {
            assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
        } else {
            assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
        }
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| *method == Method::PATCH)
                .count(),
            3
        );
        assert_eq!(root_status_patch_count(&calls), 1);
    }
}
