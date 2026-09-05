#![cfg(feature = "test-support")]

use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use controller_api::{
    OxideLoopStateSpec, RootBinding, StateBody, bootstrap_id,
    state_name as deterministic_state_name,
};
use controller_core::CommandV0;
use http::{Method, Request, Response, StatusCode};
use kube::{Client, client::Body};
use kube_transport::{
    BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION, BOOTSTRAP_AUTHORITY_VERSION_V1,
    BOOTSTRAP_PROOF_TOKEN_ANNOTATION, BOOTSTRAP_STATE_NAME_ANNOTATION,
    BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION, BOOTSTRAP_STATE_UID_ANNOTATION, COMMAND_ID_ANNOTATION,
    CommandOutcomeV0, KubeTransportV0, ROOT_UID_ANNOTATION, RootAddressV0,
    TRANSACTION_ID_ANNOTATION, TransportErrorV0,
};
use renderer_abi::{canonical_bytes, sha256_hex};
use serde_json::{Value, json};
use tower::service_fn;

const ROOT_GET: &str =
    "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a";
const ROOT_STATUS: &str =
    "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a/status?";
const CONTROLLER_FIELD_MANAGER_QUERY: &str = "&fieldManager=oxideloop-controller";
const ROOT_UID: &str = "root-uid";
const STATE_UID: &str = "state-uid";
const PROOF_TOKEN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1: &[u8] = b"oxideloop.bootstrap-authority/v1\0";

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

fn state_name() -> String {
    let derived = deterministic_state_name(ROOT_UID).expect("fixture State name");
    assert_eq!(
        derived,
        deterministic_state_name(ROOT_UID).expect("fixture State name remains derivable"),
        "fixture State name must remain derived from ROOT_UID"
    );
    derived
}

fn state_get() -> String {
    format!(
        "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{}",
        state_name()
    )
}

fn bootstrap_state_spec(divergent: bool) -> Value {
    let state = OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: ROOT_UID.into(),
            bootstrap_id: bootstrap_id(ROOT_UID).expect("bootstrap id"),
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
    serde_json::to_value(state).expect("fixture State spec JSON")
}

fn bootstrap_spec_digest(spec: &Value) -> String {
    let canonical = canonical_bytes(spec).expect("fixture State spec canonicalizes");
    let mut bytes = Vec::with_capacity(BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1.len() + canonical.len());
    bytes.extend_from_slice(BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1);
    bytes.extend_from_slice(&canonical);
    sha256_hex(&bytes)
}

fn bootstrap_authority_annotations(
    root_uid: &str,
    state_name: &str,
    state_uid: &str,
    state_spec: &Value,
    proof_token: &str,
) -> serde_json::Map<String, Value> {
    let mut annotations = serde_json::Map::new();
    annotations.insert(ROOT_UID_ANNOTATION.into(), json!(root_uid));
    annotations.insert(TRANSACTION_ID_ANNOTATION.into(), json!(proof_token));
    annotations.insert(COMMAND_ID_ANNOTATION.into(), json!(proof_token));
    annotations.insert(
        BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION.into(),
        json!(BOOTSTRAP_AUTHORITY_VERSION_V1),
    );
    annotations.insert(BOOTSTRAP_STATE_NAME_ANNOTATION.into(), json!(state_name));
    annotations.insert(BOOTSTRAP_STATE_UID_ANNOTATION.into(), json!(state_uid));
    annotations.insert(
        BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION.into(),
        json!(bootstrap_spec_digest(state_spec)),
    );
    annotations.insert(BOOTSTRAP_PROOF_TOKEN_ANNOTATION.into(), json!(proof_token));
    annotations
}

fn root(metadata: bool, link: bool, rv: &str, divergent: bool) -> Value {
    let state_name = state_name();
    let annotations = metadata.then(|| {
        bootstrap_authority_annotations(
            ROOT_UID,
            &state_name,
            STATE_UID,
            &bootstrap_state_spec(false),
            PROOF_TOKEN,
        )
    });
    let status = if link {
        json!({"state":{"rootUid":ROOT_UID,"name":state_name,"uid":STATE_UID}})
    } else {
        json!({})
    };
    json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication",
        "metadata":{"name":"app-a","namespace":"tenant-a","uid":ROOT_UID,"resourceVersion":rv,"generation":1,
          "finalizers":if metadata { json!(["demo.oxideloop.dev/finalizer"]) } else { json!([]) },"annotations":annotations.unwrap_or_default()},
        "spec":if divergent { json!({"changed":true}) } else { json!({"image":"example.invalid/web:latest","replicas":1,"message":"hello","expose":false,"program":{"digest":"b".repeat(64)}}) },"status":status})
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
    let state_name = state_name();
    let transaction_id = if divergent {
        "changed".to_owned()
    } else {
        PROOF_TOKEN.to_owned()
    };
    let spec = bootstrap_state_spec(divergent);
    json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"OxideLoopState",
      "metadata":{"name":state_name,"namespace":"tenant-a","uid":STATE_UID,"resourceVersion":rv,
        "finalizers":if finalizer { json!(["runtime.oxideloop.dev/state-finalizer"]) } else { json!([]) },
        "annotations":{"runtime.oxideloop.dev/root-uid":ROOT_UID,"runtime.oxideloop.dev/transaction-id":transaction_id,"runtime.oxideloop.dev/command-id":PROOF_TOKEN},
        "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":ROOT_UID,"controller":true,"blockOwnerDeletion":false}]},"spec":spec})
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
    let state_get = state_get();
    calls
        .iter()
        .filter(|(method, path)| *method == Method::PATCH && path.starts_with(&state_get))
        .count()
}
fn command() -> CommandV0 {
    let state = OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: ROOT_UID.into(),
            bootstrap_id: bootstrap_id(ROOT_UID).expect("bootstrap id"),
            bundle_digest: "bundle".into(),
        },
        state: StateBody::default(),
    };
    CommandV0::EnsureBootstrap {
        token: PROOF_TOKEN.into(),
        state,
        root_uid: ROOT_UID.into(),
        root_generation: 1,
        root_resource_version: "1".into(),
        state_name: state_name(),
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
        state_name: state_name(),
    };
    let result = transport.dispatch(&address, &command()).await;
    (result, calls.lock().expect("calls").clone())
}

/// A complete direct `EnsureBootstrap` transaction under the R17 phase order:
/// State POST, root metadata bundle, State finalizer, then root `StateLink`.
/// The first three response parameters are the raw responses to those last
/// three writes, so each caller can vary exactly one response-proof surface.
fn fresh_bootstrap_success_responses(
    root_after_metadata: Value,
    state_after_finalizer: Value,
    root_after_link: Value,
) -> Vec<Response<Body>> {
    let root_before = root_with_apply_managed_fields(false, false, "1");
    let state_created = state_with_apply_managed_fields(false, "2", false);
    vec![
        response(root_before.clone()),
        not_found(),
        response(root_before.clone()),
        response(state_created),
        response(root_before.clone()),
        response(root_before),
        response(root_after_metadata.clone()),
        response(root_after_metadata.clone()),
        response(state_after_finalizer.clone()),
        response(root_after_metadata),
        response(root_after_link.clone()),
        response(state_after_finalizer),
        response(root_after_link),
    ]
}

fn fresh_bootstrap_through_state_finalizer_preflight(
    root_after_metadata: Value,
) -> Vec<Response<Body>> {
    let root_before = root_with_apply_managed_fields(false, false, "1");
    let state_created = state_with_apply_managed_fields(false, "2", false);
    vec![
        response(root_before.clone()),
        not_found(),
        response(root_before.clone()),
        response(state_created),
        response(root_before.clone()),
        response(root_before),
        response(root_after_metadata.clone()),
        response(root_after_metadata),
    ]
}

fn fresh_bootstrap_through_metadata_preflight() -> Vec<Response<Body>> {
    let root_before = root_with_apply_managed_fields(false, false, "1");
    let state_created = state_with_apply_managed_fields(false, "2", false);
    vec![
        response(root_before.clone()),
        not_found(),
        response(root_before.clone()),
        response(state_created),
        response(root_before.clone()),
        response(root_before),
    ]
}

fn fresh_bootstrap_through_root_link_preflight(
    root_after_metadata: Value,
    state_after_finalizer: Value,
) -> Vec<Response<Body>> {
    let mut responses =
        fresh_bootstrap_through_state_finalizer_preflight(root_after_metadata.clone());
    responses.push(response(state_after_finalizer));
    responses.push(response(root_after_metadata));
    responses
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
    let state_get = state_get();
    assert_eq!(
        calls
            .iter()
            .map(|(_, path)| path.as_str())
            .collect::<Vec<_>>(),
        vec![
            ROOT_GET,
            state_get.as_str(),
            ROOT_GET,
            "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates?"
        ]
    );
}

#[tokio::test]
async fn state_finalizer_direct_response_normalizes_controller_managed_fields() {
    let root_after_metadata = root(true, false, "2", false);
    let state_after_finalizer = state_with_controller_finalizer_managed_fields(true, "3", false);
    let root_after_link = root(true, true, "3", false);
    let (result, calls) = run_async(fresh_bootstrap_success_responses(
        root_after_metadata,
        state_after_finalizer,
        root_after_link,
    ))
    .await;

    assert!(matches!(result, Ok(CommandOutcomeV0::Event(_))));
    assert_eq!(state_finalizer_patch_count(&calls), 1);
    let state_get = state_get();
    assert!(calls.iter().any(|(method, path)| {
        *method == Method::PATCH
            && path.starts_with(&state_get)
            && path.ends_with(CONTROLLER_FIELD_MANAGER_QUERY)
    }));
    assert_eq!(
        root_status_patch_count(&calls),
        1,
        "R17 links only after the State-finalizer response"
    );
}

#[tokio::test]
async fn state_finalizer_response_loss_holds_without_a_proof_get() {
    let root_after_metadata = root(true, false, "2", false);
    let mut responses = fresh_bootstrap_through_state_finalizer_preflight(root_after_metadata);
    responses.push(lost());
    let (result, calls) = run_async(responses).await;

    assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
    assert_eq!(state_finalizer_patch_count(&calls), 1);
    assert_eq!(root_status_patch_count(&calls), 0);
    let state_get = state_get();
    assert_eq!(
        calls.iter().filter(|(_, path)| path == &state_get).count(),
        1,
        "a direct bootstrap has no collected raw barrier to turn a lost State-finalizer response into a proof GET"
    );
}

#[tokio::test]
async fn state_finalizer_direct_response_rejects_any_foreign_semantic_mutation() {
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
        let root_after_metadata = root(true, false, "2", false);
        let mut responses = fresh_bootstrap_through_state_finalizer_preflight(root_after_metadata);
        responses.push(response(state_with_foreign_finalizer_mutation(mutation)));
        let (result, calls) = run_async(responses).await;

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
            2,
            "only RootMetadata and the rejected State-finalizer PATCH may be polled for {mutation}"
        );
    }
}

#[tokio::test]
async fn root_metadata_response_loss_holds_without_a_collected_raw_barrier() {
    let mut responses = fresh_bootstrap_through_metadata_preflight();
    responses.push(lost());
    let (result, calls) = run_async(responses).await;

    assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| *method == Method::PATCH)
            .count(),
        1,
        "the response-lost metadata write is not replayed by a direct dispatcher"
    );
    assert_eq!(state_finalizer_patch_count(&calls), 0);
    assert_eq!(root_status_patch_count(&calls), 0);
}

#[tokio::test]
async fn root_metadata_direct_response_normalizes_controller_managed_fields() {
    let root_after_metadata = root_with_controller_managed_fields(true, false, "2");
    let state_after_finalizer = state(true, "3", false);
    let root_after_link = root(true, true, "3", false);
    let (result, calls) = run_async(fresh_bootstrap_success_responses(
        root_after_metadata,
        state_after_finalizer,
        root_after_link,
    ))
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
async fn root_metadata_direct_response_rejects_any_foreign_semantic_mutation() {
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
        let mut responses = fresh_bootstrap_through_metadata_preflight();
        responses.push(response(foreign));
        let (result, calls) = run_async(responses).await;
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
            1,
            "a rejected metadata response must make no later bootstrap write for {mutation}"
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
    let root_after_metadata = root_with_controller_managed_fields(true, false, "2");
    let state_after = state_with_controller_finalizer_managed_fields(true, "3", false);
    let root_after_link = root_with_controller_status_managed_fields(true, true, "3");
    let (result, calls) = run_async(fresh_bootstrap_success_responses(
        root_after_metadata,
        state_after,
        root_after_link,
    ))
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
async fn root_state_link_response_loss_holds_without_replaying_the_final_write() {
    let root_after_metadata = root(true, false, "2", false);
    let state_after_finalizer = state(true, "3", false);
    let mut responses =
        fresh_bootstrap_through_root_link_preflight(root_after_metadata, state_after_finalizer);
    responses.push(lost());
    let (result, calls) = run_async(responses).await;

    assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
    assert_eq!(root_status_patch_count(&calls), 1);
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| *method == Method::PATCH)
            .count(),
        3,
        "the final State-link PATCH is polled once and never replayed"
    );
}

#[tokio::test]
async fn root_state_link_direct_response_rejects_any_foreign_semantic_mutation() {
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
        let root_after_metadata = root(true, false, "2", false);
        let state_after_finalizer = state(true, "3", false);
        let mut responses =
            fresh_bootstrap_through_root_link_preflight(root_after_metadata, state_after_finalizer);
        responses.push(response(root_with_foreign_state_link_mutation(mutation)));
        let (result, calls) = run_async(responses).await;

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
async fn partial_v1_authority_bundle_holds_before_any_bootstrap_write() {
    let mut partial = root(true, false, "1", false);
    partial["metadata"]["annotations"]
        .as_object_mut()
        .expect("fixture annotations")
        .remove(BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION);
    let (result, calls) =
        run_async(vec![response(partial), response(state(false, "2", false))]).await;

    assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
    assert!(
        calls
            .iter()
            .all(|(method, _)| *method != Method::POST && *method != Method::PATCH),
        "a partial R17 v1 bundle is never overwritten or resumed"
    );
}
