//! Async production ports. Kubernetes remains behind its sealed typed API and
//! renderer socket I/O is isolated from Tokio's executor.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use futures_util::future::select_all;
use oxideloop_bundle_contract::BundleDescriptorV0;
use oxideloop_controller_api::{AutomationRunSpec, InventoryEntry, ObjectIdentity, StateBody};
use oxideloop_controller_core::{
    AutomationRunCleanupV0, DesiredObservationV0, PhaseV0 as CorePhaseV0, RecoveryProofV0,
};
use oxideloop_kube_transport::{
    AutomationCompletionAckV0, AutomationRunExpectationV0, AutomationRunObservationBarrierV0,
    EventMaterializationRefV0, KubeTransportV0, ObjectRefV0, ObjectV0, ResourceKindV0,
    RootAddressV0, WatchEventV0,
};
use oxideloop_mutation_model::{
    BackendQualification, ObjectIdentity as MutationIdentity, Observation as CoreObservation,
};
use oxideloop_renderer_abi::{
    RenderInputV0,
    model::{Dependency, Observation, RenderOutputV0},
};
use oxideloop_renderer_protocol::{PhaseV0, RequestV0, ResponseV0};
use renderer_client::RendererClient;

use crate::{
    AutomationPortV0, BarrierV0, CompletedObservationFenceV0, CompletedObservationSessionV0,
    CoverageChangedV0, DeletionCommandCapabilityV0, FixedPointPortV0, FixedPointResultV0,
    GenuineChangeV0, ObservationBarrierV0, RenderCommandCapabilityV0, RendererPortV0, RootActorV0,
    RuntimeErrorV0, ValidatedDeletionObservationV0, ValidatedRenderObservationV0,
    canonical_render_input, completed_observation_fence_token, deterministic_request_id,
    establish_dependencies_async, normalize_dependencies, reconcile_fixed_point,
    renderer_dependency_plan, renderer_request, resource,
};

/// Renderer port for an explicitly configured Unix socket. Each call owns a
/// short-lived client, while `spawn_blocking` keeps the deadline-bounded sync
/// protocol implementation off the async executor.
#[derive(Clone, Debug)]
pub struct UnixRendererPortV0 {
    socket: PathBuf,
}

impl UnixRendererPortV0 {
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
}

#[async_trait]
impl RendererPortV0 for UnixRendererPortV0 {
    async fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0> {
        let socket = self.socket.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                let mut client =
                    RendererClient::new_unix(socket).map_err(|_| RuntimeErrorV0::Renderer)?;
                client.call(&request).map_err(|_| RuntimeErrorV0::Renderer)
            }
            #[cfg(not(unix))]
            {
                let _ = socket;
                let _ = request;
                Err(RuntimeErrorV0::Renderer)
            }
        })
        .await
        .map_err(|_| RuntimeErrorV0::Renderer)?
    }
}

/// Typed adapter for Event materialization and `AutomationRun` `/status` only.
pub struct KubeAutomationPortV0<'a> {
    pub transport: &'a KubeTransportV0,
}

#[async_trait]
impl AutomationPortV0 for KubeAutomationPortV0<'_> {
    async fn create_or_exact_recover_event(
        &mut self,
        plan: &oxideloop_event_planner::PlannedEventV0,
        event_time: &str,
    ) -> Result<EventMaterializationRefV0, RuntimeErrorV0> {
        self.transport
            .create_or_exact_recover_event(plan, event_time)
            .await
            .map_err(RuntimeErrorV0::Discovery)
    }
    async fn persist_completion(
        &mut self,
        namespace: &str,
        name: &str,
        expected_uid: &str,
        expected_resource_version: &str,
        expected_spec: &oxideloop_controller_api::AutomationRunSpec,
        status: &oxideloop_controller_api::AutomationRunStatus,
    ) -> Result<AutomationCompletionAckV0, RuntimeErrorV0> {
        self.transport
            .persist_automation_run_completion(
                namespace,
                name,
                expected_uid,
                expected_resource_version,
                expected_spec,
                status,
            )
            .await
            .map_err(RuntimeErrorV0::Discovery)
    }
}

/// Sealed adapter for observing a durable pending `AutomationRun`.  It exposes
/// only the authenticated list/watch/reread barrier from kube-transport, so a
/// production driver cannot accidentally consume an unvalidated raw run.
pub struct KubeAutomationObservationPortV0<'a> {
    pub transport: &'a KubeTransportV0,
}

impl KubeAutomationObservationPortV0<'_> {
    pub async fn establish(
        &self,
        expectation: AutomationRunExpectationV0,
    ) -> Result<AutomationRunObservationBarrierV0, RuntimeErrorV0> {
        self.transport
            .observe_pending_automation_run(expectation)
            .await
            .map_err(RuntimeErrorV0::Discovery)
    }
}

/// Sealed deletion-observation adapter. A driver obtains a capability only by
/// running this exact root/state fence plus owned-child list/reread path; it
/// never receives a raw `DeletionRevalidated` event to acknowledge itself.
pub struct KubeDeletionObservationPortV0<'a> {
    transport: &'a KubeTransportV0,
    address: RootAddressV0,
}

/// A sealed result of an actor-free deletion list/reread operation.  It is
/// deliberately consumed only through the same adapter, which re-enters the
/// actor's owned-command validation after the future has completed.
pub(crate) struct DeletionObservationCompletionV0 {
    capability: DeletionCommandCapabilityV0,
    observed_inventory: Vec<InventoryEntry>,
    observed_automation_runs: Vec<AutomationRunCleanupV0>,
}

impl<'a> KubeDeletionObservationPortV0<'a> {
    #[must_use]
    pub fn new(transport: &'a KubeTransportV0, address: RootAddressV0) -> Self {
        Self { transport, address }
    }

    pub async fn observe_for_actor(
        &self,
        actor: &RootActorV0,
    ) -> Result<ValidatedDeletionObservationV0, RuntimeErrorV0> {
        let completion = self
            .observe_for_capability(actor.issue_deletion_capability()?)
            .await?;
        Self::consume_completion(actor, completion)
    }

    /// Executes the sealed deletion observation fence without borrowing the
    /// actor.  The returned completion cannot be fabricated by a driver.
    pub(crate) async fn observe_for_capability(
        &self,
        capability: DeletionCommandCapabilityV0,
    ) -> Result<DeletionObservationCompletionV0, RuntimeErrorV0> {
        let snapshot = self
            .transport
            .observe_deletion_revalidation(&self.address, capability.preconditions())
            .await
            .map_err(RuntimeErrorV0::Discovery)?;
        let root = ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: capability.root().namespace.clone(),
            name: capability.root().name.clone(),
        };
        if snapshot.root.reference != root
            || snapshot.root.metadata.uid != capability.root().uid
            || snapshot.state.metadata.uid != capability.root().state_uid
        {
            return Err(RuntimeErrorV0::Hold(
                "deletion observation transport fence does not match the actor root".into(),
            ));
        }
        let observed_inventory = deletion_inventory_observations(
            &snapshot.inventory,
            &root,
            &capability.root().uid,
            &capability.durable().inventory,
        )?;
        let observed_automation_runs = deletion_automation_observations(
            &snapshot.automation_runs,
            &root,
            &capability.root().uid,
            capability.durable(),
        )?;
        Ok(DeletionObservationCompletionV0 {
            capability,
            observed_inventory,
            observed_automation_runs,
        })
    }

    /// Re-enters the actor only after the actor-free future completed.  This
    /// is the sole conversion from the sealed completion to an acknowledgement
    /// capability accepted by `RootActorV0`.
    pub(crate) fn consume_completion(
        actor: &RootActorV0,
        completion: DeletionObservationCompletionV0,
    ) -> Result<ValidatedDeletionObservationV0, RuntimeErrorV0> {
        actor.validated_deletion_observation(
            completion.capability.preconditions().clone(),
            completion.capability.request_token().into(),
            completion.observed_inventory,
            completion.observed_automation_runs,
        )
    }
}

fn deletion_inventory_observations(
    objects: &[ObjectV0],
    root: &ObjectRefV0,
    root_uid: &str,
    durable_inventory: &[InventoryEntry],
) -> Result<Vec<InventoryEntry>, RuntimeErrorV0> {
    let mut observed_inventory = Vec::with_capacity(objects.len());
    for object in objects {
        let identity = deletion_object_identity(object)?;
        if !exact_controller_owner(object, root, root_uid) {
            return Err(RuntimeErrorV0::Hold(
                "deletion observation child lacks the exact sole controller owner".into(),
            ));
        }
        let Some(known) = durable_inventory
            .iter()
            .find(|item| item.identity == identity)
        else {
            return Err(RuntimeErrorV0::Hold(
                "deletion observation found an unjournaled owned child".into(),
            ));
        };
        let mut observed = known.clone();
        observed.uid.clone_from(&object.metadata.uid);
        observed
            .resource_version
            .clone_from(&object.metadata.resource_version);
        observed_inventory.push(observed);
    }
    Ok(observed_inventory)
}

fn deletion_automation_observations(
    objects: &[ObjectV0],
    root: &ObjectRefV0,
    root_uid: &str,
    durable: &StateBody,
) -> Result<Vec<AutomationRunCleanupV0>, RuntimeErrorV0> {
    let mut observed_automation_runs = Vec::with_capacity(objects.len());
    for object in objects {
        deletion_object_identity(object)?;
        if object.reference.resource != ResourceKindV0::AutomationRun
            || !exact_controller_owner(object, root, root_uid)
            || serde_json::from_value::<AutomationRunSpec>(
                object.value.get("spec").cloned().ok_or_else(|| {
                    RuntimeErrorV0::Hold("deletion AutomationRun spec is absent".into())
                })?,
            )
            .map_err(|_| RuntimeErrorV0::Hold("deletion AutomationRun spec is malformed".into()))?
            .root_uid
                != root_uid
        {
            return Err(RuntimeErrorV0::Hold(
                "deletion AutomationRun does not exactly belong to this root".into(),
            ));
        }
        let occurrence_id = durable
            .automation
            .pending
            .iter()
            .find_map(|pending| {
                (pending.run_name == object.reference.name
                    && pending.run_uid == object.metadata.uid
                    && pending.run_resource_version == object.metadata.resource_version)
                    .then(|| pending.occurrence_id.clone())
            })
            .or_else(|| {
                durable
                    .automation
                    .completions
                    .iter()
                    .find_map(|completion| {
                        (completion.run_name == object.reference.name
                            && completion.run_uid == object.metadata.uid
                            && completion.completed_resource_version
                                == object.metadata.resource_version)
                            .then(|| completion.occurrence_id.clone())
                    })
            })
            .ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "deletion observation found an unjournaled AutomationRun".into(),
                )
            })?;
        observed_automation_runs.push(AutomationRunCleanupV0 {
            occurrence_id,
            name: object.reference.name.clone(),
            uid: object.metadata.uid.clone(),
            resource_version: object.metadata.resource_version.clone(),
        });
    }
    Ok(observed_automation_runs)
}

/// Render and discovery adapter. Its descriptor pin is part of every render
/// request; no discovered object can select a different renderer module.
pub struct KubeFixedPointPortV0<'a, R> {
    transport: Option<&'a KubeTransportV0>,
    pub renderer: R,
    pub descriptor: BundleDescriptorV0,
    pub barrier: BarrierV0,
    fence_sequence: u64,
    identity: u64,
    root_namespace: Option<String>,
    next_retained_incarnation: u64,
    retained: Option<RetainedObservationBarrierV0>,
    #[cfg(test)]
    test_barrier: Option<ObservationBarrierV0>,
}

static NEXT_FIXED_POINT_PORT_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn next_fixed_point_port_identity() -> u64 {
    NEXT_FIXED_POINT_PORT_IDENTITY.fetch_add(1, Ordering::Relaxed)
}

/// The only externally visible outcome of retained dependency watch handling.
/// It intentionally exposes neither a caller-supplied dependency token nor
/// the internal `GenuineChangeV0` capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetainedWatchCoordinationV0 {
    ReconcileScheduled,
    DuplicateIgnored,
    RestartRequired,
}

/// An actor-free retained-watch delivery.  The concrete port mints this only
/// after it has coalesced its authenticated final snapshot.  Its provenance is
/// private: the driver may retain it while an owned command is executing, but
/// can apply it only through the same concrete port after that command's
/// acknowledgement has reduced.
#[derive(Clone, Debug)]
pub(crate) enum BufferedRetainedWatchV0 {
    Change {
        port_identity: u64,
        barrier: BarrierV0,
        change: GenuineChangeV0,
    },
    RestartRequired {
        port_identity: u64,
        barrier: BarrierV0,
    },
}

/// The renderer/discovery half of one fixed-point pass.  It is obtained only
/// by consuming a concrete [`KubeFixedPointPortV0`] and deliberately contains
/// no reference to `RootActorV0`: an owned `RequestRender` can therefore stay
/// pinned while the driver continues to receive root, run, and old-retained
/// dependency records.
///
/// This is crate-private orchestration plumbing, not a second controller
/// replica boundary.  The single production driver reunites it with its
/// companion collector before it can acknowledge a render observation.
pub(crate) struct RenderExecutionPortV0<'a, R> {
    transport: Option<&'a KubeTransportV0>,
    renderer: R,
    descriptor: BundleDescriptorV0,
    barrier: BarrierV0,
    fence_sequence: u64,
    identity: u64,
    root_namespace: Option<String>,
    next_retained_incarnation: u64,
    retained: Option<RetainedObservationBarrierV0>,
    #[cfg(test)]
    test_barrier: Option<ObservationBarrierV0>,
}

pub(crate) enum RenderExecutionOutcomeV0 {
    Stable(Box<FixedPointResultV0>),
    CoverageChanged(CoverageChangedV0),
}

/// The retained-watch half of a split fixed-point port.  It owns the concrete
/// watch handles from the last stable render and can only yield a sealed
/// record tied to the original port identity and barrier.
pub(crate) struct RetainedWatchCollectorV0 {
    identity: u64,
    barrier: BarrierV0,
    retained: Option<RetainedObservationBarrierV0>,
}

/// A one-shot, opaque proof that an execution port and retained collector
/// came from the same concrete fixed-point port.  Its fields stay private so
/// neither a driver nor a test double can fabricate a cross-port reunion.
pub(crate) struct RenderReunionCapabilityV0 {
    identity: u64,
    barrier: BarrierV0,
    // A non-Copy seal makes reunion consumption explicit even though the
    // identity/barrier pins themselves are scalar values.
    _one_shot: Box<()>,
}

/// Final-fence completion cannot silently ignore a buffered watch change. A
/// caller receives either the completed fence or an adapter-authenticated
/// instruction to reconcile/restart; it cannot choose which outcome applies.
#[derive(Debug, PartialEq)]
pub enum CompletedObservationFenceOutcomeV0 {
    Completed(Box<ValidatedRenderObservationV0>),
    ReconcileScheduled,
    DuplicateIgnored,
    RestartRequired,
}

impl<'a, R> KubeFixedPointPortV0<'a, R> {
    pub(crate) fn invalidate_to_barrier(&mut self, barrier: BarrierV0) {
        self.retained = None;
        self.barrier = barrier;
    }

    #[must_use]
    pub fn new(
        transport: &'a KubeTransportV0,
        renderer: R,
        descriptor: BundleDescriptorV0,
        barrier: BarrierV0,
    ) -> Self {
        Self {
            transport: Some(transport),
            renderer,
            descriptor,
            barrier,
            fence_sequence: 0,
            identity: next_fixed_point_port_identity(),
            root_namespace: None,
            next_retained_incarnation: 0,
            retained: None,
            #[cfg(test)]
            test_barrier: None,
        }
    }

    #[must_use]
    pub(crate) fn identity(&self) -> u64 {
        self.identity
    }

    /// Queues the first render only after a verified descriptor has reached
    /// the actor's durable `InformersPending` continuation. No retained
    /// observation exists before that first render, so waiting for one would
    /// incorrectly turn startup into a watch restart.
    ///
    /// This is deliberately narrower than retained-watch coordination: a
    /// retained barrier here means observation authority already exists and
    /// must not be discarded or re-issued as initial readiness.
    pub(crate) fn admit_initial_informer_readiness(
        &mut self,
        actor: &mut RootActorV0,
    ) -> Result<bool, RuntimeErrorV0> {
        if !matches!(actor.mode(), crate::ModeV0::Running)
            || actor.state().phase() != CorePhaseV0::InformersPending
        {
            return Ok(false);
        }
        if self.retained.is_some() {
            return Err(RuntimeErrorV0::Hold(
                "informers pending fixed-point port unexpectedly retains an observation barrier"
                    .into(),
            ));
        }

        // An exact post-ack root self-watch or a prior terminal watch fault
        // may have advanced the actor barrier. With no retained handles there
        // is no old observation authority to preserve, so bind this verified
        // descriptor port to the actor's current sealed readiness candidate.
        self.barrier = actor.begin_observation_barrier();
        actor.prepare_initial_informer_observation(self.barrier)?;
        actor.informers_ready(self.barrier);
        Ok(true)
    }

    /// Consumes the concrete port into two actor-free halves.  The execution
    /// half may perform renderer/discovery I/O while the collector owns the
    /// previously retained Kubernetes watches.  Only
    /// [`Self::reunite_after_render`] accepts the matching opaque capability.
    pub(crate) fn split_for_render(
        self,
    ) -> (
        RenderExecutionPortV0<'a, R>,
        RetainedWatchCollectorV0,
        RenderReunionCapabilityV0,
    ) {
        let Self {
            transport,
            renderer,
            descriptor,
            barrier,
            fence_sequence,
            identity,
            root_namespace,
            next_retained_incarnation,
            retained,
            #[cfg(test)]
            test_barrier,
        } = self;
        (
            RenderExecutionPortV0 {
                transport,
                renderer,
                descriptor,
                barrier,
                fence_sequence,
                identity,
                root_namespace,
                next_retained_incarnation,
                retained: None,
                #[cfg(test)]
                test_barrier,
            },
            RetainedWatchCollectorV0 {
                identity,
                barrier,
                retained,
            },
            RenderReunionCapabilityV0 {
                identity,
                barrier,
                _one_shot: Box::new(()),
            },
        )
    }

    /// Reassembles exactly the two halves produced by `split_for_render`.
    /// A mismatch is a stale/fabricated orchestration path, so no retained
    /// watch is carried forward and no completed fence can be minted.
    pub(crate) fn reunite_after_render(
        execution: RenderExecutionPortV0<'a, R>,
        collector: RetainedWatchCollectorV0,
        reunion: RenderReunionCapabilityV0,
    ) -> Result<Self, RuntimeErrorV0> {
        let RetainedWatchCollectorV0 {
            identity: collector_identity,
            barrier: collector_barrier,
            retained: _old_retained,
        } = collector;
        let RenderReunionCapabilityV0 {
            identity: reunion_identity,
            barrier: reunion_barrier,
            _one_shot: one_shot,
        } = reunion;
        drop(one_shot);
        if execution.identity != reunion_identity
            || collector_identity != reunion_identity
            || execution.barrier != reunion_barrier
            || collector_barrier != reunion_barrier
        {
            return Err(RuntimeErrorV0::Hold(
                "render split halves do not match their concrete fixed-point port".into(),
            ));
        }
        Ok(Self {
            transport: execution.transport,
            renderer: execution.renderer,
            descriptor: execution.descriptor,
            barrier: execution.barrier,
            fence_sequence: execution.fence_sequence,
            identity: execution.identity,
            root_namespace: execution.root_namespace,
            next_retained_incarnation: execution.next_retained_incarnation,
            // The newly established barrier belongs to this completed render
            // pass. The old collector is intentionally consumed here rather
            // than cloned or merged; any authenticated old change was
            // separately buffered and must supersede before a final fence.
            retained: execution.retained,
            #[cfg(test)]
            test_barrier: execution.test_barrier,
        })
    }

    #[cfg(test)]
    fn observation_only_for_test(barrier: BarrierV0) -> KubeFixedPointPortV0<'static, ()> {
        KubeFixedPointPortV0 {
            transport: None,
            renderer: (),
            descriptor: BundleDescriptorV0 {
                descriptor_version: "oxideloop.dev/descriptor/v0".into(),
                renderer_abi_version: "renderer.oxideloop.dev/v0".into(),
                render_module: oxideloop_bundle_contract::DescriptorModuleV0 {
                    identity: "render/main.mjs".into(),
                    digest: "a".repeat(64),
                },
                action_modules: vec![],
            },
            barrier,
            fence_sequence: 0,
            identity: next_fixed_point_port_identity(),
            root_namespace: None,
            next_retained_incarnation: 0,
            retained: None,
            test_barrier: None,
        }
    }

    /// Test-only construction still exercises the production retained-watch
    /// coordinator rather than allowing runtime tests to fabricate a genuine
    /// change capability directly.
    #[cfg(test)]
    pub(crate) fn retained_for_test(
        barrier: BarrierV0,
        observation: ObservationBarrierV0,
    ) -> KubeFixedPointPortV0<'static, ()> {
        let mut port = Self::observation_only_for_test(barrier);
        port.next_retained_incarnation = 1;
        port.retained = Some(RetainedObservationBarrierV0::with_incarnation(
            observation,
            1,
        ));
        port
    }

    #[cfg(test)]
    pub(crate) fn fixed_point_for_test(
        renderer: R,
        descriptor: BundleDescriptorV0,
        barrier: BarrierV0,
        observation: ObservationBarrierV0,
    ) -> Self {
        Self {
            transport: None,
            renderer,
            descriptor,
            barrier,
            fence_sequence: 0,
            identity: next_fixed_point_port_identity(),
            root_namespace: None,
            next_retained_incarnation: 0,
            retained: None,
            test_barrier: Some(observation),
        }
    }

    /// Wait for an authenticated retained-watch change, including all events
    /// buffered by sibling handles. This is the sole coordinator path for
    /// retained dependency events: it derives the global renderer token inside
    /// `RootActorV0`, suppresses duplicate final snapshots, and turns 410 or
    /// overflow into one epoch invalidation that drops every retained handle.
    pub async fn coordinate_retained_watch(
        &mut self,
        actor: &mut RootActorV0,
    ) -> Result<RetainedWatchCoordinationV0, RuntimeErrorV0> {
        if self.barrier != actor.barrier() {
            self.retained = None;
            self.barrier = actor.barrier();
            return Ok(RetainedWatchCoordinationV0::RestartRequired);
        }
        let record = self.await_buffered_retained_watch().await?;
        self.apply_buffered_retained_watch(actor, record)
    }

    /// Waits for a retained transition without borrowing the root actor.  A
    /// driver pins this future beside an owned effect and merely accumulates
    /// its result; dropping neither the effect nor actor authority is needed
    /// to observe a concurrent dependency transition.
    pub(crate) async fn await_buffered_retained_watch(
        &mut self,
    ) -> Result<BufferedRetainedWatchV0, RuntimeErrorV0> {
        if self
            .retained
            .as_ref()
            .is_none_or(|retained| retained.barrier.generation != self.barrier)
        {
            return Ok(BufferedRetainedWatchV0::RestartRequired {
                port_identity: self.identity,
                barrier: self.barrier,
            });
        }
        let retained = self.retained.as_mut().expect("checked retained barrier");
        let changed = match retained.drain_buffered_events() {
            Ok(changed) => changed,
            Err(RuntimeErrorV0::Discovery(
                oxideloop_kube_transport::TransportErrorV0::WatchGone
                | oxideloop_kube_transport::TransportErrorV0::WatchOverflow,
            )) => {
                return Ok(BufferedRetainedWatchV0::RestartRequired {
                    port_identity: self.identity,
                    barrier: self.barrier,
                });
            }
            Err(error) => return Err(error),
        };
        if !changed {
            match retained.await_change().await {
                Ok(()) => {}
                Err(RuntimeErrorV0::Discovery(
                    oxideloop_kube_transport::TransportErrorV0::WatchGone
                    | oxideloop_kube_transport::TransportErrorV0::WatchOverflow,
                )) => {
                    return Ok(BufferedRetainedWatchV0::RestartRequired {
                        port_identity: self.identity,
                        barrier: self.barrier,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(BufferedRetainedWatchV0::Change {
            port_identity: self.identity,
            barrier: self.barrier,
            change: GenuineChangeV0::from_observations(
                &retained.observations(),
                retained.barrier.generation,
                retained.incarnation,
            )?,
        })
    }

    /// Applies one retained record only after an owned command has completed.
    /// Mismatched port identity, barrier, or incarnation is stale observation
    /// data and therefore restarts/drops observation authority without
    /// releasing or reconstructing any effect.
    pub(crate) fn apply_buffered_retained_watch(
        &mut self,
        actor: &mut RootActorV0,
        record: BufferedRetainedWatchV0,
    ) -> Result<RetainedWatchCoordinationV0, RuntimeErrorV0> {
        let (port_identity, record_barrier) = match &record {
            BufferedRetainedWatchV0::Change {
                port_identity,
                barrier,
                ..
            }
            | BufferedRetainedWatchV0::RestartRequired {
                port_identity,
                barrier,
            } => (*port_identity, *barrier),
        };
        if port_identity != self.identity
            || record_barrier != self.barrier
            || self.barrier != actor.barrier()
        {
            self.retained = None;
            self.barrier = actor.barrier();
            return Ok(RetainedWatchCoordinationV0::RestartRequired);
        }
        let outcome = match record {
            BufferedRetainedWatchV0::Change { change, .. } => {
                coordinate_retained_watch_result(actor, self.barrier, Ok(change))?
            }
            BufferedRetainedWatchV0::RestartRequired { .. } => {
                actor.watch_invalidated();
                RetainedWatchCoordinationV0::RestartRequired
            }
        };
        if outcome == RetainedWatchCoordinationV0::RestartRequired
            || self.barrier != actor.barrier()
        {
            self.retained = None;
            self.barrier = actor.barrier();
        }
        Ok(outcome)
    }

    /// Installs only the final input returned by this reunited concrete port.
    /// The actor verifies its sealed provenance before replacing the
    /// speculative input it pinned when the render capability was issued.
    pub(crate) fn bind_completed_fixed_point_input(
        &self,
        actor: &mut RootActorV0,
        result: &FixedPointResultV0,
    ) -> Result<(), RuntimeErrorV0> {
        let retained = self.retained.as_ref().ok_or_else(|| {
            RuntimeErrorV0::Hold(
                "completed fixed-point input has no retained observation barrier".into(),
            )
        })?;
        if self.barrier != actor.barrier() || retained.barrier.generation != self.barrier {
            return Err(RuntimeErrorV0::Hold(
                "completed fixed-point input retained barrier is not current".into(),
            ));
        }
        let provenance = result.provenance.as_ref().ok_or_else(|| {
            RuntimeErrorV0::Hold(
                "completed fixed-point input has no sealed provenance capability".into(),
            )
        })?;
        if retained.coverage_dependencies != provenance.coverage_dependencies {
            return Err(RuntimeErrorV0::Hold(
                "completed fixed-point input retained coverage does not match sealed provenance"
                    .into(),
            ));
        }
        actor.bind_completed_fixed_point_input(
            result,
            self.identity,
            self.barrier,
            retained.incarnation,
        )
    }

    #[must_use]
    pub fn has_retained_observation_barrier(&self) -> bool {
        self.retained.is_some()
    }

    /// Completes the final, retained dependency snapshot into one unique
    /// fence.  Every desired identity and durable inventory member must be in
    /// the normalized final dependency coverage; an omitted watch therefore
    /// cannot be mistaken for an absent desired resource.
    pub fn complete_observation_fence(
        &mut self,
        actor: &mut RootActorV0,
        final_result: &mut FixedPointResultV0,
    ) -> Result<CompletedObservationFenceOutcomeV0, RuntimeErrorV0> {
        // A port that was superseded before completion cannot consume a stale
        // render result. Drop all retained handles and restart from the
        // actor's current barrier before touching the one-shot capability.
        if self.barrier != actor.barrier() {
            self.retained = None;
            self.barrier = actor.barrier();
            return Ok(CompletedObservationFenceOutcomeV0::RestartRequired);
        }
        let provenance = final_result.provenance.take().ok_or_else(|| {
            RuntimeErrorV0::Hold(
                "completed observation fence requires a sealed fixed-point provenance capability"
                    .into(),
            )
        })?;
        let retained = self.retained.as_mut().ok_or_else(|| {
            RuntimeErrorV0::Hold("no retained dependency barrier is available".into())
        })?;
        if retained.barrier.generation != actor.barrier() {
            self.retained = None;
            self.barrier = actor.barrier();
            return Ok(CompletedObservationFenceOutcomeV0::RestartRequired);
        }
        let session = actor.completed_observation_session(
            final_result,
            retained.barrier.generation,
            retained.incarnation,
            self.identity,
            &provenance,
        )?;
        let changed = retained.drain_buffered_events();
        match changed {
            Ok(true) => {
                let change = GenuineChangeV0::from_observations(
                    &retained.observations(),
                    retained.barrier.generation,
                    retained.incarnation,
                )?;
                let outcome = coordinate_retained_watch_result(actor, self.barrier, Ok(change))?;
                if outcome == RetainedWatchCoordinationV0::RestartRequired {
                    self.retained = None;
                    self.barrier = actor.barrier();
                    Ok(CompletedObservationFenceOutcomeV0::RestartRequired)
                } else if outcome == RetainedWatchCoordinationV0::ReconcileScheduled {
                    Ok(CompletedObservationFenceOutcomeV0::ReconcileScheduled)
                } else {
                    Ok(CompletedObservationFenceOutcomeV0::DuplicateIgnored)
                }
            }
            Ok(false) => {
                if final_result.input.observations != retained.renderer_observations() {
                    return Err(RuntimeErrorV0::Hold(
                        "final fixed-point input observations differ from the drained retained snapshot"
                            .into(),
                    ));
                }
                let fence =
                    retained.complete_observation_fence(&mut self.fence_sequence, &session)?;
                let (preconditions, request_id) = provenance.request();
                actor
                    .validated_render_observation(final_result, preconditions, request_id, fence)
                    .map(|capability| {
                        CompletedObservationFenceOutcomeV0::Completed(Box::new(capability))
                    })
            }
            Err(RuntimeErrorV0::Discovery(
                oxideloop_kube_transport::TransportErrorV0::WatchGone
                | oxideloop_kube_transport::TransportErrorV0::WatchOverflow,
            )) => {
                actor.watch_invalidated();
                self.retained = None;
                self.barrier = actor.barrier();
                Ok(CompletedObservationFenceOutcomeV0::RestartRequired)
            }
            Err(error) => Err(error),
        }
    }
}

/// Test-only convenience that exercises the exact production completion
/// authority: issue an actor capability, split the concrete port, execute,
/// drain the old collector, reunite, consume a sealed coverage change if one
/// was minted, and bind only a reunited stable result.  Tests deliberately do
/// not have a second direct reconciliation implementation.
#[cfg(test)]
pub(crate) async fn reconcile_split_for_test<'a, R: RendererPortV0>(
    actor: &mut RootActorV0,
    port: KubeFixedPointPortV0<'a, R>,
    input: RenderInputV0,
) -> Result<(KubeFixedPointPortV0<'a, R>, FixedPointResultV0), RuntimeErrorV0> {
    let capability = actor.issue_render_capability(input, port.identity())?;
    let (mut execution, mut collector, reunion) = port.split_for_render();
    let outcome = execution.reconcile_for_capability(capability).await;
    let buffered = collector.drain_ready_retained_watch()?;
    let mut port = KubeFixedPointPortV0::reunite_after_render(execution, collector, reunion)?;
    if let Some(record) = buffered {
        port.apply_buffered_retained_watch(actor, record)?;
        if !actor.owns_render_request() {
            return Err(RuntimeErrorV0::Hold(
                "test split render was superseded by a retained watch".into(),
            ));
        }
    }
    match outcome {
        Ok(RenderExecutionOutcomeV0::Stable(result)) => {
            let result = *result;
            port.bind_completed_fixed_point_input(actor, &result)?;
            Ok((port, result))
        }
        Ok(RenderExecutionOutcomeV0::CoverageChanged(change)) => {
            port.consume_coverage_change(actor, change)?;
            Err(RuntimeErrorV0::Hold(
                "final coverage changed the renderer dependency projection".into(),
            ))
        }
        Err(RuntimeErrorV0::Discovery(
            error @ (oxideloop_kube_transport::TransportErrorV0::WatchGone
            | oxideloop_kube_transport::TransportErrorV0::WatchOverflow),
        )) => {
            actor.watch_invalidated();
            // The reunited port owns no current fixed-point result after a
            // terminal establishment fault, so drop its retained authority.
            port.retained = None;
            port.barrier = actor.barrier();
            Err(RuntimeErrorV0::Discovery(error))
        }
        Err(error) => Err(error),
    }
}

fn coordinate_retained_watch_result(
    actor: &mut RootActorV0,
    port_barrier: BarrierV0,
    change: Result<GenuineChangeV0, RuntimeErrorV0>,
) -> Result<RetainedWatchCoordinationV0, RuntimeErrorV0> {
    match change {
        Ok(change) if change.barrier != port_barrier || change.barrier != actor.barrier() => {
            Ok(RetainedWatchCoordinationV0::RestartRequired)
        }
        Ok(change) => Ok(if actor.record_genuine_dependency_change(&change) {
            RetainedWatchCoordinationV0::ReconcileScheduled
        } else {
            RetainedWatchCoordinationV0::DuplicateIgnored
        }),
        Err(RuntimeErrorV0::Discovery(
            oxideloop_kube_transport::TransportErrorV0::WatchGone
            | oxideloop_kube_transport::TransportErrorV0::WatchOverflow,
        )) => {
            actor.watch_invalidated();
            Ok(RetainedWatchCoordinationV0::RestartRequired)
        }
        Err(error) => Err(error),
    }
}

#[async_trait]
impl<R: RendererPortV0> FixedPointPortV0 for KubeFixedPointPortV0<'_, R> {
    async fn render(&mut self, input: &RenderInputV0) -> Result<RenderOutputV0, RuntimeErrorV0> {
        let payload = canonical_render_input(input)?;
        let hash = oxideloop_renderer_abi::sha256_hex(&payload);
        let request = renderer_request(
            PhaseV0::Render,
            deterministic_request_id(&hash)?,
            &input.bundle_digest,
            Some(self.descriptor.render_module.digest.clone()),
            payload,
        )?;
        let response = self.renderer.call(&request).await?;
        // Keep the parser/correlation rule identical to the direct client path.
        let payload = crate::renderer_payload(&response, &request)?;
        let text = std::str::from_utf8(&payload).map_err(|_| RuntimeErrorV0::RendererAbi)?;
        Ok(oxideloop_renderer_abi::parse_validate_render_output(text)
            .map_err(|_| RuntimeErrorV0::RendererAbi)?
            .document)
    }

    async fn observe_after_barrier(
        &mut self,
        dependencies: &[oxideloop_renderer_abi::model::Dependency],
    ) -> Result<Vec<Observation>, RuntimeErrorV0> {
        let namespace = self.root_namespace.as_deref().ok_or_else(|| {
            RuntimeErrorV0::Hold(
                "renderer dependency plan lacks an authorized root namespace".into(),
            )
        })?;
        let dependencies = renderer_dependency_plan(dependencies, namespace)?;
        #[cfg(test)]
        let barrier = if let Some(barrier) = self.test_barrier.take() {
            if dependencies != barrier.dependencies {
                return Err(RuntimeErrorV0::Hold(
                    "test fixed-point barrier dependencies do not match render output".into(),
                ));
            }
            barrier
        } else {
            let transport = self.transport.ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "test-only fixed-point port cannot establish dependencies".into(),
                )
            })?;
            establish_dependencies_async(transport, &dependencies, self.barrier).await?
        };
        #[cfg(not(test))]
        let barrier = {
            let transport = self.transport.ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "test-only fixed-point port cannot establish dependencies".into(),
                )
            })?;
            establish_dependencies_async(transport, &dependencies, self.barrier).await?
        };
        self.next_retained_incarnation = self
            .next_retained_incarnation
            .checked_add(1)
            .ok_or_else(|| RuntimeErrorV0::Hold("retained barrier incarnation overflow".into()))?;
        let mut retained =
            RetainedObservationBarrierV0::with_incarnation(barrier, self.next_retained_incarnation);
        retained.drain_buffered_events()?;
        let observations = retained.renderer_observations();
        // The fixed-point loop returns immediately after its stable render.
        // Keep these handles rather than dropping them at that boundary so a
        // later dependency event can trigger the next reconciliation.
        self.retained = Some(retained);
        Ok(observations)
    }

    fn watch_invalidated(&mut self, barrier: BarrierV0) {
        // A terminal watch error makes every snapshot from this barrier stale.
        // Dropping the handles is intentional; the restart establishes a fresh
        // list/watch/reread barrier at the actor's new epoch.
        self.retained = None;
        self.barrier = barrier;
    }

    fn set_barrier(&mut self, barrier: BarrierV0) {
        if self.barrier != barrier {
            self.retained = None;
        }
        self.barrier = barrier;
    }
}

impl<R> KubeFixedPointPortV0<'_, R> {
    /// Consumes the only coverage-change authority minted by this concrete
    /// port.  Split execution cannot route a raw fingerprint into the actor:
    /// reunion must prove the port, barrier, retained U incarnation, and
    /// current D-only projection still match before supersession.
    pub(crate) fn consume_coverage_change(
        &mut self,
        actor: &mut RootActorV0,
        outcome: CoverageChangedV0,
    ) -> Result<RetainedWatchCoordinationV0, RuntimeErrorV0> {
        let Some(retained) = self.retained.as_ref() else {
            return Err(RuntimeErrorV0::Hold(
                "coverage-change outcome has no current retained union barrier".into(),
            ));
        };
        let expected = GenuineChangeV0::from_observations(
            &retained.renderer_observations(),
            self.barrier,
            retained.incarnation,
        )?;
        if outcome.port_identity != self.identity
            || outcome.barrier != self.barrier
            || self.barrier != actor.barrier()
            || outcome.retained_incarnation != retained.incarnation
            || outcome.change != expected
        {
            return Err(RuntimeErrorV0::Hold(
                "coverage-change outcome does not belong to the current retained union barrier"
                    .into(),
            ));
        }
        let result = coordinate_retained_watch_result(actor, self.barrier, Ok(outcome.change))?;
        if result == RetainedWatchCoordinationV0::RestartRequired || self.barrier != actor.barrier()
        {
            self.retained = None;
            self.barrier = actor.barrier();
        }
        Ok(result)
    }

    /// Drains records already queued by the retained watches without waiting.
    /// This is the completion-edge counterpart to the asynchronous collector.
    pub(crate) fn drain_ready_retained_watch(
        &mut self,
    ) -> Result<Option<BufferedRetainedWatchV0>, RuntimeErrorV0> {
        let Some(retained) = self.retained.as_mut() else {
            return Ok(None);
        };
        if retained.barrier.generation != self.barrier {
            return Ok(Some(BufferedRetainedWatchV0::RestartRequired {
                port_identity: self.identity,
                barrier: self.barrier,
            }));
        }
        match retained.drain_buffered_events() {
            Ok(false) => Ok(None),
            Ok(true) => Ok(Some(BufferedRetainedWatchV0::Change {
                port_identity: self.identity,
                barrier: self.barrier,
                change: GenuineChangeV0::from_observations(
                    &retained.observations(),
                    retained.barrier.generation,
                    retained.incarnation,
                )?,
            })),
            Err(RuntimeErrorV0::Discovery(
                oxideloop_kube_transport::TransportErrorV0::WatchGone
                | oxideloop_kube_transport::TransportErrorV0::WatchOverflow,
            )) => Ok(Some(BufferedRetainedWatchV0::RestartRequired {
                port_identity: self.identity,
                barrier: self.barrier,
            })),
            Err(error) => Err(error),
        }
    }
}

impl<R> RenderExecutionPortV0<'_, R> {
    /// Executes a render/discovery pass from the actor-issued capability.  It
    /// has no actor reference; only the matching reunited concrete port can
    /// later turn the returned provenance into a final fence.
    pub(crate) async fn reconcile_for_capability(
        &mut self,
        capability: RenderCommandCapabilityV0,
    ) -> Result<RenderExecutionOutcomeV0, RuntimeErrorV0>
    where
        R: RendererPortV0,
    {
        if capability.port_identity() != self.identity {
            return Err(RuntimeErrorV0::Hold(
                "render capability port identity mismatch".into(),
            ));
        }
        self.root_namespace = Some(capability.root_namespace().into());
        let mut result = reconcile_fixed_point(self, capability.input().clone()).await?;
        let (coverage, coverage_hash) = capability.coverage_plan(result.output())?;
        self.replace_with_union(&result, coverage.clone()).await?;
        let retained = self.retained.as_ref().ok_or_else(|| {
            RuntimeErrorV0::Hold("fixed-point result has no retained observation barrier".into())
        })?;
        if retained.barrier.generation != self.barrier {
            return Err(RuntimeErrorV0::Hold(
                "render capability retained barrier changed".into(),
            ));
        }
        if result.input.observations != retained.renderer_observations() {
            return Ok(RenderExecutionOutcomeV0::CoverageChanged(
                CoverageChangedV0::new(
                    self.identity,
                    self.barrier,
                    retained.incarnation,
                    GenuineChangeV0::from_observations(
                        &retained.renderer_observations(),
                        self.barrier,
                        retained.incarnation,
                    )?,
                ),
            ));
        }
        result.provenance =
            Some(capability.complete(retained.incarnation, coverage, coverage_hash));
        Ok(RenderExecutionOutcomeV0::Stable(Box::new(result)))
    }

    async fn replace_with_union(
        &mut self,
        result: &FixedPointResultV0,
        coverage: Vec<Dependency>,
    ) -> Result<(), RuntimeErrorV0> {
        if self.transport.is_none() {
            let retained = self.retained.as_mut().ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "fixed-point result has no retained observation barrier".into(),
                )
            })?;
            let renderer_dependencies = normalize_dependencies(result.dependencies())?;
            let coverage_dependencies = normalize_dependencies(&coverage)?;
            let mut union = renderer_dependencies.clone();
            union.extend(coverage_dependencies.clone());
            retained.barrier.dependencies = normalize_dependencies(&union)?;
            retained.renderer_dependencies = renderer_dependencies;
            retained.coverage_dependencies = coverage_dependencies;
            return Ok(());
        }
        let transport = self.transport.expect("none handled above");
        self.next_retained_incarnation = self
            .next_retained_incarnation
            .checked_add(1)
            .ok_or_else(|| RuntimeErrorV0::Hold("retained barrier incarnation overflow".into()))?;
        self.retained = Some(
            establish_union_retained(
                transport,
                result.dependencies(),
                coverage,
                self.barrier,
                self.next_retained_incarnation,
            )
            .await?,
        );
        Ok(())
    }
}

async fn establish_union_retained(
    transport: &KubeTransportV0,
    renderer_dependencies: &[Dependency],
    coverage_dependencies: Vec<Dependency>,
    barrier: BarrierV0,
    incarnation: u64,
) -> Result<RetainedObservationBarrierV0, RuntimeErrorV0> {
    let renderer_dependencies = normalize_dependencies(renderer_dependencies)?;
    let coverage_dependencies = normalize_dependencies(&coverage_dependencies)?;
    let mut union = renderer_dependencies.clone();
    union.extend(coverage_dependencies.clone());
    let union = normalize_dependencies(&union)?;
    let barrier = establish_dependencies_async(transport, &union, barrier).await?;
    let mut retained = RetainedObservationBarrierV0::with_union(
        barrier,
        incarnation,
        renderer_dependencies,
        coverage_dependencies,
    );
    // Any ready event after the reread belongs to this new coverage barrier.
    retained.drain_buffered_events()?;
    Ok(retained)
}

#[async_trait]
impl<R: RendererPortV0> FixedPointPortV0 for RenderExecutionPortV0<'_, R> {
    async fn render(&mut self, input: &RenderInputV0) -> Result<RenderOutputV0, RuntimeErrorV0> {
        let payload = canonical_render_input(input)?;
        let hash = oxideloop_renderer_abi::sha256_hex(&payload);
        let request = renderer_request(
            PhaseV0::Render,
            deterministic_request_id(&hash)?,
            &input.bundle_digest,
            Some(self.descriptor.render_module.digest.clone()),
            payload,
        )?;
        let response = self.renderer.call(&request).await?;
        let payload = crate::renderer_payload(&response, &request)?;
        let text = std::str::from_utf8(&payload).map_err(|_| RuntimeErrorV0::RendererAbi)?;
        Ok(oxideloop_renderer_abi::parse_validate_render_output(text)
            .map_err(|_| RuntimeErrorV0::RendererAbi)?
            .document)
    }

    async fn observe_after_barrier(
        &mut self,
        dependencies: &[oxideloop_renderer_abi::model::Dependency],
    ) -> Result<Vec<Observation>, RuntimeErrorV0> {
        let namespace = self.root_namespace.as_deref().ok_or_else(|| {
            RuntimeErrorV0::Hold(
                "renderer dependency plan lacks an authorized root namespace".into(),
            )
        })?;
        let dependencies = renderer_dependency_plan(dependencies, namespace)?;
        #[cfg(test)]
        let barrier = if let Some(barrier) = self.test_barrier.take() {
            if dependencies != barrier.dependencies {
                return Err(RuntimeErrorV0::Hold(
                    "test fixed-point barrier dependencies do not match render output".into(),
                ));
            }
            barrier
        } else {
            let transport = self.transport.ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "test-only fixed-point port cannot establish dependencies".into(),
                )
            })?;
            establish_dependencies_async(transport, &dependencies, self.barrier).await?
        };
        #[cfg(not(test))]
        let barrier = {
            let transport = self.transport.ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "test-only fixed-point port cannot establish dependencies".into(),
                )
            })?;
            establish_dependencies_async(transport, &dependencies, self.barrier).await?
        };
        self.next_retained_incarnation = self
            .next_retained_incarnation
            .checked_add(1)
            .ok_or_else(|| RuntimeErrorV0::Hold("retained barrier incarnation overflow".into()))?;
        let mut retained =
            RetainedObservationBarrierV0::with_incarnation(barrier, self.next_retained_incarnation);
        retained.drain_buffered_events()?;
        let observations = retained.renderer_observations();
        self.retained = Some(retained);
        Ok(observations)
    }

    fn watch_invalidated(&mut self, barrier: BarrierV0) {
        self.retained = None;
        self.barrier = barrier;
    }

    fn set_barrier(&mut self, barrier: BarrierV0) {
        if self.barrier != barrier {
            self.retained = None;
        }
        self.barrier = barrier;
    }
}

impl RetainedWatchCollectorV0 {
    /// Drains only records already available from the split port's old
    /// retained barrier.  The driver uses this at the command-completion edge
    /// before it reunites the execution half, so a simultaneously-ready old
    /// watch can never be discarded as though the command were still current.
    pub(crate) fn drain_ready_retained_watch(
        &mut self,
    ) -> Result<Option<BufferedRetainedWatchV0>, RuntimeErrorV0> {
        let Some(retained) = self.retained.as_mut() else {
            return Ok(None);
        };
        if retained.barrier.generation != self.barrier {
            return Ok(Some(BufferedRetainedWatchV0::RestartRequired {
                port_identity: self.identity,
                barrier: self.barrier,
            }));
        }
        let changed = match retained.drain_buffered_events() {
            Ok(changed) => changed,
            Err(RuntimeErrorV0::Discovery(
                oxideloop_kube_transport::TransportErrorV0::WatchGone
                | oxideloop_kube_transport::TransportErrorV0::WatchOverflow,
            )) => {
                return Ok(Some(BufferedRetainedWatchV0::RestartRequired {
                    port_identity: self.identity,
                    barrier: self.barrier,
                }));
            }
            Err(error) => return Err(error),
        };
        if !changed {
            return Ok(None);
        }
        Ok(Some(BufferedRetainedWatchV0::Change {
            port_identity: self.identity,
            barrier: self.barrier,
            change: GenuineChangeV0::from_observations(
                &retained.observations(),
                retained.barrier.generation,
                retained.incarnation,
            )?,
        }))
    }

    /// Receives exactly one sealed old-barrier dependency record without an
    /// actor borrow.  A render with no prior stable dependencies has nothing
    /// to collect, so it remains pending rather than manufacturing a restart.
    pub(crate) async fn await_buffered_retained_watch(
        &mut self,
    ) -> Result<BufferedRetainedWatchV0, RuntimeErrorV0> {
        if let Some(record) = self.drain_ready_retained_watch()? {
            return Ok(record);
        }
        let Some(retained) = self.retained.as_mut() else {
            return std::future::pending().await;
        };
        match retained.await_change().await {
            Ok(()) => {}
            Err(RuntimeErrorV0::Discovery(
                oxideloop_kube_transport::TransportErrorV0::WatchGone
                | oxideloop_kube_transport::TransportErrorV0::WatchOverflow,
            )) => {
                return Ok(BufferedRetainedWatchV0::RestartRequired {
                    port_identity: self.identity,
                    barrier: self.barrier,
                });
            }
            Err(error) => return Err(error),
        }
        self.drain_ready_retained_watch()?.ok_or_else(|| {
            RuntimeErrorV0::Hold(
                "retained watch woke without producing a retained observation result".into(),
            )
        })
    }
}

type ObservationKeyV0 = (ResourceKindV0, String, String);

enum CoalescedWatchStateV0 {
    Present(ObjectV0),
    Tombstone(ObjectV0),
}

/// The stable barrier's snapshot plus all watch changes seen since its reread.
/// A delete stays a tombstone (JSON null) rather than resurrecting the deleted
/// object from the list snapshot. `BTreeMap` makes cross-dependency output
/// ordering deterministic while the last event per object owns its RV.
struct RetainedObservationBarrierV0 {
    barrier: ObservationBarrierV0,
    incarnation: u64,
    latest: BTreeMap<ObservationKeyV0, CoalescedWatchStateV0>,
    renderer_dependencies: Vec<Dependency>,
    coverage_dependencies: Vec<Dependency>,
}

impl RetainedObservationBarrierV0 {
    #[cfg(test)]
    fn new(barrier: ObservationBarrierV0) -> Self {
        Self::with_incarnation(barrier, 0)
    }

    fn with_incarnation(barrier: ObservationBarrierV0, incarnation: u64) -> Self {
        let renderer_dependencies = barrier.dependencies.clone();
        Self {
            barrier,
            incarnation,
            latest: BTreeMap::new(),
            renderer_dependencies,
            coverage_dependencies: Vec::new(),
        }
    }

    fn with_union(
        barrier: ObservationBarrierV0,
        incarnation: u64,
        renderer_dependencies: Vec<Dependency>,
        coverage_dependencies: Vec<Dependency>,
    ) -> Self {
        Self {
            barrier,
            incarnation,
            latest: BTreeMap::new(),
            renderer_dependencies,
            coverage_dependencies,
        }
    }

    fn drain_buffered_events(&mut self) -> Result<bool, RuntimeErrorV0> {
        let mut buffered = Vec::new();
        for watch in &mut self.barrier.watches {
            while let Some(event) = watch.try_next_event().map_err(RuntimeErrorV0::Discovery)? {
                buffered.push(event);
            }
        }
        let mut changed = false;
        for event in buffered {
            changed = self.record(event) || changed;
        }
        Ok(changed)
    }

    async fn await_change(&mut self) -> Result<(), RuntimeErrorV0> {
        if self.barrier.watches.is_empty() {
            return Err(RuntimeErrorV0::Hold(
                "retained dependency barrier has no watch handles".into(),
            ));
        }
        loop {
            let pending = self
                .barrier
                .watches
                .iter_mut()
                .enumerate()
                .map(|(index, watch)| Box::pin(async move { (index, watch.next_event().await) }))
                .collect::<Vec<_>>();
            let ((_, event), _, _) = select_all(pending).await;
            let mut changed = self.record(event.map_err(RuntimeErrorV0::Discovery)?);
            changed |= self.drain_buffered_events()?;
            if changed {
                return Ok(());
            }
        }
    }

    fn record(&mut self, event: WatchEventV0) -> bool {
        let reference = match &event {
            WatchEventV0::Added(object)
            | WatchEventV0::Modified(object)
            | WatchEventV0::Deleted(object) => &object.reference,
            WatchEventV0::Bookmark { .. } => return false,
        };
        if !self.tracks(reference) {
            return false;
        }
        match event {
            WatchEventV0::Added(object) | WatchEventV0::Modified(object) => {
                let key = observation_key(&object.reference);
                if self.present_identity(&key).is_some_and(|(uid, rv)| {
                    uid == object.metadata.uid && rv == object.metadata.resource_version
                }) {
                    return false;
                }
                self.latest
                    .insert(key, CoalescedWatchStateV0::Present(object));
                true
            }
            WatchEventV0::Deleted(object) => {
                let key = observation_key(&object.reference);
                // Kubernetes watch streams may deliver a stale delete after a
                // replacement with the same name has been observed. A
                // tombstone has authority only over the currently retained
                // UID; unknown/unpinned deletes are intentionally ignored so
                // they cannot erase a newer object from the final snapshot.
                let Some((present_uid, _)) = self.present_identity(&key) else {
                    return false;
                };
                if present_uid != object.metadata.uid {
                    return false;
                }
                if self.latest.get(&key).is_some_and(|state| {
                    matches!(
                        state,
                        CoalescedWatchStateV0::Tombstone(existing)
                            if existing.metadata.uid == object.metadata.uid
                                && existing.metadata.resource_version
                                    == object.metadata.resource_version
                    )
                }) {
                    return false;
                }
                self.latest
                    .insert(key, CoalescedWatchStateV0::Tombstone(object));
                true
            }
            WatchEventV0::Bookmark { .. } => false,
        }
    }

    fn present_identity(&self, key: &ObservationKeyV0) -> Option<(String, String)> {
        if let Some(state) = self.latest.get(key) {
            return match state {
                CoalescedWatchStateV0::Present(object) => Some((
                    object.metadata.uid.clone(),
                    object.metadata.resource_version.clone(),
                )),
                CoalescedWatchStateV0::Tombstone(_) => None,
            };
        }
        let reference = reference_from_observation_key(key);
        if let Some(object) = self
            .barrier
            .objects
            .iter()
            .find_map(|(candidate, object)| (candidate == &reference).then_some(object.as_ref()))
            .flatten()
        {
            return Some((
                object.metadata.uid.clone(),
                object.metadata.resource_version.clone(),
            ));
        }
        self.barrier
            .collections
            .values()
            .flat_map(|list| &list.items)
            .find(|object| observation_key(&object.reference) == *key)
            .map(|object| {
                (
                    object.metadata.uid.clone(),
                    object.metadata.resource_version.clone(),
                )
            })
    }

    fn observations(&self) -> Vec<Observation> {
        let mut observations = self
            .final_objects()
            .into_iter()
            .map(|(key, object)| match object {
                Some(object) => renderer_observation(object),
                // Tombstone metadata stays inside the retained barrier for
                // UID-correlated dedupe only; the canonical final object map
                // makes every absence an unpinned JSON null for the renderer.
                None => absent_observation(&reference_from_observation_key(&key)),
            })
            .collect::<Vec<_>>();
        observations.sort_by(observation_sort_key);
        observations
    }

    fn renderer_observations(&self) -> Vec<Observation> {
        let mut observations = self
            .renderer_final_objects()
            .into_iter()
            .map(|(key, object)| match object {
                Some(object) => renderer_observation(object),
                None => absent_observation(&reference_from_observation_key(&key)),
            })
            .collect::<Vec<_>>();
        observations.sort_by(observation_sort_key);
        observations
    }

    fn dependencies_cover(dependencies: &[Dependency], reference: &ObjectRefV0) -> bool {
        dependencies.iter().any(|dependency| match dependency {
            Dependency::Collection {
                api_version,
                kind,
                namespace,
                ..
            } => {
                resource(api_version, kind).ok() == Some(reference.resource)
                    && namespace == &reference.namespace
            }
            Dependency::Object {
                api_version,
                kind,
                namespace,
                name,
                ..
            } => {
                resource(api_version, kind).ok() == Some(reference.resource)
                    && namespace == &reference.namespace
                    && name == &reference.name
            }
        })
    }

    fn tracks(&self, reference: &ObjectRefV0) -> bool {
        Self::dependencies_cover(&self.renderer_dependencies, reference)
            || Self::dependencies_cover(&self.coverage_dependencies, reference)
    }

    fn final_objects(&self) -> BTreeMap<ObservationKeyV0, Option<ObjectV0>> {
        let mut final_objects = BTreeMap::new();
        for list in self.barrier.collections.values() {
            for object in &list.items {
                final_objects.insert(observation_key(&object.reference), Some(object.clone()));
            }
        }
        for (reference, object) in &self.barrier.objects {
            final_objects.insert(observation_key(reference), object.clone());
        }
        for (key, state) in &self.latest {
            match state {
                CoalescedWatchStateV0::Present(object) => {
                    final_objects.insert(key.clone(), Some(object.clone()));
                }
                CoalescedWatchStateV0::Tombstone(_) => {
                    final_objects.insert(key.clone(), None);
                }
            }
        }
        final_objects
    }

    /// Reconstruct the renderer's D view without allowing C's exact
    /// desired/inventory reads to overwrite a collection member or invent an
    /// absent object. The union map remains authoritative only for fencing.
    fn renderer_final_objects(&self) -> BTreeMap<ObservationKeyV0, Option<ObjectV0>> {
        let mut objects = BTreeMap::new();
        for list in self.barrier.collections.values() {
            for object in &list.items {
                if Self::dependencies_cover(&self.renderer_dependencies, &object.reference) {
                    objects.insert(observation_key(&object.reference), Some(object.clone()));
                }
            }
        }
        for (reference, object) in &self.barrier.objects {
            if self.renderer_dependencies.iter().any(|dependency| {
                matches!(dependency,
                    Dependency::Object { api_version, kind, namespace, name, .. }
                        if resource(api_version, kind).ok() == Some(reference.resource)
                            && namespace == &reference.namespace && name == &reference.name
                )
            }) {
                objects.insert(observation_key(reference), object.clone());
            }
        }
        for (key, state) in &self.latest {
            let reference = reference_from_observation_key(key);
            if !Self::dependencies_cover(&self.renderer_dependencies, &reference) {
                continue;
            }
            objects.insert(
                key.clone(),
                match state {
                    CoalescedWatchStateV0::Present(object) => Some(object.clone()),
                    CoalescedWatchStateV0::Tombstone(_) => None,
                },
            );
        }
        objects
    }

    fn covers(&self, reference: &ObjectRefV0) -> bool {
        self.barrier
            .collections
            .contains_key(&(reference.resource, reference.namespace.clone()))
            || self
                .barrier
                .objects
                .iter()
                .any(|(object, _)| object == reference)
    }

    fn desired_observations(
        &self,
        root: &ObjectRefV0,
        root_uid: &str,
        desired_identities: &[ObjectIdentity],
        durable_inventory: &[InventoryEntry],
    ) -> Result<Vec<DesiredObservationV0>, RuntimeErrorV0> {
        let mut requested = BTreeMap::new();
        for identity in desired_identities
            .iter()
            .chain(durable_inventory.iter().map(|entry| &entry.identity))
        {
            let reference = object_reference(identity)?;
            requested.insert(observation_key(&reference), (identity.clone(), reference));
        }
        let final_objects = self.final_objects();
        let mut observations = Vec::with_capacity(requested.len());
        for (key, (identity, reference)) in requested {
            if !self.covers(&reference) {
                return Err(RuntimeErrorV0::Hold(
                    "completed observation fence omits a desired or inventory dependency".into(),
                ));
            }
            match final_objects.get(&key).and_then(Option::as_ref) {
                Some(object) => observations.push(desired_observation(root, root_uid, object)?),
                None => observations.push(absent_desired_observation(identity)?),
            }
        }
        observations.sort_by(|left, right| {
            (
                left.identity.api_version.as_str(),
                left.identity.kind.as_str(),
                left.identity.namespace.as_str(),
                left.identity.name.as_str(),
            )
                .cmp(&(
                    right.identity.api_version.as_str(),
                    right.identity.kind.as_str(),
                    right.identity.namespace.as_str(),
                    right.identity.name.as_str(),
                ))
        });
        Ok(observations)
    }

    /// The sole final-fence completion path. It consumes only a sealed session
    /// made by `RootActorV0` from a stable fixed-point result and that actor's
    /// durable state; callers cannot substitute a shorter desired/inventory
    /// set or choose already-used prune tokens.
    fn complete_observation_fence(
        &mut self,
        fence_sequence: &mut u64,
        session: &CompletedObservationSessionV0,
    ) -> Result<CompletedObservationFenceV0, RuntimeErrorV0> {
        if session.root.resource != ResourceKindV0::WebApplication
            || session.root.namespace.is_empty()
            || session.root.name.is_empty()
            || session.root_uid.is_empty()
        {
            return Err(RuntimeErrorV0::Hold(
                "completed observation fence root identity is invalid".into(),
            ));
        }
        if session.retained_barrier != self.barrier.generation
            || session.retained_incarnation != self.incarnation
        {
            return Err(RuntimeErrorV0::Hold(
                "completed observation session does not belong to this retained barrier incarnation"
                    .into(),
            ));
        }
        let renderer_dependencies = normalize_dependencies(&session.dependencies)?;
        let coverage_dependencies = normalize_dependencies(&session.coverage_dependencies)?;
        let mut requested_dependencies = renderer_dependencies.clone();
        requested_dependencies.extend(coverage_dependencies.clone());
        let dependencies = normalize_dependencies(&requested_dependencies)?;
        if renderer_dependencies != self.renderer_dependencies
            || coverage_dependencies != self.coverage_dependencies
            || dependencies != self.barrier.dependencies
        {
            return Err(RuntimeErrorV0::Hold(
                "completed observation fence dependencies do not match the retained barrier".into(),
            ));
        }
        let renderer_observations = self.renderer_observations();
        let desired_observations = self.desired_observations(
            &session.root,
            &session.root_uid,
            &session.desired_identities,
            &session.durable_inventory,
        )?;
        let (sequence, token) = loop {
            *fence_sequence = fence_sequence.checked_add(1).ok_or_else(|| {
                RuntimeErrorV0::Hold("completed observation fence sequence overflow".into())
            })?;
            let snapshot = serde_json::json!({
                "dependencies": dependencies,
                "rendererObservations": renderer_observations,
                "desiredObservations": desired_observations,
                "durableInventory": session.durable_inventory,
            });
            let token = completed_observation_fence_token(
                &session.root_uid,
                self.barrier.generation,
                *fence_sequence,
                &snapshot,
            );
            if !session
                .durable_prune_tokens
                .iter()
                .any(|used| used == &token)
            {
                break (*fence_sequence, token);
            }
        };
        Ok(CompletedObservationFenceV0 {
            token,
            barrier: self.barrier.generation,
            sequence,
            dependencies,
            renderer_observations,
            desired_observations,
        })
    }
}

fn renderer_observation(object: ObjectV0) -> Observation {
    let is_present = !object.value.is_null();
    Observation {
        api_version: object.reference.resource.api_version().into(),
        kind: object.reference.resource.kind().into(),
        namespace: object.reference.namespace,
        name: object.reference.name,
        uid: if is_present {
            object.metadata.uid
        } else {
            String::new()
        },
        resource_version: if is_present {
            object.metadata.resource_version
        } else {
            String::new()
        },
        value: object.value,
    }
}

fn observation_key(reference: &ObjectRefV0) -> ObservationKeyV0 {
    (
        reference.resource,
        reference.namespace.clone(),
        reference.name.clone(),
    )
}

fn reference_from_observation_key(key: &ObservationKeyV0) -> ObjectRefV0 {
    ObjectRefV0 {
        resource: key.0,
        namespace: key.1.clone(),
        name: key.2.clone(),
    }
}

fn absent_observation(reference: &ObjectRefV0) -> Observation {
    Observation {
        api_version: reference.resource.api_version().into(),
        kind: reference.resource.kind().into(),
        namespace: reference.namespace.clone(),
        name: reference.name.clone(),
        uid: String::new(),
        resource_version: String::new(),
        value: serde_json::Value::Null,
    }
}

fn observation_sort_key(left: &Observation, right: &Observation) -> std::cmp::Ordering {
    (&left.api_version, &left.kind, &left.namespace, &left.name).cmp(&(
        &right.api_version,
        &right.kind,
        &right.namespace,
        &right.name,
    ))
}

fn object_reference(identity: &ObjectIdentity) -> Result<ObjectRefV0, RuntimeErrorV0> {
    let resource = match (identity.api_version.as_str(), identity.kind.as_str()) {
        ("v1", "ConfigMap") => ResourceKindV0::ConfigMap,
        ("v1", "Service") => ResourceKindV0::Service,
        _ => {
            return Err(RuntimeErrorV0::Hold(
                "completed observation fence contains an unsupported desired identity".into(),
            ));
        }
    };
    if identity.namespace.is_empty() || identity.name.is_empty() {
        return Err(RuntimeErrorV0::Hold(
            "completed observation fence desired identity is invalid".into(),
        ));
    }
    Ok(ObjectRefV0 {
        resource,
        namespace: identity.namespace.clone(),
        name: identity.name.clone(),
    })
}

fn absent_desired_observation(
    identity: ObjectIdentity,
) -> Result<DesiredObservationV0, RuntimeErrorV0> {
    let gvr = match (identity.api_version.as_str(), identity.kind.as_str()) {
        ("v1", "ConfigMap") => "v1/configmaps",
        ("v1", "Service") => "v1/services",
        _ => {
            return Err(RuntimeErrorV0::Hold(
                "unsupported absent desired resource".into(),
            ));
        }
    };
    Ok(DesiredObservationV0 {
        identity,
        observation: CoreObservation::Absent,
        qualification: BackendQualification::BuiltInTested {
            kubernetes_minor: "1.34".into(),
            gvr: gvr.into(),
        },
        recovery_proof: None,
        deletion_recovery_proof: None,
    })
}

/// Converts exact Kubernetes observations for reducer reconciliation. Only
/// built-in ConfigMap/Service objects qualify at Kubernetes v1.34. Recovery
/// proof is issued only for one exact controller owner reference plus all
/// proof annotations; a same-name foreign object therefore has no adoption
/// path.
pub fn desired_observation(
    root: &ObjectRefV0,
    root_uid: &str,
    object: &ObjectV0,
) -> Result<DesiredObservationV0, RuntimeErrorV0> {
    if root.resource != ResourceKindV0::WebApplication
        || root.namespace.is_empty()
        || root.name.is_empty()
        || root_uid.is_empty()
    {
        return Err(RuntimeErrorV0::Hold(
            "root ownership identity is invalid".into(),
        ));
    }
    let gvr = match object.reference.resource {
        ResourceKindV0::ConfigMap => "v1/configmaps",
        ResourceKindV0::Service => "v1/services",
        _ => return Err(RuntimeErrorV0::Hold("unsupported observed resource".into())),
    };
    let recovery_proof = match exact_recovery_annotations(object, root_uid) {
        Some((transaction_id, command_id)) if exact_controller_owner(object, root, root_uid) => {
            Some(RecoveryProofV0 {
                root_uid: root_uid.into(),
                transaction_id,
                command_id,
                uid: object.metadata.uid.clone(),
                resource_version: object.metadata.resource_version.clone(),
            })
        }
        _ => None,
    };
    Ok(DesiredObservationV0 {
        identity: ObjectIdentity {
            api_version: object.reference.resource.api_version().into(),
            kind: object.reference.resource.kind().into(),
            namespace: object.reference.namespace.clone(),
            name: object.reference.name.clone(),
        },
        observation: CoreObservation::Existing {
            identity: MutationIdentity {
                uid: object.metadata.uid.clone(),
                resource_version: object.metadata.resource_version.clone(),
            },
        },
        qualification: BackendQualification::BuiltInTested {
            kubernetes_minor: "1.34".into(),
            gvr: gvr.into(),
        },
        recovery_proof,
        deletion_recovery_proof: None,
    })
}

fn exact_recovery_annotations(object: &ObjectV0, root_uid: &str) -> Option<(String, String)> {
    let annotations = object.value.pointer("/metadata/annotations")?.as_object()?;
    let root = annotations
        .get(oxideloop_kube_transport::ROOT_UID_ANNOTATION)?
        .as_str()?;
    let transaction_id = annotations
        .get(oxideloop_kube_transport::TRANSACTION_ID_ANNOTATION)?
        .as_str()?;
    let command_id = annotations
        .get(oxideloop_kube_transport::COMMAND_ID_ANNOTATION)?
        .as_str()?;
    if root != root_uid
        || object
            .metadata
            .annotations
            .get(oxideloop_kube_transport::ROOT_UID_ANNOTATION)
            .is_none_or(|value| value != root)
        || object
            .metadata
            .annotations
            .get(oxideloop_kube_transport::TRANSACTION_ID_ANNOTATION)
            .is_none_or(|value| value != transaction_id)
        || object
            .metadata
            .annotations
            .get(oxideloop_kube_transport::COMMAND_ID_ANNOTATION)
            .is_none_or(|value| value != command_id)
    {
        return None;
    }
    Some((transaction_id.into(), command_id.into()))
}

fn exact_controller_owner(object: &ObjectV0, root: &ObjectRefV0, root_uid: &str) -> bool {
    object
        .value
        .pointer("/metadata/ownerReferences")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|owners| {
            owners.len() == 1
                && owners[0].as_object().is_some_and(|owner| {
                    owner.get("apiVersion").and_then(serde_json::Value::as_str)
                        == Some(root.resource.api_version())
                        && owner.get("kind").and_then(serde_json::Value::as_str)
                            == Some(root.resource.kind())
                        && owner.get("name").and_then(serde_json::Value::as_str)
                            == Some(root.name.as_str())
                        && owner.get("uid").and_then(serde_json::Value::as_str) == Some(root_uid)
                        && owner.get("controller").and_then(serde_json::Value::as_bool)
                            == Some(true)
                        && owner
                            .get("blockOwnerDeletion")
                            .and_then(serde_json::Value::as_bool)
                            == Some(false)
                })
        })
}

fn deletion_object_identity(object: &ObjectV0) -> Result<ObjectIdentity, RuntimeErrorV0> {
    let metadata = object
        .value
        .get("metadata")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| RuntimeErrorV0::Hold("deletion observation metadata is malformed".into()))?;
    if object
        .value
        .get("apiVersion")
        .and_then(serde_json::Value::as_str)
        != Some(object.reference.resource.api_version())
        || object.value.get("kind").and_then(serde_json::Value::as_str)
            != Some(object.reference.resource.kind())
        || metadata.get("name").and_then(serde_json::Value::as_str)
            != Some(object.reference.name.as_str())
        || metadata
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            != Some(object.reference.namespace.as_str())
        || metadata.get("uid").and_then(serde_json::Value::as_str)
            != Some(object.metadata.uid.as_str())
        || metadata
            .get("resourceVersion")
            .and_then(serde_json::Value::as_str)
            != Some(object.metadata.resource_version.as_str())
        || object.metadata.uid.is_empty()
        || object.metadata.resource_version.is_empty()
    {
        return Err(RuntimeErrorV0::Hold(
            "deletion observation object identity does not match its exact reread".into(),
        ));
    }
    Ok(ObjectIdentity {
        api_version: object.reference.resource.api_version().into(),
        kind: object.reference.resource.kind().into(),
        namespace: object.reference.namespace.clone(),
        name: object.reference.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use http::{Request, Response};
    use kube::{Client, client::Body};
    use oxideloop_controller_api::{
        InventoryEntry, OxideLoopStateSpec, ProgramBundleReference, PruneCandidate,
        PruneObservation, RootBinding, STATE_VERSION_V1, StateBody, StateLink, WebApplicationSpec,
        WebApplicationStatus, bootstrap_id, state_name,
    };
    use oxideloop_controller_core::{
        CommandV0, EventV0, MachineStateV0, PreconditionsV0, RootSnapshotV0,
    };
    use oxideloop_kube_transport::{ListV0, ObjectMetaV0, TransportErrorV0};
    use oxideloop_renderer_abi::model::{
        Dependency, RenderDisposition, ResourceDeclaration, RootProjection,
    };
    use serde_json::json;
    use tower::service_fn;

    use crate::FixedPointResultV0;

    use super::*;

    #[async_trait]
    impl RendererPortV0 for () {
        async fn call(&mut self, _: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0> {
            Err(RuntimeErrorV0::Renderer)
        }
    }

    struct StableRendererV0 {
        output: RenderOutputV0,
    }

    #[async_trait]
    impl RendererPortV0 for StableRendererV0 {
        async fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0> {
            Ok(ResponseV0 {
                request_id: request.request_id,
                phase: request.phase,
                bundle_digest: request.bundle_digest.clone(),
                module_digest: request.module_digest.clone(),
                status: oxideloop_renderer_protocol::ResponseStatusV0::Ok,
                error_category: None,
                payload: serde_json::to_vec(&self.output).expect("test output serializes"),
            })
        }
    }

    struct BlockingRendererV0 {
        output: RenderOutputV0,
        started: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    #[async_trait]
    impl RendererPortV0 for BlockingRendererV0 {
        async fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0> {
            self.started.store(true, Ordering::SeqCst);
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(release) = self.release.take() {
                let _ = release.await;
            }
            Ok(ResponseV0 {
                request_id: request.request_id,
                phase: request.phase,
                bundle_digest: request.bundle_digest.clone(),
                module_digest: request.module_digest.clone(),
                status: oxideloop_renderer_protocol::ResponseStatusV0::Ok,
                error_category: None,
                payload: serde_json::to_vec(&self.output).expect("test output serializes"),
            })
        }
    }

    fn reference(name: &str) -> ObjectRefV0 {
        ObjectRefV0 {
            resource: ResourceKindV0::ConfigMap,
            namespace: "ns".into(),
            name: name.into(),
        }
    }

    fn root() -> ObjectRefV0 {
        ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: "ns".into(),
            name: "root".into(),
        }
    }

    fn object(name: &str, uid: &str, resource_version: &str) -> ObjectV0 {
        ObjectV0 {
            reference: reference(name),
            metadata: ObjectMetaV0 {
                uid: uid.into(),
                resource_version: resource_version.into(),
                annotations: BTreeMap::new(),
                owner_uids: vec!["root-uid".into()],
            },
            value: json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": name, "namespace": "ns"},
                "data": {"rv": resource_version},
            }),
        }
    }

    fn service_object(name: &str, uid: &str, resource_version: &str) -> ObjectV0 {
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::Service,
                namespace: "ns".into(),
                name: name.into(),
            },
            metadata: ObjectMetaV0 {
                uid: uid.into(),
                resource_version: resource_version.into(),
                annotations: BTreeMap::new(),
                owner_uids: vec!["root-uid".into()],
            },
            value: json!({
                "apiVersion": "v1", "kind": "Service",
                "metadata": {"name": name, "namespace": "ns"}
            }),
        }
    }

    fn fenced_actor(
        inventory: Vec<InventoryEntry>,
        prior_prune_token: Option<String>,
    ) -> RootActorV0 {
        let root_uid = "root-uid";
        let state_name = state_name(root_uid).expect("state name");
        let prune_candidate = prior_prune_token.map(|barrier_token| PruneCandidate {
            complete_key: "f".repeat(64),
            inventory: inventory[0].clone(),
            first_observation: PruneObservation {
                observed_resource_version: "old-service-rv".into(),
                observed_generation: 1,
                barrier_token,
            },
            second_observation: None,
        });
        let machine = MachineStateV0::new(
            RootSnapshotV0 {
                uid: root_uid.into(),
                namespace: "ns".into(),
                name: "root".into(),
                generation: 1,
                spec: WebApplicationSpec {
                    message: "hello".into(),
                    expose: false,
                    program: ProgramBundleReference {
                        config_map_name: "bundle".into(),
                        key: "program.mjs".into(),
                        digest: "a".repeat(64),
                    },
                },
                bundle_digest: "a".repeat(64),
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
                    bundle_digest: "a".repeat(64),
                },
                state: StateBody {
                    inventory,
                    prune_candidate,
                    ..StateBody::default()
                },
            },
        )
        .expect("valid durable actor state");
        let mut actor = RootActorV0::new(machine);
        actor.start();
        let descriptor = actor
            .next_command()
            .expect("fenced actor descriptor request")
            .expect("fenced actor must request descriptor");
        let CommandV0::RequestDescriptor { token, .. } = descriptor else {
            panic!("fenced actor did not request descriptor");
        };
        actor
            .acknowledge(EventV0::DescriptorReady {
                token,
                bundle_digest: "a".repeat(64),
            })
            .expect("fenced actor descriptor acknowledgement");
        let status = actor
            .next_command()
            .expect("fenced actor status request")
            .expect("fenced actor must persist descriptor status");
        let CommandV0::PersistRootStatus {
            token,
            old_root_resource_version,
            root_uid,
            ..
        } = status
        else {
            panic!("fenced actor did not persist descriptor status");
        };
        actor
            .acknowledge(EventV0::RootStatusPersisted {
                token,
                old_root_resource_version,
                new_root_resource_version: "root-rv-after-descriptor".into(),
                root_uid,
            })
            .expect("fenced actor status acknowledgement");
        let next = actor
            .next_command()
            .expect("fenced actor informer transition");
        assert!(next.is_none(), "unexpected fenced actor command: {next:?}");
        actor
    }

    fn fixed_final_result(dependencies: Vec<Dependency>) -> FixedPointResultV0 {
        FixedPointResultV0 {
            input: RenderInputV0 {
                abi_version: "renderer.oxideloop.dev/v0".into(),
                root: RootProjection {
                    api_version: "demo.oxideloop.dev/v1alpha1".into(),
                    kind: "WebApplication".into(),
                    namespace: "ns".into(),
                    name: "root".into(),
                    uid: "root-uid".into(),
                    generation: 1,
                    spec: json!({
                        "message": "hello",
                        "expose": false,
                        "program": {
                            "configMapName": "bundle",
                            "key": "program.mjs",
                            "digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        }
                    }),
                },
                bundle_digest: "a".repeat(64),
                cache_epoch: "4".into(),
                observation_token: "global-token".into(),
                observations: vec![],
            },
            output: RenderOutputV0 {
                abi_version: "renderer.oxideloop.dev/v0".into(),
                disposition: RenderDisposition::Apply,
                resources: vec![ResourceDeclaration {
                    logical_id: "config".into(),
                    object: json!({
                        "apiVersion": "v1",
                        "kind": "ConfigMap",
                        "metadata": {"name": "config", "namespace": "ns"},
                    }),
                    readiness: None,
                }],
                dependencies: dependencies.clone(),
                automations: vec![],
                diagnostics: vec![],
            },
            dependencies,
            provenance: None,
        }
    }

    fn test_descriptor() -> BundleDescriptorV0 {
        BundleDescriptorV0 {
            descriptor_version: "oxideloop.dev/descriptor/v0".into(),
            renderer_abi_version: "renderer.oxideloop.dev/v0".into(),
            render_module: oxideloop_bundle_contract::DescriptorModuleV0 {
                identity: "render/main.mjs".into(),
                digest: "a".repeat(64),
            },
            action_modules: vec![],
        }
    }

    fn owned_render(actor: &mut RootActorV0, input: &RenderInputV0) {
        let root = actor.state().root().clone();
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
    }

    fn arm_completed_fence(
        actor: &mut RootActorV0,
        port: &mut KubeFixedPointPortV0<'_, ()>,
        result: &mut FixedPointResultV0,
    ) {
        result.input.observations = port
            .retained
            .as_ref()
            .expect("test retained barrier")
            .renderer_observations();
        owned_render(actor, result.input());
        let capability = actor
            .issue_render_capability(result.input.clone(), port.identity)
            .expect("test render request pins exact input");
        let (coverage, coverage_hash) = capability
            .coverage_plan(result.output())
            .expect("test final coverage is valid");
        let retained = port.retained.as_mut().expect("test retained barrier");
        let mut union = retained.renderer_dependencies.clone();
        union.extend(coverage.clone());
        retained.barrier.dependencies =
            normalize_dependencies(&union).expect("test union normalizes");
        retained.coverage_dependencies = coverage.clone();
        let incarnation = retained.incarnation;
        result.provenance = Some(capability.complete(incarnation, coverage, coverage_hash));
        port.bind_completed_fixed_point_input(actor, result)
            .expect("test final input is sealed by the concrete port");
    }

    fn retained_with(object: ObjectV0) -> RetainedObservationBarrierV0 {
        let dependencies = vec![Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            selector: String::new(),
            collection_revision: "1".into(),
            observation_token: "test-token".into(),
        }];
        let mut collections = BTreeMap::new();
        collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![object],
                resource_version: "1".into(),
            },
        );
        RetainedObservationBarrierV0::new(ObservationBarrierV0 {
            generation: BarrierV0 {
                generation: 1,
                cache_epoch: 4,
            },
            dependencies,
            collections,
            objects: vec![],
            watches: vec![],
        })
    }

    #[test]
    fn union_coverage_tracks_only_exact_c_names_and_keeps_renderer_projection_d_only() {
        let d = vec![Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            selector: String::new(),
            collection_revision: "rv".into(),
            observation_token: "t".into(),
        }];
        let c = vec![Dependency::Object {
            api_version: "v1".into(),
            kind: "Service".into(),
            namespace: "ns".into(),
            name: "managed".into(),
            uid: None,
            resource_version: None,
            observation_token: "t".into(),
        }];
        let mut collections = retained_collections();
        collections.insert(
            (ResourceKindV0::Service, "ns".into()),
            ListV0 {
                items: vec![service_object("managed", "service-uid", "1")],
                resource_version: "1".into(),
            },
        );
        let mut union = d.clone();
        union.extend(c.clone());
        let mut retained = RetainedObservationBarrierV0::with_union(
            ObservationBarrierV0 {
                generation: BarrierV0 {
                    generation: 9,
                    cache_epoch: 1,
                },
                dependencies: normalize_dependencies(&union).unwrap(),
                collections,
                objects: vec![],
                watches: vec![],
            },
            7,
            d,
            c,
        );
        assert_eq!(retained.renderer_observations().len(), 1);
        assert!(!retained.record(WatchEventV0::Modified(service_object(
            "unrelated",
            "other",
            "2"
        ))));
        assert!(retained.record(WatchEventV0::Modified(service_object(
            "managed",
            "service-uid",
            "2"
        ))));
        assert!(retained.record(WatchEventV0::Deleted(service_object(
            "managed",
            "service-uid",
            "3"
        ))));
        assert!(retained.record(WatchEventV0::Added(service_object(
            "managed",
            "replacement",
            "4"
        ))));
        assert_eq!(retained.renderer_observations().len(), 1);
    }

    #[test]
    fn renderer_projection_keeps_d_origin_separate_from_exact_c_reads() {
        let barrier = BarrierV0 {
            generation: 9,
            cache_epoch: 1,
        };
        let collection_d = vec![Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            selector: String::new(),
            collection_revision: "rv".into(),
            observation_token: "t".into(),
        }];
        let exact = reference("config");
        let exact_c = vec![Dependency::Object {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "config".into(),
            uid: None,
            resource_version: None,
            observation_token: "t".into(),
        }];
        let union = |items: Vec<ObjectV0>, object_read: Option<ObjectV0>| {
            let mut collections = BTreeMap::new();
            collections.insert(
                (ResourceKindV0::ConfigMap, "ns".into()),
                ListV0 {
                    items,
                    resource_version: "rv".into(),
                },
            );
            let mut dependencies = collection_d.clone();
            dependencies.extend(exact_c.clone());
            RetainedObservationBarrierV0::with_union(
                ObservationBarrierV0 {
                    generation: barrier,
                    dependencies: normalize_dependencies(&dependencies).unwrap(),
                    collections,
                    objects: vec![(exact.clone(), object_read)],
                    watches: vec![],
                },
                1,
                collection_d.clone(),
                exact_c.clone(),
            )
        };

        let absent_c = union(vec![], None);
        assert!(
            absent_c.renderer_observations().is_empty(),
            "a C-only exact absence cannot invent D collection membership"
        );

        let present = object("config", "config-uid", "1");
        let mut present_then_deleted = union(vec![present.clone()], None);
        let projected = present_then_deleted.renderer_observations();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].uid, "config-uid");
        assert!(present_then_deleted.record(WatchEventV0::Deleted(object(
            "config",
            "config-uid",
            "2"
        ))));
        let tombstone = present_then_deleted.renderer_observations();
        assert_eq!(tombstone.len(), 1);
        assert!(tombstone[0].value.is_null());

        let exact_d = exact_c.clone();
        let retained = RetainedObservationBarrierV0::with_union(
            ObservationBarrierV0 {
                generation: barrier,
                dependencies: exact_d.clone(),
                collections: BTreeMap::new(),
                objects: vec![(exact, None)],
                watches: vec![],
            },
            2,
            exact_d,
            vec![],
        );
        let projected = retained.renderer_observations();
        assert_eq!(projected.len(), 1);
        assert!(projected[0].value.is_null());
    }

    #[test]
    fn retained_snapshot_survives_stable_render_and_dependency_changes() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        let stable = retained.observations();
        assert_eq!(stable.len(), 1);
        assert_eq!(stable[0].resource_version, "1");

        assert!(retained.record(WatchEventV0::Modified(object("config", "uid-1", "2"))));
        let changed = retained.observations();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].resource_version, "2");
        assert_eq!(changed[0].value["data"]["rv"], "2");
    }

    #[test]
    fn initial_informer_readiness_is_phase_mode_and_retention_gated() {
        let mut actor = fenced_actor(vec![], None);
        actor.watch_invalidated();
        let prior_barrier = BarrierV0 {
            generation: actor.barrier().generation.saturating_sub(1),
            cache_epoch: actor.barrier().cache_epoch,
        };
        let mut port = KubeFixedPointPortV0::<()>::observation_only_for_test(prior_barrier);

        assert!(
            port.admit_initial_informer_readiness(&mut actor)
                .expect("empty informer port is admitted")
        );
        assert_eq!(port.barrier, actor.barrier());
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::RequestRender { ref preconditions }))
                if !preconditions.observation_token.is_empty()
                    && preconditions.cache_epoch == actor.barrier().cache_epoch.to_string()
        ));

        let mut held_actor = fenced_actor(vec![], None);
        held_actor.hold("test hold".into());
        let mut held_port =
            KubeFixedPointPortV0::<()>::observation_only_for_test(held_actor.barrier());
        assert!(
            !held_port
                .admit_initial_informer_readiness(&mut held_actor)
                .expect("held actor is not admitted")
        );

        let mut render_pending_actor = fenced_actor(vec![], None);
        render_pending_actor.informers_ready(render_pending_actor.barrier());
        assert!(matches!(
            render_pending_actor.next_command(),
            Ok(Some(CommandV0::RequestRender { .. }))
        ));
        let mut render_pending_port =
            KubeFixedPointPortV0::<()>::observation_only_for_test(render_pending_actor.barrier());
        assert!(
            !render_pending_port
                .admit_initial_informer_readiness(&mut render_pending_actor)
                .expect("render-pending actor is not admitted")
        );

        let mut retained_actor = fenced_actor(vec![], None);
        let mut retained_port =
            KubeFixedPointPortV0::<()>::observation_only_for_test(retained_actor.barrier());
        retained_port.retained = Some(retained_with(object("config", "uid-1", "1")));
        assert!(matches!(
            retained_port.admit_initial_informer_readiness(&mut retained_actor),
            Err(RuntimeErrorV0::Hold(_))
        ));

        // A terminal discovery restart can reunite an old retained port
        // before the actor reaches InformersPending again. The production
        // driver must clear that authority before admitting the replacement
        // observation barrier.
        let mut restarted_actor = fenced_actor(vec![], None);
        let mut restarted_port =
            KubeFixedPointPortV0::<()>::observation_only_for_test(restarted_actor.barrier());
        restarted_port.retained = Some(retained_with(object("config", "uid-1", "1")));
        restarted_actor.watch_invalidated();
        restarted_port.invalidate_to_barrier(restarted_actor.barrier());
        assert!(
            restarted_port
                .admit_initial_informer_readiness(&mut restarted_actor)
                .expect("invalidated retained port admits the replacement barrier")
        );
        assert_eq!(restarted_port.barrier, restarted_actor.barrier());
        assert!(restarted_port.retained.is_none());
    }

    #[test]
    fn retained_watch_ignores_duplicate_uid_rv_but_admits_a_genuine_change() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        assert!(!retained.record(WatchEventV0::Modified(object("config", "uid-1", "1"))));
        assert!(retained.record(WatchEventV0::Modified(object("config", "uid-1", "2"))));
        assert!(!retained.record(WatchEventV0::Modified(object("config", "uid-1", "2"))));
        assert!(retained.record(WatchEventV0::Deleted(object("config", "uid-1", "3"))));
        assert!(!retained.record(WatchEventV0::Deleted(object("config", "uid-1", "3"))));
    }

    #[test]
    fn coalescing_uses_last_event_and_deleted_is_an_unpinned_tombstone() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        retained.record(WatchEventV0::Added(object("config", "uid-1", "2")));
        retained.record(WatchEventV0::Modified(object("config", "uid-1", "3")));
        retained.record(WatchEventV0::Deleted(object("config", "uid-1", "4")));

        let observations = retained.observations();
        assert_eq!(observations.len(), 1);
        assert!(observations[0].uid.is_empty());
        assert!(observations[0].resource_version.is_empty());
        assert_eq!(observations[0].value, serde_json::Value::Null);
    }

    #[test]
    fn tombstone_pins_are_absent_from_raw_renderer_input() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        retained.record(WatchEventV0::Deleted(object("config", "uid-1", "4")));
        let observations = retained.observations();
        let input = RenderInputV0 {
            abi_version: oxideloop_renderer_abi::model::ABI_VERSION_V0.into(),
            root: oxideloop_renderer_abi::model::RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: "ns".into(),
                name: "root".into(),
                uid: "root-uid".into(),
                generation: 1,
                spec: json!({}),
            },
            bundle_digest: "a".repeat(64),
            cache_epoch: "1".into(),
            observation_token: "token".into(),
            observations,
        };
        let bytes = crate::canonical_render_input(&input).expect("valid raw renderer input");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON input");
        let observation = &value["observations"][0];
        assert_eq!(observation["value"], serde_json::Value::Null);
        assert_eq!(observation["uid"], "");
        assert_eq!(observation["resourceVersion"], "");
    }

    #[test]
    fn coalescing_delete_then_add_restores_the_latest_present_object() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        retained.record(WatchEventV0::Deleted(object("config", "uid-1", "2")));
        retained.record(WatchEventV0::Added(object("config", "uid-2", "3")));

        let observations = retained.observations();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].uid, "uid-2");
        assert_eq!(observations[0].resource_version, "3");
        assert_eq!(observations[0].value["data"]["rv"], "3");
    }

    #[test]
    fn buffered_delivery_order_never_lexically_orders_resource_versions() {
        let mut retained = retained_with(object("config", "uid-1", "z-last-in-list"));
        retained.record(WatchEventV0::Modified(object(
            "config",
            "uid-1",
            "a-delivered-last",
        )));

        let observations = retained.observations();
        assert_eq!(observations[0].resource_version, "a-delivered-last");
    }

    #[test]
    fn absent_nominated_object_is_explicit_null_without_identity_pins() {
        let retained = RetainedObservationBarrierV0::new(ObservationBarrierV0 {
            generation: BarrierV0 {
                generation: 1,
                cache_epoch: 0,
            },
            dependencies: vec![],
            collections: BTreeMap::new(),
            objects: vec![(reference("missing"), None)],
            watches: vec![],
        });
        assert_eq!(
            retained.observations(),
            vec![Observation {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "missing".into(),
                uid: String::new(),
                resource_version: String::new(),
                value: serde_json::Value::Null,
            }]
        );
    }

    // This exercises a single durable sequence: incomplete coverage, two
    // unchanged fences, then a restart that skips its durable prune token.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn completed_fence_requires_full_coverage_and_uses_real_actor_port_path() {
        let present = object("config", "uid-1", "z-old-rv");
        let mut collections = BTreeMap::new();
        collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![present],
                resource_version: "collection-rv".into(),
            },
        );
        let dependencies = vec![
            Dependency::Collection {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                selector: String::new(),
                collection_revision: "collection-rv".into(),
                observation_token: "global-token".into(),
            },
            Dependency::Object {
                api_version: "v1".into(),
                kind: "Service".into(),
                namespace: "ns".into(),
                name: "missing-service".into(),
                uid: None,
                resource_version: None,
                observation_token: "global-token".into(),
            },
        ];
        let barrier = BarrierV0 {
            generation: 9,
            cache_epoch: 4,
        };
        let uncovered = RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: BarrierV0 {
                    generation: 9,
                    cache_epoch: 4,
                },
                dependencies: dependencies.clone(),
                collections,
                objects: vec![],
                watches: vec![],
            },
            1,
        );
        let inventory = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "Service".into(),
                namespace: "ns".into(),
                name: "missing-service".into(),
            },
            logical_id: "missing-service".into(),
            uid: "old-service-uid".into(),
            resource_version: "old-service-rv".into(),
            bundle_digest: "a".repeat(64),
        };
        let mut final_result = fixed_final_result(dependencies.clone());
        let mut actor = fenced_actor(vec![inventory.clone()], None);
        actor.barrier = barrier;
        let mut uncovered_port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        uncovered_port.retained = Some(uncovered);
        arm_completed_fence(&mut actor, &mut uncovered_port, &mut final_result);
        assert!(matches!(
            uncovered_port.complete_observation_fence(&mut actor, &mut final_result),
            Err(RuntimeErrorV0::Hold(_))
        ));

        let missing = ObjectRefV0 {
            resource: ResourceKindV0::Service,
            namespace: "ns".into(),
            name: "missing-service".into(),
        };
        let retained = RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: BarrierV0 {
                    generation: 9,
                    cache_epoch: 4,
                },
                dependencies: dependencies.clone(),
                collections: retained_collections(),
                objects: vec![(missing, None)],
                watches: vec![],
            },
            2,
        );
        let mut port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        port.retained = Some(retained);
        let mut first_result = fixed_final_result(dependencies.clone());
        let mut first_actor = fenced_actor(vec![inventory.clone()], None);
        first_actor.barrier = barrier;
        arm_completed_fence(&mut first_actor, &mut port, &mut first_result);
        let CompletedObservationFenceOutcomeV0::Completed(first) = port
            .complete_observation_fence(&mut first_actor, &mut first_result)
            .expect("the real completion path drains and covers desired plus inventory")
        else {
            panic!("unchanged fully-covered retained snapshot must mint a fence");
        };
        let first_token = first.token().to_owned();
        assert!(matches!(
            first.desired_observations()[0].observation,
            CoreObservation::Existing { .. }
        ));
        assert_eq!(
            first.desired_observations()[1].observation,
            CoreObservation::Absent
        );
        first_actor
            .acknowledge_render_observation(*first)
            .expect("first prune fence acknowledges through its sealed render capability");
        assert!(!first_actor.in_flight);
        let mut second_result = fixed_final_result(dependencies.clone());
        let mut second_actor = fenced_actor(vec![inventory.clone()], None);
        second_actor.barrier = barrier;
        arm_completed_fence(&mut second_actor, &mut port, &mut second_result);
        let CompletedObservationFenceOutcomeV0::Completed(second) = port
            .complete_observation_fence(&mut second_actor, &mut second_result)
            .expect("an unchanged snapshot still completes a distinct fence")
        else {
            panic!("a fresh exact render command must mint a distinct fence");
        };
        assert_ne!(first_token, second.token());
        assert_eq!(second.barrier().generation, 9);
        second_actor
            .acknowledge_render_observation(*second)
            .expect("second prune fence acknowledges through its sealed render capability");
        assert!(!second_actor.in_flight);

        let mut restart_actor = fenced_actor(vec![inventory], Some(first_token.clone()));
        restart_actor.barrier = barrier;
        let restarted = RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: BarrierV0 {
                    generation: 9,
                    cache_epoch: 4,
                },
                dependencies: dependencies.clone(),
                collections: retained_collections(),
                objects: vec![(
                    ObjectRefV0 {
                        resource: ResourceKindV0::Service,
                        namespace: "ns".into(),
                        name: "missing-service".into(),
                    },
                    None,
                )],
                watches: vec![],
            },
            3,
        );
        let mut restarted_port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        restarted_port.retained = Some(restarted);
        let mut restart_result = fixed_final_result(dependencies);
        arm_completed_fence(&mut restart_actor, &mut restarted_port, &mut restart_result);
        let CompletedObservationFenceOutcomeV0::Completed(after_restart) = restarted_port
            .complete_observation_fence(&mut restart_actor, &mut restart_result)
            .expect("a durable first prune token is skipped after restart")
        else {
            panic!("restart retained snapshot must mint a fence");
        };
        assert_eq!(after_restart.sequence(), 2);
        assert_ne!(after_restart.token(), first_token);
    }

    fn retained_collections() -> BTreeMap<(ResourceKindV0, String), ListV0> {
        let present = object("config", "uid-1", "z-old-rv");
        let mut collections = BTreeMap::new();
        collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![present],
                resource_version: "collection-rv".into(),
            },
        );
        collections
    }

    fn active_pin_bytes(actor: &RootActorV0) -> Vec<u8> {
        canonical_render_input(
            &actor
                .active_render_input
                .as_ref()
                .expect("test actor has an active render pin")
                .1,
        )
        .expect("active test input is canonical")
    }

    async fn split_result_with_discovered_observations() -> (
        RootActorV0,
        KubeFixedPointPortV0<'static, StableRendererV0>,
        FixedPointResultV0,
    ) {
        split_result_with_discovered_observations_and_inventory(vec![]).await
    }

    async fn split_result_with_discovered_observations_and_inventory(
        inventory: Vec<InventoryEntry>,
    ) -> (
        RootActorV0,
        KubeFixedPointPortV0<'static, StableRendererV0>,
        FixedPointResultV0,
    ) {
        let barrier = BarrierV0 {
            generation: 9,
            cache_epoch: 4,
        };
        let dependencies = vec![Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            selector: String::new(),
            collection_revision: "collection-rv".into(),
            observation_token: "global-token".into(),
        }];
        let template = fixed_final_result(dependencies.clone());
        let observation = ObservationBarrierV0 {
            generation: barrier,
            dependencies: normalize_dependencies(&dependencies).expect("test dependencies"),
            collections: retained_collections(),
            objects: vec![],
            watches: vec![],
        };
        let mut actor = fenced_actor(inventory, None);
        actor.barrier = barrier;
        owned_render(&mut actor, template.input());
        let port = KubeFixedPointPortV0::fixed_point_for_test(
            StableRendererV0 {
                output: template.output,
            },
            test_descriptor(),
            barrier,
            observation,
        );
        let capability = actor
            .issue_render_capability(template.input, port.identity())
            .expect("split path seals its speculative input");
        let (mut execution, collector, reunion) = port.split_for_render();
        let result = execution
            .reconcile_for_capability(capability)
            .await
            .expect("split path reaches a stable fixed point");
        let RenderExecutionOutcomeV0::Stable(result) = result else {
            panic!("split test fixture must stabilize its D projection");
        };
        let port = KubeFixedPointPortV0::reunite_after_render(execution, collector, reunion)
            .expect("matching split halves reunite");
        assert!(
            !result.input().observations.is_empty(),
            "the test must exercise the final discovered-observations handoff"
        );
        (actor, port, *result)
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn union_observation_uses_real_transport_once_per_parent_and_exact_rereads() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&paths);
        let service = service_fn(move |request: Request<Body>| {
            let recorded = Arc::clone(&recorded);
            async move {
                let path = request.uri().path().to_owned();
                recorded.lock().expect("path lock").push(path.clone());
                let body = if path.ends_with("/configmaps") {
                    json!({"apiVersion":"v1","kind":"ConfigMapList","metadata":{"resourceVersion":"cm-list"},"items":[]})
                } else if path.ends_with("/services") {
                    json!({"apiVersion":"v1","kind":"ServiceList","metadata":{"resourceVersion":"svc-list"},"items":[]})
                } else if path.ends_with("/configmaps/removed") {
                    json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"removed","namespace":"ns","uid":"cm-uid","resourceVersion":"cm-rv"}})
                } else if path.ends_with("/services/managed") {
                    json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"managed","namespace":"ns","uid":"svc-uid","resourceVersion":"svc-rv"}})
                } else {
                    panic!("unexpected U request {path}");
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .header(http::header::CONTENT_TYPE, "application/json")
                        .body(Body::from(serde_json::to_vec(&body).expect("wire JSON")))
                        .expect("wire response"),
                )
            }
        });
        let transport = oxideloop_kube_transport::KubeTransportV0::test_from_client(Client::new(
            service, "default",
        ));
        let mut watch_senders = Vec::new();
        for _ in 0..2 {
            let (sender, watch) = oxideloop_kube_transport::WatchHandleV0::test_channel(1);
            watch_senders.push(sender);
            transport.test_push_watch(watch);
        }
        let d = vec![Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            selector: String::new(),
            collection_revision: "d".into(),
            observation_token: "t".into(),
        }];
        let c = vec![
            Dependency::Object {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "removed".into(),
                uid: None,
                resource_version: None,
                observation_token: "t".into(),
            },
            Dependency::Object {
                api_version: "v1".into(),
                kind: "Service".into(),
                namespace: "ns".into(),
                name: "managed".into(),
                uid: None,
                resource_version: None,
                observation_token: "t".into(),
            },
        ];
        let retained = establish_union_retained(
            &transport,
            &d,
            c,
            BarrierV0 {
                generation: 7,
                cache_epoch: 1,
            },
            41,
        )
        .await
        .expect("real U barrier");
        assert_eq!(retained.incarnation, 41);
        assert_eq!(
            retained.renderer_dependencies,
            normalize_dependencies(&d).unwrap()
        );
        assert_eq!(retained.coverage_dependencies.len(), 2);
        assert_eq!(watch_senders.len(), 2);
        assert!(
            retained
                .renderer_observations()
                .iter()
                .all(|observation| observation.kind != "Service")
        );
        assert_eq!(
            retained
                .barrier
                .objects
                .iter()
                .map(|(reference, _)| reference.name.as_str())
                .collect::<Vec<_>>(),
            vec!["removed", "managed"]
        );
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            [
                "/api/v1/namespaces/ns/configmaps",
                "/api/v1/namespaces/ns/services",
                "/api/v1/namespaces/ns/configmaps",
                "/api/v1/namespaces/ns/services",
                "/api/v1/namespaces/ns/configmaps/removed",
                "/api/v1/namespaces/ns/services/managed",
            ]
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn actor_capability_builds_real_union_from_desired_and_inventory_without_d_leakage() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&paths);
        let service = service_fn(move |request: Request<Body>| {
            let recorded = Arc::clone(&recorded);
            async move {
                let path = request.uri().path().to_owned();
                recorded.lock().expect("path lock").push(path.clone());
                if path.ends_with("/configmaps") {
                    return Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1",
                                    "kind":"ConfigMapList",
                                    "metadata":{"resourceVersion":"cm-list"},
                                    "items":[{
                                        "apiVersion":"v1",
                                        "kind":"ConfigMap",
                                        "metadata":{
                                            "name":"foreign",
                                            "namespace":"ns",
                                            "uid":"foreign-uid",
                                            "resourceVersion":"foreign-rv"
                                        },
                                        "data":{"foreign":"true"}
                                    }]
                                }))
                                .expect("ConfigMap list"),
                            ))
                            .expect("ConfigMap response"),
                    );
                }
                if path.ends_with("/services") {
                    return Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1",
                                    "kind":"ServiceList",
                                    "metadata":{"resourceVersion":"svc-list"},
                                    "items":[]
                                }))
                                .expect("Service list"),
                            ))
                            .expect("Service response"),
                    );
                }
                if path.ends_with("/configmaps/foreign") {
                    return Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1",
                                    "kind":"ConfigMap",
                                    "metadata":{
                                        "name":"foreign",
                                        "namespace":"ns",
                                        "uid":"foreign-uid",
                                        "resourceVersion":"foreign-rv"
                                    },
                                    "data":{"foreign":"true"}
                                }))
                                .expect("foreign ConfigMap"),
                            ))
                            .expect("foreign ConfigMap response"),
                    );
                }
                if path.ends_with("/configmaps/dependency")
                    || path.ends_with("/configmaps/removed")
                    || path.ends_with("/services/managed")
                {
                    return Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(404)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1",
                                    "kind":"Status",
                                    "status":"Failure",
                                    "reason":"NotFound",
                                    "code":404
                                }))
                                .expect("not found"),
                            ))
                            .expect("not-found response"),
                    );
                }
                panic!("unexpected capability U request {path}");
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let mut watch_senders = Vec::new();
        for _ in 0..3 {
            let (sender, watch) = oxideloop_kube_transport::WatchHandleV0::test_channel(1);
            watch_senders.push(sender);
            transport.test_push_watch(watch);
        }

        let inventory = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "removed".into(),
            },
            logical_id: "removed".into(),
            uid: "removed-uid".into(),
            resource_version: "removed-rv".into(),
            bundle_digest: "a".repeat(64),
        };
        let mut actor = fenced_actor(vec![inventory], None);
        actor.barrier = BarrierV0 {
            generation: 9,
            cache_epoch: 4,
        };
        let barrier = actor.barrier();
        actor
            .prepare_initial_informer_observation(barrier)
            .expect("the production informer transition seals its observation token");
        actor.informers_ready(barrier);
        let CommandV0::RequestRender { preconditions } = actor
            .next_command()
            .expect("the production actor accepts informer readiness")
            .expect("the production actor requests a render")
        else {
            panic!("informer readiness must issue the owned render command");
        };
        let d = vec![Dependency::Object {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "dependency".into(),
            uid: None,
            resource_version: None,
            observation_token: preconditions.observation_token.clone(),
        }];
        let mut template = fixed_final_result(d.clone());
        template.input.observation_token = preconditions.observation_token;
        template.input.cache_epoch = preconditions.cache_epoch;
        template.output.resources = vec![
            ResourceDeclaration {
                logical_id: "managed".into(),
                object: json!({
                    "apiVersion":"v1",
                    "kind":"Service",
                    "metadata":{"name":"managed","namespace":"ns"},
                    "spec":{"selector":{"app":"demo"},"ports":[{"port":80}]}
                }),
                readiness: None,
            },
            ResourceDeclaration {
                logical_id: "foreign".into(),
                object: json!({
                    "apiVersion":"v1",
                    "kind":"ConfigMap",
                    "metadata":{"name":"foreign","namespace":"ns"},
                    "data":{"managed":"true"}
                }),
                readiness: None,
            },
        ];
        let port = KubeFixedPointPortV0::new(
            &transport,
            StableRendererV0 {
                output: template.output.clone(),
            },
            test_descriptor(),
            actor.barrier(),
        );
        let capability = actor
            .issue_render_capability(template.input.clone(), port.identity())
            .expect("actor issues the only C-authoritative capability");
        let (mut execution, collector, reunion) = port.split_for_render();
        let outcome = execution
            .reconcile_for_capability(capability)
            .await
            .expect("full D then U production execution");
        let RenderExecutionOutcomeV0::Stable(result) = outcome else {
            panic!("C-only absences must not force a D coverage restart");
        };
        assert_eq!(
            result.input().observations,
            vec![absent_observation(&ObjectRefV0 {
                resource: ResourceKindV0::ConfigMap,
                namespace: "ns".into(),
                name: "dependency".into(),
            })],
            "an exact D object must not inherit unrelated members of its parent LIST"
        );
        let retained = execution.retained.as_ref().expect("production U retained");
        assert_eq!(retained.incarnation, 2);
        assert_eq!(
            retained.renderer_observations(),
            result.input().observations
        );
        assert_eq!(retained.coverage_dependencies.len(), 3);
        assert!(retained.coverage_dependencies.iter().any(|dependency| matches!(dependency,
            Dependency::Object { kind, name, .. } if kind == "ConfigMap" && name == "removed"
        )));
        assert!(
            retained
                .coverage_dependencies
                .iter()
                .any(|dependency| matches!(dependency,
                    Dependency::Object { kind, name, .. } if kind == "Service" && name == "managed"
                ))
        );
        assert!(retained.coverage_dependencies.iter().any(|dependency| matches!(dependency,
            Dependency::Object { kind, name, .. } if kind == "ConfigMap" && name == "foreign"
        )));
        assert_eq!(retained.barrier.watches.len(), 2);
        let mut result = *result;
        let mut port = KubeFixedPointPortV0::reunite_after_render(execution, collector, reunion)
            .expect("full capability path reunites");
        port.bind_completed_fixed_point_input(&mut actor, &result)
            .expect("stable D-only input binds after C collision coverage");
        let CompletedObservationFenceOutcomeV0::Completed(observation) = port
            .complete_observation_fence(&mut actor, &mut result)
            .expect("the desired-name collision reaches the full final fence")
        else {
            panic!("a stable collision snapshot must complete rather than reschedule");
        };
        assert!(observation.desired_observations().iter().any(|observed| {
            observed.identity.kind == "ConfigMap"
                && observed.identity.name == "foreign"
                && matches!(observed.observation, CoreObservation::Existing { .. })
        }));
        let observation_token = result.input().observation_token.clone();
        let cache_epoch = result.input().cache_epoch.clone();
        actor
            .acknowledge_render_observation(*observation)
            .expect("completed render observation is acknowledged by the actor");
        let status = actor
            .next_command()
            .expect("render acknowledgement must queue root status")
            .expect("root status continuation command");
        let (status_token, old_root_resource_version, acknowledged_status) = match status {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                status,
                ..
            } => {
                assert_eq!(
                    status.input_token.as_deref(),
                    Some(observation_token.as_str())
                );
                assert_eq!(status.cache_epoch, cache_epoch.parse::<u64>().ok());
                (token, old_root_resource_version, status)
            }
            other => panic!("expected render status continuation, got {other:?}"),
        };
        let mut acknowledged_root = actor.state().root().clone();
        acknowledged_root.root_resource_version = "root-rv-after-render".into();
        acknowledged_root.status = acknowledged_status;
        actor
            .acknowledge(EventV0::RootStatusPersisted {
                token: status_token,
                old_root_resource_version,
                new_root_resource_version: "root-rv-after-render".into(),
                root_uid: "root-uid".into(),
            })
            .expect("exact root status acknowledgement");
        assert!(actor.root_observed(acknowledged_root));
        let follow_up = actor
            .next_command()
            .expect("status acknowledgement must queue follow-up render")
            .expect("follow-up render command");
        let follow_up_preconditions = match follow_up {
            CommandV0::RequestRender { preconditions } => preconditions,
            other => panic!("expected follow-up render, got {other:?}"),
        };
        assert_eq!(
            follow_up_preconditions.root_resource_version,
            "root-rv-after-render"
        );
        assert_eq!(follow_up_preconditions.observation_token, observation_token);
        assert_eq!(follow_up_preconditions.cache_epoch, cache_epoch);
        let mut follow_up_input = template.input.clone();
        follow_up_input.observation_token = follow_up_preconditions.observation_token.clone();
        follow_up_input.cache_epoch = follow_up_preconditions.cache_epoch.clone();
        actor
            .issue_render_capability(follow_up_input, port.identity())
            .expect("follow-up render capability remains issuable after status continuation");
        assert_eq!(watch_senders.len(), 3);
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            [
                "/api/v1/namespaces/ns/configmaps",
                "/api/v1/namespaces/ns/configmaps",
                "/api/v1/namespaces/ns/configmaps/dependency",
                "/api/v1/namespaces/ns/configmaps",
                "/api/v1/namespaces/ns/services",
                "/api/v1/namespaces/ns/configmaps",
                "/api/v1/namespaces/ns/services",
                "/api/v1/namespaces/ns/configmaps/dependency",
                "/api/v1/namespaces/ns/configmaps/foreign",
                "/api/v1/namespaces/ns/configmaps/removed",
                "/api/v1/namespaces/ns/services/managed",
            ]
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn invalid_d_or_c_stops_production_execution_before_disallowed_io() {
        let invalid_d = [
            Dependency::Object {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "foreign".into(),
                name: "x".into(),
                uid: None,
                resource_version: None,
                observation_token: "global-token".into(),
            },
            Dependency::Object {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: String::new(),
                uid: None,
                resource_version: None,
                observation_token: "global-token".into(),
            },
            Dependency::Object {
                api_version: "v1".into(),
                kind: "Secret".into(),
                namespace: "ns".into(),
                name: "x".into(),
                uid: None,
                resource_version: None,
                observation_token: "global-token".into(),
            },
        ];
        for dependency in invalid_d {
            let calls = Arc::new(AtomicUsize::new(0));
            let recorded = Arc::clone(&calls);
            let service = service_fn(move |_request: Request<Body>| {
                recorded.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(500)
                            .body(Body::from(Vec::<u8>::new()))
                            .expect("unexpected response"),
                    )
                }
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let mut template = fixed_final_result(vec![dependency]);
            template.output.resources.clear();
            let mut actor = fenced_actor(vec![], None);
            actor.barrier = BarrierV0 {
                generation: 9,
                cache_epoch: 4,
            };
            owned_render(&mut actor, template.input());
            let port = KubeFixedPointPortV0::new(
                &transport,
                StableRendererV0 {
                    output: template.output,
                },
                test_descriptor(),
                actor.barrier(),
            );
            let capability = actor
                .issue_render_capability(template.input, port.identity())
                .expect("invalid D remains actor-scoped data until execution");
            let (mut execution, _collector, _reunion) = port.split_for_render();
            assert!(
                execution
                    .reconcile_for_capability(capability)
                    .await
                    .is_err(),
                "invalid D must stop production execution"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }

        let invalid_inventory = [
            ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "foreign".into(),
                name: "foreign".into(),
            },
            ObjectIdentity {
                api_version: "runtime.oxideloop.dev/v1alpha1".into(),
                kind: "AutomationRun".into(),
                namespace: "ns".into(),
                name: "unsupported".into(),
            },
        ];
        for identity in invalid_inventory {
            let calls = Arc::new(AtomicUsize::new(0));
            let recorded = Arc::clone(&calls);
            let service = service_fn(move |request: Request<Body>| {
                let recorded = Arc::clone(&recorded);
                async move {
                    recorded.fetch_add(1, Ordering::SeqCst);
                    assert!(request.uri().path().ends_with("/configmaps"));
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1",
                                    "kind":"ConfigMapList",
                                    "metadata":{"resourceVersion":"d-list"},
                                    "items":[]
                                }))
                                .expect("D list"),
                            ))
                            .expect("D response"),
                    )
                }
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let (watch_sender, watch) = oxideloop_kube_transport::WatchHandleV0::test_channel(1);
            transport.test_push_watch(watch);
            let d = vec![Dependency::Collection {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                selector: String::new(),
                collection_revision: "d".into(),
                observation_token: "global-token".into(),
            }];
            let template = fixed_final_result(d);
            let mut actor = fenced_actor(vec![], None);
            actor.barrier = BarrierV0 {
                generation: 9,
                cache_epoch: 4,
            };
            owned_render(&mut actor, template.input());
            let port = KubeFixedPointPortV0::new(
                &transport,
                StableRendererV0 {
                    output: template.output,
                },
                test_descriptor(),
                actor.barrier(),
            );
            let mut capability = actor
                .issue_render_capability(template.input, port.identity())
                .expect("actor issues capability before the adversarial C mutation");
            capability.seed.inventory = vec![InventoryEntry {
                identity,
                logical_id: "invalid".into(),
                uid: "invalid-uid".into(),
                resource_version: "invalid-rv".into(),
                bundle_digest: "a".repeat(64),
            }];
            capability.seed.inventory_hash =
                crate::inventory_hash(&capability.seed.inventory).expect("inventory hash");
            let (mut execution, _collector, _reunion) = port.split_for_render();
            assert!(matches!(
                execution.reconcile_for_capability(capability).await,
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "only the valid D LIST/re-LIST may run before invalid C rejection"
            );
            drop(watch_sender);
        }
    }

    #[tokio::test]
    async fn split_final_input_handoff_is_sealed_and_replaces_only_observations() {
        let (mut actor, mut port, mut result) = split_result_with_discovered_observations().await;
        let initial = active_pin_bytes(&actor);
        let cloned = result.clone();
        assert!(matches!(
            port.bind_completed_fixed_point_input(&mut actor, &cloned),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_eq!(active_pin_bytes(&actor), initial);

        port.bind_completed_fixed_point_input(&mut actor, &result)
            .expect("the reunited port binds its own sealed final input");
        assert_ne!(active_pin_bytes(&actor), initial);
        assert_eq!(
            active_pin_bytes(&actor),
            canonical_render_input(result.input()).unwrap()
        );
        let CompletedObservationFenceOutcomeV0::Completed(capability) = port
            .complete_observation_fence(&mut actor, &mut result)
            .expect("bound final input completes the retained fence")
        else {
            panic!("split fixed point must mint a render acknowledgement");
        };
        actor
            .acknowledge_render_observation(*capability)
            .expect("split fence acknowledgement remains authorized");
    }

    #[tokio::test]
    async fn sealed_final_input_handoff_rejects_replacements_without_mutating_the_pin() {
        let mutators: [fn(&mut FixedPointResultV0); 5] = [
            |result| result.input.abi_version = "other-abi".into(),
            |result| result.input.root.name = "other-root".into(),
            |result| result.input.observation_token = "other-token".into(),
            |result| result.input.cache_epoch = "other-epoch".into(),
            |result| result.input.root.spec = json!({"message":"other"}),
        ];
        for mutate in mutators {
            let (mut actor, port, mut result) = split_result_with_discovered_observations().await;
            let initial = active_pin_bytes(&actor);
            mutate(&mut result);
            assert!(matches!(
                port.bind_completed_fixed_point_input(&mut actor, &result),
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert_eq!(active_pin_bytes(&actor), initial);
        }

        let (mut actor, port, mut result) = split_result_with_discovered_observations().await;
        let initial = active_pin_bytes(&actor);
        result
            .provenance
            .as_mut()
            .expect("split result has sealed provenance")
            .seed
            .initial_input_hash = "0".repeat(64);
        assert!(matches!(
            port.bind_completed_fixed_point_input(&mut actor, &result),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_eq!(active_pin_bytes(&actor), initial);

        let (mut actor, port, mut result) = split_result_with_discovered_observations().await;
        let initial = active_pin_bytes(&actor);
        let forged_request_id = "forged-fixed-point-request-id".to_owned();
        result
            .provenance
            .as_mut()
            .expect("split result has sealed provenance")
            .seed
            .request_id = forged_request_id.clone();
        actor.render_provenance_request = Some(forged_request_id);
        assert!(matches!(
            port.bind_completed_fixed_point_input(&mut actor, &result),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_eq!(active_pin_bytes(&actor), initial);

        let (mut actor, port, result) = split_result_with_discovered_observations().await;
        actor.active_render_input = None;
        assert!(matches!(
            port.bind_completed_fixed_point_input(&mut actor, &result),
            Err(RuntimeErrorV0::Hold(_))
        ));

        let (mut actor, port, result) = split_result_with_discovered_observations().await;
        let initial = active_pin_bytes(&actor);
        let mut replacement = result.input().clone();
        replacement.observation_token = "replacement".into();
        actor.active_render_input = Some((actor.barrier(), replacement));
        let replacement_bytes = active_pin_bytes(&actor);
        assert!(matches!(
            port.bind_completed_fixed_point_input(&mut actor, &result),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_ne!(replacement_bytes, initial);
        assert_eq!(active_pin_bytes(&actor), replacement_bytes);
    }

    #[tokio::test]
    async fn durable_inventory_mutations_reject_binding_and_fencing_without_changing_the_pin() {
        let entry = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "config".into(),
            },
            logical_id: "config".into(),
            uid: "config-uid".into(),
            resource_version: "config-rv".into(),
            bundle_digest: "a".repeat(64),
        };
        let mutations: [fn(&mut InventoryEntry); 4] = [
            |entry| entry.uid = "other-uid".into(),
            |entry| entry.resource_version = "other-rv".into(),
            |entry| entry.logical_id = "other-logical-id".into(),
            |entry| entry.bundle_digest = "b".repeat(64),
        ];
        for mutate in mutations {
            let (mut actor, mut port, mut result) =
                split_result_with_discovered_observations_and_inventory(vec![entry.clone()]).await;
            let initial = active_pin_bytes(&actor);
            let mut changed = entry.clone();
            mutate(&mut changed);
            actor
                .replace_durable_inventory_for_test(vec![changed])
                .expect("test inventory remains a valid durable state");
            assert!(matches!(
                port.bind_completed_fixed_point_input(&mut actor, &result),
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert_eq!(active_pin_bytes(&actor), initial);
            assert!(matches!(
                port.complete_observation_fence(&mut actor, &mut result),
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert_eq!(active_pin_bytes(&actor), initial);
        }
    }

    #[tokio::test]
    async fn sealed_coverage_change_rejects_stale_or_forged_authority_and_supersedes_once() {
        let (mut actor, mut port, _result) = split_result_with_discovered_observations().await;
        let (incarnation, expected) = {
            let retained = port
                .retained
                .as_ref()
                .expect("split result retains the U barrier");
            (
                retained.incarnation,
                GenuineChangeV0::from_observations(
                    &retained.renderer_observations(),
                    port.barrier,
                    retained.incarnation,
                )
                .expect("canonical D projection"),
            )
        };
        let initial = active_pin_bytes(&actor);
        let stale_barrier = BarrierV0 {
            generation: port.barrier.generation + 1,
            cache_epoch: port.barrier.cache_epoch,
        };
        let cases = [
            CoverageChangedV0::new(
                port.identity + 1,
                port.barrier,
                incarnation,
                expected.clone(),
            ),
            CoverageChangedV0::new(port.identity, stale_barrier, incarnation, expected.clone()),
            CoverageChangedV0::new(
                port.identity,
                port.barrier,
                incarnation + 1,
                expected.clone(),
            ),
            CoverageChangedV0::new(
                port.identity,
                port.barrier,
                incarnation,
                GenuineChangeV0::from_observations(&[], port.barrier, incarnation)
                    .expect("different canonical projection"),
            ),
        ];
        for outcome in cases {
            assert!(matches!(
                port.consume_coverage_change(&mut actor, outcome),
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert_eq!(active_pin_bytes(&actor), initial);
        }

        let outcome = CoverageChangedV0::new(port.identity, port.barrier, incarnation, expected);
        let before = actor.barrier();
        assert_eq!(
            port.consume_coverage_change(&mut actor, outcome)
                .expect("the port consumes its own exact sealed outcome"),
            RetainedWatchCoordinationV0::ReconcileScheduled
        );
        assert_eq!(actor.barrier().generation, before.generation + 1);
        assert!(!actor.owns_render_request());
    }

    #[tokio::test]
    async fn sealed_final_input_handoff_rejects_cross_port_or_incarnation_and_root_change() {
        let (mut actor, port, result) = split_result_with_discovered_observations().await;
        let initial = active_pin_bytes(&actor);
        let mut other = KubeFixedPointPortV0::<()>::observation_only_for_test(actor.barrier());
        other.retained = Some(empty_retained(actor.barrier(), 1));
        assert!(matches!(
            other.bind_completed_fixed_point_input(&mut actor, &result),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_eq!(active_pin_bytes(&actor), initial);

        let coverage_mutators: [fn(&mut Vec<Dependency>); 2] = [
            |coverage| {
                coverage.clear();
            },
            |coverage| {
                coverage.push(Dependency::Object {
                    api_version: "v1".into(),
                    kind: "Service".into(),
                    namespace: "ns".into(),
                    name: "extra".into(),
                    uid: None,
                    resource_version: None,
                    observation_token: "global-token".into(),
                });
            },
        ];
        for mutate_coverage in coverage_mutators {
            let (mut actor, mut port, result) = split_result_with_discovered_observations().await;
            let initial = active_pin_bytes(&actor);
            mutate_coverage(
                &mut port
                    .retained
                    .as_mut()
                    .expect("split result retains union coverage")
                    .coverage_dependencies,
            );
            assert!(matches!(
                port.bind_completed_fixed_point_input(&mut actor, &result),
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert_eq!(active_pin_bytes(&actor), initial);
        }

        let mut port = port;
        port.retained = Some(empty_retained(actor.barrier(), 2));
        assert!(matches!(
            port.bind_completed_fixed_point_input(&mut actor, &result),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_eq!(active_pin_bytes(&actor), initial);

        let (mut actor, mut port, mut result) = split_result_with_discovered_observations().await;
        port.bind_completed_fixed_point_input(&mut actor, &result)
            .expect("sealed final input binds before a genuine root change");
        let mut changed = actor.state().root().clone();
        changed.root_resource_version = "root-rv-after-final-bind".into();
        assert!(actor.root_observed(changed));
        assert_eq!(
            port.complete_observation_fence(&mut actor, &mut result)
                .expect("genuine root change restarts rather than completing"),
            CompletedObservationFenceOutcomeV0::RestartRequired
        );

        let (mut actor, mut port, result) = split_result_with_discovered_observations().await;
        port.bind_completed_fixed_point_input(&mut actor, &result)
            .expect("sealed final input binds before a genuine dependency change");
        let change = {
            let retained = port
                .retained
                .as_mut()
                .expect("split port retains its dependency barrier");
            assert!(
                retained.record(WatchEventV0::Modified(object(
                    "config",
                    "uid-1",
                    "changed-rv"
                ))),
                "a post-bind dependency modification is buffered"
            );
            GenuineChangeV0::from_observations(
                &retained.observations(),
                retained.barrier.generation,
                retained.incarnation,
            )
            .expect("buffered dependency change is canonical")
        };
        assert_eq!(
            port.apply_buffered_retained_watch(
                &mut actor,
                BufferedRetainedWatchV0::Change {
                    port_identity: port.identity(),
                    barrier: port.barrier,
                    change,
                },
            )
            .expect("genuine dependency delivery is coordinated"),
            RetainedWatchCoordinationV0::ReconcileScheduled
        );
        assert!(
            !actor.owns_render_request(),
            "a buffered genuine dependency change blocks completion of the bound render"
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn split_render_collects_old_dependency_changes_without_cancelling_the_future() {
        let barrier = BarrierV0 {
            generation: 9,
            cache_epoch: 4,
        };
        let dependencies = vec![Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            selector: String::new(),
            collection_revision: "collection-rv".into(),
            observation_token: "global-token".into(),
        }];
        for event in [
            Ok(WatchEventV0::Modified(object(
                "config",
                "uid-1",
                "modified-rv",
            ))),
            Ok(WatchEventV0::Deleted(object(
                "config",
                "uid-1",
                "deleted-rv",
            ))),
            Err(TransportErrorV0::WatchGone),
            Err(TransportErrorV0::WatchOverflow),
        ] {
            let template = fixed_final_result(dependencies.clone());
            let (watch_sender, old_watch) =
                oxideloop_kube_transport::WatchHandleV0::test_channel(1);
            let old_observation = ObservationBarrierV0 {
                generation: barrier,
                dependencies: normalize_dependencies(&dependencies).expect("test dependencies"),
                collections: retained_collections(),
                objects: vec![],
                watches: vec![old_watch],
            };
            let new_observation = ObservationBarrierV0 {
                generation: barrier,
                dependencies: normalize_dependencies(&dependencies).expect("test dependencies"),
                collections: retained_collections(),
                objects: vec![],
                watches: vec![],
            };
            let started = Arc::new(AtomicBool::new(false));
            let calls = Arc::new(AtomicUsize::new(0));
            let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
            let mut port = KubeFixedPointPortV0::fixed_point_for_test(
                BlockingRendererV0 {
                    output: template.output.clone(),
                    started: Arc::clone(&started),
                    calls: Arc::clone(&calls),
                    release: Some(release_receiver),
                },
                test_descriptor(),
                barrier,
                new_observation,
            );
            port.next_retained_incarnation = 1;
            port.retained = Some(RetainedObservationBarrierV0::with_incarnation(
                old_observation,
                1,
            ));
            let mut actor = fenced_actor(vec![], None);
            actor.barrier = barrier;
            owned_render(&mut actor, template.input());
            let capability = actor
                .issue_render_capability(template.input.clone(), port.identity())
                .expect("owned request pins split render capability");
            let (mut execution, mut collector, reunion) = port.split_for_render();
            let collect = async {
                while !started.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
                watch_sender
                    .try_send(event)
                    .expect("old retained watch records while renderer is pending");
                let record = collector
                    .await_buffered_retained_watch()
                    .await
                    .expect("collector returns one sealed record");
                release_sender
                    .send(())
                    .expect("blocked renderer is still owned exactly once");
                record
            };
            let (render, record) =
                tokio::join!(execution.reconcile_for_capability(capability), collect,);
            let _render = render.expect("the original render future completes after release");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            let mut port =
                KubeFixedPointPortV0::reunite_after_render(execution, collector, reunion)
                    .expect("only matching split halves reunite");
            let outcome = port
                .apply_buffered_retained_watch(&mut actor, record)
                .expect("old record is applied only after the future resolved");
            assert!(matches!(
                outcome,
                RetainedWatchCoordinationV0::ReconcileScheduled
                    | RetainedWatchCoordinationV0::RestartRequired
            ));
            assert!(
                !actor.owns_render_request(),
                "a collected change supersedes the completed no-effect render before a fence"
            );
        }
    }

    #[test]
    fn split_reunion_rejects_cross_port_halves() {
        let barrier = BarrierV0 {
            generation: 1,
            cache_epoch: 0,
        };
        let first = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        let second = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        let (execution, _, reunion) = first.split_for_render();
        let (_, collector, _) = second.split_for_render();
        assert!(matches!(
            KubeFixedPointPortV0::reunite_after_render(execution, collector, reunion),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }

    #[tokio::test]
    async fn completion_requires_the_concrete_port_provenance_capability() {
        let barrier = BarrierV0 {
            generation: 9,
            cache_epoch: 4,
        };
        let dependencies = vec![Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            selector: String::new(),
            collection_revision: "collection-rv".into(),
            observation_token: "global-token".into(),
        }];
        let template = fixed_final_result(dependencies.clone());
        let observation = ObservationBarrierV0 {
            generation: barrier,
            dependencies: normalize_dependencies(&dependencies).expect("test dependencies"),
            collections: retained_collections(),
            objects: vec![],
            watches: vec![],
        };
        let mut actor = fenced_actor(vec![], None);
        actor.barrier = barrier;
        owned_render(&mut actor, template.input());
        let port = KubeFixedPointPortV0::fixed_point_for_test(
            StableRendererV0 {
                output: template.output.clone(),
            },
            test_descriptor(),
            barrier,
            observation,
        );
        let (mut port, mut result) =
            reconcile_split_for_test(&mut actor, port, template.input.clone())
                .await
                .expect("only the concrete port can attach provenance");
        assert!(
            !result.input().observations.is_empty(),
            "direct reconciliation must bind the same discovered final input"
        );
        assert_eq!(
            active_pin_bytes(&actor),
            canonical_render_input(result.input()).expect("final direct input is canonical")
        );
        let CompletedObservationFenceOutcomeV0::Completed(capability) = port
            .complete_observation_fence(&mut actor, &mut result)
            .expect("concrete port completes the full observation fence")
        else {
            panic!("concrete port must mint the sealed render acknowledgement");
        };
        actor
            .acknowledge_render_observation(*capability)
            .expect("only the sealed concrete-port capability acknowledges RequestRender");
        assert!(!actor.in_flight);

        let mut generic_actor = fenced_actor(vec![], None);
        generic_actor.barrier = barrier;
        let mut generic_port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        generic_port.retained = Some(RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: barrier,
                dependencies: normalize_dependencies(&dependencies).expect("test dependencies"),
                collections: retained_collections(),
                objects: vec![],
                watches: vec![],
            },
            1,
        ));
        let mut generic = fixed_final_result(dependencies);
        generic.input.observations = generic_port
            .retained
            .as_ref()
            .expect("test retained")
            .observations();
        owned_render(&mut generic_actor, generic.input());
        let _capability = generic_actor
            .issue_render_capability(generic.input.clone(), generic_port.identity())
            .expect("generic test pins a speculative input");
        assert!(matches!(
            generic_port.complete_observation_fence(&mut generic_actor, &mut generic),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn buffered_c_only_union_watch_change_or_invalidation_never_mints_a_fence() {
        for event in [
            Ok(WatchEventV0::Modified(service_object(
                "managed",
                "service-uid",
                "changed-rv",
            ))),
            Ok(WatchEventV0::Deleted(service_object(
                "managed",
                "service-uid",
                "deleted-rv",
            ))),
            Ok(WatchEventV0::Added(service_object(
                "managed",
                "replacement-uid",
                "replacement-rv",
            ))),
            Err(TransportErrorV0::WatchGone),
            Err(TransportErrorV0::WatchOverflow),
        ] {
            let barrier = BarrierV0 {
                generation: 9,
                cache_epoch: 4,
            };
            let dependencies = vec![Dependency::Collection {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                selector: String::new(),
                collection_revision: "collection-rv".into(),
                observation_token: "global-token".into(),
            }];
            let mut template = fixed_final_result(dependencies.clone());
            template.output.resources[0] = ResourceDeclaration {
                logical_id: "managed-service".into(),
                object: json!({
                    "apiVersion": "v1",
                    "kind": "Service",
                    "metadata": {"name": "managed", "namespace": "ns"},
                }),
                readiness: None,
            };
            let (sender, watch) = oxideloop_kube_transport::WatchHandleV0::test_channel(1);
            let observation = ObservationBarrierV0 {
                generation: barrier,
                dependencies: normalize_dependencies(&dependencies).expect("test dependencies"),
                collections: retained_collections(),
                objects: vec![],
                watches: vec![watch],
            };
            let mut actor = fenced_actor(vec![], None);
            actor.barrier = barrier;
            let port = KubeFixedPointPortV0::fixed_point_for_test(
                StableRendererV0 {
                    output: template.output.clone(),
                },
                test_descriptor(),
                barrier,
                observation,
            );
            template.input.observations =
                vec![renderer_observation(object("config", "uid-1", "z-old-rv"))];
            owned_render(&mut actor, template.input());
            let (mut port, mut result) =
                reconcile_split_for_test(&mut actor, port, template.input.clone())
                    .await
                    .expect("concrete test fixed point completes before the race");
            port.retained
                .as_mut()
                .expect("test union retained barrier")
                .barrier
                .collections
                .insert(
                    (ResourceKindV0::Service, "ns".into()),
                    ListV0 {
                        items: vec![service_object("managed", "service-uid", "initial-rv")],
                        resource_version: "initial-rv".into(),
                    },
                );
            sender.try_send(event).expect("buffer final watch event");
            let outcome = port
                .complete_observation_fence(&mut actor, &mut result)
                .expect("buffered watch outcome is coordinated");
            if actor.barrier().cache_epoch == barrier.cache_epoch {
                assert_eq!(
                    outcome,
                    CompletedObservationFenceOutcomeV0::ReconcileScheduled
                );
                assert_eq!(actor.barrier().generation, barrier.generation + 1);
            } else {
                assert_eq!(outcome, CompletedObservationFenceOutcomeV0::RestartRequired);
                assert_eq!(actor.barrier().cache_epoch, barrier.cache_epoch + 1);
            }

            // The buffered event consumed the old no-effect RequestRender.
            // Drive the actor through a fresh informer fence, a fresh concrete
            // fixed point, and a sealed acknowledgement to prove it cannot
            // remain stuck InFlight or replay the abandoned render effect.
            actor.informers_ready(actor.barrier());
            let CommandV0::RequestRender { preconditions } = actor
                .next_command()
                .expect("fresh informer transition")
                .expect("fresh render request after buffered watch event")
            else {
                panic!("buffered observation did not re-enter RequestRender");
            };
            let mut retry_input = template.input.clone();
            retry_input.observation_token = preconditions.observation_token;
            retry_input.cache_epoch = preconditions.cache_epoch;
            let retry_observation = ObservationBarrierV0 {
                generation: actor.barrier(),
                dependencies: normalize_dependencies(&dependencies)
                    .expect("retry dependencies are normalized"),
                collections: retained_collections(),
                objects: vec![],
                watches: vec![],
            };
            let retry_port = KubeFixedPointPortV0::fixed_point_for_test(
                StableRendererV0 {
                    output: template.output.clone(),
                },
                test_descriptor(),
                actor.barrier(),
                retry_observation,
            );
            retry_input.observations =
                vec![renderer_observation(object("config", "uid-1", "z-old-rv"))];
            let (mut retry_port, mut retry) =
                reconcile_split_for_test(&mut actor, retry_port, retry_input)
                    .await
                    .expect("fresh fixed point after buffered watch event");
            retry_port
                .retained
                .as_mut()
                .expect("test retry union retained barrier")
                .barrier
                .collections
                .insert(
                    (ResourceKindV0::Service, "ns".into()),
                    ListV0 {
                        items: vec![service_object("managed", "service-uid", "retry-rv")],
                        resource_version: "retry-rv".into(),
                    },
                );
            let CompletedObservationFenceOutcomeV0::Completed(capability) = retry_port
                .complete_observation_fence(&mut actor, &mut retry)
                .expect("fresh full fence after buffered watch event")
            else {
                panic!("fresh fixed point did not mint a render capability");
            };
            actor
                .acknowledge_render_observation(*capability)
                .expect("sealed retry render acknowledgement");
            assert!(matches!(
                actor.next_command(),
                Ok(Some(
                    CommandV0::Persist { .. } | CommandV0::PersistRootStatus { .. }
                ))
            ));
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn initial_fixed_point_watch_invalidation_consumes_the_render_capability() {
        for error in [TransportErrorV0::WatchGone, TransportErrorV0::WatchOverflow] {
            let barrier = BarrierV0 {
                generation: 9,
                cache_epoch: 4,
            };
            let dependencies = vec![Dependency::Collection {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                selector: String::new(),
                collection_revision: "collection-rv".into(),
                observation_token: "global-token".into(),
            }];
            let template = fixed_final_result(dependencies.clone());
            let (sender, watch) = oxideloop_kube_transport::WatchHandleV0::test_channel(1);
            sender
                .try_send(Err(error))
                .expect("buffer initial terminal watch error");
            let mut actor = fenced_actor(vec![], None);
            actor.barrier = barrier;
            owned_render(&mut actor, template.input());
            let port = KubeFixedPointPortV0::fixed_point_for_test(
                StableRendererV0 {
                    output: template.output.clone(),
                },
                test_descriptor(),
                barrier,
                ObservationBarrierV0 {
                    generation: barrier,
                    dependencies: normalize_dependencies(&dependencies)
                        .expect("initial dependencies are normalized"),
                    collections: retained_collections(),
                    objects: vec![],
                    watches: vec![watch],
                },
            );
            assert!(matches!(
                reconcile_split_for_test(&mut actor, port, template.input.clone()).await,
                Err(RuntimeErrorV0::Discovery(
                    TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow
                ))
            ));
            assert!(
                !actor.in_flight,
                "watch invalidation must consume RequestRender"
            );
            actor.informers_ready(actor.barrier());
            let CommandV0::RequestRender { preconditions } = actor
                .next_command()
                .expect("fresh informer transition after initial invalidation")
                .expect("initial invalidation must re-enter RequestRender")
            else {
                panic!("initial invalidation did not re-enter RequestRender");
            };
            let mut retry_input = template.input.clone();
            retry_input.observation_token = preconditions.observation_token;
            retry_input.cache_epoch = preconditions.cache_epoch;
            let retry_port = KubeFixedPointPortV0::fixed_point_for_test(
                StableRendererV0 {
                    output: template.output.clone(),
                },
                test_descriptor(),
                actor.barrier(),
                ObservationBarrierV0 {
                    generation: actor.barrier(),
                    dependencies: normalize_dependencies(&dependencies)
                        .expect("retry dependencies are normalized"),
                    collections: retained_collections(),
                    objects: vec![],
                    watches: vec![],
                },
            );
            let (mut retry_port, mut retry) =
                reconcile_split_for_test(&mut actor, retry_port, retry_input)
                    .await
                    .expect("fresh fixed point after initial invalidation");
            let CompletedObservationFenceOutcomeV0::Completed(capability) = retry_port
                .complete_observation_fence(&mut actor, &mut retry)
                .expect("fresh full fence after initial invalidation")
            else {
                panic!("initial invalidation retry did not mint a render capability");
            };
            actor
                .acknowledge_render_observation(*capability)
                .expect("sealed retry render acknowledgement");
            assert!(matches!(
                actor.next_command(),
                Ok(Some(
                    CommandV0::Persist { .. } | CommandV0::PersistRootStatus { .. }
                ))
            ));
        }
    }

    #[tokio::test]
    async fn superseded_retained_barrier_drops_without_scheduling_its_payload() {
        let mut actor = fenced_actor(vec![], None);
        let old_barrier = actor.barrier();
        let mut port = KubeFixedPointPortV0::<()>::observation_only_for_test(old_barrier);
        let mut retained = retained_with(object("config", "uid-a", "old-rv"));
        retained.barrier.generation = old_barrier;
        port.retained = Some(retained);

        // A fresh root observation replaces the actor's barrier before a
        // retained watch can deliver. The old handle must be dropped before
        // it can turn its payload into a renderer-token transition.
        let mut changed_root = actor.state().root().clone();
        changed_root.root_resource_version = "root-rv-after-retained-barrier".into();
        assert!(actor.root_observed(changed_root));
        assert_eq!(
            port.coordinate_retained_watch(&mut actor)
                .await
                .expect("stale retained barrier is coordinated"),
            RetainedWatchCoordinationV0::RestartRequired
        );
        assert!(!port.has_retained_observation_barrier());
        assert!(actor.observation_events.is_empty());
    }

    #[test]
    fn stale_or_unpinned_delete_never_erases_a_replacement() {
        let mut retained = retained_with(object("config", "uid-a", "1"));
        assert!(retained.record(WatchEventV0::Deleted(object("config", "uid-a", "2"))));
        assert!(retained.record(WatchEventV0::Added(object("config", "uid-b", "3"))));
        assert!(!retained.record(WatchEventV0::Deleted(object("config", "uid-a", "4"))));
        assert_eq!(retained.observations()[0].uid, "uid-b");
        assert!(!retained.record(WatchEventV0::Deleted(object("unknown", "uid-c", "5"))));
        assert_eq!(retained.observations()[0].uid, "uid-b");
    }

    #[test]
    fn exact_reread_precedes_collection_for_delete_uid_correlation() {
        let mut collections = BTreeMap::new();
        collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![object("config", "uid-a", "collection-rv")],
                resource_version: "collection-rv".into(),
            },
        );
        let mut retained = RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: BarrierV0 {
                    generation: 1,
                    cache_epoch: 0,
                },
                dependencies: vec![],
                collections,
                objects: vec![(
                    reference("config"),
                    Some(object("config", "uid-b", "exact-rv")),
                )],
                watches: vec![],
            },
            1,
        );

        assert_eq!(
            retained.present_identity(&observation_key(&reference("config"))),
            Some(("uid-b".into(), "exact-rv".into()))
        );
        assert!(!retained.record(WatchEventV0::Deleted(object("config", "uid-a", "stale-rv"))));
        let observations = retained.observations();
        assert_eq!(observations[0].uid, "uid-b");
        assert_eq!(observations[0].resource_version, "exact-rv");
    }

    #[test]
    fn retained_dedupe_is_scoped_to_the_barrier_incarnation() {
        let mut actor = fenced_actor(vec![], None);
        let barrier_one = BarrierV0 {
            generation: 1,
            cache_epoch: 4,
        };
        actor.barrier = barrier_one;
        let first = RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: barrier_one,
                dependencies: vec![Dependency::Collection {
                    api_version: "v1".into(),
                    kind: "ConfigMap".into(),
                    namespace: "ns".into(),
                    selector: String::new(),
                    collection_revision: "first".into(),
                    observation_token: "first".into(),
                }],
                collections: retained_collections(),
                objects: vec![],
                watches: vec![],
            },
            1,
        );
        let first_change = GenuineChangeV0::from_observations(
            &first.observations(),
            first.barrier.generation,
            first.incarnation,
        )
        .expect("canonical first retained snapshot");
        assert!(actor.record_genuine_dependency_change(&first_change));

        let mut second = RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: actor.barrier(),
                dependencies: vec![Dependency::Collection {
                    api_version: "v1".into(),
                    kind: "ConfigMap".into(),
                    namespace: "ns".into(),
                    selector: String::new(),
                    collection_revision: "second".into(),
                    observation_token: "second".into(),
                }],
                collections: {
                    let mut collections = BTreeMap::new();
                    collections.insert(
                        (ResourceKindV0::ConfigMap, "ns".into()),
                        ListV0 {
                            items: vec![object("config", "uid-1", "y")],
                            resource_version: "y".into(),
                        },
                    );
                    collections
                },
                objects: vec![],
                watches: vec![],
            },
            2,
        );
        assert!(second.record(WatchEventV0::Modified(object(
            "config", "uid-1", "z-old-rv"
        ))));
        let second_change = GenuineChangeV0::from_observations(
            &second.observations(),
            second.barrier.generation,
            second.incarnation,
        )
        .expect("canonical replacement retained snapshot");
        assert!(
            actor.record_genuine_dependency_change(&second_change),
            "barrier two's Y-to-X change must not be suppressed by barrier one's X snapshot"
        );
    }

    #[test]
    fn completed_fence_rejects_fixed_point_pin_mismatches_and_barrier_replacement() {
        let barrier = BarrierV0 {
            generation: 9,
            cache_epoch: 4,
        };
        let mismatches: [fn(&mut FixedPointResultV0); 7] = [
            |result| result.input.root.name = "other-root".into(),
            |result| result.input.root.uid = "other-uid".into(),
            |result| result.input.root.generation = 2,
            |result| result.input.root.spec = json!({"message":"other"}),
            |result| result.input.bundle_digest = "b".repeat(64),
            |result| result.input.cache_epoch = "5".into(),
            |result| result.input.observation_token = "other-token".into(),
        ];
        for mismatch in mismatches {
            let mut actor = fenced_actor(vec![], None);
            actor.barrier = barrier;
            let retained = RetainedObservationBarrierV0::with_incarnation(
                ObservationBarrierV0 {
                    generation: barrier,
                    dependencies: vec![],
                    collections: BTreeMap::new(),
                    objects: vec![],
                    watches: vec![],
                },
                1,
            );
            let mut port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
            port.retained = Some(retained);
            let mut invalid_result = fixed_final_result(vec![]);
            arm_completed_fence(&mut actor, &mut port, &mut invalid_result);
            mismatch(&mut invalid_result);
            assert!(matches!(
                port.complete_observation_fence(&mut actor, &mut invalid_result),
                Err(RuntimeErrorV0::Hold(_))
            ));
        }

        let retained = RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: barrier,
                dependencies: vec![],
                collections: BTreeMap::new(),
                objects: vec![],
                watches: vec![],
            },
            1,
        );
        let mut port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        port.retained = Some(retained);
        let mut actor = fenced_actor(vec![], None);
        actor.barrier = barrier;
        let mut replacement_result = fixed_final_result(vec![]);
        arm_completed_fence(&mut actor, &mut port, &mut replacement_result);
        let replacement_barrier = BarrierV0 {
            generation: 10,
            cache_epoch: 4,
        };
        port.set_barrier(replacement_barrier);
        assert!(!port.has_retained_observation_barrier());
        assert_eq!(
            port.complete_observation_fence(&mut actor, &mut replacement_result)
                .expect("a superseded port barrier restarts rather than fencing"),
            CompletedObservationFenceOutcomeV0::RestartRequired
        );

        assert_cross_port_and_incarnation_rejection(barrier);
    }

    fn empty_retained(barrier: BarrierV0, incarnation: u64) -> RetainedObservationBarrierV0 {
        RetainedObservationBarrierV0::with_incarnation(
            ObservationBarrierV0 {
                generation: barrier,
                dependencies: vec![],
                collections: BTreeMap::new(),
                objects: vec![],
                watches: vec![],
            },
            incarnation,
        )
    }

    fn assert_cross_port_and_incarnation_rejection(barrier: BarrierV0) {
        let mut source_actor = fenced_actor(vec![], None);
        source_actor.barrier = barrier;
        let mut source_port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        source_port.retained = Some(empty_retained(barrier, 1));
        let mut cross_port_result = fixed_final_result(vec![]);
        arm_completed_fence(&mut source_actor, &mut source_port, &mut cross_port_result);
        let mut other_port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        other_port.retained = Some(empty_retained(barrier, 1));
        assert!(matches!(
            other_port.complete_observation_fence(&mut source_actor, &mut cross_port_result),
            Err(RuntimeErrorV0::Hold(_))
        ));

        let mut incarnation_actor = fenced_actor(vec![], None);
        incarnation_actor.barrier = barrier;
        let mut incarnation_port = KubeFixedPointPortV0::<()>::observation_only_for_test(barrier);
        incarnation_port.retained = Some(empty_retained(barrier, 1));
        let mut old_incarnation_result = fixed_final_result(vec![]);
        arm_completed_fence(
            &mut incarnation_actor,
            &mut incarnation_port,
            &mut old_incarnation_result,
        );
        incarnation_port.retained = Some(empty_retained(barrier, 2));
        assert!(matches!(
            incarnation_port
                .complete_observation_fence(&mut incarnation_actor, &mut old_incarnation_result),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }

    #[test]
    fn retained_watch_410_and_overflow_each_restart_the_actor_epoch_once() {
        for error in [TransportErrorV0::WatchGone, TransportErrorV0::WatchOverflow] {
            let mut actor = fenced_actor(vec![], None);
            let before = actor.barrier();
            let port_barrier = actor.barrier();
            assert_eq!(
                coordinate_retained_watch_result(
                    &mut actor,
                    port_barrier,
                    Err(RuntimeErrorV0::Discovery(error)),
                )
                .expect("terminal retained-watch error is coordinated"),
                RetainedWatchCoordinationV0::RestartRequired,
            );
            assert_eq!(
                actor.barrier(),
                BarrierV0 {
                    generation: before.generation + 1,
                    cache_epoch: before.cache_epoch + 1,
                }
            );
        }
    }

    fn owned_proven_object() -> ObjectV0 {
        let mut object = object("owned", "child-uid", "7");
        let annotations = BTreeMap::from([
            (
                oxideloop_kube_transport::ROOT_UID_ANNOTATION.into(),
                "root-uid".into(),
            ),
            (
                oxideloop_kube_transport::TRANSACTION_ID_ANNOTATION.into(),
                "tx-1".into(),
            ),
            (
                oxideloop_kube_transport::COMMAND_ID_ANNOTATION.into(),
                "command-1".into(),
            ),
        ]);
        object.metadata.annotations = annotations.clone();
        object.value["metadata"]["annotations"] = serde_json::to_value(annotations).unwrap();
        object.value["metadata"]["ownerReferences"] = json!([{
            "apiVersion": "demo.oxideloop.dev/v1alpha1",
            "kind": "WebApplication",
            "name": "root",
            "uid": "root-uid",
            "controller": true,
            "blockOwnerDeletion": false,
        }]);
        object
    }

    #[test]
    fn desired_observation_requires_exact_owner_and_proof_annotations() {
        let object = owned_proven_object();
        let desired = desired_observation(&root(), "root-uid", &object).unwrap();
        assert_eq!(
            desired.qualification,
            BackendQualification::BuiltInTested {
                kubernetes_minor: "1.34".into(),
                gvr: "v1/configmaps".into(),
            }
        );
        assert_eq!(desired.recovery_proof.unwrap().transaction_id, "tx-1");
    }

    #[test]
    fn ambiguous_owner_or_proof_never_adopts() {
        let mut extra_owner = owned_proven_object();
        extra_owner.value["metadata"]["ownerReferences"]
            .as_array_mut()
            .unwrap()
            .push(json!({"uid": "foreign"}));
        assert!(
            desired_observation(&root(), "root-uid", &extra_owner)
                .unwrap()
                .recovery_proof
                .is_none()
        );

        let mut blocking_owner = owned_proven_object();
        blocking_owner.value["metadata"]["ownerReferences"][0]["blockOwnerDeletion"] = json!(true);
        assert!(
            desired_observation(&root(), "root-uid", &blocking_owner)
                .unwrap()
                .recovery_proof
                .is_none()
        );

        let mut mismatched_proof = owned_proven_object();
        mismatched_proof.metadata.annotations.insert(
            oxideloop_kube_transport::COMMAND_ID_ANNOTATION.into(),
            "different".into(),
        );
        assert!(
            desired_observation(&root(), "root-uid", &mismatched_proof)
                .unwrap()
                .recovery_proof
                .is_none()
        );
    }
}
