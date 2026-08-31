//! P6 keyed, single-replica controller scheduling.
#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc, clippy::must_use_candidate)]

#[cfg(all(feature = "test-failpoints", not(debug_assertions)))]
compile_error!("the test-failpoints feature is only available in debug builds");

mod adapters;
mod driver;
mod engine;
#[cfg(feature = "test-failpoints")]
pub mod failpoint_uds;
pub(crate) use adapters::{BufferedRetainedWatchV0, RetainedWatchCollectorV0};
pub use adapters::{
    CompletedObservationFenceOutcomeV0, KubeAutomationObservationPortV0, KubeAutomationPortV0,
    KubeDeletionObservationPortV0, KubeFixedPointPortV0, RetainedWatchCoordinationV0,
    UnixRendererPortV0, desired_observation,
};
pub use driver::{ControllerDriverV0, DriverConfigV0, DriverExitV0};
pub use engine::reconcile_with_watch_restarts;

use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
};

use oxideloop_bundle_archive::{ArchiveError, install_archive};
use oxideloop_bundle_contract::{
    BundleDescriptorV0, BundleError, BundleStore, ExecutionPhaseV0, InstalledBundle,
};
use oxideloop_controller_api::{
    AutomationRunCompletion, AutomationRunCompletionState, AutomationRunSpec, AutomationRunStatus,
    InventoryEntry, ObjectIdentity, WebApplicationSpec, WebApplicationStatus,
};
use oxideloop_controller_core::{
    AutomationRunCleanupV0, CommandV0, DesiredObservationV0, EventV0, InternalReconcileCauseV0,
    MachineStateV0, PhaseV0, PreconditionsV0, RootSnapshotV0, reduce,
};
use oxideloop_event_planner::{
    ObjectRef as PlannerObjectRefV0, PlannedEventV0, PlannerError, RunCorrelationV0, plan_event,
    semantically_equal,
};
use oxideloop_kube_transport::{
    DiscoveryPortV0, KubeTransportV0, ListV0, ObjectRefV0, ResourceKindV0, TransportErrorV0,
    WatchEventV0, WatchHandleV0,
};
use oxideloop_renderer_abi::{
    model::{ActionInputV0, ActionOutputV0, Dependency, RenderInputV0, RenderOutputV0},
    parse_validate_action_output, parse_validate_render_output,
};
use oxideloop_renderer_protocol::{
    PhaseV0 as RendererPhaseV0, RequestIdV0, RequestV0, ResponseStatusV0, ResponseV0,
};
use renderer_client::RendererClient;
use serde_json::Value;
use thiserror::Error;

pub const MAX_DISCOVERY_PASSES: u8 = 8;
pub(crate) const MAX_SEQUENCING_RETRIES: u8 = 4;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ActorKeyV0(pub String);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarrierV0 {
    pub generation: u64,
    pub cache_epoch: u64,
}

/// A prune candidate's first observation is never sufficient to delete.  The
/// scheduler must request a new list/watch/reread epoch rather than treating
/// the barrier that discovered it as the confirming observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarrierScheduleV0 {
    ImmediateFreshBarrier(BarrierV0),
}

#[must_use]
pub fn schedule_prune_confirmation(
    current: BarrierV0,
    prune_pending: bool,
    delete_issued: bool,
) -> Option<BarrierScheduleV0> {
    if prune_pending && !delete_issued {
        // A second prune observation needs a fresh list/watch/reread fence,
        // not a fabricated global dependency generation.  The completed
        // fence gets its own unique token even when this global barrier is
        // unchanged.
        Some(BarrierScheduleV0::ImmediateFreshBarrier(current))
    } else {
        None
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModeV0 {
    Running,
    Hold(String),
    Terminal,
}

type RootObservationFingerprintV0 = (
    String,
    i64,
    String,
    String,
    bool,
    String,
    String,
    bool,
    bool,
);

/// A non-forgeable retained-watch delta. Only the retained observation
/// adapter can derive it from canonical final observations; it carries no
/// caller-chosen renderer token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenuineChangeV0 {
    barrier: BarrierV0,
    retained_incarnation: u64,
    fingerprint: String,
}

/// An authenticated retained-watch transition that arrived while a durable
/// command still owns the actor.  It is deliberately private: only the
/// retained-watch coordinator can create the underlying change, and only the
/// actor can turn it into its internal reconciliation event after the durable
/// acknowledgement has reduced to `Ready`.
#[derive(Clone, Debug)]
struct DeferredGenuineChangeV0 {
    change: GenuineChangeV0,
    observation_token: String,
    cache_epoch: String,
}

/// A readiness acknowledgement is valid only for the exact root snapshot and
/// retained cache barrier that admitted it. Keeping the full root snapshot is
/// intentionally conservative: any changed root pin drops the signal rather
/// than allowing an old informer pass to issue a render command.
#[derive(Clone, Debug)]
struct QueuedInformerReadinessV0 {
    barrier: BarrierV0,
    root: RootSnapshotV0,
}

impl QueuedInformerReadinessV0 {
    fn current(barrier: BarrierV0, root: &RootSnapshotV0) -> Self {
        Self {
            barrier,
            root: root.clone(),
        }
    }

    fn matches(&self, barrier: BarrierV0, root: &RootSnapshotV0) -> bool {
        self.barrier == barrier && self.root == *root
    }
}

/// Token-bearing dependency wakeups use the same sealed root/barrier pin as
/// informer readiness. The reducer still receives the canonical core event,
/// but only after the runtime has established that it remains current.
#[derive(Clone, Debug)]
struct QueuedObservationV0 {
    barrier: BarrierV0,
    root: RootSnapshotV0,
    event: EventV0,
}

impl QueuedObservationV0 {
    fn reconcile(
        barrier: BarrierV0,
        root: &RootSnapshotV0,
        observation_token: String,
        cache_epoch: String,
    ) -> Self {
        Self {
            barrier,
            root: root.clone(),
            event: EventV0::ReconcileRequested {
                cause: InternalReconcileCauseV0::DependencyWatch,
                observation_token,
                cache_epoch,
            },
        }
    }

    fn matches(&self, barrier: BarrierV0, root: &RootSnapshotV0) -> bool {
        self.barrier == barrier && self.root == *root
    }
}

/// Opaque proof that a deletion list/reread was performed for the exact
/// in-flight deletion-observation command. Its fields are private so a driver
/// cannot turn a caller-provided `DeletionRevalidated` event into authority.
pub struct ValidatedDeletionObservationV0 {
    request_token: String,
    preconditions: PreconditionsV0,
    observed_inventory: Vec<InventoryEntry>,
    observed_automation_runs: Vec<AutomationRunCleanupV0>,
}

/// One-shot descriptor authority captured from an owned `RequestDescriptor`.
/// The asynchronous ConfigMap/archive operation never needs to borrow the
/// actor after this value is issued.
#[derive(Clone, Debug)]
pub(crate) struct DescriptorCommandCapabilityV0 {
    token: String,
    preconditions: PreconditionsV0,
    program: oxideloop_controller_api::ProgramBundleReference,
    bundle_digest: String,
}

impl DescriptorCommandCapabilityV0 {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }
    pub(crate) fn preconditions(&self) -> &PreconditionsV0 {
        &self.preconditions
    }
    pub(crate) fn program(&self) -> &oxideloop_controller_api::ProgramBundleReference {
        &self.program
    }
    pub(crate) fn bundle_digest(&self) -> &str {
        &self.bundle_digest
    }
}

/// Exact deletion authority captured before the actor-free list/reread
/// future begins.  It carries only authenticated root/state/journal pins.
#[derive(Clone, Debug)]
pub(crate) struct DeletionCommandCapabilityV0 {
    preconditions: PreconditionsV0,
    request_token: String,
    root: RootSnapshotV0,
    durable: oxideloop_controller_api::StateBody,
}

impl DeletionCommandCapabilityV0 {
    pub(crate) fn preconditions(&self) -> &PreconditionsV0 {
        &self.preconditions
    }
    pub(crate) fn request_token(&self) -> &str {
        &self.request_token
    }
    pub(crate) fn root(&self) -> &RootSnapshotV0 {
        &self.root
    }
    pub(crate) fn durable(&self) -> &oxideloop_controller_api::StateBody {
        &self.durable
    }
}

/// Opaque acknowledgement authority for one exact owned render command.  It
/// is minted only after the concrete Kubernetes fixed-point port has consumed
/// its one-shot provenance and completed the full retained-observation fence.
/// In particular, a driver cannot turn a raw `Rendered`/`Revalidated` event,
/// or an arbitrary fence token, into reducer authority.
#[derive(Debug, PartialEq)]
pub struct ValidatedRenderObservationV0 {
    request_id: String,
    preconditions: PreconditionsV0,
    event: EventV0,
    fence: CompletedObservationFenceV0,
}

impl ValidatedRenderObservationV0 {
    #[must_use]
    pub fn fence(&self) -> &CompletedObservationFenceV0 {
        &self.fence
    }

    #[must_use]
    pub fn token(&self) -> &str {
        self.fence.token()
    }

    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.fence.sequence()
    }

    #[must_use]
    pub fn barrier(&self) -> BarrierV0 {
        self.fence.barrier()
    }

    #[must_use]
    pub fn desired_observations(&self) -> &[DesiredObservationV0] {
        self.fence.desired_observations()
    }
}

/// A render provenance seed is captured only while the actor owns an exact
/// `RequestRender` command. The concrete fixed-point adapter finishes it once a
/// retained barrier incarnation exists.
#[derive(Clone, Debug)]
struct FixedPointProvenanceSeedV0 {
    request_id: String,
    preconditions: PreconditionsV0,
    root_name: String,
    root_namespace: String,
    root_uid: String,
    root_generation: i64,
    root_spec: Value,
    bundle_digest: String,
    barrier: BarrierV0,
    port_identity: u64,
}

#[derive(Debug)]
struct FixedPointProvenanceV0 {
    seed: FixedPointProvenanceSeedV0,
    retained_incarnation: u64,
}

impl FixedPointProvenanceSeedV0 {
    fn complete(self, retained_incarnation: u64) -> FixedPointProvenanceV0 {
        FixedPointProvenanceV0 {
            seed: self,
            retained_incarnation,
        }
    }
}

/// One-shot render authority captured before an actor-free fixed-point future
/// starts. The concrete port identity is pinned so another port cannot turn a
/// renderer result into an acknowledgement.
#[derive(Debug)]
pub(crate) struct RenderCommandCapabilityV0 {
    input: RenderInputV0,
    seed: FixedPointProvenanceSeedV0,
    port_identity: u64,
}

impl RenderCommandCapabilityV0 {
    pub(crate) fn input(&self) -> &RenderInputV0 {
        &self.input
    }
    pub(crate) fn port_identity(&self) -> u64 {
        self.port_identity
    }
    pub(crate) fn complete(self, incarnation: u64) -> FixedPointProvenanceV0 {
        self.seed.complete(incarnation)
    }
}

impl FixedPointProvenanceV0 {
    fn request(&self) -> (&PreconditionsV0, &str) {
        (&self.seed.preconditions, &self.seed.request_id)
    }
}

impl GenuineChangeV0 {
    fn from_observations(
        observations: &[oxideloop_renderer_abi::model::Observation],
        barrier: BarrierV0,
        retained_incarnation: u64,
    ) -> Result<Self, RuntimeErrorV0> {
        let snapshot = serde_json::to_value(observations).map_err(|_| {
            RuntimeErrorV0::Hold("retained dependency observations cannot be canonicalized".into())
        })?;
        let canonical = oxideloop_renderer_abi::canonical_bytes(&snapshot).map_err(|_| {
            RuntimeErrorV0::Hold("retained dependency observations cannot be canonicalized".into())
        })?;
        Ok(Self {
            barrier,
            retained_incarnation,
            fingerprint: oxideloop_renderer_abi::sha256_hex(&canonical),
        })
    }
}

/// The only crash simulation points permitted by the reusable action driver.
/// This is deliberately instance owned: production wiring decides whether to
/// expose it, while tests can prove recovery without an environment variable
/// or a process-global switch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum FailpointV0 {
    AfterEventBeforeStatus,
    AfterStatusBeforeAck,
    #[cfg(feature = "test-failpoints")]
    AfterStateFinalizerBeforeRootFinalizer,
}

/// A one-shot, atomically consumed failpoint arm.  The state is small enough
/// to keep inside an engine instance; no socket server is part of P6.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FailpointsV0 {
    armed: Option<FailpointV0>,
}

impl FailpointsV0 {
    pub fn arm(&mut self, point: FailpointV0) -> Result<(), RuntimeErrorV0> {
        if self.armed.is_some() {
            return Err(RuntimeErrorV0::Hold("a failpoint is already armed".into()));
        }
        self.armed = Some(point);
        Ok(())
    }

    #[must_use]
    pub fn take(&mut self, point: FailpointV0) -> bool {
        if self.armed == Some(point) {
            self.armed = None;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Error)]
pub enum RuntimeErrorV0 {
    #[error("hold: {0}")]
    Hold(String),
    #[error("one command is already in flight")]
    InFlight,
    #[error("core: {0}")]
    Core(String),
    #[error("discovery: {0}")]
    Discovery(#[from] TransportErrorV0),
    #[error("archive: {0}")]
    Archive(#[from] ArchiveError),
    #[error("bundle: {0}")]
    Bundle(#[from] BundleError),
    #[error("planner: {0}")]
    Planner(#[from] PlannerError),
    #[error("renderer response rejected")]
    Renderer,
    #[error("renderer ABI output rejected")]
    RendererAbi,
    #[error("injected crash at {0:?}")]
    InjectedCrash(FailpointV0),
}

/// Renderer boundary: protocol DTOs only, and exactly one call at a time.
#[async_trait]
pub trait RendererPortV0: Send {
    async fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0>;
}
/// Archive acquisition has no environment switch or implicit network policy.
pub trait ArchiveSourceV0 {
    fn fetch(&mut self, digest: &str) -> Result<Vec<u8>, RuntimeErrorV0>;
}
/// P6 can materialize only the P7 typed plan, never a generic Event body.
#[async_trait]
pub trait EventPortV0: Send {
    async fn create_or_exact_recover_event(
        &mut self,
        plan: &PlannedEventV0,
        event_time: &str,
    ) -> Result<(), RuntimeErrorV0>;
}

/// Typed, fakeable persistence boundary for an already-created `AutomationRun`.
/// The concrete Kubernetes adapter maps these methods to its purpose-named
/// event and `/status` operations; it is intentionally not a generic writer.
#[async_trait]
pub trait AutomationPortV0: Send {
    async fn create_or_exact_recover_event(
        &mut self,
        plan: &PlannedEventV0,
        event_time: &str,
    ) -> Result<oxideloop_kube_transport::EventMaterializationRefV0, RuntimeErrorV0>;
    async fn persist_completion(
        &mut self,
        namespace: &str,
        name: &str,
        expected_uid: &str,
        expected_resource_version: &str,
        expected_spec: &AutomationRunSpec,
        status: &AutomationRunStatus,
    ) -> Result<oxideloop_kube_transport::AutomationCompletionAckV0, RuntimeErrorV0>;
}

/// Immutable live references supplied by the exact rereads that ensured the
/// run. Keeping these as planner DTOs makes P7's root/source/run correlation
/// explicit and independently fakeable.
#[derive(Clone, Debug)]
pub struct AutomationExecutionV0 {
    pub root: PlannerObjectRefV0,
    pub source: PlannerObjectRefV0,
    pub run: PlannerObjectRefV0,
    pub correlation: RunCorrelationV0,
}

pub struct BundleRuntimeV0 {
    store: BundleStore,
}

/// Explicit local archive capability used by the v0 binary.  It deliberately
/// has no URL or environment fallback: the caller supplies the archive file
/// on the command line and the archive verifier still authenticates it by its
/// expected digest before publication in the store.
#[derive(Clone, Debug)]
pub struct FileArchiveSourceV0 {
    archive: PathBuf,
}

impl FileArchiveSourceV0 {
    #[must_use]
    pub fn new(archive: impl Into<PathBuf>) -> Self {
        Self {
            archive: archive.into(),
        }
    }
}

impl ArchiveSourceV0 for FileArchiveSourceV0 {
    fn fetch(&mut self, _digest: &str) -> Result<Vec<u8>, RuntimeErrorV0> {
        std::fs::read(&self.archive)
            .map_err(|error| RuntimeErrorV0::Hold(format!("bundle archive is unreadable: {error}")))
    }
}
impl BundleRuntimeV0 {
    pub fn new(store: BundleStore) -> Self {
        Self { store }
    }
    pub fn install_or_open<S: ArchiveSourceV0>(
        &self,
        source: &mut S,
        digest: &str,
    ) -> Result<InstalledBundle, RuntimeErrorV0> {
        match self.store.open_installed(digest) {
            Ok(bundle) => Ok(bundle),
            Err(_) => Ok(install_archive(
                &self.store,
                &source.fetch(digest)?,
                digest,
            )?),
        }
    }
    pub fn module(
        &self,
        bundle: &InstalledBundle,
        phase: ExecutionPhaseV0,
        identity: &str,
    ) -> Result<Vec<u8>, RuntimeErrorV0> {
        Ok(bundle.open_phase_module(phase, identity)?.bytes().to_vec())
    }
    pub fn descriptor(&self, bundle: &InstalledBundle) -> Result<Vec<u8>, RuntimeErrorV0> {
        Ok(bundle
            .open_declared(&bundle.manifest().descriptor.path)?
            .bytes()
            .to_vec())
    }
}

/// Actor state is strictly local: a tokenless readiness signal is accepted only
/// when its local generation and cache epoch still match.
pub struct RootActorV0 {
    state: MachineStateV0,
    mode: ModeV0,
    in_flight: bool,
    owned_in_flight: Option<CommandV0>,
    barrier: BarrierV0,
    /// Reducer/effect acknowledgements are durable work. Observation events may
    /// be discarded when a watch cache is invalidated; these may not.
    priority_ack: Option<EventV0>,
    core_events: VecDeque<EventV0>,
    pending_root_refresh: Option<RootSnapshotV0>,
    pending_informer_readiness: Option<QueuedInformerReadinessV0>,
    observation_events: VecDeque<QueuedObservationV0>,
    commands: VecDeque<CommandV0>,
    /// Dedupe is scoped to the retained-barrier incarnation and bounded to
    /// the most recently delivered canonical snapshot. A retained handle is
    /// dropped after every genuine transition, so older deliveries cannot
    /// re-enter under a later barrier incarnation.
    last_dependency_observation: Option<(BarrierV0, u64, String)>,
    /// Latest genuine retained snapshot received while an effect, durable
    /// persistence, or closure command owns the in-flight slot.  It must not
    /// cancel that command; delivery is deferred until the actor is quiescent
    /// and `Ready`.
    deferred_genuine_change: Option<DeferredGenuineChangeV0>,
    last_root_observation: Option<RootObservationFingerprintV0>,
    active_render_input: Option<(BarrierV0, RenderInputV0)>,
    render_provenance_request: Option<String>,
}
impl RootActorV0 {
    pub fn new(state: MachineStateV0) -> Self {
        Self {
            state,
            mode: ModeV0::Running,
            in_flight: false,
            owned_in_flight: None,
            barrier: BarrierV0 {
                generation: 0,
                cache_epoch: 0,
            },
            priority_ack: None,
            core_events: VecDeque::new(),
            pending_root_refresh: None,
            pending_informer_readiness: None,
            observation_events: VecDeque::new(),
            commands: VecDeque::new(),
            last_dependency_observation: None,
            deferred_genuine_change: None,
            last_root_observation: None,
            active_render_input: None,
            render_provenance_request: None,
        }
    }
    pub fn barrier(&self) -> BarrierV0 {
        self.barrier
    }
    pub fn mode(&self) -> &ModeV0 {
        &self.mode
    }
    pub fn state(&self) -> &MachineStateV0 {
        &self.state
    }

    /// Seals the only completion inputs accepted by the retained-observation
    /// adapter. The final render result came from the fixed-point loop (its
    /// fields have no public constructor), while inventory and already-used
    /// prune fence tokens come directly from this actor's validated durable
    /// journal.
    fn completed_observation_session(
        &self,
        final_result: &FixedPointResultV0,
        retained_barrier: BarrierV0,
        retained_incarnation: u64,
        port_identity: u64,
        provenance: &FixedPointProvenanceV0,
    ) -> Result<CompletedObservationSessionV0, RuntimeErrorV0> {
        let root_snapshot = self.state.root();
        if root_snapshot.uid.is_empty()
            || root_snapshot.namespace.is_empty()
            || root_snapshot.name.is_empty()
        {
            return Err(RuntimeErrorV0::Hold(
                "completed observation session root identity is invalid".into(),
            ));
        }
        let (active_barrier, active_input) =
            self.active_render_input.as_ref().ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "completed observation session has no active fixed-point input pin".into(),
                )
            })?;
        if *active_barrier != self.barrier
            || retained_barrier != self.barrier
            || final_result.input != *active_input
            || provenance.seed.port_identity != port_identity
            || provenance.retained_incarnation != retained_incarnation
            || provenance.seed.barrier != self.barrier
            || provenance.seed.root_name != root_snapshot.name
            || provenance.seed.root_namespace != root_snapshot.namespace
            || provenance.seed.root_uid != root_snapshot.uid
            || provenance.seed.root_generation != root_snapshot.generation
            || provenance.seed.root_spec
                != serde_json::to_value(&root_snapshot.spec).map_err(|_| {
                    RuntimeErrorV0::Hold(
                        "completed observation session root spec cannot be canonicalized".into(),
                    )
                })?
            || provenance.seed.bundle_digest != root_snapshot.bundle_digest
            || !preconditions_match_root(&provenance.seed.preconditions, root_snapshot)
            || final_result.input.observation_token
                != provenance.seed.preconditions.observation_token
            || final_result.input.cache_epoch != provenance.seed.preconditions.cache_epoch
            || provenance.seed.request_id != fixed_point_request_id(&provenance.seed.preconditions)
            || !matches!(
                self.owned_in_flight.as_ref(),
                Some(CommandV0::RequestRender { preconditions })
                    if preconditions == &provenance.seed.preconditions
            )
            || final_result.input.root.spec != provenance.seed.root_spec
            || !render_input_matches_root_and_barrier(
                &final_result.input,
                root_snapshot,
                self.barrier,
            )
        {
            return Err(RuntimeErrorV0::Hold(
                "completed observation session fixed-point input does not match the current actor barrier".into(),
            ));
        }
        let desired_identities =
            desired_resource_identities(&final_result.output, &root_snapshot.namespace)?;
        let durable = &self.state.journal().state;
        let durable_prune_tokens = durable
            .prune_candidate
            .iter()
            .flat_map(|candidate| {
                std::iter::once(candidate.first_observation.barrier_token.clone()).chain(
                    candidate
                        .second_observation
                        .iter()
                        .map(|observation| observation.barrier_token.clone()),
                )
            })
            .collect();
        Ok(CompletedObservationSessionV0 {
            root: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: root_snapshot.namespace.clone(),
                name: root_snapshot.name.clone(),
            },
            root_uid: root_snapshot.uid.clone(),
            dependencies: final_result.dependencies.clone(),
            desired_identities,
            durable_inventory: durable.inventory.clone(),
            durable_prune_tokens,
            retained_barrier,
            retained_incarnation,
        })
    }

    fn begin_fixed_point_provenance(
        &mut self,
        input: &RenderInputV0,
        port_identity: u64,
    ) -> Result<FixedPointProvenanceSeedV0, RuntimeErrorV0> {
        let Some(CommandV0::RequestRender { preconditions }) = self.owned_in_flight.as_ref() else {
            return Err(RuntimeErrorV0::Hold(
                "fixed-point provenance requires an owned RequestRender command".into(),
            ));
        };
        let root = self.state.root();
        let root_spec = serde_json::to_value(&root.spec).map_err(|_| {
            RuntimeErrorV0::Hold("fixed-point provenance root spec cannot be canonicalized".into())
        })?;
        if !preconditions_match_root(preconditions, root)
            || !render_input_matches_root_and_barrier(input, root, self.barrier)
            || input.root.spec != root_spec
            || input.observation_token != preconditions.observation_token
            || input.cache_epoch != preconditions.cache_epoch
        {
            return Err(RuntimeErrorV0::Hold(
                "fixed-point provenance input does not match the owned render command".into(),
            ));
        }
        let request_id = fixed_point_request_id(preconditions);
        if self.render_provenance_request.is_some() {
            return Err(RuntimeErrorV0::Hold(
                "an owned RequestRender command already issued a fixed-point provenance capability"
                    .into(),
            ));
        }
        let seed = FixedPointProvenanceSeedV0 {
            request_id: request_id.clone(),
            preconditions: preconditions.clone(),
            root_name: root.name.clone(),
            root_namespace: root.namespace.clone(),
            root_uid: root.uid.clone(),
            root_generation: root.generation,
            root_spec,
            bundle_digest: root.bundle_digest.clone(),
            barrier: self.barrier,
            port_identity,
        };
        self.render_provenance_request = Some(request_id);
        Ok(seed)
    }

    fn validated_deletion_observation(
        &self,
        preconditions: PreconditionsV0,
        request_token: String,
        observed_inventory: Vec<InventoryEntry>,
        observed_automation_runs: Vec<AutomationRunCleanupV0>,
    ) -> Result<ValidatedDeletionObservationV0, RuntimeErrorV0> {
        let Some(CommandV0::RequestDeletionObservation {
            preconditions: owned_preconditions,
        }) = self.owned_in_flight.as_ref()
        else {
            return Err(RuntimeErrorV0::Hold(
                "deletion observation has no owned RequestDeletionObservation command".into(),
            ));
        };
        if !self.state.root().deleting
            || &preconditions != owned_preconditions
            || !preconditions_match_root(&preconditions, self.state.root())
            || request_token != deletion_observation_request_token(&preconditions)
        {
            return Err(RuntimeErrorV0::Hold(
                "deletion observation pins do not match the owned command".into(),
            ));
        }
        Ok(ValidatedDeletionObservationV0 {
            request_token,
            preconditions,
            observed_inventory,
            observed_automation_runs,
        })
    }

    fn validated_render_observation(
        &self,
        final_result: &FixedPointResultV0,
        preconditions: &PreconditionsV0,
        request_id: &str,
        fence: CompletedObservationFenceV0,
    ) -> Result<ValidatedRenderObservationV0, RuntimeErrorV0> {
        let Some(CommandV0::RequestRender {
            preconditions: owned_preconditions,
        }) = self.owned_in_flight.as_ref()
        else {
            return Err(RuntimeErrorV0::Hold(
                "render observation has no owned RequestRender command".into(),
            ));
        };
        let root = self.state.root();
        let Some((active_barrier, active_input)) = self.active_render_input.as_ref() else {
            return Err(RuntimeErrorV0::Hold(
                "render observation has no active fixed-point input pin".into(),
            ));
        };
        if owned_preconditions != preconditions
            || request_id != fixed_point_request_id(preconditions)
            || !preconditions_match_root(preconditions, root)
            || *active_barrier != self.barrier
            || active_input != final_result.input()
            || !render_input_matches_root_and_barrier(final_result.input(), root, self.barrier)
            || fence.barrier != self.barrier
            || fence.token.is_empty()
        {
            return Err(RuntimeErrorV0::Hold(
                "render observation does not match the owned render capability".into(),
            ));
        }
        let render_input = canonical_render_input(final_result.input())?;
        let raw_output = serde_json::to_string(final_result.output())
            .map_err(|_| RuntimeErrorV0::RendererAbi)?;
        let render_output = parse_validate_render_output(&raw_output)
            .map_err(|_| RuntimeErrorV0::RendererAbi)?
            .canonical_bytes;
        let render_input_json =
            String::from_utf8(render_input.clone()).map_err(|_| RuntimeErrorV0::RendererAbi)?;
        let render_output_json =
            String::from_utf8(render_output).map_err(|_| RuntimeErrorV0::RendererAbi)?;
        let frozen = oxideloop_controller_core::FrozenInputV0 {
            root_generation: preconditions.root_generation,
            state_resource_version: preconditions.state_resource_version.clone(),
            observation_token: preconditions.observation_token.clone(),
            cache_epoch: preconditions.cache_epoch.clone(),
            final_observation_fence_token: fence.token.clone(),
            render_input_hash: oxideloop_renderer_abi::sha256_hex(&render_input),
        };
        let observed = fence.desired_observations.clone();
        let event = match self.state.phase() {
            // A concrete actor can only own RequestRender in RenderPending or
            // RevalidatePending. InformersPending is retained for the narrow
            // in-crate fixture path that proves a manually armed command is
            // still subject to the same private capability checks.
            PhaseV0::RenderPending | PhaseV0::InformersPending => EventV0::Rendered {
                frozen,
                render_input_json,
                render_output_json,
                observed,
            },
            PhaseV0::RevalidatePending => EventV0::Revalidated {
                frozen,
                render_input_json,
                render_output_json,
                observed,
            },
            _ => {
                return Err(RuntimeErrorV0::Hold(
                    "render observation acknowledgement phase mismatch".into(),
                ));
            }
        };
        Ok(ValidatedRenderObservationV0 {
            request_id: request_id.into(),
            preconditions: preconditions.clone(),
            event,
            fence,
        })
    }

    fn deletion_observation_request(&self) -> Result<(PreconditionsV0, String), RuntimeErrorV0> {
        let Some(CommandV0::RequestDeletionObservation { preconditions }) =
            self.owned_in_flight.as_ref()
        else {
            return Err(RuntimeErrorV0::Hold(
                "deletion observation has no owned RequestDeletionObservation command".into(),
            ));
        };
        if !self.state.root().deleting
            || !preconditions_match_root(preconditions, self.state.root())
        {
            return Err(RuntimeErrorV0::Hold(
                "deletion observation request pins do not match the current root".into(),
            ));
        }
        Ok((
            preconditions.clone(),
            deletion_observation_request_token(preconditions),
        ))
    }

    /// Captures an exact descriptor capability before an actor-free archive
    /// read/install future is pinned by the driver.
    pub(crate) fn issue_descriptor_capability(
        &self,
    ) -> Result<DescriptorCommandCapabilityV0, RuntimeErrorV0> {
        let Some(CommandV0::RequestDescriptor {
            token,
            preconditions,
        }) = self.owned_in_flight.as_ref()
        else {
            return Err(RuntimeErrorV0::Hold(
                "descriptor capability has no owned request".into(),
            ));
        };
        let root = self.state.root();
        if !preconditions_match_root(preconditions, root) {
            return Err(RuntimeErrorV0::Hold(
                "descriptor capability root pins are stale".into(),
            ));
        }
        Ok(DescriptorCommandCapabilityV0 {
            token: token.clone(),
            preconditions: preconditions.clone(),
            program: root.spec.program.clone(),
            bundle_digest: root.bundle_digest.clone(),
        })
    }

    /// Captures the root/state/deletion journal fence before an actor-free
    /// deletion list/reread future is pinned by the driver.
    pub(crate) fn issue_deletion_capability(
        &self,
    ) -> Result<DeletionCommandCapabilityV0, RuntimeErrorV0> {
        let (preconditions, request_token) = self.deletion_observation_request()?;
        Ok(DeletionCommandCapabilityV0 {
            preconditions,
            request_token,
            root: self.state.root().clone(),
            durable: self.state.journal().state.clone(),
        })
    }

    /// Captures the exact fixed-point input currently being evaluated by the
    /// engine. This is crate-private so a production driver cannot mint a
    /// completion pin independently of the fixed-point loop.
    pub(crate) fn bind_fixed_point_input(&mut self, input: &RenderInputV0) {
        self.active_render_input = Some((self.barrier, input.clone()));
    }

    /// Captures the only render provenance capability before the renderer and
    /// retained-discovery future starts. It is intentionally crate-private:
    /// production callers must use the concrete fixed-point adapter.
    pub(crate) fn issue_render_capability(
        &mut self,
        input: RenderInputV0,
        port_identity: u64,
    ) -> Result<RenderCommandCapabilityV0, RuntimeErrorV0> {
        let seed = self.begin_fixed_point_provenance(&input, port_identity)?;
        self.bind_fixed_point_input(&input);
        Ok(RenderCommandCapabilityV0 {
            input,
            seed,
            port_identity,
        })
    }
    /// Starts the actor without exposing its internal event queue to a driver.
    /// Effect acknowledgements remain checked by [`Self::acknowledge`], while
    /// dependency wakeups are accepted only from the sealed retained-watch
    /// coordinator.
    pub fn start(&mut self) {
        self.enqueue(EventV0::Start);
    }

    pub(crate) fn enqueue(&mut self, event: EventV0) {
        match event {
            EventV0::RootRefreshed { root } => {
                self.drop_superseded_observation_queue();
                self.pending_root_refresh = Some(root);
            }
            // This crate-private test/recovery seam still cannot queue a raw
            // reducer readiness event. It is sealed to the current root and
            // cache barrier exactly like the public readiness callback.
            EventV0::InformersReady => self.queue_informer_readiness(self.barrier),
            event => self.core_events.push_back(event),
        }
    }

    fn queue_informer_readiness(&mut self, candidate: BarrierV0) {
        if candidate == self.barrier && matches!(self.mode, ModeV0::Running) {
            self.pending_informer_readiness = Some(QueuedInformerReadinessV0::current(
                candidate,
                self.state.root(),
            ));
        }
    }

    fn enqueue_observation_reconcile(&mut self, observation_token: String, cache_epoch: String) {
        self.observation_events
            .push_back(QueuedObservationV0::reconcile(
                self.barrier,
                self.state.root(),
                observation_token,
                cache_epoch,
            ));
    }

    fn take_current_observation_event(&mut self) -> Option<EventV0> {
        while let Some(observation) = self.observation_events.pop_front() {
            if observation.matches(self.barrier, self.state.root()) {
                return Some(observation.event);
            }
        }
        None
    }

    fn take_current_informer_readiness(&mut self) -> bool {
        self.pending_informer_readiness
            .take()
            .is_some_and(|readiness| readiness.matches(self.barrier, self.state.root()))
    }

    /// Root or cache-barrier replacement invalidates every queued
    /// observation-only transition. Durable acknowledgements and core events
    /// remain intact.
    fn drop_superseded_observation_queue(&mut self) {
        self.pending_informer_readiness = None;
        self.observation_events.clear();
        self.deferred_genuine_change = None;
    }
    /// Consumes a canonical retained-watch change. This crate-private handoff
    /// prevents external orchestration from manufacturing a dependency token
    /// or advancing the global renderer barrier without an authenticated
    /// watch/list/reread snapshot.
    pub(crate) fn record_genuine_dependency_change(&mut self, change: &GenuineChangeV0) -> bool {
        if change.barrier != self.barrier {
            return false;
        }
        let dedupe_key = (
            change.barrier,
            change.retained_incarnation,
            change.fingerprint.clone(),
        );
        if self.last_dependency_observation.as_ref() == Some(&dedupe_key) {
            return false;
        }
        self.last_dependency_observation = Some(dedupe_key);
        self.pending_informer_readiness = None;
        self.observation_events.clear();
        self.barrier.generation = self.barrier.generation.saturating_add(1);
        let observation_token = oxideloop_renderer_abi::sha256_hex(
            format!(
                "oxideloop-genuine-dependency-change/v0\0{}\0{}\0{}\0{}",
                self.state.root().uid,
                self.barrier.generation,
                self.barrier.cache_epoch,
                change.fingerprint,
            )
            .as_bytes(),
        );
        let deferred = DeferredGenuineChangeV0 {
            change: change.clone(),
            observation_token: observation_token.clone(),
            cache_epoch: self.barrier.cache_epoch.to_string(),
        };
        if self.delivery_is_quiescent() {
            self.active_render_input = None;
            self.render_provenance_request = None;
            self.enqueue_observation_reconcile(
                observation_token,
                self.barrier.cache_epoch.to_string(),
            );
            true
        } else if self.can_supersede_observation_only() {
            self.supersede_observation_only(observation_token)
        } else {
            // A durable command is still authoritative.  Keep its ownership
            // and coalesce to the latest authenticated final snapshot.
            self.deferred_genuine_change = Some(deferred);
            true
        }
    }

    fn delivery_is_quiescent(&self) -> bool {
        self.state.phase() == PhaseV0::Ready
            && !self.in_flight
            && self.owned_in_flight.is_none()
            && self.priority_ack.is_none()
            && self.core_events.is_empty()
            && self.pending_root_refresh.is_none()
            && self.pending_informer_readiness.is_none()
            && self.observation_events.is_empty()
            && self.commands.is_empty()
    }

    /// Delivers exactly one coalesced retained-watch transition after all
    /// durable reducer work has completed. The event is internal and its
    /// token was derived when the retained watch authenticated the snapshot;
    /// no driver-controlled token enters this path.
    fn deliver_deferred_genuine_change(&mut self) -> Result<(), RuntimeErrorV0> {
        if !self.delivery_is_quiescent() {
            return Ok(());
        }
        let Some(deferred) = self.deferred_genuine_change.take() else {
            return Ok(());
        };
        if deferred.change.barrier.cache_epoch != self.barrier.cache_epoch
            || deferred.cache_epoch != self.barrier.cache_epoch.to_string()
            || deferred.change.fingerprint.is_empty()
        {
            self.hold(
                "deferred genuine dependency change no longer matches the actor barrier".into(),
            );
            return Err(RuntimeErrorV0::Hold(
                "deferred genuine dependency change no longer matches the actor barrier".into(),
            ));
        }
        self.reduce_event(EventV0::ReconcileRequested {
            cause: InternalReconcileCauseV0::DependencyWatch,
            observation_token: deferred.observation_token,
            cache_epoch: deferred.cache_epoch,
        })
    }

    fn can_supersede_observation_only(&self) -> bool {
        matches!(self.owned_in_flight, Some(CommandV0::RequestRender { .. }))
            || (!self.in_flight
                && matches!(
                    self.state.phase(),
                    PhaseV0::InformersPending | PhaseV0::RenderPending | PhaseV0::RevalidatePending
                ))
    }

    /// Atomically consumes only a no-effect render/informer continuation and
    /// returns the reducer to `InformersPending` with authenticated pins. A
    /// durable effect is never cancelled by a retained watch event.
    fn supersede_observation_only(&mut self, observation_token: String) -> bool {
        if !self.can_supersede_observation_only() {
            return false;
        }
        let root = self.state.root();
        let preconditions = PreconditionsV0 {
            root_uid: root.uid.clone(),
            root_generation: root.generation,
            root_resource_version: root.root_resource_version.clone(),
            state_uid: root.state_uid.clone(),
            state_resource_version: root.state_resource_version.clone(),
            observation_token,
            cache_epoch: self.barrier.cache_epoch.to_string(),
        };
        self.in_flight = false;
        self.owned_in_flight = None;
        self.active_render_input = None;
        self.render_provenance_request = None;
        self.deferred_genuine_change = None;
        self.commands.clear();
        self.pending_informer_readiness = None;
        self.observation_events.clear();
        if self
            .reduce_event(EventV0::ObservationSuperseded { preconditions })
            .is_err()
        {
            self.hold("observation supersession reducer transition failed".into());
            return false;
        }
        true
    }
    /// Enqueues an exact fresh root snapshot once. Root authority is retained
    /// with durable core events across a watch restart; duplicate UID/RV/spec
    /// observations do not advance the global barrier.
    pub fn root_observed(&mut self, root: RootSnapshotV0) -> bool {
        let fingerprint = (
            root.uid.clone(),
            root.generation,
            root.root_resource_version.clone(),
            root.bundle_digest.clone(),
            root.deleting,
            root.state_uid.clone(),
            root.state_resource_version.clone(),
            root.root_finalizer_present,
            root.state_finalizer_present,
        );
        if self.last_root_observation.as_ref() == Some(&fingerprint) {
            return false;
        }
        self.last_root_observation = Some(fingerprint);
        self.barrier.generation = self.barrier.generation.saturating_add(1);
        self.active_render_input = None;
        self.render_provenance_request = None;
        // A root snapshot supersedes all dependency authority captured under
        // its predecessor; the root refresh itself will establish a new
        // retained observation barrier.
        self.drop_superseded_observation_queue();
        self.enqueue(EventV0::RootRefreshed { root });
        // A render/fixed-point pass has no Kubernetes mutation authority.  A
        // newer exact root snapshot observed after its actor-free future has
        // resolved must supersede that stale pass instead of leaving an
        // acknowledgement slot that can never match the new root pins.
        if matches!(self.owned_in_flight, Some(CommandV0::RequestRender { .. })) {
            self.in_flight = false;
            self.owned_in_flight = None;
            self.commands.clear();
        }
        true
    }

    /// True only while the exact observation-only render request captured by
    /// a concrete fixed-point capability is still acknowledgement-eligible.
    /// The driver uses this after applying buffered root/dependency records;
    /// it never turns a raw renderer result into a capability.
    pub(crate) fn owns_render_request(&self) -> bool {
        self.in_flight && matches!(self.owned_in_flight, Some(CommandV0::RequestRender { .. }))
    }
    pub fn next_command(&mut self) -> Result<Option<CommandV0>, RuntimeErrorV0> {
        if let ModeV0::Hold(reason) = &self.mode {
            return Err(RuntimeErrorV0::Hold(reason.clone()));
        }
        if self.in_flight {
            return Err(RuntimeErrorV0::InFlight);
        }
        // A transport acknowledgement is reducer authority. It has a
        // dedicated priority slot so a root watch delivered first cannot make
        // accepted effect facts stale. After accepting it, reduce the latest
        // coalesced root snapshot before issuing its persist/finalizer
        // continuation. The root transition replaces commands derived under
        // obsolete pins, except the post-finalizer exact UID absence proof.
        if let Some(ack) = self.priority_ack.take() {
            self.reduce_event(ack)?;
            if let Some(root) = self.pending_root_refresh.take() {
                if self.state.phase() != PhaseV0::RootDeletionVerifyPending {
                    self.commands.clear();
                }
                self.reduce_event(EventV0::RootRefreshed { root })?;
            }
        } else if let Some(root) = self.pending_root_refresh.take() {
            if self.state.phase() != PhaseV0::RootDeletionVerifyPending {
                self.commands.clear();
            }
            self.reduce_event(EventV0::RootRefreshed { root })?;
        } else {
            let event = self.core_events.pop_front().or_else(|| {
                self.take_current_informer_readiness()
                    .then_some(EventV0::InformersReady)
                    .or_else(|| self.take_current_observation_event())
            });
            if let Some(event) = event {
                self.reduce_event(event)?;
            }
        }
        if self.state.phase() == PhaseV0::Terminal {
            self.terminal_root_deleted();
            return Ok(None);
        }
        self.deliver_deferred_genuine_change()?;
        if let Some(command) = self.commands.pop_front() {
            self.in_flight = true;
            self.owned_in_flight = Some(command.clone());
            Ok(Some(command))
        } else {
            Ok(None)
        }
    }
    /// Release the one in-flight slot only after the effect's core event is
    /// durable/available to the reducer.
    pub fn acknowledge(&mut self, event: EventV0) -> Result<(), RuntimeErrorV0> {
        if matches!(
            event,
            EventV0::DeletionRevalidated { .. }
                | EventV0::Rendered { .. }
                | EventV0::Revalidated { .. }
        ) {
            self.hold("raw deletion or render events are not acknowledgement capabilities".into());
            return Err(RuntimeErrorV0::Hold(
                "raw deletion or render events are not acknowledgement capabilities".into(),
            ));
        }
        let Some(command) = self.owned_in_flight.as_ref() else {
            self.hold("effect acknowledgement arrived without an owned in-flight command".into());
            return Err(RuntimeErrorV0::Hold(
                "effect acknowledgement arrived without an owned in-flight command".into(),
            ));
        };
        if !self.acknowledgement_matches(command, &event) {
            self.hold(
                "effect acknowledgement does not exactly correlate to the owned in-flight command"
                    .into(),
            );
            return Err(RuntimeErrorV0::Hold(
                "effect acknowledgement does not exactly correlate to the owned in-flight command"
                    .into(),
            ));
        }
        self.accept_acknowledgement(event)
    }

    /// Consumes a sealed deletion observation minted by the exact
    /// list/reread adapter. There is intentionally no `EventV0` overload for
    /// this acknowledgement.
    pub fn acknowledge_deletion_observation(
        &mut self,
        observation: ValidatedDeletionObservationV0,
    ) -> Result<(), RuntimeErrorV0> {
        let Some(CommandV0::RequestDeletionObservation { preconditions }) =
            self.owned_in_flight.as_ref()
        else {
            self.hold("deletion observation arrived without an owned request".into());
            return Err(RuntimeErrorV0::Hold(
                "deletion observation arrived without an owned request".into(),
            ));
        };
        if preconditions != &observation.preconditions
            || observation.request_token
                != deletion_observation_request_token(&observation.preconditions)
            || !preconditions_match_root(preconditions, self.state.root())
        {
            self.hold("deletion observation does not match the owned request pins".into());
            return Err(RuntimeErrorV0::Hold(
                "deletion observation does not match the owned request pins".into(),
            ));
        }
        self.accept_acknowledgement(EventV0::DeletionRevalidated {
            observed_inventory: observation.observed_inventory,
            observed_automation_runs: observation.observed_automation_runs,
        })
    }

    /// Consumes the one exact render acknowledgement minted by the concrete
    /// fixed-point port after a full retained-observation fence.
    pub fn acknowledge_render_observation(
        &mut self,
        observation: ValidatedRenderObservationV0,
    ) -> Result<(), RuntimeErrorV0> {
        let Some(CommandV0::RequestRender { preconditions }) = self.owned_in_flight.as_ref() else {
            self.hold("render observation arrived without an owned request".into());
            return Err(RuntimeErrorV0::Hold(
                "render observation arrived without an owned request".into(),
            ));
        };
        if preconditions != &observation.preconditions
            || observation.request_id != fixed_point_request_id(preconditions)
            || !preconditions_match_root(preconditions, self.state.root())
            || observation.fence.barrier != self.barrier
            || self.render_provenance_request.as_deref() != Some(observation.request_id.as_str())
            || !matches!(
                &observation.event,
                EventV0::Rendered { frozen, .. } | EventV0::Revalidated { frozen, .. }
                    if frozen_matches_preconditions(frozen, preconditions)
            )
        {
            self.hold("render observation does not match the owned request pins".into());
            return Err(RuntimeErrorV0::Hold(
                "render observation does not match the owned request pins".into(),
            ));
        }
        self.accept_acknowledgement(observation.event)
    }

    /// Queues one authenticated pending-run completion in the same priority
    /// lane as a transport acknowledgement. A completion is not an ordinary
    /// watch event: it must exactly match the durable pending occurrence, its
    /// immutable canonical spec, and the old resourceVersion captured when
    /// the run was ensured. This makes completion reduce before a concurrent
    /// root closure/rebase.
    pub(crate) fn acknowledge_automation_completion(
        &mut self,
        event: EventV0,
    ) -> Result<(), RuntimeErrorV0> {
        let EventV0::AutomationCompleted {
            spec,
            occurrence_id,
            name,
            uid,
            old_resource_version,
            ..
        } = &event
        else {
            return Err(RuntimeErrorV0::Hold(
                "automation completion priority path accepts only a completion".into(),
            ));
        };
        let pending = self
            .state
            .journal()
            .state
            .automation
            .pending
            .iter()
            .find(|pending| pending.occurrence_id == *occurrence_id)
            .ok_or_else(|| RuntimeErrorV0::Hold("completion has no durable pending run".into()))?;
        let canonical =
            oxideloop_renderer_abi::canonical_bytes(&serde_json::to_value(spec).map_err(|_| {
                RuntimeErrorV0::Hold("automation completion spec cannot be canonicalized".into())
            })?)
            .map_err(|_| {
                RuntimeErrorV0::Hold("automation completion spec cannot be canonicalized".into())
            })?;
        if spec.root_uid != self.state.root().uid
            || pending.run_name != *name
            || pending.run_uid != *uid
            || pending.run_resource_version != *old_resource_version
            || pending.spec_hash != oxideloop_renderer_abi::sha256_hex(&canonical)
        {
            return Err(RuntimeErrorV0::Hold(
                "automation completion does not match durable pending pins".into(),
            ));
        }
        if self.priority_ack.replace(event).is_some() {
            return Err(RuntimeErrorV0::Hold(
                "multiple priority acknowledgements attempted to occupy the priority slot".into(),
            ));
        }
        Ok(())
    }

    fn accept_acknowledgement(&mut self, event: EventV0) -> Result<(), RuntimeErrorV0> {
        self.in_flight = false;
        self.owned_in_flight = None;
        self.render_provenance_request = None;
        if self.priority_ack.replace(event).is_some() {
            self.hold("multiple acknowledgements attempted to occupy the priority slot".into());
            return Err(RuntimeErrorV0::Hold(
                "multiple acknowledgements attempted to occupy the priority slot".into(),
            ));
        }
        Ok(())
    }

    // Keep the complete command/event correlation matrix together: splitting
    // it would make an omitted acknowledgement variant harder to audit.
    #[allow(clippy::match_same_arms, clippy::too_many_lines)]
    fn acknowledgement_matches(&self, command: &CommandV0, event: &EventV0) -> bool {
        match (command, event) {
            (
                CommandV0::EnsureBootstrap {
                    token,
                    root_uid,
                    root_resource_version,
                    state_name,
                    ..
                },
                EventV0::BootstrapAcknowledged {
                    token: event_token,
                    state_name: event_state_name,
                    old_root_resource_version,
                    status_state_link,
                    ..
                },
            ) => {
                token == event_token
                    && state_name == event_state_name
                    && root_resource_version == old_root_resource_version
                    && root_uid == &status_state_link.root_uid
            }
            (
                CommandV0::Persist {
                    preconditions,
                    operation,
                    ..
                },
                EventV0::Persisted {
                    token,
                    old_state_resource_version,
                    state_uid,
                    ..
                },
            ) => {
                token == &operation.token
                    && old_state_resource_version == &operation.old_state_resource_version
                    && state_uid == &preconditions.state_uid
            }
            (
                CommandV0::PersistRootStatus {
                    token,
                    root_uid,
                    old_root_resource_version,
                    ..
                },
                EventV0::RootStatusPersisted {
                    token: event_token,
                    root_uid: event_root_uid,
                    old_root_resource_version: event_old_root_resource_version,
                    ..
                },
            ) => {
                token == event_token
                    && root_uid == event_root_uid
                    && old_root_resource_version == event_old_root_resource_version
            }
            (
                CommandV0::RequestDescriptor { token, .. },
                EventV0::DescriptorReady {
                    token: event_token,
                    bundle_digest,
                },
            ) => token == event_token && bundle_digest == &self.state.root().bundle_digest,
            (
                CommandV0::Mutate {
                    transaction_id,
                    attempt_token,
                    ..
                },
                EventV0::MutationAcknowledged {
                    transaction_id: event_transaction_id,
                    attempt_token: event_attempt_token,
                    ..
                }
                | EventV0::MutationFailed {
                    transaction_id: event_transaction_id,
                    attempt_token: event_attempt_token,
                    ..
                },
            ) => transaction_id == event_transaction_id && attempt_token == event_attempt_token,
            (
                CommandV0::EnsureAutomationRun {
                    token, name, spec, ..
                },
                EventV0::AutomationRunEnsured {
                    token: event_token,
                    name: event_name,
                    spec: event_spec,
                    ..
                },
            ) => token == event_token && name == event_name && spec == event_spec,
            (
                CommandV0::DeleteAutomationRun {
                    token,
                    occurrence_id,
                    name,
                    uid,
                    resource_version,
                    ..
                },
                EventV0::AutomationRunDeleted {
                    token: event_token,
                    occurrence_id: event_occurrence_id,
                    name: event_name,
                    uid: event_uid,
                    resource_version: event_resource_version,
                },
            ) => {
                token == event_token
                    && occurrence_id == event_occurrence_id
                    && name == event_name
                    && uid == event_uid
                    && resource_version == event_resource_version
            }
            (
                CommandV0::DeleteAutomationRun { token, .. },
                EventV0::AutomationRunDeleteFailed {
                    token: event_token, ..
                },
            ) => token == event_token,
            (
                CommandV0::VerifyOwnedChildrenAbsent {
                    token,
                    preconditions,
                },
                EventV0::OwnedChildrenVerifiedAbsent {
                    token: event_token,
                    root_uid,
                    root_generation,
                    root_resource_version,
                    state_uid,
                    state_resource_version,
                    cleanup_token,
                },
            ) => {
                token == event_token
                    && (self.state.is_root_only_deletion_recovery() || token == cleanup_token)
                    && root_uid == &preconditions.root_uid
                    && root_generation == &preconditions.root_generation
                    && root_resource_version == &preconditions.root_resource_version
                    && state_uid == &preconditions.state_uid
                    && state_resource_version == &preconditions.state_resource_version
            }
            (
                CommandV0::VerifyOwnedChildrenAbsent { token, .. },
                EventV0::OwnedChildrenVerificationFailed {
                    token: event_token, ..
                },
            ) => token == event_token,
            (
                CommandV0::RemoveStateFinalizer {
                    token,
                    preconditions,
                    ..
                },
                EventV0::StateFinalizerRemoved {
                    token: event_token,
                    state_uid,
                    old_state_resource_version,
                    ..
                },
            ) => {
                token == event_token
                    && state_uid == &preconditions.state_uid
                    && old_state_resource_version == &preconditions.state_resource_version
            }
            (
                CommandV0::RemoveRootFinalizer {
                    token,
                    preconditions,
                    ..
                },
                EventV0::RootFinalizerRemoved {
                    token: event_token,
                    root_uid,
                    old_root_resource_version,
                    ..
                },
            ) => {
                token == event_token
                    && root_uid == &preconditions.root_uid
                    && old_root_resource_version == &preconditions.root_resource_version
            }
            (
                CommandV0::VerifyRootUidAbsent {
                    token, root_uid, ..
                },
                EventV0::RootUidVerifiedAbsent {
                    token: event_token,
                    root_uid: event_root_uid,
                },
            ) => token == event_token && root_uid == event_root_uid,
            (
                CommandV0::VerifyRootUidAbsent {
                    token, root_uid, ..
                },
                EventV0::RootUidAbsenceFailed {
                    token: event_token,
                    observed_uid,
                },
            ) => token == event_token && root_uid != observed_uid,
            _ => false,
        }
    }

    fn reduce_event(&mut self, event: EventV0) -> Result<(), RuntimeErrorV0> {
        let transition = reduce(self.state.clone(), event)
            .map_err(|error| RuntimeErrorV0::Core(error.to_string()))?;
        self.state = transition.state;
        self.commands.extend(transition.commands);
        Ok(())
    }

    fn recover_stale_preflight(&mut self, root: RootSnapshotV0) {
        self.in_flight = false;
        self.owned_in_flight = None;
        self.render_provenance_request = None;
        self.commands.clear();
        self.drop_superseded_observation_queue();
        self.pending_root_refresh = Some(root);
    }
    /// `Hold` and `IgnoredStale` are local reducer commands. They are consumed
    /// by the driver and never manufactured as a transport acknowledgement.
    fn consume_internal(&mut self) {
        self.in_flight = false;
        self.owned_in_flight = None;
        self.render_provenance_request = None;
    }
    pub fn begin_observation_barrier(&mut self) -> BarrierV0 {
        // Starting a list/watch/reread pass is not itself a cluster change.
        // Only root/dependency change or watch invalidation advances the
        // global renderer-observation barrier.
        self.barrier
    }
    pub fn informers_ready(&mut self, candidate: BarrierV0) {
        self.queue_informer_readiness(candidate);
    }
    pub(crate) fn watch_invalidated(&mut self) {
        let prior_token = self
            .active_render_input
            .as_ref()
            .map(|(_, input)| input.observation_token.clone())
            .or_else(|| self.state.root().status.input_token.clone())
            .unwrap_or_else(|| self.state.root().uid.clone());
        self.barrier.cache_epoch += 1;
        self.barrier.generation += 1;
        self.pending_informer_readiness = None;
        self.observation_events.clear();
        self.last_dependency_observation = None;
        self.deferred_genuine_change = None;
        let observation_token = observation_token_for_barrier(&prior_token, self.barrier);
        if self.can_supersede_observation_only() {
            let _ = self.supersede_observation_only(observation_token);
        } else {
            self.active_render_input = None;
            self.render_provenance_request = None;
        }
        // A 410 only invalidates observation state. In-flight durable effects,
        // their acknowledgements, and command ownership remain intact.
    }
    pub fn hold(&mut self, reason: String) {
        self.mode = ModeV0::Hold(reason);
        self.core_events.clear();
        self.priority_ack = None;
        self.pending_root_refresh = None;
        self.pending_informer_readiness = None;
        self.observation_events.clear();
        self.commands.clear();
        self.in_flight = false;
        self.owned_in_flight = None;
        self.active_render_input = None;
        self.render_provenance_request = None;
        self.deferred_genuine_change = None;
    }
    pub fn terminal_root_deleted(&mut self) {
        self.mode = ModeV0::Terminal;
        self.core_events.clear();
        self.priority_ack = None;
        self.pending_root_refresh = None;
        self.pending_informer_readiness = None;
        self.observation_events.clear();
        self.commands.clear();
        self.in_flight = false;
        self.owned_in_flight = None;
        self.active_render_input = None;
        self.render_provenance_request = None;
    }
}

fn frozen_matches_preconditions(
    frozen: &oxideloop_controller_core::FrozenInputV0,
    preconditions: &oxideloop_controller_core::PreconditionsV0,
) -> bool {
    frozen.root_generation == preconditions.root_generation
        && frozen.state_resource_version == preconditions.state_resource_version
        // Bootstrap/recovery renders predate the first dependency barrier and
        // therefore have no token-bearing command preconditions. Once either
        // pin exists, the acknowledgement must match it exactly.
        && (preconditions.observation_token.is_empty()
            || frozen.observation_token == preconditions.observation_token)
        && (preconditions.cache_epoch.is_empty() || frozen.cache_epoch == preconditions.cache_epoch)
}

fn render_input_matches_root_and_barrier(
    input: &RenderInputV0,
    root: &RootSnapshotV0,
    barrier: BarrierV0,
) -> bool {
    input.root.api_version == "demo.oxideloop.dev/v1alpha1"
        && input.root.kind == "WebApplication"
        && input.root.namespace == root.namespace
        && input.root.name == root.name
        && input.root.uid == root.uid
        && u64::try_from(root.generation).ok() == Some(input.root.generation)
        && input.bundle_digest == root.bundle_digest
        && input.cache_epoch == barrier.cache_epoch.to_string()
        && !input.observation_token.is_empty()
}

fn preconditions_match_root(preconditions: &PreconditionsV0, root: &RootSnapshotV0) -> bool {
    preconditions.root_uid == root.uid
        && preconditions.root_generation == root.generation
        && preconditions.root_resource_version == root.root_resource_version
        && preconditions.state_uid == root.state_uid
        && preconditions.state_resource_version == root.state_resource_version
}

fn fixed_point_request_id(preconditions: &PreconditionsV0) -> String {
    oxideloop_renderer_abi::sha256_hex(
        format!(
            "oxideloop-fixed-point-request/v0\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            preconditions.root_uid,
            preconditions.root_generation,
            preconditions.root_resource_version,
            preconditions.state_uid,
            preconditions.state_resource_version,
            preconditions.observation_token,
            preconditions.cache_epoch,
        )
        .as_bytes(),
    )
}

fn deletion_observation_request_token(preconditions: &PreconditionsV0) -> String {
    oxideloop_renderer_abi::sha256_hex(
        format!(
            "oxideloop-deletion-observation-request/v0\0{}\0{}\0{}\0{}\0{}",
            preconditions.root_uid,
            preconditions.root_generation,
            preconditions.root_resource_version,
            preconditions.state_uid,
            preconditions.state_resource_version,
        )
        .as_bytes(),
    )
}

fn desired_resource_identities(
    output: &RenderOutputV0,
    root_namespace: &str,
) -> Result<Vec<ObjectIdentity>, RuntimeErrorV0> {
    let mut identities = BTreeMap::new();
    for resource in &output.resources {
        let metadata = resource
            .object
            .get("metadata")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                RuntimeErrorV0::Hold(
                    "final renderer resource lacks object metadata for observation fencing".into(),
                )
            })?;
        let identity = ObjectIdentity {
            api_version: resource
                .object
                .get("apiVersion")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    RuntimeErrorV0::Hold(
                        "final renderer resource lacks apiVersion for observation fencing".into(),
                    )
                })?
                .into(),
            kind: resource
                .object
                .get("kind")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    RuntimeErrorV0::Hold(
                        "final renderer resource lacks kind for observation fencing".into(),
                    )
                })?
                .into(),
            namespace: metadata
                .get("namespace")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(root_namespace)
                .into(),
            name: metadata
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    RuntimeErrorV0::Hold(
                        "final renderer resource lacks name for observation fencing".into(),
                    )
                })?
                .into(),
        };
        match (identity.api_version.as_str(), identity.kind.as_str()) {
            ("v1", "ConfigMap" | "Service") => {}
            _ => {
                return Err(RuntimeErrorV0::Hold(
                    "completed observation fence contains an unsupported desired identity".into(),
                ));
            }
        }
        if identity.namespace != root_namespace {
            return Err(RuntimeErrorV0::Hold(
                "final renderer resource escapes the root namespace".into(),
            ));
        }
        let key = format!(
            "{}\0{}\0{}\0{}",
            identity.api_version, identity.kind, identity.namespace, identity.name
        );
        if identities.insert(key, identity).is_some() {
            return Err(RuntimeErrorV0::Hold(
                "final renderer resources contain duplicate observation identities".into(),
            ));
        }
    }
    Ok(identities.into_values().collect())
}

#[derive(Default)]
pub struct ControllerRuntimeV0 {
    actors: BTreeMap<ActorKeyV0, RootActorV0>,
}

/// Validate the raw transport DTO before it becomes reducer authority. This is
/// deliberately local and narrow: callers must nominate the exact root key,
/// while the typed spec/status parsers reject unknown authored/status fields.
pub fn root_snapshot_from_object(
    object: &oxideloop_kube_transport::ObjectV0,
    namespace: &str,
    name: &str,
) -> Result<RootSnapshotV0, RuntimeErrorV0> {
    if object.reference.resource != ResourceKindV0::WebApplication
        || object.reference.namespace != namespace
        || object.reference.name != name
        || object.metadata.uid.is_empty()
        || object.metadata.resource_version.is_empty()
        || object.value.get("apiVersion").and_then(Value::as_str)
            != Some(ResourceKindV0::WebApplication.api_version())
        || object.value.get("kind").and_then(Value::as_str)
            != Some(ResourceKindV0::WebApplication.kind())
    {
        return Err(RuntimeErrorV0::Hold("root identity/GVK is invalid".into()));
    }
    let metadata = object
        .value
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| RuntimeErrorV0::Hold("root metadata is invalid".into()))?;
    if metadata.get("name").and_then(Value::as_str) != Some(name)
        || metadata.get("namespace").and_then(Value::as_str) != Some(namespace)
        || metadata.get("uid").and_then(Value::as_str) != Some(object.metadata.uid.as_str())
        || metadata.get("resourceVersion").and_then(Value::as_str)
            != Some(object.metadata.resource_version.as_str())
    {
        return Err(RuntimeErrorV0::Hold(
            "root metadata pins are invalid".into(),
        ));
    }
    let generation = metadata
        .get("generation")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or_else(|| RuntimeErrorV0::Hold("root generation is invalid".into()))?;
    let finalizers = metadata
        .get("finalizers")
        .and_then(Value::as_array)
        .ok_or_else(|| RuntimeErrorV0::Hold("root finalizers are invalid".into()))?;
    if finalizers.iter().any(|value| value.as_str().is_none()) {
        return Err(RuntimeErrorV0::Hold("root finalizers are invalid".into()));
    }
    let spec: WebApplicationSpec = serde_json::from_value(
        object
            .value
            .get("spec")
            .cloned()
            .ok_or_else(|| RuntimeErrorV0::Hold("root spec is missing".into()))?,
    )
    .map_err(|_| RuntimeErrorV0::Hold("root spec is invalid".into()))?;
    spec.validate()
        .map_err(|_| RuntimeErrorV0::Hold("root spec is invalid".into()))?;
    let status: WebApplicationStatus = serde_json::from_value(
        object
            .value
            .get("status")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .map_err(|_| RuntimeErrorV0::Hold("root status is invalid".into()))?;
    let state = status.state.clone();
    Ok(RootSnapshotV0 {
        uid: object.metadata.uid.clone(),
        namespace: namespace.into(),
        name: name.into(),
        generation,
        spec: spec.clone(),
        bundle_digest: spec.program.digest,
        status,
        state_name: state
            .as_ref()
            .map_or_else(String::new, |link| link.name.clone()),
        state_uid: state
            .as_ref()
            .map_or_else(String::new, |link| link.uid.clone()),
        state_resource_version: String::new(),
        root_resource_version: object.metadata.resource_version.clone(),
        root_finalizer_present: finalizers
            .iter()
            .any(|value| value.as_str() == Some(oxideloop_controller_api::ROOT_FINALIZER)),
        state_finalizer_present: false,
        deleting: metadata
            .get("deletionTimestamp")
            .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty())),
    })
}

fn recover_deleting_root_without_state(
    snapshot: RootSnapshotV0,
) -> Result<MachineStateV0, RuntimeErrorV0> {
    MachineStateV0::recover_deleting_root_without_state(snapshot)
        .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))
}

#[async_trait]
trait LoaderRereadPortV0: Sync {
    async fn reread_object(
        &self,
        reference: &ObjectRefV0,
    ) -> Result<Option<oxideloop_kube_transport::ObjectV0>, TransportErrorV0>;
}

#[async_trait]
impl LoaderRereadPortV0 for KubeTransportV0 {
    async fn reread_object(
        &self,
        reference: &ObjectRefV0,
    ) -> Result<Option<oxideloop_kube_transport::ObjectV0>, TransportErrorV0> {
        self.reread(reference).await
    }
}

async fn recover_unlinked_deleting_root<P: LoaderRereadPortV0>(
    transport: &P,
    namespace: &str,
    snapshot: RootSnapshotV0,
) -> Result<Option<MachineStateV0>, RuntimeErrorV0> {
    let receipt = oxideloop_controller_core::validate_deleting_root_receipt(&snapshot)
        .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))?;
    let state_ref = ObjectRefV0 {
        resource: ResourceKindV0::OxideLoopState,
        namespace: namespace.into(),
        name: receipt.state_name.clone(),
    };
    if transport.reread_object(&state_ref).await?.is_some() {
        return Err(RuntimeErrorV0::Hold(
            "deleting root has no state link but the deterministic state name is occupied".into(),
        ));
    }
    recover_deleting_root_without_state(snapshot).map(Some)
}

fn validate_state_status(state: &oxideloop_kube_transport::ObjectV0) -> Result<(), RuntimeErrorV0> {
    serde_json::from_value::<oxideloop_controller_api::OxideLoopStateStatus>(
        state
            .value
            .get("status")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .map(|_| ())
    .map_err(|_| RuntimeErrorV0::Hold("linked state status is invalid".into()))
}

fn install_linked_state_pins(
    snapshot: &mut RootSnapshotV0,
    namespace: &str,
    link: &oxideloop_controller_api::StateLink,
    state_ref: &ObjectRefV0,
    state: &oxideloop_kube_transport::ObjectV0,
) -> Result<(), RuntimeErrorV0> {
    if state.reference != *state_ref
        || state.metadata.uid != link.uid
        || state.metadata.resource_version.is_empty()
        || state.value.get("apiVersion").and_then(Value::as_str)
            != Some(ResourceKindV0::OxideLoopState.api_version())
        || state.value.get("kind").and_then(Value::as_str)
            != Some(ResourceKindV0::OxideLoopState.kind())
    {
        return Err(RuntimeErrorV0::Hold(
            "linked state identity/GVK is invalid".into(),
        ));
    }
    let metadata = state
        .value
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| RuntimeErrorV0::Hold("linked state metadata is invalid".into()))?;
    if metadata.get("name").and_then(Value::as_str) != Some(link.name.as_str())
        || metadata.get("namespace").and_then(Value::as_str) != Some(namespace)
        || metadata.get("uid").and_then(Value::as_str) != Some(state.metadata.uid.as_str())
        || metadata.get("resourceVersion").and_then(Value::as_str)
            != Some(state.metadata.resource_version.as_str())
    {
        return Err(RuntimeErrorV0::Hold(
            "linked state metadata pins are invalid".into(),
        ));
    }
    let finalizers = metadata
        .get("finalizers")
        .and_then(Value::as_array)
        .ok_or_else(|| RuntimeErrorV0::Hold("linked state finalizers are invalid".into()))?;
    if finalizers.iter().any(|value| value.as_str().is_none()) {
        return Err(RuntimeErrorV0::Hold(
            "linked state finalizers are invalid".into(),
        ));
    }
    snapshot.state_name.clone_from(&link.name);
    snapshot.state_uid.clone_from(&state.metadata.uid);
    snapshot
        .state_resource_version
        .clone_from(&state.metadata.resource_version);
    snapshot.state_finalizer_present = finalizers
        .iter()
        .any(|value| value.as_str() == Some(oxideloop_controller_api::STATE_FINALIZER));
    Ok(())
}

/// Obtains only fresh object identity and resource-version pins after a stale
/// write preflight. The state journal is deliberately not deserialized here:
/// accepted acknowledgement facts remain in the actor's in-memory journal and
/// are rebased by the reducer under these fresh pins.
async fn fresh_root_snapshot_from_transport<P: LoaderRereadPortV0>(
    transport: &P,
    namespace: &str,
    name: &str,
) -> Result<RootSnapshotV0, RuntimeErrorV0> {
    let root_ref = ObjectRefV0 {
        resource: ResourceKindV0::WebApplication,
        namespace: namespace.into(),
        name: name.into(),
    };
    let root = transport.reread_object(&root_ref).await?.ok_or_else(|| {
        RuntimeErrorV0::Hold("root disappeared during stale preflight recovery".into())
    })?;
    let mut snapshot = root_snapshot_from_object(&root, namespace, name)?;
    let Some(link) = snapshot.status.state.clone() else {
        if !snapshot.deleting {
            return Ok(snapshot);
        }
        let receipt = oxideloop_controller_core::validate_deleting_root_receipt(&snapshot)
            .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))?;
        let state_ref = ObjectRefV0 {
            resource: ResourceKindV0::OxideLoopState,
            namespace: namespace.into(),
            name: receipt.state_name.clone(),
        };
        if transport.reread_object(&state_ref).await?.is_some() {
            return Err(RuntimeErrorV0::Hold(
                "deleting root has no state link but the deterministic state name is occupied"
                    .into(),
            ));
        }
        return recover_deleting_root_without_state(snapshot).map(|machine| machine.root().clone());
    };
    let deterministic_state_name = oxideloop_controller_api::state_name(&snapshot.uid)
        .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))?;
    if link.root_uid != snapshot.uid || link.name != deterministic_state_name || link.uid.is_empty()
    {
        return Err(RuntimeErrorV0::Hold("root state link is invalid".into()));
    }
    let state_ref = ObjectRefV0 {
        resource: ResourceKindV0::OxideLoopState,
        namespace: namespace.into(),
        name: link.name.clone(),
    };
    let Some(state) = transport.reread_object(&state_ref).await? else {
        if snapshot.deleting {
            return recover_deleting_root_without_state(snapshot)
                .map(|machine| machine.root().clone());
        }
        return Err(RuntimeErrorV0::Hold("linked state is absent".into()));
    };
    install_linked_state_pins(&mut snapshot, namespace, &link, &state_ref, &state)?;
    Ok(snapshot)
}

/// Loads exactly one root and, if linked, its immutable journal.  Raw DTOs are
/// checked before serde accepts their typed fields, preventing a malformed
/// object from becoming reducer authority. An absent state bootstraps only a
/// nondeleting root. A deleting root instead needs an exact cleanup receipt
/// and enters the receipt-bounded root-only recovery protocol.
pub async fn load_machine_from_transport(
    transport: &KubeTransportV0,
    namespace: &str,
    name: &str,
) -> Result<Option<MachineStateV0>, RuntimeErrorV0> {
    load_machine_from_reader(transport, namespace, name).await
}

async fn load_machine_from_reader<P: LoaderRereadPortV0>(
    transport: &P,
    namespace: &str,
    name: &str,
) -> Result<Option<MachineStateV0>, RuntimeErrorV0> {
    let root_ref = ObjectRefV0 {
        resource: ResourceKindV0::WebApplication,
        namespace: namespace.into(),
        name: name.into(),
    };
    let Some(root) = transport.reread_object(&root_ref).await? else {
        return Ok(None);
    };
    let mut snapshot = root_snapshot_from_object(&root, namespace, name)?;
    let Some(link) = snapshot.status.state.clone() else {
        if snapshot.deleting {
            return recover_unlinked_deleting_root(transport, namespace, snapshot).await;
        }
        if snapshot.status.deletion_cleanup_receipt.is_some() {
            return Err(RuntimeErrorV0::Hold(
                "nondeleting root cannot bootstrap from a deletion cleanup receipt".into(),
            ));
        }
        return MachineStateV0::bootstrap(snapshot)
            .map(Some)
            .map_err(|error| RuntimeErrorV0::Hold(error.to_string()));
    };
    let deterministic_state_name = oxideloop_controller_api::state_name(&snapshot.uid)
        .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))?;
    if link.root_uid != snapshot.uid || link.name != deterministic_state_name || link.uid.is_empty()
    {
        return Err(RuntimeErrorV0::Hold("root state link is invalid".into()));
    }
    let state_ref = ObjectRefV0 {
        resource: ResourceKindV0::OxideLoopState,
        namespace: namespace.into(),
        name: link.name.clone(),
    };
    let Some(state) = transport.reread_object(&state_ref).await? else {
        if snapshot.deleting {
            return recover_deleting_root_without_state(snapshot).map(Some);
        }
        return Err(RuntimeErrorV0::Hold("linked state is absent".into()));
    };
    install_linked_state_pins(&mut snapshot, namespace, &link, &state_ref, &state)?;
    let journal: oxideloop_controller_api::OxideLoopStateSpec = serde_json::from_value(
        state
            .value
            .get("spec")
            .cloned()
            .ok_or_else(|| RuntimeErrorV0::Hold("linked state spec is missing".into()))?,
    )
    .map_err(|_| RuntimeErrorV0::Hold("linked state spec is invalid".into()))?;
    journal
        .validate()
        .map_err(|_| RuntimeErrorV0::Hold("linked state spec is invalid".into()))?;
    validate_state_status(&state)?;
    MachineStateV0::recover(snapshot, journal)
        .map(Some)
        .map_err(|error| RuntimeErrorV0::Hold(error.to_string()))
}
impl ControllerRuntimeV0 {
    pub fn insert(&mut self, key: ActorKeyV0, actor: RootActorV0) {
        self.actors.insert(key, actor);
    }
    pub fn actor_mut(&mut self, key: &ActorKeyV0) -> Option<&mut RootActorV0> {
        self.actors.get_mut(key)
    }
}

/// Execute one sealed Kubernetes effect. Public mutation dispatch accepts only
/// one prepared mutation at a time; aggregate acknowledgements are rejected at
/// this boundary because Kubernetes cannot make a multi-object batch atomic.
pub async fn dispatch_transport_command(
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
    command: &CommandV0,
) -> Result<Option<EventV0>, RuntimeErrorV0> {
    validate_transport_command_shape(command)?;
    match transport.dispatch(address, command).await? {
        oxideloop_kube_transport::CommandOutcomeV0::Event(event) => Ok(Some(*event)),
        // Bootstrap is a deliberately resumable multi-write effect.  Retain
        // command ownership and retry it rather than acknowledging early.
        oxideloop_kube_transport::CommandOutcomeV0::NeedsSequencing { .. }
        | oxideloop_kube_transport::CommandOutcomeV0::NotKubernetesEffect => Ok(None),
        oxideloop_kube_transport::CommandOutcomeV0::Hold { reason } => {
            Err(RuntimeErrorV0::Hold(reason))
        }
    }
}

fn validate_transport_command_shape(command: &CommandV0) -> Result<(), RuntimeErrorV0> {
    if let CommandV0::Mutate { commands, .. } = command
        && commands.len() != 1
    {
        return Err(RuntimeErrorV0::Hold(
            "public mutation dispatch requires exactly one prepared command".into(),
        ));
    }
    Ok(())
}

/// Runs the reducer's Kubernetes commands to quiescence.  Non-Kubernetes
/// commands are returned to the caller, which owns descriptor/render/action
/// evaluation and must subsequently acknowledge them through the actor.
pub async fn drive_kubernetes_until_external(
    actor: &mut RootActorV0,
    transport: &KubeTransportV0,
    address: &oxideloop_kube_transport::RootAddressV0,
) -> Result<Option<CommandV0>, RuntimeErrorV0> {
    let mut dispatcher = SealedKubernetesDispatcherV0 { transport, address };
    drive_kubernetes_with_dispatcher(actor, &mut dispatcher).await
}

#[async_trait]
trait KubernetesDispatcherV0 {
    async fn dispatch(&mut self, command: &CommandV0) -> Result<Option<EventV0>, RuntimeErrorV0>;
    async fn fresh_root_snapshot(&mut self) -> Result<RootSnapshotV0, RuntimeErrorV0>;
}

struct SealedKubernetesDispatcherV0<'a> {
    transport: &'a KubeTransportV0,
    address: &'a oxideloop_kube_transport::RootAddressV0,
}

#[async_trait]
impl KubernetesDispatcherV0 for SealedKubernetesDispatcherV0<'_> {
    async fn dispatch(&mut self, command: &CommandV0) -> Result<Option<EventV0>, RuntimeErrorV0> {
        dispatch_transport_command(self.transport, self.address, command).await
    }

    async fn fresh_root_snapshot(&mut self) -> Result<RootSnapshotV0, RuntimeErrorV0> {
        fresh_root_snapshot_from_transport(
            self.transport,
            &self.address.namespace,
            &self.address.name,
        )
        .await
    }
}

fn stale_preflight_recoverable(command: &CommandV0) -> bool {
    matches!(
        command,
        CommandV0::Persist { .. }
            | CommandV0::PersistRootStatus { .. }
            | CommandV0::RemoveStateFinalizer { .. }
            | CommandV0::RemoveRootFinalizer { .. }
    )
}

async fn drive_kubernetes_with_dispatcher<D: KubernetesDispatcherV0>(
    actor: &mut RootActorV0,
    dispatcher: &mut D,
) -> Result<Option<CommandV0>, RuntimeErrorV0> {
    let mut owned_command = None;
    let mut sequencing_retries = 0_u8;
    loop {
        let command = if let Some(command) = owned_command.take() {
            command
        } else {
            let Some(command) = actor.next_command()? else {
                return Ok(None);
            };
            sequencing_retries = 0;
            command
        };
        if matches!(
            command,
            CommandV0::RequestDescriptor { .. }
                | CommandV0::RequestRender { .. }
                | CommandV0::RequestDeletionObservation { .. }
        ) {
            return Ok(Some(command));
        }
        if let CommandV0::Hold { ref reason } = command {
            actor.hold(reason.clone());
            return Err(RuntimeErrorV0::Hold(reason.clone()));
        }
        if matches!(command, CommandV0::IgnoredStale { .. }) {
            actor.consume_internal();
            continue;
        }
        match dispatcher.dispatch(&command).await {
            Ok(Some(event)) => {
                sequencing_retries = 0;
                actor.acknowledge(event)?;
            }
            // The command remains owned by this loop until the transport gives
            // a durable core event; do not let an observation invalidate it.
            // `NeedsSequencing` is intentionally retried *once per loop
            // iteration* with the same owned command.  Do not dispatch it a
            // second time as part of error handling and do not fabricate Hold.
            Ok(None) => {
                sequencing_retries = sequencing_retries.saturating_add(1);
                if sequencing_retries > MAX_SEQUENCING_RETRIES {
                    actor.hold("transport sequencing retry bound exceeded".into());
                    return Err(RuntimeErrorV0::Hold(
                        "transport sequencing retry bound exceeded".into(),
                    ));
                }
                owned_command = Some(command);
            }
            Err(RuntimeErrorV0::Discovery(TransportErrorV0::Stale(_)))
                if stale_preflight_recoverable(&command) =>
            {
                let root = dispatcher.fresh_root_snapshot().await?;
                actor.recover_stale_preflight(root);
                sequencing_retries = 0;
            }
            Err(RuntimeErrorV0::Discovery(TransportErrorV0::ManualRecoveryHold)) => {
                actor.hold("manual recovery required after delete/finalizer ambiguity".into());
                return Err(RuntimeErrorV0::Hold(
                    "manual recovery required after delete/finalizer ambiguity".into(),
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Establish a mandatory list/watch/reread barrier.  `WatchGone` and overflow
/// are returned to the actor so it increments `cacheEpoch` before retrying.
pub fn establish_dependencies<P: DiscoveryPortV0>(
    port: &mut P,
    dependencies: &[Dependency],
) -> Result<(), RuntimeErrorV0> {
    let dependencies = normalize_dependencies(dependencies)?;
    let mut collections = BTreeSet::new();
    let mut objects = Vec::new();
    for dependency in &dependencies {
        match dependency {
            Dependency::Object {
                api_version,
                kind,
                namespace,
                name,
                ..
            } => {
                let resource = resource(api_version, kind)?;
                collections.insert((resource, namespace.clone()));
                objects.push(ObjectRefV0 {
                    resource,
                    namespace: namespace.clone(),
                    name: name.clone(),
                });
            }
            Dependency::Collection {
                api_version,
                kind,
                namespace,
                ..
            } => {
                collections.insert((resource(api_version, kind)?, namespace.clone()));
            }
        }
    }
    for (kind, namespace) in collections {
        let ListV0 {
            resource_version, ..
        } = port.list_exact(kind, &namespace)?;
        let _ = port.watch_from(kind, &namespace, &resource_version)?;
    }
    for object in objects {
        let _ = port.reread(&object)?;
    }
    Ok(())
}

/// Declarations are renderer data, so their incidental array order must not
/// change the discovery topology.  Keep a single canonical sort key at this
/// boundary and reject values which cannot be represented as canonical JSON.
pub fn normalize_dependencies(
    dependencies: &[Dependency],
) -> Result<Vec<Dependency>, RuntimeErrorV0> {
    let mut keyed = dependencies
        .iter()
        .map(|dependency| {
            serde_json::to_string(&dependency)
                .map(|key| (key, dependency.clone()))
                .map_err(|_| RuntimeErrorV0::RendererAbi)
        })
        .collect::<Result<Vec<_>, _>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    keyed.dedup_by(|left, right| left.0 == right.0);
    Ok(keyed
        .into_iter()
        .map(|(_, dependency)| dependency)
        .collect())
}

/// Render, observe, and rerender until declarations stabilize. On the ninth
/// unstable declaration the actor must enter explicit Hold.
pub fn fixed_point<P: DiscoveryPortV0>(
    port: &mut P,
    mut render: impl FnMut() -> Result<Vec<Dependency>, RuntimeErrorV0>,
) -> Result<Vec<Dependency>, RuntimeErrorV0> {
    let mut prior = None;
    for _ in 0..MAX_DISCOVERY_PASSES {
        let next = normalize_dependencies(&render()?)?;
        establish_dependencies(port, &next)?;
        if prior.as_ref() == Some(&next) {
            return Ok(next);
        }
        prior = Some(next);
    }
    Err(RuntimeErrorV0::Hold(
        "dependency declarations did not stabilize".into(),
    ))
}

/// The reusable render/discovery lifecycle.  The port is deliberately typed:
/// a fake supplies canonical renderer output and the observations obtained
/// after its list/watch/reread barrier, rather than imitating Kubernetes or a
/// Unix socket in unit tests.
#[async_trait]
pub trait FixedPointPortV0: Send {
    async fn render(&mut self, input: &RenderInputV0) -> Result<RenderOutputV0, RuntimeErrorV0>;
    async fn observe_after_barrier(
        &mut self,
        dependencies: &[Dependency],
    ) -> Result<Vec<oxideloop_renderer_abi::model::Observation>, RuntimeErrorV0>;

    /// Installs the current global barrier without manufacturing a renderer
    /// observation transition.  Concrete ports use it to bind a fresh
    /// list/watch/reread fence to the actor's genuine-change generation.
    fn set_barrier(&mut self, _barrier: BarrierV0) {}

    /// Drop any retained observation cache after a watch epoch is invalidated.
    /// The default keeps lightweight fake ports source-compatible.
    fn watch_invalidated(&mut self, _barrier: BarrierV0) {}
}

/// Derive a new renderer-visible observation token for an invalidated barrier.
/// This is deterministic for a given prior token and barrier, while ensuring a
/// stale observation cannot be mistaken for one from the restarted epoch.
#[must_use]
pub fn observation_token_for_barrier(token: &str, barrier: BarrierV0) -> String {
    oxideloop_renderer_abi::sha256_hex(
        format!(
            "oxideloop-observation-v0\\0{token}\\0{}\\0{}",
            barrier.generation, barrier.cache_epoch
        )
        .as_bytes(),
    )
}

#[derive(Debug)]
pub struct FixedPointResultV0 {
    input: RenderInputV0,
    output: RenderOutputV0,
    dependencies: Vec<Dependency>,
    provenance: Option<FixedPointProvenanceV0>,
}

// Cloning a generic fixed-point result intentionally drops the one-shot
// completion authority. This keeps ordinary engine tests ergonomic while a
// copied result can never be replayed as a completed-observation capability.
impl Clone for FixedPointResultV0 {
    fn clone(&self) -> Self {
        Self {
            input: self.input.clone(),
            output: self.output.clone(),
            dependencies: self.dependencies.clone(),
            provenance: None,
        }
    }
}

impl FixedPointResultV0 {
    #[must_use]
    pub fn input(&self) -> &RenderInputV0 {
        &self.input
    }

    #[must_use]
    pub fn output(&self) -> &RenderOutputV0 {
        &self.output
    }

    #[must_use]
    pub fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }
}

/// A sealed hand-off from a validated fixed-point result plus the actor's
/// authenticated durable state to the retained observation adapter.  There is
/// deliberately no public constructor: callers cannot assert a smaller
/// desired/inventory set or choose prune tokens to make a fence complete.
#[derive(Clone, Debug)]
struct CompletedObservationSessionV0 {
    root: ObjectRefV0,
    root_uid: String,
    dependencies: Vec<Dependency>,
    desired_identities: Vec<ObjectIdentity>,
    durable_inventory: Vec<InventoryEntry>,
    durable_prune_tokens: Vec<String>,
    retained_barrier: BarrierV0,
    retained_incarnation: u64,
}

/// A completed dependency observation fence.  It is separate from the
/// actor's global renderer-observation generation: two completed fences over
/// an unchanged snapshot receive different tokens, while neither can invent a
/// dependency-change token.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletedObservationFenceV0 {
    token: String,
    barrier: BarrierV0,
    sequence: u64,
    dependencies: Vec<Dependency>,
    renderer_observations: Vec<oxideloop_renderer_abi::model::Observation>,
    desired_observations: Vec<DesiredObservationV0>,
}

impl CompletedObservationFenceV0 {
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    #[must_use]
    pub const fn barrier(&self) -> BarrierV0 {
        self.barrier
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub fn dependencies(&self) -> &[Dependency] {
        &self.dependencies
    }

    #[must_use]
    pub fn renderer_observations(&self) -> &[oxideloop_renderer_abi::model::Observation] {
        &self.renderer_observations
    }

    #[must_use]
    pub fn desired_observations(&self) -> &[DesiredObservationV0] {
        &self.desired_observations
    }
}

/// Deterministically authenticates one completed final snapshot.  `sequence`
/// is local to the retained observation port and must advance for every
/// completed fence; callers pass durable prune tokens to skip after restart.
///
/// # Panics
///
/// Panics only if `final_snapshot` cannot be represented as canonical JSON.
/// The completed-fence adapter constructs it solely from serializable DTOs.
#[must_use]
fn completed_observation_fence_token(
    root_uid: &str,
    barrier: BarrierV0,
    sequence: u64,
    final_snapshot: &serde_json::Value,
) -> String {
    let canonical = oxideloop_renderer_abi::canonical_bytes(final_snapshot)
        .expect("completed observation fence snapshot is serializable JSON");
    let snapshot_hash = oxideloop_renderer_abi::sha256_hex(&canonical);
    oxideloop_renderer_abi::sha256_hex(
        format!(
            "oxideloop-completed-observation-fence/v0\0{root_uid}\0{}\0{sequence}\0{snapshot_hash}",
            barrier.cache_epoch
        )
        .as_bytes(),
    )
}

/// Speculatively render, establish a full dependency barrier, then rerender
/// with exact observations until the normalized declaration set is stable.
/// Eight passes is a hard safety bound; callers turn the returned Hold into
/// the actor's durable Hold mode.
pub async fn reconcile_fixed_point<P: FixedPointPortV0>(
    port: &mut P,
    mut input: RenderInputV0,
) -> Result<FixedPointResultV0, RuntimeErrorV0> {
    let mut previous: Option<Vec<Dependency>> = None;
    for _ in 0..MAX_DISCOVERY_PASSES {
        let output = port.render(&input).await?;
        let dependencies = normalize_dependencies(&output.dependencies)?;
        if previous.as_ref() == Some(&dependencies) {
            return Ok(FixedPointResultV0 {
                input,
                output,
                dependencies,
                provenance: None,
            });
        }
        input.observations = port.observe_after_barrier(&dependencies).await?;
        input.observations.sort_by(|left, right| {
            (
                left.api_version.as_str(),
                left.kind.as_str(),
                left.namespace.as_str(),
                left.name.as_str(),
                left.uid.as_str(),
                left.resource_version.as_str(),
            )
                .cmp(&(
                    right.api_version.as_str(),
                    right.kind.as_str(),
                    right.namespace.as_str(),
                    right.name.as_str(),
                    right.uid.as_str(),
                    right.resource_version.as_str(),
                ))
        });
        previous = Some(dependencies);
    }
    Err(RuntimeErrorV0::Hold(
        "dependency declarations did not stabilize".into(),
    ))
}

/// A concrete, retained async observation barrier. Unlike the legacy
/// `DiscoveryPortV0` test seam, production callers keep these handles alive
/// until the actor invalidates the epoch. Lists include absent object
/// dependencies and are repeated after every watch is established.
pub struct ObservationBarrierV0 {
    pub generation: BarrierV0,
    /// The normalized dependency declaration set that this exact
    /// list/watch/reread snapshot covers.
    pub dependencies: Vec<Dependency>,
    pub collections: BTreeMap<(ResourceKindV0, String), ListV0>,
    pub objects: Vec<(ObjectRefV0, Option<oxideloop_kube_transport::ObjectV0>)>,
    pub(crate) watches: Vec<WatchHandleV0>,
}

impl ObservationBarrierV0 {
    pub async fn next_event(&mut self, index: usize) -> Result<WatchEventV0, RuntimeErrorV0> {
        // The runtime driver polls all retained handles; this helper provides a
        // deterministic first-ready path for single/dependency-light barriers.
        let Some(watch) = self.watches.get_mut(index) else {
            return Err(RuntimeErrorV0::Hold(
                "observation barrier has no watches".into(),
            ));
        };
        watch.next_event().await.map_err(RuntimeErrorV0::Discovery)
    }
    pub fn watch_count(&self) -> usize {
        self.watches.len()
    }
}

pub async fn establish_dependencies_async(
    transport: &KubeTransportV0,
    dependencies: &[Dependency],
    generation: BarrierV0,
) -> Result<ObservationBarrierV0, RuntimeErrorV0> {
    let dependencies = normalize_dependencies(dependencies)?;
    let mut requested = BTreeSet::new();
    let mut objects = Vec::new();
    for dependency in &dependencies {
        match dependency {
            Dependency::Object {
                api_version,
                kind,
                namespace,
                name,
                ..
            } => {
                let resource = resource(api_version, kind)?;
                requested.insert((resource, namespace.clone()));
                let object = ObjectRefV0 {
                    resource,
                    namespace: namespace.clone(),
                    name: name.clone(),
                };
                if !objects.iter().any(|existing| existing == &object) {
                    objects.push(object);
                }
            }
            Dependency::Collection {
                api_version,
                kind,
                namespace,
                ..
            } => {
                requested.insert((resource(api_version, kind)?, namespace.clone()));
            }
        }
    }
    let mut collections = BTreeMap::new();
    let mut watches = Vec::new();
    // LIST -> exact-RV WATCH for every collection, including object parents.
    for (kind, namespace) in &requested {
        let list = transport.list_exact(*kind, namespace).await?;
        let watch = transport
            .watch_from(*kind, namespace, &list.resource_version)
            .await?;
        collections.insert((*kind, namespace.clone()), list);
        watches.push(watch);
    }
    // Re-read both exact objects (including absence) and collections while all
    // watches are live. Buffered watch events remain on their retained handle.
    let mut reread_collections = BTreeMap::new();
    for (kind, namespace) in &requested {
        reread_collections.insert(
            (*kind, namespace.clone()),
            transport.list_exact(*kind, namespace).await?,
        );
    }
    let mut observed_objects = Vec::new();
    for object in objects {
        let observed = transport.reread(&object).await?;
        observed_objects.push((object, observed));
    }
    Ok(ObservationBarrierV0 {
        generation,
        dependencies,
        collections: reread_collections,
        objects: observed_objects,
        watches,
    })
}

/// Strictly correlate protocol traffic and parse the canonical renderer ABI.
/// The caller supplies only the already-verified bundle/module digest.
pub fn renderer_request(
    phase: RendererPhaseV0,
    request_id: [u8; 16],
    bundle_digest: &str,
    module_digest: Option<String>,
    payload: Vec<u8>,
) -> Result<RequestV0, RuntimeErrorV0> {
    let request = RequestV0 {
        request_id: RequestIdV0 { bytes: request_id },
        phase,
        bundle_digest: bundle_digest.into(),
        module_digest,
        duration_millis: 1_000,
        payload,
    };
    request.validate().map_err(|_| RuntimeErrorV0::Renderer)?;
    Ok(request)
}

pub fn call_render(
    renderer: &mut RendererClient,
    request: &RequestV0,
) -> Result<RenderOutputV0, RuntimeErrorV0> {
    let response = renderer
        .call(request)
        .map_err(|_| RuntimeErrorV0::Renderer)?;
    let payload = renderer_payload(&response, request)?;
    let text = std::str::from_utf8(&payload).map_err(|_| RuntimeErrorV0::RendererAbi)?;
    Ok(parse_validate_render_output(text)
        .map_err(|_| RuntimeErrorV0::RendererAbi)?
        .document)
}

pub fn call_action(
    renderer: &mut RendererClient,
    request: &RequestV0,
) -> Result<ActionOutputV0, RuntimeErrorV0> {
    let response = renderer
        .call(request)
        .map_err(|_| RuntimeErrorV0::Renderer)?;
    let payload = renderer_payload(&response, request)?;
    let text = std::str::from_utf8(&payload).map_err(|_| RuntimeErrorV0::RendererAbi)?;
    Ok(parse_validate_action_output(text)
        .map_err(|_| RuntimeErrorV0::RendererAbi)?
        .document)
}

pub fn canonical_render_input(input: &RenderInputV0) -> Result<Vec<u8>, RuntimeErrorV0> {
    serde_json::to_string(input)
        .map_err(|_| RuntimeErrorV0::RendererAbi)
        .and_then(|json| {
            oxideloop_renderer_abi::parse_validate_render_input(&json)
                .map(|validated| validated.canonical_bytes)
                .map_err(|_| RuntimeErrorV0::RendererAbi)
        })
}

pub fn canonical_action_input(input: &ActionInputV0) -> Result<Vec<u8>, RuntimeErrorV0> {
    serde_json::to_string(input)
        .map_err(|_| RuntimeErrorV0::RendererAbi)
        .and_then(|json| {
            oxideloop_renderer_abi::parse_validate_action_input(&json)
                .map(|validated| validated.canonical_bytes)
                .map_err(|_| RuntimeErrorV0::RendererAbi)
        })
}

/// Select an action route only when both immutable descriptor pins agree.
/// Identity alone is never a capability to execute a module.
pub fn action_module_from_descriptor(
    descriptor: &BundleDescriptorV0,
    identity: &str,
    digest: &str,
) -> Result<String, RuntimeErrorV0> {
    descriptor
        .action_modules
        .iter()
        .find(|module| module.identity == identity && module.digest == digest)
        .map(|module| module.digest.clone())
        .ok_or_else(|| RuntimeErrorV0::Hold("action descriptor identity/digest mismatch".into()))
}

/// Renderer request IDs are stable replay keys, not random nonces. The input
/// is an immutable canonical hash (normally `AutomationRunSpec.actionInputHash`).
pub fn deterministic_request_id(immutable_hash: &str) -> Result<[u8; 16], RuntimeErrorV0> {
    if immutable_hash.len() != 64
        || !immutable_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RuntimeErrorV0::Hold(
            "immutable request hash is invalid".into(),
        ));
    }
    let mut id = [0_u8; 16];
    for (index, byte) in id.iter_mut().enumerate() {
        let nibble = |value: u8| match value {
            b'0'..=b'9' => value - b'0',
            b'a'..=b'f' => value - b'a' + 10,
            _ => unreachable!("validated lowercase hexadecimal"),
        };
        *byte = (nibble(immutable_hash.as_bytes()[index * 2]) << 4)
            | nibble(immutable_hash.as_bytes()[index * 2 + 1]);
    }
    Ok(id)
}

/// Execute the durable half of an automation exactly once. Replays use the
/// same renderer request ID, event plan and status desired value, so the two
/// port recovery operations provide the no-duplicate restart behaviour.
#[allow(clippy::too_many_arguments)]
pub async fn drive_automation<P: AutomationPortV0, R: RendererPortV0>(
    port: &mut P,
    renderer: &mut R,
    descriptor: &BundleDescriptorV0,
    execution: &AutomationExecutionV0,
    spec: &AutomationRunSpec,
    event_time: &str,
    failpoints: &mut FailpointsV0,
) -> Result<EventV0, RuntimeErrorV0> {
    spec.validate()
        .map_err(|_| RuntimeErrorV0::Hold("AutomationRun spec is invalid".into()))?;
    let module_digest = action_module_from_descriptor(
        descriptor,
        &spec.action_module_identity,
        &spec.action_module_digest,
    )?;
    let input = oxideloop_renderer_abi::parse_validate_action_input(&spec.action_input_v0)
        .map_err(|_| RuntimeErrorV0::RendererAbi)?;
    if input.canonical_bytes != spec.action_input_v0.as_bytes() {
        return Err(RuntimeErrorV0::RendererAbi);
    }
    let request = renderer_request(
        RendererPhaseV0::Action,
        deterministic_request_id(&spec.action_input_hash)?,
        &spec.bundle_digest,
        Some(module_digest),
        input.canonical_bytes.clone(),
    )?;
    let response = renderer.call(&request).await?;
    let payload = renderer_payload(&response, &request)?;
    let payload = std::str::from_utf8(&payload).map_err(|_| RuntimeErrorV0::RendererAbi)?;
    let output = parse_validate_action_output(payload).map_err(|_| RuntimeErrorV0::RendererAbi)?;
    let plan = plan_event(
        &input,
        &output,
        &execution.root,
        &execution.source,
        &execution.run,
        &execution.correlation,
    )?;
    let materialized = port
        .create_or_exact_recover_event(&plan, event_time)
        .await?;
    if failpoints.take(FailpointV0::AfterEventBeforeStatus) {
        return Err(RuntimeErrorV0::InjectedCrash(
            FailpointV0::AfterEventBeforeStatus,
        ));
    }
    let status = AutomationRunStatus {
        attempts: 1,
        materialization_name: Some(materialized.name.clone()),
        materialization_uid: Some(materialized.uid.clone()),
        completion: Some(AutomationRunCompletion {
            state: AutomationRunCompletionState::Succeeded,
            message: "action event materialized".into(),
        }),
    };
    status
        .validate()
        .map_err(|_| RuntimeErrorV0::Hold("AutomationRun status is invalid".into()))?;
    let ack = port
        .persist_completion(
            &execution.run.identity.namespace,
            &execution.run.identity.name,
            &execution.run.uid,
            &execution.run.resource_version,
            spec,
            &status,
        )
        .await?;
    if failpoints.take(FailpointV0::AfterStatusBeforeAck) {
        return Err(RuntimeErrorV0::InjectedCrash(
            FailpointV0::AfterStatusBeforeAck,
        ));
    }
    Ok(EventV0::AutomationCompleted {
        spec: spec.clone(),
        occurrence_id: spec.occurrence_id.clone(),
        name: execution.run.identity.name.clone(),
        uid: ack.uid,
        old_resource_version: ack.old_resource_version,
        new_resource_version: ack.new_resource_version,
        completion: ack.completion,
        attempts: ack.attempts,
        materialization_name: ack.materialization_name,
        materialization_uid: ack.materialization_uid,
    })
}

fn resource(api_version: &str, kind: &str) -> Result<ResourceKindV0, RuntimeErrorV0> {
    match (api_version, kind) {
        ("v1", "ConfigMap") => Ok(ResourceKindV0::ConfigMap),
        ("v1", "Service") => Ok(ResourceKindV0::Service),
        ("demo.oxideloop.dev/v1alpha1", "WebApplication") => Ok(ResourceKindV0::WebApplication),
        ("runtime.oxideloop.dev/v1alpha1", "OxideLoopState") => Ok(ResourceKindV0::OxideLoopState),
        ("runtime.oxideloop.dev/v1alpha1", "AutomationRun") => Ok(ResourceKindV0::AutomationRun),
        _ => Err(RuntimeErrorV0::Hold("unsupported dependency GVK".into())),
    }
}

/// Renderer output reaches Kubernetes only after event-planner constructs the
/// one deterministic Event plan and P7 materializes it.
pub async fn execute_event_action<E: EventPortV0>(
    port: &mut E,
    plan: &PlannedEventV0,
    event_time: &str,
) -> Result<(), RuntimeErrorV0> {
    port.create_or_exact_recover_event(plan, event_time).await
}
pub fn recovery_matches(expected: &Value, observed: &Value) -> Result<bool, RuntimeErrorV0> {
    Ok(semantically_equal(expected, observed)?)
}
pub fn renderer_payload(
    response: &ResponseV0,
    request: &RequestV0,
) -> Result<Vec<u8>, RuntimeErrorV0> {
    response
        .matches(request)
        .map_err(|_| RuntimeErrorV0::Renderer)?;
    if response.status == ResponseStatusV0::Ok {
        Ok(response.payload.clone())
    } else {
        Err(RuntimeErrorV0::Renderer)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use super::*;
    use http::{Method, Request, Response};
    use kube::{Client, client::Body};
    use oxideloop_controller_api::{
        ApplyOutcome, ApplyResult, AutomationPending, DeletionTransaction,
        GenerationAdvanceClosurePurpose, InventoryEntry, ObjectIdentity, OxideLoopStateSpec,
        PersistPurpose, ProgramBundleReference, RootBinding, StateBody, StateLink,
        WebApplicationSpec, WebApplicationStatus, bootstrap_id,
    };
    use oxideloop_controller_core::{
        ActionTriggerV0, AutomationRunCleanupV0, DesiredObservationV0, FrozenInputV0,
        RootSnapshotV0, TransitionV0, automation_source_transition_token,
    };
    use oxideloop_kube_transport::{ObjectMetaV0, ObjectV0, WatchEventV0};
    use oxideloop_mutation_model::{BackendQualification, Observation};
    use oxideloop_renderer_abi::model::{
        ABI_VERSION_V0, RenderDisposition, ResourceDeclaration, RootProjection,
    };

    fn root_spec(digest: impl Into<String>) -> WebApplicationSpec {
        WebApplicationSpec {
            message: "hello".into(),
            expose: false,
            program: ProgramBundleReference {
                config_map_name: "bundle".into(),
                key: "program.mjs".into(),
                digest: digest.into(),
            },
        }
    }
    use oxideloop_renderer_abi::parse_validate_render_input;
    use tower::service_fn;

    #[derive(Default)]
    struct LoaderPort {
        objects: Vec<ObjectV0>,
    }

    #[async_trait]
    impl LoaderRereadPortV0 for LoaderPort {
        async fn reread_object(
            &self,
            reference: &ObjectRefV0,
        ) -> Result<Option<ObjectV0>, TransportErrorV0> {
            Ok(self
                .objects
                .iter()
                .find(|object| object.reference == *reference)
                .cloned())
        }
    }

    fn loader_port_for_machine(machine: &MachineStateV0, state_rv: &str) -> LoaderPort {
        let root = machine.root();
        let mut root_metadata = serde_json::json!({
            "name": root.name,
            "namespace": root.namespace,
            "uid": root.uid,
            "resourceVersion": root.root_resource_version,
            "generation": root.generation,
            "finalizers": if root.root_finalizer_present {
                vec![oxideloop_controller_api::ROOT_FINALIZER]
            } else {
                vec![]
            },
        });
        if root.deleting {
            root_metadata["deletionTimestamp"] = serde_json::json!("2026-08-30T00:00:00Z");
        }
        let root_ref = ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: root.namespace.clone(),
            name: root.name.clone(),
        };
        let root_object = ObjectV0 {
            reference: root_ref.clone(),
            metadata: ObjectMetaV0 {
                uid: root.uid.clone(),
                resource_version: root.root_resource_version.clone(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: serde_json::json!({
                "apiVersion": "demo.oxideloop.dev/v1alpha1",
                "kind": "WebApplication",
                "metadata": root_metadata,
                "spec": root.spec,
                "status": root.status,
            }),
        };
        let state_ref = ObjectRefV0 {
            resource: ResourceKindV0::OxideLoopState,
            namespace: root.namespace.clone(),
            name: root.state_name.clone(),
        };
        let state_object = ObjectV0 {
            reference: state_ref.clone(),
            metadata: ObjectMetaV0 {
                uid: root.state_uid.clone(),
                resource_version: state_rv.into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![root.uid.clone()],
            },
            value: serde_json::json!({
                "apiVersion": "runtime.oxideloop.dev/v1alpha1",
                "kind": "OxideLoopState",
                "metadata": {
                    "name": root.state_name,
                    "namespace": root.namespace,
                    "uid": root.state_uid,
                    "resourceVersion": state_rv,
                    "finalizers": if root.state_finalizer_present {
                        vec![oxideloop_controller_api::STATE_FINALIZER]
                    } else {
                        vec![]
                    },
                },
                "spec": machine.journal(),
                "status": {},
            }),
        };
        LoaderPort {
            objects: vec![root_object, state_object],
        }
    }
    #[derive(Default)]
    struct Port {
        calls: Vec<String>,
    }
    impl DiscoveryPortV0 for Port {
        fn list_exact(&mut self, r: ResourceKindV0, n: &str) -> Result<ListV0, TransportErrorV0> {
            self.calls.push(format!("L:{r:?}:{n}"));
            Ok(ListV0 {
                items: vec![],
                resource_version: "7".into(),
            })
        }
        fn watch_from(
            &mut self,
            r: ResourceKindV0,
            n: &str,
            rv: &str,
        ) -> Result<Vec<WatchEventV0>, TransportErrorV0> {
            self.calls.push(format!("W:{r:?}:{n}:{rv}"));
            Ok(vec![])
        }
        fn reread(&mut self, r: &ObjectRefV0) -> Result<Option<ObjectV0>, TransportErrorV0> {
            self.calls.push(format!("R:{}", r.name));
            Ok(None)
        }
    }
    fn dep() -> Dependency {
        Dependency::Object {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "gone".into(),
            uid: None,
            resource_version: None,
            observation_token: "t".into(),
        }
    }

    struct DeferredStableRendererV0 {
        output: RenderOutputV0,
    }

    #[async_trait]
    impl RendererPortV0 for DeferredStableRendererV0 {
        async fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0> {
            Ok(ResponseV0 {
                request_id: request.request_id,
                phase: request.phase,
                bundle_digest: request.bundle_digest.clone(),
                module_digest: request.module_digest.clone(),
                status: ResponseStatusV0::Ok,
                error_category: None,
                payload: serde_json::to_vec(&self.output)
                    .expect("deferred test render output serializes"),
            })
        }
    }

    fn deferred_test_descriptor() -> BundleDescriptorV0 {
        BundleDescriptorV0 {
            descriptor_version: "oxideloop.dev/descriptor/v0".into(),
            renderer_abi_version: ABI_VERSION_V0.into(),
            render_module: oxideloop_bundle_contract::DescriptorModuleV0 {
                identity: "render/main.mjs".into(),
                digest: "a".repeat(64),
            },
            action_modules: vec![],
        }
    }

    fn dependency_object(resource_version: &str) -> ObjectV0 {
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
            value: serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {
                    "name": "dependency",
                    "namespace": "ns",
                    "uid": "dependency-uid",
                    "resourceVersion": resource_version,
                },
                "data": {"revision": resource_version},
            }),
        }
    }

    fn deferred_render_input(
        actor: &RootActorV0,
        preconditions: &PreconditionsV0,
    ) -> RenderInputV0 {
        let root = actor.state().root();
        RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                uid: root.uid.clone(),
                namespace: root.namespace.clone(),
                name: root.name.clone(),
                generation: u64::try_from(root.generation)
                    .expect("test root generation is non-negative"),
                spec: serde_json::to_value(&root.spec).expect("test root spec serializes"),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: preconditions.cache_epoch.clone(),
            observation_token: preconditions.observation_token.clone(),
            observations: vec![],
        }
    }

    fn deferred_render_output() -> RenderOutputV0 {
        RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::ExplicitEmpty,
            resources: vec![],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        }
    }

    #[test]
    fn discovery_lists_watches_exact_rv_then_rereads_absence() {
        let mut p = Port::default();
        establish_dependencies(&mut p, &[dep()]).unwrap();
        assert_eq!(p.calls, ["L:ConfigMap:ns", "W:ConfigMap:ns:7", "R:gone"]);
    }
    #[test]
    fn fixed_point_holds_after_bounded_instability() {
        let mut p = Port::default();
        let mut n = 0;
        let result = fixed_point(&mut p, || {
            n += 1;
            let mut d = dep();
            if let Dependency::Object {
                observation_token, ..
            } = &mut d
            {
                *observation_token = n.to_string();
            }
            Ok(vec![d])
        });
        assert!(matches!(result, Err(RuntimeErrorV0::Hold(_))));
    }
    #[test]
    fn stale_readiness_is_not_admitted() {
        let a = BarrierV0 {
            generation: 1,
            cache_epoch: 0,
        };
        let b = BarrierV0 {
            generation: 2,
            cache_epoch: 0,
        };
        assert_ne!(a, b);
    }
    fn actor() -> RootActorV0 {
        RootActorV0::new(
            MachineStateV0::bootstrap(RootSnapshotV0 {
                uid: "root-uid".into(),
                namespace: "ns".into(),
                name: "demo".into(),
                generation: 1,
                spec: root_spec("0".repeat(64)),
                bundle_digest: "0".repeat(64),
                status: WebApplicationStatus::default(),
                state_name: String::new(),
                state_uid: String::new(),
                state_resource_version: String::new(),
                root_resource_version: "1".into(),
                root_finalizer_present: false,
                state_finalizer_present: false,
                deleting: false,
            })
            .unwrap(),
        )
    }

    fn linked_machine() -> MachineStateV0 {
        let linked_state_name = oxideloop_controller_api::state_name("root-uid").unwrap();
        MachineStateV0::new(
            RootSnapshotV0 {
                uid: "root-uid".into(),
                namespace: "ns".into(),
                name: "demo".into(),
                generation: 7,
                spec: root_spec("a".repeat(64)),
                bundle_digest: "a".repeat(64),
                status: WebApplicationStatus {
                    observed_generation: Some(7),
                    verified_bundle_digest: Some("a".repeat(64)),
                    input_token: Some("observation-1".into()),
                    cache_epoch: Some(1),
                    state: Some(StateLink {
                        root_uid: "root-uid".into(),
                        name: linked_state_name.clone(),
                        uid: "state-uid".into(),
                    }),
                    ..WebApplicationStatus::default()
                },
                state_name: linked_state_name,
                state_uid: "state-uid".into(),
                state_resource_version: "state-rv-1".into(),
                root_resource_version: "root-rv-1".into(),
                root_finalizer_present: true,
                state_finalizer_present: true,
                deleting: false,
            },
            OxideLoopStateSpec {
                state_version: oxideloop_controller_api::STATE_VERSION_V1,
                root: RootBinding {
                    uid: "root-uid".into(),
                    bootstrap_id: bootstrap_id("root-uid").unwrap(),
                    bundle_digest: "a".repeat(64),
                },
                state: StateBody::default(),
            },
        )
        .unwrap()
    }

    fn informers_pending_actor() -> RootActorV0 {
        let mut actor = RootActorV0::new(linked_machine());
        actor.start();
        let CommandV0::RequestDescriptor { token, .. } = actor
            .next_command()
            .expect("descriptor request is issued")
            .expect("linked actor requests its descriptor")
        else {
            panic!("linked actor did not issue a descriptor request");
        };
        actor
            .acknowledge(EventV0::DescriptorReady {
                token,
                bundle_digest: "a".repeat(64),
            })
            .expect("descriptor acknowledgement is exact");
        assert!(
            actor
                .next_command()
                .expect("descriptor acknowledgement reduces")
                .is_none(),
            "linked actor should now await sealed informer readiness"
        );
        assert_eq!(actor.state().phase(), PhaseV0::InformersPending);
        actor
    }

    #[allow(clippy::too_many_lines)]
    fn mutation_pending() -> (MachineStateV0, EventV0) {
        let started = reduce(linked_machine(), EventV0::Start).unwrap();
        let descriptor_token = match &started.commands[0] {
            CommandV0::RequestDescriptor { token, .. } => token.clone(),
            command => panic!("unexpected start command: {command:?}"),
        };
        let descriptor = reduce(
            started.state,
            EventV0::DescriptorReady {
                token: descriptor_token,
                bundle_digest: "a".repeat(64),
            },
        )
        .unwrap();
        let render = reduce(descriptor.state, EventV0::InformersReady).unwrap();
        let input_document = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                uid: "root-uid".into(),
                namespace: "ns".into(),
                name: render.state.root().name.clone(),
                generation: 7,
                spec: serde_json::to_value(&render.state.root().spec).unwrap(),
            },
            bundle_digest: "a".repeat(64),
            cache_epoch: "1".into(),
            observation_token: "observation-1".into(),
            observations: vec![],
        };
        let input = serde_json::to_string(&input_document).unwrap();
        let frozen = FrozenInputV0 {
            root_generation: 7,
            state_resource_version: "state-rv-1".into(),
            observation_token: "observation-1".into(),
            cache_epoch: "1".into(),
            final_observation_fence_token: "f".repeat(64),
            render_input_hash: parse_validate_render_input(&input).unwrap().sha256,
        };
        let identity = oxideloop_controller_api::ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "managed".into(),
        };
        let output = serde_json::to_string(&RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::Apply,
            resources: vec![ResourceDeclaration {
                logical_id: "managed".into(),
                object: serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": "managed", "namespace": "ns"},
                    "data": {"value": "one"}
                }),
                readiness: None,
            }],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        })
        .unwrap();
        let observed = vec![DesiredObservationV0 {
            identity: identity.clone(),
            observation: Observation::Absent,
            qualification: BackendQualification::BuiltInTested {
                kubernetes_minor: "1.36".into(),
                gvr: "configmaps".into(),
            },
            recovery_proof: None,
            deletion_recovery_proof: None,
        }];
        let prepared = reduce(
            render.state,
            EventV0::Rendered {
                frozen: frozen.clone(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let operation = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            command => panic!("unexpected prepare command: {command:?}"),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: "state-rv-2".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let mut frozen = frozen;
        frozen.state_resource_version = "state-rv-2".into();
        let issued = reduce(
            revalidate.state,
            EventV0::Revalidated {
                frozen,
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let (transaction_id, attempt_token, command) = match &issued.commands[0] {
            CommandV0::Mutate {
                transaction_id,
                attempt_token,
                commands,
            } => (
                transaction_id.clone(),
                attempt_token.clone(),
                commands[0].clone(),
            ),
            command => panic!("unexpected mutation command: {command:?}"),
        };
        let ack = EventV0::MutationAcknowledged {
            transaction_id,
            attempt_token: attempt_token.clone(),
            results: vec![ApplyResult {
                command_id: command.command_id,
                attempt_token,
                identity,
                outcome: ApplyOutcome::Applied,
                uid: Some("managed-uid".into()),
                resource_version: Some("managed-rv-1".into()),
            }],
        };
        (issued.state, ack)
    }

    fn acknowledge_persist(transition: TransitionV0, new_rv: &str) -> TransitionV0 {
        let operation = match &transition.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            command => panic!("unexpected persistence command: {command:?}"),
        };
        reduce(
            transition.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: new_rv.into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap()
    }

    fn ready_machine() -> MachineStateV0 {
        let started = reduce(linked_machine(), EventV0::Start).unwrap();
        let descriptor_token = match &started.commands[0] {
            CommandV0::RequestDescriptor { token, .. } => token.clone(),
            command => panic!("unexpected start command: {command:?}"),
        };
        let descriptor = reduce(
            started.state,
            EventV0::DescriptorReady {
                token: descriptor_token,
                bundle_digest: "a".repeat(64),
            },
        )
        .unwrap();
        let render = reduce(descriptor.state, EventV0::InformersReady).unwrap();
        let input_document = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                uid: "root-uid".into(),
                namespace: "ns".into(),
                name: render.state.root().name.clone(),
                generation: 7,
                spec: serde_json::to_value(&render.state.root().spec).unwrap(),
            },
            bundle_digest: "a".repeat(64),
            cache_epoch: "1".into(),
            observation_token: "observation-1".into(),
            observations: vec![],
        };
        let input = serde_json::to_string(&input_document).unwrap();
        let mut frozen = FrozenInputV0 {
            root_generation: 7,
            state_resource_version: "state-rv-1".into(),
            observation_token: "observation-1".into(),
            cache_epoch: "1".into(),
            final_observation_fence_token: "f".repeat(64),
            render_input_hash: parse_validate_render_input(&input).unwrap().sha256,
        };
        let output = serde_json::to_string(&RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::ExplicitEmpty,
            resources: vec![],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        })
        .unwrap();
        let prepared = reduce(
            render.state,
            EventV0::Rendered {
                frozen: frozen.clone(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: vec![],
            },
        )
        .unwrap();
        let revalidate = acknowledge_persist(prepared, "state-rv-2");
        frozen.state_resource_version = "state-rv-2".into();
        let completion = reduce(
            revalidate.state,
            EventV0::Revalidated {
                frozen,
                render_input_json: input,
                render_output_json: output,
                observed: vec![],
            },
        )
        .unwrap();
        acknowledge_persist(completion, "state-rv-3").state
    }

    fn automation_trigger(source_rv: &str) -> ActionTriggerV0 {
        let mut trigger = ActionTriggerV0 {
            automation_identity: "smoke".into(),
            automation_version: "v1".into(),
            action_module_identity: "actions/smoke.mjs".into(),
            action_module_digest: "b".repeat(64),
            source: oxideloop_controller_api::ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "source".into(),
            },
            source_uid: "source-uid".into(),
            source_resource_version: source_rv.into(),
            observation_token: String::new(),
            event_metadata: serde_json::json!({"reason":"changed"}),
        };
        trigger.observation_token = automation_source_transition_token(&trigger);
        trigger
    }

    fn automation_ensure_pending() -> (MachineStateV0, EventV0) {
        let baseline = reduce(
            ready_machine(),
            EventV0::EstablishAutomationBaseline {
                trigger: automation_trigger("source-rv-1"),
            },
        )
        .unwrap();
        let ready = acknowledge_persist(baseline, "state-rv-4");
        let issued = reduce(
            ready.state,
            EventV0::TriggerAutomation {
                trigger: automation_trigger("source-rv-2"),
            },
        )
        .unwrap();
        let (token, name, spec) = match &issued.commands[0] {
            CommandV0::EnsureAutomationRun {
                token, name, spec, ..
            } => (token.clone(), name.clone(), spec.clone()),
            command => panic!("unexpected automation command: {command:?}"),
        };
        (
            issued.state,
            EventV0::AutomationRunEnsured {
                token,
                name,
                spec,
                uid: "run-uid".into(),
                resource_version: "run-rv-1".into(),
            },
        )
    }

    fn automation_ensure_pending_command() -> (MachineStateV0, EventV0, CommandV0) {
        let (machine, acknowledgement) = automation_ensure_pending();
        let EventV0::AutomationRunEnsured {
            token, name, spec, ..
        } = &acknowledgement
        else {
            panic!("automation pending fixture has an unexpected acknowledgement");
        };
        let root = machine.root();
        let command = CommandV0::EnsureAutomationRun {
            token: token.clone(),
            name: name.clone(),
            spec: spec.clone(),
            preconditions: PreconditionsV0 {
                root_uid: root.uid.clone(),
                root_generation: root.generation,
                root_resource_version: root.root_resource_version.clone(),
                state_uid: root.state_uid.clone(),
                state_resource_version: root.state_resource_version.clone(),
                observation_token: String::new(),
                cache_epoch: String::new(),
            },
        };
        (machine, acknowledgement, command)
    }

    fn automation_completion_pending() -> (MachineStateV0, EventV0) {
        let (machine, ensured) = automation_ensure_pending();
        let spec = match &ensured {
            EventV0::AutomationRunEnsured { spec, .. } => spec.clone(),
            _ => panic!("expected automation ensure acknowledgement"),
        };
        let pending = reduce(machine, ensured).unwrap();
        let ready = acknowledge_persist(pending, "state-rv-5");
        let run = ready.state.journal().state.automation.pending[0].clone();
        (
            ready.state,
            EventV0::AutomationCompleted {
                spec,
                occurrence_id: run.occurrence_id,
                name: run.run_name,
                uid: run.run_uid,
                old_resource_version: run.run_resource_version,
                new_resource_version: "run-rv-2".into(),
                completion: AutomationRunCompletionState::Succeeded,
                attempts: 1,
                materialization_name: "event-one".into(),
                materialization_uid: "event-uid".into(),
            },
        )
    }

    #[test]
    fn authenticated_automation_completion_has_priority_over_a_coalesced_root_refresh() {
        let (machine, completion) = automation_completion_pending();
        let mut actor = RootActorV0::new(machine);
        let mut refreshed = actor.state().root().clone();
        refreshed.root_resource_version = "root-rv-after-completion".into();
        assert!(actor.root_observed(refreshed));

        actor
            .acknowledge_automation_completion(completion)
            .expect("the exact durable pending run is accepted into priority acknowledgement");
        let _ = actor
            .next_command()
            .expect("completion reduces before the queued root closure");
        assert!(
            actor.state().journal().state.automation.pending.is_empty(),
            "completion must reduce before root refresh can replace its continuation"
        );
        assert_eq!(
            actor.state().journal().state.automation.completions.len(),
            1,
            "the accepted completion is durable before root closure/rebase"
        );
    }

    #[test]
    fn priority_automation_completion_rejects_a_foreign_run_pin() {
        let (machine, mut completion) = automation_completion_pending();
        let mut actor = RootActorV0::new(machine);
        let EventV0::AutomationCompleted { uid, .. } = &mut completion else {
            panic!("fixture must create an automation completion");
        };
        *uid = "foreign-run-uid".into();
        assert!(matches!(
            actor.acknowledge_automation_completion(completion),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }

    fn deleting_machine(
        inventory: Vec<InventoryEntry>,
        automation_pending: Vec<AutomationPending>,
    ) -> MachineStateV0 {
        let ready = ready_machine();
        let mut root = ready.root().clone();
        root.deleting = true;
        let mut journal = ready.journal().clone();
        journal.state.inventory = inventory;
        journal.state.automation.pending = automation_pending;
        journal.state.deletion = Some(DeletionTransaction {
            id: "d".repeat(64),
            deletion_start_generation: root.generation,
        });
        MachineStateV0::recover(root, journal).unwrap()
    }

    struct DeletionWireExpectation {
        path: String,
        response: Response<Body>,
    }

    #[derive(Clone)]
    struct DeletionWireApi {
        expected: Arc<Mutex<VecDeque<DeletionWireExpectation>>>,
    }

    impl DeletionWireApi {
        fn transport(
            expected: impl IntoIterator<Item = DeletionWireExpectation>,
        ) -> (KubeTransportV0, Self) {
            let api = Self {
                expected: Arc::new(Mutex::new(expected.into_iter().collect())),
            };
            let queued = Arc::clone(&api.expected);
            let service = service_fn(move |request: Request<Body>| {
                let queued = Arc::clone(&queued);
                async move {
                    let expected = queued
                        .lock()
                        .expect("deletion wire script lock")
                        .pop_front()
                        .expect("unexpected deletion observation request");
                    assert_eq!(request.method(), Method::GET);
                    assert_eq!(
                        request
                            .uri()
                            .path_and_query()
                            .expect("deletion observation request URI")
                            .as_str(),
                        expected.path
                    );
                    let body = request
                        .into_body()
                        .collect_bytes()
                        .await
                        .expect("read deletion observation request body");
                    assert!(body.is_empty());
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
                self.expected
                    .lock()
                    .expect("deletion wire script lock")
                    .is_empty(),
                "unissued deletion observation requests"
            );
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn deletion_response(value: serde_json::Value) -> Response<Body> {
        Response::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&value).expect("deletion response JSON"),
            ))
            .expect("deletion response")
    }

    #[allow(clippy::needless_pass_by_value)]
    fn deletion_list(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": format!("{kind}List"),
            "metadata": {"resourceVersion": "list-rv"},
            "items": items,
        })
    }

    fn deletion_root(actor: &RootActorV0, resource_version: &str) -> serde_json::Value {
        let root = actor.state().root();
        serde_json::json!({
            "apiVersion": "demo.oxideloop.dev/v1alpha1",
            "kind": "WebApplication",
            "metadata": {
                "name": root.name,
                "namespace": root.namespace,
                "uid": root.uid,
                "resourceVersion": resource_version,
                "generation": root.generation,
                "deletionTimestamp": "2026-08-30T00:00:00Z",
            },
        })
    }

    fn deletion_state(actor: &RootActorV0, resource_version: &str) -> serde_json::Value {
        let root = actor.state().root();
        serde_json::json!({
            "apiVersion": "runtime.oxideloop.dev/v1alpha1",
            "kind": "OxideLoopState",
            "metadata": {
                "name": root.state_name,
                "namespace": root.namespace,
                "uid": root.state_uid,
                "resourceVersion": resource_version,
            },
        })
    }

    fn deletion_owner(actor: &RootActorV0) -> serde_json::Value {
        let root = actor.state().root();
        serde_json::json!([{
            "apiVersion": "demo.oxideloop.dev/v1alpha1",
            "kind": "WebApplication",
            "name": root.name,
            "uid": root.uid,
            "controller": true,
            "blockOwnerDeletion": false,
        }])
    }

    fn deletion_child(
        actor: &RootActorV0,
        api_version: &str,
        kind: &str,
        name: &str,
        uid: &str,
        resource_version: &str,
        owned: bool,
    ) -> serde_json::Value {
        let root = actor.state().root();
        let mut metadata = serde_json::json!({
            "name": name,
            "namespace": root.namespace,
            "uid": uid,
            "resourceVersion": resource_version,
        });
        if owned {
            metadata["ownerReferences"] = deletion_owner(actor);
        }
        serde_json::json!({
            "apiVersion": api_version,
            "kind": kind,
            "metadata": metadata,
        })
    }

    fn deletion_automation_run(
        actor: &RootActorV0,
        name: &str,
        uid: &str,
        resource_version: &str,
    ) -> serde_json::Value {
        let mut run = deletion_child(
            actor,
            "runtime.oxideloop.dev/v1alpha1",
            "AutomationRun",
            name,
            uid,
            resource_version,
            true,
        );
        run["spec"] = serde_json::json!({
            "occurrenceId": "occurrence-one",
            "rootUid": actor.state().root().uid,
            "source": {"apiVersion":"v1","kind":"ConfigMap","namespace":"ns","name":"source"},
            "sourceUid": "source-uid",
            "sourceResourceVersion": "source-rv",
            "sourceTransitionId": "transition",
            "automationId": "smoke",
            "automationVersion": "v1",
            "bundleDigest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "program": {"configMapName":"bundle","key":"program.mjs","digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "actionModuleIdentity": "action.mjs",
            "actionModuleDigest": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "rendererAbiVersion": "renderer.oxideloop.dev/v0",
            "actionInputV0": "{}",
            "actionInputHash": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "eventMetadataHash": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        });
        run
    }

    fn deletion_request_actor(
        inventory: Vec<InventoryEntry>,
        automation_pending: Vec<AutomationPending>,
    ) -> RootActorV0 {
        let mut actor = RootActorV0::new(deleting_machine(inventory, automation_pending));
        actor.start();
        let prepare = actor
            .next_command()
            .expect("deletion prepare command")
            .expect("deleting actor must persist preparation");
        actor
            .acknowledge(persisted_event(&prepare, "state-rv-deletion-observation"))
            .expect("deletion preparation acknowledgement");
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::RequestDeletionObservation { .. }))
        ));
        actor
    }

    fn deletion_address(actor: &RootActorV0) -> oxideloop_kube_transport::RootAddressV0 {
        let root = actor.state().root();
        oxideloop_kube_transport::RootAddressV0 {
            namespace: root.namespace.clone(),
            name: root.name.clone(),
            state_name: root.state_name.clone(),
        }
    }

    fn deletion_fence_prefix(actor: &RootActorV0) -> Vec<DeletionWireExpectation> {
        let root = actor.state().root();
        vec![
            DeletionWireExpectation {
                path: format!(
                    "/apis/demo.oxideloop.dev/v1alpha1/namespaces/{}/webapplications/{}",
                    root.namespace, root.name
                ),
                response: deletion_response(deletion_root(actor, &root.root_resource_version)),
            },
            DeletionWireExpectation {
                path: format!(
                    "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/{}/oxideloopstates/{}",
                    root.namespace, root.state_name
                ),
                response: deletion_response(deletion_state(actor, &root.state_resource_version)),
            },
        ]
    }

    #[tokio::test]
    async fn deletion_observation_port_empty_fence_acknowledges_the_owned_request() {
        let mut actor = deletion_request_actor(vec![], vec![]);
        let mut script = deletion_fence_prefix(&actor);
        script.extend([
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/configmaps?".into(),
                response: deletion_response(deletion_list("ConfigMap", vec![])),
            },
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/services?".into(),
                response: deletion_response(deletion_list("Service", vec![])),
            },
            DeletionWireExpectation {
                path: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/ns/automationruns?".into(),
                response: deletion_response(deletion_list("AutomationRun", vec![])),
            },
        ]);
        let (transport, wire) = DeletionWireApi::transport(script);
        let port = KubeDeletionObservationPortV0::new(&transport, deletion_address(&actor));
        let observation = port
            .observe_for_actor(&actor)
            .await
            .expect("exact empty deletion fence");
        actor
            .acknowledge_deletion_observation(observation)
            .expect("sealed empty deletion observation acknowledgement");
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::VerifyOwnedChildrenAbsent { .. }))
        ));
        wire.assert_drained();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn deletion_observation_port_retains_only_exact_owned_children_on_all_routes() {
        let config = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "owned-config".into(),
            },
            logical_id: "owned-config".into(),
            uid: "config-uid".into(),
            resource_version: "config-rv".into(),
            bundle_digest: "a".repeat(64),
        };
        let service = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "Service".into(),
                namespace: "ns".into(),
                name: "owned-service".into(),
            },
            logical_id: "owned-service".into(),
            uid: "service-uid".into(),
            resource_version: "service-rv".into(),
            bundle_digest: "a".repeat(64),
        };
        let pending = AutomationPending {
            occurrence_id: "occurrence-one".into(),
            automation_id: "smoke".into(),
            source: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "source".into(),
            },
            run_name: "owned-run".into(),
            run_uid: "run-uid".into(),
            run_resource_version: "run-rv".into(),
            spec_hash: "a".repeat(64),
        };
        let mut actor =
            deletion_request_actor(vec![config.clone(), service.clone()], vec![pending]);
        let config_value = deletion_child(
            &actor,
            "v1",
            "ConfigMap",
            "owned-config",
            "config-uid",
            "config-rv",
            true,
        );
        let foreign_config = deletion_child(
            &actor,
            "v1",
            "ConfigMap",
            "foreign-config",
            "foreign-uid",
            "foreign-rv",
            false,
        );
        let service_value = deletion_child(
            &actor,
            "v1",
            "Service",
            "owned-service",
            "service-uid",
            "service-rv",
            true,
        );
        let run_value = deletion_automation_run(&actor, "owned-run", "run-uid", "run-rv");
        let mut script = deletion_fence_prefix(&actor);
        script.extend([
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/configmaps?".into(),
                response: deletion_response(deletion_list(
                    "ConfigMap",
                    vec![config_value.clone(), foreign_config],
                )),
            },
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/configmaps/owned-config".into(),
                response: deletion_response(config_value),
            },
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/services?".into(),
                response: deletion_response(deletion_list("Service", vec![service_value.clone()])),
            },
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/services/owned-service".into(),
                response: deletion_response(service_value),
            },
            DeletionWireExpectation {
                path: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/ns/automationruns?".into(),
                response: deletion_response(deletion_list(
                    "AutomationRun",
                    vec![run_value.clone()],
                )),
            },
            DeletionWireExpectation {
                path: "/apis/runtime.oxideloop.dev/v1alpha1/namespaces/ns/automationruns/owned-run"
                    .into(),
                response: deletion_response(run_value),
            },
        ]);
        let (transport, wire) = DeletionWireApi::transport(script);
        let port = KubeDeletionObservationPortV0::new(&transport, deletion_address(&actor));
        let observation = port
            .observe_for_actor(&actor)
            .await
            .expect("owned exact deletion children");
        assert_eq!(observation.observed_inventory.len(), 2);
        assert_eq!(observation.observed_automation_runs.len(), 1);
        actor
            .acknowledge_deletion_observation(observation)
            .expect("sealed owned deletion observation acknowledgement");
        wire.assert_drained();
    }

    #[tokio::test]
    async fn deletion_observation_port_rejects_stale_root_or_state_pins() {
        for stale_state in [false, true] {
            let actor = deletion_request_actor(vec![], vec![]);
            let root = actor.state().root();
            let mut script = deletion_fence_prefix(&actor);
            if stale_state {
                script[1].response = deletion_response(deletion_state(&actor, "stale-state-rv"));
            } else {
                script[0].response = deletion_response(deletion_root(&actor, "stale-root-rv"));
            }
            let (transport, wire) = DeletionWireApi::transport(script);
            let port = KubeDeletionObservationPortV0::new(&transport, deletion_address(&actor));
            assert!(matches!(
                port.observe_for_actor(&actor).await,
                Err(RuntimeErrorV0::Discovery(TransportErrorV0::Stale(_)))
            ));
            assert!(
                actor.in_flight,
                "stale fence must not acknowledge the request"
            );
            wire.assert_drained();
            assert_eq!(root.uid, "root-uid");
        }
    }

    #[tokio::test]
    async fn deletion_observation_port_holds_when_listed_child_changes_before_exact_get() {
        let inventory = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "owned-config".into(),
            },
            logical_id: "owned-config".into(),
            uid: "listed-uid".into(),
            resource_version: "listed-rv".into(),
            bundle_digest: "a".repeat(64),
        };
        let actor = deletion_request_actor(vec![inventory], vec![]);
        let listed = deletion_child(
            &actor,
            "v1",
            "ConfigMap",
            "owned-config",
            "listed-uid",
            "listed-rv",
            true,
        );
        let replacement = deletion_child(
            &actor,
            "v1",
            "ConfigMap",
            "owned-config",
            "replacement-uid",
            "replacement-rv",
            true,
        );
        let mut script = deletion_fence_prefix(&actor);
        script.extend([
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/configmaps?".into(),
                response: deletion_response(deletion_list("ConfigMap", vec![listed])),
            },
            DeletionWireExpectation {
                path: "/api/v1/namespaces/ns/configmaps/owned-config".into(),
                response: deletion_response(replacement),
            },
        ]);
        let (transport, wire) = DeletionWireApi::transport(script);
        let port = KubeDeletionObservationPortV0::new(&transport, deletion_address(&actor));
        assert!(matches!(
            port.observe_for_actor(&actor).await,
            Err(RuntimeErrorV0::Discovery(TransportErrorV0::Stale(
                "deletion child identity"
            )))
        ));
        assert!(
            actor.in_flight,
            "replaced child must not be treated as absent"
        );
        wire.assert_drained();
    }

    fn deletion_mutation_pending() -> (MachineStateV0, EventV0) {
        let identity = oxideloop_controller_api::ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "managed".into(),
        };
        let inventory = InventoryEntry {
            identity: identity.clone(),
            logical_id: "managed".into(),
            uid: "managed-uid".into(),
            resource_version: "managed-rv-1".into(),
            bundle_digest: "a".repeat(64),
        };
        let prepared = reduce(
            deleting_machine(vec![inventory.clone()], vec![]),
            EventV0::Start,
        )
        .unwrap();
        let observation = acknowledge_persist(prepared, "state-rv-4");
        let issued = reduce(
            observation.state,
            EventV0::DeletionRevalidated {
                observed_inventory: vec![inventory],
                observed_automation_runs: vec![],
            },
        )
        .unwrap();
        let (transaction_id, attempt_token, command) = match &issued.commands[0] {
            CommandV0::Mutate {
                transaction_id,
                attempt_token,
                commands,
            } => (
                transaction_id.clone(),
                attempt_token.clone(),
                commands[0].clone(),
            ),
            command => panic!("unexpected deletion mutation command: {command:?}"),
        };
        (
            issued.state,
            EventV0::MutationAcknowledged {
                transaction_id,
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: command.command_id,
                    attempt_token,
                    identity,
                    outcome: ApplyOutcome::Deleted,
                    uid: Some("managed-uid".into()),
                    resource_version: Some("managed-rv-1".into()),
                }],
            },
        )
    }

    fn deletion_automation_cleanup_pending() -> (MachineStateV0, EventV0) {
        let source = oxideloop_controller_api::ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "source".into(),
        };
        let pending = AutomationPending {
            occurrence_id: "occurrence-one".into(),
            automation_id: "smoke".into(),
            source,
            run_name: "ol-run-one".into(),
            run_uid: "run-uid".into(),
            run_resource_version: "run-rv-1".into(),
            spec_hash: "a".repeat(64),
        };
        let prepared = reduce(
            deleting_machine(vec![], vec![pending.clone()]),
            EventV0::Start,
        )
        .unwrap();
        let observation = acknowledge_persist(prepared, "state-rv-4");
        let run = AutomationRunCleanupV0 {
            occurrence_id: pending.occurrence_id,
            name: pending.run_name,
            uid: pending.run_uid,
            resource_version: pending.run_resource_version,
        };
        let issued = reduce(
            observation.state,
            EventV0::DeletionRevalidated {
                observed_inventory: vec![],
                observed_automation_runs: vec![run.clone()],
            },
        )
        .unwrap();
        let token = match &issued.commands[0] {
            CommandV0::DeleteAutomationRun { token, .. } => token.clone(),
            command => panic!("unexpected automation cleanup command: {command:?}"),
        };
        (
            issued.state,
            EventV0::AutomationRunDeleted {
                token,
                occurrence_id: run.occurrence_id,
                name: run.name,
                uid: run.uid,
                resource_version: run.resource_version,
            },
        )
    }

    fn acknowledge_owned_children(command: &CommandV0) -> EventV0 {
        let CommandV0::VerifyOwnedChildrenAbsent {
            token,
            preconditions,
        } = command
        else {
            panic!("unexpected owned-child verification command: {command:?}");
        };
        EventV0::OwnedChildrenVerifiedAbsent {
            token: token.clone(),
            root_uid: preconditions.root_uid.clone(),
            root_generation: preconditions.root_generation,
            root_resource_version: preconditions.root_resource_version.clone(),
            state_uid: preconditions.state_uid.clone(),
            state_resource_version: preconditions.state_resource_version.clone(),
            cleanup_token: token.clone(),
        }
    }

    fn state_backed_root_finalizer_pending() -> (MachineStateV0, EventV0) {
        let prepared = reduce(deleting_machine(vec![], vec![]), EventV0::Start).unwrap();
        let observation = acknowledge_persist(prepared, "state-rv-4");
        let verify_state = reduce(
            observation.state,
            EventV0::DeletionRevalidated {
                observed_inventory: vec![],
                observed_automation_runs: vec![],
            },
        )
        .unwrap();
        let receipt = reduce(
            verify_state.state,
            acknowledge_owned_children(&verify_state.commands[0]),
        )
        .unwrap();
        let (status_token, old_root_rv) = match &receipt.commands[0] {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            } => (token.clone(), old_root_resource_version.clone()),
            command => panic!("unexpected deletion receipt command: {command:?}"),
        };
        let state_finalizer = reduce(
            receipt.state,
            EventV0::RootStatusPersisted {
                token: status_token,
                old_root_resource_version: old_root_rv,
                new_root_resource_version: "root-rv-2".into(),
                root_uid: "root-uid".into(),
            },
        )
        .unwrap();
        let (state_token, old_state_rv) = match &state_finalizer.commands[0] {
            CommandV0::RemoveStateFinalizer {
                token,
                preconditions,
            } => (token.clone(), preconditions.state_resource_version.clone()),
            command => panic!("unexpected state finalizer command: {command:?}"),
        };
        let verify_root = reduce(
            state_finalizer.state,
            EventV0::StateFinalizerRemoved {
                token: state_token,
                state_uid: "state-uid".into(),
                old_state_resource_version: old_state_rv,
                new_state_resource_version: "state-rv-5".into(),
            },
        )
        .unwrap();
        let root_finalizer = reduce(
            verify_root.state,
            acknowledge_owned_children(&verify_root.commands[0]),
        )
        .unwrap();
        let (token, old_root_rv) = match &root_finalizer.commands[0] {
            CommandV0::RemoveRootFinalizer {
                token,
                preconditions,
            } => (token.clone(), preconditions.root_resource_version.clone()),
            command => panic!("unexpected root finalizer command: {command:?}"),
        };
        (
            root_finalizer.state,
            EventV0::RootFinalizerRemoved {
                token,
                root_uid: "root-uid".into(),
                old_root_resource_version: old_root_rv,
                new_root_resource_version: "root-rv-3".into(),
            },
        )
    }

    fn advanced_root(machine: &MachineStateV0, generation: i64, suffix: &str) -> RootSnapshotV0 {
        let mut root = machine.root().clone();
        root.generation = generation;
        root.root_resource_version = format!("root-rv-{suffix}");
        root.status.observed_generation = Some(generation);
        root
    }

    fn prune_render_documents(
        root: &RootSnapshotV0,
        state_resource_version: &str,
        observation_token: &str,
        cache_epoch: &str,
    ) -> (FrozenInputV0, String, String, Vec<DesiredObservationV0>) {
        let input_document = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                uid: root.uid.clone(),
                namespace: root.namespace.clone(),
                name: root.name.clone(),
                generation: u64::try_from(root.generation).unwrap(),
                spec: serde_json::to_value(&root.spec).unwrap(),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: cache_epoch.into(),
            observation_token: observation_token.into(),
            observations: vec![],
        };
        let input = serde_json::to_string(&input_document).unwrap();
        let output = serde_json::to_string(&RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::ExplicitEmpty,
            resources: vec![],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        })
        .unwrap();
        let identity = oxideloop_controller_api::ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "ns".into(),
            name: "managed".into(),
        };
        (
            FrozenInputV0 {
                root_generation: root.generation,
                state_resource_version: state_resource_version.into(),
                observation_token: observation_token.into(),
                cache_epoch: cache_epoch.into(),
                final_observation_fence_token: oxideloop_renderer_abi::sha256_hex(
                    format!("prune-final-fence/v0/{observation_token}").as_bytes(),
                ),
                render_input_hash: parse_validate_render_input(&input).unwrap().sha256,
            },
            input,
            output,
            vec![DesiredObservationV0 {
                identity,
                observation: Observation::Existing {
                    identity: oxideloop_mutation_model::ObjectIdentity {
                        uid: "managed-uid".into(),
                        resource_version: "managed-rv-1".into(),
                    },
                },
                qualification: BackendQualification::BuiltInTested {
                    kubernetes_minor: "1.36".into(),
                    gvr: "configmaps".into(),
                },
                recovery_proof: None,
                deletion_recovery_proof: None,
            }],
        )
    }

    fn persisted_event(command: &CommandV0, new_state_rv: &str) -> EventV0 {
        let CommandV0::Persist { operation, .. } = command else {
            panic!("unexpected non-persist command: {command:?}");
        };
        EventV0::Persisted {
            token: operation.token.clone(),
            old_state_resource_version: operation.old_state_resource_version.clone(),
            new_state_resource_version: new_state_rv.into(),
            state_uid: "state-uid".into(),
        }
    }

    fn assert_actor_closure(
        machine: &MachineStateV0,
        ack: &EventV0,
        expected: GenerationAdvanceClosurePurpose,
    ) {
        for watch_first in [true, false] {
            let root = advanced_root(machine, 8, "2");
            let mut actor = RootActorV0::new(machine.clone());
            actor.priority_ack = Some(ack.clone());
            if watch_first {
                assert!(actor.root_observed(root.clone()));
            } else {
                assert!(actor.root_observed(root));
            }
            assert!(matches!(
                actor.next_command(),
                Ok(Some(CommandV0::Persist {
                    operation: oxideloop_controller_api::PersistOperation {
                        purpose: oxideloop_controller_api::PersistPurpose::GenerationAdvanceClosure,
                        ..
                    },
                    ..
                }))
            ));
            assert_eq!(actor.state().root().generation, 8);
            assert_eq!(
                actor
                    .state()
                    .journal()
                    .state
                    .generation_advance_closure
                    .as_ref()
                    .map(|closure| closure.purpose),
                Some(expected)
            );
        }
    }

    #[test]
    fn actor_closes_mutation_result_across_root_race() {
        let (machine, ack) = mutation_pending();
        assert_actor_closure(
            &machine,
            &ack,
            GenerationAdvanceClosurePurpose::MutationResult,
        );
    }

    #[test]
    fn actor_closes_automation_pending_across_root_race() {
        let (machine, ack) = automation_ensure_pending();
        assert_actor_closure(
            &machine,
            &ack,
            GenerationAdvanceClosurePurpose::AutomationPending,
        );
    }

    #[test]
    fn actor_closes_automation_completion_across_root_race() {
        let (machine, ack) = automation_completion_pending();
        assert_actor_closure(
            &machine,
            &ack,
            GenerationAdvanceClosurePurpose::AutomationCompletion,
        );
    }

    #[test]
    fn actor_closes_deletion_mutation_result_across_root_race() {
        let (machine, ack) = deletion_mutation_pending();
        assert_actor_closure(
            &machine,
            &ack,
            GenerationAdvanceClosurePurpose::DeletionMutationResult,
        );
    }

    #[test]
    fn actor_closes_deletion_automation_cleanup_across_root_race() {
        let (machine, ack) = deletion_automation_cleanup_pending();
        assert_actor_closure(
            &machine,
            &ack,
            GenerationAdvanceClosurePurpose::DeletionAutomationCleanup,
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn durable_closure_process_recreation_preserves_deletion_for_all_five_purposes() {
        let cases = vec![
            (
                "mutation-result",
                mutation_pending(),
                GenerationAdvanceClosurePurpose::MutationResult,
                PersistPurpose::DeletionStart,
            ),
            (
                "automation-pending",
                automation_ensure_pending(),
                GenerationAdvanceClosurePurpose::AutomationPending,
                PersistPurpose::DeletionStart,
            ),
            (
                "automation-completion",
                automation_completion_pending(),
                GenerationAdvanceClosurePurpose::AutomationCompletion,
                PersistPurpose::DeletionStart,
            ),
            (
                "deletion-mutation-result",
                deletion_mutation_pending(),
                GenerationAdvanceClosurePurpose::DeletionMutationResult,
                PersistPurpose::DeletionMutationResult,
            ),
            (
                "deletion-automation-cleanup",
                deletion_automation_cleanup_pending(),
                GenerationAdvanceClosurePurpose::DeletionAutomationCleanup,
                PersistPurpose::DeletionAutomationCleanup,
            ),
        ]
        .into_boxed_slice();

        for (name, (machine, ack), purpose, expected_recovery_purpose) in cases {
            let mut actor = RootActorV0::new(machine);
            actor.priority_ack = Some(ack);
            let mut deletion_refresh = advanced_root(actor.state(), 8, "closure");
            deletion_refresh.deleting = true;
            assert!(actor.root_observed(deletion_refresh));
            let first_closure = actor
                .next_command()
                .unwrap_or_else(|error| panic!("{name} first closure failed: {error}"))
                .unwrap_or_else(|| panic!("{name} emitted no first closure"));
            assert!(matches!(
                first_closure,
                CommandV0::Persist { ref operation, .. }
                    if operation.purpose == PersistPurpose::GenerationAdvanceClosure
            ));

            // Model a stale preflight after the acknowledgement facts were
            // accepted but before this closure could write. Only fresh
            // root/state pins are installed; the accepted journal facts stay
            // in the actor and the closure is rebased without effect replay.
            let mut fresh = advanced_root(actor.state(), 9, "closure-rebase");
            fresh.deleting = true;
            fresh.state_resource_version = format!("state-rv-stale-{name}");
            actor.recover_stale_preflight(fresh);
            let rebased = actor
                .next_command()
                .unwrap_or_else(|error| panic!("{name} closure rebase failed: {error}"))
                .unwrap_or_else(|| panic!("{name} emitted no rebased closure"));
            assert!(matches!(
                rebased,
                CommandV0::Persist { ref operation, .. }
                    if operation.purpose == PersistPurpose::GenerationAdvanceClosure
            ));
            assert_eq!(
                actor
                    .state()
                    .journal()
                    .state
                    .generation_advance_closure
                    .as_ref()
                    .map(|closure| (closure.purpose, closure.rebase_count)),
                Some((purpose, 1)),
                "{name} did not retain the exact rebased closure"
            );
            assert!(
                actor
                    .state()
                    .root()
                    .status
                    .deletion_cleanup_receipt
                    .is_none(),
                "{name} unexpectedly relied on a deletion receipt"
            );

            // The closure spec write lands, its acknowledgement is lost, and
            // the entire actor is destroyed. The loader observes the new state
            // RV, reconstructs from the durable state object, and a new actor
            // must resume deletion rather than any normal effect path.
            let durable_state_rv = format!("state-rv-closure-written-{name}");
            let loader = loader_port_for_machine(actor.state(), &durable_state_rv);
            drop(actor);
            let reloaded = load_machine_from_reader(&loader, "ns", "demo")
                .await
                .unwrap_or_else(|error| panic!("{name} loader recovery failed: {error}"))
                .unwrap_or_else(|| panic!("{name} root disappeared during recovery"));
            assert_eq!(
                reloaded.root().state_resource_version,
                durable_state_rv,
                "{name} did not install the state object's fresh RV"
            );
            let mut recreated_actor = RootActorV0::new(reloaded);
            recreated_actor.enqueue(EventV0::Start);
            let resumed = recreated_actor
                .next_command()
                .unwrap_or_else(|error| panic!("{name} recreated actor failed: {error}"))
                .unwrap_or_else(|| panic!("{name} recreated actor emitted no command"));
            assert!(matches!(
                resumed,
                CommandV0::Persist { ref operation, .. }
                    if operation.purpose == expected_recovery_purpose
            ));
            assert!(
                !matches!(
                    resumed,
                    CommandV0::Mutate { .. }
                        | CommandV0::EnsureAutomationRun { .. }
                        | CommandV0::RequestDescriptor { .. }
                        | CommandV0::RequestRender { .. }
                ),
                "{name} resumed normal work after deletion"
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn prepared_delete_recreated_actor_revalidates_before_exact_retry() {
        let base = linked_machine();
        let root = base.root().clone();
        let mut journal = base.journal().clone();
        journal.state.inventory.push(InventoryEntry {
            identity: oxideloop_controller_api::ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "managed".into(),
            },
            logical_id: "managed".into(),
            uid: "managed-uid".into(),
            resource_version: "managed-rv-1".into(),
            bundle_digest: "a".repeat(64),
        });
        let recovered = MachineStateV0::recover(root, journal).unwrap();
        let mut actor = RootActorV0::new(recovered);

        // First post-restart render establishes only the first prune barrier.
        actor.enqueue(EventV0::Start);
        let descriptor_token = match actor.next_command().unwrap().unwrap() {
            CommandV0::RequestDescriptor { token, .. } => token,
            command => panic!("unexpected recovery command: {command:?}"),
        };
        actor
            .acknowledge(EventV0::DescriptorReady {
                token: descriptor_token,
                bundle_digest: "a".repeat(64),
            })
            .unwrap();
        assert!(actor.next_command().unwrap().is_none());
        actor.enqueue(EventV0::InformersReady);
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let (first_frozen, first_input, output, observed) =
            prune_render_documents(actor.state().root(), "state-rv-1", "observation-1", "1");
        actor
            .accept_acknowledgement(EventV0::Rendered {
                frozen: first_frozen.clone(),
                render_input_json: first_input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            })
            .unwrap();
        let first_prepare = actor.next_command().unwrap().unwrap();
        actor
            .acknowledge(persisted_event(&first_prepare, "state-rv-2"))
            .unwrap();
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let mut first_revalidated_frozen = first_frozen;
        first_revalidated_frozen.state_resource_version = "state-rv-2".into();
        actor
            .accept_acknowledgement(EventV0::Revalidated {
                frozen: first_revalidated_frozen,
                render_input_json: first_input,
                render_output_json: output.clone(),
                observed: observed.clone(),
            })
            .unwrap();
        let first_completion = actor.next_command().unwrap().unwrap();
        actor
            .acknowledge(persisted_event(&first_completion, "state-rv-3"))
            .unwrap();
        assert!(actor.next_command().unwrap().is_none());

        // A genuinely new observation token supplies the second prune barrier.
        actor.enqueue(EventV0::ReconcileRequested {
            cause: InternalReconcileCauseV0::PruneConfirmation,
            observation_token: "observation-2".into(),
            cache_epoch: "1".into(),
        });
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let (second_frozen, second_input, _, second_observed) =
            prune_render_documents(actor.state().root(), "state-rv-3", "observation-2", "1");
        actor
            .accept_acknowledgement(EventV0::Rendered {
                frozen: second_frozen.clone(),
                render_input_json: second_input.clone(),
                render_output_json: output.clone(),
                observed: second_observed.clone(),
            })
            .unwrap();
        let status = actor.next_command().unwrap().unwrap();
        let (status_token, old_root_rv) = match &status {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            } => (token.clone(), old_root_resource_version.clone()),
            command => panic!("second barrier skipped root status durability: {command:?}"),
        };
        actor
            .acknowledge(EventV0::RootStatusPersisted {
                token: status_token,
                old_root_resource_version: old_root_rv,
                new_root_resource_version: "root-rv-2".into(),
                root_uid: "root-uid".into(),
            })
            .unwrap();
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        actor
            .accept_acknowledgement(EventV0::Rendered {
                frozen: second_frozen.clone(),
                render_input_json: second_input.clone(),
                render_output_json: output.clone(),
                observed: second_observed.clone(),
            })
            .unwrap();
        let delete_prepare = actor.next_command().unwrap().unwrap();
        actor
            .acknowledge(persisted_event(&delete_prepare, "state-rv-4"))
            .unwrap();
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));

        // Crash with the prepared delete durable but before the fresh
        // revalidation response. Reload through the production loader seam and
        // drive a brand-new actor through descriptor/informer/render again.
        let loader = loader_port_for_machine(actor.state(), "state-rv-4");
        drop(actor);
        let reloaded = load_machine_from_reader(&loader, "ns", "demo")
            .await
            .unwrap()
            .unwrap();
        let mut recreated = RootActorV0::new(reloaded);
        recreated.enqueue(EventV0::Start);
        let descriptor_token = match recreated.next_command().unwrap().unwrap() {
            CommandV0::RequestDescriptor { token, .. } => token,
            command => panic!("recreated delete skipped descriptor: {command:?}"),
        };
        recreated
            .acknowledge(EventV0::DescriptorReady {
                token: descriptor_token,
                bundle_digest: "a".repeat(64),
            })
            .unwrap();
        assert!(recreated.next_command().unwrap().is_none());
        recreated.enqueue(EventV0::InformersReady);
        assert!(matches!(
            recreated.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let mut recovered_frozen = second_frozen;
        recovered_frozen.state_resource_version = "state-rv-4".into();
        recreated
            .accept_acknowledgement(EventV0::Revalidated {
                frozen: recovered_frozen,
                render_input_json: second_input,
                render_output_json: output,
                observed: second_observed,
            })
            .unwrap();
        let retried = recreated.next_command().unwrap().unwrap();
        assert!(matches!(
            retried,
            CommandV0::Mutate { ref commands, .. }
                if matches!(
                    commands.as_slice(),
                    [oxideloop_controller_core::MutationCommandV0 {
                        decision: oxideloop_mutation_model::MutationDecision::PreconditionedDelete {
                            preconditions: oxideloop_mutation_model::ObjectIdentity {
                                uid,
                                resource_version,
                            },
                            ..
                        },
                        ..
                    }] if uid == "managed-uid" && resource_version == "managed-rv-1"
                )
        ));
    }

    struct StaleClosureDispatcher {
        calls: Vec<CommandV0>,
        fresh_root: RootSnapshotV0,
    }

    #[async_trait]
    impl KubernetesDispatcherV0 for StaleClosureDispatcher {
        async fn dispatch(
            &mut self,
            command: &CommandV0,
        ) -> Result<Option<EventV0>, RuntimeErrorV0> {
            self.calls.push(command.clone());
            if self.calls.len() == 1 {
                return Err(RuntimeErrorV0::Discovery(TransportErrorV0::Stale(
                    "injected stale preflight",
                )));
            }
            let CommandV0::Persist {
                operation,
                preconditions,
                ..
            } = command
            else {
                return Err(RuntimeErrorV0::Hold(
                    "stale closure fake received a non-persist effect".into(),
                ));
            };
            Ok(Some(EventV0::Persisted {
                token: operation.token.clone(),
                old_state_resource_version: preconditions.state_resource_version.clone(),
                new_state_resource_version: format!("state-rv-{}", self.calls.len() + 2),
                state_uid: preconditions.state_uid.clone(),
            }))
        }

        async fn fresh_root_snapshot(&mut self) -> Result<RootSnapshotV0, RuntimeErrorV0> {
            Ok(self.fresh_root.clone())
        }
    }

    #[tokio::test]
    async fn stale_persist_preflight_rebases_closure_without_replaying_effect() {
        let (machine, ack) = mutation_pending();
        let root = advanced_root(&machine, 8, "2");
        let mut actor = RootActorV0::new(machine);
        actor.priority_ack = Some(ack);
        actor.root_observed(root);

        let mut fresh_root = advanced_root(actor.state(), 9, "3");
        fresh_root.state_resource_version = "state-rv-3".into();
        let mut dispatcher = StaleClosureDispatcher {
            calls: vec![],
            fresh_root,
        };
        let external = drive_kubernetes_with_dispatcher(&mut actor, &mut dispatcher)
            .await
            .unwrap();
        assert!(matches!(external, Some(CommandV0::RequestRender { .. })));
        assert_eq!(dispatcher.calls.len(), 3);
        assert!(
            dispatcher
                .calls
                .iter()
                .all(|command| matches!(command, CommandV0::Persist { .. }))
        );
        assert!(matches!(
            &dispatcher.calls[0],
            CommandV0::Persist { operation, .. }
                if operation.purpose
                    == oxideloop_controller_api::PersistPurpose::GenerationAdvanceClosure
        ));
        assert!(matches!(
            &dispatcher.calls[1],
            CommandV0::Persist { operation, .. }
                if operation.purpose
                    == oxideloop_controller_api::PersistPurpose::GenerationAdvanceClosure
        ));
        assert_eq!(actor.state().root().generation, 9);
        assert_eq!(
            actor.state().journal().state.inventory[0].uid,
            "managed-uid"
        );
    }

    fn root_only_deletion_snapshot(root_finalizer_present: bool) -> RootSnapshotV0 {
        let linked_state_name = oxideloop_controller_api::state_name("root-uid").unwrap();
        RootSnapshotV0 {
            uid: "root-uid".into(),
            namespace: "ns".into(),
            name: "demo".into(),
            generation: 3,
            spec: root_spec("0".repeat(64)),
            bundle_digest: "0".repeat(64),
            status: WebApplicationStatus {
                deletion_cleanup_receipt: Some(oxideloop_controller_api::DeletionCleanupReceipt {
                    root_uid: "root-uid".into(),
                    deletion_id: "d".repeat(64),
                    deletion_start_generation: 2,
                    state_name: linked_state_name,
                    state_uid: "deleted-state-uid".into(),
                    cleanup_token: "e".repeat(64),
                }),
                ..WebApplicationStatus::default()
            },
            state_name: String::new(),
            state_uid: String::new(),
            state_resource_version: String::new(),
            root_resource_version: "root-rv-3".into(),
            root_finalizer_present,
            state_finalizer_present: false,
            deleting: true,
        }
    }

    #[test]
    fn runtime_root_only_recovery_recreates_without_bootstrap_authority() {
        for _ in 0..2 {
            let machine =
                recover_deleting_root_without_state(root_only_deletion_snapshot(true)).unwrap();
            let mut actor = RootActorV0::new(machine);
            actor.enqueue(EventV0::Start);
            assert!(matches!(
                actor.next_command(),
                Ok(Some(CommandV0::VerifyOwnedChildrenAbsent {
                    preconditions,
                    ..
                })) if preconditions.state_uid == "deleted-state-uid"
                    && preconditions.state_resource_version.is_empty()
            ));
        }

        let machine =
            recover_deleting_root_without_state(root_only_deletion_snapshot(false)).unwrap();
        let mut actor = RootActorV0::new(machine);
        actor.enqueue(EventV0::Start);
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::VerifyRootUidAbsent { root_uid, .. }))
                if root_uid == "root-uid"
        ));

        let mut nondeleting = root_only_deletion_snapshot(true);
        nondeleting.deleting = false;
        assert!(matches!(
            recover_deleting_root_without_state(nondeleting),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }

    #[test]
    fn root_only_root_finalizer_watch_before_ack_still_proves_original_uid_absent() {
        let machine =
            recover_deleting_root_without_state(root_only_deletion_snapshot(true)).unwrap();
        let mut actor = RootActorV0::new(machine);
        actor.enqueue(EventV0::Start);
        let (verify_token, verify_pins) = match actor.next_command().unwrap().unwrap() {
            CommandV0::VerifyOwnedChildrenAbsent {
                token,
                preconditions,
            } => (token, preconditions),
            command => panic!("unexpected command: {command:?}"),
        };
        actor
            .acknowledge(EventV0::OwnedChildrenVerifiedAbsent {
                token: verify_token.clone(),
                root_uid: verify_pins.root_uid,
                root_generation: verify_pins.root_generation,
                root_resource_version: verify_pins.root_resource_version,
                state_uid: verify_pins.state_uid,
                state_resource_version: verify_pins.state_resource_version,
                cleanup_token: "e".repeat(64),
            })
            .unwrap();
        let (finalizer_token, old_root_rv) = match actor.next_command().unwrap().unwrap() {
            CommandV0::RemoveRootFinalizer {
                token,
                preconditions,
            } => (token, preconditions.root_resource_version),
            command => panic!("unexpected command: {command:?}"),
        };
        let mut watched = actor.state().root().clone();
        watched.root_finalizer_present = false;
        watched.root_resource_version = "root-rv-4".into();
        assert!(actor.root_observed(watched));
        actor
            .acknowledge(EventV0::RootFinalizerRemoved {
                token: finalizer_token,
                root_uid: "root-uid".into(),
                old_root_resource_version: old_root_rv,
                new_root_resource_version: "root-rv-4".into(),
            })
            .unwrap();
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::VerifyRootUidAbsent { root_uid, .. }))
                if root_uid == "root-uid"
        ));
        assert!(matches!(actor.mode(), ModeV0::Running));
    }

    #[test]
    fn state_backed_root_finalizer_watch_before_ack_still_proves_original_uid_absent() {
        let (machine, ack) = state_backed_root_finalizer_pending();
        let mut actor = RootActorV0::new(machine);
        let mut watched = actor.state().root().clone();
        watched.root_finalizer_present = false;
        watched.root_resource_version = "root-rv-3".into();
        assert!(actor.root_observed(watched));
        actor.priority_ack = Some(ack);
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::VerifyRootUidAbsent { root_uid, .. }))
                if root_uid == "root-uid"
        ));
        assert!(matches!(actor.mode(), ModeV0::Running));
    }

    #[derive(Default)]
    struct SequencingDispatcher {
        calls: Vec<CommandV0>,
    }

    #[async_trait]
    impl KubernetesDispatcherV0 for SequencingDispatcher {
        async fn dispatch(
            &mut self,
            command: &CommandV0,
        ) -> Result<Option<EventV0>, RuntimeErrorV0> {
            self.calls.push(command.clone());
            if self.calls.len() == 1 {
                return Ok(None);
            }
            let CommandV0::EnsureBootstrap {
                token,
                root_resource_version,
                state_name,
                ..
            } = command
            else {
                return Err(RuntimeErrorV0::Hold(
                    "sequencing fake received a different command".into(),
                ));
            };
            Ok(Some(EventV0::BootstrapAcknowledged {
                token: token.clone(),
                state_name: state_name.clone(),
                state_uid: "state-uid".into(),
                state_resource_version: "state-rv-1".into(),
                old_root_resource_version: root_resource_version.clone(),
                new_root_resource_version: "root-rv-2".into(),
                status_state_link: oxideloop_controller_api::StateLink {
                    root_uid: "root-uid".into(),
                    name: state_name.clone(),
                    uid: "state-uid".into(),
                },
                root_finalizer_present: true,
                state_finalizer_present: true,
            }))
        }

        async fn fresh_root_snapshot(&mut self) -> Result<RootSnapshotV0, RuntimeErrorV0> {
            Err(RuntimeErrorV0::Hold(
                "sequencing fake does not provide stale recovery".into(),
            ))
        }
    }

    #[tokio::test]
    async fn sequencing_retries_the_exact_owned_command_without_actor_reentry() {
        let mut actor = actor();
        actor.enqueue(EventV0::Start);
        let mut dispatcher = SequencingDispatcher::default();
        let external = drive_kubernetes_with_dispatcher(&mut actor, &mut dispatcher)
            .await
            .unwrap();
        assert_eq!(dispatcher.calls.len(), 2);
        assert_eq!(dispatcher.calls[0], dispatcher.calls[1]);
        assert!(matches!(
            external,
            Some(CommandV0::RequestDescriptor { .. })
        ));
    }

    #[derive(Default)]
    struct NeverSequencedDispatcher {
        calls: usize,
    }

    #[async_trait]
    impl KubernetesDispatcherV0 for NeverSequencedDispatcher {
        async fn dispatch(
            &mut self,
            _command: &CommandV0,
        ) -> Result<Option<EventV0>, RuntimeErrorV0> {
            self.calls += 1;
            Ok(None)
        }

        async fn fresh_root_snapshot(&mut self) -> Result<RootSnapshotV0, RuntimeErrorV0> {
            Err(RuntimeErrorV0::Hold("unexpected fresh read".into()))
        }
    }

    #[tokio::test]
    async fn sequencing_is_bounded_and_holds_the_actor() {
        let mut actor = actor();
        actor.enqueue(EventV0::Start);
        let mut dispatcher = NeverSequencedDispatcher::default();
        assert!(matches!(
            drive_kubernetes_with_dispatcher(&mut actor, &mut dispatcher).await,
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert_eq!(dispatcher.calls, usize::from(MAX_SEQUENCING_RETRIES) + 1);
        assert!(matches!(actor.mode(), ModeV0::Hold(_)));
    }

    #[test]
    fn public_mutation_dispatch_rejects_batches() {
        let mutation = oxideloop_controller_core::MutationCommandV0 {
            command_id: "c".repeat(64),
            attempt_token: "a".repeat(64),
            preconditions: oxideloop_controller_core::PreconditionsV0 {
                root_uid: "root-uid".into(),
                root_generation: 1,
                root_resource_version: "root-rv".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "state-rv".into(),
                observation_token: String::new(),
                cache_epoch: String::new(),
            },
            identity: oxideloop_controller_api::ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "managed".into(),
            },
            logical_id: "managed".into(),
            decision: oxideloop_mutation_model::MutationDecision::Post {
                method: oxideloop_mutation_model::HttpMethod::Post,
                body: serde_json::json!({}),
            },
        };
        let batch = CommandV0::Mutate {
            transaction_id: "t".repeat(64),
            attempt_token: "a".repeat(64),
            commands: vec![mutation.clone(), mutation],
        };
        assert!(matches!(
            validate_transport_command_shape(&batch),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }
    #[test]
    fn watch_invalidation_preserves_durable_effect_ownership() {
        let mut actor = actor();
        actor.enqueue(EventV0::Start);
        actor.watch_invalidated();
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::EnsureBootstrap { .. }))
        ));
        assert!(matches!(
            actor.next_command(),
            Err(RuntimeErrorV0::InFlight)
        ));
    }

    #[test]
    fn acknowledge_rejects_non_ack_events_and_never_releases_a_render_path() {
        let mut reconcile_in_flight = actor();
        reconcile_in_flight.start();
        assert!(matches!(
            reconcile_in_flight.next_command(),
            Ok(Some(CommandV0::EnsureBootstrap { .. }))
        ));
        assert!(matches!(
            reconcile_in_flight.acknowledge(EventV0::ReconcileRequested {
                cause: InternalReconcileCauseV0::DependencyWatch,
                observation_token: "fabricated-token".into(),
                cache_epoch: "99".into(),
            }),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert!(matches!(reconcile_in_flight.mode(), ModeV0::Hold(_)));
        assert!(matches!(
            reconcile_in_flight.next_command(),
            Err(RuntimeErrorV0::Hold(_))
        ));

        let mut refresh_in_flight = actor();
        let root = refresh_in_flight.state().root().clone();
        refresh_in_flight.start();
        assert!(refresh_in_flight.next_command().unwrap().is_some());
        assert!(matches!(
            refresh_in_flight.acknowledge(EventV0::RootRefreshed { root }),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert!(matches!(refresh_in_flight.mode(), ModeV0::Hold(_)));

        let mut no_in_flight = actor();
        assert!(matches!(
            no_in_flight.acknowledge(EventV0::ReconcileRequested {
                cause: InternalReconcileCauseV0::DependencyWatch,
                observation_token: "fabricated-token".into(),
                cache_epoch: "99".into(),
            }),
            Err(RuntimeErrorV0::Hold(_))
        ));
        assert!(matches!(no_in_flight.mode(), ModeV0::Hold(_)));

        for raw_render in [
            EventV0::Rendered {
                frozen: FrozenInputV0 {
                    root_generation: 1,
                    state_resource_version: "state-rv".into(),
                    observation_token: "forged".into(),
                    cache_epoch: "0".into(),
                    final_observation_fence_token: "f".repeat(64),
                    render_input_hash: "0".repeat(64),
                },
                render_input_json: "{}".into(),
                render_output_json: "{}".into(),
                observed: vec![],
            },
            EventV0::Revalidated {
                frozen: FrozenInputV0 {
                    root_generation: 1,
                    state_resource_version: "state-rv".into(),
                    observation_token: "forged".into(),
                    cache_epoch: "0".into(),
                    final_observation_fence_token: "f".repeat(64),
                    render_input_hash: "0".repeat(64),
                },
                render_input_json: "{}".into(),
                render_output_json: "{}".into(),
                observed: vec![],
            },
        ] {
            let mut actor = actor();
            assert!(matches!(
                actor.acknowledge(raw_render),
                Err(RuntimeErrorV0::Hold(_))
            ));
            assert!(matches!(actor.mode(), ModeV0::Hold(_)));
        }
    }

    #[test]
    fn deletion_revalidation_requires_a_sealed_owned_observation_capability() {
        fn deletion_request_actor() -> (RootActorV0, PreconditionsV0) {
            let mut actor = RootActorV0::new(deleting_machine(vec![], vec![]));
            let root = actor.state().root().clone();
            let preconditions = PreconditionsV0 {
                root_uid: root.uid,
                root_generation: root.generation,
                root_resource_version: root.root_resource_version,
                state_uid: root.state_uid,
                state_resource_version: root.state_resource_version,
                observation_token: "deletion-observation".into(),
                cache_epoch: "0".into(),
            };
            actor.in_flight = true;
            actor.owned_in_flight = Some(CommandV0::RequestDeletionObservation {
                preconditions: preconditions.clone(),
            });
            (actor, preconditions)
        }

        let (mut raw_actor, _) = deletion_request_actor();
        assert!(matches!(
            raw_actor.acknowledge(EventV0::DeletionRevalidated {
                observed_inventory: vec![],
                observed_automation_runs: vec![],
            }),
            Err(RuntimeErrorV0::Hold(_))
        ));

        let (mut exact_actor, preconditions) = deletion_request_actor();
        let observation = exact_actor
            .validated_deletion_observation(
                preconditions.clone(),
                deletion_observation_request_token(&preconditions),
                vec![],
                vec![],
            )
            .expect("exact owned deletion pins mint the sealed observation");
        exact_actor
            .acknowledge_deletion_observation(observation)
            .expect("sealed deletion observation acknowledges the owned command");
        assert!(!exact_actor.in_flight);

        let (source_actor, preconditions) = deletion_request_actor();
        let observation = source_actor
            .validated_deletion_observation(
                preconditions.clone(),
                deletion_observation_request_token(&preconditions),
                vec![],
                vec![],
            )
            .expect("exact pins mint before the stale transition");
        let stale_machine = deleting_machine(vec![], vec![]);
        let mut stale_root = stale_machine.root().clone();
        stale_root.root_resource_version = "stale-root-rv".into();
        let mut stale_actor = RootActorV0::new(
            MachineStateV0::recover(stale_root, stale_machine.journal().clone())
                .expect("stale-root test machine remains durable-valid"),
        );
        stale_actor.in_flight = true;
        stale_actor.owned_in_flight = Some(CommandV0::RequestDeletionObservation { preconditions });
        assert!(matches!(
            stale_actor.acknowledge_deletion_observation(observation),
            Err(RuntimeErrorV0::Hold(_))
        ));
    }

    #[test]
    fn stale_readiness_does_not_enqueue_an_observation_event() {
        let mut actor = actor();
        let current = actor.begin_observation_barrier();
        actor.informers_ready(BarrierV0 {
            cache_epoch: current.cache_epoch.saturating_add(1),
            generation: current.generation,
        });
        assert!(actor.next_command().unwrap().is_none());
        actor.informers_ready(current);
        // Bootstrap has not completed, so the event is safely reduced as stale
        // and cannot manufacture a render command.
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::IgnoredStale { .. }))
        ));
    }

    #[test]
    fn sealed_same_barrier_readiness_still_issues_the_current_render() {
        let mut actor = informers_pending_actor();
        let barrier = actor.barrier();
        let root = actor.state().root().clone();

        actor.informers_ready(barrier);

        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::RequestRender { ref preconditions }))
                if preconditions.root_uid == root.uid
                    && preconditions.root_generation == root.generation
                    && preconditions.root_resource_version == root.root_resource_version
                    && preconditions.state_uid == root.state_uid
                    && preconditions.state_resource_version == root.state_resource_version
        ));
    }

    #[test]
    fn root_refresh_drops_queued_readiness_before_it_can_render_old_pins() {
        let mut actor = informers_pending_actor();
        let barrier_a = actor.barrier();
        let root_a = actor.state().root().clone();
        actor.informers_ready(barrier_a);
        assert!(actor.pending_informer_readiness.is_some());

        let root_b = advanced_root(&actor.state, root_a.generation + 1, "readiness-race");
        assert!(actor.root_observed(root_b.clone()));
        assert!(actor.pending_informer_readiness.is_none());
        assert!(actor.observation_events.is_empty());
        assert!(actor.deferred_genuine_change.is_none());

        // Root authority is reduced before any observation-only queue. A
        // changed generation requires its durable invalidation; the old
        // readiness can never issue a RequestRender for A.
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::Persist { ref preconditions, .. }))
                if preconditions.root_uid == root_b.uid
                    && preconditions.root_generation == root_b.generation
                    && preconditions.root_resource_version == root_b.root_resource_version
                    && preconditions.state_uid == root_b.state_uid
                    && preconditions.state_resource_version == root_b.state_resource_version
        ));
        assert_ne!(actor.state().root().generation, root_a.generation);
        assert_eq!(actor.state().root().generation, root_b.generation);
    }

    #[test]
    fn root_refresh_after_a_completed_render_future_releases_only_that_no_effect_slot() {
        let mut actor = informers_pending_actor();
        let barrier = actor.barrier();
        actor.informers_ready(barrier);
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::RequestRender { .. }))
        ));
        assert!(actor.owns_render_request());

        let current = actor.state().root().clone();
        let updated = advanced_root(&actor.state, current.generation + 1, "render-race");
        assert!(actor.root_observed(updated.clone()));
        assert!(
            !actor.owns_render_request(),
            "the old render result cannot remain acknowledgement-eligible after root closure"
        );
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::Persist { ref preconditions, .. }))
                if preconditions.root_uid == updated.uid
                    && preconditions.root_generation == updated.generation
        ));
    }

    #[test]
    fn genuine_dependency_and_root_observations_advance_once() {
        let mut actor = actor();
        let initial = actor.barrier();
        let genuine = GenuineChangeV0::from_observations(
            &[oxideloop_renderer_abi::model::Observation {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "config".into(),
                uid: "uid-1".into(),
                resource_version: "rv-1".into(),
                value: serde_json::json!({"data":{"key":"value"}}),
            }],
            initial,
            1,
        )
        .unwrap();
        assert!(actor.record_genuine_dependency_change(&genuine));
        assert!(!actor.record_genuine_dependency_change(&genuine));
        assert_eq!(actor.barrier().generation, initial.generation + 1);
        // Bootstrap is not `Ready`, so retained authority is sealed for later
        // delivery instead of manufacturing a token-only event or cancelling
        // a non-observation command.
        assert!(actor.observation_events.is_empty());
        assert!(actor.deferred_genuine_change.is_some());

        let root = actor.state().root().clone();
        assert!(actor.root_observed(root.clone()));
        assert!(!actor.root_observed(root));
        assert_eq!(actor.barrier().generation, initial.generation + 2);
        assert!(actor.observation_events.is_empty());
        assert!(actor.pending_root_refresh.is_some());
        assert!(actor.deferred_genuine_change.is_none());
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn retained_changes_defer_behind_durable_work_then_complete_a_fresh_render() {
        let (machine, automation_ack, owned_automation) = automation_ensure_pending_command();
        let mut actor = RootActorV0::new(machine);
        actor.commands.push_back(owned_automation);
        let owned = actor
            .next_command()
            .expect("automation command is issued")
            .expect("automation command exists");
        assert!(matches!(owned, CommandV0::EnsureAutomationRun { .. }));

        let mut collections = BTreeMap::new();
        collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![dependency_object("rv-1")],
                resource_version: "list-rv-1".into(),
            },
        );
        let first_barrier = actor.barrier();
        let (first_sender, first_watch) = WatchHandleV0::test_channel(2);
        first_sender
            .try_send(Ok(WatchEventV0::Modified(dependency_object("rv-2"))))
            .expect("first genuine watch event is buffered");
        // The same object/RV is a duplicate delivery and must not advance the
        // actor a second time while the durable command remains owned.
        first_sender
            .try_send(Ok(WatchEventV0::Modified(dependency_object("rv-2"))))
            .expect("duplicate watch event is buffered");
        let mut first_port = KubeFixedPointPortV0::<()>::retained_for_test(
            first_barrier,
            ObservationBarrierV0 {
                generation: first_barrier,
                dependencies: vec![],
                collections,
                objects: vec![],
                watches: vec![first_watch],
            },
        );
        assert_eq!(
            first_port
                .coordinate_retained_watch(&mut actor)
                .await
                .expect("first retained change is authenticated"),
            RetainedWatchCoordinationV0::ReconcileScheduled
        );
        assert!(actor.in_flight, "durable command ownership is retained");
        assert!(matches!(
            actor.owned_in_flight,
            Some(CommandV0::EnsureAutomationRun { .. })
        ));
        assert!(!first_port.has_retained_observation_barrier());
        let first_deferred_token = actor
            .deferred_genuine_change
            .as_ref()
            .expect("genuine change is sealed for deferred delivery")
            .observation_token
            .clone();

        // A newer retained barrier delivers a distinct final snapshot before
        // the durable acknowledgement. It replaces, rather than loses, the
        // deferred authority and advances the global barrier exactly once.
        let second_barrier = actor.barrier();
        let (second_sender, second_watch) = WatchHandleV0::test_channel(1);
        second_sender
            .try_send(Ok(WatchEventV0::Modified(dependency_object("rv-3"))))
            .expect("newer genuine watch event is buffered");
        let mut second_collections = BTreeMap::new();
        second_collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![dependency_object("rv-2")],
                resource_version: "list-rv-2".into(),
            },
        );
        let mut second_port = KubeFixedPointPortV0::<()>::retained_for_test(
            second_barrier,
            ObservationBarrierV0 {
                generation: second_barrier,
                dependencies: vec![],
                collections: second_collections,
                objects: vec![],
                watches: vec![second_watch],
            },
        );
        assert_eq!(
            second_port
                .coordinate_retained_watch(&mut actor)
                .await
                .expect("newer retained change is authenticated"),
            RetainedWatchCoordinationV0::ReconcileScheduled
        );
        let latest_token = actor
            .deferred_genuine_change
            .as_ref()
            .expect("latest genuine change remains deferred")
            .observation_token
            .clone();
        assert_ne!(latest_token, first_deferred_token);
        assert_eq!(actor.barrier().generation, first_barrier.generation + 2);

        actor
            .acknowledge(automation_ack)
            .expect("exact durable acknowledgement remains accepted");
        let durable_persist = actor
            .next_command()
            .expect("durable acknowledgement reduces")
            .expect("durable acknowledgement is persisted");
        assert!(matches!(durable_persist, CommandV0::Persist { .. }));
        actor
            .acknowledge(persisted_event(&durable_persist, "state-rv-after-durable"))
            .expect("exact durable persistence acknowledgement is accepted");

        let CommandV0::RequestRender { preconditions } = actor
            .next_command()
            .expect("deferred reconciliation is delivered once Ready")
            .expect("deferred reconciliation requests a fresh render")
        else {
            panic!("durable completion did not issue the deferred render");
        };
        assert_eq!(preconditions.observation_token, latest_token);
        let input = deferred_render_input(&actor, &preconditions);
        let retry_barrier = actor.barrier();
        let mut retry_port = KubeFixedPointPortV0::fixed_point_for_test(
            DeferredStableRendererV0 {
                output: deferred_render_output(),
            },
            deferred_test_descriptor(),
            retry_barrier,
            ObservationBarrierV0 {
                generation: retry_barrier,
                dependencies: vec![],
                collections: BTreeMap::new(),
                objects: vec![],
                watches: vec![],
            },
        );
        let mut result = retry_port
            .reconcile_for_actor(&mut actor, input)
            .await
            .expect("fresh concrete fixed-point retry succeeds");
        let CompletedObservationFenceOutcomeV0::Completed(capability) = retry_port
            .complete_observation_fence(&mut actor, &mut result)
            .expect("fresh fixed point completes a full fence")
        else {
            panic!("fresh fixed point did not mint a sealed render capability");
        };
        actor
            .acknowledge_render_observation(*capability)
            .expect("sealed retry render acknowledgement is accepted");
        assert!(matches!(
            actor.next_command(),
            Ok(Some(
                CommandV0::Persist { .. } | CommandV0::PersistRootStatus { .. }
            ))
        ));
    }

    #[tokio::test]
    async fn retained_delete_defers_behind_durable_work_without_cancelling_ownership() {
        let (machine, automation_ack, owned_automation) = automation_ensure_pending_command();
        let mut actor = RootActorV0::new(machine);
        actor.commands.push_back(owned_automation);
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::EnsureAutomationRun { .. }))
        ));
        let barrier = actor.barrier();
        let (sender, watch) = WatchHandleV0::test_channel(1);
        sender
            .try_send(Ok(WatchEventV0::Deleted(dependency_object("rv-2"))))
            .expect("UID-correlated retained delete is buffered");
        let mut collections = BTreeMap::new();
        collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![dependency_object("rv-1")],
                resource_version: "list-rv-1".into(),
            },
        );
        let mut port = KubeFixedPointPortV0::<()>::retained_for_test(
            barrier,
            ObservationBarrierV0 {
                generation: barrier,
                dependencies: vec![],
                collections,
                objects: vec![],
                watches: vec![watch],
            },
        );
        assert_eq!(
            port.coordinate_retained_watch(&mut actor)
                .await
                .expect("retained delete is authenticated"),
            RetainedWatchCoordinationV0::ReconcileScheduled
        );
        assert!(actor.in_flight, "delete cannot cancel the durable command");
        let deferred_token = actor
            .deferred_genuine_change
            .as_ref()
            .expect("retained delete is deferred")
            .observation_token
            .clone();
        actor
            .acknowledge(automation_ack)
            .expect("exact durable acknowledgement remains accepted");
        let persist = actor
            .next_command()
            .expect("durable acknowledgement reduces")
            .expect("durable acknowledgement persists");
        actor
            .acknowledge(persisted_event(&persist, "state-rv-after-delete"))
            .expect("exact persistence acknowledgement remains accepted");
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::RequestRender { ref preconditions }))
                if preconditions.observation_token == deferred_token
        ));
    }

    #[test]
    fn core_events_reduce_before_a_queued_sibling_command() {
        let mut actor = actor();
        actor.commands.push_back(CommandV0::IgnoredStale {
            reason: "queued sibling".into(),
        });
        actor.enqueue(EventV0::Start);

        // Start is reduced first (it appends EnsureBootstrap after the
        // sibling); the transition's original command order remains intact.
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::IgnoredStale { .. }))
        ));
        actor.consume_internal();
        assert!(matches!(
            actor.next_command(),
            Ok(Some(CommandV0::EnsureBootstrap { .. }))
        ));
    }

    #[test]
    fn failpoint_arm_is_one_shot_and_instance_local() {
        let mut left = FailpointsV0::default();
        let mut right = FailpointsV0::default();
        left.arm(FailpointV0::AfterEventBeforeStatus).unwrap();
        assert!(left.take(FailpointV0::AfterEventBeforeStatus));
        assert!(!left.take(FailpointV0::AfterEventBeforeStatus));
        assert!(!right.take(FailpointV0::AfterEventBeforeStatus));
        right.arm(FailpointV0::AfterStatusBeforeAck).unwrap();
        assert!(right.arm(FailpointV0::AfterEventBeforeStatus).is_err());
        assert!(right.take(FailpointV0::AfterStatusBeforeAck));
        assert!(!right.take(FailpointV0::AfterEventBeforeStatus));
    }

    #[test]
    fn dependencies_are_sorted_and_deduplicated_before_discovery() {
        let mut other = dep();
        if let Dependency::Object { name, .. } = &mut other {
            *name = "another".into();
        }
        let normalized = normalize_dependencies(&[dep(), other.clone(), dep()]).unwrap();
        assert_eq!(normalized.len(), 2);
        let mut port = Port::default();
        establish_dependencies(&mut port, &[dep(), other, dep()]).unwrap();
        assert_eq!(
            port.calls,
            ["L:ConfigMap:ns", "W:ConfigMap:ns:7", "R:another", "R:gone"]
        );
    }

    #[test]
    fn first_prune_observation_requires_a_new_barrier() {
        let current = BarrierV0 {
            generation: 7,
            cache_epoch: 3,
        };
        assert_eq!(
            schedule_prune_confirmation(current, true, false),
            Some(BarrierScheduleV0::ImmediateFreshBarrier(current))
        );
        assert_eq!(schedule_prune_confirmation(current, true, true), None);
    }

    struct FixedPort {
        renders: usize,
        barriers: usize,
    }

    #[async_trait]
    impl FixedPointPortV0 for FixedPort {
        async fn render(
            &mut self,
            _input: &RenderInputV0,
        ) -> Result<RenderOutputV0, RuntimeErrorV0> {
            self.renders += 1;
            Ok(RenderOutputV0 {
                abi_version: "renderer.oxideloop.dev/v0".into(),
                disposition: RenderDisposition::ExplicitEmpty,
                resources: vec![],
                dependencies: vec![dep()],
                automations: vec![],
                diagnostics: vec![],
            })
        }

        async fn observe_after_barrier(
            &mut self,
            dependencies: &[Dependency],
        ) -> Result<Vec<oxideloop_renderer_abi::model::Observation>, RuntimeErrorV0> {
            assert_eq!(dependencies, [dep()]);
            self.barriers += 1;
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn fake_fixed_point_rerenders_only_after_a_fresh_barrier() {
        let mut port = FixedPort {
            renders: 0,
            barriers: 0,
        };
        let result = reconcile_fixed_point(
            &mut port,
            RenderInputV0 {
                abi_version: "renderer.oxideloop.dev/v0".into(),
                root: RootProjection {
                    api_version: "demo.oxideloop.dev/v1alpha1".into(),
                    kind: "WebApplication".into(),
                    namespace: "ns".into(),
                    name: "root".into(),
                    uid: "root-uid".into(),
                    generation: 1,
                    spec: serde_json::json!({}),
                },
                bundle_digest: "a".repeat(64),
                cache_epoch: "0".into(),
                observation_token: "token".into(),
                observations: vec![],
            },
        )
        .await
        .unwrap();
        assert_eq!(port.renders, 2);
        assert_eq!(port.barriers, 1);
        assert_eq!(result.dependencies(), [dep()]);
    }

    #[test]
    fn deterministic_request_id_decodes_lowercase_hex_bytes() {
        let hash = "0123456789abcdef".repeat(4);
        assert_eq!(
            deterministic_request_id(&hash).unwrap(),
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef
            ]
        );
        assert!(deterministic_request_id(&hash[..63]).is_err());
        assert!(deterministic_request_id(&hash.to_uppercase()).is_err());
    }

    struct InvalidatingPort {
        calls: usize,
        invalidations: usize,
        barrier: Option<BarrierV0>,
        error: TransportErrorV0,
    }
    #[async_trait]
    impl FixedPointPortV0 for InvalidatingPort {
        async fn render(&mut self, _: &RenderInputV0) -> Result<RenderOutputV0, RuntimeErrorV0> {
            self.calls += 1;
            if self.calls == 1 {
                return Err(RuntimeErrorV0::Discovery(self.error.clone()));
            }
            Ok(RenderOutputV0 {
                abi_version: "renderer.oxideloop.dev/v0".into(),
                disposition: RenderDisposition::ExplicitEmpty,
                resources: vec![],
                dependencies: vec![],
                automations: vec![],
                diagnostics: vec![],
            })
        }
        async fn observe_after_barrier(
            &mut self,
            _: &[Dependency],
        ) -> Result<Vec<oxideloop_renderer_abi::model::Observation>, RuntimeErrorV0> {
            Ok(vec![])
        }

        fn watch_invalidated(&mut self, barrier: BarrierV0) {
            self.invalidations += 1;
            self.barrier = Some(barrier);
        }
    }

    #[tokio::test]
    async fn watch_overflow_restarts_from_new_cache_epoch() {
        let mut actor = actor();
        let mut port = InvalidatingPort {
            calls: 0,
            invalidations: 0,
            barrier: None,
            error: TransportErrorV0::WatchOverflow,
        };
        let input = RenderInputV0 {
            abi_version: "renderer.oxideloop.dev/v0".into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: "ns".into(),
                name: "root".into(),
                uid: "root-uid".into(),
                generation: 1,
                spec: serde_json::json!({}),
            },
            bundle_digest: "a".repeat(64),
            cache_epoch: "0".into(),
            observation_token: "t".into(),
            observations: vec![],
        };
        let result = reconcile_with_watch_restarts(&mut actor, &mut port, input)
            .await
            .unwrap();
        assert_eq!(actor.barrier().cache_epoch, 1);
        assert_eq!(result.input().cache_epoch, "1");
        assert_ne!(result.input().observation_token, "t");
        assert_eq!(
            result.input().observation_token,
            observation_token_for_barrier(
                "t",
                BarrierV0 {
                    generation: 1,
                    cache_epoch: 1,
                }
            )
        );
        assert_eq!(port.invalidations, 1);
        assert_eq!(port.barrier, Some(actor.barrier()));
        assert_eq!(port.calls, 3); // failed pass, then fixed point's two stable renders
    }

    #[tokio::test]
    async fn watch_gone_restarts_with_a_fresh_epoch_and_token() {
        let mut actor = actor();
        let mut port = InvalidatingPort {
            calls: 0,
            invalidations: 0,
            barrier: None,
            error: TransportErrorV0::WatchGone,
        };
        let input = RenderInputV0 {
            abi_version: "renderer.oxideloop.dev/v0".into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: "ns".into(),
                name: "root".into(),
                uid: "root-uid".into(),
                generation: 1,
                spec: serde_json::json!({}),
            },
            bundle_digest: "a".repeat(64),
            cache_epoch: "0".into(),
            observation_token: "gone-token".into(),
            observations: vec![],
        };
        let result = reconcile_with_watch_restarts(&mut actor, &mut port, input)
            .await
            .unwrap();
        assert_eq!(
            actor.barrier(),
            BarrierV0 {
                generation: 1,
                cache_epoch: 1
            }
        );
        assert_eq!(result.input().cache_epoch, "1");
        assert_ne!(result.input().observation_token, "gone-token");
        assert_eq!(port.invalidations, 1);
        assert_eq!(port.barrier, Some(actor.barrier()));
    }

    #[test]
    fn observation_tokens_are_fresh_for_each_restarted_barrier() {
        let base = "initial";
        let first = observation_token_for_barrier(
            base,
            BarrierV0 {
                generation: 1,
                cache_epoch: 1,
            },
        );
        let second = observation_token_for_barrier(
            &first,
            BarrierV0 {
                generation: 2,
                cache_epoch: 2,
            },
        );
        assert_ne!(base, first);
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(second.len(), 64);
    }
}
