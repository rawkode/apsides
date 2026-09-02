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

#[cfg(feature = "test-failpoints")]
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http::{Request, Response};
use kube::{Client, client::Body};
use oxideloop_controller_core::{CommandV0, EventV0};
use oxideloop_controller_runtime::{
    ControllerDriverV0, DriverConfigV0, ModeV0, RootActorV0, RuntimeErrorV0,
    load_machine_from_transport,
};
#[cfg(feature = "test-failpoints")]
use oxideloop_kube_transport::WatchHandleV0;
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
            "image":"example.invalid/web:latest",
            "replicas":1,
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

#[cfg(feature = "test-failpoints")]
fn deleting_linked_root(
    state_name: &str,
    resource_version: &str,
    cleanup_token: Option<&str>,
) -> serde_json::Value {
    let status = cleanup_token.map_or_else(
        || {
            json!({
                "state":{"rootUid":"root-uid","name":state_name,"uid":"state-uid"},
            })
        },
        |cleanup_token| {
            json!({
                "state":{"rootUid":"root-uid","name":state_name,"uid":"state-uid"},
                "deletionCleanupReceipt":{
                    "rootUid":"root-uid",
                    "deletionId":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    "deletionStartGeneration":3,
                    "stateName":state_name,
                    "stateUid":"state-uid",
                    "cleanupToken":cleanup_token,
                },
            })
        },
    );
    json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1",
        "kind":"WebApplication",
        "metadata":{
            "name":"root", "namespace":"ns", "uid":"root-uid",
            "resourceVersion":resource_version, "generation":3,
            "annotations":{"runtime.oxideloop.dev/root-uid":"root-uid"},
            "deletionTimestamp":"2026-08-30T00:00:00Z",
            "finalizers":["demo.oxideloop.dev/finalizer"],
        },
        "spec":{
            "image":"example.invalid/web:latest",
            "replicas":1,
            "message":"deletion barrier", "expose":false,
            "program":{"configMapName":"bundle","key":"program.mjs","digest":ROOT_ONLY_DIGEST},
        },
        "status":status,
    })
}

#[cfg(feature = "test-failpoints")]
fn deleting_linked_state(
    state_name: &str,
    resource_version: &str,
    finalizer_present: bool,
) -> serde_json::Value {
    json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1",
        "kind":"OxideLoopState",
        "metadata":{
            "name":state_name, "namespace":"ns", "uid":"state-uid",
            "resourceVersion":resource_version,
            "annotations":{"runtime.oxideloop.dev/root-uid":"root-uid"},
            "ownerReferences":[{
                "apiVersion":"demo.oxideloop.dev/v1alpha1",
                "kind":"WebApplication",
                "name":"root",
                "uid":"root-uid",
                "controller":true,
                "blockOwnerDeletion":false,
            }],
            "finalizers":if finalizer_present {
                vec!["runtime.oxideloop.dev/state-finalizer"]
            } else {
                Vec::<&str>::new()
            },
        },
        "spec":{
            "stateVersion":1,
            "root":{
                "uid":"root-uid",
                "bootstrapId":oxideloop_controller_api::bootstrap_id("root-uid")
                    .expect("bootstrap ID"),
                "bundleDigest":ROOT_ONLY_DIGEST,
            },
            "state":{
                "deletion":{
                    "id":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                    "deletionStartGeneration":3,
                },
                "automation":{},
            },
        },
        "status":{},
    })
}

#[cfg(feature = "test-failpoints")]
fn failpoint_socket_path() -> PathBuf {
    let parent = std::fs::canonicalize("/tmp")
        .expect("canonical temporary root")
        .join(format!(
            "ol-driver-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
    std::fs::create_dir(&parent).expect("private failpoint parent");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&parent)
            .expect("failpoint parent metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&parent, permissions).expect("private failpoint parent mode");
    }
    parent.join("control.sock")
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
                    | "/apis/apps/v1/namespaces/ns/deployments"
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

#[cfg(feature = "test-failpoints")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_accepts_a_failpoint_after_reaching_actor_absent_idle_wait() {
    let service = service_fn(move |request: Request<Body>| async move {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let response = if path.ends_with("/webapplications") && method == http::Method::GET {
            json_response(&json!({
                "apiVersion":"v1", "kind":"List",
                "metadata":{"resourceVersion":"list-rv"}, "items":[],
            }))
        } else if path.ends_with("/webapplications/root") && method == http::Method::GET {
            not_found()
        } else {
            panic!("unexpected idle failpoint request {method} {path}");
        };
        Ok::<_, Infallible>(response)
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
    let (_root_sender, root_watch) = WatchHandleV0::test_channel(1);
    transport.test_push_watch(root_watch);
    let socket = failpoint_socket_path();
    let store = store_path();
    let driver = ControllerDriverV0::new(
        DriverConfigV0 {
            namespace: "ns".into(),
            root_name: "root".into(),
            bundle_store: store.clone(),
            renderer_unix_socket: PathBuf::from("/tmp/renderer.sock"),
            failpoint_unix_socket: Some(socket.clone()),
        },
        transport,
    )
    .expect("driver construction");
    let run = tokio::spawn(driver.run());
    let socket_ready = tokio::time::timeout(Duration::from_secs(2), async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        socket_ready.is_ok(),
        "driver creates the failpoint listener"
    );
    // Startup polling is bounded to 100 ms. Connecting after this delay proves
    // the actor-absent idle wait itself wakes to accept the private control.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let acknowledgement = tokio::task::spawn_blocking({
        let socket = socket.clone();
        move || {
            let mut client = UnixStream::connect(socket).expect("connect after idle");
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set arm deadline");
            client
                .write_all(b"AfterEventBeforeStatus\n")
                .expect("send action arm");
            let mut armed = [0_u8; 6];
            client.read_exact(&mut armed).expect("read real ARMED");
            armed
        }
    })
    .await
    .expect("idle client joins");
    assert_eq!(&acknowledgement, b"ARMED\n");
    run.abort();
    let _ = run.await;
    let _ = std::fs::remove_dir_all(store);
    let _ = std::fs::remove_dir_all(socket.parent().expect("failpoint parent"));
}

#[cfg(feature = "test-failpoints")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn driver_deletion_barrier_crashes_after_exact_state_finalizer_effect_and_uds_release() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let state_finalizer_removed = Arc::new(AtomicBool::new(false));
    let state_resource_version = Arc::new(AtomicU64::new(1));
    let state_rereads_after_finalizer = Arc::new(AtomicU64::new(0));
    let persisted_state = Arc::new(Mutex::new(deleting_linked_state(
        &state_name,
        "state-rv-1",
        true,
    )));
    let root_status_persisted = Arc::new(AtomicBool::new(false));
    let cleanup_token = Arc::new(Mutex::new(ROOT_ONLY_DIGEST.to_owned()));
    let finalizer_effects = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let service_state_finalizer_removed = Arc::clone(&state_finalizer_removed);
    let service_state_resource_version = Arc::clone(&state_resource_version);
    let service_state_rereads_after_finalizer = Arc::clone(&state_rereads_after_finalizer);
    let service_persisted_state = Arc::clone(&persisted_state);
    let service_root_status_persisted = Arc::clone(&root_status_persisted);
    let service_cleanup_token = Arc::clone(&cleanup_token);
    let service_finalizer_effects = Arc::clone(&finalizer_effects);
    let service_requests = Arc::clone(&requests);
    let service_state_name = state_name.clone();
    let service = service_fn(move |request: Request<Body>| {
        let state_finalizer_removed = Arc::clone(&service_state_finalizer_removed);
        let state_resource_version = Arc::clone(&service_state_resource_version);
        let state_rereads_after_finalizer = Arc::clone(&service_state_rereads_after_finalizer);
        let persisted_state = Arc::clone(&service_persisted_state);
        let root_status_persisted = Arc::clone(&service_root_status_persisted);
        let cleanup_token = Arc::clone(&service_cleanup_token);
        let finalizer_effects = Arc::clone(&service_finalizer_effects);
        let requests = Arc::clone(&service_requests);
        let state_name = service_state_name.clone();
        async move {
            let (parts, body) = request.into_parts();
            let method = parts.method;
            let path = parts.uri.path().to_owned();
            let body = body.collect_bytes().await.expect("collect request body");
            requests
                .lock()
                .expect("request log lock")
                .push(format!("{method} {path}"));
            let response = if path.ends_with("/webapplications") && method == http::Method::GET {
                json_response(&json!({
                    "apiVersion":"v1", "kind":"List",
                    "metadata":{"resourceVersion":"list-rv"}, "items":[],
                }))
            } else if path.ends_with("/webapplications/root") && method == http::Method::GET {
                json_response(&deleting_linked_root(
                    &state_name,
                    if root_status_persisted.load(Ordering::SeqCst) {
                        "root-rv-2"
                    } else {
                        "root-rv-1"
                    },
                    Some(&cleanup_token.lock().expect("cleanup token lock")),
                ))
            } else if path.ends_with(&format!("/oxideloopstates/{state_name}"))
                && method == http::Method::GET
            {
                if !state_finalizer_removed.load(Ordering::SeqCst)
                    || state_rereads_after_finalizer.fetch_add(1, Ordering::SeqCst) == 0
                {
                    json_response(&persisted_state.lock().expect("persisted State lock"))
                } else {
                    not_found()
                }
            } else if matches!(
                path.as_str(),
                "/api/v1/namespaces/ns/configmaps"
                    | "/api/v1/namespaces/ns/services"
                    | "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/ns/automationruns"
            ) && method == http::Method::GET
            {
                json_response(&json!({
                    "apiVersion":"v1", "kind":"List",
                    "metadata":{"resourceVersion":"list-rv"}, "items":[],
                }))
            } else if path.ends_with("/webapplications/root/status")
                && method == http::Method::PATCH
            {
                let written: serde_json::Value =
                    serde_json::from_slice(&body).expect("root status patch JSON");
                let token = written
                    .pointer("/status/deletionCleanupReceipt/cleanupToken")
                    .and_then(serde_json::Value::as_str)
                    .expect("root status cleanup token");
                *cleanup_token.lock().expect("cleanup token lock") = token.to_owned();
                root_status_persisted.store(true, Ordering::SeqCst);
                json_response(&deleting_linked_root(&state_name, "root-rv-2", Some(token)))
            } else if path.ends_with(&format!("/oxideloopstates/{state_name}"))
                && method == http::Method::PATCH
            {
                let removing_finalizer = root_status_persisted.load(Ordering::SeqCst);
                if removing_finalizer {
                    state_finalizer_removed.store(true, Ordering::SeqCst);
                    finalizer_effects
                        .lock()
                        .expect("finalizer effect lock")
                        .push("state-finalizer");
                }
                let next = state_resource_version.fetch_add(1, Ordering::SeqCst) + 1;
                if removing_finalizer {
                    let mut written = persisted_state
                        .lock()
                        .expect("persisted State lock")
                        .clone();
                    let metadata = written
                        .get_mut("metadata")
                        .and_then(serde_json::Value::as_object_mut)
                        .expect("state metadata");
                    metadata.insert("resourceVersion".into(), json!(format!("state-rv-{next}")));
                    metadata.insert("finalizers".into(), json!(Vec::<String>::new()));
                    metadata.insert("managedFields".into(), json!([{"manager":"fixture"}]));
                    *persisted_state.lock().expect("persisted State lock") = written.clone();
                    json_response(&written)
                } else {
                    // A State persist response must echo the exact durable
                    // spec written by the command, not an older fixture.
                    let mut written: serde_json::Value =
                        serde_json::from_slice(&body).expect("state patch JSON");
                    let metadata = written
                        .get_mut("metadata")
                        .and_then(serde_json::Value::as_object_mut)
                        .expect("state patch metadata");
                    metadata.insert("name".into(), json!(state_name));
                    metadata.insert("namespace".into(), json!("ns"));
                    metadata.insert("uid".into(), json!("state-uid"));
                    metadata.insert("resourceVersion".into(), json!(format!("state-rv-{next}")));
                    let annotations = metadata
                        .get("annotations")
                        .cloned()
                        .expect("State persist includes operation annotations");
                    metadata.insert("annotations".into(), annotations);
                    metadata.insert(
                        "ownerReferences".into(),
                        json!([{
                            "apiVersion":"demo.oxideloop.dev/v1alpha1",
                            "kind":"WebApplication",
                            "name":"root",
                            "uid":"root-uid",
                            "controller":true,
                            "blockOwnerDeletion":false,
                        }]),
                    );
                    metadata.insert(
                        "finalizers".into(),
                        json!(["runtime.oxideloop.dev/state-finalizer"]),
                    );
                    *persisted_state.lock().expect("persisted State lock") = written.clone();
                    json_response(&written)
                }
            } else {
                panic!("unexpected deletion-barrier request {method} {path}");
            };
            Ok::<_, Infallible>(response)
        }
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
    let (root_sender, root_watch) = WatchHandleV0::test_channel(1);
    transport.test_push_watch(root_watch);
    let socket = failpoint_socket_path();
    let store = store_path();
    let driver = ControllerDriverV0::new(
        DriverConfigV0 {
            namespace: "ns".into(),
            root_name: "root".into(),
            bundle_store: store.clone(),
            renderer_unix_socket: PathBuf::from("/tmp/renderer.sock"),
            failpoint_unix_socket: Some(socket.clone()),
        },
        transport,
    )
    .expect("driver construction");
    let (client_ready, client_ready_wait) = mpsc::channel();
    let (client_send, client_receive) = mpsc::channel();
    let client_socket = socket.clone();
    std::thread::spawn(move || {
        client_ready.send(()).expect("client ready signal");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match UnixStream::connect(&client_socket) {
                Ok(mut client) => {
                    client
                        .write_all(b"AfterStateFinalizerBeforeRootFinalizer\n")
                        .expect("arm deletion barrier");
                    client_send.send(client).expect("return failpoint client");
                    return;
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("connect failpoint client: {error}"),
            }
        }
    });
    client_ready_wait
        .recv()
        .expect("client starts before driver");
    let mut run = tokio::spawn(driver.run());
    let client = tokio::task::spawn_blocking(move || {
        client_receive
            .recv_timeout(Duration::from_secs(2))
            .expect("failpoint client connects")
    })
    .await
    .expect("failpoint client task joins");
    let mut client = client;
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set midpoint deadline");
    let mut armed = [0_u8; 6];
    if let Err(error) = client.read_exact(&mut armed) {
        let driver_result = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
        panic!(
            "UDS reports arm acceptance: {error}; driver_finished={}; driver_result={driver_result:?}; trace={:?}",
            run.is_finished(),
            requests.lock().expect("request log lock"),
        );
    }
    assert_eq!(&armed, b"ARMED\n");
    let mut reached = [0_u8; 8];
    if let Err(error) = client.read_exact(&mut reached) {
        let driver_result = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
        panic!(
            "UDS reports exact midpoint: {error}; driver_finished={}; driver_result={driver_result:?}; trace={:?}",
            run.is_finished(),
            requests.lock().expect("request log lock"),
        );
    }
    assert_eq!(&reached, b"REACHED\n");

    assert!(state_finalizer_removed.load(Ordering::SeqCst));
    let persisted_state_at_midpoint = persisted_state
        .lock()
        .expect("persisted State lock")
        .clone();
    assert!(
        persisted_state_at_midpoint
            .pointer("/spec/state/pendingPersist/token")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|token| token != "bootstrap-token"),
        "the linked fixture preserves the current acknowledged State operation proof"
    );
    let trace_at_midpoint = requests.lock().expect("request log lock").clone();
    assert_eq!(
        finalizer_effects
            .lock()
            .expect("finalizer effect lock")
            .as_slice(),
        ["state-finalizer"],
        "the accepted barrier exposes only the exact state-finalizer effect",
    );
    assert!(
        !trace_at_midpoint.iter().any(|request| request.as_str()
            == "PATCH /apis/demo.oxideloop.dev/v1alpha1/namespaces/ns/webapplications/root"),
        "the active actor has not acknowledged the state effect or gained root-finalizer authority",
    );

    client.write_all(b"RELEASE\n").expect("release midpoint");
    let result = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("driver exits after release")
        .expect("driver task joins");
    assert!(matches!(
        result,
        Err(RuntimeErrorV0::InjectedCrash(
            oxideloop_controller_runtime::FailpointV0::AfterStateFinalizerBeforeRootFinalizer
        ))
    ));
    assert_eq!(
        requests.lock().expect("request log lock").as_slice(),
        trace_at_midpoint.as_slice(),
        "release crashes before any root-finalizer effect is dispatched",
    );
    let _ = std::fs::remove_dir_all(store);
    let _ = std::fs::remove_dir_all(socket.parent().expect("failpoint parent"));
    drop(root_sender);
}
