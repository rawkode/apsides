#![cfg(feature = "test-support")]

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};

use http::{Method, Request, Response};
use kube::{Client, client::Body};
use oxideloop_controller_api::{
    DurablePhase, OxideLoopStateSpec, PersistOperation, PersistPurpose, RootBinding, StateBody,
    bootstrap_id,
};
use oxideloop_controller_core::{CommandV0, PreconditionsV0};
use oxideloop_kube_transport::{KubeTransportV0, RootAddressV0, TransportErrorV0};
use serde_json::{Value, json};
use tower::service_fn;

fn response(value: impl serde::Serialize) -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("response JSON"),
        ))
        .expect("response")
}

fn root() -> Value {
    json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication",
        "metadata":{"name":"app-a","namespace":"tenant-a","uid":"root-uid","resourceVersion":"9","generation":1,
          "finalizers":["demo.oxideloop.dev/finalizer"], "annotations":{"runtime.oxideloop.dev/root-uid":"root-uid"}},
        "status":{"state":{"rootUid":"root-uid","name":"state-a","uid":"state-uid"}}
    })
}

fn state() -> Value {
    let spec = OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: "root-uid".into(),
            bootstrap_id: bootstrap_id("root-uid").expect("bootstrap id"),
            bundle_digest: "bundle".into(),
        },
        state: StateBody::default(),
    };
    json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1", "kind":"OxideLoopState",
        "metadata":{"name":"state-a","namespace":"tenant-a","uid":"state-uid","resourceVersion":"4",
            "finalizers":["runtime.oxideloop.dev/state-finalizer"], "annotations":{"runtime.oxideloop.dev/root-uid":"root-uid","runtime.oxideloop.dev/transaction-id":"a".repeat(64),"runtime.oxideloop.dev/command-id":"a".repeat(64)},
          "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},
        "spec":spec
    })
}

fn persisted_state() -> Value {
    let mut value = state();
    value["metadata"]["resourceVersion"] = json!("5");
    value["metadata"]["annotations"]["runtime.oxideloop.dev/transaction-id"] =
        json!("a".repeat(64));
    value["metadata"]["annotations"]["runtime.oxideloop.dev/command-id"] = json!("a".repeat(64));
    value
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
    CommandV0::Persist {
        state,
        preconditions: PreconditionsV0 {
            root_uid: "root-uid".into(),
            root_generation: 1,
            root_resource_version: "9".into(),
            state_uid: "state-uid".into(),
            state_resource_version: "4".into(),
            observation_token: "observation".into(),
            cache_epoch: "epoch".into(),
        },
        operation: PersistOperation {
            token: "a".repeat(64),
            purpose: PersistPurpose::Prepare,
            phase: DurablePhase::Prepare,
            old_state_resource_version: "4".into(),
            root_generation: 1,
        },
    }
}

#[tokio::test]
async fn malformed_preflight_retries_frozen_request_before_any_write() {
    let calls = Arc::new(Mutex::new(Vec::<(Method, String)>::new()));
    let seen = Arc::clone(&calls);
    let service = service_fn(move |request: Request<Body>| {
        let seen = Arc::clone(&seen);
        async move {
            let method = request.method().clone();
            let path = request.uri().path_and_query().expect("URI").to_string();
            let mut calls = seen.lock().expect("calls");
            let ordinal = calls.len();
            calls.push((method.clone(), path.clone()));
            drop(calls);
            if ordinal == 0 {
                return Ok::<_, Infallible>(
                    Response::builder()
                        .status(200)
                        .body(Body::from(b"{".to_vec()))
                        .expect("malformed response"),
                );
            }
            if method == Method::GET && path.contains("webapplications/app-a") {
                return Ok(response(root()));
            }
            if method == Method::GET && path.contains("oxideloopstates/state-a") {
                return Ok(response(state()));
            }
            assert_eq!(
                method,
                Method::PATCH,
                "the first write follows the successful retry"
            );
            Ok(response(persisted_state()))
        }
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let result = transport.dispatch(&address, &command()).await;
    assert!(matches!(
        result,
        Ok(oxideloop_kube_transport::CommandOutcomeV0::Event(_))
    ));
    let calls = calls.lock().expect("calls");
    assert_eq!(
        calls[0],
        (
            Method::GET,
            "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a".into()
        )
    );
    assert_eq!(
        calls[1], calls[0],
        "preflight retry reuses the exact frozen request"
    );
    assert!(calls[..2].iter().all(|(method, _)| *method == Method::GET));
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| *method == Method::PATCH)
            .count(),
        1,
        "no retry occurs after the write"
    );
    assert_eq!(calls.len(), 4);
}

#[tokio::test]
async fn preflight_transport_failure_has_no_write_or_outer_retry() {
    let calls = Arc::new(Mutex::new(0usize));
    let seen = Arc::clone(&calls);
    let service = service_fn(move |_request: Request<Body>| {
        let seen = Arc::clone(&seen);
        async move {
            *seen.lock().expect("calls") += 1;
            Ok::<_, Infallible>(
                Response::builder()
                    .status(409)
                    .body(Body::from(
                        br#"{"kind":"Status","apiVersion":"v1","code":409}"#.to_vec(),
                    ))
                    .expect("failure"),
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
    assert!(matches!(result, Err(TransportErrorV0::Conflict(_))));
    assert_eq!(
        *calls.lock().expect("calls"),
        1,
        "a transport failure before authorization cannot write"
    );
}
