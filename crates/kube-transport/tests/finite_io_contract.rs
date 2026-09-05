#![cfg(feature = "test-support")]

use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use controller_api::{
    AutomationPending, AutomationRunCompletion, AutomationRunCompletionState, AutomationRunSpec,
    AutomationRunStatus, ObjectIdentity, ProgramBundleReference, automation_run_name,
};
use http::{Request, Response, StatusCode};
use kube::{Client, client::Body};
use kube_transport::{
    AutomationRunObservationBarrierV0, KubeTransportV0, ObjectMetaV0, ObjectRefV0, ObjectV0,
    PrewriteGateStateV0, ResourceKindV0, TransportErrorV0, WatchEventV0, WatchHandleV0,
};
use renderer_abi::{canonical_bytes, parse_validate_action_input, sha256_hex};
use serde_json::json;
use tower::service_fn;

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
// Keep this test aligned with the production operation budget while retaining
// the watch read as an intentionally unbounded stream.
const FINITE_IO_BUDGET: Duration = Duration::from_secs(15);

#[tokio::test]
async fn authenticated_invalidation_and_write_poll_are_irreversible_public_controls() {
    let (transport, _) = transport(HangAt::InitialList);
    let invalidated = transport.begin_collected_operation_session();
    assert_eq!(
        invalidated.prewrite_gate().state(),
        PrewriteGateStateV0::PreWrite
    );
    assert!(
        invalidated
            .prewrite_gate()
            .invalidate_authenticated_change()
    );
    assert_eq!(
        invalidated.prewrite_gate().state(),
        PrewriteGateStateV0::Invalidated
    );
    assert_eq!(
        invalidated.prewrite_gate().try_mark_write_polled(),
        Err(PrewriteGateStateV0::Invalidated)
    );
    assert!(
        !invalidated
            .prewrite_gate()
            .invalidate_authenticated_change()
    );

    let write_polled = transport.begin_collected_operation_session();
    assert_eq!(write_polled.prewrite_gate().try_mark_write_polled(), Ok(()));
    assert_eq!(
        write_polled.prewrite_gate().state(),
        PrewriteGateStateV0::WritePolled
    );
    assert_eq!(
        write_polled.prewrite_gate().try_mark_write_polled(),
        Err(PrewriteGateStateV0::WritePolled)
    );
    assert!(
        !write_polled
            .prewrite_gate()
            .invalidate_authenticated_change()
    );
}

#[derive(Clone, Copy)]
enum HangAt {
    InitialList,
    WatchOpen,
    DirectGet,
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
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "kind": "Status", "apiVersion": "v1", "reason": "NotFound", "code": 404
            }))
            .expect("not found response JSON"),
        ))
        .expect("not found response")
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
            let _body = request.body();
            let ordinal = seen.fetch_add(1, Ordering::SeqCst);
            match hang_at {
                HangAt::InitialList | HangAt::DirectGet if ordinal == 0 => {
                    std::future::pending::<Result<Response<Body>, Infallible>>().await
                }
                HangAt::WatchOpen if is_watch => {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_list_deadline_is_typed_and_does_not_write() {
    let (transport, requests) = transport(HangAt::InitialList);
    let result = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.list_exact(ResourceKindV0::ConfigMap, "ns"),
    )
    .await
    .expect("finite list I/O must not exceed the operation budget");
    assert_eq!(
        result,
        Err(TransportErrorV0::FiniteIoDeadline(
            kube_transport::FiniteIoStageV0::ReadOnly
        ))
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_open_deadline_is_typed_and_does_not_start_a_stream() {
    let (transport, requests) = transport(HangAt::WatchOpen);
    let result = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.watch_from(ResourceKindV0::ConfigMap, "ns", "1"),
    )
    .await
    .expect("watch-open I/O must not exceed the operation budget");
    assert!(matches!(
        result,
        Err(TransportErrorV0::FiniteIoDeadline(
            kube_transport::FiniteIoStageV0::WatchOpen
        ))
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_get_deadline_is_typed_and_does_not_write() {
    let (transport, requests) = transport(HangAt::DirectGet);
    let reference = kube_transport::ObjectRefV0 {
        resource: ResourceKindV0::ConfigMap,
        namespace: "ns".into(),
        name: "object".into(),
    };
    let result = tokio::time::timeout(TEST_TIMEOUT, transport.reread(&reference))
        .await
        .expect("finite GET I/O must not exceed the operation budget");
    assert_eq!(
        result,
        Err(TransportErrorV0::FiniteIoDeadline(
            kube_transport::FiniteIoStageV0::ReadOnly
        ))
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundle_get_deadline_is_a_typed_finite_read_failure() {
    let (transport, requests) = transport(HangAt::DirectGet);
    let result = tokio::time::timeout(
        TEST_TIMEOUT,
        transport.read_bundle_binary_data("ns", "ns", "bundle", "program.mjs", 1024),
    )
    .await
    .expect("bundle GET must not exceed the operation budget");
    assert_eq!(
        result,
        Err(TransportErrorV0::FiniteIoDeadline(
            kube_transport::FiniteIoStageV0::ReadOnly
        ))
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn retained_watch_silence_survives_the_finite_read_budget_and_then_delivers_change() {
    let (sender, watch) = WatchHandleV0::test_channel(2);
    let validated = parse_validate_action_input(
        &json!({
            "abiVersion":"renderer.oxideloop.dev/v0",
            "bundleDigest":"a".repeat(64),
            "automationIdentity":"changed",
            "automationVersion":"v1",
            "actionModuleIdentity":"demo.action",
            "actionModuleDigest":"b".repeat(64),
            "occurrenceId":"occurrence-1",
            "sourceTransitionId":"transition-1",
            "sourceUid":"source-uid",
            "sourceResourceVersion":"7",
            "eventMetadata":{"note":"changed"}
        })
        .to_string(),
    )
    .expect("valid action input");
    let input = validated.document;
    let input_json =
        String::from_utf8(validated.canonical_bytes.clone()).expect("canonical action input JSON");
    let spec = AutomationRunSpec {
        occurrence_id: input.occurrence_id.clone(),
        root_uid: "root-uid".into(),
        source: ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "input".into(),
        },
        source_uid: input.source_uid.clone(),
        source_resource_version: input.source_resource_version.clone(),
        source_transition_id: input.source_transition_id.clone(),
        automation_id: input.automation_identity.clone(),
        automation_version: input.automation_version.clone(),
        bundle_digest: input.bundle_digest.clone(),
        program: ProgramBundleReference {
            digest: input.bundle_digest.clone(),
        },
        action_module_identity: input.action_module_identity.clone(),
        action_module_digest: input.action_module_digest.clone(),
        renderer_abi_version: input.abi_version.clone(),
        action_input_v0: input_json,
        action_input_hash: validated.sha256,
        event_metadata_hash: sha256_hex(
            &canonical_bytes(&input.event_metadata).expect("metadata bytes"),
        ),
    };
    let spec_hash = sha256_hex(
        &canonical_bytes(&serde_json::to_value(&spec).expect("spec JSON")).expect("spec bytes"),
    );
    let run_name = automation_run_name(&spec.root_uid, &spec.occurrence_id)
        .expect("deterministic AutomationRun name");
    let pending = AutomationPending {
        occurrence_id: spec.occurrence_id.clone(),
        automation_id: spec.automation_id.clone(),
        source: spec.source.clone(),
        run_name: run_name.clone(),
        run_uid: "run-uid".into(),
        run_resource_version: "1".into(),
        spec_hash,
    };
    let expectation = kube_transport::AutomationRunExpectationV0::from_pending(
        ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: "ns".into(),
            name: "root".into(),
        },
        "root-uid",
        &pending,
    )
    .expect("pending expectation");
    let current = ObjectV0 {
        reference: ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: "ns".into(),
            name: run_name.clone(),
        },
        metadata: ObjectMetaV0 {
            uid: "run-uid".into(),
            resource_version: "1".into(),
            annotations: std::collections::BTreeMap::new(),
            owner_uids: vec!["root-uid".into()],
        },
        value: json!({
            "apiVersion":"runtime.oxideloop.dev/v1alpha1",
            "kind":"AutomationRun",
            "metadata":{"name":run_name,"namespace":"ns","uid":"run-uid","resourceVersion":"1",
                "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"root","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},
            "spec":spec,
            "status":{}
        }),
    };
    let mut retained = AutomationRunObservationBarrierV0::test_from_authenticated(
        expectation,
        current.clone(),
        watch,
    )
    .expect("authenticated retained watch");
    assert!(
        tokio::time::timeout(
            FINITE_IO_BUDGET + Duration::from_millis(100),
            retained.next_change()
        )
        .await
        .is_err(),
        "watch silence beyond the finite request budget must not time out a retained stream"
    );
    let mut changed = current;
    changed.metadata.resource_version = "2".into();
    changed.value["metadata"]["resourceVersion"] = json!("2");
    changed.value["status"] = serde_json::to_value(AutomationRunStatus {
        attempts: 1,
        materialization_name: Some("event".into()),
        materialization_uid: Some("event-uid".into()),
        completion: Some(AutomationRunCompletion {
            state: AutomationRunCompletionState::Succeeded,
            message: "materialized".into(),
        }),
    })
    .expect("completion status JSON");
    sender
        .send(Ok(WatchEventV0::Modified(changed)))
        .await
        .expect("authenticated retained change");
    let delivered = tokio::time::timeout(Duration::from_secs(1), retained.next_change())
        .await
        .expect("retained event delivery")
        .expect("retained event validates");
    assert_eq!(delivered.resource_version(), "2");
}
