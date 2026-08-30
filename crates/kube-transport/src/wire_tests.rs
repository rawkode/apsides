//! Adversarial wire tests for the private Kubernetes client boundary.
//!
//! The scripted service rejects an unexpected request or request order.  These
//! tests intentionally assert HTTP details rather than reproducing transport
//! helpers at the DTO layer.

use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use http::{Method, Request, Response, StatusCode};
use kube::{Client, client::Body};
use oxideloop_controller_api::{
    AutomationRunCompletion, AutomationRunCompletionState, AutomationRunSpec, AutomationRunStatus,
    ObjectIdentity, ProgramBundleReference,
};
use oxideloop_controller_core::{CommandV0, MutationCommandV0, PreconditionsV0};
use oxideloop_event_planner::{
    ObjectIdentity as PlannerObjectIdentity, ObjectRef as PlannerObjectRef, RunCorrelationV0,
    materialize_event, plan_event,
};
use oxideloop_mutation_model::{HttpMethod, MutationDecision};
use oxideloop_renderer_abi::{
    canonical_bytes, parse_validate_action_input, parse_validate_action_output, sha256_hex,
};
use serde_json::{Value, json};
use tower::service_fn;

use super::{
    COMMAND_ID_ANNOTATION, KubeTransportV0, ObjectMetaV0, ObjectRefV0, ObjectV0, ROOT_FINALIZER,
    ROOT_UID_ANNOTATION, ResourceKindV0, RootAddressV0, TRANSACTION_ID_ANNOTATION,
    TransportErrorV0,
};

struct Expected {
    method: Method,
    path_and_query: &'static str,
    content_type: Option<&'static str>,
    body: Option<Value>,
    response: Response<Body>,
}

#[derive(Clone)]
struct ScriptedApi {
    expected: Arc<Mutex<VecDeque<Expected>>>,
}

impl ScriptedApi {
    fn transport(expected: impl IntoIterator<Item = Expected>) -> (KubeTransportV0, Self) {
        let api = Self {
            expected: Arc::new(Mutex::new(expected.into_iter().collect())),
        };
        let queued = Arc::clone(&api.expected);
        let service = service_fn(move |request: Request<Body>| {
            let queued = Arc::clone(&queued);
            async move {
                let expected = queued
                    .lock()
                    .expect("script lock")
                    .pop_front()
                    .expect("unexpected Kubernetes request");
                assert_eq!(request.method(), expected.method);
                assert_eq!(
                    request
                        .uri()
                        .path_and_query()
                        .expect("request URI")
                        .as_str(),
                    expected.path_and_query
                );
                assert_eq!(
                    request
                        .headers()
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok()),
                    expected.content_type,
                );
                let received = request
                    .into_body()
                    .collect_bytes()
                    .await
                    .expect("read request body");
                match expected.body {
                    Some(body) => assert_eq!(
                        serde_json::from_slice::<Value>(&received).expect("JSON body"),
                        body
                    ),
                    None => assert!(
                        received.is_empty(),
                        "expected an empty body, got {received:?}"
                    ),
                }
                Ok::<_, Infallible>(expected.response)
            }
        });
        (
            KubeTransportV0::from_client(Client::new(service, "default")),
            api,
        )
    }

    fn assert_drained(&self) {
        assert!(
            self.expected.lock().expect("script lock").is_empty(),
            "unissued scripted Kubernetes requests"
        );
    }
}

fn response(value: Value) -> Response<Body> {
    Response::builder()
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("response JSON"),
        ))
        .expect("response")
}

fn status(code: StatusCode, value: Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&value).expect("status JSON")))
        .expect("response")
}

fn lost_response() -> Response<Body> {
    Response::builder()
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(b"{".to_vec()))
        .expect("lost response")
}

fn generated_event_plan() -> oxideloop_event_planner::PlannedEventV0 {
    let input = parse_validate_action_input(
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
    .expect("valid P7 action input");
    let root = PlannerObjectRef {
        identity: PlannerObjectIdentity {
            api_version: "demo.oxideloop.dev/v1alpha1".into(),
            kind: "WebApplication".into(),
            namespace: "tenant-a".into(),
            name: "portal".into(),
        },
        uid: "root-uid".into(),
        resource_version: "4".into(),
    };
    let source = PlannerObjectRef {
        identity: PlannerObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "tenant-a".into(),
            name: "input".into(),
        },
        uid: "source-uid".into(),
        resource_version: "7".into(),
    };
    let run = PlannerObjectRef {
        identity: PlannerObjectIdentity {
            api_version: "runtime.oxideloop.dev/v1alpha1".into(),
            kind: "AutomationRun".into(),
            namespace: "tenant-a".into(),
            name: "ol-run-c9c330b81f6a82450cba7ad5b2d6782b52a82054dcd4487f2b635180".into(),
        },
        uid: "run-uid".into(),
        resource_version: "8".into(),
    };
    let document = &input.document;
    let correlation = RunCorrelationV0 {
        occurrence_id: document.occurrence_id.clone(),
        root: root.clone(),
        run: run.clone(),
        source: source.identity.clone(),
        source_uid: document.source_uid.clone(),
        source_resource_version: document.source_resource_version.clone(),
        source_transition_id: document.source_transition_id.clone(),
        automation_identity: document.automation_identity.clone(),
        automation_version: document.automation_version.clone(),
        bundle_digest: document.bundle_digest.clone(),
        action_module_identity: document.action_module_identity.clone(),
        action_module_digest: document.action_module_digest.clone(),
        renderer_abi_version: document.abi_version.clone(),
        action_input_hash: input.sha256.clone(),
        event_metadata_hash: sha256_hex(
            &canonical_bytes(&document.event_metadata).expect("metadata bytes"),
        ),
    };
    let output = parse_validate_action_output(
        &include_str!("../../../packages/example-bundle/fixtures/input-changed-action-output.json")
            .replace("\"demo\"", "\"tenant-a\""),
    )
    .expect("valid P7 action output");
    plan_event(&input, &output, &root, &source, &run, &correlation).expect("P7 plan")
}

fn event_response(plan: &oxideloop_event_planner::PlannedEventV0, uid: &str, rv: &str) -> Value {
    let mut event = materialize_event(plan, "2026-01-01T00:00:00Z").expect("materialized event");
    event["metadata"]["uid"] = json!(uid);
    event["metadata"]["resourceVersion"] = json!(rv);
    event
}

fn automation_spec() -> AutomationRunSpec {
    let input = parse_validate_action_input(
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
    let input_document = input.document;
    let event_metadata_hash =
        sha256_hex(&canonical_bytes(&input_document.event_metadata).expect("metadata bytes"));
    AutomationRunSpec {
        occurrence_id: input_document.occurrence_id,
        root_uid: "root-uid".into(),
        source: ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "tenant-a".into(),
            name: "input".into(),
        },
        source_uid: input_document.source_uid,
        source_resource_version: input_document.source_resource_version,
        source_transition_id: input_document.source_transition_id,
        automation_id: input_document.automation_identity,
        automation_version: input_document.automation_version,
        bundle_digest: input_document.bundle_digest.clone(),
        program: ProgramBundleReference {
            config_map_name: "bundle".into(),
            key: "program.mjs".into(),
            digest: input_document.bundle_digest,
        },
        action_module_identity: input_document.action_module_identity,
        action_module_digest: input_document.action_module_digest,
        renderer_abi_version: input_document.abi_version,
        action_input_v0: String::from_utf8(input.canonical_bytes).expect("canonical input"),
        action_input_hash: input.sha256,
        event_metadata_hash,
    }
}

fn completion_status() -> AutomationRunStatus {
    AutomationRunStatus {
        attempts: 2,
        materialization_name: Some("event-a".into()),
        materialization_uid: Some("event-uid".into()),
        completion: Some(AutomationRunCompletion {
            state: AutomationRunCompletionState::Succeeded,
            message: "created".into(),
        }),
    }
}

fn automation_response(
    spec: &AutomationRunSpec,
    status_value: &AutomationRunStatus,
    rv: &str,
) -> Value {
    json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1",
        "kind":"AutomationRun",
        "metadata":{"name":"run-a","namespace":"tenant-a","uid":"run-uid","resourceVersion":rv},
        "spec": spec,
        "status": status_value,
    })
}

fn object_json(kind: ResourceKindV0, name: &str, uid: &str, rv: &str) -> Value {
    json!({
        "apiVersion": kind.api_version(),
        "kind": kind.kind(),
        "metadata": {"name": name, "uid": uid, "resourceVersion": rv},
    })
}

fn collection_json(items: Value) -> Value {
    json!({"apiVersion":"v1", "kind":"List", "metadata":{"resourceVersion":"list-rv"}, "items":items})
}

fn root_only_preconditions(
    root_rv: &str,
    root_generation: i64,
    state_uid: &str,
) -> PreconditionsV0 {
    PreconditionsV0 {
        root_uid: "root-uid".into(),
        root_generation,
        root_resource_version: root_rv.into(),
        state_uid: state_uid.into(),
        state_resource_version: String::new(),
        observation_token: "o".into(),
        cache_epoch: "e".into(),
    }
}

fn root_only_json(
    root_rv: &str,
    generation: i64,
    state_name: &str,
    receipt_state_uid: &str,
    cleanup_token: &str,
) -> Value {
    json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1",
        "kind":"WebApplication",
        "metadata":{
            "name":"app-a",
            "namespace":"tenant-a",
            "uid":"root-uid",
            "resourceVersion":root_rv,
            "generation":generation,
            "deletionTimestamp":"2026-01-01T00:00:00Z",
            "finalizers":[ROOT_FINALIZER]
        },
        "status":{
            "deletionCleanupReceipt":{
                "rootUid":"root-uid",
                "deletionId":"dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                "deletionStartGeneration":generation,
                "stateName":state_name,
                "stateUid":receipt_state_uid,
                "cleanupToken":cleanup_token
            }
        }
    })
}

fn desired(kind: ResourceKindV0, name: &str, rv: &str) -> ObjectV0 {
    ObjectV0 {
        reference: ObjectRefV0 {
            resource: kind,
            namespace: "tenant-a".into(),
            name: name.into(),
        },
        metadata: ObjectMetaV0 {
            uid: "old-uid".into(),
            resource_version: rv.into(),
            annotations: BTreeMap::new(),
            owner_uids: vec![],
        },
        value: json!({
            "apiVersion": kind.api_version(),
            "kind": kind.kind(),
            "metadata": {"name": name, "namespace": "tenant-a"},
            "spec": {"desired": true},
        }),
    }
}

fn delete_command(
    kind: ResourceKindV0,
    name: &str,
    uid: &str,
    resource_version: &str,
) -> CommandV0 {
    CommandV0::Mutate {
        transaction_id: "tx".into(),
        attempt_token: "attempt".into(),
        commands: vec![MutationCommandV0 {
            command_id: "command".into(),
            attempt_token: "attempt".into(),
            preconditions: PreconditionsV0 {
                root_uid: "root-uid".into(),
                root_generation: 1,
                root_resource_version: "9".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "4".into(),
                observation_token: "o".into(),
                cache_epoch: "e".into(),
            },
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: kind.kind().into(),
                namespace: "tenant-a".into(),
                name: name.into(),
            },
            logical_id: name.into(),
            decision: MutationDecision::PreconditionedDelete {
                method: HttpMethod::Delete,
                preconditions: oxideloop_mutation_model::ObjectIdentity {
                    uid: uid.into(),
                    resource_version: resource_version.into(),
                },
            },
        }],
    }
}

fn delete_fence_script(
    kind: ResourceKindV0,
    name: &str,
    target_uid: &str,
    target_resource_version: &str,
) -> [Expected; 3] {
    [
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(json!({
                "apiVersion":"demo.oxideloop.dev/v1alpha1",
                "kind":"WebApplication",
                "metadata":{"name":"app-a","uid":"root-uid","resourceVersion":"9","generation":1}
            })),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
            content_type: None,
            body: None,
            response: response(object_json(
                ResourceKindV0::OxideLoopState,
                "state-a",
                "state-uid",
                "4",
            )),
        },
        Expected {
            method: Method::GET,
            path_and_query: match kind {
                ResourceKindV0::ConfigMap => "/api/v1/namespaces/tenant-a/configmaps/cm-a",
                ResourceKindV0::Service => "/api/v1/namespaces/tenant-a/services/svc-a",
                _ => panic!("delete test only supports core resources"),
            },
            content_type: None,
            body: None,
            response: response(object_json(kind, name, target_uid, target_resource_version)),
        },
    ]
}

#[tokio::test]
async fn unsupported_service_mode_is_rejected_before_any_fence_or_write_request() {
    let (transport, api) = ScriptedApi::transport([]);
    let mut command = CommandV0::Mutate {
        transaction_id: "tx".into(),
        attempt_token: "attempt".into(),
        commands: vec![MutationCommandV0 {
            command_id: "command".into(),
            attempt_token: "attempt".into(),
            preconditions: PreconditionsV0 {
                root_uid: "root".into(),
                root_generation: 1,
                root_resource_version: "1".into(),
                state_uid: "state".into(),
                state_resource_version: "1".into(),
                observation_token: "o".into(),
                cache_epoch: "e".into(),
            },
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "Service".into(),
                namespace: "tenant-a".into(),
                name: "svc".into(),
            },
            logical_id: "svc".into(),
            decision: MutationDecision::Post {
                method: HttpMethod::Post,
                body: json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"svc","namespace":"tenant-a"},"spec":{"type":"NodePort","ports":[{"port":80}]}}),
            },
        }],
    };
    for spec in [
        json!({"type":"NodePort","ports":[{"port":80}]}),
        json!({"type":"ClusterIP","ports":[{"port":80,"nodePort":30080}]}),
        json!({"clusterIP":"","ports":[{"port":80}]}),
        json!({"clusterIPs":[],"ports":[{"port":80}]}),
        json!({"clusterIPs":[""],"ports":[{"port":80}]}),
    ] {
        let CommandV0::Mutate { commands, .. } = &mut command else {
            unreachable!()
        };
        let MutationDecision::Post { body, .. } = &mut commands[0].decision else {
            unreachable!()
        };
        *body = json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"svc","namespace":"tenant-a"},"spec":spec});
        assert_eq!(
            transport
                .dispatch(
                    &RootAddressV0 {
                        namespace: "tenant-a".into(),
                        name: "root".into(),
                        state_name: "state".into()
                    },
                    &command
                )
                .await,
            Err(TransportErrorV0::UnsupportedTarget)
        );
    }
    api.assert_drained();
}

#[tokio::test]
async fn list_and_reread_use_exact_namespace_paths_and_collection_resource_version() {
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
            content_type: None,
            body: None,
            response: response(
                json!({"apiVersion":"v1", "kind":"ConfigMapList", "metadata":{"resourceVersion":"collection-rv"}, "items":[]}),
            ),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps/cm-a",
            content_type: None,
            body: None,
            response: response(object_json(
                ResourceKindV0::ConfigMap,
                "cm-a",
                "cm-uid",
                "42",
            )),
        },
    ]);

    let listed = transport
        .list_exact(ResourceKindV0::ConfigMap, "tenant-a")
        .await
        .expect("list");
    assert_eq!(listed.resource_version, "collection-rv");
    assert!(listed.items.is_empty());
    let reread = transport
        .reread(&ObjectRefV0 {
            resource: ResourceKindV0::ConfigMap,
            namespace: "tenant-a".into(),
            name: "cm-a".into(),
        })
        .await
        .expect("reread")
        .expect("object");
    assert_eq!(reread.metadata.resource_version, "42");
    api.assert_drained();
}

#[tokio::test]
async fn watch_uses_exact_resource_version_and_410_is_watch_gone() {
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?&watch=true&timeoutSeconds=290&allowWatchBookmarks=true&resourceVersion=77",
        content_type: None,
        body: None,
        response: status(
            StatusCode::GONE,
            json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "message":"too old", "reason":"Gone", "code":410}),
        ),
    }]);

    let mut watch = transport
        .watch_from(ResourceKindV0::AutomationRun, "tenant-a", "77")
        .await
        .expect("watch stream opens before its first status event");
    assert!(matches!(
        watch.next_event().await,
        Err(TransportErrorV0::WatchGone)
    ));
    api.assert_drained();
}

#[tokio::test]
async fn watch_eof_invalidates_the_epoch() {
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?&watch=true&timeoutSeconds=290&allowWatchBookmarks=true&resourceVersion=78",
        content_type: None,
        body: None,
        response: Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .expect("EOF response"),
    }]);
    let mut watch = transport
        .watch_from(ResourceKindV0::AutomationRun, "tenant-a", "78")
        .await
        .expect("watch");
    assert!(matches!(
        watch.next_event().await,
        Err(TransportErrorV0::WatchGone)
    ));
    api.assert_drained();
}

#[tokio::test]
async fn terminal_watch_error_is_latched_and_never_releases_a_queued_event() {
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    sender
        .send(Err(TransportErrorV0::WatchGone))
        .await
        .expect("send terminal");
    sender
        .send(Ok(super::WatchEventV0::Bookmark {
            resource_version: "later".into(),
        }))
        .await
        .expect("send queued event");
    let mut watch = super::WatchHandleV0 {
        events: receiver,
        overflowed: Arc::new(AtomicBool::new(false)),
        terminal: None,
    };
    assert!(matches!(
        watch.next_event().await,
        Err(TransportErrorV0::WatchGone)
    ));
    assert!(matches!(
        watch.next_event().await,
        Err(TransportErrorV0::WatchGone)
    ));
}

#[tokio::test]
async fn apply_status_and_delete_have_exact_wire_preconditions() {
    let config_map = desired(ResourceKindV0::ConfigMap, "cm-a", "42");
    let config_map_body = json!({
        "apiVersion":"v1", "kind":"ConfigMap", "metadata":{"name":"cm-a", "namespace":"tenant-a", "resourceVersion":"42"}, "spec":{"desired":true}
    });
    let mut root = desired(ResourceKindV0::WebApplication, "app-a", "99");
    root.value = json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication", "metadata":{"name":"app-a", "namespace":"tenant-a"}, "status":{"phase":"Ready"}
    });
    let root_body = json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication", "metadata":{"name":"app-a", "namespace":"tenant-a", "resourceVersion":"99"}, "status":{"phase":"Ready"}
    });
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::PATCH,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps/cm-a?&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(config_map_body),
            response: response(object_json(
                ResourceKindV0::ConfigMap,
                "cm-a",
                "cm-uid",
                "43",
            )),
        },
        Expected {
            method: Method::PATCH,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a/status?&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(root_body),
            response: response(object_json(
                ResourceKindV0::WebApplication,
                "app-a",
                "root-uid",
                "100",
            )),
        },
        Expected {
            method: Method::DELETE,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps/cm-a?",
            content_type: Some("application/json"),
            body: Some(json!({"preconditions":{"uid":"cm-uid", "resourceVersion":"43"}})),
            response: response(json!({"kind":"Status", "apiVersion":"v1", "status":"Success"})),
        },
    ]);

    transport
        .patch_config_map(&config_map, "42")
        .await
        .expect("SSA config map");
    transport
        .patch_web_application_status(&root, "99")
        .await
        .expect("status patch");
    transport
        .delete_config_map(&config_map.reference, "cm-uid", "43")
        .await
        .expect("delete");
    api.assert_drained();
}

#[tokio::test]
async fn managed_delete_ack_echoes_prepared_identity_for_config_map_and_service() {
    for (kind, name, uid, resource_version, delete_path) in [
        (
            ResourceKindV0::ConfigMap,
            "cm-a",
            "prepared-cm-uid",
            "prepared-cm-rv",
            "/api/v1/namespaces/tenant-a/configmaps/cm-a?",
        ),
        (
            ResourceKindV0::Service,
            "svc-a",
            "prepared-svc-uid",
            "prepared-svc-rv",
            "/api/v1/namespaces/tenant-a/services/svc-a?",
        ),
    ] {
        let mut expected: Vec<_> = delete_fence_script(kind, name, uid, resource_version)
            .into_iter()
            .collect();
        expected.push(Expected {
            method: Method::DELETE,
            path_and_query: delete_path,
            content_type: Some("application/json"),
            body: Some(json!({
                "preconditions":{"uid":uid,"resourceVersion":resource_version}
            })),
            response: response(json!({"kind":"Status","apiVersion":"v1","status":"Success"})),
        });
        let (transport, api) = ScriptedApi::transport(expected);
        let outcome = transport
            .dispatch(
                &RootAddressV0 {
                    namespace: "tenant-a".into(),
                    name: "app-a".into(),
                    state_name: "state-a".into(),
                },
                &delete_command(kind, name, uid, resource_version),
            )
            .await
            .expect("delete acknowledgement");
        assert!(matches!(
            outcome,
            super::CommandOutcomeV0::Event(event)
                if matches!(*event, oxideloop_controller_core::EventV0::MutationAcknowledged { ref results, .. }
                    if results.len() == 1
                        && results[0].uid.as_deref() == Some(uid)
                        && results[0].resource_version.as_deref() == Some(resource_version))
        ));
        api.assert_drained();
    }
}

#[tokio::test]
async fn stale_delete_target_is_non_mutating_and_not_response_loss_recovery() {
    let (transport, api) = ScriptedApi::transport(delete_fence_script(
        ResourceKindV0::ConfigMap,
        "cm-a",
        "observed-uid",
        "observed-rv",
    ));
    assert_eq!(
        transport
            .dispatch(
                &RootAddressV0 {
                    namespace: "tenant-a".into(),
                    name: "app-a".into(),
                    state_name: "state-a".into(),
                },
                &delete_command(
                    ResourceKindV0::ConfigMap,
                    "cm-a",
                    "prepared-uid",
                    "prepared-rv"
                ),
            )
            .await,
        Err(TransportErrorV0::Stale("mutation delete pins"))
    );
    api.assert_drained();
}

#[tokio::test]
async fn root_only_verify_uses_receipt_state_identity_and_empty_state_rv() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let token = "c".repeat(64);
    let state_path = format!(
        "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{state_name}"
    );
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root_only_json("9", 3, &state_name, "state-uid", &token)),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(state_path.into_boxed_str()),
            content_type: None,
            body: None,
            response: status(
                StatusCode::NOT_FOUND,
                json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
            ),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
            content_type: None,
            body: None,
            response: response(collection_json(json!([]))),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/services?",
            content_type: None,
            body: None,
            response: response(collection_json(json!([]))),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?",
            content_type: None,
            body: None,
            response: response(collection_json(json!([]))),
        },
    ]);
    let outcome = transport
        .dispatch(
            &RootAddressV0 {
                namespace: "tenant-a".into(),
                name: "app-a".into(),
                state_name,
            },
            &CommandV0::VerifyOwnedChildrenAbsent {
                token: token.clone(),
                preconditions: root_only_preconditions("9", 3, "state-uid"),
            },
        )
        .await
        .expect("root-only verify");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(*event, oxideloop_controller_core::EventV0::OwnedChildrenVerifiedAbsent {
                token: ref actual_token,
                cleanup_token: ref actual_cleanup,
                root_generation: 3,
                ref root_uid,
                root_resource_version: ref root_rv,
                state_uid: ref actual_state_uid,
                state_resource_version: ref state_rv,
            } if actual_token == &token
                && actual_cleanup == &token
                && root_uid == "root-uid"
                && root_rv == "9"
                && actual_state_uid == "state-uid"
                && state_rv.is_empty())
    ));
    api.assert_drained();
}

#[tokio::test]
async fn root_only_remove_rechecks_children_before_exact_finalizer_patch() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let token = "c".repeat(64);
    let state_path = format!(
        "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{state_name}"
    );
    let root = root_only_json("9", 3, &state_name, "state-uid", &token);
    let patch = json!([
        {"op":"test","path":"/metadata/uid","value":"root-uid"},
        {"op":"test","path":"/metadata/resourceVersion","value":"9"},
        {"op":"test","path":"/metadata/finalizers/0","value":ROOT_FINALIZER},
        {"op":"remove","path":"/metadata/finalizers/0"}
    ]);
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(state_path.into_boxed_str()),
            content_type: None,
            body: None,
            response: status(
                StatusCode::NOT_FOUND,
                json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
            ),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
            content_type: None,
            body: None,
            response: response(collection_json(json!([]))),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/services?",
            content_type: None,
            body: None,
            response: response(collection_json(json!([]))),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?",
            content_type: None,
            body: None,
            response: response(collection_json(json!([]))),
        },
        Expected {
            method: Method::PATCH,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a?",
            content_type: Some("application/json-patch+json"),
            body: Some(patch),
            response: response(object_json(
                ResourceKindV0::WebApplication,
                "app-a",
                "root-uid",
                "10",
            )),
        },
    ]);
    let outcome = transport
        .dispatch(
            &RootAddressV0 {
                namespace: "tenant-a".into(),
                name: "app-a".into(),
                state_name,
            },
            &CommandV0::RemoveRootFinalizer {
                token: token.clone(),
                preconditions: root_only_preconditions("9", 3, "state-uid"),
            },
        )
        .await
        .expect("root-only remove");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(&*event, oxideloop_controller_core::EventV0::RootFinalizerRemoved {
                token: actual_token,
                root_uid,
                old_root_resource_version,
                new_root_resource_version,
            } if actual_token == &token
                && root_uid == "root-uid"
                && old_root_resource_version == "9"
                && new_root_resource_version == "10")
    ));
    api.assert_drained();
}

#[tokio::test]
async fn root_only_remove_holds_for_state_or_child_reappearance_without_patch() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let token = "c".repeat(64);
    let state_path = format!(
        "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{state_name}"
    );
    let root = root_only_json("9", 3, &state_name, "state-uid", &token);
    let absent = || {
        status(
            StatusCode::NOT_FOUND,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
        )
    };
    let state_object = object_json(
        ResourceKindV0::OxideLoopState,
        &state_name,
        "state-uid",
        "21",
    );
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root.clone()),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(state_path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: response(state_object),
        },
    ]);
    assert_eq!(
        transport
            .dispatch(
                &RootAddressV0 {
                    namespace: "tenant-a".into(),
                    name: "app-a".into(),
                    state_name: state_name.clone(),
                },
                &CommandV0::RemoveRootFinalizer {
                    token: token.clone(),
                    preconditions: root_only_preconditions("9", 3, "state-uid"),
                },
            )
            .await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();

    let owned_child = json!({
        "apiVersion":"v1",
        "kind":"ConfigMap",
        "metadata":{"name":"child","uid":"child-uid","resourceVersion":"1","ownerReferences":[{"uid":"root-uid"}]}
    });
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(state_path.into_boxed_str()),
            content_type: None,
            body: None,
            response: absent(),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
            content_type: None,
            body: None,
            response: response(collection_json(json!([owned_child]))),
        },
    ]);
    assert_eq!(
        transport
            .dispatch(
                &RootAddressV0 {
                    namespace: "tenant-a".into(),
                    name: "app-a".into(),
                    state_name,
                },
                &CommandV0::RemoveRootFinalizer {
                    token,
                    preconditions: root_only_preconditions("9", 3, "state-uid"),
                },
            )
            .await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();
}

#[tokio::test]
async fn root_only_validation_failures_never_issue_state_or_patch_requests() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let token = "c".repeat(64);
    let stale_rv = root_only_json("10", 3, &state_name, "state-uid", &token);
    let mut not_deleting = root_only_json("9", 3, &state_name, "state-uid", &token);
    not_deleting["metadata"]
        .as_object_mut()
        .expect("metadata")
        .remove("deletionTimestamp");
    let mut no_receipt = root_only_json("9", 3, &state_name, "state-uid", &token);
    no_receipt["status"]
        .as_object_mut()
        .expect("status")
        .remove("deletionCleanupReceipt");
    let mut receipt_root_mismatch = root_only_json("9", 3, &state_name, "state-uid", &token);
    receipt_root_mismatch["status"]["deletionCleanupReceipt"]["rootUid"] = json!("other-root");
    let wrong_state_name = root_only_json("9", 3, "wrong-state", "state-uid", &token);
    let wrong_state_uid = root_only_json("9", 3, &state_name, "other-state", &token);
    let wrong_token = root_only_json("9", 3, &state_name, "state-uid", &"d".repeat(64));
    let mut no_finalizer = root_only_json("9", 3, &state_name, "state-uid", &token);
    no_finalizer["metadata"]["finalizers"] = json!([]);
    let mut wrong_root_kind = root_only_json("9", 3, &state_name, "state-uid", &token);
    wrong_root_kind["kind"] = json!("Service");
    let mut wrong_root_name = root_only_json("9", 3, &state_name, "state-uid", &token);
    wrong_root_name["metadata"]["name"] = json!("other");
    let mut wrong_root_namespace = root_only_json("9", 3, &state_name, "state-uid", &token);
    wrong_root_namespace["metadata"]["namespace"] = json!("tenant-b");
    let cases = [
        stale_rv,
        not_deleting,
        no_receipt,
        receipt_root_mismatch,
        wrong_state_name,
        wrong_state_uid,
        wrong_token,
        no_finalizer,
        wrong_root_kind,
        wrong_root_name,
        wrong_root_namespace,
    ];
    for root in cases {
        let (transport, api) = ScriptedApi::transport([Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root),
        }]);
        assert_eq!(
            transport
                .dispatch(
                    &RootAddressV0 {
                        namespace: "tenant-a".into(),
                        name: "app-a".into(),
                        state_name: state_name.clone(),
                    },
                    &CommandV0::RemoveRootFinalizer {
                        token: token.clone(),
                        preconditions: root_only_preconditions("9", 3, "state-uid"),
                    },
                )
                .await,
            Err(TransportErrorV0::ManualRecoveryHold)
        );
        api.assert_drained();
    }
}

#[tokio::test]
async fn empty_state_rv_on_unrelated_effect_is_held_without_a_request() {
    let (transport, api) = ScriptedApi::transport([]);
    assert_eq!(
        transport
            .dispatch(
                &RootAddressV0 {
                    namespace: "tenant-a".into(),
                    name: "app-a".into(),
                    state_name: "state-a".into(),
                },
                &CommandV0::RemoveStateFinalizer {
                    token: "token".into(),
                    preconditions: root_only_preconditions("9", 3, "state-uid"),
                },
            )
            .await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();
}

#[tokio::test]
async fn empty_state_rv_on_automation_effects_is_held_without_requests() {
    let commands = [
        CommandV0::EnsureAutomationRun {
            token: "token".into(),
            name: "run-a".into(),
            spec: automation_spec(),
            preconditions: root_only_preconditions("9", 3, "state-uid"),
        },
        CommandV0::DeleteAutomationRun {
            token: "token".into(),
            occurrence_id: "occurrence-1".into(),
            name: "run-a".into(),
            uid: "run-uid".into(),
            resource_version: "4".into(),
            preconditions: root_only_preconditions("9", 3, "state-uid"),
        },
    ];

    for command in commands {
        let (transport, api) = ScriptedApi::transport([]);
        assert_eq!(
            transport
                .dispatch(
                    &RootAddressV0 {
                        namespace: "tenant-a".into(),
                        name: "app-a".into(),
                        state_name: "state-a".into(),
                    },
                    &command,
                )
                .await,
            Err(TransportErrorV0::ManualRecoveryHold)
        );
        api.assert_drained();
    }
}

#[tokio::test]
async fn root_absence_terminal_check_allows_empty_state_rv_sentinel() {
    let token = "c".repeat(64);
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
        content_type: None,
        body: None,
        response: status(
            StatusCode::NOT_FOUND,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
        ),
    }]);
    let outcome = transport
        .dispatch(
            &RootAddressV0 {
                namespace: "tenant-a".into(),
                name: "app-a".into(),
                state_name: "ignored-for-terminal-check".into(),
            },
            &CommandV0::VerifyRootUidAbsent {
                token: token.clone(),
                root_uid: "root-uid".into(),
                preconditions: root_only_preconditions("9", 3, "state-uid"),
            },
        )
        .await
        .expect("terminal absence check");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(&*event, oxideloop_controller_core::EventV0::RootUidVerifiedAbsent {
                token: actual_token,
                root_uid: actual_uid,
            } if actual_token == &token && actual_uid == "root-uid")
    ));
    api.assert_drained();
}

#[tokio::test]
async fn bundle_reader_accepts_only_bounded_binary_data_in_controller_namespace() {
    let bundle = json!({
        "apiVersion":"v1",
        "kind":"ConfigMap",
        "metadata":{"name":"bundle","namespace":"tenant-a","uid":"bundle-uid","resourceVersion":"17"},
        "binaryData":{"bundle.wasm":"AAH/"}
    });
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/api/v1/namespaces/tenant-a/configmaps/bundle",
        content_type: None,
        body: None,
        response: response(bundle),
    }]);
    assert_eq!(
        transport
            .read_bundle_binary_data("tenant-a", "tenant-a", "bundle", "bundle.wasm", 3)
            .await
            .expect("binaryData bundle"),
        super::ConfigMapBinaryDataV0 {
            bytes: vec![0, 1, 255],
            uid: "bundle-uid".into(),
            resource_version: "17".into(),
        }
    );
    api.assert_drained();

    let (transport, api) = ScriptedApi::transport([]);
    assert_eq!(
        transport
            .read_bundle_binary_data("tenant-a", "tenant-b", "bundle", "bundle.wasm", 3)
            .await,
        Err(TransportErrorV0::UnsupportedTarget)
    );
    api.assert_drained();

    let identity_cases = [
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"other","namespace":"tenant-a","uid":"u","resourceVersion":"1"},"binaryData":{"bundle.wasm":"AA=="}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","namespace":"tenant-b","uid":"u","resourceVersion":"1"},"binaryData":{"bundle.wasm":"AA=="}}),
        json!({"apiVersion":"apps/v1","kind":"ConfigMap","metadata":{"name":"bundle","namespace":"tenant-a","uid":"u","resourceVersion":"1"},"binaryData":{"bundle.wasm":"AA=="}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","namespace":"tenant-a","uid":"","resourceVersion":"1"},"binaryData":{"bundle.wasm":"AA=="}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","namespace":"tenant-a","uid":"u","resourceVersion":""},"binaryData":{"bundle.wasm":"AA=="}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","namespace":"tenant-a","uid":"u".repeat(254),"resourceVersion":"1"},"binaryData":{"bundle.wasm":"AA=="}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","namespace":"tenant-a","uid":"u","resourceVersion":"r".repeat(254)},"binaryData":{"bundle.wasm":"AA=="}}),
    ];
    for value in identity_cases {
        let (transport, api) = ScriptedApi::transport([Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps/bundle",
            content_type: None,
            body: None,
            response: response(value),
        }]);
        assert_eq!(
            transport
                .read_bundle_binary_data("tenant-a", "tenant-a", "bundle", "bundle.wasm", 1)
                .await,
            Err(TransportErrorV0::InvalidBundleData)
        );
        api.assert_drained();
    }
}

#[tokio::test]
async fn bundle_reader_rejects_missing_text_empty_and_oversize_values() {
    let cases = [
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","uid":"u","resourceVersion":"1"}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","uid":"u","resourceVersion":"1"},"data":{"bundle.wasm":"text"}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","uid":"u","resourceVersion":"1"},"binaryData":{"other":"AA=="}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","uid":"u","resourceVersion":"1"},"binaryData":{"bundle.wasm":""}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","uid":"u","resourceVersion":"1"},"binaryData":{"bundle.wasm":"AAE="}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bundle","uid":"u","resourceVersion":"1"},"binaryData":{"bundle.wasm":"!!!!"}}),
    ];
    for value in cases {
        let (transport, api) = ScriptedApi::transport([Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps/bundle",
            content_type: None,
            body: None,
            response: response(value),
        }]);
        assert_eq!(
            transport
                .read_bundle_binary_data("tenant-a", "tenant-a", "bundle", "bundle.wasm", 1)
                .await,
            Err(TransportErrorV0::InvalidBundleData)
        );
        api.assert_drained();
    }

    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/api/v1/namespaces/tenant-a/configmaps/bundle",
        content_type: None,
        body: None,
        response: status(
            StatusCode::NOT_FOUND,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
        ),
    }]);
    assert_eq!(
        transport
            .read_bundle_binary_data("tenant-a", "tenant-a", "bundle", "bundle.wasm", 1)
            .await,
        Err(TransportErrorV0::InvalidBundleData)
    );
    api.assert_drained();
}

#[tokio::test]
async fn create_carries_recovery_proof_and_non_blocking_owner_reference() {
    let mut object = desired(ResourceKindV0::ConfigMap, "cm-a", "ignored");
    object.value["metadata"]["annotations"] = json!({
        ROOT_UID_ANNOTATION: "root-uid",
        TRANSACTION_ID_ANNOTATION: "tx-1",
        COMMAND_ID_ANNOTATION: "cmd-1",
    });
    object.value["metadata"]["ownerReferences"] = json!([{
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication", "name":"app-a", "uid":"root-uid", "controller":true, "blockOwnerDeletion":false
    }]);
    let expected_body = object.value.clone();
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::POST,
        path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
        content_type: Some("application/json"),
        body: Some(expected_body),
        response: response(object_json(
            ResourceKindV0::ConfigMap,
            "cm-a",
            "cm-uid",
            "1",
        )),
    }]);

    transport.create_config_map(&object).await.expect("create");
    api.assert_drained();
}

#[tokio::test]
async fn event_create_and_exact_recovery_are_typed_and_use_only_get_or_post() {
    let plan = generated_event_plan();
    let path = format!(
        "/apis/events.k8s.io/v1/namespaces/tenant-a/events/{}",
        plan.metadata.name
    );
    let collection = "/apis/events.k8s.io/v1/namespaces/tenant-a/events?";
    let desired = materialize_event(&plan, "2026-01-01T00:00:00Z").expect("event body");
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: status(
                StatusCode::NOT_FOUND,
                json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","message":"not found","code":404}),
            ),
        },
        Expected {
            method: Method::POST,
            path_and_query: collection,
            content_type: Some("application/json"),
            body: Some(desired.clone()),
            response: response(event_response(&plan, "event-uid", "9")),
        },
    ]);
    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await
            .expect("created event")
            .resource_version,
        "9"
    );
    api.assert_drained();

    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: Box::leak(path.clone().into_boxed_str()),
        content_type: None,
        body: None,
        response: response(event_response(&plan, "event-uid", "9")),
    }]);
    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await
            .expect("exact existing event")
            .uid,
        "event-uid"
    );
    api.assert_drained();
}

#[tokio::test]
async fn event_lost_create_recovers_only_a_fresh_semantic_match() {
    let plan = generated_event_plan();
    let path = format!(
        "/apis/events.k8s.io/v1/namespaces/tenant-a/events/{}",
        plan.metadata.name
    );
    let desired = materialize_event(&plan, "2026-01-01T00:00:00Z").expect("event body");
    let missing = || {
        status(
            StatusCode::NOT_FOUND,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","message":"not found","code":404}),
        )
    };
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: missing(),
        },
        Expected {
            method: Method::POST,
            path_and_query: "/apis/events.k8s.io/v1/namespaces/tenant-a/events?",
            content_type: Some("application/json"),
            body: Some(desired.clone()),
            response: lost_response(),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: response(event_response(&plan, "event-uid", "10")),
        },
    ]);
    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await,
        Ok(super::EventMaterializationRefV0 {
            name: plan.metadata.name.clone(),
            uid: "event-uid".into(),
            resource_version: "10".into()
        })
    );
    api.assert_drained();

    let mut foreign = event_response(&plan, "foreign", "11");
    foreign["note"] = json!("different");
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: missing(),
        },
        Expected {
            method: Method::POST,
            path_and_query: "/apis/events.k8s.io/v1/namespaces/tenant-a/events?",
            content_type: Some("application/json"),
            body: Some(desired.clone()),
            response: lost_response(),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: response(foreign),
        },
    ]);
    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await,
        Err(TransportErrorV0::ForeignCollision)
    );
    api.assert_drained();

    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(path.into_boxed_str()),
            content_type: None,
            body: None,
            response: missing(),
        },
        Expected {
            method: Method::POST,
            path_and_query: "/apis/events.k8s.io/v1/namespaces/tenant-a/events?",
            content_type: Some("application/json"),
            body: Some(desired),
            response: lost_response(),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(
                format!(
                    "/apis/events.k8s.io/v1/namespaces/tenant-a/events/{}",
                    plan.metadata.name
                )
                .into_boxed_str(),
            ),
            content_type: None,
            body: None,
            response: missing(),
        },
    ]);
    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await,
        Err(TransportErrorV0::ResponseLost)
    );
    api.assert_drained();
}

#[tokio::test]
async fn generic_event_reads_and_watches_are_rejected_without_a_wire_request() {
    let reference = ObjectRefV0 {
        resource: ResourceKindV0::Event,
        namespace: "tenant-a".into(),
        name: "event-a".into(),
    };
    let (transport, api) = ScriptedApi::transport([]);
    assert_eq!(
        transport.reread(&reference).await,
        Err(TransportErrorV0::UnsupportedTarget)
    );
    assert_eq!(
        transport
            .list_exact(ResourceKindV0::Event, "tenant-a")
            .await,
        Err(TransportErrorV0::UnsupportedTarget)
    );
    assert!(matches!(
        transport
            .watch_from(ResourceKindV0::Event, "tenant-a", "1")
            .await,
        Err(TransportErrorV0::UnsupportedTarget)
    ));
    api.assert_drained();
}

#[tokio::test]
async fn automation_completion_uses_only_status_patch_with_an_exact_recovery_proof() {
    let spec = automation_spec();
    let desired_status = completion_status();
    let observed = automation_response(&spec, &AutomationRunStatus::default(), "4");
    let completed = automation_response(&spec, &desired_status, "5");
    let patch_body = json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1", "kind":"AutomationRun",
        "metadata":{"name":"run-a", "namespace":"tenant-a", "resourceVersion":"4"},
        "status": desired_status,
    });
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a",
            content_type: None,
            body: None,
            response: response(observed),
        },
        Expected {
            method: Method::PATCH,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a/status?&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(patch_body),
            response: response(completed),
        },
    ]);
    let ack = transport
        .persist_automation_run_completion(
            "tenant-a",
            "run-a",
            "run-uid",
            "4",
            &spec,
            &desired_status,
        )
        .await
        .expect("status persisted");
    assert_eq!(ack.new_resource_version, "5");
    assert_eq!(ack.completion, AutomationRunCompletionState::Succeeded);
    api.assert_drained();
}

#[tokio::test]
async fn automation_completion_lost_response_never_adopts_divergent_status_or_spec() {
    let spec = automation_spec();
    let desired_status = completion_status();
    let observed = automation_response(&spec, &AutomationRunStatus::default(), "4");
    let completed = automation_response(&spec, &desired_status, "5");
    let patch_body = json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"AutomationRun","metadata":{"name":"run-a","namespace":"tenant-a","resourceVersion":"4"},"status":desired_status});
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a",
            content_type: None,
            body: None,
            response: response(observed.clone()),
        },
        Expected {
            method: Method::PATCH,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a/status?&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(patch_body.clone()),
            response: lost_response(),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a",
            content_type: None,
            body: None,
            response: response(completed),
        },
    ]);
    assert_eq!(
        transport
            .persist_automation_run_completion(
                "tenant-a",
                "run-a",
                "run-uid",
                "4",
                &spec,
                &desired_status
            )
            .await
            .expect("recovered completion")
            .new_resource_version,
        "5"
    );
    api.assert_drained();

    let mut divergent = automation_response(&spec, &desired_status, "5");
    divergent["metadata"]["uid"] = json!("other-run");
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a",
            content_type: None,
            body: None,
            response: response(observed),
        },
        Expected {
            method: Method::PATCH,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a/status?&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(patch_body),
            response: lost_response(),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a",
            content_type: None,
            body: None,
            response: response(divergent),
        },
    ]);
    assert_eq!(
        transport
            .persist_automation_run_completion(
                "tenant-a",
                "run-a",
                "run-uid",
                "4",
                &spec,
                &desired_status
            )
            .await,
        Err(TransportErrorV0::Stale("automation completion pins"))
    );
    api.assert_drained();
}

#[tokio::test]
async fn stale_pins_make_no_effect_request_and_409_maps_to_conflict() {
    let stale = desired(ResourceKindV0::ConfigMap, "cm-a", "actual");
    let (transport, api) = ScriptedApi::transport([]);
    assert_eq!(
        transport.patch_config_map(&stale, "expected").await,
        Err(TransportErrorV0::Stale("write resourceVersion"))
    );
    api.assert_drained();

    let conflict = desired(ResourceKindV0::ConfigMap, "cm-a", "42");
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::PATCH,
        path_and_query: "/api/v1/namespaces/tenant-a/configmaps/cm-a?&fieldManager=oxideloop-controller",
        content_type: Some("application/apply-patch+yaml"),
        body: Some(
            json!({"apiVersion":"v1", "kind":"ConfigMap", "metadata":{"name":"cm-a", "namespace":"tenant-a", "resourceVersion":"42"}, "spec":{"desired":true}}),
        ),
        response: status(
            StatusCode::CONFLICT,
            json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "reason":"Conflict", "code":409}),
        ),
    }]);
    assert_eq!(
        transport.patch_config_map(&conflict, "42").await,
        Err(TransportErrorV0::Conflict)
    );
    api.assert_drained();
}

#[tokio::test]
async fn finalizer_json_patch_tests_uid_rv_and_value_and_ambiguous_response_holds() {
    let mut root = desired(ResourceKindV0::WebApplication, "app-a", "99");
    root.metadata.uid = "root-uid".into();
    root.value["metadata"]["finalizers"] = json!(["other", ROOT_FINALIZER]);
    let exact_patch = json!([
        {"op":"test", "path":"/metadata/uid", "value":"root-uid"},
        {"op":"test", "path":"/metadata/resourceVersion", "value":"99"},
        {"op":"test", "path":"/metadata/finalizers/1", "value":ROOT_FINALIZER},
        {"op":"remove", "path":"/metadata/finalizers/1"},
    ]);
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::PATCH,
        path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a?",
        content_type: Some("application/json-patch+json"),
        body: Some(exact_patch),
        response: response(object_json(
            ResourceKindV0::WebApplication,
            "app-a",
            "root-uid",
            "100",
        )),
    }]);
    transport
        .remove_finalizer(&root, ROOT_FINALIZER)
        .await
        .expect("finalizer removal");
    api.assert_drained();

    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::PATCH,
        path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a?",
        content_type: Some("application/json-patch+json"),
        body: Some(json!([
            {"op":"test", "path":"/metadata/uid", "value":"root-uid"},
            {"op":"test", "path":"/metadata/resourceVersion", "value":"99"},
            {"op":"test", "path":"/metadata/finalizers/1", "value":ROOT_FINALIZER},
            {"op":"remove", "path":"/metadata/finalizers/1"},
        ])),
        response: status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"kind":"Status", "apiVersion":"v1", "status":"Failure", "code":500}),
        ),
    }]);
    assert_eq!(
        transport.remove_finalizer(&root, ROOT_FINALIZER).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();
}

#[tokio::test]
async fn owned_child_verification_blocks_an_automation_run_by_owner_uid_alone() {
    let root = json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"app-a","uid":"root-uid","resourceVersion":"9","generation":1}});
    let state = object_json(ResourceKindV0::OxideLoopState, "state-a", "state-uid", "4");
    let owned_run = json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"AutomationRun","metadata":{"name":"run-a","uid":"run-uid","resourceVersion":"1","ownerReferences":[{"uid":"root-uid"}]}});
    let list = |items: Value| {
        response(
            json!({"apiVersion":"v1","kind":"List","metadata":{"resourceVersion":"x"},"items":items}),
        )
    };
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
            content_type: None,
            body: None,
            response: response(state),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
            content_type: None,
            body: None,
            response: list(json!([])),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/services?",
            content_type: None,
            body: None,
            response: list(json!([])),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?",
            content_type: None,
            body: None,
            response: list(json!([owned_run])),
        },
    ]);
    let preconditions = PreconditionsV0 {
        root_uid: "root-uid".into(),
        root_generation: 1,
        root_resource_version: "9".into(),
        state_uid: "state-uid".into(),
        state_resource_version: "4".into(),
        observation_token: "o".into(),
        cache_epoch: "e".into(),
    };
    let outcome = transport
        .dispatch(
            &super::RootAddressV0 {
                namespace: "tenant-a".into(),
                name: "app-a".into(),
                state_name: "state-a".into(),
            },
            &CommandV0::VerifyOwnedChildrenAbsent {
                token: "verify".into(),
                preconditions,
            },
        )
        .await
        .expect("verification response");
    assert!(
        matches!(outcome, super::CommandOutcomeV0::Event(event) if matches!(*event, oxideloop_controller_core::EventV0::OwnedChildrenVerificationFailed { .. }))
    );
    api.assert_drained();
}
