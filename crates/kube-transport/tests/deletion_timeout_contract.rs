#![cfg(feature = "test-support")]

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use http::{Method, Request, Response};
use kube::{Client, client::Body};
use oxideloop_controller_api::{
    OxideLoopStateSpec, RootBinding, StateBody, bootstrap_id,
    state_name as deterministic_state_name,
};
use oxideloop_controller_core::{CommandV0, PreconditionsV0};
use oxideloop_kube_transport::{
    BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION, BOOTSTRAP_AUTHORITY_VERSION_V1,
    BOOTSTRAP_PROOF_TOKEN_ANNOTATION, BOOTSTRAP_STATE_NAME_ANNOTATION,
    BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION, BOOTSTRAP_STATE_UID_ANNOTATION, COMMAND_ID_ANNOTATION,
    KubeTransportV0, ROOT_UID_ANNOTATION, RootAddressV0, TRANSACTION_ID_ANNOTATION,
    TransportErrorV0,
};
use oxideloop_renderer_abi::{canonical_bytes, sha256_hex};
use serde_json::{Value, json};
use tower::service_fn;

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
fn state_spec() -> Value {
    serde_json::to_value(OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: ROOT_UID.into(),
            bootstrap_id: bootstrap_id(ROOT_UID).expect("fixture bootstrap ID"),
            bundle_digest: "bundle".into(),
        },
        state: StateBody::default(),
    })
    .expect("fixture State spec JSON")
}

fn state_name() -> String {
    deterministic_state_name(ROOT_UID).expect("fixture State name")
}

fn state_spec_digest(spec: &Value) -> String {
    let canonical = canonical_bytes(spec).expect("fixture State spec canonicalizes");
    let mut bytes = Vec::with_capacity(BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1.len() + canonical.len());
    bytes.extend_from_slice(BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1);
    bytes.extend_from_slice(&canonical);
    sha256_hex(&bytes)
}

fn authority_annotations(state_name: &str, state_spec: &Value) -> serde_json::Map<String, Value> {
    let mut annotations = serde_json::Map::new();
    annotations.insert(ROOT_UID_ANNOTATION.into(), json!(ROOT_UID));
    annotations.insert(TRANSACTION_ID_ANNOTATION.into(), json!(PROOF_TOKEN));
    annotations.insert(COMMAND_ID_ANNOTATION.into(), json!(PROOF_TOKEN));
    annotations.insert(
        BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION.into(),
        json!(BOOTSTRAP_AUTHORITY_VERSION_V1),
    );
    annotations.insert(BOOTSTRAP_STATE_NAME_ANNOTATION.into(), json!(state_name));
    annotations.insert(BOOTSTRAP_STATE_UID_ANNOTATION.into(), json!(STATE_UID));
    annotations.insert(
        BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION.into(),
        json!(state_spec_digest(state_spec)),
    );
    annotations.insert(BOOTSTRAP_PROOF_TOKEN_ANNOTATION.into(), json!(PROOF_TOKEN));
    annotations
}

fn root() -> Value {
    let spec = state_spec();
    let state_name = state_name();
    json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"app-a","namespace":"tenant-a","uid":ROOT_UID,"resourceVersion":"9","generation":1,"finalizers":["demo.oxideloop.dev/finalizer"],"annotations":authority_annotations(&state_name, &spec)},"status":{}})
}

fn state() -> Value {
    let state_name = state_name();
    json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"OxideLoopState","metadata":{"name":state_name,"namespace":"tenant-a","uid":STATE_UID,"resourceVersion":"4","finalizers":["runtime.oxideloop.dev/state-finalizer"],"annotations":{ROOT_UID_ANNOTATION:ROOT_UID,TRANSACTION_ID_ANNOTATION:PROOF_TOKEN,COMMAND_ID_ANNOTATION:PROOF_TOKEN},"ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":ROOT_UID,"controller":true,"blockOwnerDeletion":false}]},"spec":state_spec()})
}

#[tokio::test]
async fn finalizer_response_loss_is_a_shared_budget_hold_without_replay() {
    let responses = Arc::new(Mutex::new(vec![response(state()), response(root())]));
    let calls = Arc::new(Mutex::new(Vec::<(Method, String)>::new()));
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
            if request.method() == Method::PATCH {
                std::future::pending::<Result<Response<Body>, Infallible>>().await
            } else {
                Ok::<_, Infallible>(
                    next.lock()
                        .expect("responses")
                        .pop()
                        .expect("unexpected request"),
                )
            }
        }
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: state_name(),
    };
    let command = CommandV0::RemoveStateFinalizer {
        token: "d".repeat(64),
        preconditions: PreconditionsV0 {
            root_uid: "root-uid".into(),
            root_generation: 1,
            root_resource_version: "9".into(),
            state_uid: STATE_UID.into(),
            state_resource_version: "4".into(),
            observation_token: "observation".into(),
            cache_epoch: "epoch".into(),
        },
    };
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        transport.dispatch(&address, &command),
    )
    .await
    .expect("finite finalizer response budget");
    assert_eq!(result, Err(TransportErrorV0::ManualRecoveryHold));
    let calls = calls.lock().expect("calls");
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| *method == Method::PATCH)
            .count(),
        1
    );
    assert_eq!(
        calls.len(),
        3,
        "an ambiguous finalizer write cannot be replayed or followed by a guessed delete"
    );
}
