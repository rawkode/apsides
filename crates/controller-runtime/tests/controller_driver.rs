//! Production-driver construction coverage.  The deeper blocked-command
//! watch ordering lives beside the private driver collector, where its sealed
//! records cannot be forged by an integration test.

use std::{
    convert::Infallible,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use http::{Request, Response};
use kube::{Client, client::Body};
use oxideloop_controller_runtime::{ControllerDriverV0, DriverConfigV0, RuntimeErrorV0};
use oxideloop_kube_transport::KubeTransportV0;
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
