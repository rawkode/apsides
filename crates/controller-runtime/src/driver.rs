//! The production, single-root controller driver.
//!
//! This module is intentionally the only place where the three long-lived
//! streams (the nominated root, retained dependencies, and a durable pending
//! `AutomationRun`) are coordinated.  It owns exactly one root actor and
//! never adopts a replacement UID.  Kubernetes effects remain in the sealed
//! dispatcher; renderer and action calls are delegated to their typed ports.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::PathBuf,
};

use oxideloop_bundle_archive::install_archive;
use oxideloop_bundle_contract::{BundleDescriptorV0, BundleStore, MAX_BUNDLE_BYTES};
use oxideloop_controller_api::{ObjectIdentity, ProgramBundleReference};
use oxideloop_controller_core::{
    ActionTriggerV0, CommandV0, EventV0, MachineStateV0, PhaseV0, PreconditionsV0,
    automation_source_transition_token,
};
use oxideloop_event_planner::{
    ObjectIdentity as PlannerIdentityV0, ObjectRef as PlannerObjectRefV0, RunCorrelationV0,
};
use oxideloop_kube_transport::{
    AutomationRunExpectationV0, AutomationRunObservationBarrierV0, KubeTransportV0, ObjectRefV0,
    ResourceKindV0, TransportErrorV0, WatchEventV0, WatchHandleV0,
};
use oxideloop_renderer_abi::{
    limits::validate_event_metadata,
    model::{ABI_VERSION_V0, Dependency, RenderInputV0, RootProjection},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
#[cfg(feature = "test-failpoints")]
use tokio::time::{Instant as TokioInstant, sleep, timeout_at};

#[cfg(feature = "test-failpoints")]
use crate::failpoint_uds::{DeletionBarrierV0, FailpointControlV0};
use crate::{
    BufferedRetainedWatchV0, CompletedObservationFenceOutcomeV0, FailpointsV0, FixedPointPortV0,
    KubeAutomationObservationPortV0, KubeAutomationPortV0, KubeDeletionObservationPortV0,
    KubeFixedPointPortV0, MAX_SEQUENCING_RETRIES, ModeV0, RetainedWatchCollectorV0,
    RetainedWatchCoordinationV0, RootActorV0, RuntimeErrorV0, UnixRendererPortV0,
    dispatch_transport_command, drive_automation, load_machine_from_transport,
};
#[cfg(feature = "test-failpoints")]
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

enum QuiescentWakeV0 {
    #[cfg(feature = "test-failpoints")]
    FailpointPoll,
    Root(Result<WatchEventV0, TransportErrorV0>),
    Dependency(Result<BufferedRetainedWatchV0, RuntimeErrorV0>),
    Automation(Box<Result<oxideloop_kube_transport::ValidatedAutomationRunV0, TransportErrorV0>>),
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
        let (mut root_watch, initial_machine) =
            establish_root_barrier(&self.transport, &self.config).await?;
        let mut actor = initial_machine.map(start_actor);
        if let Some(active) = actor
            .as_ref()
            .filter(|active| active.state().is_root_only_deletion_recovery())
        {
            eprintln!(
                "oxideloop: entering receipt-bounded root-only deletion recovery for {}/{} uid={}",
                self.config.namespace,
                self.config.root_name,
                active.state().root().uid
            );
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
        let mut failpoint_listener = self
            .config
            .failpoint_unix_socket
            .as_ref()
            .map(crate::failpoint_uds::FailpointUdsListenerV0::bind)
            .transpose()
            .map_err(|error| RuntimeErrorV0::Hold(format!("invalid failpoint socket: {error}")))?;
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
                    Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
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
            match drain_root_watch(&mut root_watch, active, &self.transport, &self.config).await? {
                RootWatchDrainV0::Current => {}
                RootWatchDrainV0::RootOnlyRecovery => {
                    clear_root_only_recovery_ephemera(
                        &mut fixed_point,
                        &mut automation_watch,
                        &mut automation_candidates,
                    );
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
                        continue;
                    }
                }
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
                    let (completion, buffered) = collect_while_observing(
                        deletion.observe_for_capability(capability),
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &self.config.root_name,
                    )
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
                    let collected = collect_render_while_observing(
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
                    let (mut result, mut buffered) = match collected {
                        Ok(result) => result,
                        Err(RuntimeErrorV0::Discovery(
                            TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow,
                        )) => {
                            active.watch_invalidated();
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    // Root and old-retained records can only be reduced now
                    // that the no-effect render future/capability path has
                    // resolved. A genuine change supersedes the render and
                    // deliberately prevents any fabricated fence.
                    if buffered.root.is_some() || buffered.dependency.is_some() {
                        let automation = buffered.automation.take();
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
                        if !active.owns_render_request() {
                            continue;
                        }
                        buffered = BufferedCommandWatchesV0 {
                            root: None,
                            dependency: None,
                            automation,
                            root_faulted: false,
                            dependency_faulted: false,
                            automation_faulted: false,
                            fault: None,
                        };
                    }
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
                    let (event, buffered) = match execute_kubernetes_command_while_observing(
                        &self.transport,
                        &root_address(active)?,
                        &command,
                        &mut root_watch,
                        &mut fixed_point,
                        &mut automation_watch,
                        &self.config.root_name,
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
                            }
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let event = event.ok_or_else(|| {
                        RuntimeErrorV0::Hold(
                            "Kubernetes command did not produce an acknowledgement".into(),
                        )
                    })?;
                    #[cfg(feature = "test-failpoints")]
                    if deletion_barrier.is_some()
                        && matches!(command, CommandV0::RemoveStateFinalizer { .. })
                    {
                        if !is_exact_state_finalizer_result(&command, &event) {
                            return Err(RuntimeErrorV0::Hold(
                                "armed deletion barrier received a mismatched state-finalizer acknowledgement"
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
                            &event,
                            active,
                            &self.transport,
                            &self.config,
                        )
                        .await?;
                        return Err(RuntimeErrorV0::InjectedCrash(
                            crate::FailpointV0::AfterStateFinalizerBeforeRootFinalizer,
                        ));
                    }
                    // Exact effect acknowledgement is reducer authority and
                    // must reduce before an observed root closure/rebase.
                    active.acknowledge(event)?;
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
                    if matches!(active.mode(), ModeV0::Terminal) {
                        return Ok(DriverExitV0::RootFinalized);
                    }
                    if active.state().phase() == PhaseV0::InformersPending {
                        let port = fixed_point.as_mut().ok_or_else(|| {
                            RuntimeErrorV0::Hold(
                                "informers pending before a verified fixed-point port".into(),
                            )
                        })?;
                        if port.admit_initial_informer_readiness(active)? {
                            // The actor will reduce its sealed readiness into
                            // the normal RequestRender path on the next turn.
                            // Do not enter the retained-watch idle path before
                            // that first render installs its barrier.
                            continue;
                        }
                    }
                    // The journal owns pending runs.  Their list/watch/GET
                    // barrier is re-established after a restart, so a run
                    // created before the watch cannot be missed.
                    if automation_watch.is_none() {
                        automation_watch = establish_pending_watch(active, &self.transport).await?;
                    }
                    if let Some(watch) = automation_watch.as_mut() {
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
                    if active.state().phase() == PhaseV0::Ready
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
                                match automation_watch.as_mut() {
                                    Some(watch) => watch.next_change().await,
                                    None => std::future::pending::<Result<
                                        oxideloop_kube_transport::ValidatedAutomationRunV0,
                                        TransportErrorV0,
                                    >>().await,
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
                                match automation_watch.as_mut() {
                                    Some(watch) => watch.next_change().await,
                                    None => std::future::pending::<Result<
                                        oxideloop_kube_transport::ValidatedAutomationRunV0,
                                        TransportErrorV0,
                                    >>().await,
                                }
                            } => QuiescentWakeV0::Automation(Box::new(run)),
                        };
                        match wake {
                            #[cfg(feature = "test-failpoints")]
                            QuiescentWakeV0::FailpointPoll => {}
                            QuiescentWakeV0::Root(root) => match root {
                                Ok(event) if watch_targets_root(&event, &self.config.root_name) => {
                                    root_only_recovery = refresh_actor_from_transport(
                                        active,
                                        &self.transport,
                                        &self.config,
                                    )
                                    .await?;
                                }
                                Ok(_) => {}
                                Err(
                                    TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow,
                                ) => {
                                    active.watch_invalidated();
                                    let (watch, fresh) =
                                        establish_root_barrier(&self.transport, &self.config)
                                            .await?;
                                    root_watch = watch;
                                    root_only_recovery = refresh_actor_from_machine(active, fresh)?;
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
                                Err(
                                    TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow,
                                ) => {
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
                    }
                }
            }
        }
    }
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
) -> Result<(Option<EventV0>, BufferedCommandWatchesV0), RuntimeErrorV0> {
    collect_while_observing(
        dispatch_owned_command_until_event(transport, address, command),
        root_watch,
        fixed_point,
        automation_watch,
        root_name,
    )
    .await
}

/// Keeps the exact actor-issued command owned across a resumable bootstrap
/// sequence.  The surrounding collector continues polling watches while this
/// single future retries; it never asks `next_command` for a reconstruction.
async fn dispatch_owned_command_until_event(
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
    command: &CommandV0,
) -> Result<Option<EventV0>, RuntimeErrorV0> {
    for _ in 0..=MAX_SEQUENCING_RETRIES {
        if let Some(event) = dispatch_transport_command(transport, address, command).await? {
            return Ok(Some(event));
        }
    }
    Err(RuntimeErrorV0::Hold(
        "transport sequencing retry bound exceeded".into(),
    ))
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
    let mut effect = Box::pin(effect);
    let mut buffered = BufferedCommandWatchesV0 {
        root: None,
        dependency: None,
        automation: None,
        root_faulted: false,
        dependency_faulted: false,
        automation_faulted: false,
        fault: None,
    };
    loop {
        tokio::select! {
            result = &mut effect => {
                return Ok((result?, buffered));
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
                        buffered.root = buffered_root_watch(event);
                    }
                    Ok(_) => {}
                    Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
                        buffered.root = Some(BufferedRootWatchV0::RestartRequired);
                    }
                    Err(error) => {
                        buffered.root_faulted = true;
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
                    Ok(record) => buffered.dependency = Some(record),
                    Err(error) => {
                        buffered.dependency_faulted = true;
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
                    }
                    Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
                        // Its durable expectation is re-established after the
                        // command acknowledgement; no raw run crosses this
                        // boundary.
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
) -> Result<(T, BufferedCommandWatchesV0), RuntimeErrorV0>
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
    };
    loop {
        tokio::select! {
            result = &mut effect => return Ok((result?, buffered)),
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
                Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
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
                Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
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
    if let Some(record) = buffered.dependency
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

/// Establishes the root list/watch/exact-GET barrier. Every start and watch
/// restart uses this one helper so a root that appeared or changed between
/// list and watch is still observed without requiring another event.
async fn establish_root_barrier(
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
) -> Result<(WatchHandleV0, Option<MachineStateV0>), RuntimeErrorV0> {
    let list = transport
        .list_exact(ResourceKindV0::WebApplication, &config.namespace)
        .await?;
    let watch = transport
        .watch_from(
            ResourceKindV0::WebApplication,
            &config.namespace,
            &list.resource_version,
        )
        .await
        .map_err(RuntimeErrorV0::Discovery)?;
    let fresh =
        load_machine_from_transport(transport, &config.namespace, &config.root_name).await?;
    Ok((watch, fresh))
}

async fn drain_root_watch(
    watch: &mut WatchHandleV0,
    actor: &mut RootActorV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
) -> Result<RootWatchDrainV0, RuntimeErrorV0> {
    loop {
        match watch.try_next_event() {
            Ok(Some(event)) if watch_targets_root(&event, &config.root_name) => {
                if refresh_actor_from_transport(actor, transport, config).await? {
                    return Ok(RootWatchDrainV0::RootOnlyRecovery);
                }
            }
            Ok(Some(_) | None) => return Ok(RootWatchDrainV0::Current),
            Err(TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow) => {
                actor.watch_invalidated();
                return Ok(RootWatchDrainV0::Invalidated);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// The exact root stream may either leave the current actor authoritative,
/// require a new list/watch/GET epoch, or reveal that its linked state has
/// vanished and only receipt-bounded deletion authority remains.
enum RootWatchDrainV0 {
    Current,
    Invalidated,
    RootOnlyRecovery,
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
            if replace_actor_with_root_only_recovery(actor, &fresh) {
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
fn replace_actor_with_root_only_recovery(actor: &mut RootActorV0, fresh: &MachineStateV0) -> bool {
    if fresh.root().uid == actor.state().root().uid
        && fresh.root().deleting
        && fresh.is_root_only_deletion_recovery()
        && !actor.state().is_root_only_deletion_recovery()
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
    if replace_actor_with_root_only_recovery(actor, fresh) {
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

#[cfg(feature = "test-failpoints")]
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

/// Proves the exact root-only deletion recovery state before exposing the
/// midpoint to e2e. The direct state reread establishes absence first; the
/// loader then validates the same deleting root, its finalizer, and complete
/// cleanup receipt without allowing an arbitrary raw object to become proof.
#[cfg(feature = "test-failpoints")]
async fn verify_deletion_barrier_and_wait(
    barrier: DeletionBarrierV0,
    event: &EventV0,
    active: &RootActorV0,
    transport: &KubeTransportV0,
    config: &DriverConfigV0,
) -> Result<(), RuntimeErrorV0> {
    let original = active.state().root();
    let (_, state_ref) = root_address(active)?.references();
    let EventV0::StateFinalizerRemoved {
        state_uid: expected_state_uid,
        ..
    } = event
    else {
        return Err(RuntimeErrorV0::Hold(
            "deletion barrier received the wrong event".into(),
        ));
    };
    wait_for_exact_state_absence(transport, &state_ref, expected_state_uid).await?;
    let fresh = load_machine_from_transport(transport, &config.namespace, &config.root_name)
        .await?
        .ok_or_else(|| RuntimeErrorV0::Hold("deletion barrier root is absent".into()))?;
    validate_deletion_barrier_root(original, fresh.root())?;
    tokio::task::block_in_place(|| barrier.reached_and_wait_for_release())
        .map_err(|error| RuntimeErrorV0::Hold(format!("deletion barrier failed: {error}")))
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

#[cfg(feature = "test-failpoints")]
const STATE_ABSENCE_TOTAL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(feature = "test-failpoints")]
const STATE_ABSENCE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Deletion of a state object can lag its successful finalizer patch. Only
/// the exact state UID may remain during this bounded wait; a same-name
/// replacement is never a reason to wait or continue the deletion protocol.
#[cfg(feature = "test-failpoints")]
async fn wait_for_exact_state_absence(
    transport: &KubeTransportV0,
    state_ref: &ObjectRefV0,
    expected_uid: &str,
) -> Result<(), RuntimeErrorV0> {
    wait_for_exact_state_absence_with(
        || async {
            transport
                .reread(state_ref)
                .await
                .map_err(RuntimeErrorV0::from)
        },
        expected_uid,
        STATE_ABSENCE_TOTAL_DEADLINE,
        STATE_ABSENCE_POLL_INTERVAL,
    )
    .await
}

#[cfg(feature = "test-failpoints")]
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
#[cfg(feature = "test-failpoints")]
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
    KubeAutomationObservationPortV0 { transport }
        .establish(expectation)
        .await
        .map(Some)
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
        AutomationRunSpec, AutomationRunStatus, ObjectIdentity, WebApplicationSpec,
        WebApplicationStatus, automation_run_name,
    };
    use oxideloop_controller_core::RootSnapshotV0;
    use oxideloop_kube_transport::{ObjectMetaV0, ObjectV0};
    use oxideloop_renderer_abi::{
        ActionInputV0, canonical_bytes,
        model::{
            ABI_VERSION_V0, AutomationDeclaration, Dependency, Observation, RenderDisposition,
            RenderOutputV0,
        },
        sha256_hex,
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
        root_metadata: bool,
        root_status: bool,
    ) -> serde_json::Value {
        bootstrap_wire_root_for_bundle(
            token,
            state_name,
            root_metadata,
            root_status
                .then(|| json!({"state":{"rootUid":ROOT_UID,"name":state_name,"uid":"state-uid"}})),
            BUNDLE_DIGEST,
        )
    }

    fn bootstrap_wire_root_for_bundle(
        token: &str,
        _state_name: &str,
        root_metadata: bool,
        status: Option<serde_json::Value>,
        bundle_digest: &str,
    ) -> serde_json::Value {
        let annotations = root_metadata.then(|| {
            json!({
                "runtime.oxideloop.dev/root-uid": ROOT_UID,
                "runtime.oxideloop.dev/transaction-id": token,
                "runtime.oxideloop.dev/command-id": token,
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
                    wire_json(bootstrap_wire_root(&token, &state_name, true, true))
                } else if path.ends_with("/webapplications/root") && method == http::Method::PATCH {
                    script.root_metadata = true;
                    wire_json(bootstrap_wire_root(&token, &state_name, true, false))
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
    async fn driver_run_bootstraps_through_self_watches_then_reaches_renderer() {
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
        let service_wire = Arc::clone(&wire);
        let service_root_present = Arc::clone(&root_present);
        let service_root_sender = root_sender.clone();
        let service = service_fn(move |request: Request<Body>| {
            let wire = Arc::clone(&service_wire);
            let root_present = Arc::clone(&service_root_present);
            let root_sender = service_root_sender.clone();
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
                    wire_json(json!({
                        "apiVersion":"demo.oxideloop.dev/v1alpha1",
                        "kind":"WebApplicationList",
                        "metadata":{"resourceVersion":"root-list-rv"},
                        "items":[],
                    }))
                } else if path.ends_with("/webapplications/root") && method == http::Method::GET {
                    if root_present.load(Ordering::SeqCst) {
                        let mut root = bootstrap_wire_root_for_bundle(
                            &token,
                            &state_name,
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
                        true,
                        wire.root_statuses.last().cloned(),
                        &bundle_digest,
                    );
                    root["metadata"]["resourceVersion"] = json!(resource_version.clone());
                    root_sender
                        .try_send(Ok(WatchEventV0::Modified(root_event(&resource_version))))
                        .expect("root self-watch fits the composed buffer");
                    wire_json(root)
                } else if path.ends_with("/webapplications/root") && method == http::Method::PATCH {
                    wire.root_metadata = true;
                    wire_json(bootstrap_wire_root(&token, &state_name, true, false))
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
            "the verified descriptor and sealed initial readiness must reach the renderer: {result:?}; {request_log:?}"
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
            "bootstrap and descriptor verification each persist root status once"
        );
        assert_eq!(
            wire.root_statuses.len(),
            2,
            "bootstrap and descriptor verification each persist one exact status"
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
        let collect = collect_while_observing(
            async move {
                effect_starts.fetch_add(1, Ordering::SeqCst);
                release_receiver.await.expect("test releases exact command");
                Ok::<_, RuntimeErrorV0>(())
            },
            &mut root_watch,
            &mut fixed_point,
            &mut automation_watch,
            "root",
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
        let ((), buffered) = collected.expect("pinned effect completes once");
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
}
