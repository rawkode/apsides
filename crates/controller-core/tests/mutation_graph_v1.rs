//! Black-box coverage for the public V1 transaction/revalidation protocol.
//!
//! These tests intentionally drive the reducer only through exported DTOs and
//! `reduce`; they are a compatibility tripwire for the renderer/controller
//! boundary rather than white-box tests of private planner helpers.

use oxideloop_controller_api::{
    ObjectIdentity, OxideLoopStateSpec, PersistPurpose, ProgramBundleReference, RootBinding,
    STATE_VERSION_V1, StateBody, StateLink, WebApplicationSpec, WebApplicationStatus, bootstrap_id,
    state_name,
};
use oxideloop_controller_core::{
    CommandV0, DesiredObservationV0, EventV0, FrozenInputV0, MachineStateV0, PhaseV0,
    PreconditionsV0, RecoveryProofV0, RootSnapshotV0, reduce,
};
use oxideloop_mutation_model::{
    BackendQualification, ObjectIdentity as MutationIdentity, Observation,
};
use oxideloop_renderer_abi::model::{
    ABI_VERSION_V0, Dependency, RenderDisposition, ResourceDeclaration, RootProjection,
};
use oxideloop_renderer_abi::{RenderInputV0, RenderOutputV0, parse_validate_render_input};

const ROOT_UID: &str = "root-uid";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn identity(name: &str) -> ObjectIdentity {
    ObjectIdentity {
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        namespace: "demo".into(),
        name: name.into(),
    }
}

fn linked() -> MachineStateV0 {
    let state = state_name(ROOT_UID).expect("state name");
    MachineStateV0::new(
        RootSnapshotV0 {
            uid: ROOT_UID.into(),
            namespace: "demo".into(),
            name: "demo".into(),
            generation: 1,
            spec: WebApplicationSpec {
                message: "hello".into(),
                expose: false,
                program: ProgramBundleReference {
                    config_map_name: "bundle".into(),
                    key: "program.mjs".into(),
                    digest: DIGEST.into(),
                },
            },
            bundle_digest: DIGEST.into(),
            status: WebApplicationStatus {
                state: Some(StateLink {
                    root_uid: ROOT_UID.into(),
                    name: state.clone(),
                    uid: "state-uid".into(),
                }),
                input_token: Some("barrier-1".into()),
                ..Default::default()
            },
            state_name: state,
            state_uid: "state-uid".into(),
            state_resource_version: "11".into(),
            root_resource_version: "20".into(),
            root_finalizer_present: true,
            state_finalizer_present: true,
            deleting: false,
        },
        OxideLoopStateSpec {
            state_version: STATE_VERSION_V1,
            root: RootBinding {
                uid: ROOT_UID.into(),
                bootstrap_id: bootstrap_id(ROOT_UID).expect("bootstrap"),
                bundle_digest: DIGEST.into(),
            },
            state: StateBody::default(),
        },
    )
    .expect("linked machine")
}

fn render_input(machine: &MachineStateV0, token: &str, epoch: &str) -> String {
    serde_json::to_string(&RenderInputV0 {
        abi_version: ABI_VERSION_V0.into(),
        root: RootProjection {
            api_version: "demo.oxideloop.dev/v1alpha1".into(),
            kind: "WebApplication".into(),
            namespace: machine.root().namespace.clone(),
            name: machine.root().name.clone(),
            uid: machine.root().uid.clone(),
            generation: u64::try_from(machine.root().generation).expect("nonnegative generation"),
            spec: serde_json::to_value(&machine.root().spec).expect("spec"),
        },
        bundle_digest: DIGEST.into(),
        cache_epoch: epoch.into(),
        observation_token: token.into(),
        observations: vec![],
    })
    .expect("input")
}

fn output(name: &str, dependency_token: &str) -> String {
    serde_json::to_string(&RenderOutputV0 {
        abi_version: ABI_VERSION_V0.into(), disposition: RenderDisposition::Apply,
        resources: vec![ResourceDeclaration { logical_id: "object".into(), object: serde_json::json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"namespace":"demo","name":name}}), readiness: None }],
        dependencies: vec![Dependency::Object { api_version: "v1".into(), kind: "ConfigMap".into(), namespace: "demo".into(), name: "input".into(), uid: Some("input-uid".into()), resource_version: Some("1".into()), observation_token: dependency_token.into() }],
        automations: vec![], diagnostics: vec![],
    }).expect("output")
}

fn observed(name: &str, current: Option<(&str, &str)>) -> Vec<DesiredObservationV0> {
    vec![DesiredObservationV0 {
        identity: identity(name),
        observation: match current {
            Some((uid, rv)) => Observation::Existing {
                identity: MutationIdentity {
                    uid: uid.into(),
                    resource_version: rv.into(),
                },
            },
            None => Observation::Absent,
        },
        qualification: BackendQualification::BuiltInTested {
            kubernetes_minor: "1.36".into(),
            gvr: "configmaps".into(),
        },
        recovery_proof: None,
        deletion_recovery_proof: None,
    }]
}

fn render_pending() -> (MachineStateV0, PreconditionsV0) {
    let started = reduce(linked(), EventV0::Start).expect("start");
    let descriptor = match &started.commands[..] {
        [CommandV0::RequestDescriptor { token, .. }] => token.clone(),
        c => panic!("descriptor: {c:?}"),
    };
    let descriptor = reduce(
        started.state,
        EventV0::DescriptorReady {
            token: descriptor,
            bundle_digest: DIGEST.into(),
        },
    )
    .expect("descriptor ready");
    let (token, old) = match &descriptor.commands[..] {
        [
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            },
        ] => (token.clone(), old_root_resource_version.clone()),
        c => panic!("status: {c:?}"),
    };
    let status = reduce(
        descriptor.state,
        EventV0::RootStatusPersisted {
            token,
            old_root_resource_version: old,
            new_root_resource_version: "21".into(),
            root_uid: ROOT_UID.into(),
        },
    )
    .expect("status");
    let render = reduce(status.state, EventV0::InformersReady).expect("informers");
    let preconditions = match &render.commands[..] {
        [CommandV0::RequestRender { preconditions }] => preconditions.clone(),
        c => panic!("render: {c:?}"),
    };
    (render.state, preconditions)
}

fn make_frozen(
    machine: &MachineStateV0,
    preconditions: &PreconditionsV0,
    input: &str,
) -> FrozenInputV0 {
    FrozenInputV0 {
        root_generation: machine.root().generation,
        state_resource_version: preconditions.state_resource_version.clone(),
        observation_token: preconditions.observation_token.clone(),
        cache_epoch: preconditions.cache_epoch.clone(),
        final_observation_fence_token: "f".repeat(64),
        render_input_hash: parse_validate_render_input(input)
            .expect("canonical input")
            .sha256,
    }
}

fn prepare(
    machine: MachineStateV0,
    resource_name: &str,
    dependency_token: &str,
) -> (
    MachineStateV0,
    String,
    String,
    Vec<DesiredObservationV0>,
    FrozenInputV0,
) {
    let preconditions = match machine.phase() {
        PhaseV0::RenderPending => PreconditionsV0 {
            root_uid: machine.root().uid.clone(),
            root_generation: machine.root().generation,
            root_resource_version: machine.root().root_resource_version.clone(),
            state_uid: machine.root().state_uid.clone(),
            state_resource_version: machine.root().state_resource_version.clone(),
            observation_token: machine
                .root()
                .status
                .input_token
                .clone()
                .expect("input token"),
            cache_epoch: "0".into(),
        },
        phase => panic!("render phase: {phase:?}"),
    };
    let input = render_input(
        &machine,
        &preconditions.observation_token,
        &preconditions.cache_epoch,
    );
    let output_json = output(resource_name, dependency_token);
    let observed = observed(resource_name, None);
    let frozen = make_frozen(&machine, &preconditions, &input);
    let rendered = reduce(
        machine,
        EventV0::Rendered {
            frozen: frozen.clone(),
            render_input_json: input.clone(),
            render_output_json: output_json.clone(),
            observed: observed.clone(),
        },
    )
    .expect("render");
    let (rendered, input, frozen) = match &rendered.commands[..] {
        [
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            },
        ] => {
            let status = reduce(
                rendered.state,
                EventV0::RootStatusPersisted {
                    token: token.clone(),
                    old_root_resource_version: old_root_resource_version.clone(),
                    new_root_resource_version: "22".into(),
                    root_uid: ROOT_UID.into(),
                },
            )
            .expect("root status ack");
            let preconditions = match &status.commands[..] {
                [CommandV0::RequestRender { preconditions }] => preconditions.clone(),
                c => panic!("render after status: {c:?}"),
            };
            let input = render_input(
                &status.state,
                &preconditions.observation_token,
                &preconditions.cache_epoch,
            );
            let frozen = make_frozen(&status.state, &preconditions, &input);
            let rendered = reduce(
                status.state,
                EventV0::Rendered {
                    frozen: frozen.clone(),
                    render_input_json: input.clone(),
                    render_output_json: output_json.clone(),
                    observed: observed.clone(),
                },
            )
            .expect("render after status");
            (rendered, input, frozen)
        }
        _ => (rendered, input, frozen),
    };
    let operation = match &rendered.commands[..] {
        [CommandV0::Persist { operation, .. }] if operation.purpose == PersistPurpose::Prepare => {
            operation.clone()
        }
        c => panic!("prepare: {c:?}"),
    };
    let revalidate = reduce(
        rendered.state,
        EventV0::Persisted {
            token: operation.token,
            old_state_resource_version: operation.old_state_resource_version,
            new_state_resource_version: "22".into(),
            state_uid: "state-uid".into(),
        },
    )
    .expect("prepare ack");
    (revalidate.state, input, output_json, observed, frozen)
}

fn authorize(
    prepared: MachineStateV0,
    input: &str,
    output_json: &str,
    mut obs: Vec<DesiredObservationV0>,
) -> (MachineStateV0, String, String) {
    let input_doc: RenderInputV0 = serde_json::from_str(input).expect("input");
    let preconditions = PreconditionsV0 {
        root_uid: prepared.root().uid.clone(),
        root_generation: prepared.root().generation,
        root_resource_version: prepared.root().root_resource_version.clone(),
        state_uid: prepared.root().state_uid.clone(),
        state_resource_version: prepared.root().state_resource_version.clone(),
        observation_token: input_doc.observation_token,
        cache_epoch: input_doc.cache_epoch,
    };
    let mut input_doc: RenderInputV0 = serde_json::from_str(input).expect("input");
    input_doc.cache_epoch.clone_from(&preconditions.cache_epoch);
    input_doc
        .observation_token
        .clone_from(&preconditions.observation_token);
    let input = serde_json::to_string(&input_doc).expect("fresh input");
    let fresh = make_frozen(&prepared, &preconditions, &input);
    let name = obs[0].identity.name.clone();
    obs[0].observation = Observation::Absent;
    let transition = reduce(
        prepared,
        EventV0::Revalidated {
            frozen: fresh,
            render_input_json: input.clone(),
            render_output_json: output_json.into(),
            observed: obs,
        },
    )
    .expect("revalidation");
    let operation = match &transition.commands[..] {
        [CommandV0::Persist { operation, .. }]
            if operation.purpose == PersistPurpose::DispatchAuthorization =>
        {
            operation.clone()
        }
        c => panic!("authorization: {c:?}"),
    };
    let authorized = reduce(
        transition.state,
        EventV0::Persisted {
            token: operation.token,
            old_state_resource_version: operation.old_state_resource_version,
            new_state_resource_version: "23".into(),
            state_uid: "state-uid".into(),
        },
    )
    .expect("authorization ack");
    let command_id = match &authorized.commands[..] {
        [CommandV0::Mutate { commands, .. }] => commands[0].command_id.clone(),
        c => panic!("mutate: {c:?}"),
    };
    (authorized.state, command_id, name)
}

fn restart_to_render(state: &MachineStateV0) -> (MachineStateV0, PreconditionsV0) {
    let recovered =
        MachineStateV0::recover_json(&serde_json::to_string(&state).expect("durable machine JSON"))
            .expect("recover machine");
    let started = reduce(recovered, EventV0::Start).expect("restart start");
    let descriptor = match &started.commands[..] {
        [CommandV0::RequestDescriptor { token, .. }] => token.clone(),
        c => panic!("descriptor: {c:?}"),
    };
    let descriptor = reduce(
        started.state,
        EventV0::DescriptorReady {
            token: descriptor,
            bundle_digest: DIGEST.into(),
        },
    )
    .expect("descriptor");
    let render = reduce(descriptor.state, EventV0::InformersReady).expect("fresh barrier");
    assert!(matches!(
        render.commands.as_slice(),
        [CommandV0::RequestRender { .. }]
    ));
    let preconditions = PreconditionsV0 {
        root_uid: render.state.root().uid.clone(),
        root_generation: render.state.root().generation,
        root_resource_version: render.state.root().root_resource_version.clone(),
        state_uid: render.state.root().state_uid.clone(),
        state_resource_version: render.state.root().state_resource_version.clone(),
        observation_token: "restart-observation".into(),
        cache_epoch: "restart-epoch".into(),
    };
    (render.state, preconditions)
}

#[test]
fn dependency_only_change_preserves_v1_command_and_promotes_fresh_dependencies() {
    let (pending, _) = render_pending();
    let (prepared, input, _output_json, observed, _) = prepare(pending, "object", "dependency-a");
    let old_command_id = prepared
        .journal()
        .state
        .apply_transaction
        .as_ref()
        .expect("transaction")
        .prepared_intents[0]
        .command_id
        .clone();
    let input_doc: RenderInputV0 = serde_json::from_str(&input).expect("input");
    let preconditions = PreconditionsV0 {
        root_uid: prepared.root().uid.clone(),
        root_generation: prepared.root().generation,
        root_resource_version: prepared.root().root_resource_version.clone(),
        state_uid: prepared.root().state_uid.clone(),
        state_resource_version: prepared.root().state_resource_version.clone(),
        observation_token: input_doc.observation_token,
        cache_epoch: input_doc.cache_epoch,
    };
    let mut input_doc: RenderInputV0 = serde_json::from_str(&input).expect("input");
    input_doc.cache_epoch = preconditions.cache_epoch.clone();
    input_doc.observation_token = preconditions.observation_token.clone();
    let fresh_input = serde_json::to_string(&input_doc).expect("fresh input");
    let fresh = make_frozen(&prepared, &preconditions, &fresh_input);
    let transition = reduce(
        prepared,
        EventV0::Revalidated {
            frozen: fresh,
            render_input_json: fresh_input,
            render_output_json: output("object", "dependency-b").clone(),
            observed,
        },
    )
    .expect("dependency-only revalidation");
    let operation = match &transition.commands[..] {
        [CommandV0::Persist { operation, .. }]
            if operation.purpose == PersistPurpose::DispatchAuthorization =>
        {
            operation.clone()
        }
        c => panic!("dispatch authorization: {c:?}"),
    };
    assert_eq!(
        transition.state.journal().state.pending_dependencies[0].observation_token,
        "dependency-b"
    );
    let authorized = reduce(
        transition.state,
        EventV0::Persisted {
            token: operation.token,
            old_state_resource_version: operation.old_state_resource_version,
            new_state_resource_version: "24".into(),
            state_uid: "state-uid".into(),
        },
    )
    .expect("dispatch ack");
    match &authorized.commands[..] {
        [CommandV0::Mutate { commands, .. }] => assert_eq!(commands[0].command_id, old_command_id),
        c => panic!("mutation: {c:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn changed_graph_and_wrong_or_a_b_a_proof_hold_without_mutation() {
    let (pending, _) = render_pending();
    let (prepared, input, output_json, _, _) = prepare(pending, "object", "dependency-a");
    let (issued_state, _, _) = authorize(prepared, &input, &output_json, observed("object", None));
    let recovered =
        MachineStateV0::recover_json(&serde_json::to_string(&issued_state).expect("durable"))
            .expect("restart");
    let started = reduce(recovered, EventV0::Start).expect("restart start");
    let descriptor = match &started.commands[..] {
        [CommandV0::RequestDescriptor { token, .. }] => token.clone(),
        c => panic!("descriptor: {c:?}"),
    };
    let descriptor = reduce(
        started.state,
        EventV0::DescriptorReady {
            token: descriptor,
            bundle_digest: DIGEST.into(),
        },
    )
    .expect("descriptor");
    let render = reduce(descriptor.state, EventV0::InformersReady).expect("fresh barrier");
    assert!(matches!(
        render.commands.as_slice(),
        [CommandV0::RequestRender { .. }]
    ));
    let preconditions = PreconditionsV0 {
        root_uid: render.state.root().uid.clone(),
        root_generation: render.state.root().generation,
        root_resource_version: render.state.root().root_resource_version.clone(),
        state_uid: render.state.root().state_uid.clone(),
        state_resource_version: render.state.root().state_resource_version.clone(),
        observation_token: "recovery-observation".into(),
        cache_epoch: "recovery-epoch".into(),
    };
    let fresh_input = render_input(
        &render.state,
        &preconditions.observation_token,
        &preconditions.cache_epoch,
    );
    let changed_output = output("different-object", "dependency-a");
    let changed = reduce(
        render.state.clone(),
        EventV0::Revalidated {
            frozen: make_frozen(&render.state, &preconditions, &fresh_input),
            render_input_json: fresh_input.clone(),
            render_output_json: changed_output,
            observed: observed("different-object", None),
        },
    )
    .expect("changed graph");
    assert!(matches!(
        changed.commands.as_slice(),
        [CommandV0::Hold { .. }]
    ));
    assert!(
        !changed
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );

    let tx = render
        .state
        .journal()
        .state
        .apply_transaction
        .as_ref()
        .expect("transaction")
        .clone();
    let mut wrong = observed("object", Some(("wrong-uid", "99")));
    wrong[0].recovery_proof = Some(RecoveryProofV0 {
        root_uid: ROOT_UID.into(),
        transaction_id: tx.id.clone(),
        command_id: "c".repeat(64),
        uid: "wrong-uid".into(),
        resource_version: "99".into(),
    });
    let proof = reduce(
        render.state.clone(),
        EventV0::Revalidated {
            frozen: make_frozen(&render.state, &preconditions, &fresh_input),
            render_input_json: fresh_input.clone(),
            render_output_json: output_json.clone(),
            observed: wrong,
        },
    )
    .expect("wrong proof");
    assert!(matches!(
        proof.commands.as_slice(),
        [CommandV0::Hold { .. }]
    ));
    assert!(
        !proof
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );

    let mut aba = observed("object", Some(("created-uid", "30")));
    aba[0].recovery_proof = Some(RecoveryProofV0 {
        root_uid: ROOT_UID.into(),
        transaction_id: tx.id,
        command_id: "c".repeat(64),
        uid: "created-uid".into(),
        resource_version: "30".into(),
    });
    let proof = reduce(
        render.state.clone(),
        EventV0::Revalidated {
            frozen: make_frozen(&render.state, &preconditions, &fresh_input),
            render_input_json: fresh_input,
            render_output_json: output_json,
            observed: aba,
        },
    );
    assert!(
        proof.is_err()
            || matches!(
                proof.expect("A-B-A transition").commands.as_slice(),
                [CommandV0::Hold { .. }]
            )
    );
}

#[test]
fn legacy_v0_unissued_journal_invalidates_before_any_mutation() {
    let (pending, _) = render_pending();
    let (prepared, _input, output_json, observed, _) = prepare(pending, "object", "dependency-a");
    let mut wire: serde_json::Value = serde_json::to_value(&prepared).expect("machine JSON");
    wire["journal"]["state"]["applyTransaction"]["graphIdentityAlgorithm"] =
        serde_json::json!("LegacyV0");
    let legacy: MachineStateV0 = serde_json::from_value(wire).expect("legacy journal");
    let (state, preconditions) = restart_to_render(&legacy);
    let fresh_input = render_input(
        &state,
        &preconditions.observation_token,
        &preconditions.cache_epoch,
    );
    let transition = reduce(
        state.clone(),
        EventV0::Revalidated {
            frozen: make_frozen(&state, &preconditions, &fresh_input),
            render_input_json: fresh_input,
            render_output_json: output_json,
            observed,
        },
    )
    .expect("legacy unissued revalidation");
    assert!(matches!(
        transition.commands.as_slice(),
        [CommandV0::Persist { operation, .. }] if operation.purpose == PersistPurpose::Invalidate
    ));
    assert!(
        !transition
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );
}

#[test]
fn legacy_v0_issued_journal_holds_before_any_mutation() {
    let (pending, _) = render_pending();
    let (prepared, input, output_json, _, _) = prepare(pending, "object", "dependency-a");
    let (issued, _, _) = authorize(prepared, &input, &output_json, observed("object", None));
    let mut wire: serde_json::Value = serde_json::to_value(&issued).expect("machine JSON");
    wire["journal"]["state"]["applyTransaction"]["graphIdentityAlgorithm"] =
        serde_json::json!("LegacyV0");
    let legacy: MachineStateV0 = serde_json::from_value(wire).expect("legacy issued journal");
    let (state, preconditions) = restart_to_render(&legacy);
    let fresh_input = render_input(
        &state,
        &preconditions.observation_token,
        &preconditions.cache_epoch,
    );
    let transition = reduce(
        state.clone(),
        EventV0::Revalidated {
            frozen: make_frozen(&state, &preconditions, &fresh_input),
            render_input_json: fresh_input,
            render_output_json: output_json,
            observed: observed("object", None),
        },
    )
    .expect("legacy issued revalidation");
    assert!(matches!(
        transition.commands.as_slice(),
        [CommandV0::Hold { .. }]
    ));
    assert!(
        !transition
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );
}
