use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use oxideloop_bundle_contract::{BundleDescriptorV0, DescriptorModuleV0};
use oxideloop_controller_api::{
    AutomationRunCompletion, AutomationRunCompletionState, AutomationRunSpec, AutomationRunStatus,
    ObjectIdentity as ApiObjectIdentity, automation_run_name,
};
use oxideloop_controller_core::EventV0;
use oxideloop_controller_runtime::{
    AutomationExecutionV0, AutomationPortV0, FailpointV0, FailpointsV0, RendererPortV0,
    RuntimeErrorV0, drive_automation,
};
use oxideloop_event_planner::{
    ObjectIdentity as PlannerObjectIdentity, ObjectRef as PlannerObjectRefV0, PlannedEventV0,
    RunCorrelationV0,
};
use oxideloop_kube_transport::{AutomationCompletionAckV0, EventMaterializationRefV0};
use oxideloop_renderer_abi::{
    ActionInputV0, ActionOutputV0, canonical_bytes,
    model::{ABI_VERSION_V0, ActionDeclaration},
    sha256_hex,
};
use oxideloop_renderer_protocol::{PhaseV0, RequestV0, ResponseStatusV0, ResponseV0};
use serde_json::json;

const BUNDLE_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ACTION_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const EVENT_TIME: &str = "2026-08-30T12:00:00Z";

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredStatus {
    spec: AutomationRunSpec,
    status: AutomationRunStatus,
}

#[derive(Default)]
struct DurableAutomationState {
    event: Option<PlannedEventV0>,
    status: Option<StoredStatus>,
    event_creations: usize,
    event_recoveries: usize,
    status_creations: usize,
    status_recoveries: usize,
}

#[derive(Clone)]
struct FakeAutomationPort {
    durable: Arc<Mutex<DurableAutomationState>>,
}

impl FakeAutomationPort {
    fn new(durable: Arc<Mutex<DurableAutomationState>>) -> Self {
        Self { durable }
    }
}

#[async_trait]
impl AutomationPortV0 for FakeAutomationPort {
    async fn create_or_exact_recover_event(
        &mut self,
        plan: &PlannedEventV0,
        _event_time: &str,
    ) -> Result<EventMaterializationRefV0, RuntimeErrorV0> {
        let mut durable = self.durable.lock().expect("durable state lock");
        match &durable.event {
            None => {
                durable.event = Some(plan.clone());
                durable.event_creations += 1;
            }
            Some(existing) if existing == plan => durable.event_recoveries += 1,
            Some(_) => {
                return Err(RuntimeErrorV0::Hold(
                    "preexisting Event is not exact".into(),
                ));
            }
        }
        Ok(EventMaterializationRefV0 {
            name: plan.metadata.name.clone(),
            uid: "event-uid".into(),
            resource_version: "21".into(),
        })
    }

    async fn persist_completion(
        &mut self,
        namespace: &str,
        name: &str,
        expected_uid: &str,
        expected_resource_version: &str,
        expected_spec: &AutomationRunSpec,
        status: &AutomationRunStatus,
    ) -> Result<AutomationCompletionAckV0, RuntimeErrorV0> {
        let execution = execution(expected_spec);
        if namespace != execution.run.identity.namespace
            || name != execution.run.identity.name
            || expected_uid != execution.run.uid
            || expected_resource_version != execution.run.resource_version
        {
            return Err(RuntimeErrorV0::Hold(
                "AutomationRun identity pins changed".into(),
            ));
        }
        let mut durable = self.durable.lock().expect("durable state lock");
        match &durable.status {
            None => {
                durable.status = Some(StoredStatus {
                    spec: expected_spec.clone(),
                    status: status.clone(),
                });
                durable.status_creations += 1;
            }
            Some(existing) if existing.spec == *expected_spec && existing.status == *status => {
                durable.status_recoveries += 1;
            }
            Some(_) => {
                return Err(RuntimeErrorV0::Hold(
                    "preexisting AutomationRun status/spec is not exact".into(),
                ));
            }
        }
        completion_ack(status)
    }
}

#[derive(Clone)]
struct FakeRenderer {
    payload: Vec<u8>,
    requests: Arc<Mutex<Vec<RequestV0>>>,
}

impl FakeRenderer {
    fn new(requests: Arc<Mutex<Vec<RequestV0>>>) -> Self {
        Self {
            payload: action_output_payload(),
            requests,
        }
    }
}

#[async_trait]
impl RendererPortV0 for FakeRenderer {
    async fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0> {
        assert_eq!(request.phase, PhaseV0::Action);
        assert_eq!(request.bundle_digest, BUNDLE_DIGEST);
        assert_eq!(request.module_digest.as_deref(), Some(ACTION_DIGEST));
        self.requests
            .lock()
            .expect("renderer requests lock")
            .push(request.clone());
        Ok(ResponseV0 {
            request_id: request.request_id,
            phase: request.phase,
            bundle_digest: request.bundle_digest.clone(),
            module_digest: request.module_digest.clone(),
            status: ResponseStatusV0::Ok,
            error_category: None,
            payload: self.payload.clone(),
        })
    }
}

fn descriptor() -> BundleDescriptorV0 {
    BundleDescriptorV0 {
        descriptor_version: "oxideloop.dev/descriptor/v0".into(),
        renderer_abi_version: ABI_VERSION_V0.into(),
        render_module: DescriptorModuleV0 {
            identity: "render/main.mjs".into(),
            digest: "c".repeat(64),
        },
        action_modules: vec![DescriptorModuleV0 {
            identity: "actions/input-changed.mjs".into(),
            digest: ACTION_DIGEST.into(),
        }],
    }
}

fn spec(occurrence_id: &str) -> AutomationRunSpec {
    let event_metadata = json!({"trigger": "input changed"});
    let action_input = ActionInputV0 {
        abi_version: ABI_VERSION_V0.into(),
        bundle_digest: BUNDLE_DIGEST.into(),
        automation_identity: "input-changed".into(),
        automation_version: "v1".into(),
        action_module_identity: "actions/input-changed.mjs".into(),
        action_module_digest: ACTION_DIGEST.into(),
        occurrence_id: occurrence_id.into(),
        source_transition_id: "transition-17".into(),
        source_uid: "configmap-input-uid".into(),
        source_resource_version: "19".into(),
        event_metadata: event_metadata.clone(),
    };
    let action_input_bytes = canonical_bytes(&serde_json::to_value(action_input).expect("input"))
        .expect("canonical action input");
    AutomationRunSpec {
        occurrence_id: occurrence_id.into(),
        root_uid: "root-uid".into(),
        source: ApiObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "input".into(),
        },
        source_uid: "configmap-input-uid".into(),
        source_resource_version: "19".into(),
        source_transition_id: "transition-17".into(),
        automation_id: "input-changed".into(),
        automation_version: "v1".into(),
        bundle_digest: BUNDLE_DIGEST.into(),
        action_module_identity: "actions/input-changed.mjs".into(),
        action_module_digest: ACTION_DIGEST.into(),
        renderer_abi_version: ABI_VERSION_V0.into(),
        action_input_v0: String::from_utf8(action_input_bytes.clone()).expect("UTF-8 action input"),
        action_input_hash: sha256_hex(&action_input_bytes),
        event_metadata_hash: sha256_hex(
            &canonical_bytes(&event_metadata).expect("canonical event metadata"),
        ),
    }
}

fn planner_ref(
    api_version: &str,
    kind: &str,
    name: &str,
    uid: &str,
    resource_version: &str,
) -> PlannerObjectRefV0 {
    PlannerObjectRefV0 {
        identity: PlannerObjectIdentity {
            api_version: api_version.into(),
            kind: kind.into(),
            namespace: "demo".into(),
            name: name.into(),
        },
        uid: uid.into(),
        resource_version: resource_version.into(),
    }
}

fn execution(spec: &AutomationRunSpec) -> AutomationExecutionV0 {
    let root = planner_ref(
        "demo.oxideloop.dev/v1alpha1",
        "WebApplication",
        "app",
        &spec.root_uid,
        "11",
    );
    let source = planner_ref(
        &spec.source.api_version,
        &spec.source.kind,
        &spec.source.name,
        &spec.source_uid,
        &spec.source_resource_version,
    );
    let run = planner_ref(
        "runtime.oxideloop.dev/v1alpha1",
        "AutomationRun",
        &automation_run_name(&spec.root_uid, &spec.occurrence_id).expect("run name"),
        "automation-run-uid",
        "31",
    );
    AutomationExecutionV0 {
        root: root.clone(),
        source: source.clone(),
        run: run.clone(),
        correlation: RunCorrelationV0 {
            occurrence_id: spec.occurrence_id.clone(),
            root,
            run,
            source: source.identity,
            source_uid: spec.source_uid.clone(),
            source_resource_version: spec.source_resource_version.clone(),
            source_transition_id: spec.source_transition_id.clone(),
            automation_identity: spec.automation_id.clone(),
            automation_version: spec.automation_version.clone(),
            bundle_digest: spec.bundle_digest.clone(),
            action_module_identity: spec.action_module_identity.clone(),
            action_module_digest: spec.action_module_digest.clone(),
            renderer_abi_version: spec.renderer_abi_version.clone(),
            action_input_hash: spec.action_input_hash.clone(),
            event_metadata_hash: spec.event_metadata_hash.clone(),
        },
    }
}

fn action_output_payload() -> Vec<u8> {
    let output = ActionOutputV0 {
        abi_version: ABI_VERSION_V0.into(),
        actions: vec![ActionDeclaration {
            logical_id: "input-changed-event".into(),
            kind: "Event".into(),
            specification: json!({
                "apiVersion": "events.k8s.io/v1",
                "kind": "Event",
                "metadata": {"namespace": "demo"},
                "regarding": {
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "namespace": "demo",
                    "name": "input"
                },
                "action": "Changed",
                "reason": "InputChanged",
                "note": "input changed",
                "type": "Normal"
            }),
        }],
        diagnostics: vec![],
    };
    canonical_bytes(&serde_json::to_value(output).expect("action output"))
        .expect("canonical action output")
}

fn completion_status() -> AutomationRunStatus {
    AutomationRunStatus {
        attempts: 1,
        materialization_name: Some("wrong-event-name".into()),
        materialization_uid: Some("wrong-event-uid".into()),
        completion: Some(AutomationRunCompletion {
            state: AutomationRunCompletionState::Succeeded,
            message: "different but valid persisted status".into(),
        }),
    }
}

fn completion_ack(
    status: &AutomationRunStatus,
) -> Result<AutomationCompletionAckV0, RuntimeErrorV0> {
    let completion = status
        .completion
        .as_ref()
        .ok_or_else(|| RuntimeErrorV0::Hold("completion is absent".into()))?;
    Ok(AutomationCompletionAckV0 {
        uid: "automation-run-uid".into(),
        old_resource_version: "31".into(),
        new_resource_version: "32".into(),
        attempts: status.attempts,
        materialization_name: status
            .materialization_name
            .clone()
            .ok_or_else(|| RuntimeErrorV0::Hold("materialization name is absent".into()))?,
        materialization_uid: status
            .materialization_uid
            .clone()
            .ok_or_else(|| RuntimeErrorV0::Hold("materialization UID is absent".into()))?,
        completion: completion.state,
    })
}

fn planned_event(spec: &AutomationRunSpec) -> PlannedEventV0 {
    let execution = execution(spec);
    oxideloop_event_planner::plan_event(
        &oxideloop_renderer_abi::parse_validate_action_input(&spec.action_input_v0)
            .expect("valid action input"),
        &oxideloop_renderer_abi::parse_validate_action_output(
            std::str::from_utf8(&action_output_payload()).expect("UTF-8 action output"),
        )
        .expect("valid action output"),
        &execution.root,
        &execution.source,
        &execution.run,
        &execution.correlation,
    )
    .expect("valid plan")
}

fn expected_completion(spec: &AutomationRunSpec) -> EventV0 {
    EventV0::AutomationCompleted {
        occurrence_id: spec.occurrence_id.clone(),
        name: automation_run_name(&spec.root_uid, &spec.occurrence_id).expect("run name"),
        uid: "automation-run-uid".into(),
        old_resource_version: "31".into(),
        new_resource_version: "32".into(),
        completion: AutomationRunCompletionState::Succeeded,
        attempts: 1,
        materialization_name: planned_event(spec).metadata.name,
        materialization_uid: "event-uid".into(),
    }
}

async fn drive(
    durable: Arc<Mutex<DurableAutomationState>>,
    requests: Arc<Mutex<Vec<RequestV0>>>,
    spec: &AutomationRunSpec,
    failpoints: &mut FailpointsV0,
) -> Result<EventV0, RuntimeErrorV0> {
    drive_automation(
        &mut FakeAutomationPort::new(durable),
        &mut FakeRenderer::new(requests),
        &descriptor(),
        &execution(spec),
        spec,
        EVENT_TIME,
        failpoints,
    )
    .await
}

#[tokio::test]
async fn automation_success_uses_strict_pins_and_materializes_the_typed_event() {
    let durable = Arc::new(Mutex::new(DurableAutomationState::default()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let spec = spec("occurrence-17");
    let mut failpoints = FailpointsV0::default();

    let completion = drive(durable.clone(), requests.clone(), &spec, &mut failpoints)
        .await
        .expect("automation succeeds");

    assert_eq!(completion, expected_completion(&spec));
    let durable = durable.lock().expect("durable state lock");
    let event = durable.event.as_ref().expect("one Event materialized");
    assert_eq!(durable.event_creations, 1);
    assert_eq!(event.regarding.name, spec.source.name);
    assert_eq!(event.regarding.uid, spec.source_uid);
    assert_eq!(
        event.regarding.resource_version,
        spec.source_resource_version
    );
    assert_eq!(event.metadata.owner_references[0].uid, "automation-run-uid");
    assert_eq!(
        event.metadata.annotations["runtime.oxideloop.dev/root-uid"],
        spec.root_uid
    );
    assert_eq!(
        event.metadata.annotations["runtime.oxideloop.dev/occurrence-id"],
        spec.occurrence_id
    );
    drop(durable);
    let requests = requests.lock().expect("renderer requests lock");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].request_id.bytes,
        super_request_id(&spec.action_input_hash)
    );
    assert_eq!(requests[0].payload, spec.action_input_v0.as_bytes());
}

#[tokio::test]
async fn event_crash_replay_recovers_exactly_once_with_fresh_failpoints() {
    let durable = Arc::new(Mutex::new(DurableAutomationState::default()));
    let first_requests = Arc::new(Mutex::new(Vec::new()));
    let replay_requests = Arc::new(Mutex::new(Vec::new()));
    let spec = spec("occurrence-17");
    let mut first_failpoints = FailpointsV0::default();
    first_failpoints
        .arm(FailpointV0::AfterEventBeforeStatus)
        .expect("arm first runtime only");
    let mut fresh_failpoints = FailpointsV0::default();

    assert!(matches!(
        drive(
            durable.clone(),
            first_requests.clone(),
            &spec,
            &mut first_failpoints,
        )
        .await,
        Err(RuntimeErrorV0::InjectedCrash(
            FailpointV0::AfterEventBeforeStatus
        ))
    ));
    assert!(!first_failpoints.take(FailpointV0::AfterEventBeforeStatus));
    assert!(!fresh_failpoints.take(FailpointV0::AfterEventBeforeStatus));

    assert_eq!(
        drive(
            durable.clone(),
            replay_requests.clone(),
            &spec,
            &mut fresh_failpoints,
        )
        .await
        .expect("fresh replay succeeds"),
        expected_completion(&spec)
    );
    let durable = durable.lock().expect("durable state lock");
    assert_eq!(
        durable.event_creations, 1,
        "replay did not create a second Event"
    );
    assert_eq!(durable.event_recoveries, 1);
    assert_eq!(durable.status_creations, 1);
    drop(durable);
    let first = first_requests.lock().expect("first request lock")[0].clone();
    let replay = replay_requests.lock().expect("replay request lock")[0].clone();
    assert_eq!(first.request_id, replay.request_id);
    assert_eq!(first.payload, replay.payload);
}

#[tokio::test]
async fn status_crash_replay_exactly_recovers_status_before_reducer_ack() {
    let durable = Arc::new(Mutex::new(DurableAutomationState::default()));
    let spec = spec("occurrence-17");
    let mut first_failpoints = FailpointsV0::default();
    first_failpoints
        .arm(FailpointV0::AfterStatusBeforeAck)
        .expect("arm first runtime only");

    assert!(matches!(
        drive(
            durable.clone(),
            Arc::new(Mutex::new(Vec::new())),
            &spec,
            &mut first_failpoints,
        )
        .await,
        Err(RuntimeErrorV0::InjectedCrash(
            FailpointV0::AfterStatusBeforeAck
        ))
    ));

    let mut fresh_failpoints = FailpointsV0::default();
    assert_eq!(
        drive(
            durable.clone(),
            Arc::new(Mutex::new(Vec::new())),
            &spec,
            &mut fresh_failpoints,
        )
        .await
        .expect("fresh replay receives reducer acknowledgement"),
        expected_completion(&spec)
    );
    let durable = durable.lock().expect("durable state lock");
    assert_eq!(durable.event_creations, 1);
    assert_eq!(durable.event_recoveries, 1);
    assert_eq!(durable.status_creations, 1);
    assert_eq!(durable.status_recoveries, 1);
}

#[tokio::test]
async fn preexisting_mismatches_are_held_and_never_adopted() {
    let run_spec = spec("occurrence-17");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut failpoints = FailpointsV0::default();
    let mut mismatched_event = DurableAutomationState::default();
    let mut wrong_event = planned_event(&run_spec);
    wrong_event.reason = "DifferentReason".into();
    mismatched_event.event = Some(wrong_event.clone());
    let durable = Arc::new(Mutex::new(mismatched_event));
    assert!(matches!(
        drive(durable.clone(), requests, &run_spec, &mut failpoints).await,
        Err(RuntimeErrorV0::Hold(_))
    ));
    assert_eq!(
        durable.lock().expect("durable state lock").event.as_ref(),
        Some(&wrong_event)
    );

    for stored in [
        StoredStatus {
            spec: run_spec.clone(),
            status: completion_status(),
        },
        StoredStatus {
            spec: spec("occurrence-other"),
            status: AutomationRunStatus {
                attempts: 1,
                materialization_name: Some("event-name".into()),
                materialization_uid: Some("event-uid".into()),
                completion: Some(AutomationRunCompletion {
                    state: AutomationRunCompletionState::Succeeded,
                    message: "action event materialized".into(),
                }),
            },
        },
    ] {
        let mut state = DurableAutomationState {
            status: Some(stored.clone()),
            status_creations: 1,
            ..DurableAutomationState::default()
        };
        let valid_plan = planned_event(&run_spec);
        state.event = Some(valid_plan);
        let durable = Arc::new(Mutex::new(state));
        let mut failpoints = FailpointsV0::default();
        assert!(matches!(
            drive(
                durable.clone(),
                Arc::new(Mutex::new(Vec::new())),
                &run_spec,
                &mut failpoints,
            )
            .await,
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_eq!(
            durable.lock().expect("durable state lock").status.as_ref(),
            Some(&stored)
        );
    }
}

fn super_request_id(hash: &str) -> [u8; 16] {
    oxideloop_controller_runtime::deterministic_request_id(hash).expect("valid hash")
}
