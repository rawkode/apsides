//! The production, single-root controller driver.
//!
//! This module is intentionally the only place where the three long-lived
//! streams (the nominated root, retained dependencies, and a durable pending
//! `AutomationRun`) are coordinated.  It owns exactly one root actor and
//! never adopts a replacement UID.  Kubernetes effects remain in the sealed
//! dispatcher; renderer and action calls are delegated to their typed ports.

#[cfg(all(feature = "test-failpoints", test))]
use std::future::Future;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    pin::Pin,
};

use oxideloop_bundle_archive::install_archive;
use oxideloop_bundle_contract::{BundleDescriptorV0, BundleStore, MAX_BUNDLE_BYTES};
#[cfg(feature = "test-failpoints")]
use oxideloop_controller_api::PersistPurpose;
use oxideloop_controller_api::{ObjectIdentity, ProgramBundleReference};
use oxideloop_controller_core::{
    ActionTriggerV0, CommandV0, EventV0, MachineStateV0, PhaseV0, PreconditionsV0,
    automation_source_transition_token, retired_state_delete_command_token,
    root_only_owned_children_verify_token,
};
use oxideloop_event_planner::{
    ObjectIdentity as PlannerIdentityV0, ObjectRef as PlannerObjectRefV0, RunCorrelationV0,
};
use oxideloop_kube_transport::{
    AutomationRunExpectationV0, AutomationRunObservationBarrierV0, ConflictProvenanceV0,
    KubeTransportV0, ObjectRefV0, OperationSessionV0, PrewriteGateStateV0, ResourceKindV0,
    TransportErrorV0, WatchEventV0, WatchHandleV0,
};
use oxideloop_renderer_abi::{
    limits::validate_event_metadata,
    model::{ABI_VERSION_V0, Dependency, RenderInputV0, RootProjection},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
#[cfg(all(feature = "test-failpoints", test))]
use tokio::time::{Instant as TokioInstant, sleep, timeout_at};

#[cfg(feature = "test-failpoints")]
use crate::failpoint_uds::{EffectBarrierV0, FailpointControlV0};
use crate::{
    BufferedRetainedWatchV0, CompletedObservationFenceOutcomeV0, FailpointsV0, FixedPointPortV0,
    KubeAutomationObservationPortV0, KubeAutomationPortV0, KubeDeletionObservationPortV0,
    KubeFixedPointPortV0, ModeV0, NoCommandDispositionV0, PrewriteRootAuthenticationV0,
    PrewriteRootWitnessV0, RenderExecutionOutcomeV0, RetainedWatchCollectorV0,
    RetainedWatchCoordinationV0, RootActorV0, RuntimeErrorV0, UnixRendererPortV0,
    dispatch_transport_command_in_operation, drive_automation,
    fresh_root_snapshot_from_transport_prewrite_in_operation, load_machine_from_transport,
    load_machine_from_transport_in_operation,
};
#[cfg(all(feature = "test-failpoints", test))]
use oxideloop_kube_transport::ObjectV0;

/// Explicit production inputs.  There is no archive-file, TCP renderer, or
/// ambient environment configuration path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverConfigV0 {
    pub namespace: String,
    pub root_name: String,
    pub bundle_store: PathBuf,
    pub renderer_unix_socket: PathBuf,
    #[cfg(feature = "test-failpoints")]
    pub failpoint_unix_socket: Option<PathBuf>,
}

impl DriverConfigV0 {
    /// Validates the intentionally small operational surface before opening
    /// the Kubernetes client or a local bundle store.
    pub fn validate(&self) -> Result<(), RuntimeErrorV0> {
        if self.namespace.is_empty()
            || self.root_name.is_empty()
            || !self.renderer_unix_socket.is_absolute()
        {
            return Err(RuntimeErrorV0::Hold(
                "namespace/root name must be nonempty and renderer socket must be absolute".into(),
            ));
        }
        #[cfg(feature = "test-failpoints")]
        if self
            .failpoint_unix_socket
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(RuntimeErrorV0::Hold(
                "failpoint socket must be absolute".into(),
            ));
        }
        Ok(())
    }
}

/// A normal controller lifetime ends only after the reducer accepts its exact
/// `RootFinalizerRemoved` → original-UID-absence sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverExitV0 {
    RootFinalized,
}

/// A terminal API watch fault must not spin the controller before its process
/// supervisor can apply backoff. These are deliberately fixed, local bounds.
const MAX_BARRIER_WATCH_RESTARTS: usize = 3;
const BARRIER_WATCH_RESTART_DELAY_MS: u64 = 10;

enum QuiescentWakeV0 {
    #[cfg(feature = "test-failpoints")]
    FailpointPoll,
    Root(Result<WatchEventV0, TransportErrorV0>),
    Dependency(Result<BufferedRetainedWatchV0, RuntimeErrorV0>),
    Automation(Box<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>),
}

/// The only no-command driver boundary. Keeping this policy separate from the
/// `select!` makes it auditable that a witnessed foreign collision cannot
/// establish or poll an automation watch, while every other non-watchable
/// reducer phase fails before any external wait is entered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NoCommandDriverActionV0 {
    Exit,
    ContinueAfterInformerAdmission,
    Watch {
        watch_automation: bool,
        emitted_observability: bool,
    },
}

fn drive_no_command_disposition<F>(
    active: &mut RootActorV0,
    mut admit_informers: F,
) -> Result<NoCommandDriverActionV0, RuntimeErrorV0>
where
    F: FnMut(&mut RootActorV0) -> Result<bool, RuntimeErrorV0>,
{
    match active.no_command_disposition() {
        NoCommandDispositionV0::Terminal => Ok(NoCommandDriverActionV0::Exit),
        NoCommandDispositionV0::AdmitInformers => {
            active.emit_observability("waiting_informers", None);
            if !admit_informers(active)? {
                active.emit_observability("illegal_quiescence", None);
                let reason: String = "informer readiness admission made no progress".into();
                active.hold(reason.clone());
                return Err(RuntimeErrorV0::Hold(reason));
            }
            // Admission queues exactly one sealed readiness; the next turn
            // must reduce it into RequestRender rather than entering a watch.
            Ok(NoCommandDriverActionV0::ContinueAfterInformerAdmission)
        }
        NoCommandDispositionV0::WatchReady => Ok(NoCommandDriverActionV0::Watch {
            watch_automation: true,
            emitted_observability: active.emit_ready_if_quiescent(),
        }),
        NoCommandDispositionV0::WatchBlockedForeignCollision => {
            Ok(NoCommandDriverActionV0::Watch {
                watch_automation: false,
                emitted_observability: active.emit_blocked_collision_if_new(),
            })
        }
        NoCommandDispositionV0::IllegalQuiescence => {
            active.emit_observability("illegal_quiescence", None);
            let reason = format!(
                "illegal quiescence: phase {:?} produced no command",
                active.state().phase()
            );
            active.hold(reason.clone());
            Err(RuntimeErrorV0::Hold(reason))
        }
    }
}

/// Single-replica process driver.  One instance is deliberately one exact
/// `(namespace, rootName)` authority boundary; running another instance for a
/// different root requires another process/replica boundary.
pub struct ControllerDriverV0 {
    config: DriverConfigV0,
    transport: KubeTransportV0,
    store: BundleStore,
}

impl ControllerDriverV0 {
    /// Builds the production driver from an already-created in-cluster
    /// transport.  Keeping construction separate makes the orchestration
    /// testable without opening an ambient client in a unit test.
    pub fn new(config: DriverConfigV0, transport: KubeTransportV0) -> Result<Self, RuntimeErrorV0> {
        config.validate()?;
        let store = BundleStore::new(&config.bundle_store)?;
        Ok(Self {
            config,
            transport,
            store,
        })
    }

    /// Creates the only supported production Kubernetes transport.
    pub async fn in_cluster(config: DriverConfigV0) -> Result<Self, RuntimeErrorV0> {
        let transport = KubeTransportV0::in_cluster().await?;
        Self::new(config, transport)
    }

    /// Runs until the exact original root has completed deletion.  An absent
    /// root is not success: the process retains an exact root collection watch
    /// and starts only when the nominated name appears.
    #[allow(clippy::too_many_lines)]
    pub async fn run(self) -> Result<DriverExitV0, RuntimeErrorV0> {
        // Bind the debug-only control before the first root barrier so an e2e
        // client can arm a narrow effect seam without racing startup I/O.
        #[cfg(feature = "test-failpoints")]
        let mut failpoint_listener = self
            .config
            .failpoint_unix_socket
            .as_ref()
            .map(crate::failpoint_uds::FailpointUdsListenerV0::bind)
            .transpose()
            .map_err(|error| RuntimeErrorV0::Hold(format!("invalid failpoint socket: {error}")))?;
        let (mut root_watch, initial_machine) =
            establish_root_barrier(&self.transport, &self.config).await?;
        let mut actor = initial_machine.map(start_actor);
        if let Some(message) = actor
            .as_ref()
            .and_then(|active| deletion_recovery_observability(&self.config, active.state()))
        {
            eprintln!("{message}");
        }
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, UnixRendererPortV0>> = None;
        let mut automation_watch: Option<AutomationRunObservationBarrierV0> = None;
        let mut descriptors = BTreeMap::<String, BundleDescriptorV0>::new();
        // These are derived only from a completed final render fence.  They
        // remain local to this process and are pinned to the exact root and
        // retained-observation barrier that authenticated the render.  Any
        // root/dependency/render supersession drops them before a trigger can
        // become reducer input.
        let mut automation_candidates = VecDeque::<QueuedAutomationV0>::new();
        let mut failpoints = FailpointsV0::default();
        #[cfg(feature = "test-failpoints")]
        let mut prepare_barrier = None;
        #[cfg(feature = "test-failpoints")]
        let mut deletion_barrier = None;
        #[cfg(feature = "test-failpoints")]
        for _ in 0..10 {
            let Some(listener) = failpoint_listener.as_mut() else {
                break;
            };
            let control = listener
                .poll_control()
                .map_err(|error| RuntimeErrorV0::Hold(format!("failpoint arm failed: {error}")))?;
            if listener.is_consumed() {
                failpoint_listener = None;
            }
            match control {
                Some(FailpointControlV0::Arm(arm)) => {
                    failpoints.arm(arm.point())?;
                    arm.acknowledge().map_err(|error| {
                        RuntimeErrorV0::Hold(format!("failpoint acknowledgement failed: {error}"))
                    })?;
                    break;
                }
                Some(FailpointControlV0::PrepareBarrier(mut barrier)) => {
                    barrier.acknowledge().map_err(|error| {
                        RuntimeErrorV0::Hold(format!("failpoint acknowledgement failed: {error}"))
                    })?;
                    prepare_barrier = Some(barrier);
                    break;
                }
                Some(FailpointControlV0::DeletionBarrier(mut barrier)) => {
                    barrier.acknowledge().map_err(|error| {
                        RuntimeErrorV0::Hold(format!("failpoint acknowledgement failed: {error}"))
                    })?;
                    deletion_barrier = Some(barrier);
                    break;
                }
                None => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }

        loop {
            // Debug control is checked before every root-watch drain and
            // command choice. It is nonblocking until the exact deletion
            // midpoint, so an armed test cannot be stranded behind an idle
            // controller or an unrelated action execution.
            #[cfg(feature = "test-failpoints")]
            if let Some(listener) = failpoint_listener.as_mut() {
                let control = listener.poll_control().map_err(|error| {
                    RuntimeErrorV0::Hold(format!("failpoint arm failed: {error}"))
                })?;
                if listener.is_consumed() {
                    failpoint_listener = None;
                }
                match control {
                    Some(FailpointControlV0::Arm(arm)) => {
                        failpoints.arm(arm.point())?;
                        arm.acknowledge().map_err(|error| {
                            RuntimeErrorV0::Hold(format!(
                                "failpoint acknowledgement failed: {error}"
                            ))
                        })?;
                    }
                    Some(FailpointControlV0::PrepareBarrier(mut barrier)) => {
                        barrier.acknowledge().map_err(|error| {
                            RuntimeErrorV0::Hold(format!(
                                "failpoint acknowledgement failed: {error}"
                            ))
                        })?;
                        prepare_barrier = Some(barrier);
                    }
                    Some(FailpointControlV0::DeletionBarrier(mut barrier)) => {
                        barrier.acknowledge().map_err(|error| {
                            RuntimeErrorV0::Hold(format!(
                                "failpoint acknowledgement failed: {error}"
                            ))
                        })?;
                        deletion_barrier = Some(barrier);
                    }
                    None => {}
                }
            }
            let Some(active) = actor.as_mut() else {
                #[cfg(feature = "test-failpoints")]
                let Ok(root_event) = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    root_watch.next_event(),
                )
                .await
                else {
                    continue;
                };
                #[cfg(not(feature = "test-failpoints"))]
                let root_event = root_watch.next_event().await;
                match root_event {
                    Ok(event) => {
                        if watch_targets_root(&event, &self.config.root_name)
                            && let Some(machine) = load_machine_from_transport(
                                &self.transport,
                                &self.config.namespace,
                                &self.config.root_name,
                            )
                            .await?
                        {
                            actor = Some(start_actor(machine));
                        }
                    }
                    Err(error) if is_root_watch_terminal(&error) => {
                        let (watch, fresh) =
                            establish_root_barrier(&self.transport, &self.config).await?;
                        root_watch = watch;
                        if let Some(machine) = fresh {
                            actor = Some(start_actor(machine));
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
                continue;
            };

            // Root observations are coalesced before any new command.  A
            // changed UID is never adopted; the active actor enters Hold.
            let root_barrier_before_drain = active.barrier();
            match drain_root_watch(&mut root_watch, active, &self.transport, &self.config).await? {
                RootWatchDrainV0::Current => {}
                RootWatchDrainV0::DeletionRecovery => {
                    clear_root_only_recovery_ephemera(
                        &mut fixed_point,
                        &mut automation_watch,
                        &mut automation_candidates,
                    );
                    descriptors.clear();
                    continue;
                }
                RootWatchDrainV0::Invalidated => {
                    let (watch, fresh) =
                        establish_root_barrier(&self.transport, &self.config).await?;
                    root_watch = watch;
                    if refresh_actor_from_machine(active, fresh)? {
                        clear_root_only_recovery_ephemera(
                            &mut fixed_point,
                            &mut automation_watch,
                            &mut automation_candidates,
                        );
                        descriptors.clear();
                        if let Some(message) =
                            deletion_recovery_observability(&self.config, active.state())
                        {
                            eprintln!("{message}");
                        }
                        continue;
                    }
                }
            }
            if active.barrier() != root_barrier_before_drain
                && let Some(port) = fixed_point.as_mut()
            {
                // Root list/watch/GET authority changed.  No retained
                // dependency snapshot from the prior root barrier may survive.
                port.invalidate_to_barrier(active.barrier());
            }
            automation_candidates.retain(|candidate| candidate.matches(active));
            if matches!(active.mode(), ModeV0::Terminal) {
                return Ok(DriverExitV0::RootFinalized);
            }

            match active.next_command()? {
                Some(CommandV0::RequestDescriptor {
                    token,
                    preconditions,
                }) => {
                    let capability = active.issue_descriptor_capability()?;
                    if capability.token() != token || capability.preconditions() != &preconditions {
                        return Err(RuntimeErrorV0::Hold(
                            "descriptor command pins changed".into(),
                        ));
                    }
                    let program = capability.program().clone();
                    let (descriptor, buffered) = match collect_while_observing(
                        descriptor_for_program(
                            &self.transport,
                            &self.store,
                            &self.config.namespace,
                            &program,
                        ),
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &self.config.root_name,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => {
                            let reason =
                                format!("verified bundle descriptor is unavailable: {error}");
                            active.hold(reason.clone());
                            return Err(RuntimeErrorV0::Hold(reason));
                        }
                    };
                    if descriptor_for_digest(&descriptors, &active.state().root().bundle_digest)
                        .is_none()
                    {
                        descriptors.insert(
                            active.state().root().bundle_digest.clone(),
                            descriptor.clone(),
                        );
                    }
                    fixed_point = Some(KubeFixedPointPortV0::new(
                        &self.transport,
                        UnixRendererPortV0::new(self.config.renderer_unix_socket.clone()),
                        descriptor,
                        active.barrier(),
                    ));
                    active.acknowledge(EventV0::DescriptorReady {
                        token: capability.token().into(),
                        bundle_digest: capability.bundle_digest().into(),
                    })?;
                    apply_buffered_watches(
                        active,
                        &self.transport,
                        &self.config,
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &mut automation_candidates,
                        buffered,
                    )
                    .await?;
                }
                Some(CommandV0::RequestDeletionObservation { .. }) => {
                    let capability = active.issue_deletion_capability()?;
                    let deletion =
                        KubeDeletionObservationPortV0::new(&self.transport, root_address(active)?);
                    let (completion, buffered) = Box::pin(collect_while_observing(
                        deletion.observe_for_capability(capability),
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &self.config.root_name,
                    ))
                    .await?;
                    let observation =
                        KubeDeletionObservationPortV0::consume_completion(active, completion)?;
                    active.acknowledge_deletion_observation(observation)?;
                    apply_buffered_watches(
                        active,
                        &self.transport,
                        &self.config,
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &mut automation_candidates,
                        buffered,
                    )
                    .await?;
                }
                Some(CommandV0::RequestRender { preconditions }) => {
                    if active.state().root().deleting {
                        active.hold("deleting root cannot execute a render command".into());
                        return Err(RuntimeErrorV0::Hold(
                            "deleting root cannot execute a render command".into(),
                        ));
                    }
                    let mut port = fixed_point.take().ok_or_else(|| {
                        RuntimeErrorV0::Hold("render requested before a verified descriptor".into())
                    })?;
                    port.set_barrier(active.barrier());
                    let capability = active.issue_render_capability(
                        render_input(active, &preconditions)?,
                        port.identity(),
                    )?;
                    // Split the concrete port before polling the future. The
                    // execution half performs renderer/discovery I/O while
                    // the collector owns every retained pre-render watch;
                    // neither borrows the actor and neither can be cloned.
                    let (mut execution, mut retained, reunion) = port.split_for_render();
                    let (execution_result, mut buffered) = collect_render_while_observing(
                        execution.reconcile_for_capability(capability),
                        &mut root_watch,
                        &mut retained,
                        &mut automation_watch,
                        &self.config.root_name,
                    )
                    .await;
                    // The effect future has resolved (success or failure),
                    // so rejoin before any actor mutation.  A mismatched
                    // concrete identity/barrier cannot manufacture a final
                    // fence and is rejected inside the sealed adapter.
                    let reunited =
                        KubeFixedPointPortV0::reunite_after_render(execution, retained, reunion)?;
                    fixed_point = Some(reunited);
                    let outcome = match execution_result {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            if let Some(pre_finalization) =
                                buffered.take_root_dependency_pre_finalization()
                            {
                                apply_buffered_watches(
                                    active,
                                    &self.transport,
                                    &self.config,
                                    &mut root_watch,
                                    &mut fixed_point,
                                    &mut automation_watch,
                                    &mut automation_candidates,
                                    pre_finalization,
                                )
                                .await?;
                                if !active.owns_render_request() {
                                    continue;
                                }
                            }
                            apply_buffered_watches(
                                active,
                                &self.transport,
                                &self.config,
                                &mut root_watch,
                                &mut fixed_point,
                                &mut automation_watch,
                                &mut automation_candidates,
                                buffered,
                            )
                            .await?;
                            match error {
                                RuntimeErrorV0::Discovery(error)
                                    if is_dependency_watch_terminal(&error) =>
                                {
                                    active.watch_invalidated();
                                    if let Some(port) = fixed_point.as_mut() {
                                        // Reuniting after a failed discovery retains the old
                                        // handles. The actor's terminal-watch restart advanced
                                        // its authority, so they cannot gate the next initial
                                        // informer admission.
                                        port.invalidate_to_barrier(active.barrier());
                                    }
                                    continue;
                                }
                                error => return Err(error),
                            }
                        }
                    };
                    let mut result = match outcome {
                        RenderExecutionOutcomeV0::Stable(result) => *result,
                        RenderExecutionOutcomeV0::CoverageChanged(outcome) => {
                            let _ = consume_coverage_changed_after_reunion(
                                active,
                                &self.transport,
                                &self.config,
                                &mut root_watch,
                                &mut fixed_point,
                                &mut automation_watch,
                                &mut automation_candidates,
                                buffered,
                                outcome,
                            )
                            .await?;
                            continue;
                        }
                    };
                    if let Some(pre_finalization) = buffered.take_root_dependency_pre_finalization()
                    {
                        apply_buffered_watches(
                            active,
                            &self.transport,
                            &self.config,
                            &mut root_watch,
                            &mut fixed_point,
                            &mut automation_watch,
                            &mut automation_candidates,
                            pre_finalization,
                        )
                        .await?;
                        if !active.owns_render_request() {
                            continue;
                        }
                    }
                    fixed_point
                        .as_ref()
                        .ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "reunited render port disappeared before final input binding"
                                    .into(),
                            )
                        })?
                        .bind_completed_fixed_point_input(active, &result)?;
                    let port = fixed_point.as_mut().ok_or_else(|| {
                        RuntimeErrorV0::Hold("reunited render port disappeared".into())
                    })?;
                    match port.complete_observation_fence(active, &mut result)? {
                        CompletedObservationFenceOutcomeV0::Completed(observation) => {
                            let descriptor = descriptor_for_digest(
                                &descriptors,
                                &active.state().root().bundle_digest,
                            )
                            .ok_or_else(|| {
                                RuntimeErrorV0::Hold(
                                    "completed render has no verified descriptor for its bundle"
                                        .into(),
                                )
                            })?;
                            // The render result is now stable and its full
                            // retained-observation fence completed, but the
                            // actor still owns the exact render command.  A
                            // candidate queue is therefore discardable until
                            // the sealed render acknowledgement reduces and
                            // all resulting durable work reaches Ready.
                            automation_candidates = automation_candidates_from_final_render(
                                active,
                                result.input(),
                                result.output(),
                                result.dependencies(),
                                descriptor,
                            )?;
                            active.acknowledge_render_observation(*observation)?;
                            apply_buffered_watches(
                                active,
                                &self.transport,
                                &self.config,
                                &mut root_watch,
                                &mut fixed_point,
                                &mut automation_watch,
                                &mut automation_candidates,
                                buffered,
                            )
                            .await?;
                        }
                        CompletedObservationFenceOutcomeV0::ReconcileScheduled
                        | CompletedObservationFenceOutcomeV0::DuplicateIgnored
                        | CompletedObservationFenceOutcomeV0::RestartRequired => {
                            automation_candidates.clear();
                            // A final-fence watch transition consumed the
                            // observation-only render slot. Durable pending
                            // run facts still resume from their validated
                            // list/watch/GET barrier after that resolution.
                            apply_buffered_watches(
                                active,
                                &self.transport,
                                &self.config,
                                &mut root_watch,
                                &mut fixed_point,
                                &mut automation_watch,
                                &mut automation_candidates,
                                buffered,
                            )
                            .await?;
                        }
                    }
                }
                Some(CommandV0::Hold { reason }) => {
                    active.hold(reason.clone());
                    return Err(RuntimeErrorV0::Hold(reason));
                }
                Some(CommandV0::IgnoredStale { .. }) => {
                    active.consume_internal();
                }
                Some(command) => {
                    #[cfg(feature = "test-failpoints")]
                    reject_late_deletion_barrier(&command, deletion_barrier.is_some())?;
                    #[cfg(feature = "test-failpoints")]
                    reject_late_prepare_barrier(&command, prepare_barrier.is_some())?;
                    // Bootstrap has no actor-snapshot duplicate path: its
                    // pre-I/O authority is exclusively the transport's sealed
                    // full command/address/raw-baseline fence.
                    let prewrite_root_witness =
                        (!matches!(command, CommandV0::EnsureBootstrap { .. }))
                            .then(|| active.prewrite_root_witness());
                    let (completion, buffered) = match execute_kubernetes_command_while_observing(
                        &self.transport,
                        &root_address(active)?,
                        &command,
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &self.config.root_name,
                        prewrite_root_witness,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(RuntimeErrorV0::Discovery(TransportErrorV0::Stale(_))) => {
                            let fresh = load_machine_from_transport(
                                &self.transport,
                                &self.config.namespace,
                                &self.config.root_name,
                            )
                            .await?
                            .ok_or_else(|| {
                                RuntimeErrorV0::Hold(
                                    "root disappeared during stale command recovery".into(),
                                )
                            })?;
                            // The failed stale preflight did not acknowledge
                            // the owned command.  Preserve its durable state,
                            // discard only stale observation authority, and
                            // let the actor issue the exact rebased command.
                            if recover_stale_command_actor(active, &fresh) {
                                clear_root_only_recovery_ephemera(
                                    &mut fixed_point,
                                    &mut automation_watch,
                                    &mut automation_candidates,
                                );
                                descriptors.clear();
                            }
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if matches!(
                        &completion,
                        KubernetesEffectCompletionV0::FreshBootstrapCutInvalidated
                    ) {
                        if rebarrier_after_fresh_bootstrap_cut(
                            active,
                            &command,
                            &self.transport,
                            &self.config,
                            &mut root_watch,
                            &mut fixed_point,
                            &mut automation_watch,
                            &mut automation_candidates,
                        )
                        .await?
                            == FreshBootstrapCutRebarrierV0::ActorRetired
                        {
                            actor = None;
                            continue;
                        }
                        continue;
                    }
                    if let KubernetesEffectCompletionV0::PreWriteInvalidated {
                        capability,
                        root_authentication,
                    } = completion
                    {
                        // The collector won the pre-poll race.  It gives us
                        // the exact transport session and retained facts, but
                        // no reducer acknowledgement: releasing this slot
                        // cannot advance persistence or manufacture a result.
                        if capability.prewrite_gate().state() != PrewriteGateStateV0::Invalidated {
                            active.hold("prewrite capability was not invalidated".into());
                            return Err(RuntimeErrorV0::Hold(
                                "prewrite capability was not invalidated".into(),
                            ));
                        }
                        active.release_unpolled_command(&command)?;
                        apply_buffered_watches(
                            active,
                            &self.transport,
                            &self.config,
                            &mut root_watch,
                            &mut fixed_point,
                            &mut automation_watch,
                            &mut automation_candidates,
                            buffered,
                        )
                        .await?;
                        // `Mutate` follows durable DispatchAuthorization and
                        // every other effect has an unacknowledged reducer
                        // phase.  No durable rollback or fresh-budget retry
                        // is representable here.  Buffered authority was
                        // reduced above; stop before the actor can select an
                        // old or reconstructed command.
                        let reason: String = match root_authentication {
                            Some(RootAuthenticationOutcomeV0::Genuine) =>
                                "fresh root authentication invalidated an unpolled Kubernetes command; refusing replay",
                            Some(RootAuthenticationOutcomeV0::Ambiguous) =>
                                "root authentication was ambiguous before an unpolled Kubernetes command; refusing replay",
                            None =>
                                "authenticated change invalidated an unpolled Kubernetes command; refusing replay",
                        }.into();
                        active.hold(reason.clone());
                        return Err(RuntimeErrorV0::Hold(reason));
                    }
                    let KubernetesEffectCompletionV0::Acknowledged(event) = completion else {
                        unreachable!("prewrite invalidation handled above");
                    };
                    let event = (*event).ok_or_else(|| {
                        RuntimeErrorV0::Hold(
                            "Kubernetes command did not produce an acknowledgement".into(),
                        )
                    })?;
                    // Exact effect acknowledgement is reducer authority and
                    // must reduce before an observed root closure/rebase.
                    let is_state_finalizer_ack = matches!(
                        (&command, &event),
                        (
                            CommandV0::RemoveStateFinalizer { .. },
                            EventV0::StateFinalizerRemoved { .. }
                        )
                    );
                    let is_retired_state_delete_ack = matches!(
                        (&command, &event),
                        (
                            CommandV0::DeleteRetiredState { .. },
                            EventV0::RetiredStateDeleted { .. }
                        )
                    );
                    active.acknowledge(event)?;
                    if is_state_finalizer_ack {
                        // Reduce the exact acknowledgement far enough to
                        // establish the one pending tombstone DELETE, but do
                        // not poll it. Reclassify raw Kubernetes state and
                        // replace the whole actor so no linked renderer,
                        // dependency, collision, automation, dispatch, or
                        // mutation ephemera can cross the authority cut.
                        let pending_delete = active.next_command()?.ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "state-finalizer acknowledgement did not queue retired State DELETE"
                                    .into(),
                            )
                        })?;
                        let fresh = load_machine_from_transport(
                            &self.transport,
                            &self.config.namespace,
                            &self.config.root_name,
                        )
                        .await?
                        .ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "root disappeared while classifying retired State".into(),
                            )
                        })?;
                        validate_deletion_authority_cut(
                            DeletionAuthorityCutV0::LinkedToRetired,
                            active.state(),
                            &fresh,
                            &pending_delete,
                        )?;
                        #[cfg(feature = "test-failpoints")]
                        if deletion_barrier.is_some() {
                            if !is_exact_retired_state_delete(&pending_delete, active.state()) {
                                return Err(RuntimeErrorV0::Hold(
                                    "armed deletion barrier did not reach retired State authority"
                                        .into(),
                                ));
                            }
                            let barrier = deletion_barrier.take().ok_or_else(|| {
                                RuntimeErrorV0::Hold(
                                    "armed deletion barrier disappeared before interception".into(),
                                )
                            })?;
                            verify_deletion_barrier_and_wait(
                                barrier,
                                &pending_delete,
                                active,
                                &fresh,
                            )?;
                            return Err(RuntimeErrorV0::InjectedCrash(
                                crate::FailpointV0::AfterStateFinalizerBeforeRootFinalizer,
                            ));
                        }
                        *active = start_actor(fresh);
                        clear_root_only_recovery_ephemera(
                            &mut fixed_point,
                            &mut automation_watch,
                            &mut automation_candidates,
                        );
                        descriptors.clear();
                        if let Some(message) =
                            deletion_recovery_observability(&self.config, active.state())
                        {
                            eprintln!("{message}");
                        }
                        continue;
                    }
                    if is_retired_state_delete_ack {
                        // The 404-proven tombstone acknowledgement is the
                        // retired-to-root-only cut. Reduce only enough to
                        // observe the new root-only child proof, then reload
                        // and replace the actor before that proof can poll.
                        let pending_verify = active.next_command()?.ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "retired State deletion acknowledgement did not queue root-only child proof"
                                    .into(),
                            )
                        })?;
                        let fresh = load_machine_from_transport(
                            &self.transport,
                            &self.config.namespace,
                            &self.config.root_name,
                        )
                        .await?
                        .ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "root disappeared while classifying root-only deletion recovery"
                                    .into(),
                            )
                        })?;
                        validate_deletion_authority_cut(
                            DeletionAuthorityCutV0::RetiredToRootOnly,
                            active.state(),
                            &fresh,
                            &pending_verify,
                        )?;
                        *active = start_actor(fresh);
                        clear_root_only_recovery_ephemera(
                            &mut fixed_point,
                            &mut automation_watch,
                            &mut automation_candidates,
                        );
                        descriptors.clear();
                        if let Some(message) =
                            deletion_recovery_observability(&self.config, active.state())
                        {
                            eprintln!("{message}");
                        }
                        continue;
                    }
                    #[cfg(feature = "test-failpoints")]
                    if prepare_barrier.is_some()
                        && matches!(
                            &command,
                            CommandV0::Persist { operation, .. }
                                if operation.purpose == PersistPurpose::Prepare
                        )
                    {
                        let barrier = prepare_barrier.take().ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "prepared-dispatch barrier disappeared before interception".into(),
                            )
                        })?;
                        tokio::task::block_in_place(|| barrier.reached_and_wait_for_release())
                            .map_err(|error| {
                                RuntimeErrorV0::Hold(format!(
                                    "prepared-dispatch barrier failed: {error}"
                                ))
                            })?;
                        return Err(RuntimeErrorV0::InjectedCrash(
                            crate::FailpointV0::AfterPrepareBeforeDispatchAuthorization,
                        ));
                    }
                    apply_buffered_watches(
                        active,
                        &self.transport,
                        &self.config,
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &mut automation_candidates,
                        buffered,
                    )
                    .await?;
                }
                None => {
                    let action = drive_no_command_disposition(active, |actor| {
                        let port = fixed_point.as_mut().ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "informers pending before a verified fixed-point port".into(),
                            )
                        })?;
                        port.admit_initial_informer_readiness(actor)
                    })?;
                    let blocked_foreign_collision = match action {
                        NoCommandDriverActionV0::Exit => return Ok(DriverExitV0::RootFinalized),
                        NoCommandDriverActionV0::ContinueAfterInformerAdmission => continue,
                        NoCommandDriverActionV0::Watch {
                            watch_automation, ..
                        } => !watch_automation,
                    };
                    // The journal owns pending runs.  Their list/watch/GET
                    // barrier is re-established after a restart, so a run
                    // created before the watch cannot be missed.
                    if !blocked_foreign_collision && automation_watch.is_none() {
                        automation_watch = establish_pending_watch(active, &self.transport).await?;
                    }
                    if !blocked_foreign_collision && let Some(watch) = automation_watch.as_mut() {
                        if let Some(completion) = watch.current().completion() {
                            active.acknowledge_automation_completion(
                                EventV0::AutomationCompleted {
                                    spec: watch.current().spec().clone(),
                                    occurrence_id: watch.current().spec().occurrence_id.clone(),
                                    name: watch.current().reference().name.clone(),
                                    uid: completion.uid().into(),
                                    old_resource_version: completion.old_resource_version().into(),
                                    new_resource_version: completion.new_resource_version().into(),
                                    completion: completion.completion(),
                                    attempts: completion.attempts(),
                                    materialization_name: completion.materialization_name().into(),
                                    materialization_uid: completion.materialization_uid().into(),
                                },
                            )?;
                            automation_watch = None;
                            continue;
                        }
                        if !watch.current().is_pristine()
                            || watch.current().resource_version()
                                != watch.current().pending_resource_version()
                        {
                            active.hold("pending AutomationRun is not pristine at its durable resourceVersion".into());
                            return Err(RuntimeErrorV0::Hold(
                                "pending AutomationRun is not pristine at its durable resourceVersion".into(),
                            ));
                        }
                        if !active.state().root().deleting {
                            let descriptor = descriptor_for_program(
                                &self.transport,
                                &self.store,
                                &self.config.namespace,
                                watch.current().program(),
                            )
                            .await?;
                            descriptors
                                .entry(watch.current().spec().bundle_digest.clone())
                                .or_insert_with(|| descriptor.clone());
                            let mut automation_port = KubeAutomationPortV0 {
                                transport: &self.transport,
                            };
                            let mut renderer =
                                UnixRendererPortV0::new(self.config.renderer_unix_socket.clone());
                            let event = drive_automation(
                                &mut automation_port,
                                &mut renderer,
                                descriptor_for_digest(
                                    &descriptors,
                                    &watch.current().spec().bundle_digest,
                                )
                                .ok_or_else(|| {
                                    RuntimeErrorV0::Hold(
                                        "missing verified action descriptor".into(),
                                    )
                                })?,
                                &automation_execution(active, watch.current())?,
                                watch.current().spec(),
                                &event_time()?,
                                &mut failpoints,
                            )
                            .await?;
                            active.acknowledge_automation_completion(event)?;
                            automation_watch = None;
                            continue;
                        }
                    }

                    // Automations are submitted only after the sealed render
                    // acknowledgement and all of its mutation/persist work
                    // have reached Ready.  `TriggerAutomation` itself
                    // establishes the baseline if necessary; serial delivery
                    // prevents a later declaration from overtaking that
                    // durable baseline/write.
                    if !blocked_foreign_collision
                        && active.state().phase() == PhaseV0::Ready
                        && !active.state().root().deleting
                        && enqueue_next_automation_candidate(active, &mut automation_candidates)
                    {
                        // A candidate was just queued into the actor; do not
                        // consume another one until it completes its own
                        // durable baseline/ensure lifecycle.
                        continue;
                    }

                    // No command is pending.  Keep retained watches alive and
                    // wait for either the exact root or a genuine dependency
                    // transition.  Automation changes are handled on the next
                    // quiescent pass after a root/dependency wakeup.
                    let mut root_only_recovery = false;
                    {
                        let port = fixed_point.as_mut();
                        #[cfg(feature = "test-failpoints")]
                        let wake = tokio::select! {
                            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                                QuiescentWakeV0::FailpointPoll
                            }
                            root = root_watch.next_event() => QuiescentWakeV0::Root(root),
                            change = async {
                                match port {
                                    Some(port) => port.await_buffered_retained_watch().await,
                                    None => std::future::pending::<Result<
                                        BufferedRetainedWatchV0,
                                        RuntimeErrorV0,
                                    >>().await,
                                }
                            } => QuiescentWakeV0::Dependency(change),
                            run = async {
                                if blocked_foreign_collision {
                                    std::future::pending::<Result<
                                        oxideloop_kube_transport::ValidatedAutomationRunV0,
                                        TransportErrorV0,
                                    >>().await
                                } else {
                                    match automation_watch.as_mut() {
                                        Some(watch) => watch.next_change().await,
                                        None => std::future::pending::<Result<
                                            oxideloop_kube_transport::ValidatedAutomationRunV0,
                                            TransportErrorV0,
                                        >>().await,
                                    }
                                }
                            } => QuiescentWakeV0::Automation(Box::new(run)),
                        };
                        #[cfg(not(feature = "test-failpoints"))]
                        let wake = tokio::select! {
                            root = root_watch.next_event() => QuiescentWakeV0::Root(root),
                            change = async {
                                match port {
                                    Some(port) => port.await_buffered_retained_watch().await,
                                    None => std::future::pending::<Result<
                                        BufferedRetainedWatchV0,
                                        RuntimeErrorV0,
                                    >>().await,
                                }
                            } => QuiescentWakeV0::Dependency(change),
                            run = async {
                                if blocked_foreign_collision {
                                    std::future::pending::<Result<
                                        oxideloop_kube_transport::ValidatedAutomationRunV0,
                                        TransportErrorV0,
                                    >>().await
                                } else {
                                    match automation_watch.as_mut() {
                                        Some(watch) => watch.next_change().await,
                                        None => std::future::pending::<Result<
                                            oxideloop_kube_transport::ValidatedAutomationRunV0,
                                            TransportErrorV0,
                                        >>().await,
                                    }
                                }
                            } => QuiescentWakeV0::Automation(Box::new(run)),
                        };
                        match wake {
                            #[cfg(feature = "test-failpoints")]
                            QuiescentWakeV0::FailpointPoll => {}
                            QuiescentWakeV0::Root(root) => match root {
                                Ok(event) if watch_targets_root(&event, &self.config.root_name) => {
                                    let prior_barrier = active.barrier();
                                    root_only_recovery = refresh_actor_from_transport(
                                        active,
                                        &self.transport,
                                        &self.config,
                                    )
                                    .await?;
                                    if !root_only_recovery
                                        && active.barrier() != prior_barrier
                                        && let Some(port) = fixed_point.as_mut()
                                    {
                                        port.invalidate_to_barrier(active.barrier());
                                    }
                                }
                                Ok(_) => {}
                                Err(error) if is_root_watch_terminal(&error) => {
                                    active.watch_invalidated();
                                    let (watch, fresh) =
                                        establish_root_barrier(&self.transport, &self.config)
                                            .await?;
                                    root_watch = watch;
                                    root_only_recovery = refresh_actor_from_machine(active, fresh)?;
                                    if !root_only_recovery && let Some(port) = fixed_point.as_mut()
                                    {
                                        port.invalidate_to_barrier(active.barrier());
                                    }
                                }
                                Err(error) => return Err(error.into()),
                            },
                            QuiescentWakeV0::Dependency(change) => match change {
                                Ok(record) => {
                                    let port = fixed_point.as_mut().ok_or_else(|| {
                                        RuntimeErrorV0::Hold("retained port disappeared".into())
                                    })?;
                                    match port.apply_buffered_retained_watch(active, record)? {
                                        RetainedWatchCoordinationV0::ReconcileScheduled
                                        | RetainedWatchCoordinationV0::DuplicateIgnored
                                        | RetainedWatchCoordinationV0::RestartRequired => {}
                                    }
                                }
                                Err(error) => return Err(error),
                            },
                            QuiescentWakeV0::Automation(run) => match *run {
                                Ok(_) => {}
                                Err(error) if is_automation_watch_terminal(&error) => {
                                    automation_watch = None;
                                }
                                Err(error) => return Err(error.into()),
                            },
                        }
                    }
                    if root_only_recovery {
                        clear_root_only_recovery_ephemera(
                            &mut fixed_point,
                            &mut automation_watch,
                            &mut automation_candidates,
                        );
                        descriptors.clear();
                    }
                }
            }
        }
    }
}

/// Resolves a sealed D-projection mismatch only after every completion-edge
/// root/dependency record has been reduced. A buffered supersession therefore
/// makes the opaque outcome stale instead of letting it mint another change.
#[allow(clippy::too_many_arguments)]
async fn consume_coverage_changed_after_reunion<R>(
    actor: &mut RootActorV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    automation_candidates: &mut VecDeque<QueuedAutomationV0>,
    buffered: BufferedCommandWatchesV0,
    outcome: crate::CoverageChangedV0,
) -> Result<bool, RuntimeErrorV0> {
    apply_buffered_watches(
        actor,
        transport,
        config,
        root_watch,
        fixed_point,
        automation_watch,
        automation_candidates,
        buffered,
    )
    .await?;
    if !actor.owns_render_request() {
        return Ok(false);
    }
    fixed_point
        .as_mut()
        .ok_or_else(|| {
            RuntimeErrorV0::Hold(
                "reunited render port disappeared before coverage-change consumption".into(),
            )
        })?
        .consume_coverage_change(actor, outcome)?;
    Ok(true)
}

/// Watch records accumulated while one exact Kubernetes command future owns
/// the process.  None of these records is reducer authority on its own: they
/// are applied only after `effect` has acknowledged successfully.
struct BufferedCommandWatchesV0 {
    root: Option<BufferedRootWatchV0>,
    dependency: Option<BufferedRetainedWatchV0>,
    automation: Option<BufferedAutomationWatchV0>,
    root_faulted: bool,
    dependency_faulted: bool,
    automation_faulted: bool,
    fault: Option<String>,
    /// A transport-minted capability proving that an authenticated concurrent
    /// change won before the owned command ever polled its write.  T2 consumes
    /// this together with these buffered facts; the reducer is not notified
    /// here and the command is never replayed by this collector.
    prewrite_invalidated: Option<OperationSessionV0>,
}

/// The sole non-acknowledging completion a pinned Kubernetes effect may
/// return.  Its session is transport-minted and is checked again by the
/// driver before it releases the actor-local slot.  It is never reduced as a
/// core event.
enum KubernetesEffectCompletionV0 {
    Acknowledged(Box<Option<EventV0>>),
    /// The operation-owned exact LIST/WATCH/REREAD cut changed before the
    /// fence could be sealed. No preflight or write was polled, so an
    /// `EnsureBootstrap` slot may be released only into a fresh ordinary root
    /// barrier; it is intentionally distinct from an authenticated
    /// prewrite-invalidated command, which must stop rather than replay.
    FreshBootstrapCutInvalidated,
    PreWriteInvalidated {
        capability: OperationSessionV0,
        root_authentication: Option<RootAuthenticationOutcomeV0>,
    },
}

/// Runtime-only provenance for the no-write collector completion.  It is not
/// a reducer event: buffered observations are still reduced through their
/// ordinary path before the driver stops the actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootAuthenticationOutcomeV0 {
    Genuine,
    Ambiguous,
}

enum CollectedEffectV0<T> {
    Completed(T),
    PreWriteInvalidated(OperationSessionV0),
}

impl BufferedCommandWatchesV0 {
    fn empty() -> Self {
        Self {
            root: None,
            dependency: None,
            automation: None,
            root_faulted: false,
            dependency_faulted: false,
            automation_faulted: false,
            fault: None,
            prewrite_invalidated: None,
        }
    }

    /// Detaches every root/old-D fact and fault that must be reduced before a
    /// renderer result may replace its speculative input or mint a fence.
    /// Automation delivery remains buffered for its existing post-ack path.
    fn take_root_dependency_pre_finalization(&mut self) -> Option<Self> {
        if self.root.is_none()
            && self.dependency.is_none()
            && !self.root_faulted
            && !self.dependency_faulted
            && self.prewrite_invalidated.is_none()
        {
            return None;
        }
        let root_faulted = std::mem::take(&mut self.root_faulted);
        let dependency_faulted = std::mem::take(&mut self.dependency_faulted);
        let fault = if root_faulted || dependency_faulted {
            self.fault.take()
        } else {
            None
        };
        Some(Self {
            root: self.root.take(),
            dependency: self.dependency.take(),
            automation: None,
            root_faulted,
            dependency_faulted,
            automation_faulted: false,
            fault,
            prewrite_invalidated: self.prewrite_invalidated.take(),
        })
    }
}

fn drain_completion_edge<R>(
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    root_name: &str,
    buffered: &mut BufferedCommandWatchesV0,
) {
    let cap = root_watch.drain_capacity();
    let mut drained = 0;
    while drained < cap
        && !buffered.root_faulted
        && !matches!(buffered.root, Some(BufferedRootWatchV0::RestartRequired))
    {
        match root_watch.try_next_event() {
            Ok(Some(event)) => {
                drained += 1;
                if watch_targets_root(&event, root_name) {
                    buffered.root = buffered_root_watch(event);
                }
            }
            Ok(None) => break,
            Err(error) if is_root_watch_terminal(&error) => {
                buffered.root = Some(BufferedRootWatchV0::RestartRequired);
            }
            Err(error) => {
                buffered.root_faulted = true;
                buffered
                    .fault
                    .get_or_insert_with(|| format!("root watch failed: {error}"));
            }
        }
    }
    if drained == cap && cap != 0 {
        match root_watch.try_next_event() {
            Ok(Some(_)) | Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
                buffered.root = Some(BufferedRootWatchV0::RestartRequired);
            }
            Ok(None) => {}
            Err(error) => {
                buffered.root_faulted = true;
                buffered
                    .fault
                    .get_or_insert_with(|| format!("root watch failed: {error}"));
            }
        }
    }
    if !buffered.dependency_faulted
        && buffered.dependency.is_none()
        && let Some(port) = fixed_point.as_mut()
    {
        match port.drain_ready_retained_watch() {
            Ok(Some(record)) => buffered.dependency = Some(record),
            Ok(None) => {}
            Err(error) => {
                buffered.dependency_faulted = true;
                buffered
                    .fault
                    .get_or_insert_with(|| format!("dependency watch failed: {error}"));
            }
        }
    }
}

/// A final-render declaration that has passed the renderer ABI plus the
/// driver's additional descriptor, source, and final-observation checks. It
/// is deliberately not durable: the root UID and observation barrier make it
/// invalid as soon as the rendered world is superseded.
struct QueuedAutomationV0 {
    root_uid: String,
    barrier: crate::BarrierV0,
    render_observation_token: String,
    trigger: ActionTriggerV0,
}

impl QueuedAutomationV0 {
    fn matches(&self, actor: &RootActorV0) -> bool {
        self.root_uid == actor.state().root().uid
            && self.barrier == actor.barrier()
            && !self.render_observation_token.is_empty()
            && actor.active_render_observation_token()
                == Some(self.render_observation_token.as_str())
            && !actor.state().root().deleting
    }
}

/// Moves exactly one still-pinned declaration into the actor. Callers must
/// establish `Ready` before using it; returning `true` is independent of the
/// number of candidates left, so a single declaration cannot strand a queued
/// reducer event behind the driver's watch `select!`.
fn enqueue_next_automation_candidate(
    actor: &mut RootActorV0,
    candidates: &mut VecDeque<QueuedAutomationV0>,
) -> bool {
    while let Some(candidate) = candidates.pop_front() {
        if candidate.matches(actor) {
            actor.enqueue(EventV0::TriggerAutomation {
                trigger: candidate.trigger,
            });
            return true;
        }
    }
    false
}

/// Converts only the stabilized, fence-complete render output into action
/// triggers.  The validation is intentionally repeated at this boundary even
/// though the renderer ABI parsed the document: a renderer declaration cannot
/// name an action that the verified installed descriptor did not authorize,
/// and it cannot use a dependency absent from the final snapshot.
#[allow(clippy::too_many_lines)]
fn automation_candidates_from_final_render(
    actor: &RootActorV0,
    input: &RenderInputV0,
    output: &oxideloop_renderer_abi::RenderOutputV0,
    final_dependencies: &[Dependency],
    descriptor: &BundleDescriptorV0,
) -> Result<VecDeque<QueuedAutomationV0>, RuntimeErrorV0> {
    let root = actor.state().root();
    if root.deleting
        || input.root.uid != root.uid
        || input.bundle_digest != root.bundle_digest
        || input.bundle_digest != root.spec.program.digest
        || output.abi_version != ABI_VERSION_V0
        || descriptor.renderer_abi_version != ABI_VERSION_V0
    {
        return Err(RuntimeErrorV0::Hold(
            "final render automation declarations do not match the active root/renderer pins"
                .into(),
        ));
    }

    let mut candidates = BTreeMap::new();
    for declaration in &output.automations {
        if declaration.bundle_digest != root.spec.program.digest
            || declaration.bundle_digest != root.bundle_digest
            || declaration.renderer_abi_version != ABI_VERSION_V0
            || !descriptor.action_modules.iter().any(|module| {
                module.identity == declaration.action_module_identity
                    && module.digest == declaration.action_module_digest
            })
        {
            return Err(RuntimeErrorV0::Hold(
                "final render automation declaration is not authorized by the verified bundle descriptor".into(),
            ));
        }
        validate_event_metadata(&declaration.event_metadata).map_err(|error| {
            RuntimeErrorV0::Hold(format!(
                "final render automation event metadata is not canonical/bounded: {error}"
            ))
        })?;

        let Dependency::Object {
            api_version,
            kind,
            namespace,
            name,
            uid: Some(source_uid),
            resource_version: Some(source_resource_version),
            ..
        } = &declaration.source_dependency
        else {
            return Err(RuntimeErrorV0::Hold(
                "final render automation source is not a concretely observed object".into(),
            ));
        };
        if source_uid.is_empty()
            || source_resource_version.is_empty()
            || !output
                .dependencies
                .iter()
                .any(|dependency| dependency == &declaration.source_dependency)
            || !final_dependencies
                .iter()
                .any(|dependency| dependency == &declaration.source_dependency)
        {
            return Err(RuntimeErrorV0::Hold(
                "final render automation source was not declared by the stabilized dependency set"
                    .into(),
            ));
        }
        let Some(observation) = input.observations.iter().find(|observation| {
            observation.api_version == *api_version
                && observation.kind == *kind
                && observation.namespace == *namespace
                && observation.name == *name
                && observation.uid == *source_uid
                && observation.resource_version == *source_resource_version
                && !observation.value.is_null()
        }) else {
            return Err(RuntimeErrorV0::Hold(
                "final render automation source has no exact PRESENT observation".into(),
            ));
        };
        // Bind the trigger to the exact observed payload too.  This is only a
        // presence assertion here; `event_metadata` is the canonical action
        // payload and the immutable AutomationRun spec captures its hash.
        if observation.value.is_null() {
            return Err(RuntimeErrorV0::Hold(
                "final render automation source observation is absent".into(),
            ));
        }
        let source = ObjectIdentity {
            api_version: api_version.clone(),
            kind: kind.clone(),
            namespace: namespace.clone(),
            name: name.clone(),
        };
        let mut trigger = ActionTriggerV0 {
            automation_identity: declaration.identity.clone(),
            automation_version: declaration.version.clone(),
            action_module_identity: declaration.action_module_identity.clone(),
            action_module_digest: declaration.action_module_digest.clone(),
            source,
            source_uid: source_uid.clone(),
            source_resource_version: source_resource_version.clone(),
            observation_token: String::new(),
            event_metadata: declaration.event_metadata.clone(),
        };
        // This token intentionally excludes the global render barrier; only
        // the automation declaration and its exact source transition may
        // establish a baseline or occurrence.
        trigger.observation_token = automation_source_transition_token(&trigger);
        let key = format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}",
            trigger.automation_identity,
            trigger.automation_version,
            trigger.source.api_version,
            trigger.source.kind,
            trigger.source.namespace,
            trigger.source.name,
            trigger.source_resource_version,
        );
        if candidates
            .insert(
                key,
                QueuedAutomationV0 {
                    root_uid: root.uid.clone(),
                    barrier: actor.barrier(),
                    render_observation_token: input.observation_token.clone(),
                    trigger,
                },
            )
            .is_some()
        {
            return Err(RuntimeErrorV0::Hold(
                "final render contains duplicate automation source declarations".into(),
            ));
        }
    }
    Ok(candidates.into_values().collect())
}

/// Exact root watch evidence retained while an owned command future runs.
/// The later reread remains authoritative, but this record preserves whether
/// the watch delivered a particular old UID/tombstone or invalidated entirely
/// instead of reducing an untyped boolean across the command boundary.
enum BufferedRootWatchV0 {
    Observed {
        uid: String,
        resource_version: String,
        deleted: bool,
    },
    RestartRequired,
}

/// An authenticated pending-run delivery remains opaque after the shared
/// list/watch/GET validator.  The driver only uses it to decide that the
/// retained barrier must be re-established from durable pending state after
/// the command acknowledgement; it never turns it into a reducer event.
enum BufferedAutomationWatchV0 {
    Changed(Box<oxideloop_kube_transport::ValidatedAutomationRunV0>),
    RestartRequired,
}

fn buffered_root_watch(event: WatchEventV0) -> Option<BufferedRootWatchV0> {
    match event {
        WatchEventV0::Added(object) | WatchEventV0::Modified(object) => {
            Some(BufferedRootWatchV0::Observed {
                uid: object.metadata.uid,
                resource_version: object.metadata.resource_version,
                deleted: false,
            })
        }
        WatchEventV0::Deleted(object) => Some(BufferedRootWatchV0::Observed {
            uid: object.metadata.uid,
            resource_version: object.metadata.resource_version,
            deleted: true,
        }),
        WatchEventV0::Bookmark { .. } => None,
    }
}

/// Drains the independent bootstrap fence through a fully-enqueued watermark.
/// The watermark is captured before the drain, not inferred from whichever
/// records happened to be available to the driver.  If the pump commits a
/// later record while this runs, the post-drain capture repeats under the same
/// immutable operation budget; no later root mutation may slip past the next
/// bootstrap write lease.
#[allow(clippy::too_many_lines)]
async fn drain_collected_bootstrap_fence_to_watermark(
    root_watch: &mut WatchHandleV0,
    operation: &OperationSessionV0,
    root_name: &str,
    buffered: &mut BufferedCommandWatchesV0,
) -> Result<(), RuntimeErrorV0> {
    loop {
        let watermark = match operation.capture_bootstrap_fence_watermark().await {
            Ok(watermark) => watermark,
            Err(_error) if operation.bootstrap_fence_terminal().is_some() => {
                // A terminal record may already have been consumed by this
                // very drain.  Preserve its normal prewrite/rebarrier path
                // instead of recasting it as an unrelated collector hold.
                buffered
                    .prewrite_invalidated
                    .get_or_insert_with(|| operation.clone());
                return Ok(());
            }
            Err(error) => {
                return Err(RuntimeErrorV0::Hold(format!(
                    "bootstrap fence could not capture an enqueued watermark: {error}"
                )));
            }
        };
        let required = match operation.bootstrap_fence_records_until(watermark) {
            Ok(required) => required,
            Err(_error) if operation.bootstrap_fence_terminal().is_some() => {
                buffered
                    .prewrite_invalidated
                    .get_or_insert_with(|| operation.clone());
                return Ok(());
            }
            Err(error) => {
                return Err(RuntimeErrorV0::Hold(format!(
                    "bootstrap fence watermark is not drainable: {error}"
                )));
            }
        };
        let cap = root_watch.drain_capacity();
        if required > u64::try_from(cap).expect("usize fits u64") {
            operation.bootstrap_fence_consumer_gap();
            buffered.root = Some(BufferedRootWatchV0::RestartRequired);
            return Ok(());
        }

        let mut reads = 0_usize;
        while !operation
            .bootstrap_fence_consumed_through(watermark)
            .map_err(|error| {
                RuntimeErrorV0::Hold(format!(
                    "bootstrap fence consumption could not be inspected: {error}"
                ))
            })?
        {
            if reads >= cap {
                operation.bootstrap_fence_consumer_gap();
                buffered.root = Some(BufferedRootWatchV0::RestartRequired);
                return Ok(());
            }
            match root_watch.try_next_event() {
                Ok(Some(event)) => {
                    reads += 1;
                    // The fresh bootstrap watch is an exact-name capability.
                    // Classify every non-bookmark record through the sealed
                    // transport fence before the driver's legacy name-only
                    // helper can decide whether to buffer it.  In
                    // particular, a scripted or API-leaked namespace/kind
                    // record must become a terminal fence fact, never an
                    // ignored record that lets the next subwrite proceed.
                    if !operation.observe_bootstrap_root_watch(&event)
                        && watch_targets_root(&event, root_name)
                    {
                        buffered.root = buffered_root_watch(event);
                    }
                    if operation.bootstrap_fence_terminal().is_some() {
                        buffered
                            .prewrite_invalidated
                            .get_or_insert_with(|| operation.clone());
                        return Ok(());
                    }
                }
                Ok(None) => {
                    // A captured record missing from the bounded output queue
                    // means publication and consumption have diverged.  Do
                    // not wait for a later record to make the gap disappear.
                    operation.bootstrap_fence_consumer_gap();
                    buffered.root = Some(BufferedRootWatchV0::RestartRequired);
                    return Ok(());
                }
                Err(error) if is_root_watch_terminal(&error) => {
                    operation.observe_bootstrap_root_terminal(error);
                    buffered.root = Some(BufferedRootWatchV0::RestartRequired);
                    return Ok(());
                }
                Err(error) => {
                    operation.observe_bootstrap_root_terminal(error.clone());
                    buffered.root_faulted = true;
                    buffered
                        .fault
                        .get_or_insert_with(|| format!("root watch failed: {error}"));
                    return Ok(());
                }
            }
        }

        // Re-enter the pre-command drain when a post-watermark record raced
        // the completion edge.  Yield cooperatively so a busy source cannot
        // starve the finite operation deadline check in capture.
        let after = match operation.capture_bootstrap_fence_watermark().await {
            Ok(after) => after,
            Err(_error) if operation.bootstrap_fence_terminal().is_some() => {
                buffered
                    .prewrite_invalidated
                    .get_or_insert_with(|| operation.clone());
                return Ok(());
            }
            Err(error) => {
                return Err(RuntimeErrorV0::Hold(format!(
                    "bootstrap fence became unavailable after drain: {error}"
                )));
            }
        };
        if after == watermark {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}

/// Pins a single Kubernetes effect future and keeps collecting watch facts
/// without borrowing the actor.  `select!` never drops `effect`: it is pinned
/// for the entire loop, so a root/dependency/run transition cannot cancel or
/// replay a write that is already owned by the actor.
#[allow(clippy::too_many_arguments)]
async fn execute_kubernetes_command_while_observing(
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
    command: &CommandV0,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, UnixRendererPortV0>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    root_name: &str,
    prewrite_root_witness: Option<PrewriteRootWitnessV0>,
) -> Result<(KubernetesEffectCompletionV0, BufferedCommandWatchesV0), RuntimeErrorV0> {
    // Bootstrap uses the same collector-bound session as every other
    // Kubernetes command. Seal its exact freshly minted session *before*
    // entering the collector select: a ready root Added may win before the
    // effect future makes even its initial poll.
    let operation = transport.begin_collected_operation_session();
    let prewrite_root_witness = if matches!(command, CommandV0::EnsureBootstrap { .. }) {
        let (fresh_watch, raw_root, raw_state) = match transport
            .open_collected_bootstrap_cut_in_operation(address, command, &operation)
            .await
        {
            Ok(cut) => cut,
            Err(error) if fresh_bootstrap_cut_was_superseded(&error) => {
                return Ok((
                    KubernetesEffectCompletionV0::FreshBootstrapCutInvalidated,
                    BufferedCommandWatchesV0::empty(),
                ));
            }
            Err(error) => {
                return Err(RuntimeErrorV0::Hold(format!(
                    "bootstrap collector could not acquire its fresh exact-name cut: {error}"
                )));
            }
        };
        // Retire the old retained epoch only after the new list/watch/reread
        // cut exists.  The returned watch becomes the ordinary retained watch
        // again after the operation-owned pump's close handshake.
        *root_watch = fresh_watch;
        operation
            .seal_collected_bootstrap_fence(address, command, &raw_root, raw_state.as_ref())
            .map_err(|_| {
                RuntimeErrorV0::Hold(
                    "bootstrap collector could not seal its exact raw pre-poll authority".into(),
                )
            })?;
        // Bootstrap duplicate authority is exclusively the sealed transport
        // fence (full Command + RootAddress + raw list/reread baseline).  Do
        // not retain an actor snapshot compatibility witness that could grant
        // a second pre-I/O duplicate path.
        None
    } else {
        prewrite_root_witness
    };
    collect_kubernetes_command_while_observing(
        transport,
        address,
        command,
        root_watch,
        fixed_point,
        automation_watch,
        root_name,
        operation,
        prewrite_root_witness,
    )
    .await
}

/// A fresh exact-name cut has not yet sealed an operation fence or polled its
/// effect. These outcomes mean the list/watch/reread view was superseded and
/// may safely return to the driver's ordinary rebarrier path. Everything
/// else (malformed input, authority rejection, deadline, or transport error)
/// remains a fail-closed hold.
fn fresh_bootstrap_cut_was_superseded(error: &TransportErrorV0) -> bool {
    matches!(error, TransportErrorV0::FreshBootstrapCutSuperseded)
}

/// The exact fresh bootstrap cut failed before an operation fence existed or
/// its command future could poll. Re-enter the driver's ordinary root
/// list/watch/reread barrier and release only that exact unpolled slot. A
/// concurrent disappearance is safe here because `EnsureBootstrap` cannot
/// have made a State or root mutation yet; the caller resumes the normal
/// wait-for-create loop rather than holding or inventing an acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FreshBootstrapCutRebarrierV0 {
    ActorRetired,
    Active,
}

#[allow(clippy::too_many_arguments)]
async fn rebarrier_after_fresh_bootstrap_cut<R>(
    active: &mut RootActorV0,
    command: &CommandV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    automation_candidates: &mut VecDeque<QueuedAutomationV0>,
) -> Result<FreshBootstrapCutRebarrierV0, RuntimeErrorV0> {
    if !matches!(command, CommandV0::EnsureBootstrap { .. }) {
        return Err(RuntimeErrorV0::Hold(
            "a non-bootstrap command cannot use the fresh-cut rebarrier path".into(),
        ));
    }
    active.release_unpolled_command(command)?;
    active.watch_invalidated();
    let (watch, fresh) = establish_root_barrier(transport, config).await?;
    *root_watch = watch;
    let Some(fresh) = fresh else {
        clear_root_only_recovery_ephemera(fixed_point, automation_watch, automation_candidates);
        return Ok(FreshBootstrapCutRebarrierV0::ActorRetired);
    };
    if fresh.root().uid != active.state().root().uid {
        active
            .hold("nominated root name was replaced by a different UID; refusing adoption".into());
        return Err(RuntimeErrorV0::Hold(
            "nominated root name was replaced by a different UID; refusing adoption".into(),
        ));
    }
    // No bootstrap subwrite was polled, so the old actor contains only an
    // unacknowledged bootstrap command. Replacing it from the fresh barrier
    // is safer than feeding that stale unlinked state through `RootRefreshed`:
    // the latter retains an empty State pin that a reducer quite rightly
    // refuses to persist. The replacement still rejects a same-name UID
    // change above and cannot adopt/link any untrusted State.
    *active = start_actor(fresh);
    clear_root_only_recovery_ephemera(fixed_point, automation_watch, automation_candidates);
    Ok(FreshBootstrapCutRebarrierV0::Active)
}

/// Keeps the exact actor-issued command owned across a resumable bootstrap
/// sequence.  The surrounding collector continues polling watches while this
/// single future retries; it never asks `next_command` for a reconstruction.
#[cfg(test)]
async fn dispatch_owned_command_until_event(
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
    command: &CommandV0,
) -> Result<Option<EventV0>, RuntimeErrorV0> {
    dispatch_owned_command_until_event_with_operation(
        transport,
        address,
        command,
        transport.begin_operation_session(),
    )
    .await
}

async fn dispatch_owned_command_until_event_with_operation(
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
    command: &CommandV0,
    operation: OperationSessionV0,
) -> Result<Option<EventV0>, RuntimeErrorV0> {
    for _ in 0..=operation.max_prewrite_retries() {
        if let Some(event) =
            dispatch_transport_command_in_operation(transport, address, command, &operation).await?
        {
            return Ok(Some(event));
        }
    }
    Err(RuntimeErrorV0::Hold(
        "transport sequencing retry bound exceeded".into(),
    ))
}

/// Collected Kubernetes commands have one extra authority fence beyond the
/// generic pinned collector.  A root watch received before the first write
/// poll suspends and drops the unpolled preflight, then authenticates the
/// record with the *same* immutable operation session.  No root record is
/// allowed to become a reducer event from this function.
type OwnedCommandFutureV0<'a> =
    Pin<Box<dyn Future<Output = Result<Option<EventV0>, RuntimeErrorV0>> + Send + 'a>>;
type RootAuthenticationFutureV0<'a> = Pin<
    Box<
        dyn Future<Output = Result<oxideloop_controller_core::RootSnapshotV0, RuntimeErrorV0>>
            + Send
            + 'a,
    >,
>;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn collect_kubernetes_command_while_observing<R>(
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
    command: &CommandV0,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    root_name: &str,
    operation: OperationSessionV0,
    root_witness: Option<PrewriteRootWitnessV0>,
) -> Result<(KubernetesEffectCompletionV0, BufferedCommandWatchesV0), RuntimeErrorV0> {
    let fenced = operation.has_collected_bootstrap_fence();
    if fenced {
        root_watch
            .start_collected_bootstrap_fence(&operation)
            .map_err(|error| {
                RuntimeErrorV0::Hold(format!(
                    "bootstrap collector could not install its operation fence pump: {error}"
                ))
            })?;
    }
    let result = collect_kubernetes_command_while_observing_inner(
        transport,
        address,
        command,
        root_watch,
        fixed_point,
        automation_watch,
        root_name,
        operation.clone(),
        root_witness,
    )
    .await;
    let close = if fenced {
        operation.close_collected_bootstrap_fence(root_watch).await
    } else {
        Ok(())
    };
    match (result, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(RuntimeErrorV0::Hold(format!(
            "bootstrap collector fence close failed: {error}"
        ))),
        (Err(error), _) => Err(error),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn collect_kubernetes_command_while_observing_inner<R>(
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
    command: &CommandV0,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    root_name: &str,
    operation: OperationSessionV0,
    root_witness: Option<PrewriteRootWitnessV0>,
) -> Result<(KubernetesEffectCompletionV0, BufferedCommandWatchesV0), RuntimeErrorV0> {
    let mut effect: OwnedCommandFutureV0<'_> =
        Box::pin(dispatch_owned_command_until_event_with_operation(
            transport,
            address,
            command,
            operation.clone(),
        ));
    let mut root_authentication: Option<RootAuthenticationFutureV0<'_>> = None;
    let mut root_auth_epoch = None;
    let mut buffered = BufferedCommandWatchesV0 {
        root: None,
        dependency: None,
        automation: None,
        root_faulted: false,
        dependency_faulted: false,
        automation_faulted: false,
        fault: None,
        prewrite_invalidated: None,
    };

    // Finite first-poll handshake.  The exact named watch was registered from
    // the list RV before this collector was entered.  Capture and drain a
    // fully-enqueued fence watermark before the effect can make its first
    // read/write poll.
    if operation.has_collected_bootstrap_fence() {
        drain_collected_bootstrap_fence_to_watermark(
            root_watch,
            &operation,
            root_name,
            &mut buffered,
        )
        .await?;
    }

    loop {
        tokio::select! {
            // A root record that arrived while a previous bootstrap subwrite
            // was already polled must not cancel that write. Once the permit
            // proves and opens the next phase, however, the retained record
            // becomes authority for that *next* subwrite and must receive the
            // same fresh authentication before the effect can continue.
            () = operation.prewrite_gate().wait_for_bootstrap_phase_pending(), if root_authentication.is_none() => {
                if operation.has_collected_bootstrap_fence() {
                    // Every completed collected subwrite re-enters the same
                    // watermark drain.  Records published after the prior
                    // boundary therefore cannot bypass the next lease.
                    drain_collected_bootstrap_fence_to_watermark(
                        root_watch,
                        &operation,
                        root_name,
                        &mut buffered,
                    )
                    .await?;
                    if operation.prewrite_gate().state() == PrewriteGateStateV0::BootstrapPhasePending {
                        let _ = operation.prewrite_gate().seal_bootstrap_phase();
                    }
                } else if !operation.bootstrap_phase_is_complete()
                    && matches!(buffered.root, Some(BufferedRootWatchV0::Observed { .. }))
                    && root_witness.is_some()
                    && let Some(epoch) = operation.prewrite_gate().begin_root_authentication()
                {
                    effect = Box::pin(std::future::pending());
                    root_auth_epoch = Some(epoch);
                    root_authentication = Some(Box::pin(
                        fresh_root_snapshot_from_transport_prewrite_in_operation(
                            transport,
                            &address.namespace,
                            root_name,
                            &operation,
                        ),
                    ));
                } else if buffered.root_faulted
                    || matches!(buffered.root, Some(BufferedRootWatchV0::RestartRequired))
                {
                    let _ = operation.prewrite_gate().invalidate_bootstrap_phase();
                } else {
                    let _ = operation.prewrite_gate().seal_bootstrap_phase();
                }
            }
            result = &mut effect, if root_authentication.is_none() => {
                drain_completion_edge(root_watch, fixed_point, root_name, &mut buffered);
                record_prewrite_invalidation(Some(&operation), &mut buffered);
                match result {
                    Ok(event) => return Ok((KubernetesEffectCompletionV0::Acknowledged(Box::new(event)), buffered)),
                    Err(RuntimeErrorV0::Discovery(TransportErrorV0::PreWriteInvalidated)) => {
                        let Some(capability) = buffered.prewrite_invalidated.as_ref() else {
                            return Err(RuntimeErrorV0::Hold(
                                "transport prewrite invalidation lacks a collector capability".into(),
                            ));
                        };
                        if !capability.matches_exact_session(&operation) {
                            return Err(RuntimeErrorV0::Hold(
                                "transport prewrite invalidation belongs to another operation".into(),
                            ));
                        }
                        return Ok((KubernetesEffectCompletionV0::PreWriteInvalidated {
                            capability: capability.clone(),
                            root_authentication: None,
                        }, buffered));
                    }
                    Err(error) => return Err(error),
                }
            }
            root = async {
                if buffered.root_faulted
                    || matches!(buffered.root, Some(BufferedRootWatchV0::RestartRequired))
                {
                    std::future::pending::<Result<WatchEventV0, TransportErrorV0>>().await
                } else {
                    root_watch.next_event().await
                }
            } => {
                match root {
                    Ok(event) => {
                        let consumed = if operation.has_collected_bootstrap_fence() {
                            // Do not pre-filter this event by the driver's
                            // ordinary name comparison. The sealed exact-name
                            // fence owns leaked-record validation and latches
                            // a foreign kind/namespace/name before it can be
                            // silently discarded here.
                            operation.observe_bootstrap_root_watch(&event)
                        } else {
                            false
                        };
                        // Only the record that was just received may start a
                        // root-authentication epoch. In particular, a
                        // defensively ignored bookmark (or another
                        // non-target record) must not authenticate an older
                        // buffered root observation after an exact self echo
                        // has restarted prewrite.
                        let target_root_record = !consumed && watch_targets_root(&event, root_name);
                        if target_root_record {
                            buffered.root = buffered_root_watch(event);
                        }
                        if target_root_record
                            && !operation.has_collected_bootstrap_fence()
                            && root_authentication.is_none()
                            && let Some(witness) = root_witness.as_ref()
                            && matches!(
                                operation.prewrite_gate().state(),
                                PrewriteGateStateV0::PreWrite
                                    | PrewriteGateStateV0::BootstrapPhasePending
                            )
                            && (operation.prewrite_gate().state()
                                != PrewriteGateStateV0::BootstrapPhasePending
                                || !operation.bootstrap_phase_is_complete())
                            && let Some(epoch) = operation.prewrite_gate().begin_root_authentication()
                        {
                            // The preflight had no write permit. Dropping it is
                            // safe and prevents partial read authority from
                            // crossing the root-authentication fence.
                            effect = Box::pin(std::future::pending());
                            root_auth_epoch = Some(epoch);
                            root_authentication = Some(Box::pin(
                                fresh_root_snapshot_from_transport_prewrite_in_operation(
                                    transport,
                                    &address.namespace,
                                    root_name,
                                    &operation,
                                ),
                            ));
                            let _ = witness;
                        }
                    }
                    Err(error) if is_root_watch_terminal(&error) => {
                        buffered.root = Some(BufferedRootWatchV0::RestartRequired);
                        if operation.has_collected_bootstrap_fence() {
                            operation.observe_bootstrap_root_terminal(error.clone());
                        }
                        if let Some(epoch) = root_auth_epoch {
                            let _ = operation.prewrite_gate().invalidate_root_authentication(epoch);
                            buffered.prewrite_invalidated = Some(operation.clone());
                            return Ok((KubernetesEffectCompletionV0::PreWriteInvalidated {
                                capability: operation.clone(),
                                root_authentication: Some(RootAuthenticationOutcomeV0::Ambiguous),
                            }, buffered));
                        }
                        invalidate_prewrite_gate(Some(&operation));
                    }
                    Err(error) => {
                        buffered.root_faulted = true;
                        buffered.fault.get_or_insert_with(|| format!("root watch failed: {error}"));
                        if operation.has_collected_bootstrap_fence() {
                            operation.observe_bootstrap_root_terminal(error.clone());
                        }
                        if let Some(epoch) = root_auth_epoch {
                            let _ = operation.prewrite_gate().invalidate_root_authentication(epoch);
                            buffered.prewrite_invalidated = Some(operation.clone());
                            return Ok((KubernetesEffectCompletionV0::PreWriteInvalidated {
                                capability: operation.clone(),
                                root_authentication: Some(RootAuthenticationOutcomeV0::Ambiguous),
                            }, buffered));
                        }
                        invalidate_prewrite_gate(Some(&operation));
                    }
                }
            }
            authentication = async {
                if let Some(authentication) = root_authentication.as_mut() {
                    Some(authentication.await)
                } else {
                    std::future::pending::<Option<Result<oxideloop_controller_core::RootSnapshotV0, RuntimeErrorV0>>>().await
                }
            }, if root_authentication.is_some() => {
                let epoch = root_auth_epoch.expect("root authentication owns an epoch");
                root_authentication = None;
                root_auth_epoch = None;
                let result = authentication.expect("enabled root authentication resolves");
                let latest_matches = matches!(
                    (&buffered.root, &result),
                    (Some(BufferedRootWatchV0::Observed { uid, resource_version, deleted: false }), Ok(fresh))
                        if *uid == fresh.uid && *resource_version == fresh.root_resource_version
                );
                let outcome = match result {
                    Ok(fresh) if latest_matches => root_witness
                        .as_ref()
                        .map(|witness| witness.authenticate(&fresh, epoch)),
                    Ok(_) | Err(_) => None,
                };
                if outcome == Some(PrewriteRootAuthenticationV0::ExactOwnedStatusEcho)
                    && operation.prewrite_gate().restart_after_exact_self_echo(
                        epoch,
                        u32::from(operation.max_prewrite_retries()),
                    )
                {
                    // The fresh snapshot proves that this raw record is only
                    // our owned status write. It is consumed here, not later
                    // replayed through root_observed, and the exact frozen
                    // command gets a wholly fresh preflight under epoch + 1.
                    buffered.root = None;
                    effect = Box::pin(dispatch_owned_command_until_event_with_operation(
                        transport,
                        address,
                        command,
                        operation.clone(),
                    ));
                    continue;
                }
                let _ = operation.prewrite_gate().invalidate_root_authentication(epoch);
                buffered.prewrite_invalidated = Some(operation.clone());
                return Ok((KubernetesEffectCompletionV0::PreWriteInvalidated {
                    capability: operation,
                    root_authentication: Some(match outcome {
                        Some(PrewriteRootAuthenticationV0::GenuineOrAmbiguous) => RootAuthenticationOutcomeV0::Genuine,
                        Some(PrewriteRootAuthenticationV0::ExactOwnedStatusEcho) | None => RootAuthenticationOutcomeV0::Ambiguous,
                    }),
                }, buffered));
            }
            dependency = async {
                if buffered.dependency_faulted || buffered.dependency.is_some() {
                    std::future::pending::<Result<BufferedRetainedWatchV0, RuntimeErrorV0>>().await
                } else if let Some(port) = fixed_point.as_mut() {
                    port.await_buffered_retained_watch().await
                } else {
                    std::future::pending::<Result<BufferedRetainedWatchV0, RuntimeErrorV0>>().await
                }
            } => {
                match dependency {
                    Ok(record) => {
                        buffered.dependency = Some(record);
                        invalidate_prewrite_gate(Some(&operation));
                    }
                    Err(error) => {
                        buffered.dependency_faulted = true;
                        buffered.fault.get_or_insert_with(|| format!("dependency watch failed: {error}"));
                        invalidate_prewrite_gate(Some(&operation));
                    }
                }
            }
            run = async {
                if buffered.automation_faulted || buffered.automation.is_some() {
                    std::future::pending::<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>().await
                } else if let Some(watch) = automation_watch.as_mut() {
                    watch.next_change().await
                } else {
                    std::future::pending::<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>().await
                }
            } => {
                match run {
                    Ok(run) => {
                        buffered.automation = Some(BufferedAutomationWatchV0::Changed(Box::new(run)));
                        invalidate_prewrite_gate(Some(&operation));
                    }
                    Err(error) if is_automation_watch_terminal(&error) => {
                        buffered.automation = Some(BufferedAutomationWatchV0::RestartRequired);
                        invalidate_prewrite_gate(Some(&operation));
                    }
                    Err(error) => {
                        buffered.automation_faulted = true;
                        buffered.fault.get_or_insert_with(|| format!("AutomationRun watch failed: {error}"));
                        invalidate_prewrite_gate(Some(&operation));
                    }
                }
            }
        }
    }
}

/// Generic pinned-future collector shared by Kubernetes, descriptor, deletion
/// and renderer command routes.  The effect is boxed and polled to completion
/// exactly once while no branch is allowed to borrow the actor.
#[allow(clippy::too_many_arguments)]
async fn collect_while_observing<T, F, R>(
    effect: F,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    root_name: &str,
) -> Result<(T, BufferedCommandWatchesV0), RuntimeErrorV0>
where
    F: Future<Output = Result<T, RuntimeErrorV0>>,
{
    let (completion, buffered) = collect_while_observing_with_operation(
        effect,
        root_watch,
        fixed_point,
        automation_watch,
        root_name,
        None,
    )
    .await?;
    match completion {
        CollectedEffectV0::Completed(value) => Ok((value, buffered)),
        CollectedEffectV0::PreWriteInvalidated(capability) => {
            let _ = capability.prewrite_gate().state();
            Err(RuntimeErrorV0::Hold(
                "uncollected command received an invalidated prewrite capability".into(),
            ))
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn collect_while_observing_with_operation<T, F, R>(
    effect: F,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    root_name: &str,
    operation: Option<OperationSessionV0>,
) -> Result<(CollectedEffectV0<T>, BufferedCommandWatchesV0), RuntimeErrorV0>
where
    F: Future<Output = Result<T, RuntimeErrorV0>>,
{
    let mut effect = Box::pin(effect);
    let mut buffered = BufferedCommandWatchesV0 {
        root: None,
        dependency: None,
        automation: None,
        root_faulted: false,
        dependency_faulted: false,
        automation_faulted: false,
        fault: None,
        prewrite_invalidated: None,
    };
    loop {
        tokio::select! {
            result = &mut effect => {
                drain_completion_edge(root_watch, fixed_point, root_name, &mut buffered);
                record_prewrite_invalidation(operation.as_ref(), &mut buffered);
                match result {
                    Ok(value) => return Ok((CollectedEffectV0::Completed(value), buffered)),
                    Err(RuntimeErrorV0::Discovery(TransportErrorV0::PreWriteInvalidated)) => {
                        let Some(operation) = operation.as_ref() else {
                            return Err(RuntimeErrorV0::Hold(
                                "transport returned prewrite invalidation outside a collected operation".into(),
                            ));
                        };
                        let Some(capability) = buffered.prewrite_invalidated.as_ref() else {
                            return Err(RuntimeErrorV0::Hold(
                                "transport prewrite invalidation lacks a collector capability".into(),
                            ));
                        };
                        if !capability.matches_exact_session(operation) {
                            return Err(RuntimeErrorV0::Hold(
                                "transport prewrite invalidation belongs to another operation".into(),
                            ));
                        }
                        return Ok((
                            CollectedEffectV0::PreWriteInvalidated(capability.clone()),
                            buffered,
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
            root = async {
                if buffered.root_faulted
                    || matches!(buffered.root, Some(BufferedRootWatchV0::RestartRequired))
                {
                    std::future::pending::<Result<WatchEventV0, TransportErrorV0>>().await
                } else {
                    root_watch.next_event().await
                }
            } => {
                match root {
                    Ok(event) if watch_targets_root(&event, root_name) => {
                        // Raw root events are not authenticated change
                        // authority here: an owned root-status self-echo is
                        // indistinguishable until T2's exact root refresh.
                        buffered.root = buffered_root_watch(event);
                    }
                    Ok(_) => {}
                    Err(error) if is_root_watch_terminal(&error) => {
                        buffered.root = Some(BufferedRootWatchV0::RestartRequired);
                        invalidate_prewrite_gate(operation.as_ref());
                    }
                    Err(error) => {
                        buffered.root_faulted = true;
                        invalidate_prewrite_gate(operation.as_ref());
                        buffered.fault.get_or_insert_with(|| format!("root watch failed: {error}"));
                    }
                }
            }
            dependency = async {
                if buffered.dependency_faulted || buffered.dependency.is_some() {
                    std::future::pending::<Result<BufferedRetainedWatchV0, RuntimeErrorV0>>().await
                } else if let Some(port) = fixed_point.as_mut() {
                    port.await_buffered_retained_watch().await
                } else {
                    std::future::pending::<Result<BufferedRetainedWatchV0, RuntimeErrorV0>>().await
                }
            } => {
                match dependency {
                    Ok(record) => {
                        buffered.dependency = Some(record);
                        invalidate_prewrite_gate(operation.as_ref());
                    }
                    Err(error) => {
                        buffered.dependency_faulted = true;
                        invalidate_prewrite_gate(operation.as_ref());
                        buffered.fault.get_or_insert_with(|| format!("dependency watch failed: {error}"));
                    }
                }
            }
            run = async {
                if buffered.automation_faulted || buffered.automation.is_some() {
                    std::future::pending::<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>().await
                } else if let Some(watch) = automation_watch.as_mut() {
                    watch.next_change().await
                } else {
                    std::future::pending::<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>().await
                }
            } => {
                match run {
                    Ok(run) => {
                        buffered.automation = Some(BufferedAutomationWatchV0::Changed(Box::new(run)));
                        invalidate_prewrite_gate(operation.as_ref());
                    }
                    Err(error) if is_automation_watch_terminal(&error) => {
                        // Its durable expectation is re-established after the
                        // command acknowledgement; no raw run crosses this
                        // boundary.
                        buffered.automation = Some(BufferedAutomationWatchV0::RestartRequired);
                        invalidate_prewrite_gate(operation.as_ref());
                    }
                    Err(error) => {
                        buffered.automation_faulted = true;
                        invalidate_prewrite_gate(operation.as_ref());
                        buffered.fault.get_or_insert_with(|| format!("AutomationRun watch failed: {error}"));
                    }
                }
            }
        }
    }
}

fn invalidate_prewrite_gate(operation: Option<&OperationSessionV0>) {
    if let Some(operation) = operation {
        let _ = operation.prewrite_gate().invalidate_authenticated_change();
    }
}

fn record_prewrite_invalidation(
    operation: Option<&OperationSessionV0>,
    buffered: &mut BufferedCommandWatchesV0,
) {
    if buffered.prewrite_invalidated.is_none()
        && let Some(operation) = operation
        && operation.prewrite_gate().state() == PrewriteGateStateV0::Invalidated
    {
        buffered.prewrite_invalidated = Some(operation.clone());
    }
}

/// Render-specific pinned collector.  Unlike the generic command collector,
/// this owns the retained watches from the port *before* the render began and
/// can therefore poll them concurrently with a separate actor-free execution
/// half.  The two halves are reunited only after the effect resolves.
#[allow(clippy::too_many_arguments)]
async fn collect_render_while_observing<T, F>(
    effect: F,
    root_watch: &mut WatchHandleV0,
    retained: &mut RetainedWatchCollectorV0,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    root_name: &str,
) -> (Result<T, RuntimeErrorV0>, BufferedCommandWatchesV0)
where
    F: Future<Output = Result<T, RuntimeErrorV0>>,
{
    let mut effect = Box::pin(effect);
    let mut buffered = BufferedCommandWatchesV0 {
        root: None,
        dependency: None,
        automation: None,
        root_faulted: false,
        dependency_faulted: false,
        automation_faulted: false,
        fault: None,
        prewrite_invalidated: None,
    };
    loop {
        tokio::select! {
            result = &mut effect => {
                drain_render_completion_edge(root_watch, retained, root_name, &mut buffered);
                return (result, buffered);
            },
            root = async {
                if buffered.root_faulted
                    || matches!(buffered.root, Some(BufferedRootWatchV0::RestartRequired))
                {
                    std::future::pending::<Result<WatchEventV0, TransportErrorV0>>().await
                } else {
                    root_watch.next_event().await
                }
            } => match root {
                Ok(event) if watch_targets_root(&event, root_name) => {
                    buffered.root = buffered_root_watch(event);
                }
                Ok(_) => {}
                Err(error) if is_root_watch_terminal(&error) => {
                    buffered.root = Some(BufferedRootWatchV0::RestartRequired);
                }
                Err(error) => {
                    buffered.root_faulted = true;
                    buffered.fault.get_or_insert_with(|| format!("root watch failed: {error}"));
                }
            },
            dependency = async {
                if buffered.dependency_faulted || buffered.dependency.is_some() {
                    std::future::pending::<Result<BufferedRetainedWatchV0, RuntimeErrorV0>>().await
                } else {
                    retained.await_buffered_retained_watch().await
                }
            } => match dependency {
                Ok(record) => buffered.dependency = Some(record),
                Err(error) => {
                    buffered.dependency_faulted = true;
                    buffered.fault.get_or_insert_with(|| format!("dependency watch failed: {error}"));
                }
            },
            run = async {
                if buffered.automation_faulted || buffered.automation.is_some() {
                    std::future::pending::<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>().await
                } else if let Some(watch) = automation_watch.as_mut() {
                    watch.next_change().await
                } else {
                    std::future::pending::<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>().await
                }
            } => match run {
                Ok(run) => {
                    buffered.automation = Some(BufferedAutomationWatchV0::Changed(Box::new(run)));
                }
                Err(error) if is_automation_watch_terminal(&error) => {
                    buffered.automation = Some(BufferedAutomationWatchV0::RestartRequired);
                }
                Err(error) => {
                    buffered.automation_faulted = true;
                    buffered.fault.get_or_insert_with(|| format!("AutomationRun watch failed: {error}"));
                }
            }
        }
    }
}

fn drain_render_completion_edge(
    root_watch: &mut WatchHandleV0,
    retained: &mut RetainedWatchCollectorV0,
    root_name: &str,
    buffered: &mut BufferedCommandWatchesV0,
) {
    let cap = root_watch.drain_capacity();
    let mut drained = 0;
    while drained < cap
        && !buffered.root_faulted
        && !matches!(buffered.root, Some(BufferedRootWatchV0::RestartRequired))
    {
        match root_watch.try_next_event() {
            Ok(Some(event)) => {
                drained += 1;
                if watch_targets_root(&event, root_name) {
                    buffered.root = buffered_root_watch(event);
                }
            }
            Ok(None) => break,
            Err(error) if is_root_watch_terminal(&error) => {
                buffered.root = Some(BufferedRootWatchV0::RestartRequired);
            }
            Err(error) => {
                buffered.root_faulted = true;
                buffered
                    .fault
                    .get_or_insert_with(|| format!("root watch failed: {error}"));
            }
        }
    }
    if drained == cap && cap != 0 {
        match root_watch.try_next_event() {
            Ok(Some(_)) | Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
                buffered.root = Some(BufferedRootWatchV0::RestartRequired);
            }
            Ok(None) => {}
            Err(error) => {
                buffered.root_faulted = true;
                buffered
                    .fault
                    .get_or_insert_with(|| format!("root watch failed: {error}"));
            }
        }
    }
    if !buffered.dependency_faulted && buffered.dependency.is_none() {
        match retained.drain_ready_retained_watch() {
            Ok(Some(record)) => buffered.dependency = Some(record),
            Ok(None) => {}
            Err(error) => {
                buffered.dependency_faulted = true;
                buffered
                    .fault
                    .get_or_insert_with(|| format!("dependency watch failed: {error}"));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_buffered_watches<R>(
    actor: &mut RootActorV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
    root_watch: &mut WatchHandleV0,
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    automation_candidates: &mut VecDeque<QueuedAutomationV0>,
    buffered: BufferedCommandWatchesV0,
) -> Result<(), RuntimeErrorV0> {
    // The effect acknowledgement has already reduced when this helper is
    // called. A fatal collector error is then terminal for the current actor
    // epoch: do not apply a later buffered root/dependency/run record after
    // the fault, because that record may have crossed the same broken watch
    // boundary. A process restart re-establishes every list/watch/GET fence.
    if let Some(fault) = buffered.fault {
        actor.hold(fault.clone());
        return Err(RuntimeErrorV0::Hold(fault));
    }
    let root_barrier_before = actor.barrier();
    let root_only_recovery = match buffered.root {
        Some(BufferedRootWatchV0::RestartRequired) => {
            actor.watch_invalidated();
            let (watch, fresh) = establish_root_barrier(transport, config).await?;
            *root_watch = watch;
            // LIST→WATCH→GET closes the restart gap before any root authority
            // is reduced. `refresh` rejects a same-name replacement UID.
            refresh_actor_from_machine(actor, fresh)?
        }
        Some(BufferedRootWatchV0::Observed {
            uid,
            resource_version,
            deleted,
        }) => {
            // The post-command reread is authoritative. Retaining the exact
            // old watch identity here prevents an untyped wakeup from being
            // mistaken for an arbitrary root refresh in logs/debug paths.
            if uid.is_empty() || resource_version.is_empty() {
                return Err(RuntimeErrorV0::Hold(
                    "root watch record lacks an exact UID/resourceVersion".into(),
                ));
            }
            refresh_after_buffered_root_watch(actor, transport, config, &uid, deleted).await?
        }
        None => false,
    };
    if root_only_recovery {
        clear_root_only_recovery_ephemera(fixed_point, automation_watch, automation_candidates);
        return Ok(());
    }
    let root_rebarriered = actor.barrier() != root_barrier_before;
    if root_rebarriered && let Some(port) = fixed_point.as_mut() {
        // The fresh root reread may itself advance the actor barrier.  Align
        // only after that reread so InformersPending never inherits U handles.
        port.invalidate_to_barrier(actor.barrier());
    }
    if !root_rebarriered
        && let Some(record) = buffered.dependency
        && let Some(port) = fixed_point.as_mut()
    {
        let _ = port.apply_buffered_retained_watch(actor, record)?;
    }
    match buffered.automation {
        Some(BufferedAutomationWatchV0::Changed(delivery)) => {
            let current = automation_watch.as_ref().ok_or_else(|| {
                RuntimeErrorV0::Hold("pending AutomationRun watch disappeared".into())
            })?;
            if delivery.reference() != current.current().reference()
                || delivery.uid() != current.current().uid()
                || delivery.resource_version() != current.current().resource_version()
                || delivery.pending_resource_version()
                    != current.current().pending_resource_version()
            {
                return Err(RuntimeErrorV0::Hold(
                    "buffered AutomationRun delivery no longer matches its retained barrier".into(),
                ));
            }
            // Re-establishing from the durable pending record is deliberately
            // conservative and closes both 410 and delivery races.
            *automation_watch = None;
        }
        Some(BufferedAutomationWatchV0::RestartRequired) => {
            *automation_watch = None;
        }
        None => {}
    }
    Ok(())
}

/// Rechecks a typed root watch record after an owned command has acknowledged.
/// A tombstone for the active UID cannot be reinterpreted as a still-present
/// root; a same-name replacement remains a no-adopt Hold through the ordinary
/// refresh path.
async fn refresh_after_buffered_root_watch(
    actor: &mut RootActorV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
    watched_uid: &str,
    deleted: bool,
) -> Result<bool, RuntimeErrorV0> {
    if deleted && watched_uid == actor.state().root().uid {
        return match load_machine_from_transport(transport, &config.namespace, &config.root_name)
            .await?
        {
            Some(fresh) if fresh.root().uid == watched_uid => Err(RuntimeErrorV0::Hold(
                "deleted root UID remained present after its exact reread".into(),
            )),
            Some(_) => {
                actor.hold(
                    "nominated root name was replaced by a different UID; refusing adoption".into(),
                );
                Err(RuntimeErrorV0::Hold(
                    "nominated root name was replaced by a different UID; refusing adoption".into(),
                ))
            }
            None if actor.state().root().deleting => Ok(false),
            None => Err(RuntimeErrorV0::Hold(
                "nominated root disappeared without its finalizer protocol".into(),
            )),
        };
    }
    refresh_actor_from_transport(actor, transport, config).await
}

fn start_actor(machine: MachineStateV0) -> RootActorV0 {
    let mut actor = RootActorV0::new(machine);
    actor.start();
    actor
}

/// Returns the operator-facing mode-transition record for either restricted
/// deletion authority. The caller emits this after every whole-actor install,
/// including the retired-State to root-only cut within one process.
fn deletion_recovery_observability(
    config: &DriverConfigV0,
    state: &MachineStateV0,
) -> Option<String> {
    let mode = if state.is_root_only_deletion_recovery() {
        "receipt-bounded root-only deletion recovery"
    } else if state.is_retired_state_recovery() {
        "exact retired-State deletion recovery"
    } else {
        return None;
    };
    Some(format!(
        "oxideloop: entering {mode} for {}/{} uid={}",
        config.namespace,
        config.root_name,
        state.root().uid
    ))
}

/// Establishes the root list/watch/exact-GET barrier. Every start and watch
/// restart uses this one helper so a root that appeared or changed between
/// list and watch is still observed without requiring another event.
async fn establish_root_barrier(
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
) -> Result<(WatchHandleV0, Option<MachineStateV0>), RuntimeErrorV0> {
    let mut restarts = 0;
    loop {
        // A root fence is a single finite authority acquisition.  Its list,
        // watch-open, and root/State proof reads must share one immutable
        // budget; a terminal watch restart begins a new acquisition.
        let operation = transport.begin_operation_session();
        let list = transport
            .list_named_in_operation(
                ResourceKindV0::WebApplication,
                &config.namespace,
                &config.root_name,
                &operation,
            )
            .await?;
        let watch = match transport
            .watch_named_from_in_operation(
                ResourceKindV0::WebApplication,
                &config.namespace,
                &config.root_name,
                &list.resource_version,
                &operation,
            )
            .await
        {
            Ok(watch) => watch,
            Err(error) if is_root_watch_terminal(&error) => {
                if restarts == MAX_BARRIER_WATCH_RESTARTS {
                    return Err(error.into());
                }
                restarts += 1;
                wait_before_barrier_restart(restarts).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let fresh = load_machine_from_transport_in_operation(
            transport,
            &config.namespace,
            &config.root_name,
            &operation,
        )
        .await?;
        return Ok((watch, fresh));
    }
}

async fn drain_root_watch(
    watch: &mut WatchHandleV0,
    actor: &mut RootActorV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
) -> Result<RootWatchDrainV0, RuntimeErrorV0> {
    let cap = watch.drain_capacity();
    let mut drained = 0;
    while drained < cap {
        match watch.try_next_event() {
            Ok(Some(event)) => {
                drained += 1;
                if watch_targets_root(&event, &config.root_name)
                    && refresh_actor_from_transport(actor, transport, config).await?
                {
                    return Ok(RootWatchDrainV0::DeletionRecovery);
                }
            }
            Ok(None) => return Ok(RootWatchDrainV0::Current),
            Err(error) if is_root_watch_terminal(&error) => {
                actor.watch_invalidated();
                return Ok(RootWatchDrainV0::Invalidated);
            }
            Err(error) => return Err(error.into()),
        }
    }
    if cap != 0 {
        match watch.try_next_event() {
            Ok(Some(_)) | Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
                actor.watch_invalidated();
                return Ok(RootWatchDrainV0::Invalidated);
            }
            Ok(None) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(RootWatchDrainV0::Current)
}

/// The exact root stream may either leave the current actor authoritative,
/// require a new list/watch/GET epoch, or cross a restricted deletion
/// authority boundary that requires whole-actor replacement.
enum RootWatchDrainV0 {
    Current,
    Invalidated,
    DeletionRecovery,
}

async fn refresh_actor_from_transport(
    actor: &mut RootActorV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
) -> Result<bool, RuntimeErrorV0> {
    let fresh =
        load_machine_from_transport(transport, &config.namespace, &config.root_name).await?;
    refresh_actor_from_machine(actor, fresh)
}

fn refresh_actor_from_machine(
    actor: &mut RootActorV0,
    fresh: Option<MachineStateV0>,
) -> Result<bool, RuntimeErrorV0> {
    match fresh {
        Some(fresh) if fresh.root().uid == actor.state().root().uid => {
            if replace_actor_with_deletion_recovery(actor, &fresh) {
                return Ok(true);
            }
            actor.root_observed(fresh.root().clone());
            Ok(false)
        }
        Some(_) => {
            actor.hold(
                "nominated root name was replaced by a different UID; refusing adoption".into(),
            );
            Err(RuntimeErrorV0::Hold(
                "nominated root name was replaced by a different UID; refusing adoption".into(),
            ))
        }
        None if actor.state().root().deleting => Ok(false),
        None => {
            actor.hold("nominated root disappeared without its finalizer protocol".into());
            Err(RuntimeErrorV0::Hold(
                "nominated root disappeared without its finalizer protocol".into(),
            ))
        }
    }
}

/// Installs only a same-UID receipt-bounded recovery. The fresh machine was
/// built through the loader's exact root/state rereads; it therefore replaces
/// normal actor state instead of attempting to rebase stale state-backed
/// commands across a deleted journal object.
fn replace_actor_with_deletion_recovery(actor: &mut RootActorV0, fresh: &MachineStateV0) -> bool {
    let replace_retired = fresh.is_retired_state_recovery();
    let replace_root_only =
        fresh.is_root_only_deletion_recovery() && !actor.state().is_root_only_deletion_recovery();
    if fresh.root().uid == actor.state().root().uid
        && fresh.root().deleting
        && (replace_retired || replace_root_only)
    {
        *actor = start_actor(fresh.clone());
        true
    } else {
        false
    }
}

/// A stale preflight cannot rebase an owned state-backed command over an
/// absent state object. Receipt-bounded recovery starts a new restricted actor
/// instead; all other same-UID stale roots retain the ordinary rebase path.
fn recover_stale_command_actor(actor: &mut RootActorV0, fresh: &MachineStateV0) -> bool {
    if replace_actor_with_deletion_recovery(actor, fresh) {
        true
    } else {
        actor.recover_stale_preflight(fresh.root().clone());
        false
    }
}

/// Receipt-bound recovery invalidates all normal reconciliation authority.
/// The replacement actor can issue only root-only deletion commands, so no
/// retained dependency, pending `AutomationRun`, descriptor-derived port, or
/// ephemeral automation candidate may survive the state disappearance.
fn clear_root_only_recovery_ephemera<R>(
    fixed_point: &mut Option<KubeFixedPointPortV0<'_, R>>,
    automation_watch: &mut Option<AutomationRunObservationBarrierV0>,
    automation_candidates: &mut VecDeque<QueuedAutomationV0>,
) {
    *fixed_point = None;
    *automation_watch = None;
    automation_candidates.clear();
}

fn watch_targets_root(event: &WatchEventV0, root_name: &str) -> bool {
    match event {
        WatchEventV0::Added(object)
        | WatchEventV0::Modified(object)
        | WatchEventV0::Deleted(object) => object.reference.name == root_name,
        WatchEventV0::Bookmark { .. } => false,
    }
}

/// Only terminal failures from the named watch source may use its existing
/// rebarrier or restart path. Request conflicts retain command-local semantics.
fn is_watch_terminal_for(error: &TransportErrorV0, resource: ResourceKindV0) -> bool {
    matches!(
        error,
        TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow
    ) || matches!(
        error,
        TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal {
            resource: error_resource,
            ..
        }) if *error_resource == resource
    )
}

fn is_root_watch_terminal(error: &TransportErrorV0) -> bool {
    is_watch_terminal_for(error, ResourceKindV0::WebApplication)
}

fn is_automation_watch_terminal(error: &TransportErrorV0) -> bool {
    is_watch_terminal_for(error, ResourceKindV0::AutomationRun)
}

fn is_dependency_watch_terminal(error: &TransportErrorV0) -> bool {
    is_watch_terminal_for(error, ResourceKindV0::ConfigMap)
        || is_watch_terminal_for(error, ResourceKindV0::Service)
        || is_watch_terminal_for(error, ResourceKindV0::WebApplication)
        || is_watch_terminal_for(error, ResourceKindV0::OxideLoopState)
        || is_watch_terminal_for(error, ResourceKindV0::AutomationRun)
}

fn root_address(
    actor: &RootActorV0,
) -> Result<oxideloop_kube_transport::RootAddressV0, RuntimeErrorV0> {
    let root = actor.state().root();
    // A bootstrap root intentionally has no trusted state link yet, but the
    // first owned command still addresses the deterministic state name. Once
    // bootstrap is acknowledged, the linked-state pin remains authoritative.
    let state_name = if root.state_name.is_empty() {
        oxideloop_controller_api::state_name(&root.uid)
            .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))?
    } else {
        root.state_name.clone()
    };
    Ok(oxideloop_kube_transport::RootAddressV0 {
        namespace: root.namespace.clone(),
        name: root.name.clone(),
        state_name,
    })
}

/// The two irreversible boundaries in the closed deletion-authority
/// progression.  A replacement actor is allowed only after this validator
/// binds the old actor, fresh raw classifier result, and the one pending
/// effect to the same root and cleanup receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeletionAuthorityCutV0 {
    LinkedToRetired,
    RetiredToRootOnly,
}

fn recovery_preconditions_match(
    preconditions: &PreconditionsV0,
    root: &oxideloop_controller_core::RootSnapshotV0,
) -> bool {
    preconditions.root_uid == root.uid
        && preconditions.root_generation == root.generation
        && preconditions.root_resource_version == root.root_resource_version
        && preconditions.state_uid == root.state_uid
        && preconditions.state_resource_version == root.state_resource_version
        && preconditions.observation_token.is_empty()
        && preconditions.cache_epoch.is_empty()
}

fn exact_deletion_link(
    root: &oxideloop_controller_core::RootSnapshotV0,
) -> Result<
    (
        &oxideloop_controller_api::DeletionCleanupReceipt,
        &oxideloop_controller_api::StateLink,
    ),
    RuntimeErrorV0,
> {
    let receipt =
        oxideloop_controller_core::validate_deleting_root_receipt(root).map_err(|error| {
            RuntimeErrorV0::Hold(format!("deletion cut receipt is invalid: {error}"))
        })?;
    let link =
        root.status.state.as_ref().ok_or_else(|| {
            RuntimeErrorV0::Hold("deletion cut root has no exact State link".into())
        })?;
    let deterministic_name = oxideloop_controller_api::state_name(&root.uid)
        .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))?;
    if !root.deleting
        || !root.root_finalizer_present
        || root.state_name != deterministic_name
        || root.state_uid.is_empty()
        || link.root_uid != root.uid
        || link.name != deterministic_name
        || link.uid != root.state_uid
        || receipt.root_uid != root.uid
        || receipt.state_name != deterministic_name
        || receipt.state_uid != root.state_uid
    {
        return Err(RuntimeErrorV0::Hold(
            "deletion cut root, receipt, or State link is not exact".into(),
        ));
    }
    Ok((receipt, link))
}

/// Validates the only two whole-actor replacement cuts.  This is production
/// authority validation, not a failpoint assertion: no same-name root,
/// receipt, State UID/RV, or pending effect may be adopted across either cut.
fn validate_deletion_authority_cut(
    cut: DeletionAuthorityCutV0,
    active: &MachineStateV0,
    fresh: &MachineStateV0,
    pending: &CommandV0,
) -> Result<(), RuntimeErrorV0> {
    let active_root = active.root();
    let fresh_root = fresh.root();
    let (active_receipt, active_link) = exact_deletion_link(active_root)?;
    let (fresh_receipt, fresh_link) = exact_deletion_link(fresh_root)?;
    if active_root.uid != fresh_root.uid
        || active_root.namespace != fresh_root.namespace
        || active_root.name != fresh_root.name
        || active_root.generation != fresh_root.generation
        || active_root.root_resource_version != fresh_root.root_resource_version
        || active_receipt != fresh_receipt
        || active_link != fresh_link
    {
        return Err(RuntimeErrorV0::Hold(
            "deletion authority cut changed root identity, version, receipt, or State link".into(),
        ));
    }

    match cut {
        DeletionAuthorityCutV0::LinkedToRetired => {
            if !active.is_retired_state_recovery()
                || !fresh.is_retired_state_recovery()
                || active_root.state_resource_version.is_empty()
                || active_root.state_resource_version != fresh_root.state_resource_version
                || active_root.state_finalizer_present
                || fresh_root.state_finalizer_present
            {
                return Err(RuntimeErrorV0::Hold(
                    "linked-to-retired cut did not retain the exact finalizer-free State tombstone"
                        .into(),
                ));
            }
            let expected = retired_state_delete_command_token(
                &fresh_root.uid,
                fresh_root.generation,
                &fresh_root.root_resource_version,
                &fresh_root.state_name,
                &fresh_root.state_uid,
                &fresh_root.state_resource_version,
                &fresh_receipt.deletion_id,
                fresh_receipt.deletion_start_generation,
                &fresh_receipt.cleanup_token,
            );
            if !matches!(
                pending,
                CommandV0::DeleteRetiredState { token, preconditions }
                    if token == &expected
                        && recovery_preconditions_match(preconditions, active_root)
                        && recovery_preconditions_match(preconditions, fresh_root)
            ) {
                return Err(RuntimeErrorV0::Hold(
                    "linked-to-retired cut pending delete does not match exact root and State pins"
                        .into(),
                ));
            }
        }
        DeletionAuthorityCutV0::RetiredToRootOnly => {
            if !active.is_root_only_deletion_recovery()
                || !fresh.is_root_only_deletion_recovery()
                || !active_root.state_resource_version.is_empty()
                || !fresh_root.state_resource_version.is_empty()
                || active_root.state_finalizer_present
                || fresh_root.state_finalizer_present
            {
                return Err(RuntimeErrorV0::Hold(
                    "retired-to-root-only cut did not prove State absence under the exact receipt"
                        .into(),
                ));
            }
            let expected = root_only_owned_children_verify_token(
                &fresh_receipt.deletion_id,
                fresh_receipt.deletion_start_generation,
                &fresh_root.uid,
                fresh_root.generation,
                &fresh_root.root_resource_version,
                &fresh_root.state_uid,
            );
            if !matches!(
                pending,
                CommandV0::VerifyOwnedChildrenAbsent { token, preconditions }
                    if token == &expected && recovery_preconditions_match(preconditions, fresh_root)
            ) {
                return Err(RuntimeErrorV0::Hold(
                    "retired-to-root-only cut pending child proof does not match exact root-only pins"
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(all(feature = "test-failpoints", test))]
fn is_exact_state_finalizer_result(command: &CommandV0, event: &EventV0) -> bool {
    matches!(
        (command, event),
        (
            CommandV0::RemoveStateFinalizer { token, preconditions },
            EventV0::StateFinalizerRemoved {
                token: event_token,
                state_uid,
                old_state_resource_version,
                ..
            },
        ) if token == event_token
            && state_uid == &preconditions.state_uid
            && old_state_resource_version == &preconditions.state_resource_version
    )
}

#[cfg(feature = "test-failpoints")]
fn reject_late_deletion_barrier(
    command: &CommandV0,
    barrier_is_armed: bool,
) -> Result<(), RuntimeErrorV0> {
    if barrier_is_armed && matches!(command, CommandV0::RemoveRootFinalizer { .. }) {
        Err(RuntimeErrorV0::Hold(
            "deletion barrier was armed after the state-finalizer seam; refusing root-finalizer authority"
                .into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(feature = "test-failpoints")]
fn reject_late_prepare_barrier(
    command: &CommandV0,
    barrier_is_armed: bool,
) -> Result<(), RuntimeErrorV0> {
    if barrier_is_armed && matches!(command, CommandV0::Mutate { .. }) {
        Err(RuntimeErrorV0::Hold(
            "prepared-dispatch barrier was armed after the Prepare seam; refusing mutation authority"
                .into(),
        ))
    } else {
        Ok(())
    }
}

/// Proves the exact retired-State recovery boundary before exposing the H03
/// midpoint. The actor already owns the exact pending DELETE but no request
/// has been polled; the independently loaded machine proves the raw
/// tombstone classifier accepted the same root/state pins.
#[cfg(feature = "test-failpoints")]
fn verify_deletion_barrier_and_wait(
    barrier: EffectBarrierV0,
    pending_delete: &CommandV0,
    active: &RootActorV0,
    fresh: &MachineStateV0,
) -> Result<(), RuntimeErrorV0> {
    validate_deletion_authority_cut(
        DeletionAuthorityCutV0::LinkedToRetired,
        active.state(),
        fresh,
        pending_delete,
    )?;
    if !is_exact_retired_state_delete(pending_delete, active.state()) {
        return Err(RuntimeErrorV0::Hold(
            "deletion barrier did not retain exact retired State delete authority".into(),
        ));
    }
    validate_deletion_barrier_root(active.state().root(), fresh.root())?;
    if active.state().root().state_uid != fresh.root().state_uid
        || active.state().root().state_resource_version != fresh.root().state_resource_version
    {
        return Err(RuntimeErrorV0::Hold(
            "deletion barrier retired State pins changed during reclassification".into(),
        ));
    }
    tokio::task::block_in_place(|| barrier.reached_and_wait_for_release())
        .map_err(|error| RuntimeErrorV0::Hold(format!("deletion barrier failed: {error}")))
}

#[cfg(feature = "test-failpoints")]
fn is_exact_retired_state_delete(command: &CommandV0, state: &MachineStateV0) -> bool {
    let CommandV0::DeleteRetiredState { preconditions, .. } = command else {
        return false;
    };
    let root = state.root();
    state.is_retired_state_recovery()
        && preconditions.root_uid == root.uid
        && preconditions.root_generation == root.generation
        && preconditions.root_resource_version == root.root_resource_version
        && preconditions.state_uid == root.state_uid
        && preconditions.state_resource_version == root.state_resource_version
        && !preconditions.state_resource_version.is_empty()
}

#[cfg(feature = "test-failpoints")]
fn validate_deletion_barrier_root(
    original: &oxideloop_controller_core::RootSnapshotV0,
    current: &oxideloop_controller_core::RootSnapshotV0,
) -> Result<(), RuntimeErrorV0> {
    if current.uid != original.uid
        || current.namespace != original.namespace
        || current.name != original.name
        || !current.deleting
        || !current.root_finalizer_present
        || current.status.deletion_cleanup_receipt != original.status.deletion_cleanup_receipt
    {
        return Err(RuntimeErrorV0::Hold(
            "deletion barrier root/receipt proof changed or is incomplete".into(),
        ));
    }
    oxideloop_controller_core::validate_deleting_root_receipt(current).map_err(|error| {
        RuntimeErrorV0::Hold(format!("deletion barrier receipt is invalid: {error}"))
    })?;
    Ok(())
}

/// Deletion of a state object can lag its successful finalizer patch. Only
/// the exact state UID may remain during this bounded wait; a same-name
/// replacement is never a reason to wait or continue the deletion protocol.
#[cfg(all(feature = "test-failpoints", test))]
async fn wait_for_exact_state_absence_with<F, Fut>(
    mut reread: F,
    expected_uid: &str,
    total_deadline: std::time::Duration,
    poll_interval: std::time::Duration,
) -> Result<(), RuntimeErrorV0>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<ObjectV0>, RuntimeErrorV0>>,
{
    let deadline = TokioInstant::now() + total_deadline;
    loop {
        let observed = timeout_at(deadline, reread()).await.map_err(|_| {
            RuntimeErrorV0::Hold(
                "deletion barrier deterministic state reread exceeded deadline".into(),
            )
        })??;
        if state_absence_step(observed.as_ref(), expected_uid)? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(TokioInstant::now());
        if remaining.is_zero() {
            return Err(RuntimeErrorV0::Hold(
                "deletion barrier deterministic state did not become absent before deadline".into(),
            ));
        }
        sleep(poll_interval.min(remaining)).await;
    }
}

/// Returns true only for absence; the exact pre-delete UID is retryable and
/// any replacement closes the failpoint without exposing a midpoint.
#[cfg(all(feature = "test-failpoints", test))]
fn state_absence_step(
    observed: Option<&ObjectV0>,
    expected_uid: &str,
) -> Result<bool, RuntimeErrorV0> {
    match observed {
        None => Ok(true),
        Some(observed) if observed.metadata.uid == expected_uid => Ok(false),
        Some(_) => Err(RuntimeErrorV0::Hold(
            "deletion barrier deterministic state name was replaced".into(),
        )),
    }
}

fn render_input(
    actor: &RootActorV0,
    preconditions: &PreconditionsV0,
) -> Result<RenderInputV0, RuntimeErrorV0> {
    let root = actor.state().root();
    Ok(RenderInputV0 {
        abi_version: ABI_VERSION_V0.into(),
        root: RootProjection {
            api_version: "demo.oxideloop.dev/v1alpha1".into(),
            kind: "WebApplication".into(),
            namespace: root.namespace.clone(),
            name: root.name.clone(),
            uid: root.uid.clone(),
            generation: u64::try_from(root.generation)
                .map_err(|_| RuntimeErrorV0::Hold("root generation is negative".into()))?,
            spec: serde_json::to_value(&root.spec).map_err(|_| RuntimeErrorV0::RendererAbi)?,
        },
        bundle_digest: root.bundle_digest.clone(),
        cache_epoch: preconditions.cache_epoch.clone(),
        observation_token: preconditions.observation_token.clone(),
        observations: vec![],
    })
}

async fn descriptor_for_program(
    transport: &KubeTransportV0,
    store: &BundleStore,
    namespace: &str,
    program: &ProgramBundleReference,
) -> Result<BundleDescriptorV0, RuntimeErrorV0> {
    program
        .validate()
        .map_err(|_| RuntimeErrorV0::Hold("program bundle reference is invalid".into()))?;
    let installed = if let Ok(installed) = store.open_installed(&program.digest) {
        installed
    } else {
        let source = transport
            .read_bundle_binary_data(
                namespace,
                namespace,
                &program.config_map_name,
                &program.key,
                usize::try_from(MAX_BUNDLE_BYTES).map_err(|_| {
                    RuntimeErrorV0::Hold("bundle byte limit cannot fit this platform".into())
                })?,
            )
            .await?;
        install_archive(store, &source.bytes, &program.digest)?
    };
    installed.descriptor().map_err(Into::into)
}

fn descriptor_for_digest<'a>(
    descriptors: &'a BTreeMap<String, BundleDescriptorV0>,
    digest: &str,
) -> Option<&'a BundleDescriptorV0> {
    descriptors.get(digest)
}

async fn establish_pending_watch(
    actor: &RootActorV0,
    transport: &KubeTransportV0,
) -> Result<Option<AutomationRunObservationBarrierV0>, RuntimeErrorV0> {
    let Some(pending) = actor.state().journal().state.automation.pending.first() else {
        return Ok(None);
    };
    let expectation = AutomationRunExpectationV0::from_pending(
        ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: actor.state().root().namespace.clone(),
            name: actor.state().root().name.clone(),
        },
        &actor.state().root().uid,
        pending,
    )?;
    let port = KubeAutomationObservationPortV0 { transport };
    let mut restarts = 0;
    loop {
        match port.establish(expectation.clone()).await {
            Ok(barrier) => return Ok(Some(barrier)),
            Err(RuntimeErrorV0::Discovery(error)) if is_automation_watch_terminal(&error) => {
                if restarts == MAX_BARRIER_WATCH_RESTARTS {
                    return Err(RuntimeErrorV0::Discovery(error));
                }
                restarts += 1;
                wait_before_barrier_restart(restarts).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn wait_before_barrier_restart(restart: usize) {
    tokio::task::yield_now().await;
    let delay = BARRIER_WATCH_RESTART_DELAY_MS * u64::try_from(restart).unwrap_or(u64::MAX);
    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
}

fn automation_execution(
    actor: &RootActorV0,
    run: &oxideloop_kube_transport::ValidatedAutomationRunV0,
) -> Result<crate::AutomationExecutionV0, RuntimeErrorV0> {
    let spec = run.spec();
    if spec.root_uid != actor.state().root().uid || actor.state().root().deleting {
        return Err(RuntimeErrorV0::Hold(
            "AutomationRun root pin does not permit action execution".into(),
        ));
    }
    let root = planner_ref(
        "demo.oxideloop.dev/v1alpha1",
        "WebApplication",
        &actor.state().root().namespace,
        &actor.state().root().name,
        &actor.state().root().uid,
        &actor.state().root().root_resource_version,
    );
    let source = planner_ref(
        &spec.source.api_version,
        &spec.source.kind,
        &spec.source.namespace,
        &spec.source.name,
        &spec.source_uid,
        &spec.source_resource_version,
    );
    let execution_run = planner_ref(
        "runtime.oxideloop.dev/v1alpha1",
        "AutomationRun",
        &run.reference().namespace,
        &run.reference().name,
        run.uid(),
        run.pending_resource_version(),
    );
    let correlation = RunCorrelationV0 {
        occurrence_id: spec.occurrence_id.clone(),
        root: root.clone(),
        run: execution_run.clone(),
        source: source.identity.clone(),
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
    };
    Ok(crate::AutomationExecutionV0 {
        root,
        source,
        run: execution_run,
        correlation,
    })
}

fn planner_ref(
    api_version: &str,
    kind: &str,
    namespace: &str,
    name: &str,
    uid: &str,
    resource_version: &str,
) -> PlannerObjectRefV0 {
    PlannerObjectRefV0 {
        identity: PlannerIdentityV0 {
            api_version: api_version.into(),
            kind: kind.into(),
            namespace: namespace.into(),
            name: name.into(),
        },
        uid: uid.into(),
        resource_version: resource_version.into(),
    }
}

fn event_time() -> Result<String, RuntimeErrorV0> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| RuntimeErrorV0::Hold("cannot format Event timestamp".into()))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "test-failpoints")]
    use std::collections::VecDeque;
    use std::{
        collections::BTreeMap,
        convert::Infallible,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use http::{Request, Response};
    use http_body_util::BodyExt;
    use kube::{Client, client::Body};
    use oxideloop_bundle_contract::{
        BUNDLE_MANIFEST_VERSION_V0, BundleInstallRecordV0, BundleIntegrityManifestV0,
        DescriptorModuleV0, ExecutionPhaseV0, FileIntegrityV0, ModuleIntegrityV0, PhaseModuleV0,
        ToolIdentityV0,
    };
    use oxideloop_controller_api::{
        AutomationPending, AutomationRunCompletion, AutomationRunCompletionState,
        AutomationRunSpec, AutomationRunStatus, AutomationState, ObjectIdentity,
        OxideLoopStateSpec, RootBinding, STATE_VERSION_V1, StateBody, StateLink,
        WebApplicationSpec, WebApplicationStatus, automation_run_name, bootstrap_id, state_name,
    };
    use oxideloop_controller_core::{DesiredObservationV0, FrozenInputV0, RootSnapshotV0};
    use oxideloop_kube_transport::{ObjectMetaV0, ObjectV0};
    use oxideloop_mutation_model::{BackendQualification, Observation as MutationObservation};
    use oxideloop_renderer_abi::{
        ActionInputV0, canonical_bytes,
        model::{
            ABI_VERSION_V0, AutomationDeclaration, Dependency, Observation, RenderDisposition,
            RenderOutputV0, ResourceDeclaration,
        },
        parse_validate_render_input, sha256_hex,
    };
    use serde_json::json;
    use tower::service_fn;

    use super::*;

    const ROOT_UID: &str = "root-uid";
    const BUNDLE_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ACTION_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn root_event(resource_version: &str) -> ObjectV0 {
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "root".into(),
            },
            metadata: ObjectMetaV0 {
                uid: ROOT_UID.into(),
                resource_version: resource_version.into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"root","namespace":"ns","uid":ROOT_UID,"resourceVersion":resource_version}}),
        }
    }

    #[cfg(feature = "test-failpoints")]
    #[test]
    fn deletion_barrier_state_absence_waits_only_for_the_exact_deleted_uid() {
        let original = root_event("state-rv");
        assert!(!state_absence_step(Some(&original), ROOT_UID).expect("same UID is retryable"));
        assert!(state_absence_step(None, ROOT_UID).expect("absence is accepted"));

        let mut replacement = original;
        replacement.metadata.uid = "replacement-uid".into();
        assert!(matches!(
            state_absence_step(Some(&replacement), ROOT_UID),
            Err(RuntimeErrorV0::Hold(reason)) if reason.contains("replaced")
        ));
    }

    #[cfg(feature = "test-failpoints")]
    #[test]
    fn deletion_barrier_root_proof_fails_closed_on_uid_finalizer_or_receipt_change() {
        let original = receipt_bounded_root_only_machine().root().clone();
        validate_deletion_barrier_root(&original, &original).expect("complete original proof");

        let mut replacement = original.clone();
        replacement.uid = "replacement-uid".into();
        assert!(validate_deletion_barrier_root(&original, &replacement).is_err());

        let mut missing_finalizer = original.clone();
        missing_finalizer.root_finalizer_present = false;
        assert!(validate_deletion_barrier_root(&original, &missing_finalizer).is_err());

        let mut missing_receipt = original.clone();
        missing_receipt.status.deletion_cleanup_receipt = None;
        assert!(validate_deletion_barrier_root(&original, &missing_receipt).is_err());

        let mutations: [fn(&mut oxideloop_controller_api::DeletionCleanupReceipt); 6] = [
            |receipt: &mut oxideloop_controller_api::DeletionCleanupReceipt| {
                receipt.deletion_id = "f".repeat(64);
            },
            |receipt: &mut oxideloop_controller_api::DeletionCleanupReceipt| {
                receipt.deletion_start_generation += 1;
            },
            |receipt: &mut oxideloop_controller_api::DeletionCleanupReceipt| {
                receipt.cleanup_token = "f".repeat(64);
            },
            |receipt: &mut oxideloop_controller_api::DeletionCleanupReceipt| {
                receipt.state_name = "other-state".into();
            },
            |receipt: &mut oxideloop_controller_api::DeletionCleanupReceipt| {
                receipt.state_uid = "other-state-uid".into();
            },
            |receipt: &mut oxideloop_controller_api::DeletionCleanupReceipt| {
                receipt.root_uid = "other-root-uid".into();
            },
        ];
        for mutate in mutations {
            let mut changed = original.clone();
            mutate(
                changed
                    .status
                    .deletion_cleanup_receipt
                    .as_mut()
                    .expect("receipt"),
            );
            assert!(validate_deletion_barrier_root(&original, &changed).is_err());
        }
    }

    #[cfg(feature = "test-failpoints")]
    #[tokio::test]
    async fn deletion_barrier_waits_for_the_original_state_to_disappear() {
        let responses = Arc::new(Mutex::new(VecDeque::from([
            Some(root_event("state-rv-1")),
            Some(root_event("state-rv-2")),
            None,
        ])));
        wait_for_exact_state_absence_with(
            {
                let responses = Arc::clone(&responses);
                move || {
                    let responses = Arc::clone(&responses);
                    async move { Ok(responses.lock().expect("responses").pop_front().unwrap()) }
                }
            },
            ROOT_UID,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(1),
        )
        .await
        .expect("state eventually disappears");
        assert!(responses.lock().expect("responses").is_empty());
    }

    #[cfg(feature = "test-failpoints")]
    #[tokio::test]
    async fn deletion_barrier_state_absence_timeout_and_replacement_fail_closed() {
        let timeout = wait_for_exact_state_absence_with(
            || async { Ok(Some(root_event("state-rv"))) },
            ROOT_UID,
            std::time::Duration::from_millis(2),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(matches!(
            timeout,
            Err(RuntimeErrorV0::Hold(reason)) if reason.contains("did not become absent")
        ));

        let mut replacement = root_event("state-rv");
        replacement.metadata.uid = "replacement".into();
        let replacement = wait_for_exact_state_absence_with(
            || {
                let replacement = replacement.clone();
                async move { Ok(Some(replacement)) }
            },
            ROOT_UID,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(matches!(
            replacement,
            Err(RuntimeErrorV0::Hold(reason)) if reason.contains("replaced")
        ));

        let stalled = wait_for_exact_state_absence_with(
            || async { std::future::pending::<Result<Option<ObjectV0>, RuntimeErrorV0>>().await },
            ROOT_UID,
            std::time::Duration::from_millis(2),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(matches!(
            stalled,
            Err(RuntimeErrorV0::Hold(reason)) if reason.contains("reread exceeded deadline")
        ));
    }

    #[cfg(feature = "test-failpoints")]
    #[test]
    fn deletion_barrier_intercepts_only_the_exact_state_finalizer_ack() {
        let preconditions = PreconditionsV0 {
            root_uid: ROOT_UID.into(),
            root_generation: 1,
            root_resource_version: "root-rv".into(),
            state_uid: "state-uid".into(),
            state_resource_version: "state-rv".into(),
            observation_token: "observation".into(),
            cache_epoch: "epoch".into(),
        };
        let command = CommandV0::RemoveStateFinalizer {
            token: "state-finalizer-token".into(),
            preconditions: preconditions.clone(),
        };
        let exact = EventV0::StateFinalizerRemoved {
            token: "state-finalizer-token".into(),
            state_uid: "state-uid".into(),
            old_state_resource_version: "state-rv".into(),
            new_state_resource_version: "state-rv-next".into(),
        };
        assert!(is_exact_state_finalizer_result(&command, &exact));

        let stale = EventV0::StateFinalizerRemoved {
            token: "state-finalizer-token".into(),
            state_uid: "state-uid".into(),
            old_state_resource_version: "stale".into(),
            new_state_resource_version: "state-rv-next".into(),
        };
        assert!(!is_exact_state_finalizer_result(&command, &stale));
        let root_command = CommandV0::RemoveRootFinalizer {
            token: "root-finalizer-token".into(),
            preconditions,
        };
        assert!(!is_exact_state_finalizer_result(&root_command, &exact));
        assert!(reject_late_deletion_barrier(&root_command, true).is_err());
        assert!(reject_late_deletion_barrier(&root_command, false).is_ok());
        assert!(reject_late_deletion_barrier(&command, true).is_ok());
        let mutate = CommandV0::Mutate {
            transaction_id: "transaction".into(),
            attempt_token: "attempt".into(),
            commands: vec![],
        };
        assert!(reject_late_prepare_barrier(&mutate, true).is_err());
        assert!(reject_late_prepare_barrier(&mutate, false).is_ok());
        assert!(reject_late_prepare_barrier(&command, true).is_ok());
    }

    fn dependency_event(resource_version: &str) -> ObjectV0 {
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::ConfigMap,
                namespace: "ns".into(),
                name: "dependency".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "dependency-uid".into(),
                resource_version: resource_version.into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"dependency","namespace":"ns","uid":"dependency-uid","resourceVersion":resource_version}}),
        }
    }

    fn automation_spec() -> AutomationRunSpec {
        let event_metadata = json!({"trigger":"driver-test"});
        let input = ActionInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            bundle_digest: BUNDLE_DIGEST.into(),
            automation_identity: "driver-test".into(),
            automation_version: "v1".into(),
            action_module_identity: "actions/driver-test.mjs".into(),
            action_module_digest: ACTION_DIGEST.into(),
            occurrence_id: "occurrence-1".into(),
            source_transition_id: "transition-1".into(),
            source_uid: "source-uid".into(),
            source_resource_version: "7".into(),
            event_metadata: event_metadata.clone(),
        };
        let input_bytes = canonical_bytes(&serde_json::to_value(input).expect("input value"))
            .expect("canonical action input");
        AutomationRunSpec {
            occurrence_id: "occurrence-1".into(),
            root_uid: ROOT_UID.into(),
            source: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "source".into(),
            },
            source_uid: "source-uid".into(),
            source_resource_version: "7".into(),
            source_transition_id: "transition-1".into(),
            automation_id: "driver-test".into(),
            automation_version: "v1".into(),
            bundle_digest: BUNDLE_DIGEST.into(),
            program: ProgramBundleReference {
                config_map_name: "bundle".into(),
                key: "program.mjs".into(),
                digest: BUNDLE_DIGEST.into(),
            },
            action_module_identity: "actions/driver-test.mjs".into(),
            action_module_digest: ACTION_DIGEST.into(),
            renderer_abi_version: ABI_VERSION_V0.into(),
            action_input_v0: String::from_utf8(input_bytes.clone()).expect("UTF-8 action input"),
            action_input_hash: sha256_hex(&input_bytes),
            event_metadata_hash: sha256_hex(
                &canonical_bytes(&event_metadata).expect("canonical event metadata"),
            ),
        }
    }

    fn automation_object(
        spec: &AutomationRunSpec,
        name: &str,
        resource_version: &str,
        status: &AutomationRunStatus,
    ) -> ObjectV0 {
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::AutomationRun,
                namespace: "ns".into(),
                name: name.into(),
            },
            metadata: ObjectMetaV0 {
                uid: "run-uid".into(),
                resource_version: resource_version.into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![ROOT_UID.into()],
            },
            value: json!({
                "apiVersion":"runtime.oxideloop.dev/v1alpha1",
                "kind":"AutomationRun",
                "metadata": {
                    "name":name,
                    "namespace":"ns",
                    "uid":"run-uid",
                    "resourceVersion":resource_version,
                    "annotations":{},
                    "ownerReferences":[{
                        "apiVersion":"demo.oxideloop.dev/v1alpha1",
                        "kind":"WebApplication",
                        "name":"root",
                        "uid":ROOT_UID,
                        "controller":true,
                        "blockOwnerDeletion":false
                    }]
                },
                "spec":spec,
                "status":status,
            }),
        }
    }

    fn automation_candidate_actor() -> RootActorV0 {
        automation_candidate_actor_for_digest(BUNDLE_DIGEST)
    }

    fn automation_candidate_actor_for_digest(bundle_digest: &str) -> RootActorV0 {
        let program = ProgramBundleReference {
            config_map_name: "bundle".into(),
            key: "program.mjs".into(),
            digest: bundle_digest.into(),
        };
        RootActorV0::new(
            MachineStateV0::bootstrap(RootSnapshotV0 {
                uid: ROOT_UID.into(),
                namespace: "ns".into(),
                name: "root".into(),
                generation: 1,
                spec: WebApplicationSpec {
                    message: "driver automation".into(),
                    expose: false,
                    program,
                },
                bundle_digest: bundle_digest.into(),
                status: WebApplicationStatus::default(),
                state_name: String::new(),
                state_uid: String::new(),
                state_resource_version: String::new(),
                root_resource_version: "1".into(),
                root_finalizer_present: false,
                state_finalizer_present: false,
                deleting: false,
            })
            .expect("bootstrap actor"),
        )
    }

    fn pending_automation_actor() -> RootActorV0 {
        let spec = automation_spec();
        let run_name = automation_run_name(ROOT_UID, &spec.occurrence_id).expect("run name");
        let spec_hash = sha256_hex(
            &canonical_bytes(&serde_json::to_value(&spec).expect("spec value"))
                .expect("canonical spec"),
        );
        let state_name = state_name(ROOT_UID).expect("state name");
        RootActorV0::new(
            MachineStateV0::new(
                RootSnapshotV0 {
                    uid: ROOT_UID.into(),
                    namespace: "ns".into(),
                    name: "root".into(),
                    generation: 1,
                    spec: WebApplicationSpec {
                        message: "pending automation".into(),
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
                        ..WebApplicationStatus::default()
                    },
                    state_name,
                    state_uid: "state-uid".into(),
                    state_resource_version: "state-rv".into(),
                    root_resource_version: "root-rv".into(),
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
                    state: StateBody {
                        automation: AutomationState {
                            pending: vec![AutomationPending {
                                occurrence_id: spec.occurrence_id,
                                automation_id: spec.automation_id,
                                source: spec.source,
                                run_name,
                                run_uid: "run-uid".into(),
                                run_resource_version: "31".into(),
                                spec_hash,
                            }],
                            ..AutomationState::default()
                        },
                        ..StateBody::default()
                    },
                },
            )
            .expect("valid pending automation machine"),
        )
    }

    fn render_ready_actor_for_coverage_test() -> RootActorV0 {
        let root_uid = ROOT_UID;
        let state_name = state_name(root_uid).expect("state name");
        let machine = MachineStateV0::new(
            RootSnapshotV0 {
                uid: root_uid.into(),
                namespace: "ns".into(),
                name: "root".into(),
                generation: 1,
                spec: WebApplicationSpec {
                    message: "coverage race".into(),
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
                        root_uid: root_uid.into(),
                        name: state_name.clone(),
                        uid: "state-uid".into(),
                    }),
                    ..WebApplicationStatus::default()
                },
                state_name,
                state_uid: "state-uid".into(),
                state_resource_version: "state-rv".into(),
                root_resource_version: "root-rv".into(),
                root_finalizer_present: true,
                state_finalizer_present: true,
                deleting: false,
            },
            OxideLoopStateSpec {
                state_version: STATE_VERSION_V1,
                root: RootBinding {
                    uid: root_uid.into(),
                    bootstrap_id: bootstrap_id(root_uid).expect("bootstrap id"),
                    bundle_digest: BUNDLE_DIGEST.into(),
                },
                state: StateBody::default(),
            },
        )
        .expect("valid coverage-race state");
        let mut actor = RootActorV0::new(machine);
        actor.start();
        let CommandV0::RequestDescriptor { token, .. } = actor
            .next_command()
            .expect("descriptor lookup")
            .expect("descriptor command")
        else {
            panic!("ready actor must request a descriptor");
        };
        actor
            .acknowledge(EventV0::DescriptorReady {
                token,
                bundle_digest: BUNDLE_DIGEST.into(),
            })
            .expect("descriptor acknowledgement");
        let CommandV0::PersistRootStatus {
            token,
            old_root_resource_version,
            root_uid,
            ..
        } = actor
            .next_command()
            .expect("status lookup")
            .expect("status command")
        else {
            panic!("ready actor must persist descriptor status");
        };
        actor
            .acknowledge(EventV0::RootStatusPersisted {
                token,
                old_root_resource_version,
                new_root_resource_version: "root-rv-after-descriptor".into(),
                root_uid,
            })
            .expect("descriptor status acknowledgement");
        assert!(actor.next_command().expect("informer transition").is_none());
        actor
    }

    /// Produces the real reducer-owned status command used by the prewrite
    /// collector tests.  Keeping this fixture inside the runtime crate means
    /// the wire tests exercise the same pending-status witness that the
    /// production driver captures immediately before dispatch.
    fn prewrite_status_actor() -> (RootActorV0, CommandV0) {
        let mut actor = render_ready_actor_for_coverage_test();
        let barrier = actor.barrier();
        actor
            .prepare_initial_informer_observation(barrier)
            .expect("sealed informer observation");
        actor.informers_ready(barrier);
        let CommandV0::RequestRender { preconditions } = actor
            .next_command()
            .expect("informer readiness reduces")
            .expect("initial render command")
        else {
            panic!("informer readiness did not issue a render");
        };
        let root = actor.state().root();
        let input = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: root.namespace.clone(),
                name: root.name.clone(),
                uid: root.uid.clone(),
                generation: u64::try_from(root.generation).expect("nonnegative generation"),
                spec: serde_json::to_value(&root.spec).expect("root spec serializes"),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: preconditions.cache_epoch.clone(),
            observation_token: preconditions.observation_token.clone(),
            observations: vec![],
        };
        let input_json = serde_json::to_string(&input).expect("render input serializes");
        let output_json = serde_json::to_string(&RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::ExplicitEmpty,
            resources: vec![],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        })
        .expect("render output serializes");
        let frozen = oxideloop_controller_core::FrozenInputV0 {
            root_generation: root.generation,
            state_resource_version: preconditions.state_resource_version,
            observation_token: input.observation_token.clone(),
            cache_epoch: input.cache_epoch.clone(),
            final_observation_fence_token: sha256_hex(
                format!("prewrite-status-fence/v0/{}", input.observation_token).as_bytes(),
            ),
            render_input_hash: oxideloop_renderer_abi::parse_validate_render_input(&input_json)
                .expect("canonical render input")
                .sha256,
        };
        actor
            .accept_acknowledgement(EventV0::Rendered {
                frozen,
                render_input_json: input_json,
                render_output_json: output_json,
                observed: vec![],
            })
            .expect("render acknowledgement");
        let command = actor
            .next_command()
            .expect("render acknowledgement reduces")
            .expect("root-status continuation");
        assert!(matches!(command, CommandV0::PersistRootStatus { .. }));
        (actor, command)
    }

    fn prewrite_wire_root(
        actor: &RootActorV0,
        status: &WebApplicationStatus,
        resource_version: &str,
        message: Option<&str>,
    ) -> serde_json::Value {
        let root = actor.state().root();
        let mut spec = serde_json::to_value(&root.spec).expect("root spec serializes");
        if let Some(message) = message {
            spec["message"] = json!(message);
        }
        json!({
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "kind":"WebApplication",
            "metadata": {
                "name":root.name,
                "namespace":root.namespace,
                "uid":root.uid,
                "resourceVersion":resource_version,
                "generation":root.generation,
                "finalizers":[oxideloop_controller_api::ROOT_FINALIZER],
                "annotations":{oxideloop_kube_transport::ROOT_UID_ANNOTATION:root.uid},
            },
            "spec":spec,
            "status":status,
        })
    }

    fn prewrite_wire_state(actor: &RootActorV0, resource_version: &str) -> serde_json::Value {
        let root = actor.state().root();
        json!({
            "apiVersion":"runtime.oxideloop.dev/v1alpha1",
            "kind":"OxideLoopState",
            "metadata": {
                "name":root.state_name,
                "namespace":root.namespace,
                "uid":root.state_uid,
                "resourceVersion":resource_version,
                "finalizers":[oxideloop_controller_api::STATE_FINALIZER],
                "annotations":{oxideloop_kube_transport::ROOT_UID_ANNOTATION:root.uid},
                "ownerReferences":[{
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplication",
                    "name":root.name,
                    "uid":root.uid,
                    "controller":true,
                    "blockOwnerDeletion":false,
                }],
            },
            "spec":actor.state().journal(),
            "status":{},
        })
    }

    fn receipt_bounded_root_only_machine() -> MachineStateV0 {
        let mut root = automation_candidate_actor().state().root().clone();
        let state_name = oxideloop_controller_api::state_name(&root.uid)
            .expect("deterministic deleted-state name");
        root.deleting = true;
        root.root_finalizer_present = true;
        root.state_name.clear();
        root.state_uid.clear();
        root.state_resource_version.clear();
        root.state_finalizer_present = false;
        root.status = WebApplicationStatus {
            deletion_cleanup_receipt: Some(oxideloop_controller_api::DeletionCleanupReceipt {
                root_uid: root.uid.clone(),
                deletion_id: "d".repeat(64),
                deletion_start_generation: root.generation,
                state_name,
                state_uid: "deleted-state-uid".into(),
                cleanup_token: "e".repeat(64),
            }),
            ..WebApplicationStatus::default()
        };
        MachineStateV0::recover_deleting_root_without_state(root)
            .expect("receipt-bounded root-only recovery")
    }

    fn receipt_bounded_retired_state_machine() -> MachineStateV0 {
        let mut root = receipt_bounded_root_only_machine().root().clone();
        let receipt = root
            .status
            .deletion_cleanup_receipt
            .as_ref()
            .expect("deletion receipt")
            .clone();
        root.state_name = receipt.state_name.clone();
        root.state_uid = receipt.state_uid.clone();
        root.state_resource_version = "retired-state-rv".into();
        root.status.state = Some(StateLink {
            root_uid: root.uid.clone(),
            name: receipt.state_name,
            uid: receipt.state_uid,
        });
        MachineStateV0::recover_deleting_root_with_retired_state(root)
            .expect("receipt-bounded retired-State recovery")
    }

    fn root_only_machine_from_retired(retired: &MachineStateV0) -> MachineStateV0 {
        let mut root = retired.root().clone();
        root.state_resource_version.clear();
        root.state_finalizer_present = false;
        MachineStateV0::recover_deleting_root_without_state(root)
            .expect("receipt-bounded root-only machine from retired State")
    }

    fn retired_machine_with(
        mutator: impl FnOnce(&mut oxideloop_controller_core::RootSnapshotV0),
    ) -> MachineStateV0 {
        let mut root = receipt_bounded_retired_state_machine().root().clone();
        mutator(&mut root);
        MachineStateV0::recover_deleting_root_with_retired_state(root)
            .expect("mutated fixture remains an internally valid retired State")
    }

    #[test]
    fn deletion_recovery_replaces_actor_at_each_authority_boundary() {
        let mut actor = automation_candidate_actor();
        actor.start();
        let retired = receipt_bounded_retired_state_machine();
        assert!(replace_actor_with_deletion_recovery(&mut actor, &retired));
        assert!(actor.state().is_retired_state_recovery());
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::DeleteRetiredState { preconditions, .. }))
                if preconditions.state_uid == "deleted-state-uid"
                    && preconditions.state_resource_version == "retired-state-rv"
        ));

        let root_only = receipt_bounded_root_only_machine();
        assert!(replace_actor_with_deletion_recovery(&mut actor, &root_only));
        assert!(actor.state().is_root_only_deletion_recovery());
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::VerifyOwnedChildrenAbsent { preconditions, .. }))
                if preconditions.state_uid == "deleted-state-uid"
                    && preconditions.state_resource_version.is_empty()
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn deletion_authority_cuts_require_the_same_root_receipt_link_and_state_pins() {
        let started_retired = oxideloop_controller_core::reduce(
            receipt_bounded_retired_state_machine(),
            EventV0::Start,
        )
        .expect("retired State starts its one delete");
        let delete = started_retired.commands[0].clone();
        let active_retired = started_retired.state;
        let fresh_retired = receipt_bounded_retired_state_machine();
        assert!(
            validate_deletion_authority_cut(
                DeletionAuthorityCutV0::LinkedToRetired,
                &active_retired,
                &fresh_retired,
                &delete,
            )
            .is_ok()
        );

        let (delete_token, delete_pins) = match &delete {
            CommandV0::DeleteRetiredState {
                token,
                preconditions,
            } => (token.clone(), preconditions.clone()),
            command => panic!("unexpected retired State command: {command:?}"),
        };
        let started_root_only = oxideloop_controller_core::reduce(
            active_retired,
            EventV0::RetiredStateDeleted {
                token: delete_token,
                state_uid: delete_pins.state_uid,
                state_resource_version: delete_pins.state_resource_version,
            },
        )
        .expect("exact delete acknowledgement enters root-only recovery");
        let verify = started_root_only.commands[0].clone();
        let active_root_only = started_root_only.state;
        let fresh_root_only = root_only_machine_from_retired(&fresh_retired);
        assert!(
            validate_deletion_authority_cut(
                DeletionAuthorityCutV0::RetiredToRootOnly,
                &active_root_only,
                &fresh_root_only,
                &verify,
            )
            .is_ok()
        );

        let root_uid_replacement = retired_machine_with(|root| {
            let replacement_uid = "replacement-root-uid".to_owned();
            let replacement_name = oxideloop_controller_api::state_name(&replacement_uid)
                .expect("replacement deterministic name");
            root.uid = replacement_uid.clone();
            root.state_name = replacement_name.clone();
            let link = root.status.state.as_mut().expect("State link");
            link.root_uid = replacement_uid.clone();
            link.name = replacement_name.clone();
            let receipt = root
                .status
                .deletion_cleanup_receipt
                .as_mut()
                .expect("cleanup receipt");
            receipt.root_uid = replacement_uid;
            receipt.state_name = replacement_name;
        });
        assert!(
            validate_deletion_authority_cut(
                DeletionAuthorityCutV0::LinkedToRetired,
                &fresh_retired,
                &root_uid_replacement,
                &delete,
            )
            .is_err()
        );

        let receipt_drift = retired_machine_with(|root| {
            root.status
                .deletion_cleanup_receipt
                .as_mut()
                .expect("cleanup receipt")
                .cleanup_token = "f".repeat(64);
        });
        assert!(
            validate_deletion_authority_cut(
                DeletionAuthorityCutV0::LinkedToRetired,
                &fresh_retired,
                &receipt_drift,
                &delete,
            )
            .is_err()
        );

        let state_uid_drift = retired_machine_with(|root| {
            root.state_uid = "replacement-state-uid".into();
            root.status.state.as_mut().expect("State link").uid = root.state_uid.clone();
            root.status
                .deletion_cleanup_receipt
                .as_mut()
                .expect("cleanup receipt")
                .state_uid = root.state_uid.clone();
        });
        assert!(
            validate_deletion_authority_cut(
                DeletionAuthorityCutV0::LinkedToRetired,
                &fresh_retired,
                &state_uid_drift,
                &delete,
            )
            .is_err()
        );

        let state_rv_drift = retired_machine_with(|root| {
            root.state_resource_version = "replacement-state-rv".into();
        });
        assert!(
            validate_deletion_authority_cut(
                DeletionAuthorityCutV0::LinkedToRetired,
                &fresh_retired,
                &state_rv_drift,
                &delete,
            )
            .is_err()
        );
    }

    #[test]
    fn deletion_recovery_observability_names_each_restricted_authority_cut() {
        let config = drain_root_config();
        let retired = receipt_bounded_retired_state_machine();
        let root_only = receipt_bounded_root_only_machine();

        assert_eq!(
            deletion_recovery_observability(&config, &retired),
            Some(format!(
                "oxideloop: entering exact retired-State deletion recovery for ns/root uid={ROOT_UID}"
            ))
        );
        assert_eq!(
            deletion_recovery_observability(&config, &root_only),
            Some(format!(
                "oxideloop: entering receipt-bounded root-only deletion recovery for ns/root uid={ROOT_UID}"
            ))
        );
        assert_eq!(
            deletion_recovery_observability(&config, automation_candidate_actor().state()),
            None
        );
    }

    #[test]
    fn root_refresh_replaces_a_same_uid_state_disappearance_with_root_only_recovery() {
        let mut actor = automation_candidate_actor();
        actor.start();
        assert!(
            refresh_actor_from_machine(&mut actor, Some(receipt_bounded_root_only_machine()),)
                .expect("same UID root refresh")
        );
        assert!(actor.state().is_root_only_deletion_recovery());
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::VerifyOwnedChildrenAbsent { preconditions, .. }))
                if preconditions.state_resource_version.is_empty()
                    && preconditions.state_uid == "deleted-state-uid"
        ));
    }

    #[test]
    fn stale_command_replaces_a_same_uid_state_disappearance_with_root_only_recovery() {
        let mut actor = automation_candidate_actor();
        actor.start();
        assert!(recover_stale_command_actor(
            &mut actor,
            &receipt_bounded_root_only_machine(),
        ));
        assert!(actor.state().is_root_only_deletion_recovery());
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::VerifyOwnedChildrenAbsent { preconditions, .. }))
                if preconditions.state_resource_version.is_empty()
                    && preconditions.state_uid == "deleted-state-uid"
        ));
    }

    fn final_automation_input(actor: &RootActorV0) -> RenderInputV0 {
        RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: "ns".into(),
                name: "root".into(),
                uid: ROOT_UID.into(),
                generation: 1,
                spec: serde_json::to_value(&actor.state().root().spec).expect("root spec"),
            },
            bundle_digest: BUNDLE_DIGEST.into(),
            cache_epoch: "0".into(),
            observation_token: "render-observation-token".into(),
            observations: vec![Observation {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "source".into(),
                uid: "source-uid".into(),
                resource_version: "7".into(),
                value: json!({"apiVersion":"v1","kind":"ConfigMap","data":{"value":"present"}}),
            }],
        }
    }

    fn final_automation_dependency() -> Dependency {
        Dependency::Object {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "source".into(),
            uid: Some("source-uid".into()),
            resource_version: Some("7".into()),
            observation_token: "renderer-dependency-token".into(),
        }
    }

    fn final_automation_descriptor() -> BundleDescriptorV0 {
        BundleDescriptorV0 {
            descriptor_version: "bundle.oxideloop.dev/v0".into(),
            renderer_abi_version: ABI_VERSION_V0.into(),
            render_module: oxideloop_bundle_contract::DescriptorModuleV0 {
                identity: "render.mjs".into(),
                digest: "c".repeat(64),
            },
            action_modules: vec![oxideloop_bundle_contract::DescriptorModuleV0 {
                identity: "actions/source.mjs".into(),
                digest: ACTION_DIGEST.into(),
            }],
        }
    }

    fn final_automation_output(dependency: Dependency) -> RenderOutputV0 {
        RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            // This helper is tested after the production fixed-point parser
            // already validated disposition/resource shape; automation
            // derivation only consumes the final dependency/output pins.
            disposition: RenderDisposition::Apply,
            resources: vec![],
            dependencies: vec![dependency.clone()],
            automations: vec![AutomationDeclaration {
                identity: "source-change".into(),
                version: "v1".into(),
                source_dependency: dependency,
                event_metadata: json!({"reason":"source transition"}),
                bundle_digest: BUNDLE_DIGEST.into(),
                action_module_identity: "actions/source.mjs".into(),
                action_module_digest: ACTION_DIGEST.into(),
                renderer_abi_version: ABI_VERSION_V0.into(),
            }],
            diagnostics: vec![],
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn wire_json(value: serde_json::Value) -> Response<Body> {
        Response::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&value).expect("wire JSON serializes"),
            ))
            .expect("wire response")
    }

    #[allow(clippy::struct_excessive_bools)]
    #[derive(Default)]
    struct BootstrapWireStateV0 {
        state_created: bool,
        state_finalizer: bool,
        root_metadata: bool,
        root_status: bool,
        root_statuses: Vec<serde_json::Value>,
        descriptor_reads: usize,
        requests: Vec<String>,
    }

    fn bootstrap_wire_root(
        token: &str,
        state_name: &str,
        state: &OxideLoopStateSpec,
        root_metadata: bool,
        root_status: bool,
    ) -> serde_json::Value {
        bootstrap_wire_root_for_bundle(
            token,
            state_name,
            state,
            root_metadata,
            root_status
                .then(|| json!({"state":{"rootUid":ROOT_UID,"name":state_name,"uid":"state-uid"}})),
            BUNDLE_DIGEST,
        )
    }

    /// Builds the exact raw domain object that the scripted API returns.  The
    /// collector fence compares the complete object, not a reduced snapshot,
    /// so tests that model an owned watch echo must use this rather than the
    /// generic `root_event` fixture.
    fn bootstrap_wire_root_object(
        token: &str,
        state_name: &str,
        state: &OxideLoopStateSpec,
        root_metadata: bool,
        root_status: bool,
    ) -> ObjectV0 {
        let value = bootstrap_wire_root(token, state_name, state, root_metadata, root_status);
        let annotations = serde_json::from_value(value["metadata"]["annotations"].clone())
            .expect("bootstrap root annotations decode");
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "root".into(),
            },
            metadata: ObjectMetaV0 {
                uid: ROOT_UID.into(),
                resource_version: value["metadata"]["resourceVersion"]
                    .as_str()
                    .expect("bootstrap root resource version")
                    .into(),
                annotations,
                owner_uids: vec![],
            },
            value,
        }
    }

    fn root_object_from_wire(value: serde_json::Value) -> ObjectV0 {
        let annotations = serde_json::from_value(value["metadata"]["annotations"].clone())
            .expect("root wire annotations decode");
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "root".into(),
            },
            metadata: ObjectMetaV0 {
                uid: value["metadata"]["uid"]
                    .as_str()
                    .expect("root wire UID")
                    .into(),
                resource_version: value["metadata"]["resourceVersion"]
                    .as_str()
                    .expect("root wire resource version")
                    .into(),
                annotations,
                owner_uids: vec![],
            },
            value,
        }
    }

    fn bootstrap_wire_root_for_bundle(
        token: &str,
        state_name: &str,
        state: &OxideLoopStateSpec,
        root_metadata: bool,
        status: Option<serde_json::Value>,
        bundle_digest: &str,
    ) -> serde_json::Value {
        let annotations = root_metadata.then(|| {
            let canonical = canonical_bytes(
                &serde_json::to_value(state).expect("bootstrap State spec serializes"),
            )
            .expect("bootstrap State spec canonicalizes");
            let mut domain_separated = b"oxideloop.bootstrap-authority/v1\0".to_vec();
            domain_separated.extend_from_slice(&canonical);
            json!({
                "runtime.oxideloop.dev/root-uid": ROOT_UID,
                "runtime.oxideloop.dev/transaction-id": token,
                "runtime.oxideloop.dev/command-id": token,
                "runtime.oxideloop.dev/bootstrap-authority-version": "v1",
                "runtime.oxideloop.dev/bootstrap-state-name": state_name,
                "runtime.oxideloop.dev/bootstrap-state-uid": "state-uid",
                "runtime.oxideloop.dev/bootstrap-state-spec-digest": sha256_hex(&domain_separated),
                "runtime.oxideloop.dev/bootstrap-proof-token": token,
            })
        });
        let mut root = json!({
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "kind":"WebApplication",
            "metadata": {
                "name":"root",
                "namespace":"ns",
                "uid":ROOT_UID,
                "resourceVersion": if status.is_some() { "3" } else if root_metadata { "2" } else { "1" },
                "generation":1,
                "annotations":annotations.unwrap_or_else(|| json!({})),
                "finalizers":if root_metadata { vec!["demo.oxideloop.dev/finalizer"] } else { vec![] },
            },
            "spec": {
                "message":"fresh bootstrap",
                "expose":false,
                "program": {"configMapName":"bundle","key":"program.mjs","digest":bundle_digest}
            },
            "status":status.unwrap_or_else(|| json!({})),
        });
        if !root_metadata {
            root["metadata"]
                .as_object_mut()
                .expect("root metadata is an object")
                .remove("finalizers");
        }
        root
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = *chunk.get(1).unwrap_or(&0);
            let third = *chunk.get(2).unwrap_or(&0);
            encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
            encoded.push(char::from(
                ALPHABET[usize::from((first & 0x03) << 4 | second >> 4)],
            ));
            encoded.push(if chunk.len() > 1 {
                char::from(ALPHABET[usize::from((second & 0x0f) << 2 | third >> 6)])
            } else {
                '='
            });
            encoded.push(if chunk.len() > 2 {
                char::from(ALPHABET[usize::from(third & 0x3f)])
            } else {
                '='
            });
        }
        encoded
    }

    fn bundle_file(path: &str, bytes: &[u8]) -> FileIntegrityV0 {
        FileIntegrityV0 {
            path: path.into(),
            length: bytes.len().try_into().expect("test file length fits u64"),
            sha256: sha256_hex(bytes),
        }
    }

    fn unseal_test_bundle_directory(directory: &std::path::Path) -> std::io::Result<()> {
        let mut permissions = std::fs::metadata(directory)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            permissions.set_mode(0o700);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        std::fs::set_permissions(directory, permissions)
    }

    fn valid_bundle_archive() -> (Vec<u8>, String) {
        let render = b"render callable".to_vec();
        let action = b"action callable".to_vec();
        let descriptor = BundleDescriptorV0 {
            descriptor_version: "oxideloop.dev/descriptor/v0".into(),
            renderer_abi_version: ABI_VERSION_V0.into(),
            render_module: DescriptorModuleV0 {
                identity: "demo.web-application.render".into(),
                digest: sha256_hex(&render),
            },
            action_modules: vec![DescriptorModuleV0 {
                identity: "demo.web-application.input-changed".into(),
                digest: sha256_hex(&action),
            }],
        };
        let descriptor = canonical_bytes(
            &serde_json::to_value(&descriptor).expect("descriptor is serializable"),
        )
        .expect("descriptor is canonical");
        let manifest = BundleIntegrityManifestV0 {
            manifest_version: BUNDLE_MANIFEST_VERSION_V0.into(),
            compiler: ToolIdentityV0 {
                identity: "test".into(),
                version: "1".into(),
            },
            bundler: ToolIdentityV0 {
                identity: "test".into(),
                version: "1".into(),
            },
            renderer_abi_version: ABI_VERSION_V0.into(),
            descriptor: bundle_file("descriptor.json", &descriptor),
            render_modules: vec![ModuleIntegrityV0 {
                identity: "demo.web-application.render".into(),
                file: bundle_file("render.callable.js", &render),
            }],
            action_modules: vec![ModuleIntegrityV0 {
                identity: "demo.web-application.input-changed".into(),
                file: bundle_file("action.callable.js", &action),
            }],
            source_maps: vec![],
            phase_modules: vec![
                PhaseModuleV0 {
                    phase: ExecutionPhaseV0::Render,
                    module_identity: "demo.web-application.render".into(),
                },
                PhaseModuleV0 {
                    phase: ExecutionPhaseV0::Action,
                    module_identity: "demo.web-application.input-changed".into(),
                },
            ],
        };
        let manifest =
            canonical_bytes(&serde_json::to_value(&manifest).expect("manifest is serializable"))
                .expect("manifest is canonical");
        let digest = sha256_hex(&manifest);
        let install = canonical_bytes(
            &serde_json::to_value(&BundleInstallRecordV0 {
                bundle_digest: digest.clone(),
                manifest: serde_json::from_slice(&manifest).expect("manifest re-parses"),
            })
            .expect("install record is serializable"),
        )
        .expect("install record is canonical");
        let entries = [
            ("action.callable.js", action),
            ("bundle-install.json", install),
            ("descriptor.json", descriptor),
            ("integrity-manifest.json", manifest),
            ("render.callable.js", render),
        ];
        let mut archive = b"OLBA\x01\x05".to_vec();
        for (path, bytes) in &entries {
            archive.push(path.len().try_into().expect("test path fits framing"));
            archive.extend_from_slice(path.as_bytes());
            archive.extend_from_slice(
                &u32::try_from(bytes.len())
                    .expect("test payload fits framing")
                    .to_be_bytes(),
            );
            archive.extend_from_slice(sha256_hex(bytes).as_bytes());
        }
        for (_, bytes) in entries {
            archive.extend_from_slice(&bytes);
        }
        (archive, digest)
    }

    fn bootstrap_wire_state(
        state_name: &str,
        state: &oxideloop_controller_api::OxideLoopStateSpec,
        token: &str,
        state_finalizer: bool,
    ) -> serde_json::Value {
        json!({
            "apiVersion":"runtime.oxideloop.dev/v1alpha1",
            "kind":"OxideLoopState",
            "metadata": {
                "name":state_name,
                "namespace":"ns",
                "uid":"state-uid",
                "resourceVersion":"state-rv-1",
                "finalizers":if state_finalizer { vec!["runtime.oxideloop.dev/state-finalizer"] } else { vec![] },
                "annotations":{
                    "runtime.oxideloop.dev/root-uid":ROOT_UID,
                    "runtime.oxideloop.dev/transaction-id":token,
                    "runtime.oxideloop.dev/command-id":token,
                },
                "ownerReferences":[{
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplication",
                    "name":"root",
                    "uid":ROOT_UID,
                    "controller":true,
                    "blockOwnerDeletion":false,
                }],
            },
            "spec":state,
            "status":{},
        })
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn owned_bootstrap_retries_one_command_through_every_real_transport_step() {
        let mut actor = automation_candidate_actor();
        actor.start();
        let command = actor
            .next_command()
            .expect("bootstrap command available")
            .expect("bootstrap command");
        let CommandV0::EnsureBootstrap {
            token,
            state,
            state_name,
            ..
        } = &command
        else {
            panic!("bootstrap actor must issue EnsureBootstrap");
        };
        let wire = Arc::new(Mutex::new(BootstrapWireStateV0::default()));
        let script = Arc::clone(&wire);
        let token = token.clone();
        let state_name = state_name.clone();
        let expected_state_name = state_name.clone();
        let state = state.clone();
        let service = service_fn(move |request: Request<Body>| {
            let script = Arc::clone(&script);
            let token = token.clone();
            let state_name = state_name.clone();
            let state_spec = state.clone();
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let mut script = script.lock().expect("bootstrap wire script lock");
                script.requests.push(format!("{method} {path}"));
                let response = if path.ends_with("/webapplications/root")
                    && method == http::Method::GET
                {
                    wire_json(bootstrap_wire_root(
                        &token,
                        &state_name,
                        &state_spec,
                        script.root_metadata,
                        script.root_status,
                    ))
                } else if path.ends_with("/oxideloopstates") && method == http::Method::POST {
                    script.state_created = true;
                    wire_json(bootstrap_wire_state(
                        &state_name,
                        &state_spec,
                        &token,
                        false,
                    ))
                } else if path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    && method == http::Method::GET
                {
                    if script.state_created {
                        wire_json(bootstrap_wire_state(
                            &state_name,
                            &state_spec,
                            &token,
                            script.state_finalizer,
                        ))
                    } else {
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
                                .expect("not-found response JSON"),
                            ))
                            .expect("state absent response")
                    }
                } else if path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    && method == http::Method::PATCH
                {
                    script.state_finalizer = true;
                    wire_json(bootstrap_wire_state(&state_name, &state_spec, &token, true))
                } else if path.ends_with("/webapplications/root/status")
                    && method == http::Method::PATCH
                {
                    script.root_status = true;
                    wire_json(bootstrap_wire_root(
                        &token,
                        &state_name,
                        &state_spec,
                        true,
                        true,
                    ))
                } else if path.ends_with("/webapplications/root") && method == http::Method::PATCH {
                    script.root_metadata = true;
                    wire_json(bootstrap_wire_root(
                        &token,
                        &state_name,
                        &state_spec,
                        true,
                        false,
                    ))
                } else {
                    panic!("unexpected bootstrap request {method} {path}");
                };
                Ok::<_, Infallible>(response)
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let address = oxideloop_kube_transport::RootAddressV0 {
            namespace: "ns".into(),
            name: "root".into(),
            state_name: expected_state_name.clone(),
        };
        let event = dispatch_owned_command_until_event(&transport, &address, &command)
            .await
            .expect("one owned command survives all sequencing replies")
            .expect("bootstrap acknowledgement");
        assert!(matches!(
            event,
            EventV0::BootstrapAcknowledged {
                state_name: ref acknowledged_state,
                state_uid: ref acknowledged_uid,
                root_finalizer_present: true,
                state_finalizer_present: true,
                ..
            } if acknowledged_state == &expected_state_name && acknowledged_uid == "state-uid"
        ));
        let script = wire.lock().expect("bootstrap wire inspection");
        assert!(script.state_created && script.state_finalizer);
        assert!(script.root_metadata && script.root_status);
        assert!(
            script
                .requests
                .iter()
                .any(|request| request.starts_with("POST "))
                && script
                    .requests
                    .iter()
                    .any(|request| request.ends_with("/status")),
            "the real transport created state, then finalized root metadata and status"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn bootstrap_root_change_before_each_subwrite_polls_no_later_write() {
        // The collected R17 fence seals its immutable root before this
        // effect can poll.  The root GET ordinals below are therefore the
        // phase-local checks themselves: State POST (0), the metadata
        // decision/reread (1), root metadata (2), State finalizer (3), and
        // Root State-link/status (4).
        for (target_root_get, expected_writes, label, mutation) in [
            (0, 0, "State POST", "spec"),
            (0, 0, "State POST", "status"),
            (0, 0, "State POST", "annotation"),
            (0, 0, "State POST", "generation"),
            (0, 0, "State POST", "deletion"),
            (0, 0, "State POST", "identity"),
            (3, 2, "State finalizer", "spec"),
            (2, 1, "root metadata", "spec"),
            (4, 3, "root State-link/status", "spec"),
            (4, 3, "root State-link/status after status change", "status"),
            (
                4,
                3,
                "root State-link/status after State-link change",
                "state-link",
            ),
            (
                4,
                3,
                "root State-link/status after finalizer change",
                "finalizer",
            ),
            (
                4,
                3,
                "root State-link/status after annotation change",
                "annotation",
            ),
        ] {
            let mut actor = automation_candidate_actor();
            actor.start();
            let command = actor
                .next_command()
                .expect("bootstrap command is available")
                .expect("bootstrap command");
            let CommandV0::EnsureBootstrap {
                token,
                state,
                state_name,
                ..
            } = &command
            else {
                panic!("fresh actor must issue EnsureBootstrap");
            };
            let token = token.clone();
            let state = state.clone();
            let state_name = state_name.clone();
            let wire = Arc::new(Mutex::new(BootstrapWireStateV0::default()));
            let (root_sender, mut root_watch) = WatchHandleV0::test_channel(4);
            let root_gets = Arc::new(AtomicUsize::new(0));
            let preflight_started = Arc::new(AtomicUsize::new(0));
            let release_preflight = Arc::new(tokio::sync::Notify::new());
            let changed_root = Arc::new(Mutex::new(None));
            let service_token = token.clone();
            let service_state = state.clone();
            let service_state_name = state_name.clone();
            let service = service_fn({
                let wire = Arc::clone(&wire);
                let root_sender = root_sender.clone();
                let root_gets = Arc::clone(&root_gets);
                let preflight_started = Arc::clone(&preflight_started);
                let release_preflight = Arc::clone(&release_preflight);
                let changed_root = Arc::clone(&changed_root);
                let token = service_token.clone();
                let state = service_state.clone();
                let state_name = service_state_name.clone();
                move |request: Request<Body>| {
                    let wire = Arc::clone(&wire);
                    let root_sender = root_sender.clone();
                    let root_gets = Arc::clone(&root_gets);
                    let preflight_started = Arc::clone(&preflight_started);
                    let release_preflight = Arc::clone(&release_preflight);
                    let changed_root = Arc::clone(&changed_root);
                    let token = token.clone();
                    let state = state.clone();
                    let state_name = state_name.clone();
                    async move {
                        let method = request.method().clone();
                        let path = request.uri().path().to_owned();
                        if method == http::Method::GET && path.ends_with("/webapplications/root") {
                            let ordinal = root_gets.fetch_add(1, Ordering::SeqCst);
                            if ordinal == target_root_get {
                                let changed = {
                                    let wire = wire.lock().expect("bootstrap boundary wire lock");
                                    let mut root = bootstrap_wire_root(
                                        &token,
                                        &state_name,
                                        &state,
                                        wire.root_metadata,
                                        wire.root_status,
                                    );
                                    let resource_version =
                                        format!("independent-bootstrap-{ordinal}");
                                    root["metadata"]["resourceVersion"] = json!(resource_version);
                                    match mutation {
                                        "spec" => {
                                            root["spec"]["message"] =
                                                json!("independent bootstrap change");
                                        }
                                        "generation" => {
                                            root["metadata"]["generation"] = json!(2);
                                        }
                                        "deletion" => {
                                            root["metadata"]["deletionTimestamp"] =
                                                json!("2026-08-31T18:58:33Z");
                                        }
                                        "identity" => {
                                            root["metadata"]["uid"] = json!("foreign-root-uid");
                                        }
                                        "status" => {
                                            root["status"]["observedGeneration"] = json!(1);
                                        }
                                        "state-link" => {
                                            root["status"]["state"] = json!({
                                                "rootUid": ROOT_UID,
                                                "name": state_name,
                                                "uid": "foreign-state-uid",
                                            });
                                        }
                                        "finalizer" => {
                                            root["metadata"]["finalizers"] =
                                                json!(["demo.oxideloop.dev/finalizer", "foreign"]);
                                        }
                                        "annotation" => {
                                            root["metadata"]["annotations"]["foreign"] =
                                                json!("changed");
                                        }
                                        _ => {
                                            unreachable!("bootstrap mutation fixture is exhaustive")
                                        }
                                    }
                                    root
                                };
                                let resource_version = changed["metadata"]["resourceVersion"]
                                    .as_str()
                                    .expect("changed root resource version")
                                    .to_owned();
                                *changed_root.lock().expect("changed bootstrap root lock") =
                                    Some(changed);
                                let mut root_event = root_event(&resource_version);
                                if mutation == "identity" {
                                    root_event.metadata.uid = "foreign-root-uid".into();
                                    root_event.value["metadata"]["uid"] = json!("foreign-root-uid");
                                }
                                root_sender
                                    .try_send(Ok(WatchEventV0::Modified(root_event)))
                                    .expect("inject genuine root change");
                                preflight_started.store(1, Ordering::SeqCst);
                                release_preflight.notified().await;
                            }
                            if let Some(changed) = changed_root
                                .lock()
                                .expect("changed bootstrap root lock")
                                .clone()
                            {
                                return Ok::<_, Infallible>(wire_json(changed));
                            }
                            let wire = wire.lock().expect("bootstrap boundary wire lock");
                            Ok(wire_json(bootstrap_wire_root(
                                &token,
                                &state_name,
                                &state,
                                wire.root_metadata,
                                wire.root_status,
                            )))
                        } else if method == http::Method::GET
                            && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                        {
                            let wire = wire.lock().expect("bootstrap boundary wire lock");
                            if wire.state_created {
                                Ok(wire_json(bootstrap_wire_state(
                                    &state_name,
                                    &state,
                                    &token,
                                    wire.state_finalizer,
                                )))
                            } else {
                                Ok(Response::builder()
                                    .status(404)
                                    .header(http::header::CONTENT_TYPE, "application/json")
                                    .body(Body::from(
                                        serde_json::to_vec(&json!({
                                            "apiVersion":"v1", "kind":"Status",
                                            "status":"Failure", "reason":"NotFound", "code":404,
                                        }))
                                        .expect("not-found JSON"),
                                    ))
                                    .expect("state is absent"))
                            }
                        } else if method == http::Method::POST && path.ends_with("/oxideloopstates")
                        {
                            let mut wire = wire.lock().expect("bootstrap boundary wire lock");
                            wire.state_created = true;
                            wire.requests.push("State POST".into());
                            Ok(wire_json(bootstrap_wire_state(
                                &state_name,
                                &state,
                                &token,
                                false,
                            )))
                        } else if method == http::Method::PATCH
                            && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                        {
                            let mut wire = wire.lock().expect("bootstrap boundary wire lock");
                            wire.state_finalizer = true;
                            wire.requests.push("State finalizer".into());
                            Ok(wire_json(bootstrap_wire_state(
                                &state_name,
                                &state,
                                &token,
                                true,
                            )))
                        } else if method == http::Method::PATCH
                            && path.ends_with("/webapplications/root/status")
                        {
                            let mut wire = wire.lock().expect("bootstrap boundary wire lock");
                            wire.root_status = true;
                            wire.requests.push("Root State-link/status".into());
                            Ok(wire_json(bootstrap_wire_root(
                                &token,
                                &state_name,
                                &state,
                                true,
                                true,
                            )))
                        } else if method == http::Method::PATCH
                            && path.ends_with("/webapplications/root")
                        {
                            let mut wire = wire.lock().expect("bootstrap boundary wire lock");
                            wire.root_metadata = true;
                            wire.requests.push("Root metadata".into());
                            Ok(wire_json(bootstrap_wire_root(
                                &token,
                                &state_name,
                                &state,
                                true,
                                false,
                            )))
                        } else {
                            panic!("unexpected bootstrap-boundary request {method} {path}");
                        }
                    }
                }
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let address = root_address(&actor).expect("root address");
            let operation = transport.begin_collected_operation_session();
            operation
                .seal_collected_bootstrap_fence(
                    &address,
                    &command,
                    &bootstrap_wire_root_object(&token, &state_name, &state, false, false),
                    None,
                )
                .expect("bootstrap collector session seals before select");
            let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
            let mut automation_watch = None;
            let collect = collect_kubernetes_command_while_observing(
                &transport,
                &address,
                &command,
                &mut root_watch,
                &mut fixed_point,
                &mut automation_watch,
                "root",
                operation.clone(),
                None,
            );
            let release = async {
                while preflight_started.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
                wait_for_space(&root_sender).await;
                release_preflight.notify_one();
            };
            let (result, ()) = tokio::join!(collect, release);
            assert!(matches!(
                result.expect("bootstrap boundary collector result").0,
                KubernetesEffectCompletionV0::PreWriteInvalidated { .. }
            ));
            let wire = wire.lock().expect("bootstrap boundary wire inspection");
            assert_eq!(
                wire.requests.len(),
                expected_writes,
                "a genuine root change before {label} must poll zero writes at that boundary: {:?}",
                wire.requests
            );
            assert_eq!(
                operation.prewrite_gate().state(),
                PrewriteGateStateV0::Invalidated
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn fresh_bootstrap_cut_change_rebarriers_without_writes_or_a_fatal_exit() {
        let mut actor = automation_candidate_actor();
        actor.start();
        let command = actor
            .next_command()
            .expect("bootstrap command is available")
            .expect("fresh actor issues bootstrap");
        let CommandV0::EnsureBootstrap {
            token,
            state,
            state_name,
            ..
        } = &command
        else {
            panic!("fresh actor must issue EnsureBootstrap");
        };
        let listed = bootstrap_wire_root(token, state_name, state, false, false);
        let mut changed = listed.clone();
        changed["metadata"]["resourceVersion"] = json!("fresh-cut-rv-2");
        changed["spec"]["message"] = json!("changed during fresh cut");
        let lists = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let service = service_fn({
            let lists = Arc::clone(&lists);
            let writes = Arc::clone(&writes);
            let listed = listed.clone();
            let changed = changed.clone();
            move |request: Request<Body>| {
                let lists = Arc::clone(&lists);
                let writes = Arc::clone(&writes);
                let listed = listed.clone();
                let changed = changed.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    if method == http::Method::GET && path.ends_with("/webapplications") {
                        let item = if lists.fetch_add(1, Ordering::SeqCst) == 0 {
                            listed.clone()
                        } else {
                            changed.clone()
                        };
                        return Ok::<_, Infallible>(wire_json(json!({
                            "apiVersion":"demo.oxideloop.dev/v1alpha1",
                            "kind":"WebApplicationList",
                            "metadata":{"resourceVersion":"fresh-cut-list-rv"},
                            "items":[item],
                        })));
                    }
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        return Ok::<_, Infallible>(wire_json(changed.clone()));
                    }
                    if method == http::Method::GET && path.contains("/oxideloopstates/") {
                        return Ok::<_, Infallible>(Response::builder()
                            .status(http::StatusCode::NOT_FOUND)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                br#"{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404}"#.to_vec()
                            ))
                            .expect("state absence response"));
                    }
                    writes.fetch_add(1, Ordering::SeqCst);
                    panic!("fresh-cut change must not issue {method} {path}");
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let (_fresh_sender, fresh_watch) = WatchHandleV0::test_channel(1);
        let (_barrier_sender, barrier_watch) = WatchHandleV0::test_channel(1);
        transport.test_push_watch(fresh_watch);
        transport.test_push_watch(barrier_watch);
        let (_retained_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, UnixRendererPortV0>> = None;
        let mut automation_watch = None;

        let (completion, _) = execute_kubernetes_command_while_observing(
            &transport,
            &root_address(&actor).expect("root address"),
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            None,
        )
        .await
        .expect("changed fresh cut is a normal no-write completion");
        assert!(matches!(
            completion,
            KubernetesEffectCompletionV0::FreshBootstrapCutInvalidated
        ));
        let mut automation_candidates = VecDeque::new();
        assert_eq!(
            rebarrier_after_fresh_bootstrap_cut(
                &mut actor,
                &command,
                &transport,
                &drain_root_config(),
                &mut root_watch,
                &mut fixed_point,
                &mut automation_watch,
                &mut automation_candidates,
            )
            .await
            .expect("changed cut returns to the normal root barrier"),
            FreshBootstrapCutRebarrierV0::Active
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(matches!(actor.mode(), ModeV0::Running));
        let next = actor.next_command();
        assert!(next.is_ok(), "rebased actor must not fail: {next:?}");
        assert_eq!(
            actor.state().root().root_resource_version,
            "fresh-cut-rv-2",
            "the ordinary barrier reduced the fresh same-UID root before another command"
        );
    }

    #[tokio::test]
    async fn fresh_bootstrap_cut_absence_rebarriers_to_wait_without_writes_or_hold() {
        let mut actor = automation_candidate_actor();
        actor.start();
        let command = actor
            .next_command()
            .expect("bootstrap command is available")
            .expect("fresh actor issues bootstrap");
        let listed = match &command {
            CommandV0::EnsureBootstrap {
                token,
                state,
                state_name,
                ..
            } => bootstrap_wire_root(token, state_name, state, false, false),
            _ => panic!("fresh actor must issue EnsureBootstrap"),
        };
        let lists = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let service = service_fn({
            let lists = Arc::clone(&lists);
            let writes = Arc::clone(&writes);
            let listed = listed.clone();
            move |request: Request<Body>| {
                let lists = Arc::clone(&lists);
                let writes = Arc::clone(&writes);
                let listed = listed.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    if method == http::Method::GET && path.ends_with("/webapplications") {
                        let initial_cut = lists.fetch_add(1, Ordering::SeqCst) == 0;
                        return Ok::<_, Infallible>(wire_json(json!({
                            "apiVersion":"demo.oxideloop.dev/v1alpha1",
                            "kind":"WebApplicationList",
                            "metadata":{"resourceVersion":"fresh-cut-empty-rv"},
                            "items": if initial_cut { vec![listed] } else { vec![] },
                        })));
                    }
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        return Ok::<_, Infallible>(Response::builder()
                            .status(http::StatusCode::NOT_FOUND)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                br#"{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404}"#.to_vec()
                            ))
                            .expect("root absence response"));
                    }
                    writes.fetch_add(1, Ordering::SeqCst);
                    panic!("fresh-cut absence must not issue {method} {path}");
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let (_fresh_sender, fresh_watch) = WatchHandleV0::test_channel(1);
        let (_barrier_sender, barrier_watch) = WatchHandleV0::test_channel(1);
        transport.test_push_watch(fresh_watch);
        transport.test_push_watch(barrier_watch);
        let (_retained_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, UnixRendererPortV0>> = None;
        let mut automation_watch = None;

        let (completion, _) = execute_kubernetes_command_while_observing(
            &transport,
            &root_address(&actor).expect("root address"),
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            None,
        )
        .await
        .expect("absent fresh cut is a normal no-write completion");
        assert!(matches!(
            completion,
            KubernetesEffectCompletionV0::FreshBootstrapCutInvalidated
        ));
        let mut automation_candidates = VecDeque::new();
        assert_eq!(
            rebarrier_after_fresh_bootstrap_cut(
                &mut actor,
                &command,
                &transport,
                &drain_root_config(),
                &mut root_watch,
                &mut fixed_point,
                &mut automation_watch,
                &mut automation_candidates,
            )
            .await
            .expect("absent cut becomes the normal wait-for-root state"),
            FreshBootstrapCutRebarrierV0::ActorRetired
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(lists.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn malformed_fresh_bootstrap_cut_lists_hold_without_rebarriering_or_writes() {
        for label in [
            "exact-name selector leak",
            "empty list resourceVersion",
            "duplicate exact-name list items",
        ] {
            let mut actor = automation_candidate_actor();
            actor.start();
            let command = actor
                .next_command()
                .expect("bootstrap command is available")
                .expect("fresh actor issues bootstrap");
            let listed = match &command {
                CommandV0::EnsureBootstrap {
                    token,
                    state,
                    state_name,
                    ..
                } => bootstrap_wire_root(token, state_name, state, false, false),
                _ => panic!("fresh actor must issue EnsureBootstrap"),
            };
            let list = match label {
                "exact-name selector leak" => {
                    let mut leaked = listed.clone();
                    leaked["metadata"]["name"] = json!("foreign");
                    json!({
                        "apiVersion":"demo.oxideloop.dev/v1alpha1",
                        "kind":"WebApplicationList",
                        "metadata":{"resourceVersion":"fresh-cut-list-rv"},
                        "items":[leaked],
                    })
                }
                "empty list resourceVersion" => json!({
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplicationList",
                    "metadata":{"resourceVersion":""},
                    "items":[listed],
                }),
                "duplicate exact-name list items" => json!({
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplicationList",
                    "metadata":{"resourceVersion":"fresh-cut-list-rv"},
                    "items":[listed.clone(), listed],
                }),
                _ => unreachable!("fixed malformed fresh-cut case"),
            };
            let writes = Arc::new(AtomicUsize::new(0));
            let service = service_fn({
                let writes = Arc::clone(&writes);
                move |request: Request<Body>| {
                    let writes = Arc::clone(&writes);
                    let list = list.clone();
                    async move {
                        let method = request.method().clone();
                        let path = request.uri().path().to_owned();
                        if method == http::Method::GET && path.ends_with("/webapplications") {
                            return Ok::<_, Infallible>(wire_json(list));
                        }
                        writes.fetch_add(1, Ordering::SeqCst);
                        panic!("malformed fresh bootstrap {label} must not issue {method} {path}");
                    }
                }
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let (_root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
            let mut fixed_point: Option<KubeFixedPointPortV0<'_, UnixRendererPortV0>> = None;
            let mut automation_watch = None;

            assert!(
                matches!(
                    execute_kubernetes_command_while_observing(
                        &transport,
                        &root_address(&actor).expect("root address"),
                        &command,
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        "root",
                        None,
                    )
                    .await,
                    Err(RuntimeErrorV0::Hold(_))
                ),
                "{label} must fail closed instead of rebarriering"
            );
            assert_eq!(
                writes.load(Ordering::SeqCst),
                0,
                "{label} must issue zero writes"
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn bootstrap_ready_exact_root_added_before_first_effect_poll_reuses_its_epoch() {
        let mut actor = automation_candidate_actor();
        actor.start();
        let command = actor
            .next_command()
            .expect("bootstrap command is available")
            .expect("bootstrap command");
        let CommandV0::EnsureBootstrap {
            token,
            state,
            state_name,
            ..
        } = &command
        else {
            panic!("fresh actor must issue EnsureBootstrap");
        };
        let token = token.clone();
        let state = state.clone();
        let state_name = state_name.clone();
        let wire = Arc::new(Mutex::new(BootstrapWireStateV0::default()));
        let root_gets = Arc::new(AtomicUsize::new(0));
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(4);
        let service = service_fn({
            let wire = Arc::clone(&wire);
            let root_gets = Arc::clone(&root_gets);
            let root_sender = root_sender.clone();
            let token = token.clone();
            let state = state.clone();
            let state_name = state_name.clone();
            move |request: Request<Body>| {
                let wire = Arc::clone(&wire);
                let root_gets = Arc::clone(&root_gets);
                let root_sender = root_sender.clone();
                let token = token.clone();
                let state = state.clone();
                let state_name = state_name.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    let root_response = |root_metadata, root_status| {
                        let mut root = bootstrap_wire_root(
                            &token,
                            &state_name,
                            &state,
                            root_metadata,
                            root_status,
                        );
                        root["spec"]["message"] = json!("driver automation");
                        wire_json(root)
                    };
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        root_gets.fetch_add(1, Ordering::SeqCst);
                        let wire = wire.lock().expect("ready Added wire lock");
                        Ok::<_, Infallible>(root_response(wire.root_metadata, wire.root_status))
                    } else if method == http::Method::GET
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        let wire = wire.lock().expect("ready Added wire lock");
                        if wire.state_created {
                            Ok(wire_json(bootstrap_wire_state(
                                &state_name,
                                &state,
                                &token,
                                wire.state_finalizer,
                            )))
                        } else {
                            Ok(Response::builder()
                                .status(404)
                                .header(http::header::CONTENT_TYPE, "application/json")
                                .body(Body::from(
                                    serde_json::to_vec(&json!({
                                        "apiVersion":"v1", "kind":"Status",
                                        "status":"Failure", "reason":"NotFound", "code":404,
                                    }))
                                    .expect("not-found JSON"),
                                ))
                                .expect("state is absent"))
                        }
                    } else if method == http::Method::POST && path.ends_with("/oxideloopstates") {
                        let mut wire = wire.lock().expect("ready Added wire lock");
                        wire.state_created = true;
                        wire.requests.push("State POST".into());
                        Ok(wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            false,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        let mut wire = wire.lock().expect("ready Added wire lock");
                        wire.state_finalizer = true;
                        wire.requests.push("State finalizer".into());
                        Ok(wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            true,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root/status")
                    {
                        let mut wire = wire.lock().expect("ready Added wire lock");
                        wire.root_status = true;
                        wire.requests.push("Root State-link/status".into());
                        let mut event =
                            bootstrap_wire_root_object(&token, &state_name, &state, true, true);
                        event.value["spec"]["message"] = json!("driver automation");
                        root_sender
                            .try_send(Ok(WatchEventV0::Modified(event)))
                            .expect("final root echo fits root watch");
                        Ok(root_response(true, true))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root")
                    {
                        let mut wire = wire.lock().expect("ready Added wire lock");
                        wire.root_metadata = true;
                        wire.requests.push("Root metadata".into());
                        Ok(root_response(true, false))
                    } else {
                        panic!("unexpected ready-Added request {method} {path}");
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let address = root_address(&actor).expect("root address");
        let operation = transport.begin_collected_operation_session();
        let mut exact_initial =
            bootstrap_wire_root_object(&token, &state_name, &state, false, false);
        exact_initial.value["spec"]["message"] = json!("driver automation");
        operation
            .seal_collected_bootstrap_fence(&address, &command, &exact_initial, None)
            .expect("bootstrap session seals before the ready root event");
        root_sender
            .try_send(Ok(WatchEventV0::Added(exact_initial)))
            .expect("exact initial root Added is ready before collector select");
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let (completion, buffered) = collect_kubernetes_command_while_observing(
            &transport,
            &address,
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            operation.clone(),
            None,
        )
        .await
        .expect("exact ready root Added resumes bootstrap");
        assert!(matches!(
            completion,
            KubernetesEffectCompletionV0::Acknowledged(event)
                if matches!(*event, Some(EventV0::BootstrapAcknowledged { .. }))
        ));
        assert!(
            buffered.root.is_none(),
            "the exact initial record is consumed"
        );
        assert_eq!(
            wire.lock().expect("ready Added wire inspection").requests,
            vec![
                "State POST",
                "Root metadata",
                "State finalizer",
                "Root State-link/status",
            ]
        );
        assert_eq!(
            operation.prewrite_gate().state(),
            PrewriteGateStateV0::Proven,
            "only the exact final raw echo may complete the collected bootstrap"
        );
        assert!(root_gets.load(Ordering::SeqCst) >= 6);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn bootstrap_duplicate_root_metadata_echo_restarts_once_then_writes_one_status() {
        let mut actor = automation_candidate_actor();
        actor.start();
        let command = actor
            .next_command()
            .expect("bootstrap command is available")
            .expect("bootstrap command");
        let CommandV0::EnsureBootstrap {
            token,
            state,
            state_name,
            ..
        } = &command
        else {
            panic!("fresh actor must issue EnsureBootstrap");
        };
        let token = token.clone();
        let state = state.clone();
        let state_name = state_name.clone();
        let wire = Arc::new(Mutex::new(BootstrapWireStateV0::default()));
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let root_gets = Arc::new(AtomicUsize::new(0));
        let service_token = token.clone();
        let service_state = state.clone();
        let service_state_name = state_name.clone();
        let service = service_fn({
            let wire = Arc::clone(&wire);
            let root_sender = root_sender.clone();
            let root_gets = Arc::clone(&root_gets);
            let token = service_token.clone();
            let state = service_state.clone();
            let state_name = service_state_name.clone();
            move |request: Request<Body>| {
                let wire = Arc::clone(&wire);
                let root_sender = root_sender.clone();
                let root_gets = Arc::clone(&root_gets);
                let token = token.clone();
                let state = state.clone();
                let state_name = state_name.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        root_gets.fetch_add(1, Ordering::SeqCst);
                        let wire = wire.lock().expect("bootstrap echo wire lock");
                        Ok::<_, Infallible>(wire_json(bootstrap_wire_root(
                            &token,
                            &state_name,
                            &state,
                            wire.root_metadata,
                            wire.root_status,
                        )))
                    } else if method == http::Method::GET
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        let wire = wire.lock().expect("bootstrap echo wire lock");
                        if wire.state_created {
                            Ok(wire_json(bootstrap_wire_state(
                                &state_name,
                                &state,
                                &token,
                                wire.state_finalizer,
                            )))
                        } else {
                            Ok(Response::builder()
                                .status(404)
                                .header(http::header::CONTENT_TYPE, "application/json")
                                .body(Body::from(
                                    serde_json::to_vec(&json!({
                                        "apiVersion":"v1", "kind":"Status",
                                        "status":"Failure", "reason":"NotFound", "code":404,
                                    }))
                                    .expect("not-found JSON"),
                                ))
                                .expect("state is absent"))
                        }
                    } else if method == http::Method::POST && path.ends_with("/oxideloopstates") {
                        let mut wire = wire.lock().expect("bootstrap echo wire lock");
                        wire.state_created = true;
                        wire.requests.push("State POST".into());
                        Ok(wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            false,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        let mut wire = wire.lock().expect("bootstrap echo wire lock");
                        wire.state_finalizer = true;
                        wire.requests.push("State finalizer".into());
                        Ok(wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            true,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root/status")
                    {
                        let mut wire = wire.lock().expect("bootstrap echo wire lock");
                        wire.root_status = true;
                        wire.requests.push("Root State-link/status".into());
                        root_sender
                            .try_send(Ok(WatchEventV0::Modified(bootstrap_wire_root_object(
                                &token,
                                &state_name,
                                &state,
                                true,
                                true,
                            ))))
                            .expect("final root echo fits root watch");
                        Ok(wire_json(bootstrap_wire_root(
                            &token,
                            &state_name,
                            &state,
                            true,
                            true,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root")
                    {
                        let mut wire = wire.lock().expect("bootstrap echo wire lock");
                        wire.root_metadata = true;
                        wire.requests.push("Root metadata".into());
                        Ok(wire_json(bootstrap_wire_root(
                            &token,
                            &state_name,
                            &state,
                            true,
                            false,
                        )))
                    } else {
                        panic!("unexpected bootstrap-echo request {method} {path}");
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let address = root_address(&actor).expect("root address");
        let operation = transport.begin_collected_operation_session();
        operation
            .seal_collected_bootstrap_fence(
                &address,
                &command,
                &bootstrap_wire_root_object(&token, &state_name, &state, false, false),
                None,
            )
            .expect("bootstrap collector session seals before select");
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let collect = collect_kubernetes_command_while_observing(
            &transport,
            &address,
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            operation.clone(),
            None,
        );
        let release = async {};
        let (result, ()) = tokio::join!(collect, release);
        let (KubernetesEffectCompletionV0::Acknowledged(event), buffered) =
            result.expect("exact bootstrap echo restarts")
        else {
            panic!("owned bootstrap echo must not invalidate the operation");
        };
        assert!(matches!(
            *event,
            Some(EventV0::BootstrapAcknowledged { .. })
        ));
        assert!(
            buffered.root.is_none(),
            "both identical metadata records are consumed"
        );
        let wire = wire.lock().expect("bootstrap echo wire inspection");
        assert_eq!(
            wire.requests,
            vec![
                "State POST",
                "Root metadata",
                "State finalizer",
                "Root State-link/status",
            ],
            "a retained owned echo never duplicates a bootstrap subwrite"
        );
        assert!(
            root_gets.load(Ordering::SeqCst) >= 6,
            "bootstrap retains exact root preflights and final proof rereads"
        );
        assert_eq!(
            operation.prewrite_gate().state(),
            PrewriteGateStateV0::Proven,
            "the final raw root echo is consumed before the acknowledgement"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn bootstrap_nonfinal_response_proof_authenticates_buffered_root_before_later_write() {
        let mut actor = automation_candidate_actor();
        actor.start();
        let command = actor
            .next_command()
            .expect("bootstrap command is available")
            .expect("bootstrap command");
        let CommandV0::EnsureBootstrap {
            token,
            state,
            state_name,
            ..
        } = &command
        else {
            panic!("fresh actor must issue EnsureBootstrap");
        };
        let token = token.clone();
        let state = state.clone();
        let state_name = state_name.clone();
        let wire = Arc::new(Mutex::new(BootstrapWireStateV0::default()));
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let root_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recovery_state_get_started = Arc::new(AtomicUsize::new(0));
        let recovery_state_get_released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release_recovery_state_get = Arc::new(tokio::sync::Notify::new());
        let service_token = token.clone();
        let service_state = state.clone();
        let service_state_name = state_name.clone();
        let service = service_fn({
            let wire = Arc::clone(&wire);
            let root_sender = root_sender.clone();
            let root_changed = Arc::clone(&root_changed);
            let recovery_state_get_started = Arc::clone(&recovery_state_get_started);
            let recovery_state_get_released = Arc::clone(&recovery_state_get_released);
            let release_recovery_state_get = Arc::clone(&release_recovery_state_get);
            let token = service_token.clone();
            let state = service_state.clone();
            let state_name = service_state_name.clone();
            move |request: Request<Body>| {
                let wire = Arc::clone(&wire);
                let root_sender = root_sender.clone();
                let root_changed = Arc::clone(&root_changed);
                let recovery_state_get_started = Arc::clone(&recovery_state_get_started);
                let recovery_state_get_released = Arc::clone(&recovery_state_get_released);
                let release_recovery_state_get = Arc::clone(&release_recovery_state_get);
                let token = token.clone();
                let state = state.clone();
                let state_name = state_name.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        let wire = wire.lock().expect("response-proof wire lock");
                        let mut root = bootstrap_wire_root(
                            &token,
                            &state_name,
                            &state,
                            wire.root_metadata,
                            wire.root_status,
                        );
                        if root_changed.load(Ordering::SeqCst) {
                            root["metadata"]["resourceVersion"] = json!("independent-root-rv");
                            root["spec"]["message"] = json!("independent buffered root change");
                        }
                        Ok::<_, std::io::Error>(wire_json(root))
                    } else if method == http::Method::GET
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        let state_finalizer = {
                            let wire = wire.lock().expect("response-proof wire lock");
                            if !wire.state_created {
                                return Ok(Response::builder()
                                    .status(404)
                                    .header(http::header::CONTENT_TYPE, "application/json")
                                    .body(Body::from(
                                        serde_json::to_vec(&json!({
                                            "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                            "reason":"NotFound", "code":404,
                                        }))
                                        .expect("state absence JSON"),
                                    ))
                                    .expect("state absence response"));
                            }
                            wire.state_finalizer
                        };
                        if state_finalizer
                            && !recovery_state_get_released.swap(true, Ordering::SeqCst)
                        {
                            recovery_state_get_started.store(1, Ordering::SeqCst);
                            release_recovery_state_get.notified().await;
                        }
                        Ok(wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            state_finalizer,
                        )))
                    } else if method == http::Method::POST && path.ends_with("/oxideloopstates") {
                        let mut wire = wire.lock().expect("response-proof wire lock");
                        wire.state_created = true;
                        wire.requests.push("State POST".into());
                        Ok(wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            false,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        let mut wire = wire.lock().expect("response-proof wire lock");
                        wire.state_finalizer = true;
                        wire.requests.push("State finalizer".into());
                        root_changed.store(true, Ordering::SeqCst);
                        root_sender
                            .try_send(Ok(WatchEventV0::Modified(root_event(
                                "independent-root-rv",
                            ))))
                            .expect("genuine root record fits root watch");
                        Ok(wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            true,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root")
                    {
                        let mut wire = wire.lock().expect("response-proof wire lock");
                        wire.root_metadata = true;
                        wire.requests.push("Root metadata".into());
                        Ok(wire_json(bootstrap_wire_root(
                            &token,
                            &state_name,
                            &state,
                            true,
                            false,
                        )))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root/status")
                    {
                        panic!("a buffered genuine root must stop later bootstrap writes")
                    } else {
                        panic!("unexpected response-proof request {method} {path}");
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let address = root_address(&actor).expect("root address");
        let operation = transport.begin_collected_operation_session();
        operation
            .seal_collected_bootstrap_fence(
                &address,
                &command,
                &bootstrap_wire_root_object(&token, &state_name, &state, false, false),
                None,
            )
            .expect("bootstrap collector session seals before select");
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let collect = collect_kubernetes_command_while_observing(
            &transport,
            &address,
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            operation.clone(),
            None,
        );
        let release = async {};
        let (result, ()) = tokio::join!(collect, release);
        assert!(matches!(
            result.expect("response-proof collector result").0,
            KubernetesEffectCompletionV0::PreWriteInvalidated { .. }
        ));
        let wire = wire.lock().expect("response-proof wire inspection");
        assert_eq!(
            wire.requests,
            vec!["State POST", "Root metadata", "State finalizer"],
            "the buffered genuine root invalidates the phase opened by recovery before any later write"
        );
        assert_eq!(
            operation.prewrite_gate().state(),
            PrewriteGateStateV0::Invalidated
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn driver_run_promotes_root_status_sender_before_renderer_failure() {
        // Use the reducer's exact bootstrap output as the scripted API's
        // state response. This keeps the composition test focused on the
        // driver's list/watch/run sequencing rather than duplicating the
        // durable journal constructor.
        let (archive, bundle_digest) = valid_bundle_archive();
        let archive = base64_encode(&archive);
        let installed_digest = bundle_digest.clone();
        let mut expected = automation_candidate_actor_for_digest(&bundle_digest);
        expected.start();
        let command = expected
            .next_command()
            .expect("bootstrap command lookup")
            .expect("bootstrap command");
        let CommandV0::EnsureBootstrap {
            token,
            state_name,
            state,
            ..
        } = command
        else {
            panic!("fresh actor must issue bootstrap");
        };
        let root_present = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wire = Arc::new(Mutex::new(BootstrapWireStateV0::default()));
        let (root_sender, root_watch) = WatchHandleV0::test_channel(8);
        // The initial root barrier and the per-EnsureBootstrap exact cut are
        // distinct watches.  Keep their scripted streams separate so this
        // end-to-end test exercises the operation-owned fresh cut instead of
        // accidentally reusing the retained barrier channel.
        let (bootstrap_sender, bootstrap_watch) = WatchHandleV0::test_channel(8);
        let service_wire = Arc::clone(&wire);
        let service_root_present = Arc::clone(&root_present);
        let service_bootstrap_sender = bootstrap_sender.clone();
        let service = service_fn(move |request: Request<Body>| {
            let wire = Arc::clone(&service_wire);
            let root_present = Arc::clone(&service_root_present);
            let bootstrap_sender = service_bootstrap_sender.clone();
            let token = token.clone();
            let state_name = state_name.clone();
            let state = state.clone();
            let bundle_digest = bundle_digest.clone();
            let archive = archive.clone();
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                let status_body = if path.ends_with("/webapplications/root/status")
                    && method == http::Method::PATCH
                {
                    Some(
                        request
                            .into_body()
                            .collect()
                            .await
                            .expect("status patch body is readable")
                            .to_bytes(),
                    )
                } else {
                    None
                };
                let mut wire = wire.lock().expect("composed driver wire lock");
                wire.requests.push(format!("{method} {path}"));
                let response = if path.ends_with("/webapplications") && method == http::Method::GET
                {
                    if root_present.load(Ordering::SeqCst) {
                        let mut root = bootstrap_wire_root_for_bundle(
                            &token,
                            &state_name,
                            &state,
                            wire.root_metadata,
                            wire.root_statuses.last().cloned(),
                            &bundle_digest,
                        );
                        if !wire.root_statuses.is_empty() {
                            root["metadata"]["resourceVersion"] =
                                json!((2 + wire.root_statuses.len()).to_string());
                        }
                        let resource_version = root["metadata"]["resourceVersion"]
                            .as_str()
                            .expect("fresh exact list has a resourceVersion")
                            .to_owned();
                        wire_json(json!({
                            "apiVersion":"demo.oxideloop.dev/v1alpha1",
                            "kind":"WebApplicationList",
                            "metadata":{"resourceVersion":resource_version},
                            "items":[root],
                        }))
                    } else {
                        wire_json(json!({
                            "apiVersion":"demo.oxideloop.dev/v1alpha1",
                            "kind":"WebApplicationList",
                            "metadata":{"resourceVersion":"root-list-rv"},
                            "items":[],
                        }))
                    }
                } else if path.ends_with("/webapplications/root") && method == http::Method::GET {
                    if root_present.load(Ordering::SeqCst) {
                        let mut root = bootstrap_wire_root_for_bundle(
                            &token,
                            &state_name,
                            &state,
                            wire.root_metadata,
                            wire.root_statuses.last().cloned(),
                            &bundle_digest,
                        );
                        if !wire.root_statuses.is_empty() {
                            root["metadata"]["resourceVersion"] =
                                json!((2 + wire.root_statuses.len()).to_string());
                        }
                        wire_json(root)
                    } else {
                        Response::builder()
                            .status(404)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                    "reason":"NotFound", "code":404,
                                }))
                                .expect("root absence JSON"),
                            ))
                            .expect("root absence response")
                    }
                } else if path.ends_with("/oxideloopstates") && method == http::Method::POST {
                    wire.state_created = true;
                    wire_json(bootstrap_wire_state(&state_name, &state, &token, false))
                } else if path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    && method == http::Method::GET
                {
                    if wire.state_created {
                        wire_json(bootstrap_wire_state(
                            &state_name,
                            &state,
                            &token,
                            wire.state_finalizer,
                        ))
                    } else {
                        Response::builder()
                            .status(404)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                    "reason":"NotFound", "code":404,
                                }))
                                .expect("state absence JSON"),
                            ))
                            .expect("state absence response")
                    }
                } else if path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    && method == http::Method::PATCH
                {
                    wire.state_finalizer = true;
                    wire_json(bootstrap_wire_state(&state_name, &state, &token, true))
                } else if path.ends_with("/webapplications/root/status")
                    && method == http::Method::PATCH
                {
                    let status_patch: serde_json::Value =
                        serde_json::from_slice(&status_body.expect("status body was selected"))
                            .expect("status patch is JSON");
                    let status = status_patch
                        .get("status")
                        .cloned()
                        .expect("status patch carries the complete status");
                    wire.root_status = true;
                    wire.root_statuses.push(status);
                    let resource_version = (2 + wire.root_statuses.len()).to_string();
                    let mut root = bootstrap_wire_root_for_bundle(
                        &token,
                        &state_name,
                        &state,
                        true,
                        wire.root_statuses.last().cloned(),
                        &bundle_digest,
                    );
                    root["metadata"]["resourceVersion"] = json!(resource_version.clone());
                    bootstrap_sender
                        .try_send(Ok(WatchEventV0::Modified(root_object_from_wire(
                            root.clone(),
                        ))))
                        .expect("fresh bootstrap watch accepts the final root echo");
                    wire_json(root)
                } else if path.ends_with("/webapplications/root") && method == http::Method::PATCH {
                    wire.root_metadata = true;
                    wire_json(bootstrap_wire_root_for_bundle(
                        &token,
                        &state_name,
                        &state,
                        true,
                        None,
                        &bundle_digest,
                    ))
                } else if path.ends_with("/configmaps/bundle") && method == http::Method::GET {
                    wire.descriptor_reads += 1;
                    wire_json(json!({
                        "apiVersion":"v1",
                        "kind":"ConfigMap",
                        "metadata": {
                            "name":"bundle",
                            "namespace":"ns",
                            "uid":"bundle-uid",
                            "resourceVersion":"bundle-rv-1"
                        },
                        "binaryData":{"program.mjs":archive}
                    }))
                } else {
                    panic!("unexpected composed-driver request {method} {path}");
                };
                Ok::<_, Infallible>(response)
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        transport.test_push_watch(root_watch);
        transport.test_push_watch(bootstrap_watch);
        let store =
            std::env::temp_dir().join(format!("oxideloop-driver-run-smoke-{}", std::process::id()));
        let driver = ControllerDriverV0::new(
            DriverConfigV0 {
                namespace: "ns".into(),
                root_name: "root".into(),
                bundle_store: store.clone(),
                renderer_unix_socket: store.join("renderer.sock"),
                #[cfg(feature = "test-failpoints")]
                failpoint_unix_socket: None,
            },
            transport,
        )
        .expect("driver construction");
        let run = tokio::spawn(driver.run());
        for _ in 0..100 {
            if wire
                .lock()
                .expect("composed driver wire inspection")
                .requests
                .iter()
                .any(|request| request == "GET /apis/demo.oxideloop.dev/v1alpha1/namespaces/ns/webapplications/root")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !run.is_finished(),
            "an absent root must wait on its exact watch, not exit successfully"
        );
        root_present.store(true, Ordering::SeqCst);
        root_sender
            .send(Ok(WatchEventV0::Added(root_event("1"))))
            .await
            .expect("named root creation event");
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("driver reaches controlled renderer failure")
            .expect("driver task joins");
        let request_log = wire
            .lock()
            .expect("composed driver wire inspection")
            .requests
            .clone();
        assert!(
            matches!(result, Err(RuntimeErrorV0::Renderer)),
            "the promoted root-status sender must allow the next render to reach its controlled renderer failure: {result:?}; {request_log:?}"
        );
        let wire = wire.lock().expect("composed driver wire inspection");
        assert_eq!(
            wire.requests
                .iter()
                .filter(
                    |request| request.starts_with("POST ") && request.contains("oxideloopstates")
                )
                .count(),
            1,
            "fresh root creates one state object"
        );
        assert_eq!(
            wire.requests
                .iter()
                .filter(
                    |request| request.starts_with("PATCH ") && request.contains("oxideloopstates")
                )
                .count(),
            1,
            "fresh root adds the state finalizer once"
        );
        assert_eq!(
            wire.requests
                .iter()
                .filter(|request| request == &"PATCH /apis/demo.oxideloop.dev/v1alpha1/namespaces/ns/webapplications/root")
                .count(),
            1,
            "fresh root writes metadata/finalizer once"
        );
        assert_eq!(
            wire.requests
                .iter()
                .filter(|request| request == &"PATCH /apis/demo.oxideloop.dev/v1alpha1/namespaces/ns/webapplications/root/status")
            .count(),
            2,
            "bootstrap and the promoted root-status render each write status exactly once"
        );
        assert_eq!(
            wire.root_statuses.len(),
            2,
            "both root-status writes must be captured"
        );
        assert_eq!(
            wire.root_statuses[1]["verifiedBundleDigest"],
            json!(installed_digest),
            "the promoted root-status write carries the installed bundle digest"
        );
        assert_eq!(
            wire.descriptor_reads, 1,
            "the verified descriptor is read once before the renderer is contacted"
        );
        unseal_test_bundle_directory(&store.join(installed_digest))
            .expect("unseal immutable test bundle for cleanup");
        std::fs::remove_dir_all(store).expect("smoke store cleanup");
    }

    #[tokio::test]
    async fn root_barrier_rereads_a_root_that_appeared_between_list_and_watch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let call_count = Arc::clone(&calls);
        let root = json!({
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "kind":"WebApplication",
            "metadata": {
                "name":"root",
                "namespace":"ns",
                "uid":ROOT_UID,
                "resourceVersion":"11",
                "generation":1,
                "finalizers":[]
            },
            "spec": {
                "message":"appeared in list/watch gap",
                "expose":false,
                "program": {
                    "configMapName":"bundle",
                    "key":"program.mjs",
                    "digest":BUNDLE_DIGEST
                }
            }
        });
        let service = service_fn(move |request: Request<Body>| {
            let root = root.clone();
            let call_count = Arc::clone(&call_count);
            async move {
                let ordinal = call_count.fetch_add(1, Ordering::SeqCst);
                let uri = request
                    .uri()
                    .path_and_query()
                    .expect("request URI")
                    .as_str();
                let response = match ordinal {
                    0 => {
                        assert!(uri.contains("/webapplications?"));
                        wire_json(json!({
                            "apiVersion":"demo.oxideloop.dev/v1alpha1",
                            "kind":"WebApplicationList",
                            "metadata":{"resourceVersion":"list-rv-10"},
                            "items":[]
                        }))
                    }
                    1 => {
                        assert!(uri.contains("/webapplications?"));
                        assert!(uri.contains("watch=true"));
                        // The helper must not need a subsequent watch event:
                        // the exact GET below is the authority for the gap.
                        Response::builder()
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::empty())
                            .expect("empty watch response")
                    }
                    2 => {
                        assert!(uri.ends_with("/webapplications/root"));
                        wire_json(root)
                    }
                    _ => panic!("unexpected root barrier request {ordinal}: {uri}"),
                };
                Ok::<_, Infallible>(response)
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let config = DriverConfigV0 {
            namespace: "ns".into(),
            root_name: "root".into(),
            bundle_store: std::env::temp_dir().join("unused-root-barrier-store"),
            renderer_unix_socket: "/tmp/renderer.sock".into(),
            #[cfg(feature = "test-failpoints")]
            failpoint_unix_socket: None,
        };

        let (_watch, machine) = establish_root_barrier(&transport, &config)
            .await
            .expect("list/watch/get root barrier");
        assert_eq!(machine.expect("root reread").root().uid, ROOT_UID);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    fn conflict_response() -> Response<Body> {
        Response::builder()
            .status(http::StatusCode::CONFLICT)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "apiVersion":"v1",
                    "kind":"Status",
                    "status":"Failure",
                    "reason":"Conflict",
                    "message":"scripted conflict",
                    "code":409
                }))
                .expect("conflict response JSON"),
            ))
            .expect("conflict response")
    }

    #[tokio::test]
    async fn pending_automation_watch_stream_conflict_retries_only_its_bounded_barrier() {
        let spec = automation_spec();
        let run_name = automation_run_name(ROOT_UID, &spec.occurrence_id).expect("run name");
        let run = automation_object(&spec, &run_name, "31", &AutomationRunStatus::default()).value;
        let calls = Arc::new(AtomicUsize::new(0));
        let service_calls = Arc::clone(&calls);
        let service = service_fn(move |request: Request<Body>| {
            let service_calls = Arc::clone(&service_calls);
            let run = run.clone();
            let run_name = run_name.clone();
            async move {
                let ordinal = service_calls.fetch_add(1, Ordering::SeqCst);
                let response = if ordinal.is_multiple_of(2) {
                    assert!(request.uri().path().contains("/automationruns"));
                    wire_json(json!({
                        "apiVersion":"runtime.oxideloop.dev/v1alpha1",
                        "kind":"AutomationRunList",
                        "metadata":{"resourceVersion":format!("list-rv-{ordinal}")},
                        "items":[]
                    }))
                } else {
                    assert!(
                        request
                            .uri()
                            .path()
                            .ends_with(&format!("/automationruns/{run_name}"))
                    );
                    wire_json(run)
                };
                Ok::<_, Infallible>(response)
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        for _ in 0..=MAX_BARRIER_WATCH_RESTARTS {
            let (sender, watch) = WatchHandleV0::test_channel(1);
            sender
                .try_send(Err(TransportErrorV0::Conflict(
                    ConflictProvenanceV0::WatchTerminal {
                        stage: oxideloop_kube_transport::WatchStageV0::Stream,
                        resource: ResourceKindV0::AutomationRun,
                    },
                )))
                .expect("terminal AutomationRun watch record");
            transport.test_push_watch(watch);
        }
        let Err(error) = establish_pending_watch(&pending_automation_actor(), &transport).await
        else {
            panic!("fourth AutomationRun stream conflict must exhaust the bounded barrier")
        };
        assert!(matches!(
            error,
            RuntimeErrorV0::Discovery(TransportErrorV0::Conflict(
                ConflictProvenanceV0::WatchTerminal {
                    stage: oxideloop_kube_transport::WatchStageV0::Stream,
                    resource: ResourceKindV0::AutomationRun,
                }
            ))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn pending_automation_read_conflict_is_not_retried_as_a_watch_fault() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service_calls = Arc::clone(&calls);
        let service = service_fn(move |_request: Request<Body>| {
            let service_calls = Arc::clone(&service_calls);
            async move {
                service_calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(conflict_response())
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let Err(error) = establish_pending_watch(&pending_automation_actor(), &transport).await
        else {
            panic!("AutomationRun list conflict must remain terminal")
        };
        assert!(matches!(
            error,
            RuntimeErrorV0::Discovery(TransportErrorV0::Conflict(ConflictProvenanceV0::Read {
                operation: oxideloop_kube_transport::ReadOperationV0::List,
                resource: ResourceKindV0::AutomationRun,
            }))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn drain_root_json(resource_version: &str) -> serde_json::Value {
        json!({
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "kind":"WebApplication",
            "metadata": {
                "name":"root", "namespace":"ns", "uid":ROOT_UID,
                "resourceVersion":resource_version, "generation":1, "finalizers":[]
            },
            "spec": {
                "message":"drain root", "expose":false,
                "program": {"configMapName":"bundle", "key":"program.mjs", "digest":BUNDLE_DIGEST}
            }
        })
    }

    fn drain_root_config() -> DriverConfigV0 {
        DriverConfigV0 {
            namespace: "ns".into(),
            root_name: "root".into(),
            bundle_store: std::env::temp_dir().join("unused-drain-root-store"),
            renderer_unix_socket: "/tmp/renderer.sock".into(),
            #[cfg(feature = "test-failpoints")]
            failpoint_unix_socket: None,
        }
    }

    #[tokio::test]
    async fn drain_root_watch_skips_unrelated_records_before_managed_update_or_delete() {
        for event in [
            WatchEventV0::Modified(root_event("managed-modified")),
            WatchEventV0::Deleted(root_event("managed-deleted")),
        ] {
            let service = service_fn(move |request: Request<Body>| async move {
                assert!(request.uri().path().ends_with("/webapplications/root"));
                Ok::<_, Infallible>(wire_json(drain_root_json("root-rv-after-drain")))
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let (sender, mut watch) = WatchHandleV0::test_channel(3);
            let mut unrelated = root_event("unrelated");
            unrelated.reference.name = "not-the-root".into();
            sender
                .try_send(Ok(WatchEventV0::Modified(unrelated)))
                .expect("unrelated root collection record");
            sender.try_send(Ok(event)).expect("managed root record");
            let mut actor = automation_candidate_actor();
            let before = actor.barrier();

            assert!(matches!(
                drain_root_watch(&mut watch, &mut actor, &transport, &drain_root_config())
                    .await
                    .expect("managed root record is drained"),
                RootWatchDrainV0::Current
            ));
            assert!(
                actor.barrier().generation > before.generation,
                "the managed record behind an unrelated record must refresh root authority"
            );
        }
    }

    #[tokio::test]
    async fn drain_root_watch_skips_unrelated_records_before_terminal_restart() {
        for terminal in [
            TransportErrorV0::WatchGone,
            TransportErrorV0::WatchOverflow,
            TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal {
                stage: oxideloop_kube_transport::WatchStageV0::Stream,
                resource: ResourceKindV0::WebApplication,
            }),
        ] {
            let service = service_fn(move |_request: Request<Body>| async move {
                panic!("terminal root drain must not reread before restart handling");
                #[allow(unreachable_code)]
                Ok::<_, Infallible>(Response::new(Body::empty()))
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let (sender, mut watch) = WatchHandleV0::test_channel(3);
            let mut unrelated = root_event("unrelated");
            unrelated.reference.name = "not-the-root".into();
            sender
                .try_send(Ok(WatchEventV0::Modified(unrelated)))
                .expect("unrelated root collection record");
            sender
                .try_send(Err(terminal))
                .expect("terminal root record");
            let mut actor = automation_candidate_actor();
            let before = actor.barrier();

            assert!(matches!(
                drain_root_watch(&mut watch, &mut actor, &transport, &drain_root_config())
                    .await
                    .expect("terminal root record is drained"),
                RootWatchDrainV0::Invalidated
            ));
            assert!(actor.barrier().cache_epoch > before.cache_epoch);
        }
    }

    #[test]
    fn final_render_automation_is_source_pinned_and_single_candidate_is_enqueued() {
        let mut actor = automation_candidate_actor();
        let input = final_automation_input(&actor);
        let dependency = final_automation_dependency();
        let output = final_automation_output(dependency.clone());
        let mut candidates = automation_candidates_from_final_render(
            &actor,
            &input,
            &output,
            &[dependency],
            &final_automation_descriptor(),
        )
        .expect("exact final declaration is authorized");
        let stale_candidate = automation_candidates_from_final_render(
            &actor,
            &input,
            &output,
            &[final_automation_dependency()],
            &final_automation_descriptor(),
        )
        .expect("second final declaration has identical pins")
        .pop_front()
        .expect("one candidate");
        assert_eq!(candidates.len(), 1);
        let expected = automation_source_transition_token(&candidates[0].trigger);
        assert_eq!(candidates[0].trigger.observation_token, expected);
        actor.active_render_input = Some((actor.barrier(), input.clone()));

        // Regression for the single-item queue: it reports a delivery even
        // though popping it leaves the queue empty, so the driver re-enters
        // `next_command` instead of blocking in its watch select.
        assert!(enqueue_next_automation_candidate(
            &mut actor,
            &mut candidates
        ));
        assert!(candidates.is_empty());

        let mut changed = actor.state().root().clone();
        changed.root_resource_version = "2".into();
        assert!(actor.root_observed(changed));
        assert!(!stale_candidate.matches(&actor));
    }

    #[test]
    fn superseded_final_render_automation_candidate_cannot_materialize() {
        let mut actor = ready_actor();
        let initial_input = final_automation_input(&actor);
        let dependency = final_automation_dependency();
        let output = final_automation_output(dependency.clone());
        let mut stale = automation_candidates_from_final_render(
            &actor,
            &initial_input,
            &output,
            std::slice::from_ref(&dependency),
            &final_automation_descriptor(),
        )
        .expect("initial candidate is authorized");

        let mut latest_input = initial_input.clone();
        latest_input.observation_token = "latest-final-render-token".into();
        actor.active_render_input = Some((actor.barrier(), latest_input.clone()));
        assert!(
            !enqueue_next_automation_candidate(&mut actor, &mut stale),
            "the earlier render declaration must be dropped before TriggerAutomation"
        );
        assert!(
            actor
                .next_command()
                .expect("stale candidate is internal")
                .is_none()
        );

        let mut latest = automation_candidates_from_final_render(
            &actor,
            &latest_input,
            &output,
            &[dependency],
            &final_automation_descriptor(),
        )
        .expect("latest candidate is authorized");
        assert!(enqueue_next_automation_candidate(&mut actor, &mut latest));
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::Persist { operation, .. }))
                if operation.purpose
                    == oxideloop_controller_api::PersistPurpose::AutomationBaseline
        ));
    }

    #[test]
    fn final_render_automation_rejects_a_source_absent_from_final_observations() {
        let actor = automation_candidate_actor();
        let mut input = final_automation_input(&actor);
        input.observations.clear();
        let dependency = final_automation_dependency();
        assert!(matches!(
            automation_candidates_from_final_render(
                &actor,
                &input,
                &final_automation_output(dependency.clone()),
                &[dependency],
                &final_automation_descriptor(),
            ),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }

    async fn wait_for_space(
        sender: &tokio::sync::mpsc::Sender<Result<WatchEventV0, TransportErrorV0>>,
    ) {
        loop {
            match sender.try_send(Ok(WatchEventV0::Bookmark {
                resource_version: "bookmark".into(),
            })) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    tokio::task::yield_now().await;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    panic!("collector unexpectedly closed its watch")
                }
            }
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn pinned_command_collects_root_dependency_and_run_without_a_second_effect() {
        let barrier = crate::BarrierV0 {
            generation: 4,
            cache_epoch: 1,
        };
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let (dependency_sender, dependency_watch) = WatchHandleV0::test_channel(1);
        let retained = crate::ObservationBarrierV0 {
            generation: barrier,
            dependencies: vec![Dependency::Collection {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                selector: String::new(),
                collection_revision: "collection-rv".into(),
                observation_token: "token".into(),
            }],
            collections: BTreeMap::new(),
            objects: vec![],
            watches: vec![dependency_watch],
        };
        let mut fixed_point = Some(KubeFixedPointPortV0::<()>::retained_for_test(
            barrier, retained,
        ));

        let spec = automation_spec();
        let run_name = automation_run_name(ROOT_UID, &spec.occurrence_id).expect("run name");
        let spec_hash = sha256_hex(
            &canonical_bytes(&serde_json::to_value(&spec).expect("spec value"))
                .expect("canonical spec"),
        );
        let pending = AutomationPending {
            occurrence_id: spec.occurrence_id.clone(),
            automation_id: spec.automation_id.clone(),
            source: spec.source.clone(),
            run_name: run_name.clone(),
            run_uid: "run-uid".into(),
            run_resource_version: "31".into(),
            spec_hash,
        };
        let expectation = AutomationRunExpectationV0::from_pending(
            ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "root".into(),
            },
            ROOT_UID,
            &pending,
        )
        .expect("durable expectation");
        let (run_sender, run_watch) = WatchHandleV0::test_channel(1);
        let current_status = AutomationRunStatus::default();
        let current = automation_object(&spec, &run_name, "31", &current_status);
        let mut automation_watch = Some(
            AutomationRunObservationBarrierV0::test_from_authenticated(
                expectation,
                current,
                run_watch,
            )
            .expect("current run passes shared validator"),
        );

        let starts = Arc::new(AtomicUsize::new(0));
        let effect_starts = Arc::clone(&starts);
        let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
        let transport = KubeTransportV0::test_from_client(Client::new(
            service_fn(|_request: Request<Body>| async {
                Ok::<_, Infallible>(Response::new(Body::empty()))
            }),
            "default",
        ));
        let operation = transport.begin_collected_operation_session();
        let collect = collect_while_observing_with_operation(
            async move {
                effect_starts.fetch_add(1, Ordering::SeqCst);
                release_receiver.await.expect("test releases exact command");
                Ok::<_, RuntimeErrorV0>(())
            },
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            Some(operation),
        );
        let events = async {
            while starts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            root_sender
                .try_send(Ok(WatchEventV0::Modified(root_event("9"))))
                .expect("root event");
            dependency_sender
                .try_send(Ok(WatchEventV0::Modified(dependency_event("8"))))
                .expect("dependency event");
            let completed = AutomationRunStatus {
                attempts: 1,
                materialization_name: Some("event-name".into()),
                materialization_uid: Some("event-uid".into()),
                completion: Some(AutomationRunCompletion {
                    state: AutomationRunCompletionState::Succeeded,
                    message: "done".into(),
                }),
            };
            run_sender
                .try_send(Ok(WatchEventV0::Modified(automation_object(
                    &spec, &run_name, "32", &completed,
                ))))
                .expect("validated run completion event");
            wait_for_space(&root_sender).await;
            wait_for_space(&dependency_sender).await;
            wait_for_space(&run_sender).await;
            release_sender
                .send(())
                .expect("one owned effect remains pending until release");
        };
        let (collected, ()) = tokio::join!(collect, events);
        let (CollectedEffectV0::Completed(()), buffered) =
            collected.expect("pinned effect completes once")
        else {
            panic!("test effect cannot produce a prewrite invalidation");
        };
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            buffered.root,
            Some(BufferedRootWatchV0::Observed { ref uid, deleted: false, .. }) if uid == ROOT_UID
        ));
        assert!(matches!(
            buffered.dependency,
            Some(BufferedRetainedWatchV0::Change { .. })
        ));
        assert!(matches!(
            buffered.automation,
            Some(BufferedAutomationWatchV0::Changed(ref run)) if run.resource_version() == "32"
        ));
        assert!(buffered.prewrite_invalidated.is_some());
    }

    #[tokio::test]
    async fn raw_root_self_echo_does_not_invalidate_a_prewrite_session() {
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let starts = Arc::new(AtomicUsize::new(0));
        let effect_starts = Arc::clone(&starts);
        let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
        let transport = KubeTransportV0::test_from_client(Client::new(
            service_fn(|_request: Request<Body>| async {
                Ok::<_, Infallible>(Response::new(Body::empty()))
            }),
            "default",
        ));
        let collect = collect_while_observing_with_operation(
            async move {
                effect_starts.fetch_add(1, Ordering::SeqCst);
                release_receiver.await.expect("release exact command");
                Ok::<_, RuntimeErrorV0>(())
            },
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            Some(transport.begin_collected_operation_session()),
        );
        let events = async {
            while starts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            root_sender
                .try_send(Ok(WatchEventV0::Modified(root_event("9"))))
                .expect("root self echo");
            wait_for_space(&root_sender).await;
            release_sender.send(()).expect("release command");
        };
        let (collected, ()) = tokio::join!(collect, events);
        let (CollectedEffectV0::Completed(()), buffered) =
            collected.expect("command completed once")
        else {
            panic!("test effect cannot produce a prewrite invalidation");
        };
        assert!(matches!(
            buffered.root,
            Some(BufferedRootWatchV0::Observed { .. })
        ));
        assert!(buffered.prewrite_invalidated.is_none());
    }

    #[tokio::test]
    async fn invalidated_prewrite_returns_its_buffered_facts_without_an_acknowledgement() {
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let polls = Arc::new(AtomicUsize::new(0));
        let service_polls = Arc::clone(&polls);
        let transport = KubeTransportV0::test_from_client(Client::new(
            service_fn(move |_request: Request<Body>| {
                let polls = Arc::clone(&service_polls);
                async move {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(Response::new(Body::empty()))
                }
            }),
            "default",
        ));
        let operation = transport.begin_collected_operation_session();
        let expected_capability = operation.clone();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (finish_sender, finish_receiver) = tokio::sync::oneshot::channel();
        let collect = collect_while_observing_with_operation(
            async move {
                started_sender.send(()).expect("effect starts once");
                finish_receiver.await.expect("release effect");
                Err::<(), RuntimeErrorV0>(RuntimeErrorV0::Discovery(
                    TransportErrorV0::PreWriteInvalidated,
                ))
            },
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            Some(operation),
        );
        let events = async {
            started_receiver.await.expect("effect started");
            root_sender
                .try_send(Err(TransportErrorV0::WatchGone))
                .expect("terminal root event");
            wait_for_space(&root_sender).await;
            finish_sender.send(()).expect("complete exact effect");
        };
        let (collected, ()) = tokio::join!(collect, events);
        let (CollectedEffectV0::PreWriteInvalidated(capability), buffered) =
            collected.expect("special completion retains facts")
        else {
            panic!("prewrite invalidation must not become an acknowledgement");
        };
        assert!(capability.matches_exact_session(&expected_capability));
        assert!(matches!(
            buffered.root,
            Some(BufferedRootWatchV0::RestartRequired)
        ));
        assert!(buffered.prewrite_invalidated.is_some());
        assert_eq!(
            polls.load(Ordering::SeqCst),
            0,
            "the collector event wins before a child request is polled"
        );
    }

    #[tokio::test]
    async fn write_polled_gate_cannot_be_cancelled_by_a_later_terminal_watch() {
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let transport = KubeTransportV0::test_from_client(Client::new(
            service_fn(|_request: Request<Body>| async {
                Ok::<_, Infallible>(Response::new(Body::empty()))
            }),
            "default",
        ));
        let operation = transport.begin_collected_operation_session();
        operation
            .prewrite_gate()
            .try_mark_write_polled()
            .expect("transport won the first-poll CAS");
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (finish_sender, finish_receiver) = tokio::sync::oneshot::channel();
        let collect = collect_while_observing_with_operation(
            async move {
                started_sender.send(()).expect("effect starts");
                finish_receiver.await.expect("complete write-owned effect");
                Ok::<_, RuntimeErrorV0>(())
            },
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            Some(operation.clone()),
        );
        let events = async {
            started_receiver.await.expect("effect started");
            root_sender
                .try_send(Err(TransportErrorV0::WatchGone))
                .expect("terminal root watch");
            wait_for_space(&root_sender).await;
            finish_sender.send(()).expect("complete owned effect");
        };
        let (collected, ()) = tokio::join!(collect, events);
        let (CollectedEffectV0::Completed(()), buffered) =
            collected.expect("write-polled completion remains normal")
        else {
            panic!("a later watch cannot cancel a polled write");
        };
        assert_eq!(
            operation.prewrite_gate().state(),
            PrewriteGateStateV0::WritePolled
        );
        assert!(buffered.prewrite_invalidated.is_none());
        assert!(matches!(
            buffered.root,
            Some(BufferedRootWatchV0::RestartRequired)
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn real_transport_root_status_echo_restarts_the_full_prewrite_before_one_write() {
        let (actor, command) = prewrite_status_actor();
        let status = match &command {
            CommandV0::PersistRootStatus { status, .. } => status.clone(),
            _ => panic!("fixture must issue a root-status command"),
        };
        let initial_root = prewrite_wire_root(
            &actor,
            &actor.state().root().status,
            &actor.state().root().root_resource_version,
            None,
        );
        let echoed_root = prewrite_wire_root(&actor, &status, "root-rv-after-status", None);
        let state = prewrite_wire_state(&actor, &actor.state().root().state_resource_version);
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let first_preflight_started = Arc::new(AtomicUsize::new(0));
        let root_gets = Arc::new(AtomicUsize::new(0));
        let state_gets = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let release_first_preflight = Arc::new(tokio::sync::Notify::new());
        let service = service_fn({
            let root_sender = root_sender.clone();
            let first_preflight_started = Arc::clone(&first_preflight_started);
            let root_gets = Arc::clone(&root_gets);
            let state_gets = Arc::clone(&state_gets);
            let writes = Arc::clone(&writes);
            let release_first_preflight = Arc::clone(&release_first_preflight);
            move |request: Request<Body>| {
                let root_sender = root_sender.clone();
                let first_preflight_started = Arc::clone(&first_preflight_started);
                let root_gets = Arc::clone(&root_gets);
                let state_gets = Arc::clone(&state_gets);
                let writes = Arc::clone(&writes);
                let release_first_preflight = Arc::clone(&release_first_preflight);
                let initial_root = initial_root.clone();
                let echoed_root = echoed_root.clone();
                let state = state.clone();
                let status = status.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        match root_gets.fetch_add(1, Ordering::SeqCst) {
                            0 => {
                                root_sender
                                    .try_send(Ok(WatchEventV0::Modified(root_event(
                                        "root-rv-after-status",
                                    ))))
                                    .expect("inject raw root self-echo before initial response");
                                first_preflight_started.store(1, Ordering::SeqCst);
                                release_first_preflight.notified().await;
                                Ok::<_, Infallible>(wire_json(initial_root))
                            }
                            1 => Ok(wire_json(echoed_root)),
                            2 => Ok(wire_json(initial_root)),
                            other => panic!("unexpected root preflight {other}"),
                        }
                    } else if method == http::Method::GET && path.contains("/oxideloopstates/") {
                        state_gets.fetch_add(1, Ordering::SeqCst);
                        Ok(wire_json(state))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root/status")
                    {
                        let body = request
                            .into_body()
                            .collect()
                            .await
                            .expect("root-status body");
                        let body: serde_json::Value =
                            serde_json::from_slice(&body.to_bytes()).expect("root-status JSON");
                        assert_eq!(
                            body.get("status"),
                            Some(&serde_json::to_value(status).expect("status JSON"))
                        );
                        writes.fetch_add(1, Ordering::SeqCst);
                        Ok(wire_json(echoed_root))
                    } else {
                        panic!("unexpected prewrite request {method} {path}");
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let address = root_address(&actor).expect("root address");
        let witness = actor.prewrite_root_witness();
        let operation = transport.begin_collected_operation_session();
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let collect = collect_kubernetes_command_while_observing(
            &transport,
            &address,
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            operation.clone(),
            Some(witness),
        );
        let release = async {
            while first_preflight_started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            // The injected record is consumed by the collector before the
            // old GET may return. The defensive bookmark must not restart
            // authentication from the preceding exact self echo, and the old
            // epoch therefore has no write lease.
            wait_for_space(&root_sender).await;
            release_first_preflight.notify_one();
        };
        let (result, ()) = tokio::join!(collect, release);
        let (KubernetesEffectCompletionV0::Acknowledged(event), buffered) =
            result.expect("exact self-echo restarts the command")
        else {
            panic!("exact self-echo and a later bookmark must not invalidate the command");
        };
        assert!(matches!(
            *event,
            Some(EventV0::RootStatusPersisted { ref new_root_resource_version, .. })
                if new_root_resource_version == "root-rv-after-status"
        ));
        assert!(
            buffered.root.is_none(),
            "owned self-echo is consumed, never replayed"
        );
        assert_eq!(
            root_gets.load(Ordering::SeqCst),
            3,
            "old GET, authentication GET, fresh full preflight"
        );
        assert_eq!(
            state_gets.load(Ordering::SeqCst),
            2,
            "authentication and fresh preflight both pin State"
        );
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "only the fresh epoch may write"
        );
        assert_eq!(
            operation.prewrite_gate().state(),
            PrewriteGateStateV0::Proven
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn real_transport_genuine_root_change_invalidates_before_any_write() {
        let (actor, command) = prewrite_status_actor();
        let initial_root = prewrite_wire_root(
            &actor,
            &actor.state().root().status,
            &actor.state().root().root_resource_version,
            None,
        );
        let changed_root = prewrite_wire_root(
            &actor,
            &actor.state().root().status,
            "root-rv-independent-change",
            Some("independent root edit"),
        );
        let state = prewrite_wire_state(&actor, &actor.state().root().state_resource_version);
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let first_preflight_started = Arc::new(AtomicUsize::new(0));
        let root_gets = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let release_first_preflight = Arc::new(tokio::sync::Notify::new());
        let service = service_fn({
            let root_sender = root_sender.clone();
            let first_preflight_started = Arc::clone(&first_preflight_started);
            let root_gets = Arc::clone(&root_gets);
            let writes = Arc::clone(&writes);
            let release_first_preflight = Arc::clone(&release_first_preflight);
            move |request: Request<Body>| {
                let root_sender = root_sender.clone();
                let first_preflight_started = Arc::clone(&first_preflight_started);
                let root_gets = Arc::clone(&root_gets);
                let writes = Arc::clone(&writes);
                let release_first_preflight = Arc::clone(&release_first_preflight);
                let initial_root = initial_root.clone();
                let changed_root = changed_root.clone();
                let state = state.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        if root_gets.fetch_add(1, Ordering::SeqCst) == 0 {
                            root_sender
                                .try_send(Ok(WatchEventV0::Modified(root_event(
                                    "root-rv-independent-change",
                                ))))
                                .expect("inject independent root change");
                            first_preflight_started.store(1, Ordering::SeqCst);
                            release_first_preflight.notified().await;
                            Ok::<_, Infallible>(wire_json(initial_root))
                        } else {
                            Ok(wire_json(changed_root))
                        }
                    } else if method == http::Method::GET && path.contains("/oxideloopstates/") {
                        Ok(wire_json(state))
                    } else if method == http::Method::PATCH {
                        writes.fetch_add(1, Ordering::SeqCst);
                        panic!("a genuine root change must win before the write is polled");
                    } else {
                        panic!("unexpected genuine-change request {method} {path}");
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let address = root_address(&actor).expect("root address");
        let operation = transport.begin_collected_operation_session();
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let collect = collect_kubernetes_command_while_observing(
            &transport,
            &address,
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            operation.clone(),
            Some(actor.prewrite_root_witness()),
        );
        let release = async {
            while first_preflight_started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            wait_for_space(&root_sender).await;
            release_first_preflight.notify_one();
        };
        let (result, ()) = tokio::join!(collect, release);
        let (
            KubernetesEffectCompletionV0::PreWriteInvalidated {
                root_authentication: Some(RootAuthenticationOutcomeV0::Genuine),
                ..
            },
            buffered,
        ) = result.expect("genuine root change is retained without acknowledgement")
        else {
            panic!("genuine root change must invalidate before write");
        };
        assert!(matches!(
            buffered.root,
            Some(BufferedRootWatchV0::Observed { ref resource_version, .. })
                if resource_version == "root-rv-independent-change"
        ));
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            operation.prewrite_gate().state(),
            PrewriteGateStateV0::Invalidated
        );
    }

    #[tokio::test]
    async fn real_transport_write_wins_and_late_root_watch_is_only_buffered() {
        let (actor, command) = prewrite_status_actor();
        let status = match &command {
            CommandV0::PersistRootStatus { status, .. } => status.clone(),
            _ => panic!("fixture must issue a root-status command"),
        };
        let initial_root = prewrite_wire_root(
            &actor,
            &actor.state().root().status,
            &actor.state().root().root_resource_version,
            None,
        );
        let written_root = prewrite_wire_root(&actor, &status, "root-rv-after-status", None);
        let state = prewrite_wire_state(&actor, &actor.state().root().state_resource_version);
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let patch_started = Arc::new(AtomicUsize::new(0));
        let release_patch = Arc::new(tokio::sync::Notify::new());
        let service = service_fn({
            let root_sender = root_sender.clone();
            let patch_started = Arc::clone(&patch_started);
            let release_patch = Arc::clone(&release_patch);
            move |request: Request<Body>| {
                let root_sender = root_sender.clone();
                let patch_started = Arc::clone(&patch_started);
                let release_patch = Arc::clone(&release_patch);
                let initial_root = initial_root.clone();
                let written_root = written_root.clone();
                let state = state.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    if method == http::Method::GET && path.ends_with("/webapplications/root") {
                        Ok::<_, Infallible>(wire_json(initial_root))
                    } else if method == http::Method::GET && path.contains("/oxideloopstates/") {
                        Ok(wire_json(state))
                    } else if method == http::Method::PATCH
                        && path.ends_with("/webapplications/root/status")
                    {
                        patch_started.store(1, Ordering::SeqCst);
                        root_sender
                            .try_send(Err(TransportErrorV0::WatchGone))
                            .expect("deliver terminal root watch after write poll");
                        release_patch.notified().await;
                        Ok(wire_json(written_root))
                    } else {
                        panic!("unexpected write-wins request {method} {path}");
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let address = root_address(&actor).expect("root address");
        let operation = transport.begin_collected_operation_session();
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let collect = collect_kubernetes_command_while_observing(
            &transport,
            &address,
            &command,
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
            operation.clone(),
            Some(actor.prewrite_root_witness()),
        );
        let release = async {
            while patch_started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            wait_for_space(&root_sender).await;
            release_patch.notify_one();
        };
        let (result, ()) = tokio::join!(collect, release);
        let (KubernetesEffectCompletionV0::Acknowledged(event), buffered) =
            result.expect("the polled status write remains owned")
        else {
            panic!("a later root watch cannot cancel a polled write");
        };
        assert!(matches!(*event, Some(EventV0::RootStatusPersisted { .. })));
        assert!(matches!(
            buffered.root,
            Some(BufferedRootWatchV0::RestartRequired)
        ));
        assert!(buffered.prewrite_invalidated.is_none());
        assert_eq!(
            operation.prewrite_gate().state(),
            PrewriteGateStateV0::Proven
        );
    }

    #[tokio::test]
    async fn render_completion_edge_drains_simultaneously_ready_root_and_old_dependency() {
        let barrier = crate::BarrierV0 {
            generation: 4,
            cache_epoch: 1,
        };
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(3);
        let (dependency_sender, dependency_watch) = WatchHandleV0::test_channel(2);
        let mut unrelated_root = root_event("8");
        unrelated_root.reference.name = "unrelated".into();
        root_sender
            .try_send(Ok(WatchEventV0::Modified(unrelated_root)))
            .expect("ready unrelated root event");
        root_sender
            .try_send(Ok(WatchEventV0::Modified(root_event("9"))))
            .expect("ready root event");
        dependency_sender
            .try_send(Ok(WatchEventV0::Modified(dependency_event("8"))))
            .expect("ready old dependency event");
        let retained = crate::ObservationBarrierV0 {
            generation: barrier,
            dependencies: vec![Dependency::Collection {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                selector: String::new(),
                collection_revision: "collection-rv".into(),
                observation_token: "token".into(),
            }],
            collections: BTreeMap::new(),
            objects: vec![],
            watches: vec![dependency_watch],
        };
        let port = KubeFixedPointPortV0::<()>::retained_for_test(barrier, retained);
        let (_, mut collector, _) = port.split_for_render();
        let mut automation_watch = None;

        let (result, buffered) = collect_render_while_observing(
            async { Ok::<_, RuntimeErrorV0>(()) },
            &mut root_watch,
            &mut collector,
            &mut automation_watch,
            "root",
        )
        .await;
        result.expect("ready command is retained once");

        assert!(matches!(
            buffered.root,
            Some(BufferedRootWatchV0::Observed { ref uid, deleted: false, .. }) if uid == ROOT_UID
        ));
        assert!(matches!(
            buffered.dependency,
            Some(BufferedRetainedWatchV0::Change { .. })
        ));
    }

    #[tokio::test]
    async fn render_error_completion_preserves_ready_root_and_old_d_facts() {
        for terminal in [TransportErrorV0::WatchGone, TransportErrorV0::WatchOverflow] {
            let barrier = crate::BarrierV0 {
                generation: 4,
                cache_epoch: 1,
            };
            let (root_sender, mut root_watch) = WatchHandleV0::test_channel(2);
            let (dependency_sender, dependency_watch) = WatchHandleV0::test_channel(1);
            root_sender
                .try_send(Ok(WatchEventV0::Modified(root_event("9"))))
                .expect("ready managed-root event");
            dependency_sender
                .try_send(Ok(WatchEventV0::Modified(dependency_event("8"))))
                .expect("ready old-D event");
            let retained = crate::ObservationBarrierV0 {
                generation: barrier,
                dependencies: vec![Dependency::Collection {
                    api_version: "v1".into(),
                    kind: "ConfigMap".into(),
                    namespace: "ns".into(),
                    selector: String::new(),
                    collection_revision: "collection-rv".into(),
                    observation_token: "token".into(),
                }],
                collections: BTreeMap::new(),
                objects: vec![],
                watches: vec![dependency_watch],
            };
            let port = KubeFixedPointPortV0::<()>::retained_for_test(barrier, retained);
            let (_, mut collector, _) = port.split_for_render();
            let mut automation_watch = None;

            let (result, buffered) = collect_render_while_observing(
                async move { Err::<(), _>(RuntimeErrorV0::Discovery(terminal)) },
                &mut root_watch,
                &mut collector,
                &mut automation_watch,
                "root",
            )
            .await;
            assert!(matches!(
                result,
                Err(RuntimeErrorV0::Discovery(
                    TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow
                ))
            ));
            assert!(matches!(
                buffered.root,
                Some(BufferedRootWatchV0::Observed { ref uid, .. }) if uid == ROOT_UID
            ));
            assert!(matches!(
                buffered.dependency,
                Some(BufferedRetainedWatchV0::Change { .. })
            ));
        }
    }

    #[tokio::test]
    async fn completion_edge_skips_unrelated_root_records_until_the_target_record() {
        for target in [
            WatchEventV0::Modified(root_event("9")),
            WatchEventV0::Deleted(root_event("10")),
        ] {
            let (sender, mut root_watch) = WatchHandleV0::test_channel(3);
            let mut unrelated = root_event("8");
            unrelated.reference.name = "unrelated".into();
            sender
                .try_send(Ok(WatchEventV0::Modified(unrelated)))
                .expect("unrelated root record is queued first");
            sender
                .try_send(Ok(target))
                .expect("target root record is queued");
            let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
            let mut automation_watch = None;
            let ((), buffered) = collect_while_observing(
                async { Ok::<_, RuntimeErrorV0>(()) },
                &mut root_watch,
                &mut fixed_point,
                &mut automation_watch,
                "root",
            )
            .await
            .expect("completion edge drains through unrelated root data");
            assert!(matches!(
                buffered.root,
                Some(BufferedRootWatchV0::Observed { ref uid, .. }) if uid == ROOT_UID
            ));
        }
    }

    fn coverage_change_driver_fixture() -> (
        RootActorV0,
        KubeFixedPointPortV0<'static, ()>,
        crate::CoverageChangedV0,
        crate::GenuineChangeV0,
        Vec<u8>,
    ) {
        let mut actor = render_ready_actor_for_coverage_test();
        let barrier = actor.barrier();
        let dependency = Dependency::Object {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "dependency".into(),
            uid: Some("dependency-uid".into()),
            resource_version: Some("1".into()),
            observation_token: "coverage-token".into(),
        };
        let object = dependency_event("1");
        let observation = Observation {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "dependency".into(),
            uid: "dependency-uid".into(),
            resource_version: "1".into(),
            value: object.value.clone(),
        };
        let retained = crate::ObservationBarrierV0 {
            generation: barrier,
            dependencies: vec![dependency],
            collections: BTreeMap::new(),
            objects: vec![(object.reference.clone(), Some(object))],
            watches: vec![],
        };
        let port = KubeFixedPointPortV0::<()>::retained_for_test(barrier, retained);
        let root = actor.state().root().clone();
        let input = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: root.namespace.clone(),
                name: root.name.clone(),
                uid: root.uid.clone(),
                generation: u64::try_from(root.generation).expect("nonnegative generation"),
                spec: serde_json::to_value(&root.spec).expect("root spec"),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: barrier.cache_epoch.to_string(),
            observation_token: "coverage-token".into(),
            observations: vec![],
        };
        actor.in_flight = true;
        actor.owned_in_flight = Some(CommandV0::RequestRender {
            preconditions: PreconditionsV0 {
                root_uid: root.uid,
                root_generation: root.generation,
                root_resource_version: root.root_resource_version,
                state_uid: root.state_uid,
                state_resource_version: root.state_resource_version,
                observation_token: input.observation_token.clone(),
                cache_epoch: input.cache_epoch.clone(),
            },
        });
        let _capability = actor
            .issue_render_capability(input.clone(), port.identity())
            .expect("actor seals the render capability");
        let initial_pin = crate::canonical_render_input(&input).expect("canonical active pin");
        let expected = crate::GenuineChangeV0::from_observations(&[observation], barrier, 1)
            .expect("canonical retained D projection");
        let buffered = crate::GenuineChangeV0::from_observations(
            &[Observation {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "dependency".into(),
                uid: "dependency-uid".into(),
                resource_version: "2".into(),
                value: dependency_event("2").value,
            }],
            barrier,
            1,
        )
        .expect("canonical buffered D projection");
        let outcome = crate::CoverageChangedV0::new(port.identity(), barrier, 1, expected);
        (actor, port, outcome, buffered, initial_pin)
    }

    fn empty_command_buffers() -> BufferedCommandWatchesV0 {
        BufferedCommandWatchesV0 {
            root: None,
            dependency: None,
            automation: None,
            root_faulted: false,
            dependency_faulted: false,
            automation_faulted: false,
            fault: None,
            prewrite_invalidated: None,
        }
    }

    #[tokio::test]
    async fn coverage_change_reunion_reduces_old_d_before_consuming_the_sealed_outcome() {
        let unexpected_calls = Arc::new(AtomicUsize::new(0));
        let recorded_calls = Arc::clone(&unexpected_calls);
        let service = service_fn(move |_request: Request<Body>| {
            recorded_calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(500)
                        .body(Body::from(Vec::<u8>::new()))
                        .expect("unexpected response"),
                )
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let config = DriverConfigV0 {
            namespace: "ns".into(),
            root_name: "root".into(),
            bundle_store: PathBuf::from("/unused/coverage-race-bundles"),
            renderer_unix_socket: PathBuf::from("/unused/coverage-race.sock"),
            #[cfg(feature = "test-failpoints")]
            failpoint_unix_socket: None,
        };
        let (_root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut automation_watch = None;
        let mut automation_candidates = VecDeque::new();

        let (mut actor, port, outcome, buffered_change, initial_pin) =
            coverage_change_driver_fixture();
        let initial_barrier = actor.barrier();
        let port_identity = port.identity();
        let mut fixed_point = Some(port);
        let mut buffered = empty_command_buffers();
        buffered.dependency = Some(BufferedRetainedWatchV0::Change {
            port_identity,
            barrier: initial_barrier,
            change: buffered_change,
        });
        assert!(
            !consume_coverage_changed_after_reunion(
                &mut actor,
                &transport,
                &config,
                &mut root_watch,
                &mut fixed_point,
                &mut automation_watch,
                &mut automation_candidates,
                buffered,
                outcome,
            )
            .await
            .expect("buffered old-D supersedes before coverage consumption")
        );
        assert_eq!(unexpected_calls.load(Ordering::SeqCst), 0);
        assert!(!actor.owns_render_request());
        assert_eq!(actor.barrier().generation, initial_barrier.generation + 1);
        assert!(actor.active_render_input.is_none());
        assert!(actor.render_provenance_request.is_none());
        assert!(fixed_point.is_some());
        assert_ne!(
            actor
                .active_render_input
                .as_ref()
                .map(|(_, input)| crate::canonical_render_input(input).expect("active pin")),
            Some(initial_pin),
            "no completed renderer input replaced the superseded speculative pin"
        );

        let (mut actor, port, outcome, _buffered_change, _initial_pin) =
            coverage_change_driver_fixture();
        let initial_barrier = actor.barrier();
        let mut fixed_point = Some(port);
        assert!(
            consume_coverage_changed_after_reunion(
                &mut actor,
                &transport,
                &config,
                &mut root_watch,
                &mut fixed_point,
                &mut automation_watch,
                &mut automation_candidates,
                empty_command_buffers(),
                outcome,
            )
            .await
            .expect("the exact reunited port consumes its own sealed mismatch")
        );
        assert!(!actor.owns_render_request());
        assert_eq!(actor.barrier().generation, initial_barrier.generation + 1);
        assert!(actor.active_render_input.is_none());
        assert!(actor.render_provenance_request.is_none());
    }

    #[tokio::test]
    async fn root_or_old_d_fault_is_reduced_before_render_binding_or_fencing() {
        for dependency_fault in [false, true] {
            let service = service_fn(move |_request: Request<Body>| async move {
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(500)
                        .body(Body::from(Vec::<u8>::new()))
                        .expect("unexpected response"),
                )
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let config = DriverConfigV0 {
                namespace: "ns".into(),
                root_name: "root".into(),
                bundle_store: PathBuf::from("/unused/pre-finalization-bundles"),
                renderer_unix_socket: PathBuf::from("/unused/pre-finalization.sock"),
                #[cfg(feature = "test-failpoints")]
                failpoint_unix_socket: None,
            };
            let (_root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
            let mut automation_watch = None;
            let mut automation_candidates = VecDeque::new();
            let (mut actor, port, _outcome, _buffered_change, _initial_pin) =
                coverage_change_driver_fixture();
            let mut fixed_point = Some(port);
            let mut buffered = empty_command_buffers();
            buffered.root_faulted = !dependency_fault;
            buffered.dependency_faulted = dependency_fault;
            buffered.fault = Some(if dependency_fault {
                "dependency watch failed: test fault".into()
            } else {
                "root watch failed: test fault".into()
            });
            let pre_finalization = buffered
                .take_root_dependency_pre_finalization()
                .expect("fatal observation fault is a pre-finalization fact");
            assert!(matches!(
                apply_buffered_watches(
                    &mut actor,
                    &transport,
                    &config,
                    &mut root_watch,
                    &mut fixed_point,
                    &mut automation_watch,
                    &mut automation_candidates,
                    pre_finalization,
                )
                .await,
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert!(!actor.owns_render_request());
            assert!(actor.active_render_input.is_none());
            assert!(actor.render_provenance_request.is_none());
        }
    }

    #[tokio::test]
    async fn render_completion_edge_turns_old_dependency_terminal_watches_into_restart() {
        for terminal in [TransportErrorV0::WatchGone, TransportErrorV0::WatchOverflow] {
            let barrier = crate::BarrierV0 {
                generation: 4,
                cache_epoch: 1,
            };
            let (_root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
            let (dependency_sender, dependency_watch) = WatchHandleV0::test_channel(1);
            dependency_sender
                .try_send(Err(terminal))
                .expect("ready old dependency terminal event");
            let retained = crate::ObservationBarrierV0 {
                generation: barrier,
                dependencies: vec![Dependency::Collection {
                    api_version: "v1".into(),
                    kind: "ConfigMap".into(),
                    namespace: "ns".into(),
                    selector: String::new(),
                    collection_revision: "collection-rv".into(),
                    observation_token: "token".into(),
                }],
                collections: BTreeMap::new(),
                objects: vec![],
                watches: vec![dependency_watch],
            };
            let port = KubeFixedPointPortV0::<()>::retained_for_test(barrier, retained);
            let (_, mut collector, _) = port.split_for_render();
            let mut automation_watch = None;

            let (result, buffered) = collect_render_while_observing(
                async { Ok::<_, RuntimeErrorV0>(()) },
                &mut root_watch,
                &mut collector,
                &mut automation_watch,
                "root",
            )
            .await;
            result.expect("owned effect completes before restart is applied");
            assert!(matches!(
                buffered.dependency,
                Some(BufferedRetainedWatchV0::RestartRequired { .. })
            ));
        }
    }

    #[tokio::test]
    async fn failed_owned_effect_is_not_replayed_when_the_root_watch_returns_410() {
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let starts = Arc::new(AtomicUsize::new(0));
        let effect_starts = Arc::clone(&starts);
        let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
        let collect = collect_while_observing(
            async move {
                effect_starts.fetch_add(1, Ordering::SeqCst);
                release_receiver.await.expect("release failed effect");
                Err::<(), _>(RuntimeErrorV0::Renderer)
            },
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
        );
        let watches = async {
            while starts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            root_sender
                .try_send(Err(TransportErrorV0::WatchGone))
                .expect("root watch 410");
            wait_for_space(&root_sender).await;
            release_sender
                .send(())
                .expect("the original owned effect remains pending");
        };
        let (result, ()) = tokio::join!(collect, watches);
        assert!(matches!(result, Err(RuntimeErrorV0::Renderer)));
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "the failed command is surfaced for process recovery, never reconstructed"
        );
    }

    #[tokio::test]
    async fn fatal_root_watch_is_latched_until_the_owned_effect_resolves() {
        let (root_sender, mut root_watch) = WatchHandleV0::test_channel(1);
        let mut fixed_point: Option<KubeFixedPointPortV0<'_, ()>> = None;
        let mut automation_watch = None;
        let starts = Arc::new(AtomicUsize::new(0));
        let effect_starts = Arc::clone(&starts);
        let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
        let collect = collect_while_observing(
            async move {
                effect_starts.fetch_add(1, Ordering::SeqCst);
                release_receiver.await.expect("release exact effect");
                Ok::<_, RuntimeErrorV0>(())
            },
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
        );
        let watches = async {
            while starts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            root_sender
                .try_send(Err(TransportErrorV0::Other("fatal root stream".into())))
                .expect("fatal root delivery");
            wait_for_space(&root_sender).await;
            assert_eq!(starts.load(Ordering::SeqCst), 1);
            release_sender.send(()).expect("release original effect");
        };
        let (result, ()) = tokio::join!(collect, watches);
        let ((), buffered) = result.expect("effect remains owned until completion");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(buffered.root_faulted);
        assert!(
            buffered
                .fault
                .as_deref()
                .is_some_and(|fault| fault.contains("root watch failed"))
        );
    }

    /// Creates the same reducer-owned, empty-journal collision state the
    /// production fixed-point acknowledgement reaches.  This intentionally
    /// uses no adapter shortcut: the driver tests below exercise the exact
    /// no-command policy after the pure reducer has retained its witness.
    fn blocked_foreign_collision_actor() -> RootActorV0 {
        let mut actor = render_ready_actor_for_coverage_test();
        let barrier = actor.barrier();
        actor
            .prepare_initial_informer_observation(barrier)
            .expect("informer observation is sealed");
        actor.informers_ready(barrier);
        let CommandV0::RequestRender { preconditions } = actor
            .next_command()
            .expect("informer readiness is reduced")
            .expect("informer readiness requests a render")
        else {
            panic!("informer readiness must request a render");
        };
        let root = actor.state().root().clone();
        let input = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                uid: root.uid.clone(),
                namespace: root.namespace.clone(),
                name: root.name.clone(),
                generation: u64::try_from(root.generation).expect("nonnegative generation"),
                spec: serde_json::to_value(&root.spec).expect("root spec serializes"),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: preconditions.cache_epoch.clone(),
            observation_token: preconditions.observation_token.clone(),
            observations: vec![],
        };
        let input_json = serde_json::to_string(&input).expect("render input serializes");
        let output_json = serde_json::to_string(&RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::Apply,
            resources: vec![ResourceDeclaration {
                logical_id: "foreign".into(),
                object: json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": "foreign", "namespace": "ns"},
                    "data": {"value": "foreign"},
                }),
                readiness: None,
            }],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        })
        .expect("render output serializes");
        let blocked = oxideloop_controller_core::reduce(
            actor.state().clone(),
            EventV0::Rendered {
                frozen: FrozenInputV0 {
                    root_generation: root.generation,
                    state_resource_version: root.state_resource_version.clone(),
                    observation_token: preconditions.observation_token.clone(),
                    cache_epoch: preconditions.cache_epoch.clone(),
                    final_observation_fence_token: "f".repeat(64),
                    render_input_hash: parse_validate_render_input(&input_json)
                        .expect("render input validates")
                        .sha256,
                },
                render_input_json: input_json,
                render_output_json: output_json,
                observed: vec![DesiredObservationV0 {
                    identity: ObjectIdentity {
                        api_version: "v1".into(),
                        kind: "ConfigMap".into(),
                        namespace: "ns".into(),
                        name: "foreign".into(),
                    },
                    observation: MutationObservation::Existing {
                        identity: oxideloop_mutation_model::ObjectIdentity {
                            uid: "foreign-uid".into(),
                            resource_version: "foreign-rv".into(),
                        },
                    },
                    qualification: BackendQualification::BuiltInTested {
                        kubernetes_minor: "1.36".into(),
                        gvr: "configmaps".into(),
                    },
                    recovery_proof: None,
                    deletion_recovery_proof: None,
                }],
            },
        )
        .expect("empty-journal foreign collision is watchable");
        assert_eq!(blocked.state.phase(), PhaseV0::BlockedForeignCollision);
        assert!(blocked.commands.is_empty());
        RootActorV0::new(blocked.state)
    }

    #[allow(clippy::too_many_lines)]
    fn ready_actor() -> RootActorV0 {
        let mut actor = render_ready_actor_for_coverage_test();
        let barrier = actor.barrier();
        actor
            .prepare_initial_informer_observation(barrier)
            .expect("informer observation is sealed");
        actor.informers_ready(barrier);
        let CommandV0::RequestRender { preconditions } = actor
            .next_command()
            .expect("informer readiness is reduced")
            .expect("informer readiness requests a render")
        else {
            panic!("informer readiness must request a render");
        };
        let root = actor.state().root().clone();
        let input = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                uid: root.uid.clone(),
                namespace: root.namespace.clone(),
                name: root.name.clone(),
                generation: u64::try_from(root.generation).expect("nonnegative generation"),
                spec: serde_json::to_value(&root.spec).expect("root spec serializes"),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: preconditions.cache_epoch.clone(),
            observation_token: preconditions.observation_token.clone(),
            observations: vec![],
        };
        let input_json = serde_json::to_string(&input).expect("render input serializes");
        let frozen = FrozenInputV0 {
            root_generation: root.generation,
            state_resource_version: root.state_resource_version.clone(),
            observation_token: preconditions.observation_token.clone(),
            cache_epoch: preconditions.cache_epoch.clone(),
            final_observation_fence_token: "e".repeat(64),
            render_input_hash: parse_validate_render_input(&input_json)
                .expect("render input validates")
                .sha256,
        };
        let output_json = serde_json::to_string(&RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::ExplicitEmpty,
            resources: vec![],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        })
        .expect("empty output serializes");
        let first = oxideloop_controller_core::reduce(
            actor.state().clone(),
            EventV0::Rendered {
                frozen: frozen.clone(),
                render_input_json: input_json.clone(),
                render_output_json: output_json.clone(),
                observed: vec![],
            },
        )
        .expect("initial render enters persistence");
        // The driver fixture has an initial root status self-echo to close.
        // Reduce it first, then render against the status-pinned request
        // exactly as the production driver does before durable revalidation.
        let (prepared, mut completion_frozen, completion_input_json) =
            if let CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                root_uid,
                ..
            } = &first.commands[0]
            {
                let status_persisted = oxideloop_controller_core::reduce(
                    first.state,
                    EventV0::RootStatusPersisted {
                        token: token.clone(),
                        old_root_resource_version: old_root_resource_version.clone(),
                        new_root_resource_version: "root-rv-ready-status".into(),
                        root_uid: root_uid.clone(),
                    },
                )
                .expect("initial status persistence is accepted");
                let CommandV0::RequestRender { preconditions } = &status_persisted.commands[0]
                else {
                    panic!("initial status persistence must request a status-pinned render");
                };
                let root = status_persisted.state.root().clone();
                let input = RenderInputV0 {
                    abi_version: ABI_VERSION_V0.into(),
                    root: RootProjection {
                        api_version: "demo.oxideloop.dev/v1alpha1".into(),
                        kind: "WebApplication".into(),
                        uid: root.uid.clone(),
                        namespace: root.namespace.clone(),
                        name: root.name.clone(),
                        generation: u64::try_from(root.generation).expect("nonnegative generation"),
                        spec: serde_json::to_value(&root.spec).expect("root spec serializes"),
                    },
                    bundle_digest: root.bundle_digest.clone(),
                    cache_epoch: preconditions.cache_epoch.clone(),
                    observation_token: preconditions.observation_token.clone(),
                    observations: vec![],
                };
                let input_json = serde_json::to_string(&input).expect("render input serializes");
                let frozen = FrozenInputV0 {
                    root_generation: root.generation,
                    state_resource_version: root.state_resource_version.clone(),
                    observation_token: preconditions.observation_token.clone(),
                    cache_epoch: preconditions.cache_epoch.clone(),
                    final_observation_fence_token: "d".repeat(64),
                    render_input_hash: parse_validate_render_input(&input_json)
                        .expect("render input validates")
                        .sha256,
                };
                let prepared = oxideloop_controller_core::reduce(
                    status_persisted.state,
                    EventV0::Rendered {
                        frozen: frozen.clone(),
                        render_input_json: input_json.clone(),
                        render_output_json: output_json.clone(),
                        observed: vec![],
                    },
                )
                .expect("status-pinned render enters persistence");
                (prepared, frozen, input_json)
            } else {
                (first, frozen, input_json)
            };
        let CommandV0::Persist { operation, .. } = &prepared.commands[0] else {
            panic!(
                "render must persist its revalidation receipt: {:?}",
                prepared.commands[0]
            );
        };
        let revalidate = oxideloop_controller_core::reduce(
            prepared.state,
            EventV0::Persisted {
                token: operation.token.clone(),
                old_state_resource_version: operation.old_state_resource_version.clone(),
                new_state_resource_version: "state-rv-revalidate".into(),
                state_uid: root.state_uid.clone(),
            },
        )
        .expect("prepared receipt schedules revalidation");
        completion_frozen.state_resource_version = "state-rv-revalidate".into();
        let completed = oxideloop_controller_core::reduce(
            revalidate.state,
            EventV0::Revalidated {
                frozen: completion_frozen,
                render_input_json: completion_input_json,
                render_output_json: output_json,
                observed: vec![],
            },
        )
        .expect("revalidation completes");
        let CommandV0::Persist { operation, .. } = &completed.commands[0] else {
            panic!("completed revalidation must persist result");
        };
        let ready = oxideloop_controller_core::reduce(
            completed.state,
            EventV0::Persisted {
                token: operation.token.clone(),
                old_state_resource_version: operation.old_state_resource_version.clone(),
                new_state_resource_version: "state-rv-ready".into(),
                state_uid: root.state_uid.clone(),
            },
        )
        .expect("result persistence reaches Ready");
        assert_eq!(ready.state.phase(), PhaseV0::Ready);
        RootActorV0::new(ready.state)
    }

    #[test]
    fn driver_bfc_no_command_policy_disables_automation_and_exact_deletion_reenters_render() {
        let mut actor = blocked_foreign_collision_actor();
        assert!(
            actor
                .next_command()
                .expect("collision has no command")
                .is_none()
        );
        let mut informer_admissions = 0;
        let first = drive_no_command_disposition(&mut actor, |actor| {
            informer_admissions += 1;
            actor.informers_ready(actor.barrier());
            Ok(true)
        })
        .expect("valid collision enters retained-watch policy");
        assert_eq!(
            first,
            NoCommandDriverActionV0::Watch {
                watch_automation: false,
                emitted_observability: true,
            }
        );
        assert_eq!(
            informer_admissions, 0,
            "a blocked collision neither establishes nor polls automation authority"
        );
        assert_eq!(
            drive_no_command_disposition(&mut actor, |_| Ok(true))
                .expect("unchanged collision remains watchable"),
            NoCommandDriverActionV0::Watch {
                watch_automation: false,
                emitted_observability: false,
            },
            "unchanged watch select passes do not emit a loop of collision logs"
        );

        let change = crate::GenuineChangeV0::from_observations(
            &[Observation {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "foreign".into(),
                uid: "foreign-uid".into(),
                resource_version: "foreign-rv-after-delete".into(),
                value: json!({"deleted": true}),
            }],
            actor.barrier(),
            1,
        )
        .expect("exact retained deletion observation is canonical");
        assert!(
            actor.record_genuine_dependency_change(&change),
            "the exact foreign target deletion wakes the retained watch"
        );
        assert_eq!(actor.state().phase(), PhaseV0::InformersPending);
        assert_eq!(
            drive_no_command_disposition(&mut actor, |actor| {
                informer_admissions += 1;
                actor.informers_ready(actor.barrier());
                Ok(true)
            })
            .expect("fenced collision wake admits one fresh informer readiness"),
            NoCommandDriverActionV0::ContinueAfterInformerAdmission
        );
        assert_eq!(informer_admissions, 1);
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::RequestRender { .. }))
        ));
    }

    #[test]
    fn driver_stale_blocked_collision_witness_is_illegal_quiescence() {
        let mut actor = blocked_foreign_collision_actor();
        let prior_epoch = actor.barrier().cache_epoch;
        // This is a production-representable failure mode: a retained watch
        // invalidates after the reducer retained its old collision witness.
        // BFC intentionally has no generic invalidation continuation, so the
        // phase remains blocked while the witness's old cache pin is stale.
        actor.watch_invalidated();
        assert_eq!(actor.state().phase(), PhaseV0::BlockedForeignCollision);
        assert_eq!(actor.barrier().cache_epoch, prior_epoch + 1);
        assert!(
            actor
                .next_command()
                .expect("stale collision has no command")
                .is_none()
        );
        let mut informer_admissions = 0;
        assert!(matches!(
            drive_no_command_disposition(&mut actor, |_| {
                informer_admissions += 1;
                Ok(true)
            }),
            Err(RuntimeErrorV0::Hold(reason)) if reason.contains("illegal quiescence")
        ));
        assert_eq!(
            informer_admissions, 0,
            "a stale witness fails before informer or automation watch authority"
        );
        assert!(
            matches!(actor.mode(), ModeV0::Hold(reason) if reason.contains("illegal quiescence"))
        );
    }

    #[test]
    fn driver_root_deletion_preempts_blocked_collision_and_jsonl_is_correlated_and_deduped() {
        let mut actor = blocked_foreign_collision_actor();
        let first = drive_no_command_disposition(&mut actor, |_| Ok(true))
            .expect("collision reaches driver watch policy");
        assert!(matches!(
            first,
            NoCommandDriverActionV0::Watch {
                watch_automation: false,
                emitted_observability: true,
            }
        ));
        let record =
            actor.observability_record("watching_blocked_foreign_collision", None, None, None);
        assert_eq!(record["component"], "oxideloop-controller-runtime");
        assert_eq!(record["root_uid"], ROOT_UID);
        assert_eq!(record["root_resource_version"], "root-rv-after-descriptor");
        assert_eq!(record["command_class"], "none");
        assert_eq!(
            record["collision"][0]["identity"],
            "v1/ConfigMap/ns/foreign"
        );
        assert_eq!(record["collision"][0]["foreign_uid"], "foreign-uid");
        assert_eq!(
            record["collision"][0]["foreign_resource_version"],
            "foreign-rv"
        );

        let mut deleting = actor.state().root().clone();
        deleting.deleting = true;
        deleting.root_resource_version = "root-rv-deleting".into();
        assert!(actor.root_observed(deleting));
        let command = actor
            .next_command()
            .expect("root deletion refresh is reduced before a retained wait")
            .expect("root deletion begins a durable cleanup transition");
        assert_ne!(actor.state().phase(), PhaseV0::BlockedForeignCollision);
        assert!(!matches!(command, CommandV0::RequestRender { .. }));
    }

    #[test]
    fn driver_ready_policy_dedupes_stable_jsonl_observation() {
        let mut actor = ready_actor();
        assert_eq!(
            drive_no_command_disposition(&mut actor, |_| Ok(true))
                .expect("Ready enters the retained watch policy"),
            NoCommandDriverActionV0::Watch {
                watch_automation: true,
                emitted_observability: true,
            }
        );
        assert_eq!(
            drive_no_command_disposition(&mut actor, |_| Ok(true))
                .expect("unchanged Ready remains a retained watch policy"),
            NoCommandDriverActionV0::Watch {
                watch_automation: true,
                emitted_observability: false,
            }
        );
    }
}
