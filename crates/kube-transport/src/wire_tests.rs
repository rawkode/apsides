//! Adversarial wire tests for the private Kubernetes client boundary.
//!
//! The scripted service rejects an unexpected request or request order.  These
//! tests intentionally assert HTTP details rather than reproducing transport
//! helpers at the DTO layer.

use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use http::{Method, Request, Response, StatusCode};
use kube::{Client, client::Body};
use oxideloop_controller_api::{
    ApplyIntent, ApplyOperation, ApplyRecoveryPolicy, ApplyTransaction, AutomationRunCompletion,
    AutomationRunCompletionState, AutomationRunSpec, AutomationRunStatus, DurablePhase,
    NextCommandLifecycleStateV0, ObjectIdentity, OxideLoopStateSpec, PersistOperation,
    PersistPurpose, PreparedRecoveryClaim, ProgramBundleReference, RootBinding, StateBody,
    StateLink, WebApplicationStatus, automation_run_name, bootstrap_id,
};
use oxideloop_controller_core::{
    CommandV0, MutationCommandV0, PreconditionsV0, retired_state_delete_command_token,
    root_only_finalizer_command_token, root_only_owned_children_verify_token,
};
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
    AutomationRunExpectationV0, AutomationRunObservationBarrierV0, COMMAND_ID_ANNOTATION,
    ConflictProvenanceV0, KubeTransportV0, ListV0, ObjectMetaV0, ObjectRefV0, ObjectV0,
    ROOT_FINALIZER, ROOT_UID_ANNOTATION, ReadOperationV0, ResourceKindV0, RootAddressV0,
    STATE_FINALIZER, TRANSACTION_ID_ANNOTATION, TransportErrorV0, WatchEventV0, WatchHandleV0,
    WatchStageV0, WriteOperationV0, validate_automation_run_get, validate_automation_run_list,
    validate_automation_run_observation, validate_automation_watch_event,
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
            KubeTransportV0::test_from_client(Client::new(service, "default")),
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
        &include_str!("../../../packages/fixtures/golden/v0/action-output.valid.json")
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

fn pending_automation_expectation(
    spec: &AutomationRunSpec,
    resource_version: &str,
) -> AutomationRunExpectationV0 {
    let name =
        automation_run_name(&spec.root_uid, &spec.occurrence_id).expect("deterministic name");
    AutomationRunExpectationV0 {
        root: ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: "tenant-a".into(),
            name: "app-a".into(),
        },
        root_uid: spec.root_uid.clone(),
        occurrence_id: spec.occurrence_id.clone(),
        run_name: name,
        run_uid: "run-uid".into(),
        run_resource_version: resource_version.into(),
        spec_hash: sha256_hex(
            &canonical_bytes(&serde_json::to_value(spec).expect("serializable spec"))
                .expect("canonical spec"),
        ),
    }
}

fn observed_pending_run(
    expectation: &AutomationRunExpectationV0,
    spec: &AutomationRunSpec,
    status: AutomationRunStatus,
    resource_version: &str,
) -> ObjectV0 {
    let annotations = BTreeMap::new();
    ObjectV0 {
        reference: ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: expectation.root.namespace.clone(),
            name: expectation.run_name.clone(),
        },
        metadata: ObjectMetaV0 {
            uid: expectation.run_uid.clone(),
            resource_version: resource_version.into(),
            annotations: annotations.clone(),
            owner_uids: vec![expectation.root_uid.clone()],
        },
        value: json!({
            "apiVersion":"runtime.oxideloop.dev/v1alpha1",
            "kind":"AutomationRun",
            "metadata":{
                "name":expectation.run_name,
                "namespace":expectation.root.namespace,
                "uid":expectation.run_uid,
                "resourceVersion":resource_version,
                "annotations":annotations,
                "ownerReferences":[{
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplication",
                    "name":expectation.root.name,
                    "uid":expectation.root_uid,
                    "controller":true,
                    "blockOwnerDeletion":false
                }]
            },
            "spec":spec,
            "status":status,
        }),
    }
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

fn retired_state_setup() -> (RootAddressV0, CommandV0, Value, Value) {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let deletion_id = "d".repeat(64);
    let cleanup_token = "e".repeat(64);
    let root = json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1",
        "kind":"WebApplication",
        "metadata":{
            "name":"app-a",
            "namespace":"tenant-a",
            "uid":"root-uid",
            "resourceVersion":"9",
            "generation":3,
            "deletionTimestamp":"2026-01-01T00:00:00Z",
            "finalizers":[ROOT_FINALIZER],
            "annotations":{ROOT_UID_ANNOTATION:"root-uid"}
        },
        "status":{
            "state":{"rootUid":"root-uid","name":state_name,"uid":"state-uid"},
            "deletionCleanupReceipt":{
                "rootUid":"root-uid",
                "deletionId":deletion_id,
                "deletionStartGeneration":3,
                "stateName":state_name,
                "stateUid":"state-uid",
                "cleanupToken":cleanup_token
            }
        }
    });
    let state = json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1",
        "kind":"OxideLoopState",
        "metadata":{
            "name":state_name,
            "namespace":"tenant-a",
            "uid":"state-uid",
            "resourceVersion":"4",
            "annotations":{ROOT_UID_ANNOTATION:"root-uid"},
            "ownerReferences":[{
                "apiVersion":"demo.oxideloop.dev/v1alpha1",
                "kind":"WebApplication",
                "name":"app-a",
                "uid":"root-uid",
                "controller":true,
                "blockOwnerDeletion":false
            }]
        },
        "spec":{"intentionally":"ignored-by-retired-delete"}
    });
    let token = retired_state_delete_command_token(
        "root-uid",
        3,
        "9",
        &state_name,
        "state-uid",
        "4",
        &deletion_id,
        3,
        &cleanup_token,
    );
    let command = CommandV0::DeleteRetiredState {
        token,
        preconditions: PreconditionsV0 {
            root_uid: "root-uid".into(),
            root_generation: 3,
            root_resource_version: "9".into(),
            state_uid: "state-uid".into(),
            state_resource_version: "4".into(),
            observation_token: "retired-observation".into(),
            cache_epoch: "retired-cache".into(),
        },
    };
    (
        RootAddressV0 {
            namespace: "tenant-a".into(),
            name: "app-a".into(),
            state_name,
        },
        command,
        root,
        state,
    )
}

fn retired_state_path(state_name: &str) -> String {
    format!("/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{state_name}")
}

fn state_absent_response() -> Response<Body> {
    status(
        StatusCode::NOT_FOUND,
        json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
    )
}

fn root_only_finalizer_token(
    root_rv: &str,
    root_generation: i64,
    state_name: &str,
    state_uid: &str,
    cleanup_token: &str,
) -> String {
    root_only_finalizer_command_token(
        "root-uid",
        root_generation,
        root_rv,
        state_name,
        state_uid,
        &"d".repeat(64),
        root_generation,
        cleanup_token,
    )
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

fn force_fence_objects() -> (Value, Value, Value) {
    let root = json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication",
        "metadata":{"name":"app-a","namespace":"tenant-a","uid":"root-uid","resourceVersion":"9","generation":1,
            "finalizers":[ROOT_FINALIZER], "annotations":{ROOT_UID_ANNOTATION:"root-uid"}},
        "status":{"state":{"rootUid":"root-uid","name":"state-a","uid":"state-uid"}}
    });
    let state = json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1", "kind":"OxideLoopState",
        "metadata":{"name":"state-a","namespace":"tenant-a","uid":"state-uid","resourceVersion":"4",
            "finalizers":[STATE_FINALIZER], "annotations":{ROOT_UID_ANNOTATION:"root-uid"},
            "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]}
    });
    let child = json!({
        "apiVersion":"v1", "kind":"ConfigMap",
        "metadata":{"name":"cm-a","namespace":"tenant-a","uid":"child-uid","resourceVersion":"7",
            "annotations":{ROOT_UID_ANNOTATION:"root-uid"},
            "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},
        "data":{"old":"value"}
    });
    (root, state, child)
}

fn conditional_apply_command(
    prepared: Option<oxideloop_mutation_model::ObjectIdentity>,
    content_type: &str,
    manager: &str,
    force: bool,
) -> CommandV0 {
    CommandV0::Mutate {
        transaction_id: "tx".into(),
        attempt_token: "a".repeat(64),
        commands: vec![MutationCommandV0 {
            command_id: "c".repeat(64),
            attempt_token: "a".repeat(64),
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
                kind: "ConfigMap".into(),
                namespace: "tenant-a".into(),
                name: "cm-a".into(),
            },
            logical_id: "cm-a".into(),
            prepared_expected_target: prepared,
            decision: MutationDecision::ConditionalApply {
                method: HttpMethod::Patch,
                content_type: content_type.into(),
                field_manager: manager.into(),
                force,
                expected: oxideloop_mutation_model::ObjectIdentity {
                    uid: "child-uid".into(),
                    resource_version: "7".into(),
                },
                body: json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"cm-a","namespace":"tenant-a","uid":"child-uid","resourceVersion":"7"},"data":{"new":"value"}}),
            },
        }],
    }
}

fn force_fence_script(child: Value) -> Vec<Expected> {
    let (root, state, _) = force_fence_objects();
    vec![
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
            response: response(state.clone()),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps/cm-a",
            content_type: None,
            body: None,
            response: response(child),
        },
    ]
}

fn authorized_dispatch_state(
    kind: ResourceKindV0,
    name: &str,
    target_uid: &str,
    target_rv: &str,
    state_rv: &str,
    operation: ApplyOperation,
) -> Value {
    let command_id = "c".repeat(64);
    let attempt_token = "a".repeat(64);
    let transaction_id = "tx";
    let intent = ApplyIntent {
        command_id: command_id.clone(),
        operation,
        identity: ObjectIdentity {
            api_version: "v1".into(),
            kind: kind.kind().into(),
            namespace: "tenant-a".into(),
            name: name.into(),
        },
        logical_id: name.into(),
        expected_uid: Some(target_uid.into()),
        expected_resource_version: Some(target_rv.into()),
    };
    let state_spec = OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: "root-uid".into(),
            bootstrap_id: bootstrap_id("root-uid").expect("bootstrap id"),
            bundle_digest: "b".repeat(64),
        },
        state: StateBody {
            apply_transaction: Some(ApplyTransaction {
                id: transaction_id.into(),
                generation: 1,
                graph_identity_algorithm:
                    oxideloop_controller_api::TransactionGraphIdentityAlgorithm::LegacyV0,
                frozen_render_input_hash: "d".repeat(64),
                desired_graph_hash: "e".repeat(64),
                recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
                attempt_token,
                prepared_intents: vec![intent],
                next_command_lifecycle: oxideloop_controller_api::NextCommandLifecycleV0 {
                    state: NextCommandLifecycleStateV0::DispatchAuthorizedMayHaveIssued,
                    command_id: Some(command_id),
                },
                recovery_claims: vec![PreparedRecoveryClaim {
                    root_uid: "root-uid".into(),
                    transaction_id: transaction_id.into(),
                    command_id: "c".repeat(64),
                }],
                results: vec![],
                unresolved_identities: vec![],
            }),
            pending_persist: Some(PersistOperation {
                token: "f".repeat(64),
                purpose: PersistPurpose::DispatchAuthorization,
                phase: DurablePhase::DispatchAuthorization,
                old_state_resource_version: "3".into(),
                root_generation: 1,
            }),
            ..StateBody::default()
        },
    };
    state_spec
        .validate()
        .expect("authorized state spec validates");
    json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1", "kind":"OxideLoopState",
        "metadata":{"name":"state-a","namespace":"tenant-a","uid":"state-uid","resourceVersion":state_rv,
            "finalizers":[STATE_FINALIZER], "annotations":{
                ROOT_UID_ANNOTATION:"root-uid", TRANSACTION_ID_ANNOTATION:"f".repeat(64), COMMAND_ID_ANNOTATION:"f".repeat(64)},
            "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},
        "spec":serde_json::to_value(state_spec).expect("authorized state spec")
    })
}

fn authorized_force_fence_script(child: Value) -> Vec<Expected> {
    let state = authorized_dispatch_state(
        ResourceKindV0::ConfigMap,
        "cm-a",
        "child-uid",
        "7",
        "4",
        ApplyOperation::Apply,
    );
    authorized_force_fence_script_with_state(child, state)
}

fn authorized_force_fence_script_with_state(child: Value, state: Value) -> Vec<Expected> {
    let mut script = force_fence_script(child);
    script.push(Expected {
        method: Method::GET,
        path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
        content_type: None,
        body: None,
        response: response(state),
    });
    script
}

fn authorized_post_dispatch_state() -> Value {
    let mut state = authorized_dispatch_state(
        ResourceKindV0::ConfigMap,
        "cm-a",
        "unused",
        "unused",
        "4",
        ApplyOperation::Apply,
    );
    let intent = &mut state["spec"]["state"]["applyTransaction"]["preparedIntents"][0];
    intent["expectedUid"] = Value::Null;
    intent["expectedResourceVersion"] = Value::Null;
    state
}

fn force_preconditions() -> PreconditionsV0 {
    PreconditionsV0 {
        root_uid: "root-uid".into(),
        root_generation: 1,
        root_resource_version: "9".into(),
        state_uid: "state-uid".into(),
        state_resource_version: "4".into(),
        observation_token: "o".into(),
        cache_epoch: "e".into(),
    }
}

fn persisted_state() -> OxideLoopStateSpec {
    OxideLoopStateSpec {
        state_version: 1,
        root: RootBinding {
            uid: "root-uid".into(),
            bootstrap_id: bootstrap_id("root-uid").expect("bootstrap id"),
            bundle_digest: "bundle".into(),
        },
        state: StateBody::default(),
    }
}

fn persist_command() -> CommandV0 {
    CommandV0::Persist {
        state: persisted_state(),
        preconditions: force_preconditions(),
        operation: PersistOperation {
            token: "a".repeat(64),
            purpose: PersistPurpose::Prepare,
            phase: DurablePhase::Prepare,
            old_state_resource_version: "4".into(),
            root_generation: 1,
        },
    }
}

fn mutation_result_persist_command() -> CommandV0 {
    let mut command = persist_command();
    let CommandV0::Persist { operation, .. } = &mut command else {
        unreachable!("persist command shape is fixed");
    };
    operation.purpose = PersistPurpose::MutationResult;
    operation.phase = DurablePhase::MutationResult;
    command
}

fn persist_state_patch_body(state: &OxideLoopStateSpec) -> Value {
    json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1", "kind":"OxideLoopState",
        "metadata":{"name":"state-a","namespace":"tenant-a","resourceVersion":"4",
            "annotations":{ROOT_UID_ANNOTATION:"root-uid",TRANSACTION_ID_ANNOTATION:"a".repeat(64),COMMAND_ID_ANNOTATION:"a".repeat(64)}},
        "spec":state,
    })
}

fn root_status_command() -> CommandV0 {
    let token = "b".repeat(64);
    CommandV0::PersistRootStatus {
        token: token.clone(),
        root_uid: "root-uid".into(),
        old_root_resource_version: "9".into(),
        preconditions: force_preconditions(),
        status: WebApplicationStatus {
            last_status_persist_token: Some(token),
            state: Some(StateLink {
                root_uid: "root-uid".into(),
                name: "state-a".into(),
                uid: "state-uid".into(),
            }),
            ..Default::default()
        },
    }
}

fn state_force_script(recovery: Value) -> Vec<Expected> {
    let (root, state, _) = force_fence_objects();
    let body = persist_state_patch_body(&persisted_state());
    vec![
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
            method: Method::PATCH,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a?&force=true&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(body),
            response: status(
                StatusCode::CONFLICT,
                json!({"kind":"Status","apiVersion":"v1","code":409}),
            ),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
            content_type: None,
            body: None,
            response: response(recovery),
        },
    ]
}

/// A state PATCH whose server side may have applied but whose response never
/// arrives. The transport must make one PATCH, then prove the exact operation
/// with its recovery GET; the scripted service remains able to accept that
/// independent GET after the timed-out request future is dropped.
fn timed_out_state_persist_transport(
    recovery: Value,
    hang_recovery_get: bool,
) -> (KubeTransportV0, Arc<AtomicUsize>) {
    let (root, state, _) = force_fence_objects();
    let patches = Arc::new(AtomicUsize::new(0));
    let patch_count = Arc::clone(&patches);
    let recovery = Arc::new(recovery);
    let service = service_fn(move |request: Request<Body>| {
        let patch_count = Arc::clone(&patch_count);
        let recovery = Arc::clone(&recovery);
        let root = root.clone();
        let state = state.clone();
        async move {
            let method = request.method().clone();
            let path = request
                .uri()
                .path_and_query()
                .expect("request URI")
                .as_str()
                .to_owned();
            if method == Method::GET
                && path
                    == "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a"
            {
                return Ok::<_, Infallible>(response(root));
            }
            if method == Method::GET
                && path
                    == "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a"
            {
                let value = if patch_count.load(Ordering::SeqCst) == 0 {
                    state
                } else {
                    if hang_recovery_get {
                        return std::future::pending::<Result<Response<Body>, Infallible>>().await;
                    }
                    (*recovery).clone()
                };
                return Ok(response(value));
            }
            if method == Method::PATCH
                && path
                    == "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a?&force=true&fieldManager=oxideloop-controller"
            {
                patch_count.fetch_add(1, Ordering::SeqCst);
                return std::future::pending::<Result<Response<Body>, Infallible>>().await;
            }
            panic!("unexpected timeout-recovery request {method} {path}");
        }
    });
    (
        KubeTransportV0::test_from_client(Client::new(service, "default")),
        patches,
    )
}

/// A status PATCH whose response never arrives after the server may have
/// applied it.  The next exact GET is the sole proof attempt under the same
/// short test operation budget; a second PATCH would be a protocol failure.
fn timed_out_automation_completion_transport(
    spec: AutomationRunSpec,
    desired: AutomationRunStatus,
    recovery: Value,
) -> (KubeTransportV0, Arc<AtomicUsize>) {
    let patches = Arc::new(AtomicUsize::new(0));
    let patch_count = Arc::clone(&patches);
    let recovery = Arc::new(recovery);
    let initial = automation_response(&spec, &AutomationRunStatus::default(), "4");
    let service = service_fn(move |request: Request<Body>| {
        let patch_count = Arc::clone(&patch_count);
        let recovery = Arc::clone(&recovery);
        let initial = initial.clone();
        let desired = desired.clone();
        async move {
            let method = request.method().clone();
            let path = request
                .uri()
                .path_and_query()
                .expect("request URI")
                .as_str()
                .to_owned();
            if method == Method::GET
                && path
                    == "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a"
            {
                return Ok::<_, Infallible>(response(if patch_count.load(Ordering::SeqCst) == 0 {
                    initial
                } else {
                    (*recovery).clone()
                }));
            }
            if method == Method::PATCH
                && path
                    == "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns/run-a/status?&fieldManager=oxideloop-controller"
            {
                let bytes = request
                    .into_body()
                    .collect_bytes()
                    .await
                    .expect("status body bytes");
                let body = serde_json::from_slice::<Value>(&bytes).expect("status body");
                assert_eq!(
                    body.get("status"),
                    Some(&serde_json::to_value(&desired).expect("status value"))
                );
                patch_count.fetch_add(1, Ordering::SeqCst);
                return std::future::pending::<Result<Response<Body>, Infallible>>().await;
            }
            panic!("unexpected automation timeout request {method} {path}");
        }
    });
    (
        KubeTransportV0::test_from_client(Client::new(service, "default")),
        patches,
    )
}

fn root_status_force_script(recovery: Value) -> Vec<Expected> {
    let (root, state, _) = force_fence_objects();
    let CommandV0::PersistRootStatus {
        status: desired_status,
        ..
    } = root_status_command()
    else {
        unreachable!()
    };
    let body = json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication",
        "metadata":{"name":"app-a","namespace":"tenant-a","resourceVersion":"9"}, "status":desired_status,
    });
    vec![
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
            method: Method::PATCH,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a/status?&force=true&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(body),
            response: status(
                StatusCode::CONFLICT,
                json!({"kind":"Status","apiVersion":"v1","code":409}),
            ),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(recovery),
        },
    ]
}

fn delete_command(
    kind: ResourceKindV0,
    name: &str,
    uid: &str,
    resource_version: &str,
) -> CommandV0 {
    CommandV0::Mutate {
        transaction_id: "tx".into(),
        attempt_token: "a".repeat(64),
        commands: vec![MutationCommandV0 {
            command_id: "c".repeat(64),
            attempt_token: "a".repeat(64),
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
            prepared_expected_target: Some(oxideloop_mutation_model::ObjectIdentity {
                uid: uid.into(),
                resource_version: resource_version.into(),
            }),
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
    authorize: bool,
) -> Vec<Expected> {
    // Delete now has the same pre-write root/owner proof as Apply.  Keep the
    // scripted object canonical so this helper models an owned managed child;
    // mismatch cases deliberately override this baseline.
    let mut target = object_json(kind, name, target_uid, target_resource_version);
    target["metadata"]["annotations"] = json!({ROOT_UID_ANNOTATION:"root-uid"});
    target["metadata"]["ownerReferences"] = json!([{
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication", "name":"app-a",
        "uid":"root-uid", "controller":true, "blockOwnerDeletion":false
    }]);
    let mut script = vec![
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(json!({
                "apiVersion":"demo.oxideloop.dev/v1alpha1",
                "kind":"WebApplication",
                "metadata":{"name":"app-a","namespace":"tenant-a","uid":"root-uid","resourceVersion":"9","generation":1,"finalizers":[ROOT_FINALIZER],"annotations":{"runtime.oxideloop.dev/root-uid":"root-uid"}},
                "status":{"state":{"rootUid":"root-uid","name":"state-a","uid":"state-uid"}}
            })),
        },
        Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
            content_type: None,
            body: None,
            response: response(authorized_dispatch_state(
                kind,
                name,
                target_uid,
                target_resource_version,
                "4",
                ApplyOperation::Delete,
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
            response: response(target),
        },
    ];
    if authorize {
        script.push(Expected {
            method: Method::GET,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
            content_type: None,
            body: None,
            response: response(authorized_dispatch_state(kind, name, target_uid, target_resource_version, "4", ApplyOperation::Delete)),
        });
    }
    script
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
            prepared_expected_target: None,
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
async fn read_conflicts_retain_list_and_get_provenance() {
    let conflict = || {
        status(
            StatusCode::CONFLICT,
            json!({"kind":"Status","apiVersion":"v1","code":409}),
        )
    };
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
        content_type: None,
        body: None,
        response: conflict(),
    }]);
    assert_eq!(
        transport
            .list_exact(ResourceKindV0::ConfigMap, "tenant-a")
            .await,
        Err(TransportErrorV0::Conflict(ConflictProvenanceV0::Read {
            operation: ReadOperationV0::List,
            resource: ResourceKindV0::ConfigMap,
        }))
    );
    api.assert_drained();

    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/api/v1/namespaces/tenant-a/configmaps/cm-a",
        content_type: None,
        body: None,
        response: conflict(),
    }]);
    assert_eq!(
        transport
            .reread(&ObjectRefV0 {
                resource: ResourceKindV0::ConfigMap,
                namespace: "tenant-a".into(),
                name: "cm-a".into(),
            })
            .await,
        Err(TransportErrorV0::Conflict(ConflictProvenanceV0::Read {
            operation: ReadOperationV0::Get,
            resource: ResourceKindV0::ConfigMap,
        }))
    );
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
async fn watch_open_and_stream_conflicts_retain_stage_and_resource() {
    let body =
        serde_json::json!({"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","code":409}})
            .to_string()
            + "\n";
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?&watch=true&timeoutSeconds=290&allowWatchBookmarks=true&resourceVersion=77",
        content_type: None,
        body: None,
        response: Response::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.into_bytes()))
            .expect("watch conflict stream"),
    }]);
    let mut watch = transport
        .watch_from(ResourceKindV0::AutomationRun, "tenant-a", "77")
        .await
        .expect("watch opens");
    let expected = Err(TransportErrorV0::Conflict(
        ConflictProvenanceV0::WatchTerminal {
            stage: WatchStageV0::Stream,
            resource: ResourceKindV0::AutomationRun,
        },
    ));
    assert_eq!(watch.next_event().await, expected);
    assert_eq!(watch.next_event().await, expected);
    api.assert_drained();
}

#[tokio::test]
async fn deletion_barrier_scripted_live_root_watch_invalidates_before_seal() {
    // This keeps the real deletion acquisition/seal algorithm intact.  Only
    // the six retained watch sources are injected, so no test-only
    // no-stream path is exercised.
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let preconditions = PreconditionsV0 {
        root_uid: "root-uid".into(),
        root_generation: 3,
        root_resource_version: "root-rv".into(),
        state_uid: "state-uid".into(),
        state_resource_version: "state-rv".into(),
        observation_token: "token".into(),
        cache_epoch: "epoch".into(),
    };
    let root = json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication",
        "metadata":{"name":"app-a", "namespace":"tenant-a", "uid":"root-uid",
          "resourceVersion":"root-rv", "generation":3,
          "deletionTimestamp":"2026-01-01T00:00:00Z"}
    });
    let state = json!({
        "apiVersion":"runtime.oxideloop.dev/v1alpha1", "kind":"OxideLoopState",
        "metadata":{"name":"state-a", "namespace":"tenant-a", "uid":"state-uid",
          "resourceVersion":"state-rv"}
    });
    let empty = || collection_json(json!([]));
    let scripted = |initial_config: Value, closing_config: Value| {
        ScriptedApi::transport([
            Expected {
                method: Method::GET,
                path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
                content_type: None,
                body: None,
                response: response(initial_config),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/apis/apps/v1/namespaces/tenant-a/deployments?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/api/v1/namespaces/tenant-a/services?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
                content_type: None,
                body: None,
                response: response(root.clone()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
                content_type: None,
                body: None,
                response: response(state.clone()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
                content_type: None,
                body: None,
                response: response(closing_config),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/apis/apps/v1/namespaces/tenant-a/deployments?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/api/v1/namespaces/tenant-a/services?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
            Expected {
                method: Method::GET,
                path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?",
                content_type: None,
                body: None,
                response: response(empty()),
            },
        ])
    };
    let (transport, api) = scripted(empty(), empty());
    let mut senders = Vec::new();
    for _ in 0..6 {
        let (sender, watch) = WatchHandleV0::test_channel(1);
        transport.test_push_watch(watch);
        senders.push(sender);
    }
    let watched_root = ObjectV0 {
        reference: ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: "tenant-a".into(),
            name: "app-a".into(),
        },
        metadata: ObjectMetaV0 {
            uid: "root-uid".into(),
            resource_version: "new-root-rv".into(),
            annotations: BTreeMap::new(),
            owner_uids: vec![],
        },
        value: root.clone(),
    };
    senders[0]
        .send(Ok(WatchEventV0::Modified(watched_root)))
        .await
        .expect("scripted root event is buffered");
    assert_eq!(
        transport
            .observe_deletion_revalidation(&address, &preconditions)
            .await,
        Err(TransportErrorV0::Stale(
            "deletion observation watch changed"
        ))
    );
    api.assert_drained();

    // All root and linked-State changes are authoritative deletion inputs.
    // The filter intentionally does not try to interpret individual fields:
    // a UID/RV/link/receipt/finalizer/owner change must all win Live before a
    // point-in-time snapshot can be sealed.
    for (label, slot, object) in [
        (
            "root UID",
            0,
            ObjectV0 {
                reference: ObjectRefV0 {
                    resource: ResourceKindV0::WebApplication,
                    namespace: "tenant-a".into(),
                    name: "app-a".into(),
                },
                metadata: ObjectMetaV0 {
                    uid: "replacement-root".into(),
                    resource_version: "root-rv".into(),
                    annotations: BTreeMap::new(),
                    owner_uids: vec![],
                },
                value: root.clone(),
            },
        ),
        (
            "root RV/finalizer/receipt/link",
            0,
            ObjectV0 {
                reference: ObjectRefV0 {
                    resource: ResourceKindV0::WebApplication,
                    namespace: "tenant-a".into(),
                    name: "app-a".into(),
                },
                metadata: ObjectMetaV0 {
                    uid: "root-uid".into(),
                    resource_version: "changed-rv".into(),
                    annotations: BTreeMap::new(),
                    owner_uids: vec![],
                },
                value: root.clone(),
            },
        ),
        (
            "State UID/RV/owner/finalizer",
            1,
            ObjectV0 {
                reference: ObjectRefV0 {
                    resource: ResourceKindV0::OxideLoopState,
                    namespace: "tenant-a".into(),
                    name: "state-a".into(),
                },
                metadata: ObjectMetaV0 {
                    uid: "replacement-state".into(),
                    resource_version: "changed-state-rv".into(),
                    annotations: BTreeMap::new(),
                    owner_uids: vec!["other-root".into()],
                },
                value: state.clone(),
            },
        ),
    ] {
        let (transport, api) = scripted(empty(), empty());
        let mut senders = Vec::new();
        for _ in 0..6 {
            let (sender, watch) = WatchHandleV0::test_channel(1);
            transport.test_push_watch(watch);
            senders.push(sender);
        }
        senders[slot]
            .send(Ok(WatchEventV0::Modified(object)))
            .await
            .expect("authoritative scripted watch event is buffered");
        assert_eq!(
            transport
                .observe_deletion_revalidation(&address, &preconditions)
                .await,
            Err(TransportErrorV0::Stale(
                "deletion observation watch changed"
            )),
            "{label} must prevent a deletion snapshot seal"
        );
        api.assert_drained();
    }

    for terminal in [
        TransportErrorV0::WatchGone,
        TransportErrorV0::WatchOverflow,
        TransportErrorV0::Other("scripted terminal".into()),
    ] {
        let (transport, api) = scripted(empty(), empty());
        let mut senders = Vec::new();
        for _ in 0..6 {
            let (sender, watch) = WatchHandleV0::test_channel(1);
            transport.test_push_watch(watch);
            senders.push(sender);
        }
        senders[5]
            .send(Err(terminal.clone()))
            .await
            .expect("terminal scripted watch record is buffered");
        assert_eq!(
            transport
                .observe_deletion_revalidation(&address, &preconditions)
                .await,
            Err(terminal),
            "terminal watch must fail before any deletion acknowledgement"
        );
        api.assert_drained();
    }

    let (overflow_transport, overflow_api) = scripted(empty(), empty());
    let mut overflow_senders = Vec::new();
    for index in 0..6 {
        let (sender, watch) = WatchHandleV0::test_channel(1);
        if index == 3 {
            watch.test_mark_overflow();
        }
        overflow_transport.test_push_watch(watch);
        overflow_senders.push(sender);
    }
    assert_eq!(
        overflow_transport
            .observe_deletion_revalidation(&address, &preconditions)
            .await,
        Err(TransportErrorV0::WatchOverflow)
    );
    drop(overflow_senders);
    overflow_api.assert_drained();

    let (eof_transport, eof_api) = scripted(empty(), empty());
    let mut eof_senders = Vec::new();
    for index in 0..6 {
        let (sender, watch) = WatchHandleV0::test_channel(1);
        eof_transport.test_push_watch(watch);
        if index != 2 {
            eof_senders.push(sender);
        }
        // The ConfigMap sender is deliberately dropped: EOF is terminal.
    }
    assert_eq!(
        eof_transport
            .observe_deletion_revalidation(&address, &preconditions)
            .await,
        Err(TransportErrorV0::WatchGone)
    );
    drop(eof_senders);
    eof_api.assert_drained();

    let tracked = json!({
        "apiVersion":"v1", "kind":"ConfigMap",
        "metadata":{"name":"tracked", "namespace":"tenant-a", "uid":"tracked-old",
          "resourceVersion":"tracked-rv", "ownerReferences":[{
            "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication",
            "name":"app-a", "uid":"root-uid", "controller":true,
            "blockOwnerDeletion":false
          }]}
    });
    let (tracked_transport, tracked_api) = scripted(collection_json(json!([tracked])), empty());
    let mut tracked_senders = Vec::new();
    for _ in 0..6 {
        let (sender, watch) = WatchHandleV0::test_channel(1);
        tracked_transport.test_push_watch(watch);
        tracked_senders.push(sender);
    }
    let replacement = ObjectV0 {
        reference: ObjectRefV0 {
            resource: ResourceKindV0::ConfigMap,
            namespace: "tenant-a".into(),
            name: "tracked".into(),
        },
        metadata: ObjectMetaV0 {
            uid: "foreign-replacement".into(),
            resource_version: "replacement-rv".into(),
            annotations: BTreeMap::new(),
            owner_uids: vec![],
        },
        value: json!({}),
    };
    tracked_senders[2]
        .send(Ok(WatchEventV0::Modified(replacement)))
        .await
        .expect("tracked replacement is buffered");
    assert_eq!(
        tracked_transport
            .observe_deletion_revalidation(&address, &preconditions)
            .await,
        Err(TransportErrorV0::Stale(
            "deletion observation watch changed"
        )),
        "a same-name replacement cannot be treated as a quiet deletion snapshot"
    );
    tracked_api.assert_drained();

    let (quiet_transport, quiet_api) = scripted(empty(), empty());
    let mut quiet_senders = Vec::new();
    for _ in 0..6 {
        let (sender, watch) = WatchHandleV0::test_channel(1);
        quiet_transport.test_push_watch(watch);
        quiet_senders.push(sender);
    }
    let sealed = quiet_transport
        .observe_deletion_revalidation(&address, &preconditions)
        .await
        .expect("quiet exact scripted barrier seals");
    assert_eq!(sealed.root.metadata.uid, "root-uid");
    assert_eq!(sealed.state.metadata.uid, "state-uid");
    drop(quiet_senders);
    quiet_api.assert_drained();
}

#[tokio::test]
async fn root_status_conflict_recovery_distinguishes_ack_stale_and_hold() {
    let status = WebApplicationStatus {
        last_status_persist_token: Some("token".into()),
        ..Default::default()
    };
    let mut root = object_json(ResourceKindV0::WebApplication, "app-a", "root-uid", "100");
    root["metadata"]["annotations"][ROOT_UID_ANNOTATION] = json!("root-uid");
    root["metadata"]["finalizers"] = json!([ROOT_FINALIZER]);
    root["status"] = serde_json::to_value(&status).expect("status JSON");
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let expected = || Expected {
        method: Method::GET,
        path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
        content_type: None,
        body: None,
        response: response(root.clone()),
    };
    let (transport, api) = ScriptedApi::transport([expected()]);
    let acknowledged = transport
        .recover_root_status(&address, "token", "root-uid", "99", &status)
        .await
        .expect("advanced exact status is an acknowledgement");
    assert!(matches!(acknowledged, super::CommandOutcomeV0::Event(_)));
    api.assert_drained();

    let mut mismatched = root.clone();
    mismatched["status"]["cacheEpoch"] = json!(7);
    let (transport, api) = ScriptedApi::transport([Expected {
        response: response(mismatched),
        ..expected()
    }]);
    assert_eq!(
        transport
            .recover_root_status(&address, "token", "root-uid", "99", &status)
            .await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();

    let mut unchanged = root.clone();
    unchanged["metadata"]["resourceVersion"] = json!("99");
    let (transport, api) = ScriptedApi::transport([Expected {
        response: response(unchanged),
        ..expected()
    }]);
    assert_eq!(
        transport
            .recover_root_status(&address, "token", "root-uid", "99", &status)
            .await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
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
async fn watch_decodes_a_complete_automation_run_event_before_stream_end() {
    let spec = automation_spec();
    let expectation = pending_automation_expectation(&spec, "z-old-rv");
    let completed = observed_pending_run(&expectation, &spec, completion_status(), "a-new-rv");
    let watch_body = format!(
        "{}\n",
        serde_json::json!({"type":"MODIFIED","object":completed.value})
    );
    let (transport, api) = ScriptedApi::transport([Expected {
        method: Method::GET,
        path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/automationruns?&watch=true&timeoutSeconds=290&allowWatchBookmarks=true&resourceVersion=99",
        content_type: None,
        body: None,
        response: Response::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(watch_body.into_bytes()))
            .expect("watch response"),
    }]);
    let mut watch = transport
        .watch_from(ResourceKindV0::AutomationRun, "tenant-a", "99")
        .await
        .expect("watch opens");
    assert!(matches!(
        watch.next_event().await,
        Ok(WatchEventV0::Modified(_))
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
        pump_abort: None,
        deletion_filter: None,
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

#[test]
fn pending_automation_authentication_matrix_covers_list_watch_and_exact_get() {
    let spec = automation_spec();
    // These values intentionally run backwards lexically.  Completion accepts
    // only inequality with the durable old RV; Kubernetes RVs are opaque.
    let expectation = pending_automation_expectation(&spec, "z-old-rv");
    let pristine = observed_pending_run(
        &expectation,
        &spec,
        AutomationRunStatus::default(),
        "z-old-rv",
    );
    let completed = observed_pending_run(&expectation, &spec, completion_status(), "a-new-rv");
    let unrelated_malformed = ObjectV0 {
        reference: ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: "tenant-a".into(),
            name: "foreign".into(),
        },
        metadata: ObjectMetaV0 {
            uid: String::new(),
            resource_version: String::new(),
            annotations: BTreeMap::new(),
            owner_uids: vec![],
        },
        value: json!(null),
    };

    let listed = validate_automation_run_list(
        &ListV0 {
            items: vec![unrelated_malformed.clone(), pristine.clone()],
            resource_version: "list-rv".into(),
        },
        &expectation,
    )
    .expect("LIST authenticates the pristine durable run")
    .expect("exact list candidate");
    assert!(listed.is_pristine());
    assert_eq!(listed.program(), &spec.program);

    let got = validate_automation_run_get(Some(pristine.clone()), &expectation)
        .expect("GET authenticates the pristine durable run");
    assert_eq!(got.resource_version(), "z-old-rv");
    let watched =
        validate_automation_watch_event(&WatchEventV0::Modified(completed.clone()), &expectation)
            .expect("WATCH authenticates completion")
            .expect("exact watch candidate");
    let completion = watched.completion().expect("completed status");
    assert_eq!(completion.old_resource_version(), "z-old-rv");
    assert_eq!(completion.new_resource_version(), "a-new-rv");
    assert_eq!(
        completion.completion(),
        AutomationRunCompletionState::Succeeded
    );

    assert_eq!(
        validate_automation_run_observation(&unrelated_malformed, &expectation),
        Ok(None),
        "unrelated malformed runs are ignored"
    );
    let mut deleting = pristine.clone();
    deleting.value["metadata"]["deletionTimestamp"] = json!("2026-08-30T00:00:00Z");
    assert!(matches!(
        validate_automation_run_list(
            &ListV0 {
                items: vec![deleting.clone()],
                resource_version: "list-rv".into(),
            },
            &expectation,
        ),
        Err(TransportErrorV0::AutomationObservationHold(_)),
    ));
    assert!(matches!(
        validate_automation_run_get(Some(deleting.clone()), &expectation),
        Err(TransportErrorV0::AutomationObservationHold(_)),
    ));
    for event in [
        WatchEventV0::Added(deleting.clone()),
        WatchEventV0::Modified(deleting),
    ] {
        assert!(matches!(
            validate_automation_watch_event(&event, &expectation),
            Err(TransportErrorV0::AutomationObservationHold(_)),
        ));
    }
    assert!(matches!(
        validate_automation_watch_event(&WatchEventV0::Deleted(pristine), &expectation),
        Err(TransportErrorV0::AutomationObservationHold(_)),
    ));
    assert!(matches!(
        validate_automation_run_get(None, &expectation),
        Err(TransportErrorV0::AutomationObservationHold(_)),
    ));

    let mut wrong_spec = completed;
    wrong_spec.value["spec"]["automationId"] = json!("retargeted");
    assert!(
        matches!(
            validate_automation_run_observation(&wrong_spec, &expectation),
            Err(TransportErrorV0::AutomationObservationHold(_))
        ),
        "same-name immutable spec mismatches hold"
    );
}

#[tokio::test]
async fn pending_automation_list_watch_reread_retains_a_race_completion() {
    let spec = automation_spec();
    let expectation = pending_automation_expectation(&spec, "z-old-rv");
    let pristine = observed_pending_run(
        &expectation,
        &spec,
        AutomationRunStatus::default(),
        "z-old-rv",
    );
    let completed = observed_pending_run(&expectation, &spec, completion_status(), "a-new-rv");
    validate_automation_run_list(
        &ListV0 {
            items: vec![pristine.clone()],
            resource_version: "list-rv".into(),
        },
        &expectation,
    )
    .expect("LIST leg authenticates the durable candidate");
    // This is the already-buffered WATCH leg that arrives between the exact
    // GET and the caller taking ownership of the retained barrier.
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    sender
        .send(Ok(WatchEventV0::Modified(completed)))
        .await
        .expect("buffered watch event");
    let current = validate_automation_run_get(Some(pristine), &expectation)
        .expect("exact reread before watch drain");
    let mut observer = AutomationRunObservationBarrierV0 {
        expectation,
        watch: WatchHandleV0 {
            events: receiver,
            overflowed: Arc::new(AtomicBool::new(false)),
            terminal: None,
            pump_abort: None,
            deletion_filter: None,
        },
        current,
    };
    let changed = observer
        .next_change()
        .await
        .expect("buffered completion survives the list-watch race");
    assert_eq!(changed.resource_version(), "a-new-rv");
    assert!(changed.completion().is_some());
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
        .patch_config_map(&config_map, "42", false)
        .await
        .expect("SSA config map");
    transport
        .patch_web_application_status(&root, "99", false)
        .await
        .expect("status patch");
    transport
        .delete_config_map(&config_map.reference, "cm-uid", "43")
        .await
        .expect("delete");
    api.assert_drained();
}

#[tokio::test]
async fn forced_apply_is_limited_to_an_explicit_authorization() {
    let config_map = desired(ResourceKindV0::ConfigMap, "cm-a", "42");
    let state = desired(ResourceKindV0::OxideLoopState, "state-a", "4");
    let mut root = desired(ResourceKindV0::WebApplication, "app-a", "9");
    root.value = json!({
        "apiVersion":"demo.oxideloop.dev/v1alpha1", "kind":"WebApplication",
        "metadata":{"name":"app-a","namespace":"tenant-a"}, "status":{}
    });
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::PATCH,
            path_and_query: "/api/v1/namespaces/tenant-a/configmaps/cm-a?&force=true&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(
                json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"cm-a","namespace":"tenant-a","resourceVersion":"42"},"spec":{"desired":true}}),
            ),
            response: response(object_json(
                ResourceKindV0::ConfigMap,
                "cm-a",
                "child",
                "43",
            )),
        },
        Expected {
            method: Method::PATCH,
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a?&force=true&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(
                json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"OxideLoopState","metadata":{"name":"state-a","namespace":"tenant-a","resourceVersion":"4"},"spec":{"desired":true}}),
            ),
            response: response(object_json(
                ResourceKindV0::OxideLoopState,
                "state-a",
                "state",
                "5",
            )),
        },
        Expected {
            method: Method::PATCH,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a/status?&force=true&fieldManager=oxideloop-controller",
            content_type: Some("application/apply-patch+yaml"),
            body: Some(
                json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"app-a","namespace":"tenant-a","resourceVersion":"9"},"status":{}}),
            ),
            response: response(object_json(
                ResourceKindV0::WebApplication,
                "app-a",
                "root",
                "10",
            )),
        },
    ]);
    transport
        .patch_config_map(&config_map, "42", true)
        .await
        .unwrap();
    transport.patch_state(&state, "4", true).await.unwrap();
    transport
        .patch_web_application_status(&root, "9", true)
        .await
        .unwrap();
    api.assert_drained();
}

#[tokio::test]
async fn conditional_apply_authority_mismatches_issue_no_patch() {
    let (_, _, child) = force_fence_objects();
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let cases = [
        (
            None,
            "application/apply-patch+yaml",
            "oxideloop-controller",
            true,
        ),
        (
            Some(oxideloop_mutation_model::ObjectIdentity {
                uid: "wrong".into(),
                resource_version: "7".into(),
            }),
            "application/apply-patch+yaml",
            "oxideloop-controller",
            true,
        ),
        (
            Some(oxideloop_mutation_model::ObjectIdentity {
                uid: "child-uid".into(),
                resource_version: "7".into(),
            }),
            "application/merge-patch+json",
            "oxideloop-controller",
            true,
        ),
        (
            Some(oxideloop_mutation_model::ObjectIdentity {
                uid: "child-uid".into(),
                resource_version: "7".into(),
            }),
            "application/apply-patch+yaml",
            "other",
            true,
        ),
        (
            Some(oxideloop_mutation_model::ObjectIdentity {
                uid: "child-uid".into(),
                resource_version: "7".into(),
            }),
            "application/apply-patch+yaml",
            "oxideloop-controller",
            false,
        ),
    ];
    for (prepared, content_type, manager, force) in cases {
        let (transport, api) = ScriptedApi::transport([]);
        assert!(matches!(
            transport
                .dispatch(
                    &address,
                    &conditional_apply_command(prepared, content_type, manager, force)
                )
                .await,
            Ok(super::CommandOutcomeV0::Event(_))
        ));
        api.assert_drained();
    }
    let mut target_pin_mismatch = child.clone();
    target_pin_mismatch["metadata"]["resourceVersion"] = json!("8");
    let (transport, api) = ScriptedApi::transport(force_fence_script(target_pin_mismatch));
    assert!(matches!(
        transport
            .dispatch(
                &address,
                &conditional_apply_command(
                    Some(oxideloop_mutation_model::ObjectIdentity {
                        uid: "child-uid".into(),
                        resource_version: "7".into(),
                    }),
                    "application/apply-patch+yaml",
                    "oxideloop-controller",
                    true,
                ),
            )
            .await,
        Ok(super::CommandOutcomeV0::Event(_))
    ));
    api.assert_drained();
    let mut foreign_owner = child;
    foreign_owner["metadata"]["ownerReferences"][0]["uid"] = json!("foreign-root");
    // Owner validation is a definitive pre-write check, so the final State
    // authorization reread must not occur for this near miss.
    let (transport, api) = ScriptedApi::transport(force_fence_script(foreign_owner));
    assert!(matches!(
        transport
            .dispatch(
                &address,
                &conditional_apply_command(
                    Some(oxideloop_mutation_model::ObjectIdentity {
                        uid: "child-uid".into(),
                        resource_version: "7".into(),
                    }),
                    "application/apply-patch+yaml",
                    "oxideloop-controller",
                    true,
                ),
            )
            .await,
        Ok(super::CommandOutcomeV0::Event(_))
    ));
    api.assert_drained();
}

#[tokio::test]
async fn dispatch_authorization_mismatches_hold_before_any_child_write() {
    let (_, _, child) = force_fence_objects();
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let command = conditional_apply_command(
        Some(oxideloop_mutation_model::ObjectIdentity {
            uid: "child-uid".into(),
            resource_version: "7".into(),
        }),
        "application/apply-patch+yaml",
        "oxideloop-controller",
        true,
    );
    let base = authorized_dispatch_state(
        ResourceKindV0::ConfigMap,
        "cm-a",
        "child-uid",
        "7",
        "4",
        ApplyOperation::Apply,
    );
    let mut cases = Vec::new();
    let mut transaction = base.clone();
    transaction["spec"]["state"]["applyTransaction"]["id"] = json!("other-tx");
    cases.push(transaction);
    let mut attempt = base.clone();
    attempt["spec"]["state"]["applyTransaction"]["attemptToken"] = json!("b".repeat(64));
    cases.push(attempt);
    let mut lifecycle = base.clone();
    lifecycle["spec"]["state"]["applyTransaction"]["nextCommandLifecycle"]["state"] =
        json!("PreparedNotIssued");
    cases.push(lifecycle);
    let mut command_id = base.clone();
    command_id["spec"]["state"]["applyTransaction"]["nextCommandLifecycle"]["commandId"] =
        json!("d".repeat(64));
    cases.push(command_id);
    let mut receipt = base.clone();
    receipt["spec"]["state"]["applyTransaction"]["recoveryClaims"][0]["commandId"] =
        json!("d".repeat(64));
    cases.push(receipt);
    let mut root_pin = base.clone();
    root_pin["spec"]["root"]["uid"] = json!("foreign-root");
    cases.push(root_pin);
    let mut purpose = base.clone();
    purpose["spec"]["state"]["pendingPersist"]["purpose"] = json!("Prepare");
    purpose["spec"]["state"]["pendingPersist"]["phase"] = json!("Prepare");
    cases.push(purpose);
    let mut token = base.clone();
    token["metadata"]["annotations"][TRANSACTION_ID_ANNOTATION] = json!("0".repeat(64));
    cases.push(token);
    let mut state_uid = base.clone();
    state_uid["metadata"]["uid"] = json!("foreign-state");
    cases.push(state_uid);
    let mut state_rv = base.clone();
    state_rv["metadata"]["resourceVersion"] = json!("5");
    cases.push(state_rv);

    let (_, missing_auth, _) = force_fence_objects();
    let (transport, api) = ScriptedApi::transport(authorized_force_fence_script_with_state(
        child.clone(),
        missing_auth,
    ));
    assert_eq!(
        transport.dispatch(&address, &command).await,
        Err(super::TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();

    for state in cases {
        let (transport, api) = ScriptedApi::transport(authorized_force_fence_script_with_state(
            child.clone(),
            state,
        ));
        assert_eq!(
            transport.dispatch(&address, &command).await,
            Err(super::TransportErrorV0::ManualRecoveryHold)
        );
        api.assert_drained();
    }
}

#[tokio::test]
async fn managed_create_response_loss_never_adopts_an_exact_copied_replacement() {
    let (root, state, _) = force_fence_objects();
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let mut command = conditional_apply_command(
        None,
        "application/apply-patch+yaml",
        "oxideloop-controller",
        true,
    );
    let CommandV0::Mutate { commands, .. } = &mut command else {
        unreachable!()
    };
    let body = json!({
        "apiVersion":"v1",
        "kind":"ConfigMap",
        "metadata":{"name":"cm-a","namespace":"tenant-a"},
        "data":{"new":"value"}
    });
    commands[0].decision = MutationDecision::Post {
        method: HttpMethod::Post,
        body: body.clone(),
    };
    let mut expected = vec![
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
            path_and_query: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/state-a",
            content_type: None,
            body: None,
            response: response(authorized_post_dispatch_state()),
        },
    ];
    let expected_body = json!({
        "apiVersion":"v1",
        "kind":"ConfigMap",
        "metadata":{
            "name":"cm-a",
            "namespace":"tenant-a",
            "annotations":{
                ROOT_UID_ANNOTATION:"root-uid",
                TRANSACTION_ID_ANNOTATION:"tx",
                COMMAND_ID_ANNOTATION:"c".repeat(64)
            },
            "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}
            ]
        },
        "data":{"new":"value"}
    });
    expected.push(Expected {
        method: Method::POST,
        path_and_query: "/api/v1/namespaces/tenant-a/configmaps?",
        content_type: Some("application/json"),
        body: Some(expected_body),
        response: lost_response(),
    });
    let (transport, api) = ScriptedApi::transport(expected);
    assert_eq!(
        transport.dispatch(&address, &command).await,
        Err(TransportErrorV0::ResponseLost)
    );
    // Even if a same-name object with the exact desired body appeared after
    // the lost POST, no reread or replay can turn it into an acknowledgement.
    api.assert_drained();
}

#[tokio::test]
async fn conditional_apply_409_rereads_once_and_only_acks_exact_prior_success() {
    let (_, _, child) = force_fence_objects();
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let command = conditional_apply_command(
        Some(oxideloop_mutation_model::ObjectIdentity {
            uid: "child-uid".into(),
            resource_version: "7".into(),
        }),
        "application/apply-patch+yaml",
        "oxideloop-controller",
        true,
    );
    let patch_body = json!({
        "apiVersion":"v1", "kind":"ConfigMap",
        "metadata":{"name":"cm-a","namespace":"tenant-a","resourceVersion":"7",
            "annotations":{ROOT_UID_ANNOTATION:"root-uid",TRANSACTION_ID_ANNOTATION:"tx",COMMAND_ID_ANNOTATION:"c".repeat(64)},
            "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},
        "data":{"new":"value"}
    });
    let _recovered = json!({
        "apiVersion":"v1", "kind":"ConfigMap",
        "metadata":{"name":"cm-a","namespace":"tenant-a","uid":"child-uid","resourceVersion":"8",
            "annotations":{ROOT_UID_ANNOTATION:"root-uid",TRANSACTION_ID_ANNOTATION:"tx",COMMAND_ID_ANNOTATION:"c".repeat(64)},
            "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},
        "data":{"new":"value"}
    });
    let mut exact = authorized_force_fence_script(child.clone());
    exact.push(Expected {
        method:Method::PATCH,
        path_and_query:"/api/v1/namespaces/tenant-a/configmaps/cm-a?&force=true&fieldManager=oxideloop-controller",
        content_type:Some("application/apply-patch+yaml"), body:Some(patch_body.clone()),
        response:status(StatusCode::CONFLICT, json!({"kind":"Status","apiVersion":"v1","code":409})),
    });
    let (transport, api) = ScriptedApi::transport(exact);
    let outcome = transport.dispatch(&address, &command).await.unwrap();
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(
                event.as_ref(),
                oxideloop_controller_core::EventV0::MutationNotIssued {
                    transaction_id,
                    attempt_token,
                    command_id,
                    ..
                } if transaction_id == "tx" && attempt_token == &"a".repeat(64) && *command_id == "c".repeat(64)
            )
    ));
    api.assert_drained();

    let mut mismatch = authorized_force_fence_script(child);
    mismatch.push(Expected {
        method:Method::PATCH,
        path_and_query:"/api/v1/namespaces/tenant-a/configmaps/cm-a?&force=true&fieldManager=oxideloop-controller",
        content_type:Some("application/apply-patch+yaml"), body:Some(patch_body),
        response:status(StatusCode::CONFLICT, json!({"kind":"Status","apiVersion":"v1","code":409})),
    });
    let (transport, api) = ScriptedApi::transport(mismatch);
    let outcome = transport
        .dispatch(&address, &command)
        .await
        .expect("conflict becomes a reducer event");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(
                event.as_ref(),
                oxideloop_controller_core::EventV0::MutationNotIssued {
                    transaction_id,
                    attempt_token,
                    ..
                } if transaction_id == "tx" && attempt_token == &"a".repeat(64)
            )
    ));
    api.assert_drained();
}

#[tokio::test]
async fn persist_state_409_rereads_once_and_requires_an_exact_advanced_proof() {
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let (_, mut recovered, _) = force_fence_objects();
    recovered["metadata"]["resourceVersion"] = json!("5");
    recovered["metadata"]["annotations"][TRANSACTION_ID_ANNOTATION] = json!("a".repeat(64));
    recovered["metadata"]["annotations"][COMMAND_ID_ANNOTATION] = json!("a".repeat(64));
    recovered["spec"] = serde_json::to_value(persisted_state()).unwrap();
    let (transport, api) = ScriptedApi::transport(state_force_script(recovered));
    assert!(matches!(
        transport.dispatch(&address, &persist_command()).await,
        Ok(super::CommandOutcomeV0::Event(event))
            if matches!(event.as_ref(), oxideloop_controller_core::EventV0::Persisted { .. })
    ));
    api.assert_drained();

    let (_, unchanged, _) = force_fence_objects();
    let (transport, api) = ScriptedApi::transport(state_force_script(unchanged));
    assert_eq!(
        transport.dispatch(&address, &persist_command()).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();

    let (_, mut divergent, _) = force_fence_objects();
    divergent["metadata"]["resourceVersion"] = json!("5");
    divergent["metadata"]["annotations"][TRANSACTION_ID_ANNOTATION] = json!("a".repeat(64));
    divergent["metadata"]["annotations"][COMMAND_ID_ANNOTATION] = json!("a".repeat(64));
    divergent["spec"] = json!({"divergent":true});
    let (transport, api) = ScriptedApi::transport(state_force_script(divergent));
    assert_eq!(
        transport.dispatch(&address, &persist_command()).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();
}

#[tokio::test]
async fn timed_out_state_persist_proves_once_or_fails_closed_without_replay() {
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let (_, mut exact, _) = force_fence_objects();
    exact["metadata"]["resourceVersion"] = json!("5");
    exact["metadata"]["annotations"][TRANSACTION_ID_ANNOTATION] = json!("a".repeat(64));
    exact["metadata"]["annotations"][COMMAND_ID_ANNOTATION] = json!("a".repeat(64));
    exact["spec"] = serde_json::to_value(persisted_state()).expect("persisted spec serializes");
    let (transport, patches) = timed_out_state_persist_transport(exact, false);
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        transport.dispatch(&address, &persist_command()),
    )
    .await
    .expect("the bounded state PATCH reaches recovery")
    .expect("exact recovery proof becomes Persisted");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(event.as_ref(), oxideloop_controller_core::EventV0::Persisted { .. })
    ));
    assert_eq!(
        patches.load(Ordering::SeqCst),
        1,
        "a timed-out durable PATCH is never replayed"
    );

    for recovery in [
        force_fence_objects().1,
        {
            let (_, mut foreign, _) = force_fence_objects();
            foreign["metadata"]["resourceVersion"] = json!("5");
            foreign["metadata"]["uid"] = json!("foreign-state-uid");
            foreign
        },
        {
            let (_, mut divergent, _) = force_fence_objects();
            divergent["metadata"]["resourceVersion"] = json!("5");
            divergent["metadata"]["annotations"][TRANSACTION_ID_ANNOTATION] = json!("a".repeat(64));
            divergent["metadata"]["annotations"][COMMAND_ID_ANNOTATION] = json!("a".repeat(64));
            divergent["spec"] = json!({"divergent": true});
            divergent
        },
    ] {
        let (transport, patches) = timed_out_state_persist_transport(recovery, false);
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                transport.dispatch(&address, &persist_command()),
            )
            .await
            .expect("the bounded state PATCH reaches fail-closed recovery"),
            Err(TransportErrorV0::ManualRecoveryHold)
        );
        assert_eq!(
            patches.load(Ordering::SeqCst),
            1,
            "unchanged, foreign, or divergent recovery evidence cannot trigger a second PATCH"
        );
    }
}

#[tokio::test]
async fn timed_out_state_persist_hanging_recovery_get_holds_without_replay() {
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let (_, recovery, _) = force_fence_objects();
    let (transport, patches) = timed_out_state_persist_transport(recovery, true);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            transport.dispatch(&address, &persist_command()),
        )
        .await
        .expect("bounded recovery GET cannot pin dispatch"),
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    assert_eq!(
        patches.load(Ordering::SeqCst),
        1,
        "a recovery-GET timeout must not replay the ambiguous state PATCH"
    );
}

#[tokio::test]
async fn timed_out_mutation_result_persist_recovers_once_without_replaying_patch() {
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let (_, mut exact, _) = force_fence_objects();
    exact["metadata"]["resourceVersion"] = json!("5");
    exact["metadata"]["annotations"][TRANSACTION_ID_ANNOTATION] = json!("a".repeat(64));
    exact["metadata"]["annotations"][COMMAND_ID_ANNOTATION] = json!("a".repeat(64));
    exact["spec"] = serde_json::to_value(persisted_state()).expect("persisted spec serializes");
    let (transport, patches) = timed_out_state_persist_transport(exact, false);
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        transport.dispatch(&address, &mutation_result_persist_command()),
    )
    .await
    .expect("the bounded MutationResult PATCH reaches recovery")
    .expect("exact MutationResult recovery proof becomes Persisted");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(event.as_ref(), oxideloop_controller_core::EventV0::Persisted { .. })
    ));
    assert_eq!(
        patches.load(Ordering::SeqCst),
        1,
        "a timed-out MutationResult PATCH is never replayed"
    );
}

#[tokio::test]
async fn persist_root_status_409_rereads_once_and_requires_an_exact_advanced_proof() {
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let CommandV0::PersistRootStatus { status, .. } = root_status_command() else {
        unreachable!()
    };
    let (mut recovered, _, _) = force_fence_objects();
    recovered["metadata"]["resourceVersion"] = json!("10");
    recovered["status"] = serde_json::to_value(&status).unwrap();
    let (transport, api) = ScriptedApi::transport(root_status_force_script(recovered));
    assert!(matches!(
        transport.dispatch(&address, &root_status_command()).await,
        Ok(super::CommandOutcomeV0::Event(event))
            if matches!(event.as_ref(), oxideloop_controller_core::EventV0::RootStatusPersisted { .. })
    ));
    api.assert_drained();

    let (unchanged, _, _) = force_fence_objects();
    let (transport, api) = ScriptedApi::transport(root_status_force_script(unchanged));
    assert_eq!(
        transport.dispatch(&address, &root_status_command()).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();

    let (mut divergent, _, _) = force_fence_objects();
    divergent["metadata"]["resourceVersion"] = json!("10");
    divergent["status"]["lastStatusPersistToken"] = json!("other");
    let (transport, api) = ScriptedApi::transport(root_status_force_script(divergent));
    assert_eq!(
        transport.dispatch(&address, &root_status_command()).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
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
        let mut expected = delete_fence_script(kind, name, uid, resource_version, true);
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
        false,
    ));
    let outcome = transport
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
                "prepared-rv",
            ),
        )
        .await
        .expect("pre-write target mismatch emits a closed no-write event");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(event.as_ref(), oxideloop_controller_core::EventV0::MutationNotIssued {
                transaction_id, attempt_token, reason, ..
            } if transaction_id == "tx" && attempt_token == &"a".repeat(64)
                && reason == "mutation delete pins")
    ));
    api.assert_drained();
}

#[tokio::test]
async fn managed_owner_and_root_near_misses_are_correlated_no_write_events() {
    let address = RootAddressV0 {
        namespace: "tenant-a".into(),
        name: "app-a".into(),
        state_name: "state-a".into(),
    };
    let (_, _, child) = force_fence_objects();
    for target in [
        {
            let mut target = child.clone();
            target["metadata"]["ownerReferences"][0]["uid"] = json!("other-root");
            target
        },
        {
            let mut target = child.clone();
            target["metadata"]["annotations"][ROOT_UID_ANNOTATION] = json!("other-root");
            target
        },
    ] {
        let (transport, api) = ScriptedApi::transport(force_fence_script(target));
        let outcome = transport
            .dispatch(
                &address,
                &conditional_apply_command(
                    Some(oxideloop_mutation_model::ObjectIdentity {
                        uid: "child-uid".into(),
                        resource_version: "7".into(),
                    }),
                    "application/apply-patch+yaml",
                    "oxideloop-controller",
                    true,
                ),
            )
            .await
            .expect("pre-write mismatch is an event");
        assert!(matches!(outcome, super::CommandOutcomeV0::Event(event)
            if matches!(event.as_ref(), oxideloop_controller_core::EventV0::MutationNotIssued {
                transaction_id, attempt_token, command_id, ..
            } if transaction_id == "tx" && attempt_token == &"a".repeat(64) && *command_id == "c".repeat(64))));
        api.assert_drained();
    }

    for mutate in [
        |target: &mut Value| target["metadata"]["ownerReferences"][0]["uid"] = json!("other-root"),
        |target: &mut Value| {
            target["metadata"]["annotations"][ROOT_UID_ANNOTATION] = json!("other-root");
        },
    ] {
        let mut script = delete_fence_script(
            ResourceKindV0::ConfigMap,
            "cm-a",
            "prepared-uid",
            "prepared-rv",
            false,
        );
        let mut target = object_json(
            ResourceKindV0::ConfigMap,
            "cm-a",
            "prepared-uid",
            "prepared-rv",
        );
        target["metadata"]["annotations"] = json!({ROOT_UID_ANNOTATION:"root-uid"});
        target["metadata"]["ownerReferences"] = json!([{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app-a","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]);
        mutate(&mut target);
        script[2].response = response(target);
        let (transport, api) = ScriptedApi::transport(script);
        let outcome = transport
            .dispatch(
                &address,
                &delete_command(
                    ResourceKindV0::ConfigMap,
                    "cm-a",
                    "prepared-uid",
                    "prepared-rv",
                ),
            )
            .await
            .expect("pre-write mismatch event");
        assert!(matches!(outcome, super::CommandOutcomeV0::Event(event)
            if matches!(event.as_ref(), oxideloop_controller_core::EventV0::MutationNotIssued {
                transaction_id, attempt_token, command_id, ..
            } if transaction_id == "tx" && attempt_token == &"a".repeat(64) && *command_id == "c".repeat(64))));
        api.assert_drained();
    }
}

#[tokio::test]
async fn root_only_verify_uses_receipt_state_identity_and_empty_state_rv() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let token =
        root_only_owned_children_verify_token(&"d".repeat(64), 3, "root-uid", 3, "9", "state-uid");
    let receipt_cleanup_token = "d".repeat(64);
    let state_path = format!(
        "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{state_name}"
    );
    let (transport, api) = ScriptedApi::transport([
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root_only_json(
                "9",
                3,
                &state_name,
                "state-uid",
                &receipt_cleanup_token,
            )),
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
            path_and_query: "/apis/apps/v1/namespaces/tenant-a/deployments?",
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
                && actual_cleanup == &receipt_cleanup_token
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
    let receipt_cleanup_token = "d".repeat(64);
    let token = root_only_finalizer_token("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    let state_path = format!(
        "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{state_name}"
    );
    let root = root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    let patch = json!([
        {"op":"test","path":"/metadata/uid","value":"root-uid"},
        {"op":"test","path":"/metadata/resourceVersion","value":"9"},
        {"op":"test","path":"/metadata/finalizers/0","value":ROOT_FINALIZER},
        {"op":"remove","path":"/metadata/finalizers/0"}
    ]);
    let mut written_root = root.clone();
    written_root["metadata"]["resourceVersion"] = json!("10");
    written_root["metadata"]
        .as_object_mut()
        .expect("root metadata")
        .remove("finalizers");
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
            path_and_query: "/apis/apps/v1/namespaces/tenant-a/deployments?",
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
            response: response(written_root),
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
async fn root_only_current_command_tokens_are_checked_independently_of_the_receipt() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let receipt_cleanup_token = "e".repeat(64);
    let commands = [
        CommandV0::VerifyOwnedChildrenAbsent {
            token: receipt_cleanup_token.clone(),
            preconditions: root_only_preconditions("9", 3, "state-uid"),
        },
        CommandV0::RemoveRootFinalizer {
            token: receipt_cleanup_token.clone(),
            preconditions: root_only_preconditions("9", 3, "state-uid"),
        },
    ];
    for command in commands {
        let (transport, api) = ScriptedApi::transport([Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root_only_json(
                "9",
                3,
                &state_name,
                "state-uid",
                &receipt_cleanup_token,
            )),
        }]);
        assert_eq!(
            transport
                .dispatch(
                    &RootAddressV0 {
                        namespace: "tenant-a".into(),
                        name: "app-a".into(),
                        state_name: state_name.clone(),
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
async fn root_only_remove_holds_for_state_or_child_reappearance_without_patch() {
    let state_name = oxideloop_controller_api::state_name("root-uid").expect("state name");
    let receipt_cleanup_token = "d".repeat(64);
    let token = root_only_finalizer_token("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    let state_path = format!(
        "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/tenant-a/oxideloopstates/{state_name}"
    );
    let root = root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
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
    let receipt_cleanup_token = "d".repeat(64);
    let token = root_only_finalizer_token("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    let stale_rv = root_only_json("10", 3, &state_name, "state-uid", &receipt_cleanup_token);
    let mut not_deleting = root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    not_deleting["metadata"]
        .as_object_mut()
        .expect("metadata")
        .remove("deletionTimestamp");
    let mut no_receipt = root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    no_receipt["status"]
        .as_object_mut()
        .expect("status")
        .remove("deletionCleanupReceipt");
    let mut receipt_root_mismatch =
        root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    receipt_root_mismatch["status"]["deletionCleanupReceipt"]["rootUid"] = json!("other-root");
    let wrong_state_name =
        root_only_json("9", 3, "wrong-state", "state-uid", &receipt_cleanup_token);
    let wrong_state_uid =
        root_only_json("9", 3, &state_name, "other-state", &receipt_cleanup_token);
    let mut no_finalizer = root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    no_finalizer["metadata"]["finalizers"] = json!([]);
    let mut wrong_root_kind =
        root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    wrong_root_kind["kind"] = json!("Service");
    let mut wrong_root_name =
        root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    wrong_root_name["metadata"]["name"] = json!("other");
    let mut wrong_root_namespace =
        root_only_json("9", 3, &state_name, "state-uid", &receipt_cleanup_token);
    wrong_root_namespace["metadata"]["namespace"] = json!("tenant-b");
    let cases = [
        stale_rv,
        not_deleting,
        no_receipt,
        receipt_root_mismatch,
        wrong_state_name,
        wrong_state_uid,
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
async fn event_create_rejects_preexisting_name_and_uses_only_get_or_post() {
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
            .await,
        Err(super::TransportErrorV0::ForeignCollision)
    );
    api.assert_drained();
}

#[tokio::test]
async fn event_prewrite_failure_reuses_its_frozen_request_in_the_next_epoch() {
    let plan = generated_event_plan();
    let path = format!(
        "/apis/events.k8s.io/v1/namespaces/tenant-a/events/{}",
        plan.metadata.name
    );
    let desired = materialize_event(&plan, "2026-01-01T00:00:00Z").expect("event body");
    let missing = || {
        status(
            StatusCode::NOT_FOUND,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
        )
    };
    let (transport, api) = ScriptedApi::transport([
        // A pre-write GET failure is retryable only inside the original
        // operation.  The following exact GET and POST prove the retry did
        // not mint or alter the generated Event request.
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"kind":"Status","apiVersion":"v1","status":"Failure","code":500}),
            ),
        },
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
            response: response(event_response(&plan, "event-uid", "9")),
        },
    ]);

    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await
            .expect("one retry retains the Event request")
            .resource_version,
        "9"
    );
    api.assert_drained();
}

#[tokio::test]
async fn collected_event_invalidation_during_preflight_prevents_a_post() {
    let plan = generated_event_plan();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let gate = Arc::new(Mutex::new(None::<super::PrewriteGateV0>));
    let service_gate = Arc::clone(&gate);
    let service = service_fn(move |request: Request<Body>| {
        let seen = Arc::clone(&seen);
        let service_gate = Arc::clone(&service_gate);
        async move {
            assert_eq!(
                seen.fetch_add(1, Ordering::SeqCst),
                0,
                "no retry GET or POST"
            );
            assert_eq!(request.method(), Method::GET);
            assert!(
                service_gate
                    .lock()
                    .expect("gate lock")
                    .as_ref()
                    .expect("collector gate installed")
                    .invalidate_authenticated_change(),
                "the buffered fact must win before retry"
            );
            Ok::<_, Infallible>(status(
                StatusCode::NOT_FOUND,
                json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
            ))
        }
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
    let operation = transport.begin_collected_operation_session();
    *gate.lock().expect("gate lock") = Some(operation.prewrite_gate().clone());

    assert_eq!(
        transport
            .create_event_in_operation_session(&plan, "2026-01-01T00:00:00Z", &operation)
            .await,
        Err(TransportErrorV0::PreWriteInvalidated)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the fact prevents the POST"
    );
}

#[tokio::test]
async fn event_lost_create_never_adopts_a_reread() {
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
    ]);
    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await,
        Err(super::TransportErrorV0::ResponseLost)
    );
    api.assert_drained();

    // A foreign same-name response is intentionally never reread after POST.
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
    ]);
    assert_eq!(
        transport
            .create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z")
            .await,
        Err(TransportErrorV0::ResponseLost)
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
async fn timed_out_event_post_is_terminal_without_a_proof_get_or_replay() {
    let plan = generated_event_plan();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let service = service_fn(move |request: Request<Body>| {
        let seen = Arc::clone(&seen);
        async move {
            let ordinal = seen.fetch_add(1, Ordering::SeqCst);
            let method = request.method().clone();
            if ordinal == 0 {
                assert_eq!(method, Method::GET);
                return Ok::<_, Infallible>(status(
                    StatusCode::NOT_FOUND,
                    json!({"kind":"Status","apiVersion":"v1","reason":"NotFound","code":404}),
                ));
            }
            assert_eq!(
                ordinal, 1,
                "a timed-out Event POST must not reread or replay"
            );
            assert_eq!(method, Method::POST);
            std::future::pending::<Result<Response<Body>, Infallible>>().await
        }
    });
    let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            transport.create_or_exact_recover_event(&plan, "2026-01-01T00:00:00Z"),
        )
        .await
        .expect("bounded Event POST timeout returns"),
        Err(TransportErrorV0::ResponseLost)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
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
async fn automation_prewrite_failure_reuses_one_frozen_completion_patch() {
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
            response: status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"kind":"Status","apiVersion":"v1","status":"Failure","code":500}),
            ),
        },
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

    assert_eq!(
        transport
            .persist_automation_run_completion(
                "tenant-a",
                "run-a",
                "run-uid",
                "4",
                &spec,
                &desired_status,
            )
            .await
            .expect("one retry retains the completion patch")
            .new_resource_version,
        "5"
    );
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
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    api.assert_drained();
}

#[tokio::test]
async fn timed_out_automation_completion_proves_once_or_holds_without_a_second_patch() {
    let spec = automation_spec();
    let desired_status = completion_status();
    let exact = automation_response(&spec, &desired_status, "5");
    let (transport, patches) =
        timed_out_automation_completion_transport(spec.clone(), desired_status.clone(), exact);
    let ack = tokio::time::timeout(
        Duration::from_secs(1),
        transport.persist_automation_run_completion(
            "tenant-a",
            "run-a",
            "run-uid",
            "4",
            &spec,
            &desired_status,
        ),
    )
    .await
    .expect("bounded status response timeout reaches its one proof GET")
    .expect("exact post-write status proves completion");
    assert_eq!(ack.new_resource_version, "5");
    assert_eq!(patches.load(Ordering::SeqCst), 1);

    for recovery in [
        automation_response(&spec, &AutomationRunStatus::default(), "4"),
        {
            let mut foreign = automation_response(&spec, &desired_status, "5");
            foreign["metadata"]["uid"] = json!("foreign-run");
            foreign
        },
        {
            let mut divergent = automation_response(&spec, &desired_status, "5");
            divergent["status"]["attempts"] = json!(99);
            divergent
        },
    ] {
        let (transport, patches) = timed_out_automation_completion_transport(
            spec.clone(),
            desired_status.clone(),
            recovery,
        );
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(1),
                transport.persist_automation_run_completion(
                    "tenant-a",
                    "run-a",
                    "run-uid",
                    "4",
                    &spec,
                    &desired_status,
                ),
            )
            .await
            .expect("bounded status response timeout reaches fail-closed proof"),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        assert_eq!(patches.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn stale_pins_make_no_effect_request_and_409_maps_to_conflict() {
    let stale = desired(ResourceKindV0::ConfigMap, "cm-a", "actual");
    let (transport, api) = ScriptedApi::transport([]);
    assert_eq!(
        transport.patch_config_map(&stale, "expected", false).await,
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
        transport.patch_config_map(&conflict, "42", false).await,
        Err(TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
            operation: WriteOperationV0::Apply,
            resource: ResourceKindV0::ConfigMap,
        }))
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
            path_and_query: "/apis/apps/v1/namespaces/tenant-a/deployments?",
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

fn retired_delete_script(
    state_name: &str,
    root: Value,
    state: Value,
    delete_response: Response<Body>,
    proof_response: Response<Body>,
) -> Vec<Expected> {
    let state_path = retired_state_path(state_name);
    let delete_path = format!("{state_path}?");
    vec![
        Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root),
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(state_path.clone().into_boxed_str()),
            content_type: None,
            body: None,
            response: response(state),
        },
        Expected {
            method: Method::DELETE,
            path_and_query: Box::leak(delete_path.into_boxed_str()),
            content_type: Some("application/json"),
            body: Some(json!({"preconditions":{"uid":"state-uid","resourceVersion":"4"}})),
            response: delete_response,
        },
        Expected {
            method: Method::GET,
            path_and_query: Box::leak(state_path.into_boxed_str()),
            content_type: None,
            body: None,
            response: proof_response,
        },
    ]
}

#[tokio::test]
async fn retired_state_delete_is_uid_rv_fenced_and_2xx_requires_a_404_proof() {
    let (address, command, root, state) = retired_state_setup();
    let (transport, api) = ScriptedApi::transport(retired_delete_script(
        &address.state_name,
        root,
        state,
        response(json!({"kind":"Status","apiVersion":"v1","status":"Success"})),
        state_absent_response(),
    ));
    let outcome = transport
        .dispatch(&address, &command)
        .await
        .expect("the only acknowledgement is the in-session 404 proof");
    assert!(matches!(
        outcome,
        super::CommandOutcomeV0::Event(event)
            if matches!(&*event, oxideloop_controller_core::EventV0::RetiredStateDeleted {
                state_uid, state_resource_version, ..
            } if state_uid == "state-uid" && state_resource_version == "4")
    ));
    // `ScriptedApi` has exactly one DELETE and no root PATCH expectation:
    // draining proves both at-most-one deletion and no root write.
    api.assert_drained();
}

#[tokio::test]
async fn retired_state_delete_recovers_an_ambiguous_response_only_with_the_same_404_proof() {
    let (address, command, root, state) = retired_state_setup();
    let (transport, api) = ScriptedApi::transport(retired_delete_script(
        &address.state_name,
        root,
        state,
        lost_response(),
        state_absent_response(),
    ));
    assert!(matches!(
        transport.dispatch(&address, &command).await,
        Ok(super::CommandOutcomeV0::Event(event))
            if matches!(&*event, oxideloop_controller_core::EventV0::RetiredStateDeleted { .. })
    ));
    // A second DELETE or any root PATCH would be an unexpected scripted call.
    api.assert_drained();
}

#[tokio::test]
async fn retired_state_delete_ambiguous_response_with_a_present_proof_holds_without_replay() {
    let (address, command, root, state) = retired_state_setup();
    let (transport, api) = ScriptedApi::transport(retired_delete_script(
        &address.state_name,
        root,
        state.clone(),
        lost_response(),
        response(state),
    ));
    assert_eq!(
        transport.dispatch(&address, &command).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    // The script contains one DELETE and one proof GET. Draining it proves an
    // ambiguous in-process attempt is held rather than issuing a retry.
    api.assert_drained();
}

#[tokio::test]
async fn retired_state_delete_holds_for_present_or_replaced_proof_without_a_replay_or_root_write() {
    for replacement_uid in ["state-uid", "replacement-state-uid"] {
        let (address, command, root, state) = retired_state_setup();
        let mut proof = state.clone();
        proof["metadata"]["uid"] = json!(replacement_uid);
        let (transport, api) = ScriptedApi::transport(retired_delete_script(
            &address.state_name,
            root,
            state,
            response(json!({"kind":"Status","apiVersion":"v1","status":"Success"})),
            response(proof),
        ));
        assert_eq!(
            transport.dispatch(&address, &command).await,
            Err(TransportErrorV0::ManualRecoveryHold)
        );
        api.assert_drained();
    }
}

#[tokio::test]
async fn retired_state_delete_rejects_metadata_mismatch_before_delete_or_root_write() {
    let (address, command, root, mut state) = retired_state_setup();
    state["metadata"]["annotations"][ROOT_UID_ANNOTATION] = json!("other-root");
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
            path_and_query: Box::leak(retired_state_path(&address.state_name).into_boxed_str()),
            content_type: None,
            body: None,
            response: response(state),
        },
    ]);
    assert_eq!(
        transport.dispatch(&address, &command).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    // The fence fails before DELETE; the script intentionally permits neither
    // a retry nor a root PATCH.
    api.assert_drained();
}

#[tokio::test]
async fn retired_state_delete_rejects_root_or_receipt_drift_before_state_delete() {
    let mutations: [fn(&mut Value); 2] = [
        |root: &mut Value| {
            root["status"]["state"]["uid"] = json!("replacement-state-uid");
        },
        |root: &mut Value| {
            root["status"]["deletionCleanupReceipt"]["stateUid"] = json!("replacement-state-uid");
        },
    ];
    for mutate in mutations {
        let (address, command, mut root, _state) = retired_state_setup();
        mutate(&mut root);
        let (transport, api) = ScriptedApi::transport([Expected {
            method: Method::GET,
            path_and_query: "/apis/demo.oxideloop.dev/v1alpha1/namespaces/tenant-a/webapplications/app-a",
            content_type: None,
            body: None,
            response: response(root),
        }]);
        assert_eq!(
            transport.dispatch(&address, &command).await,
            Err(TransportErrorV0::ManualRecoveryHold)
        );
        // No State DELETE, retry, or root PATCH is authorized after either
        // the link or the receipt loses its exact root correlation.
        api.assert_drained();
    }
}

#[tokio::test]
async fn retired_state_delete_holds_when_the_bounded_404_proof_errors() {
    let (address, command, root, state) = retired_state_setup();
    let (transport, api) = ScriptedApi::transport(retired_delete_script(
        &address.state_name,
        root,
        state,
        response(json!({"kind":"Status","apiVersion":"v1","status":"Success"})),
        status(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","code":500}),
        ),
    ));
    assert_eq!(
        transport.dispatch(&address, &command).await,
        Err(TransportErrorV0::ManualRecoveryHold)
    );
    // The proof error consumes the one bounded recovery read; the delete is
    // never replayed and no root mutation follows.
    api.assert_drained();
}
