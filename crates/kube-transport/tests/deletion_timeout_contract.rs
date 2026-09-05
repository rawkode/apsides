#![cfg(feature = "test-support")]

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use controller_api::state_name as deterministic_state_name;
use controller_core::{CommandV0, PreconditionsV0, linked_state_finalizer_command_token};
use http::{Method, Request, Response};
use kube::{Client, client::Body};
use kube_transport::{
    COMMAND_ID_ANNOTATION, KubeTransportV0, ROOT_UID_ANNOTATION, RootAddressV0,
    TRANSACTION_ID_ANNOTATION, TransportErrorV0,
};
use serde_json::{Value, json};
use tower::service_fn;

const ROOT_UID: &str = "root-uid";
const STATE_UID: &str = "state-uid";
const DELETION_ID: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const PERSIST_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CLEANUP_TOKEN: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn response(value: impl serde::Serialize) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("response JSON"),
        ))
        .expect("response")
}
fn state_spec() -> Value {
    json!({
        "stateVersion":1,
        "root":{"uid":ROOT_UID,"bootstrapId":controller_api::bootstrap_id(ROOT_UID).expect("fixture bootstrap ID"),"bundleDigest":"0".repeat(64)},
        "state":{
            "pendingPersist":{"token":PERSIST_TOKEN,"purpose":"DeletionStart","phase":"DeletionStart","oldStateResourceVersion":"3","rootGeneration":1},
            "deletion":{"id":DELETION_ID,"deletionStartGeneration":1},
            "automation":{}
        }
    })
}

fn state_name() -> String {
    deterministic_state_name(ROOT_UID).expect("fixture State name")
}

fn root() -> Value {
    let state_name = state_name();
    json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"app-a","namespace":"tenant-a","uid":ROOT_UID,"resourceVersion":"9","generation":1,"deletionTimestamp":"2026-01-01T00:00:00Z","finalizers":["demo.oxideloop.dev/finalizer"],"annotations":{ROOT_UID_ANNOTATION:ROOT_UID}},"status":{"state":{"rootUid":ROOT_UID,"name":state_name,"uid":STATE_UID},"deletionCleanupReceipt":{"rootUid":ROOT_UID,"deletionId":DELETION_ID,"deletionStartGeneration":1,"stateName":state_name,"stateUid":STATE_UID,"cleanupToken":CLEANUP_TOKEN}}})
}

fn state() -> Value {
    let state_name = state_name();
    json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"OxideLoopState","metadata":{"name":state_name,"namespace":"tenant-a","uid":STATE_UID,"resourceVersion":"4","finalizers":["runtime.oxideloop.dev/state-finalizer"],"annotations":{ROOT_UID_ANNOTATION:ROOT_UID,TRANSACTION_ID_ANNOTATION:PERSIST_TOKEN,COMMAND_ID_ANNOTATION:PERSIST_TOKEN},"ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":ROOT_UID,"controller":true,"blockOwnerDeletion":false}]},"spec":state_spec(),"status":{}})
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
        token: linked_state_finalizer_command_token(
            ROOT_UID,
            1,
            "9",
            &state_name(),
            STATE_UID,
            "4",
            DELETION_ID,
            1,
            CLEANUP_TOKEN,
        ),
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
