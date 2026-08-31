#![cfg(feature = "test-support")]

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use http::{Method, Request, Response};
use kube::{Client, client::Body};
use oxideloop_controller_core::{CommandV0, PreconditionsV0};
use oxideloop_kube_transport::{KubeTransportV0, RootAddressV0, TransportErrorV0};
use serde_json::json;
use tower::service_fn;

fn response(value: impl serde::Serialize) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("response JSON"),
        ))
        .expect("response")
}
fn root() -> serde_json::Value {
    json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"app-a","namespace":"tenant-a","uid":"root-uid","resourceVersion":"9","generation":1,"finalizers":["demo.oxideloop.dev/finalizer"],"annotations":{"runtime.oxideloop.dev/root-uid":"root-uid"}},"status":{}})
}
fn state() -> serde_json::Value {
    json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"OxideLoopState","metadata":{"name":"state-a","namespace":"tenant-a","uid":"state-uid","resourceVersion":"4","finalizers":["runtime.oxideloop.dev/state-finalizer"],"annotations":{"runtime.oxideloop.dev/root-uid":"root-uid"},"ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},"spec":{}})
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
        state_name: "state-a".into(),
    };
    let command = CommandV0::RemoveStateFinalizer {
        token: "d".repeat(64),
        preconditions: PreconditionsV0 {
            root_uid: "root-uid".into(),
            root_generation: 1,
            root_resource_version: "9".into(),
            state_uid: "state-uid".into(),
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
