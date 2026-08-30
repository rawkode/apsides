//! Production-driver construction coverage.  The deeper blocked-command
//! watch ordering lives beside the private driver collector, where its sealed
//! records cannot be forged by an integration test.

use std::{
    convert::Infallible,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use http::{Request, Response};
use kube::{Client, client::Body};
use oxideloop_controller_core::{CommandV0, EventV0};
use oxideloop_controller_runtime::{
    ControllerDriverV0, DriverConfigV0, ModeV0, RootActorV0, RuntimeErrorV0,
    load_machine_from_transport,
};
use oxideloop_kube_transport::{CommandOutcomeV0, KubeTransportV0, RootAddressV0};
use serde_json::json;
use tower::service_fn;

static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

fn store_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "oxideloop-controller-driver-{}-{}",
        std::process::id(),
        TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

fn scripted_transport() -> KubeTransportV0 {
    let service = service_fn(|_request: Request<Body>| async move {
        Ok::<_, Infallible>(
            Response::builder()
                .status(500)
                .body(Body::empty())
                .expect("test response"),
        )
    });
    KubeTransportV0::test_from_client(Client::new(service, "default"))
}

const ROOT_ONLY_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn json_response(value: &serde_json::Value) -> Response<Body> {
    Response::builder()
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(value).expect("test JSON serializes"),
        ))
        .expect("test JSON response")
}

fn not_found() -> Response<Body> {
    Response::builder()
        .status(404)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "apiVersion":"v1",
                "kind":"Status",
                "status":"Failure",
                "reason":"NotFound",
                "code":404,
            }))
            .expect("not-found JSON serializes"),
        ))
        .expect("not-found response")
}

fn receipt_bounded_root(
    state_name: &str,
    receipt_cleanup_token: &str,
    resource_version: &str,
    root_finalizer_present: bool,
) -> serde_json::Value {
    json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1",
        "kind":"WebApplication",
        "metadata":{
            "name":"root",
            "namespace":"ns",
            "uid":"root-uid",
            "resourceVersion":resource_version,
            "generation":3,
            "deletionTimestamp":"2026-08-30T00:00:00Z",
            "finalizers":if root_finalizer_present {
                vec!["demo.oxideloop.dev/finalizer"]
            } else {
                Vec::<&str>::new()
            },
        },
        "spec":{
            "message":"receipt-bounded deletion",
            "expose":false,
            "program":{
                "configMapName":"bundle",
                "key":"program.mjs",
                "digest":ROOT_ONLY_DIGEST,
            },
        },
        "status":{
            "deletionCleanupReceipt":{
                "rootUid":"root-uid",
                "deletionId":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                "deletionStartGeneration":3,
                "stateName":state_name,
                "stateUid":"deleted-state-uid",
                "cleanupToken":receipt_cleanup_token,
            },
        },
    })
}

async fn dispatch_event(
    transport: &KubeTransportV0,
    address: &RootAddressV0,
    command: &CommandV0,
) -> EventV0 {
    match transport
        .dispatch(address, command)
        .await
        .expect("root-only transport command")
    {
        CommandOutcomeV0::Event(event) => *event,
        outcome => panic!("root-only command must acknowledge: {outcome:?}"),
    }
}

#[tokio::test]
async fn driver_accepts_only_explicit_production_configuration() {
    let store = store_path();
    let config = DriverConfigV0 {
        namespace: "ns".into(),
        root_name: "root".into(),
        bundle_store: store.clone(),
        renderer_unix_socket: PathBuf::from("/tmp/renderer.sock"),
        #[cfg(feature = "test-failpoints")]
        failpoint_unix_socket: None,
    };
    ControllerDriverV0::new(config, scripted_transport())
        .expect("driver receives a concrete in-cluster transport and explicit store/socket");
    std::fs::remove_dir_all(store).expect("test store cleanup");
}

#[tokio::test]
async fn driver_rejects_a_non_unix_renderer_endpoint() {
    let config = DriverConfigV0 {
        namespace: "ns".into(),
        root_name: "root".into(),
        bundle_store: store_path(),
        renderer_unix_socket: PathBuf::from("relative.sock"),
        #[cfg(feature = "test-failpoints")]
        failpoint_unix_socket: None,
    };
    assert!(matches!(
        ControllerDriverV0::new(config, scripted_transport()),
        Err(RuntimeErrorV0::Hold(_))
    ));
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the one test deliberately shows the complete loader-to-absence protocol"
)]
async fn root_only_deletion_composes_loader_reducer_transport_and_original_uid_absence() {
    let state_name =
        oxideloop_controller_api::state_name("root-uid").expect("deterministic deleted-state name");
    let receipt_cleanup_token = "e".repeat(64);
    let root_removed = Arc::new(AtomicBool::new(false));
    let service_root_removed = Arc::clone(&root_removed);
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let service_requests = Arc::clone(&requests);
    let service_state_name = state_name.clone();
    let service_receipt_cleanup_token = receipt_cleanup_token.clone();
    let service = service_fn(move |request: Request<Body>| {
        let root_removed = Arc::clone(&service_root_removed);
        let requests = Arc::clone(&service_requests);
        let state_name = service_state_name.clone();
        let receipt_cleanup_token = service_receipt_cleanup_token.clone();
        async move {
            let method = request.method().clone();
            let path = request.uri().path().to_owned();
            requests
                .lock()
                .expect("request log lock")
                .push(format!("{method} {path}"));
            let response = if path.ends_with("/webapplications/root") && method == http::Method::GET
            {
                if root_removed.load(Ordering::SeqCst) {
                    not_found()
                } else {
                    json_response(&receipt_bounded_root(
                        &state_name,
                        &receipt_cleanup_token,
                        "9",
                        true,
                    ))
                }
            } else if path.ends_with(&format!("/oxideloopstates/{state_name}"))
                && method == http::Method::GET
            {
                not_found()
            } else if matches!(
                path.as_str(),
                "/api/v1/namespaces/ns/configmaps"
                    | "/api/v1/namespaces/ns/services"
                    | "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/ns/automationruns"
            ) && method == http::Method::GET
            {
                json_response(&json!({
                    "apiVersion":"v1",
                    "kind":"List",
                    "metadata":{"resourceVersion":"list-rv"},
                    "items":[],
                }))
            } else if path.ends_with("/webapplications/root") && method == http::Method::PATCH {
                root_removed.store(true, Ordering::SeqCst);
                json_response(&receipt_bounded_root(
                    &state_name,
                    &receipt_cleanup_token,
                    "10",
                    false,
                ))
            } else {
                panic!("unexpected root-only composition request {method} {path}");
            };
            Ok::<_, Infallible>(response)
        }
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));

    // Loader derives the restricted machine only after a typed root GET and
    // an exact GET proving the linked state object is absent.
    let machine = load_machine_from_transport(&transport, "ns", "root")
        .await
        .expect("receipt-bounded loader")
        .expect("deleting root is present");
    assert!(machine.is_root_only_deletion_recovery());
    let address = RootAddressV0 {
        namespace: "ns".into(),
        name: "root".into(),
        state_name: machine.root().state_name.clone(),
    };
    let mut actor = RootActorV0::new(machine);
    actor.start();

    let verify = actor
        .next_command()
        .expect("root-only verify command")
        .expect("root-only verify command");
    let verify_token = match &verify {
        CommandV0::VerifyOwnedChildrenAbsent { token, .. } => token.clone(),
        command => panic!("unexpected root-only start command: {command:?}"),
    };
    assert_ne!(verify_token, receipt_cleanup_token);
    let verified = dispatch_event(&transport, &address, &verify).await;
    assert!(matches!(
        &verified,
        EventV0::OwnedChildrenVerifiedAbsent { token, cleanup_token, .. }
            if token == &verify_token && cleanup_token == &receipt_cleanup_token
    ));
    actor.acknowledge(verified).expect("verify acknowledgement");

    let remove = actor
        .next_command()
        .expect("root-only finalizer command")
        .expect("root-only finalizer command");
    let remove_token = match &remove {
        CommandV0::RemoveRootFinalizer { token, .. } => token.clone(),
        command => panic!("unexpected command after verification: {command:?}"),
    };
    assert_ne!(remove_token, receipt_cleanup_token);
    assert_ne!(remove_token, verify_token);
    actor
        .acknowledge(dispatch_event(&transport, &address, &remove).await)
        .expect("root finalizer acknowledgement");

    let absent = actor
        .next_command()
        .expect("original UID absence command")
        .expect("original UID absence command");
    let absence_token = match &absent {
        CommandV0::VerifyRootUidAbsent {
            token, root_uid, ..
        } => {
            assert_eq!(root_uid, "root-uid");
            token.clone()
        }
        command => panic!("unexpected command after finalizer removal: {command:?}"),
    };
    assert_ne!(absence_token, receipt_cleanup_token);
    actor
        .acknowledge(dispatch_event(&transport, &address, &absent).await)
        .expect("original UID absence acknowledgement");
    assert!(
        matches!(actor.next_command(), Ok(None)),
        "the terminal acknowledgement is reduced on the next actor turn"
    );
    assert!(matches!(actor.mode(), ModeV0::Terminal));

    let requests = requests.lock().expect("request log lock");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("PATCH "))
            .count(),
        1,
        "only the root finalizer write is issued"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.as_str()
                == "GET /apis/demo.oxideloop.dev/v1alpha1/namespaces/ns/webapplications/root")
            .count()
            >= 4,
        "loader, verification, finalizer, and original-UID absence each re-read the root"
    );
}
