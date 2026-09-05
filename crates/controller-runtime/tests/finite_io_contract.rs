use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use controller_runtime::{BarrierV0, RuntimeErrorV0, establish_dependencies_async};
use http::{Request, Response};
use kube::{Client, client::Body};
use kube_transport::{FiniteIoStageV0, KubeTransportV0, TransportErrorV0};
use renderer_abi::model::Dependency;
use serde_json::json;
use tower::service_fn;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum HangAt {
    InitialList,
    WatchOpen,
    ClosingList,
    ExactGet,
}

fn transport(hang_at: HangAt) -> (KubeTransportV0, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let service = service_fn(move |request: Request<Body>| {
        let seen = Arc::clone(&seen);
        async move {
            let is_watch = request
                .uri()
                .query()
                .is_some_and(|query| query.contains("watch=true"));
            let is_collection = request.uri().path().ends_with("/configmaps");
            let ordinal = seen.fetch_add(1, Ordering::SeqCst);
            match hang_at {
                HangAt::InitialList if ordinal == 0 => {
                    std::future::pending::<Result<Response<Body>, Infallible>>().await
                }
                HangAt::WatchOpen if is_watch => {
                    std::future::pending::<Result<Response<Body>, Infallible>>().await
                }
                HangAt::ClosingList if ordinal == 2 => {
                    std::future::pending::<Result<Response<Body>, Infallible>>().await
                }
                HangAt::ExactGet if ordinal == 3 => {
                    std::future::pending::<Result<Response<Body>, Infallible>>().await
                }
                _ if is_watch => Ok::<_, Infallible>(
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::empty())
                        .expect("watch response"),
                ),
                _ if is_collection => Ok(list_response()),
                _ => Ok(not_found_response()),
            }
        }
    });
    (
        KubeTransportV0::test_from_client(Client::new(service, "default")),
        requests,
    )
}

fn list_response() -> Response<Body> {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "apiVersion": "v1",
                "kind": "ConfigMapList",
                "metadata": {"resourceVersion": "1"},
                "items": []
            }))
            .expect("list response JSON"),
        ))
        .expect("list response")
}

fn not_found_response() -> Response<Body> {
    Response::builder()
        .status(http::StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "kind": "Status", "apiVersion": "v1", "reason": "NotFound", "code": 404
            }))
            .expect("not found response JSON"),
        ))
        .expect("not found response")
}

fn object_dependency() -> Vec<Dependency> {
    vec![Dependency::Object {
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        namespace: "ns".into(),
        name: "object".into(),
        uid: None,
        resource_version: None,
        observation_token: "token".into(),
    }]
}

async fn assert_dependency_hang(hang_at: HangAt, expected_requests: usize) {
    let (transport, requests) = transport(hang_at);
    let result = tokio::time::timeout(
        TEST_TIMEOUT,
        establish_dependencies_async(
            &transport,
            &object_dependency(),
            BarrierV0 {
                generation: 1,
                cache_epoch: 1,
            },
        ),
    )
    .await
    .expect("finite dependency I/O must not exceed the operation budget");
    assert!(matches!(
        result,
        Err(RuntimeErrorV0::Discovery(
            TransportErrorV0::FiniteIoDeadline(_)
        ))
    ));
    assert_eq!(requests.load(Ordering::SeqCst), expected_requests);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_initial_list_deadline_stops_before_any_follow_up() {
    assert_dependency_hang(HangAt::InitialList, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_watch_open_deadline_stops_before_closing_list() {
    assert_dependency_hang(HangAt::WatchOpen, 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_closing_list_deadline_stops_before_exact_get() {
    assert_dependency_hang(HangAt::ClosingList, 3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_exact_get_deadline_stops_after_the_complete_barrier_prefix() {
    assert_dependency_hang(HangAt::ExactGet, 4).await;
}

#[test]
fn finite_stage_contract_retains_watch_stream_as_a_distinct_stage() {
    assert_ne!(FiniteIoStageV0::WatchOpen, FiniteIoStageV0::ReadOnly);
    assert_ne!(FiniteIoStageV0::WatchOpen, FiniteIoStageV0::RecoveryProof);
}
