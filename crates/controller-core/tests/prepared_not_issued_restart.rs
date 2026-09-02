use oxideloop_controller_api::{
    ObjectIdentity, OxideLoopStateSpec, PersistPurpose, ProgramBundleReference, RootBinding,
    STATE_VERSION_V1, StateBody, StateLink, WebApplicationSpec, WebApplicationStatus, bootstrap_id,
    state_name,
};
use oxideloop_controller_core::{
    CommandV0, DesiredObservationV0, EventV0, FrozenInputV0, MachineStateV0, PhaseV0,
    RootSnapshotV0, reduce,
};
use oxideloop_mutation_model::{BackendQualification, Observation};
use oxideloop_renderer_abi::{
    RenderOutputV0,
    model::{ABI_VERSION_V0, RenderDisposition, ResourceDeclaration, RootProjection},
    parse_validate_render_input,
};

const ROOT_UID: &str = "root-uid";
const BUNDLE_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn linked_machine() -> MachineStateV0 {
    let state_name = state_name(ROOT_UID).expect("deterministic state name");
    MachineStateV0::new(
        RootSnapshotV0 {
            uid: ROOT_UID.into(),
            namespace: "demo".into(),
            name: "demo".into(),
            generation: 1,
            spec: WebApplicationSpec {
                image: "example.invalid/web:latest".into(),
                replicas: Some(1),
                message: None,
                expose: false,
                program: ProgramBundleReference {
                    config_map_name: "bundle".into(),
                    key: "program.mjs".into(),
                    digest: BUNDLE_DIGEST.into(),
                },
            },
            bundle_digest: BUNDLE_DIGEST.into(),
            status: WebApplicationStatus {
                state: Some(StateLink {
                    root_uid: ROOT_UID.into(),
                    name: state_name.clone(),
                    uid: "state-uid".into(),
                }),
                input_token: Some("observation-1".into()),
                ..WebApplicationStatus::default()
            },
            state_name,
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
                bootstrap_id: bootstrap_id(ROOT_UID).expect("bootstrap id"),
                bundle_digest: BUNDLE_DIGEST.into(),
            },
            state: StateBody::default(),
        },
    )
    .expect("valid linked machine")
}

fn render_input(machine: &MachineStateV0, token: &str, cache_epoch: &str) -> String {
    serde_json::to_string(&oxideloop_renderer_abi::RenderInputV0 {
        abi_version: ABI_VERSION_V0.into(),
        root: RootProjection {
            api_version: "demo.oxideloop.dev/v1alpha1".into(),
            kind: "WebApplication".into(),
            namespace: machine.root().namespace.clone(),
            name: machine.root().name.clone(),
            uid: machine.root().uid.clone(),
            generation: u64::try_from(machine.root().generation).expect("non-negative generation"),
            spec: serde_json::to_value(&machine.root().spec).expect("root spec"),
        },
        bundle_digest: machine.root().bundle_digest.clone(),
        cache_epoch: cache_epoch.into(),
        observation_token: token.into(),
        observations: vec![],
    })
    .expect("render input")
}

fn desired_output() -> String {
    serde_json::to_string(&RenderOutputV0 {
        abi_version: ABI_VERSION_V0.into(),
        disposition: RenderDisposition::Apply,
        resources: vec![ResourceDeclaration {
            logical_id: "object".into(),
            object: serde_json::json!({
                "apiVersion":"v1", "kind":"ConfigMap",
                "metadata":{"namespace":"demo", "name":"object"}
            }),
            readiness: None,
        }],
        dependencies: vec![],
        automations: vec![],
        diagnostics: vec![],
    })
    .expect("render output")
}

fn desired_observation() -> Vec<DesiredObservationV0> {
    vec![DesiredObservationV0 {
        identity: ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "object".into(),
        },
        observation: Observation::Absent,
        qualification: BackendQualification::BuiltInTested {
            kubernetes_minor: "1.36".into(),
            gvr: "configmaps".into(),
        },
        recovery_proof: None,
        deletion_recovery_proof: None,
    }]
}

fn prepared_state() -> MachineStateV0 {
    let started = reduce(linked_machine(), EventV0::Start).expect("start");
    let descriptor_token = match &started.commands[..] {
        [CommandV0::RequestDescriptor { token, .. }] => token.clone(),
        commands => panic!("expected descriptor request, got {commands:?}"),
    };
    let descriptor = reduce(
        started.state,
        EventV0::DescriptorReady {
            token: descriptor_token,
            bundle_digest: BUNDLE_DIGEST.into(),
        },
    )
    .expect("descriptor");
    let (status_token, old_root_resource_version) = match &descriptor.commands[..] {
        [
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            },
        ] => (token.clone(), old_root_resource_version.clone()),
        commands => panic!("expected descriptor status, got {commands:?}"),
    };
    let status = reduce(
        descriptor.state,
        EventV0::RootStatusPersisted {
            token: status_token,
            old_root_resource_version,
            new_root_resource_version: "21".into(),
            root_uid: ROOT_UID.into(),
        },
    )
    .expect("status acknowledgement");
    let render = reduce(status.state, EventV0::InformersReady).expect("informer readiness");
    let preconditions = match &render.commands[..] {
        [CommandV0::RequestRender { preconditions }] => preconditions.clone(),
        commands => panic!("expected render request, got {commands:?}"),
    };
    // The public reducer seam does not expose the controller's informer token
    // setter; the render acknowledgement itself carries the authenticated
    // barrier token and persists it in the prepared journal.
    let input = render_input(&render.state, "observation-1", "1");
    let output = desired_output();
    let observed = desired_observation();
    let frozen = FrozenInputV0 {
        root_generation: render.state.root().generation,
        state_resource_version: preconditions.state_resource_version.clone(),
        observation_token: "observation-1".into(),
        cache_epoch: "1".into(),
        final_observation_fence_token: "f".repeat(64),
        render_input_hash: parse_validate_render_input(&input)
            .expect("canonical render input")
            .sha256,
    };
    let rendered = reduce(
        render.state,
        EventV0::Rendered {
            frozen: frozen.clone(),
            render_input_json: input.clone(),
            render_output_json: output.clone(),
            observed: observed.clone(),
        },
    )
    .expect("render transition");
    let prepared = match &rendered.commands[..] {
        [CommandV0::Persist { operation, .. }] if operation.purpose == PersistPurpose::Prepare => {
            rendered
        }
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
            .expect("render status acknowledgement");
            reduce(
                status.state,
                EventV0::Rendered {
                    frozen,
                    render_input_json: input,
                    render_output_json: output,
                    observed,
                },
            )
            .expect("render after status")
        }
        commands => panic!("unexpected render continuation: {commands:?}"),
    };
    assert!(matches!(
        prepared.commands.as_slice(),
        [CommandV0::Persist { operation, .. }] if operation.purpose == PersistPurpose::Prepare
    ));
    prepared.state
}

#[test]
#[allow(clippy::too_many_lines)]
fn prepared_not_issued_restart_requires_fresh_render_authorization_before_mutate() {
    let encoded = serde_json::to_string(&prepared_state()).expect("durable state JSON");
    let recovered = MachineStateV0::recover_json(&encoded).expect("recover durable state");
    assert_eq!(recovered.phase(), PhaseV0::Recovery);
    let started = reduce(recovered, EventV0::Start).expect("restart");
    assert!(matches!(
        started.commands.as_slice(),
        [CommandV0::RequestDescriptor { .. }]
    ));
    assert!(
        !started
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );

    let descriptor_token = match &started.commands[..] {
        [CommandV0::RequestDescriptor { token, .. }] => token.clone(),
        commands => panic!("expected descriptor request, got {commands:?}"),
    };
    let descriptor = reduce(
        started.state,
        EventV0::DescriptorReady {
            token: descriptor_token,
            bundle_digest: BUNDLE_DIGEST.into(),
        },
    )
    .expect("descriptor after restart");
    assert!(
        descriptor.commands.is_empty(),
        "recovered descriptor needs no status replay"
    );
    let rerender = reduce(descriptor.state, EventV0::InformersReady).expect("fresh informer fence");
    assert!(
        !rerender
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );
    let preconditions = match &rerender.commands[..] {
        [CommandV0::RequestRender { preconditions }] => preconditions.clone(),
        commands => panic!("expected fresh render request, got {commands:?}"),
    };
    let input = render_input(&rerender.state, "observation-2", "2");
    let frozen = FrozenInputV0 {
        root_generation: rerender.state.root().generation,
        state_resource_version: preconditions.state_resource_version.clone(),
        observation_token: "observation-2".into(),
        cache_epoch: "2".into(),
        final_observation_fence_token: "f".repeat(64),
        render_input_hash: parse_validate_render_input(&input)
            .expect("canonical fresh input")
            .sha256,
    };
    let first_revalidated = reduce(
        rerender.state,
        EventV0::Revalidated {
            frozen,
            render_input_json: input,
            render_output_json: desired_output(),
            observed: desired_observation(),
        },
    )
    .expect("fresh render revalidation");
    let invalidation = match &first_revalidated.commands[..] {
        [CommandV0::Persist { operation, .. }]
            if operation.purpose == PersistPurpose::Invalidate =>
        {
            operation.clone()
        }
        commands => panic!("expected PreparedNotIssued invalidation, got {commands:?}"),
    };
    assert!(
        !first_revalidated
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );
    let replanned = reduce(
        first_revalidated.state,
        EventV0::Persisted {
            token: invalidation.token,
            old_state_resource_version: invalidation.old_state_resource_version,
            new_state_resource_version: "23".into(),
            state_uid: "state-uid".into(),
        },
    )
    .expect("invalidation acknowledgement");
    assert!(
        !replanned
            .commands
            .iter()
            .any(|command| matches!(command, CommandV0::Mutate { .. }))
    );
    let descriptor_token = match &replanned.commands[..] {
        [CommandV0::RequestDescriptor { token, .. }] => token.clone(),
        commands => panic!("expected descriptor after invalidation, got {commands:?}"),
    };
    let descriptor = reduce(
        replanned.state,
        EventV0::DescriptorReady {
            token: descriptor_token,
            bundle_digest: BUNDLE_DIGEST.into(),
        },
    )
    .expect("descriptor after invalidation");
    let informed = match &descriptor.commands[..] {
        [] => descriptor,
        [
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            },
        ] => reduce(
            descriptor.state,
            EventV0::RootStatusPersisted {
                token: token.clone(),
                old_root_resource_version: old_root_resource_version.clone(),
                new_root_resource_version: "22".into(),
                root_uid: ROOT_UID.into(),
            },
        )
        .expect("replanned status acknowledgement"),
        commands => panic!("unexpected descriptor continuation: {commands:?}"),
    };
    let render = reduce(informed.state, EventV0::InformersReady).expect("replanned readiness");
    let render_preconditions = match &render.commands[..] {
        [CommandV0::RequestRender { preconditions }] => preconditions.clone(),
        commands => panic!("expected render after invalidation, got {commands:?}"),
    };
    let render_input_json = render_input(&render.state, "observation-3", "3");
    let render_frozen = FrozenInputV0 {
        root_generation: render.state.root().generation,
        state_resource_version: render_preconditions.state_resource_version.clone(),
        observation_token: "observation-3".into(),
        cache_epoch: "3".into(),
        final_observation_fence_token: "f".repeat(64),
        render_input_hash: parse_validate_render_input(&render_input_json)
            .expect("canonical replanned input")
            .sha256,
    };
    let rendered_again = reduce(
        render.state,
        EventV0::Rendered {
            frozen: render_frozen.clone(),
            render_input_json: render_input_json.clone(),
            render_output_json: desired_output(),
            observed: desired_observation(),
        },
    )
    .expect("replanned render");
    let prepared_again = match &rendered_again.commands[..] {
        [CommandV0::Persist { operation, .. }] if operation.purpose == PersistPurpose::Prepare => {
            rendered_again
        }
        [
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            },
        ] => {
            let status = reduce(
                rendered_again.state,
                EventV0::RootStatusPersisted {
                    token: token.clone(),
                    old_root_resource_version: old_root_resource_version.clone(),
                    new_root_resource_version: "23".into(),
                    root_uid: ROOT_UID.into(),
                },
            )
            .expect("replanned render status acknowledgement");
            let retry_preconditions = match &status.commands[..] {
                [CommandV0::RequestRender { preconditions }] => preconditions.clone(),
                commands => panic!("expected render retry after status, got {commands:?}"),
            };
            let retry_input = render_input(&status.state, "observation-3", "3");
            let retry_frozen = FrozenInputV0 {
                root_generation: status.state.root().generation,
                state_resource_version: retry_preconditions.state_resource_version,
                observation_token: "observation-3".into(),
                cache_epoch: "3".into(),
                final_observation_fence_token: "f".repeat(64),
                render_input_hash: parse_validate_render_input(&retry_input)
                    .expect("canonical render retry input")
                    .sha256,
            };
            reduce(
                status.state,
                EventV0::Rendered {
                    frozen: retry_frozen,
                    render_input_json: retry_input,
                    render_output_json: desired_output(),
                    observed: desired_observation(),
                },
            )
            .expect("replanned render after status")
        }
        commands => panic!("unexpected replan continuation: {commands:?}"),
    };
    let prepare = match &prepared_again.commands[..] {
        [CommandV0::Persist { operation, .. }] if operation.purpose == PersistPurpose::Prepare => {
            operation.clone()
        }
        commands => panic!("expected Prepare after invalidation, got {commands:?}"),
    };
    let revalidate = reduce(
        prepared_again.state,
        EventV0::Persisted {
            token: prepare.token,
            old_state_resource_version: prepare.old_state_resource_version,
            new_state_resource_version: "24".into(),
            state_uid: "state-uid".into(),
        },
    )
    .expect("replanned Prepare acknowledgement");
    let revalidate_preconditions = match &revalidate.commands[..] {
        [CommandV0::RequestRender { preconditions }] => preconditions.clone(),
        commands => panic!("expected revalidation render, got {commands:?}"),
    };
    let revalidate_input = render_input(&revalidate.state, "observation-3", "3");
    let revalidate_frozen = FrozenInputV0 {
        root_generation: revalidate.state.root().generation,
        state_resource_version: revalidate_preconditions.state_resource_version.clone(),
        observation_token: "observation-3".into(),
        cache_epoch: "3".into(),
        final_observation_fence_token: "f".repeat(64),
        render_input_hash: parse_validate_render_input(&revalidate_input)
            .expect("canonical revalidation input")
            .sha256,
    };
    let revalidated = reduce(
        revalidate.state,
        EventV0::Revalidated {
            frozen: revalidate_frozen,
            render_input_json: revalidate_input,
            render_output_json: desired_output(),
            observed: desired_observation(),
        },
    )
    .expect("fresh render revalidation after replan");
    let authorization = match &revalidated.commands[..] {
        [CommandV0::Persist { operation, .. }]
            if operation.purpose == PersistPurpose::DispatchAuthorization =>
        {
            operation.clone()
        }
        commands => panic!("expected dispatch authorization after replan, got {commands:?}"),
    };
    let authorized = reduce(
        revalidated.state,
        EventV0::Persisted {
            token: authorization.token,
            old_state_resource_version: authorization.old_state_resource_version,
            new_state_resource_version: "25".into(),
            state_uid: "state-uid".into(),
        },
    )
    .expect("dispatch authorization acknowledgement");
    assert!(matches!(
        authorized.commands.as_slice(),
        [CommandV0::Mutate { .. }]
    ));
}
