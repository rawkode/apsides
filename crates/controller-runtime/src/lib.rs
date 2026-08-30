//! P6 keyed, single-replica controller scheduling.
#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc, clippy::must_use_candidate)]

mod adapters;
mod engine;
pub use adapters::{
    KubeAutomationPortV0, KubeFixedPointPortV0, UnixRendererPortV0, desired_observation,
};
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
    WebApplicationSpec, WebApplicationStatus,
};
use oxideloop_controller_core::{
    CommandV0, EventV0, MachineStateV0, PhaseV0, RootSnapshotV0, reduce,
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
const MAX_SEQUENCING_RETRIES: u8 = 4;

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
        Some(BarrierScheduleV0::ImmediateFreshBarrier(BarrierV0 {
            generation: current.generation.saturating_add(1),
            cache_epoch: current.cache_epoch,
        }))
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

/// The only crash simulation points permitted by the reusable action driver.
/// This is deliberately instance owned: production wiring decides whether to
/// expose it, while tests can prove recovery without an environment variable
/// or a process-global switch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum FailpointV0 {
    AfterEventBeforeStatus,
    AfterStatusBeforeAck,
}

/// A one-shot, atomically consumed failpoint arm.  The state is small enough
/// to keep inside an engine instance; no socket server is part of P6.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FailpointsV0 {
    armed: Option<FailpointV0>,
}

impl FailpointsV0 {
    pub fn arm(&mut self, point: FailpointV0) -> Result<(), RuntimeErrorV0> {
        if self.armed.replace(point).is_some() {
            return Err(RuntimeErrorV0::Hold("a failpoint is already armed".into()));
        }
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
    barrier: BarrierV0,
    /// Reducer/effect acknowledgements are durable work. Observation events may
    /// be discarded when a watch cache is invalidated; these may not.
    priority_ack: Option<EventV0>,
    core_events: VecDeque<EventV0>,
    pending_root_refresh: Option<RootSnapshotV0>,
    observation_events: VecDeque<EventV0>,
    commands: VecDeque<CommandV0>,
    last_dependency_observation: Option<(String, String)>,
    last_root_observation: Option<RootObservationFingerprintV0>,
}
impl RootActorV0 {
    pub fn new(state: MachineStateV0) -> Self {
        Self {
            state,
            mode: ModeV0::Running,
            in_flight: false,
            barrier: BarrierV0 {
                generation: 0,
                cache_epoch: 0,
            },
            priority_ack: None,
            core_events: VecDeque::new(),
            pending_root_refresh: None,
            observation_events: VecDeque::new(),
            commands: VecDeque::new(),
            last_dependency_observation: None,
            last_root_observation: None,
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
    pub fn enqueue(&mut self, event: EventV0) {
        match event {
            EventV0::RootRefreshed { root } => self.pending_root_refresh = Some(root),
            event => self.core_events.push_back(event),
        }
    }
    pub fn enqueue_observation(&mut self, event: EventV0) {
        self.observation_events.push_back(event);
    }
    /// Records one genuine dependency barrier. Duplicate delivery of the same
    /// token/epoch pair is ignored and cannot satisfy a second prune barrier.
    pub fn dependency_observed(&mut self, observation_token: String, cache_epoch: String) -> bool {
        let fingerprint = (observation_token.clone(), cache_epoch.clone());
        if self.last_dependency_observation.as_ref() == Some(&fingerprint) {
            return false;
        }
        self.last_dependency_observation = Some(fingerprint);
        self.barrier.generation = self.barrier.generation.saturating_add(1);
        self.enqueue_observation(EventV0::ReconcileRequested {
            observation_token,
            cache_epoch,
        });
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
        self.enqueue(EventV0::RootRefreshed { root });
        true
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
        } else {
            let event = self
                .core_events
                .pop_front()
                .or_else(|| {
                    self.pending_root_refresh
                        .take()
                        .map(|root| EventV0::RootRefreshed { root })
                })
                .or_else(|| self.observation_events.pop_front());
            if let Some(event) = event {
                self.reduce_event(event)?;
            }
        }
        if self.state.phase() == PhaseV0::Terminal {
            self.terminal_root_deleted();
            return Ok(None);
        }
        if let Some(command) = self.commands.pop_front() {
            self.in_flight = true;
            Ok(Some(command))
        } else {
            Ok(None)
        }
    }
    /// Release the one in-flight slot only after the effect's core event is
    /// durable/available to the reducer.
    pub fn acknowledge(&mut self, event: EventV0) {
        self.in_flight = false;
        if self.priority_ack.replace(event).is_some() {
            self.hold("multiple acknowledgements attempted to occupy the priority slot".into());
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
        self.commands.clear();
        self.pending_root_refresh = Some(root);
    }
    /// `Hold` and `IgnoredStale` are local reducer commands. They are consumed
    /// by the driver and never manufactured as a transport acknowledgement.
    fn consume_internal(&mut self) {
        self.in_flight = false;
    }
    pub fn begin_observation_barrier(&mut self) -> BarrierV0 {
        self.barrier.generation += 1;
        self.barrier
    }
    pub fn informers_ready(&mut self, candidate: BarrierV0) {
        if candidate == self.barrier && matches!(self.mode, ModeV0::Running) {
            self.enqueue(EventV0::InformersReady);
        }
    }
    pub fn watch_invalidated(&mut self) {
        self.barrier.cache_epoch += 1;
        self.barrier.generation += 1;
        self.observation_events.clear();
        self.last_dependency_observation = None;
        // A 410 only invalidates observation state. In-flight durable effects,
        // their acknowledgements, and command ownership remain intact.
    }
    pub fn hold(&mut self, reason: String) {
        self.mode = ModeV0::Hold(reason);
        self.core_events.clear();
        self.priority_ack = None;
        self.pending_root_refresh = None;
        self.observation_events.clear();
        self.commands.clear();
        self.in_flight = false;
    }
    pub fn terminal_root_deleted(&mut self) {
        self.mode = ModeV0::Terminal;
        self.core_events.clear();
        self.priority_ack = None;
        self.pending_root_refresh = None;
        self.observation_events.clear();
        self.commands.clear();
        self.in_flight = false;
    }
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
        generation,
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
                actor.acknowledge(event);
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

#[derive(Clone, Debug, PartialEq)]
pub struct FixedPointResultV0 {
    pub input: RenderInputV0,
    pub output: RenderOutputV0,
    pub dependencies: Vec<Dependency>,
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
    use super::*;
    use oxideloop_controller_api::{
        ApplyOutcome, ApplyResult, AutomationPending, DeletionTransaction,
        GenerationAdvanceClosurePurpose, InventoryEntry, OxideLoopStateSpec, PersistPurpose,
        RootBinding, StateBody, StateLink, WebApplicationStatus, bootstrap_id,
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
    use oxideloop_renderer_abi::parse_validate_render_input;

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
            "name": "demo",
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
            name: "demo".into(),
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
                "spec": {
                    "message": "demo",
                    "expose": false,
                    "program": {
                        "configMapName": "bundle",
                        "key": "program.mjs",
                        "digest": root.bundle_digest,
                    }
                },
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
                generation: 1,
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
                generation: 7,
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
                name: "demo".into(),
                generation: 7,
                spec: serde_json::json!({"program":{"digest":"a".repeat(64)}}),
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
                name: "demo".into(),
                generation: 7,
                spec: serde_json::json!({"program":{"digest":"a".repeat(64)}}),
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

    fn automation_completion_pending() -> (MachineStateV0, EventV0) {
        let (machine, ensured) = automation_ensure_pending();
        let pending = reduce(machine, ensured).unwrap();
        let ready = acknowledge_persist(pending, "state-rv-5");
        let run = ready.state.journal().state.automation.pending[0].clone();
        (
            ready.state,
            EventV0::AutomationCompleted {
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
    ) -> (FrozenInputV0, String, String, Vec<DesiredObservationV0>) {
        let input_document = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                uid: root.uid.clone(),
                namespace: root.namespace.clone(),
                name: "demo".into(),
                generation: u64::try_from(root.generation).unwrap(),
                spec: serde_json::json!({"program":{"digest":root.bundle_digest}}),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: "1".into(),
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
                cache_epoch: "1".into(),
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
            actor.in_flight = true;
            if watch_first {
                assert!(actor.root_observed(root.clone()));
                actor.acknowledge(ack.clone());
            } else {
                actor.acknowledge(ack.clone());
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
            actor.in_flight = true;
            actor.acknowledge(ack);
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
        actor.acknowledge(EventV0::DescriptorReady {
            token: descriptor_token,
            bundle_digest: "a".repeat(64),
        });
        assert!(actor.next_command().unwrap().is_none());
        actor.enqueue(EventV0::InformersReady);
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let (first_frozen, first_input, output, observed) =
            prune_render_documents(actor.state().root(), "state-rv-1", "observation-1");
        actor.acknowledge(EventV0::Rendered {
            frozen: first_frozen.clone(),
            render_input_json: first_input.clone(),
            render_output_json: output.clone(),
            observed: observed.clone(),
        });
        let first_prepare = actor.next_command().unwrap().unwrap();
        actor.acknowledge(persisted_event(&first_prepare, "state-rv-2"));
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let mut first_revalidated_frozen = first_frozen;
        first_revalidated_frozen.state_resource_version = "state-rv-2".into();
        actor.acknowledge(EventV0::Revalidated {
            frozen: first_revalidated_frozen,
            render_input_json: first_input,
            render_output_json: output.clone(),
            observed: observed.clone(),
        });
        let first_completion = actor.next_command().unwrap().unwrap();
        actor.acknowledge(persisted_event(&first_completion, "state-rv-3"));
        assert!(actor.next_command().unwrap().is_none());

        // A genuinely new observation token supplies the second prune barrier.
        actor.enqueue(EventV0::ReconcileRequested {
            observation_token: "observation-2".into(),
            cache_epoch: "1".into(),
        });
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let (second_frozen, second_input, _, second_observed) =
            prune_render_documents(actor.state().root(), "state-rv-3", "observation-2");
        actor.acknowledge(EventV0::Rendered {
            frozen: second_frozen.clone(),
            render_input_json: second_input.clone(),
            render_output_json: output.clone(),
            observed: second_observed.clone(),
        });
        let status = actor.next_command().unwrap().unwrap();
        let (status_token, old_root_rv) = match &status {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            } => (token.clone(), old_root_resource_version.clone()),
            command => panic!("second barrier skipped root status durability: {command:?}"),
        };
        actor.acknowledge(EventV0::RootStatusPersisted {
            token: status_token,
            old_root_resource_version: old_root_rv,
            new_root_resource_version: "root-rv-2".into(),
            root_uid: "root-uid".into(),
        });
        assert!(matches!(
            actor.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        actor.acknowledge(EventV0::Rendered {
            frozen: second_frozen.clone(),
            render_input_json: second_input.clone(),
            render_output_json: output.clone(),
            observed: second_observed.clone(),
        });
        let delete_prepare = actor.next_command().unwrap().unwrap();
        actor.acknowledge(persisted_event(&delete_prepare, "state-rv-4"));
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
        recreated.acknowledge(EventV0::DescriptorReady {
            token: descriptor_token,
            bundle_digest: "a".repeat(64),
        });
        assert!(recreated.next_command().unwrap().is_none());
        recreated.enqueue(EventV0::InformersReady);
        assert!(matches!(
            recreated.next_command().unwrap(),
            Some(CommandV0::RequestRender { .. })
        ));
        let mut recovered_frozen = second_frozen;
        recovered_frozen.state_resource_version = "state-rv-4".into();
        recreated.acknowledge(EventV0::Revalidated {
            frozen: recovered_frozen,
            render_input_json: second_input,
            render_output_json: output,
            observed: second_observed,
        });
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
        actor.in_flight = true;
        actor.acknowledge(ack);
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
            generation: 3,
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
        actor.acknowledge(EventV0::OwnedChildrenVerifiedAbsent {
            token: verify_token.clone(),
            root_uid: verify_pins.root_uid,
            root_generation: verify_pins.root_generation,
            root_resource_version: verify_pins.root_resource_version,
            state_uid: verify_pins.state_uid,
            state_resource_version: verify_pins.state_resource_version,
            cleanup_token: verify_token,
        });
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
        actor.acknowledge(EventV0::RootFinalizerRemoved {
            token: finalizer_token,
            root_uid: "root-uid".into(),
            old_root_resource_version: old_root_rv,
            new_root_resource_version: "root-rv-4".into(),
        });
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
        actor.in_flight = true;
        let mut watched = actor.state().root().clone();
        watched.root_finalizer_present = false;
        watched.root_resource_version = "root-rv-3".into();
        assert!(actor.root_observed(watched));
        actor.acknowledge(ack);
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
    fn stale_readiness_does_not_enqueue_an_observation_event() {
        let mut actor = actor();
        let current = actor.begin_observation_barrier();
        actor.informers_ready(BarrierV0 {
            generation: current.generation - 1,
            ..current
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
    fn genuine_dependency_and_root_observations_advance_once() {
        let mut actor = actor();
        let initial = actor.barrier();
        assert!(actor.dependency_observed("dependency-1".into(), "epoch-1".into()));
        assert!(!actor.dependency_observed("dependency-1".into(), "epoch-1".into()));
        assert_eq!(actor.barrier().generation, initial.generation + 1);

        let root = actor.state().root().clone();
        assert!(actor.root_observed(root.clone()));
        assert!(!actor.root_observed(root));
        assert_eq!(actor.barrier().generation, initial.generation + 2);
        assert_eq!(actor.observation_events.len(), 1);
        assert!(actor.pending_root_refresh.is_some());
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
            Some(BarrierScheduleV0::ImmediateFreshBarrier(BarrierV0 {
                generation: 8,
                cache_epoch: 3,
            }))
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
        assert_eq!(result.dependencies, [dep()]);
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
        assert_eq!(result.input.cache_epoch, "1");
        assert_ne!(result.input.observation_token, "t");
        assert_eq!(
            result.input.observation_token,
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
        assert_eq!(result.input.cache_epoch, "1");
        assert_ne!(result.input.observation_token, "gone-token");
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
