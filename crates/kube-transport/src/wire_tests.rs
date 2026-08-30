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
use oxideloop_controller_api::ObjectIdentity;
use oxideloop_controller_core::{CommandV0, MutationCommandV0, PreconditionsV0};
use oxideloop_mutation_model::{HttpMethod, MutationDecision};
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

fn object_json(kind: ResourceKindV0, name: &str, uid: &str, rv: &str) -> Value {
    json!({
        "apiVersion": kind.api_version(),
        "kind": kind.kind(),
        "metadata": {"name": name, "uid": uid, "resourceVersion": rv},
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
