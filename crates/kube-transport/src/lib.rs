//! The sole Kubernetes boundary for `OxideLoop`.
//!
//! Public callers deal exclusively in the DTOs and ports in this module.  The
//! Kubernetes client and its resource types are deliberately private, so a
//! controller cannot smuggle an arbitrary API operation through this crate.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc)]
#![allow(
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::unnecessary_wraps
)]

use std::collections::BTreeMap;
#[cfg(any(test, feature = "test-support"))]
use std::collections::VecDeque;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures_util::StreamExt;
use oxideloop_controller_api::{
    ApplyOutcome, ApplyResult, AutomationPending, AutomationRunCompletionState, AutomationRunSpec,
    AutomationRunStatus, DeletionCleanupReceipt, MAX_MESSAGE_BYTES, ObjectIdentity,
    OxideLoopStateSpec, ROOT_FINALIZER, STATE_FINALIZER, StateLink, WebApplicationStatus,
    validate_automation_run_name,
};
use oxideloop_controller_core::{
    CommandV0, MutationCommandV0, PreconditionsV0, root_finalizer_command_token,
    root_only_owned_children_verify_token,
};
use oxideloop_event_planner::{PlannedEventV0, materialize_event, semantically_equal};
use oxideloop_mutation_model::{HttpMethod, MutationDecision};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use kube::{
    Api, Client, Config,
    api::{DeleteParams, ListParams, Patch, PatchParams, Preconditions},
    core::{ApiResource, DynamicObject},
};

pub const ROOT_UID_ANNOTATION: &str = "runtime.oxideloop.dev/root-uid";
pub const TRANSACTION_ID_ANNOTATION: &str = "runtime.oxideloop.dev/transaction-id";
pub const COMMAND_ID_ANNOTATION: &str = "runtime.oxideloop.dev/command-id";
const MAX_BUNDLE_IDENTITY_BYTES: usize = 253;

/// The only namespace-scoped resources this transport will address.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ResourceKindV0 {
    WebApplication,
    OxideLoopState,
    AutomationRun,
    ConfigMap,
    /// v0 supports only `ClusterIP` Services (including headless `clusterIP:
    /// None`); external and node-port modes are rejected before any request.
    Service,
    /// Exactly `events.k8s.io/v1` Events. Event update, patch, and delete are
    /// intentionally unrepresentable in this transport.
    Event,
}

/// A sealed Kubernetes write shape. This is deliberately descriptive rather
/// than a generic HTTP method so controller recovery can remain command-local.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOperationV0 {
    Create,
    Apply,
    ApplyStatus,
    MergeStatus,
    JsonPatch,
    Delete,
}

/// A sealed read shape whose conflict must retain its request provenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadOperationV0 {
    List,
    Get,
}

/// The two points at which a Kubernetes watch may fail terminally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchStageV0 {
    Open,
    Stream,
}

/// The provenance of an API conflict. A terminal watch conflict must never be
/// confused with a write that raced after an exact fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictProvenanceV0 {
    Read {
        operation: ReadOperationV0,
        resource: ResourceKindV0,
    },
    WatchTerminal {
        stage: WatchStageV0,
        resource: ResourceKindV0,
    },
    Write {
        operation: WriteOperationV0,
        resource: ResourceKindV0,
    },
}

/// Exact namespace-scoped RBAC verbs required by the sealed dispatcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequiredRbacV0 {
    pub resource: ResourceKindV0,
    /// An exact Kubernetes subresource. This is inventory-only: it is never
    /// accepted by the generic resource/addressing APIs.
    pub subresource: Option<&'static str>,
    pub verbs: &'static [&'static str],
}
/// Returns the complete, read-only P8B RBAC inventory. No event or update verb is required.
#[must_use]
pub const fn required_rbac_v0() -> &'static [RequiredRbacV0] {
    &[
        RequiredRbacV0 {
            resource: ResourceKindV0::WebApplication,
            subresource: None,
            verbs: &["get", "list", "watch", "patch"],
        },
        RequiredRbacV0 {
            resource: ResourceKindV0::WebApplication,
            subresource: Some("status"),
            verbs: &["patch"],
        },
        RequiredRbacV0 {
            resource: ResourceKindV0::OxideLoopState,
            subresource: None,
            verbs: &["get", "list", "watch", "create", "patch"],
        },
        RequiredRbacV0 {
            resource: ResourceKindV0::AutomationRun,
            subresource: None,
            verbs: &["get", "list", "watch", "create", "delete"],
        },
        RequiredRbacV0 {
            resource: ResourceKindV0::AutomationRun,
            subresource: Some("status"),
            verbs: &["patch"],
        },
        RequiredRbacV0 {
            resource: ResourceKindV0::ConfigMap,
            subresource: None,
            verbs: &["get", "list", "watch", "create", "patch", "delete"],
        },
        RequiredRbacV0 {
            resource: ResourceKindV0::Service,
            subresource: None,
            verbs: &["get", "list", "watch", "create", "patch", "delete"],
        },
        RequiredRbacV0 {
            resource: ResourceKindV0::Event,
            subresource: None,
            verbs: &["get", "create"],
        },
    ]
}

impl ResourceKindV0 {
    #[must_use]
    pub const fn api_version(self) -> &'static str {
        match self {
            Self::WebApplication => "demo.oxideloop.dev/v1alpha1",
            Self::OxideLoopState | Self::AutomationRun => "runtime.oxideloop.dev/v1alpha1",
            Self::ConfigMap | Self::Service => "v1",
            Self::Event => "events.k8s.io/v1",
        }
    }

    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::WebApplication => "WebApplication",
            Self::OxideLoopState => "OxideLoopState",
            Self::AutomationRun => "AutomationRun",
            Self::ConfigMap => "ConfigMap",
            Self::Service => "Service",
            Self::Event => "Event",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectRefV0 {
    pub resource: ResourceKindV0,
    pub namespace: String,
    pub name: String,
}

/// P6-owned addressing context. It is validated here before use and supplies
/// names that are intentionally not duplicated in every `CommandV0` variant.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootAddressV0 {
    pub namespace: String,
    pub name: String,
    pub state_name: String,
}

impl RootAddressV0 {
    /// Returns the exact namespace-scoped root and journal addresses.
    #[must_use]
    pub fn references(&self) -> (ObjectRefV0, ObjectRefV0) {
        (
            ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: self.namespace.clone(),
                name: self.name.clone(),
            },
            ObjectRefV0 {
                resource: ResourceKindV0::OxideLoopState,
                namespace: self.namespace.clone(),
                name: self.state_name.clone(),
            },
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectMetaV0 {
    pub uid: String,
    pub resource_version: String,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub owner_uids: Vec<String>,
}

/// Exact bytes and Kubernetes identity returned by the sealed bundle reader.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigMapBinaryDataV0 {
    pub bytes: Vec<u8>,
    pub uid: String,
    pub resource_version: String,
}

/// The sole Event materialization evidence that reaches controller runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventMaterializationRefV0 {
    pub name: String,
    pub uid: String,
    pub resource_version: String,
}

/// Exact durable acknowledgement for one `AutomationRun` status completion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationCompletionAckV0 {
    pub uid: String,
    pub old_resource_version: String,
    pub new_resource_version: String,
    pub attempts: u16,
    pub materialization_name: String,
    pub materialization_uid: String,
    pub completion: AutomationRunCompletionState,
}

/// Durable identity pins for exactly one pending `AutomationRun` occurrence.
///
/// The caller must construct this from the journal record which issued the
/// run, never from an informer object.  A same-name object which fails any of
/// these pins is an authentication failure, not a candidate for adoption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomationRunExpectationV0 {
    root: ObjectRefV0,
    root_uid: String,
    occurrence_id: String,
    run_name: String,
    run_uid: String,
    run_resource_version: String,
    spec_hash: String,
}

impl AutomationRunExpectationV0 {
    /// Builds the exact expectation only from a durable pending journal item
    /// and the already-authenticated root identity.
    pub fn from_pending(
        root: ObjectRefV0,
        root_uid: impl Into<String>,
        pending: &AutomationPending,
    ) -> Result<Self, TransportErrorV0> {
        let expectation = Self {
            root,
            root_uid: root_uid.into(),
            occurrence_id: pending.occurrence_id.clone(),
            run_name: pending.run_name.clone(),
            run_uid: pending.run_uid.clone(),
            run_resource_version: pending.run_resource_version.clone(),
            spec_hash: pending.spec_hash.clone(),
        };
        validate_automation_expectation(&expectation)?;
        Ok(expectation)
    }
}

/// A run which has passed the one P6 authentication gate for list, watch and
/// exact GET observations.  Its fields are private so a caller cannot forge a
/// validated run without going through [`validate_automation_run_observation`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedAutomationRunV0 {
    reference: ObjectRefV0,
    uid: String,
    pending_resource_version: String,
    resource_version: String,
    spec: AutomationRunSpec,
    status: AutomationRunStatus,
}

impl ValidatedAutomationRunV0 {
    #[must_use]
    pub fn reference(&self) -> &ObjectRefV0 {
        &self.reference
    }

    #[must_use]
    pub fn uid(&self) -> &str {
        &self.uid
    }

    #[must_use]
    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }

    /// The durable pending RV captured when the run was ensured.  This is the
    /// only valid old RV for a completion acknowledgement; current and old
    /// Kubernetes RVs must never be ordered lexically.
    #[must_use]
    pub fn pending_resource_version(&self) -> &str {
        &self.pending_resource_version
    }

    #[must_use]
    pub fn spec(&self) -> &AutomationRunSpec {
        &self.spec
    }

    /// Returns the immutable historical program authority.  Incomplete replay
    /// must use this value, not a newer root program reference.
    #[must_use]
    pub fn program(&self) -> &oxideloop_controller_api::ProgramBundleReference {
        &self.spec.program
    }

    #[must_use]
    pub fn status(&self) -> &AutomationRunStatus {
        &self.status
    }

    #[must_use]
    pub fn is_pristine(&self) -> bool {
        self.status == AutomationRunStatus::default()
    }

    /// Completion data is available only after the validator has checked the
    /// complete status/materialization shape and the completion RV differs
    /// from the durable pending RV.
    #[must_use]
    pub fn completion(&self) -> Option<ValidatedAutomationCompletionV0> {
        let completion = self.status.completion.as_ref()?;
        Some(ValidatedAutomationCompletionV0 {
            uid: self.uid.clone(),
            old_resource_version: self.pending_resource_version.clone(),
            new_resource_version: self.resource_version.clone(),
            attempts: self.status.attempts,
            materialization_name: self.status.materialization_name.clone()?,
            materialization_uid: self.status.materialization_uid.clone()?,
            completion: completion.state,
        })
    }
}

/// Authenticated completion facts.  The old resourceVersion deliberately is
/// not carried by Kubernetes: callers must use the durable pending pin from
/// their `AutomationRunExpectationV0`, never infer an ordering between RVs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedAutomationCompletionV0 {
    uid: String,
    old_resource_version: String,
    new_resource_version: String,
    attempts: u16,
    materialization_name: String,
    materialization_uid: String,
    completion: AutomationRunCompletionState,
}

impl ValidatedAutomationCompletionV0 {
    #[must_use]
    pub fn uid(&self) -> &str {
        &self.uid
    }

    #[must_use]
    pub fn new_resource_version(&self) -> &str {
        &self.new_resource_version
    }

    #[must_use]
    pub fn old_resource_version(&self) -> &str {
        &self.old_resource_version
    }

    #[must_use]
    pub const fn attempts(&self) -> u16 {
        self.attempts
    }

    #[must_use]
    pub fn materialization_name(&self) -> &str {
        &self.materialization_name
    }

    #[must_use]
    pub fn materialization_uid(&self) -> &str {
        &self.materialization_uid
    }

    #[must_use]
    pub const fn completion(&self) -> AutomationRunCompletionState {
        self.completion
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectV0 {
    pub reference: ObjectRefV0,
    pub metadata: ObjectMetaV0,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListV0 {
    pub items: Vec<ObjectV0>,
    /// Present even for an empty list; it is the exact collection RV.
    pub resource_version: String,
}

/// A deletion-only list/reread snapshot. It is deliberately not a generic
/// discovery result: root and state pins were checked immediately before the
/// lists, and every retained child is exact-re-read before it is returned.
#[derive(Clone, Debug, PartialEq)]
pub struct DeletionObservationSnapshotV0 {
    pub root: ObjectV0,
    pub state: ObjectV0,
    pub inventory: Vec<ObjectV0>,
    pub automation_runs: Vec<ObjectV0>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum WatchEventV0 {
    Added(ObjectV0),
    Modified(ObjectV0),
    Deleted(ObjectV0),
    Bookmark { resource_version: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum TransportErrorV0 {
    #[error("unsupported resource, namespace, or v0 Service mode")]
    UnsupportedTarget,
    #[error("metadata may not be smuggled through a desired object")]
    MetadataSmuggling,
    #[error("object identity differs from the command identity")]
    IdentityMismatch,
    #[error("precondition is stale: {0}")]
    Stale(&'static str),
    #[error("existing object is foreign or lacks the exact recovery proof")]
    ForeignCollision,
    #[error("the API response was lost; live recovery proof is absent")]
    ResponseLost,
    #[error("bundle ConfigMap binaryData is invalid")]
    InvalidBundleData,
    #[error("delete/finalizer response is ambiguous and requires manual recovery")]
    ManualRecoveryHold,
    #[error("watch resource version expired (410); invalidate the cache epoch")]
    WatchGone,
    #[error("watch buffer overflow; invalidate the cache epoch")]
    WatchOverflow,
    #[error("AutomationRun observation cannot authenticate the durable pending occurrence: {0}")]
    AutomationObservationHold(String),
    #[error("Kubernetes conflict (409) at {0:?}")]
    Conflict(ConflictProvenanceV0),
    #[error("Kubernetes conflict cannot be safely rebased at {0:?}")]
    NonRebasableConflict(ConflictProvenanceV0),
    #[error("transport failure: {0}")]
    Other(String),
}

/// The sealed transport acknowledgement vocabulary.  It deliberately carries
/// core events rather than HTTP/Kubernetes responses, so a caller cannot use
/// this boundary as an escape hatch for arbitrary API operations.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandOutcomeV0 {
    Event(Box<oxideloop_controller_core::EventV0>),
    NotKubernetesEffect,
    /// `Mutate` is an ordered transaction.  The driver must submit exactly one
    /// mutation command per call and persist the acknowledgement before asking
    /// for the next one.
    NeedsSequencing {
        remaining: usize,
    },
    Hold {
        reason: String,
    },
}

/// Fixed-point discovery port. P6 must list, start at this exact RV, and
/// re-read before treating informer readiness as valid for a barrier epoch.
pub trait DiscoveryPortV0 {
    fn list_exact(
        &mut self,
        resource: ResourceKindV0,
        namespace: &str,
    ) -> Result<ListV0, TransportErrorV0>;
    fn watch_from(
        &mut self,
        resource: ResourceKindV0,
        namespace: &str,
        resource_version: &str,
    ) -> Result<Vec<WatchEventV0>, TransportErrorV0>;
    fn reread(&mut self, reference: &ObjectRefV0) -> Result<Option<ObjectV0>, TransportErrorV0>;
}

/// The bounded write capability set. This intentionally has no generic
/// Kubernetes write, replace, arbitrary-GVK, or cluster-scoped method.
/// Concrete namespace-scoped Kubernetes client. Kubernetes types are kept
/// private to this module; callers receive only `OxideLoop` domain DTOs.
#[derive(Clone)]
pub struct KubeTransportV0 {
    client: Client,
    #[cfg(any(test, feature = "test-support"))]
    test_watches: Arc<Mutex<VecDeque<WatchHandleV0>>>,
}

/// Root-only recovery permits exactly two state-absence follow-up effects.
/// Their tokens are separate from the durable receipt proof and are checked
/// against fresh root/state pins before any child list or finalizer write.
#[derive(Clone, Copy)]
enum RootOnlyCleanupCommandV0 {
    VerifyOwnedChildrenAbsent,
    RemoveRootFinalizer,
}

/// Opaque watch stream with no Kubernetes stream or event in its public API.
pub struct WatchHandleV0 {
    events: tokio::sync::mpsc::Receiver<Result<WatchEventV0, TransportErrorV0>>,
    overflowed: Arc<AtomicBool>,
    terminal: Option<TransportErrorV0>,
}

impl WatchHandleV0 {
    /// Test-only watch injection. This is feature-gated and unavailable from
    /// the production transport capability surface.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn test_channel(
        capacity: usize,
    ) -> (
        tokio::sync::mpsc::Sender<Result<WatchEventV0, TransportErrorV0>>,
        Self,
    ) {
        let (sender, events) = tokio::sync::mpsc::channel(capacity);
        (
            sender,
            Self {
                events,
                overflowed: Arc::new(AtomicBool::new(false)),
                terminal: None,
            },
        )
    }

    /// Reads the next event. A 410 or an upstream error is mapped to a domain
    /// failure so the caller must invalidate its cache epoch.
    pub async fn next_event(&mut self) -> Result<WatchEventV0, TransportErrorV0> {
        if let Some(error) = &self.terminal {
            return Err(error.clone());
        }
        let event = self
            .events
            .recv()
            .await
            .unwrap_or(Err(TransportErrorV0::WatchGone));
        if self.overflowed.load(Ordering::Acquire) {
            self.terminal = Some(TransportErrorV0::WatchOverflow);
            return Err(TransportErrorV0::WatchOverflow);
        }
        // Any stream failure invalidates the epoch; never yield queued events
        // after a malformed bookmark, conflict, or decode/transport failure.
        if let Err(error) = &event {
            self.terminal = Some(error.clone());
        }
        event
    }

    /// Returns one already-buffered event without waiting.  Closing a watch is
    /// terminal just as it is for [`Self::next_event`]; callers must restart
    /// the epoch instead of treating an empty local buffer as authority.
    pub fn try_next_event(&mut self) -> Result<Option<WatchEventV0>, TransportErrorV0> {
        if let Some(error) = &self.terminal {
            return Err(error.clone());
        }
        if self.overflowed.load(Ordering::Acquire) {
            self.terminal = Some(TransportErrorV0::WatchOverflow);
            return Err(TransportErrorV0::WatchOverflow);
        }
        match self.events.try_recv() {
            Ok(event) => {
                if let Err(error) = &event {
                    self.terminal = Some(error.clone());
                }
                event.map(Some)
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                self.terminal = Some(TransportErrorV0::WatchGone);
                Err(TransportErrorV0::WatchGone)
            }
        }
    }
}

/// Retained, authenticated watch state for one durable pending automation
/// occurrence.  It is created only through
/// [`KubeTransportV0::observe_pending_automation_run`], which establishes the
/// list/watch/reread barrier before returning it.
pub struct AutomationRunObservationBarrierV0 {
    expectation: AutomationRunExpectationV0,
    watch: WatchHandleV0,
    current: ValidatedAutomationRunV0,
}

impl AutomationRunObservationBarrierV0 {
    #[must_use]
    pub fn current(&self) -> &ValidatedAutomationRunV0 {
        &self.current
    }

    /// Drains buffered list-to-watch-race events.  Delivery order, not
    /// lexical resourceVersion ordering, decides the final value.
    pub fn drain_buffered_events(&mut self) -> Result<bool, TransportErrorV0> {
        let mut changed = false;
        while let Some(event) = self.watch.try_next_event()? {
            changed |= self.apply(event)?;
        }
        Ok(changed)
    }

    /// Waits until an authenticated change for this occurrence arrives.
    /// Unrelated list/watch traffic is ignored after the common validator
    /// identifies it as outside the durable pending name.
    pub async fn next_change(&mut self) -> Result<ValidatedAutomationRunV0, TransportErrorV0> {
        loop {
            if self.drain_buffered_events()? {
                return Ok(self.current.clone());
            }
            let event = self.watch.next_event().await?;
            if self.apply(event)? {
                return Ok(self.current.clone());
            }
        }
    }

    fn apply(&mut self, event: WatchEventV0) -> Result<bool, TransportErrorV0> {
        let Some(validated) = validate_automation_watch_event(&event, &self.expectation)? else {
            return Ok(false);
        };
        if self.current.uid == validated.uid
            && self.current.resource_version == validated.resource_version
        {
            return Ok(false);
        }
        self.current = validated;
        Ok(true)
    }

    /// Builds a retained pending-run watch only for runtime tests, while
    /// still passing the supplied current object through the same shared
    /// validator used by LIST, WATCH, and GET in production.  Production
    /// code has no construction path around `observe_pending_automation_run`.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn test_from_authenticated(
        expectation: AutomationRunExpectationV0,
        current: ObjectV0,
        watch: WatchHandleV0,
    ) -> Result<Self, TransportErrorV0> {
        let current = validate_automation_run_get(Some(current), &expectation)?;
        Ok(Self {
            expectation,
            watch,
            current,
        })
    }
}

impl KubeTransportV0 {
    /// Creates a client using only the in-cluster service-account configuration.
    ///
    /// # Errors
    ///
    /// Returns a domain transport error when in-cluster configuration or TLS
    /// client construction fails.
    pub async fn in_cluster() -> Result<Self, TransportErrorV0> {
        let config = std::future::ready(Config::incluster().map_err(map_kube_error)).await?;
        let client = Client::try_from(config).map_err(map_kube_error)?;
        Ok(Self {
            client,
            #[cfg(any(test, feature = "test-support"))]
            test_watches: Arc::new(Mutex::new(VecDeque::new())),
        })
    }

    /// Builds a transport around an in-memory Kubernetes client for external
    /// adapter contract tests.  This is feature-gated so production callers
    /// cannot substitute an arbitrary Kubernetes service.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    #[must_use]
    pub fn test_from_client(client: Client) -> Self {
        Self {
            client,
            test_watches: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Queues one opaque watch for the next test-only `watch_from` call.
    /// The production transport cannot inject events or bypass list/watch
    /// resource-version fencing.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn test_push_watch(&self, watch: WatchHandleV0) {
        if let Ok(mut watches) = self.test_watches.lock() {
            watches.push_back(watch);
        }
    }

    fn api(&self, resource: ResourceKindV0, namespace: &str) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), namespace, &api_resource(resource))
    }

    /// Executes one sealed core command. Requests for descriptors, rendering,
    /// and observation are explicitly not Kubernetes effects. `Mutate` must be
    /// sequenced by its caller: this transport will not send a batch whose
    /// later writes could race an earlier acknowledgement.
    pub async fn dispatch(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        validate_address(address)?;
        if empty_state_resource_version_sentinel(command) {
            return match command {
                CommandV0::VerifyOwnedChildrenAbsent {
                    token,
                    preconditions,
                } => {
                    self.verify_root_only_cleanup(address, token, preconditions)
                        .await
                }
                CommandV0::RemoveRootFinalizer {
                    token,
                    preconditions,
                } => {
                    self.remove_root_only_finalizer(address, token, preconditions)
                        .await
                }
                _ => Err(TransportErrorV0::ManualRecoveryHold),
            };
        }
        match command {
            CommandV0::EnsureBootstrap {
                token,
                state,
                root_uid,
                root_generation,
                root_resource_version,
                state_name,
            } => {
                self.execute_bootstrap(
                    address,
                    token,
                    state,
                    root_uid,
                    *root_generation,
                    root_resource_version,
                    state_name,
                )
                .await
            }
            CommandV0::Persist {
                state,
                preconditions,
                operation,
            } => {
                let fence = match self.reread_fence(address, preconditions).await {
                    Ok(fence) => fence,
                    Err(error) => return Err(error),
                };
                let mut desired = state_object(&fence.state, state);
                inject_operation_proof(
                    &mut desired.value,
                    &fence.root.metadata.uid,
                    &operation.token,
                )?;
                let written = match self
                    .patch_state(&desired, &preconditions.state_resource_version)
                    .await
                {
                    Ok(written) => written,
                    Err(error) if may_recover_response_loss(&error) => {
                        return self
                            .recover_persist(address, state, preconditions, operation)
                            .await;
                    }
                    Err(error) => return Err(error),
                };
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::Persisted {
                        token: operation.token.clone(),
                        old_state_resource_version: preconditions.state_resource_version.clone(),
                        new_state_resource_version: written.metadata.resource_version,
                        state_uid: written.metadata.uid,
                    },
                )))
            }
            CommandV0::PersistRootStatus {
                token,
                root_uid,
                old_root_resource_version,
                preconditions,
                status,
            } => {
                let fence = match self.reread_fence(address, preconditions).await {
                    Ok(fence) => fence,
                    Err(error) => return Err(error),
                };
                if fence.root.metadata.uid != *root_uid
                    || old_root_resource_version != &preconditions.root_resource_version
                {
                    return Err(TransportErrorV0::Stale("root status pins"));
                }
                let desired = root_status_object(&fence.root, status);
                let written = match self
                    .patch_web_application_status(&desired, old_root_resource_version)
                    .await
                {
                    Ok(written) => written,
                    Err(error)
                        if may_recover_response_loss(&error)
                            || is_root_status_write_conflict(&error) =>
                    {
                        return self
                            .recover_root_status(
                                address,
                                token,
                                root_uid,
                                old_root_resource_version,
                                status,
                            )
                            .await;
                    }
                    Err(error) => return Err(error),
                };
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::RootStatusPersisted {
                        token: token.clone(),
                        old_root_resource_version: old_root_resource_version.clone(),
                        new_root_resource_version: written.metadata.resource_version,
                        root_uid: written.metadata.uid,
                    },
                )))
            }
            CommandV0::Mutate { commands, .. } if commands.len() != 1 => {
                Ok(CommandOutcomeV0::NeedsSequencing {
                    remaining: commands.len().saturating_sub(1),
                })
            }
            CommandV0::Mutate {
                transaction_id,
                attempt_token,
                commands,
            } => {
                self.execute_mutation(address, transaction_id, attempt_token, &commands[0])
                    .await
            }
            CommandV0::EnsureAutomationRun {
                token,
                name,
                spec,
                preconditions,
            } => {
                self.execute_automation(address, token, name, spec, preconditions)
                    .await
            }
            CommandV0::DeleteAutomationRun {
                token,
                occurrence_id,
                name,
                uid,
                resource_version,
                preconditions,
            } => {
                let _fence = self.reread_fence(address, preconditions).await?;
                let reference = ObjectRefV0 {
                    resource: ResourceKindV0::AutomationRun,
                    namespace: address.namespace.clone(),
                    name: name.clone(),
                };
                if reference.namespace != address.namespace {
                    return Err(TransportErrorV0::UnsupportedTarget);
                }
                self.delete_automation_run(&reference, uid, resource_version)
                    .await
                    .map_err(|_| ambiguous_delete_or_finalizer())?;
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::AutomationRunDeleted {
                        token: token.clone(),
                        occurrence_id: occurrence_id.clone(),
                        name: name.clone(),
                        uid: uid.clone(),
                        resource_version: resource_version.clone(),
                    },
                )))
            }
            CommandV0::VerifyOwnedChildrenAbsent {
                token,
                preconditions,
            } => {
                let fence = self.reread_fence(address, preconditions).await?;
                for resource in [
                    ResourceKindV0::ConfigMap,
                    ResourceKindV0::Service,
                    ResourceKindV0::AutomationRun,
                ] {
                    if self
                        .list_exact(resource, &address.namespace)
                        .await?
                        .items
                        .into_iter()
                        .any(|item| owned_by(&item, &fence.root.metadata.uid))
                    {
                        return Ok(CommandOutcomeV0::Event(Box::new(
                            oxideloop_controller_core::EventV0::OwnedChildrenVerificationFailed {
                                token: token.clone(),
                                reason: "owned child remains present".into(),
                            },
                        )));
                    }
                }
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::OwnedChildrenVerifiedAbsent {
                        token: token.clone(),
                        cleanup_token: token.clone(),
                        root_generation: generation(&fence.root)?,
                        root_uid: fence.root.metadata.uid,
                        root_resource_version: fence.root.metadata.resource_version,
                        state_uid: fence.state.metadata.uid,
                        state_resource_version: fence.state.metadata.resource_version,
                    },
                )))
            }
            CommandV0::RemoveStateFinalizer {
                token,
                preconditions,
            } => {
                let fence = self.reread_fence(address, preconditions).await?;
                let written = self.remove_finalizer(&fence.state, STATE_FINALIZER).await?;
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::StateFinalizerRemoved {
                        token: token.clone(),
                        state_uid: fence.state.metadata.uid,
                        old_state_resource_version: fence.state.metadata.resource_version,
                        new_state_resource_version: written.metadata.resource_version,
                    },
                )))
            }
            CommandV0::RemoveRootFinalizer {
                token,
                preconditions,
            } => {
                let fence = self.reread_fence(address, preconditions).await?;
                let written = self.remove_finalizer(&fence.root, ROOT_FINALIZER).await?;
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::RootFinalizerRemoved {
                        token: token.clone(),
                        root_uid: fence.root.metadata.uid,
                        old_root_resource_version: fence.root.metadata.resource_version,
                        new_root_resource_version: written.metadata.resource_version,
                    },
                )))
            }
            CommandV0::VerifyRootUidAbsent {
                token,
                root_uid,
                preconditions,
            } => {
                if root_uid != &preconditions.root_uid {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                let (root_ref, _) = address.references();
                match self.reread(&root_ref).await? {
                    None => Ok(CommandOutcomeV0::Event(Box::new(
                        oxideloop_controller_core::EventV0::RootUidVerifiedAbsent {
                            token: token.clone(),
                            root_uid: root_uid.clone(),
                        },
                    ))),
                    Some(observed) if observed.metadata.uid != *root_uid => {
                        Ok(CommandOutcomeV0::Event(Box::new(
                            oxideloop_controller_core::EventV0::RootUidVerifiedAbsent {
                                token: token.clone(),
                                root_uid: root_uid.clone(),
                            },
                        )))
                    }
                    Some(observed) => Ok(CommandOutcomeV0::Event(Box::new(
                        oxideloop_controller_core::EventV0::RootUidAbsenceFailed {
                            token: token.clone(),
                            observed_uid: observed.metadata.uid,
                        },
                    ))),
                }
            }
            CommandV0::RequestDescriptor { .. }
            | CommandV0::RequestRender { .. }
            | CommandV0::RequestDeletionObservation { .. }
            | CommandV0::Hold { .. }
            | CommandV0::IgnoredStale { .. } => Ok(CommandOutcomeV0::NotKubernetesEffect),
        }
    }

    async fn recover_persist(
        &self,
        address: &RootAddressV0,
        state: &OxideLoopStateSpec,
        preconditions: &PreconditionsV0,
        operation: &oxideloop_controller_api::PersistOperation,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (_, state_ref) = address.references();
        let observed = self
            .reread(&state_ref)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        if observed.metadata.uid != preconditions.state_uid
            || observed.metadata.resource_version == preconditions.state_resource_version
            || observed.value.get("spec")
                != Some(
                    &serde_json::to_value(state)
                        .map_err(|_| TransportErrorV0::ManualRecoveryHold)?,
                )
            || !has_recovery_proof(
                &observed,
                &preconditions.root_uid,
                &operation.token,
                &operation.token,
            )
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::Persisted {
                token: operation.token.clone(),
                old_state_resource_version: preconditions.state_resource_version.clone(),
                new_state_resource_version: observed.metadata.resource_version,
                state_uid: observed.metadata.uid,
            },
        )))
    }

    async fn recover_root_status(
        &self,
        address: &RootAddressV0,
        token: &str,
        root_uid: &str,
        old_rv: &str,
        status: &WebApplicationStatus,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (root_ref, _) = address.references();
        let observed = self
            .reread(&root_ref)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let intended =
            serde_json::to_value(status).map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let durable_ownership = has_finalizer(&observed, ROOT_FINALIZER)
            && observed
                .metadata
                .annotations
                .get(ROOT_UID_ANNOTATION)
                .is_some_and(|value| value == root_uid);
        if observed.metadata.uid != root_uid || observed.metadata.resource_version.is_empty() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if observed.metadata.resource_version == old_rv {
            return Err(TransportErrorV0::NonRebasableConflict(
                ConflictProvenanceV0::Write {
                    operation: WriteOperationV0::ApplyStatus,
                    resource: ResourceKindV0::WebApplication,
                },
            ));
        }
        if observed.value.get("status") != Some(&intended)
            || status.last_status_persist_token.as_deref() != Some(token)
            || !durable_ownership
        {
            return Err(TransportErrorV0::Stale("root status conflict"));
        }
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::RootStatusPersisted {
                token: token.into(),
                old_root_resource_version: old_rv.into(),
                new_root_resource_version: observed.metadata.resource_version,
                root_uid: root_uid.into(),
            },
        )))
    }

    async fn execute_bootstrap(
        &self,
        address: &RootAddressV0,
        token: &str,
        state: &OxideLoopStateSpec,
        root_uid: &str,
        root_generation: i64,
        root_rv: &str,
        state_name: &str,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        if address.state_name != state_name {
            return Err(TransportErrorV0::IdentityMismatch);
        }
        let (root_ref, state_ref) = address.references();
        let root = self
            .reread(&root_ref)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        if root.metadata.uid != root_uid
            || generation(&root)? != root_generation
            || (root.metadata.resource_version != root_rv
                && !has_bootstrap_root_metadata(&root, token))
        {
            return Err(TransportErrorV0::Stale("bootstrap root pins"));
        }
        let desired = bootstrap_state(&state_ref, state, &root, token)?;
        let state = match self.reread(&state_ref).await? {
            None => {
                match self.create_state(&desired).await {
                    Ok(_) => {}
                    Err(error) if may_recover_response_loss(&error) => {
                        recover_bootstrap_state(
                            self.reread(&state_ref).await?,
                            &desired,
                            &root,
                            token,
                        )?;
                    }
                    Err(error) => return Err(error),
                }
                return Ok(CommandOutcomeV0::NeedsSequencing { remaining: 3 });
            }
            Some(observed) => recover_bootstrap_state(Some(observed), &desired, &root, token)?,
        };
        if !has_finalizer(&state, STATE_FINALIZER) {
            self.ensure_finalizer(&state, STATE_FINALIZER).await?;
            return Ok(CommandOutcomeV0::NeedsSequencing { remaining: 2 });
        }
        if !has_bootstrap_root_metadata(&root, token) {
            self.bootstrap_root_metadata(&root, token).await?;
            return Ok(CommandOutcomeV0::NeedsSequencing { remaining: 1 });
        }
        if !has_state_link(&root, &state) {
            self.bootstrap_root_status(&root, &state).await?;
            return Ok(CommandOutcomeV0::NeedsSequencing { remaining: 0 });
        }
        let final_state = self
            .reread(&state_ref)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let final_root = self
            .reread(&root_ref)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        if final_state.metadata.uid != state.metadata.uid
            || final_state.metadata.resource_version != state.metadata.resource_version
            || !has_finalizer(&final_state, STATE_FINALIZER)
            || !has_bootstrap_root_metadata(&final_root, token)
            || !has_state_link(&final_root, &final_state)
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::BootstrapAcknowledged {
                token: token.into(),
                state_name: state_name.into(),
                state_uid: final_state.metadata.uid,
                state_resource_version: final_state.metadata.resource_version,
                old_root_resource_version: root_rv.into(),
                new_root_resource_version: final_root.metadata.resource_version,
                status_state_link: StateLink {
                    root_uid: root_uid.into(),
                    name: state_name.into(),
                    uid: state.metadata.uid,
                },
                root_finalizer_present: true,
                state_finalizer_present: true,
            },
        )))
    }

    async fn execute_mutation(
        &self,
        address: &RootAddressV0,
        transaction_id: &str,
        attempt_token: &str,
        command: &MutationCommandV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let reference = reference_from_identity(&command.identity)?;
        if reference.namespace != address.namespace {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        match &command.decision {
            MutationDecision::Post { body, .. }
            | MutationDecision::ConditionalApply { body, .. } => {
                // This validation deliberately precedes the fence rereads:
                // unsupported Service modes must make zero Kubernetes calls.
                validate_desired_object(body, &reference)?;
            }
            _ => {}
        }
        let target = match &command.decision {
            MutationDecision::Post { .. } => None,
            _ => Some(reference.clone()),
        };
        let fence = self
            .reread_fence_with_target(address, &command.preconditions, target.as_ref())
            .await?;
        let outcome = match &command.decision {
            MutationDecision::Post {
                method: HttpMethod::Post,
                body,
            } => self
                .mutation_create(&reference, body, &fence.root, transaction_id, command)
                .await
                .map(|o| result(command, ApplyOutcome::Applied, Some(o))),
            MutationDecision::ConditionalApply {
                method: HttpMethod::Patch,
                body,
                expected,
                ..
            } if expected.uid == fence.target.as_ref().expect("fenced").metadata.uid
                && expected.resource_version
                    == fence
                        .target
                        .as_ref()
                        .expect("fenced")
                        .metadata
                        .resource_version =>
            {
                self.mutation_apply(
                    &reference,
                    body,
                    &fence.root,
                    transaction_id,
                    command,
                    &fence.target.expect("fenced"),
                )
                .await
                .map(|o| result(command, ApplyOutcome::Applied, Some(o)))
            }
            MutationDecision::PreconditionedDelete {
                method: HttpMethod::Delete,
                preconditions,
            } => self
                .mutation_delete(&reference, preconditions, &fence.target.expect("fenced"))
                .await
                .map(|identity| {
                    result_identity(
                        command,
                        ApplyOutcome::Deleted,
                        Some(identity.uid),
                        Some(identity.resource_version),
                    )
                }),
            _ => {
                return Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::MutationFailed {
                        attempt_token: attempt_token.into(),
                        transaction_id: transaction_id.into(),
                        reason: "unsupported mutation decision or target".into(),
                    },
                )));
            }
        }?;
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::MutationAcknowledged {
                attempt_token: attempt_token.into(),
                transaction_id: transaction_id.into(),
                results: vec![outcome],
            },
        )))
    }

    async fn reread_fence(
        &self,
        address: &RootAddressV0,
        preconditions: &PreconditionsV0,
    ) -> Result<EffectFenceV0, TransportErrorV0> {
        let (root_ref, state_ref) = address.references();
        let root = self
            .reread(&root_ref)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        let state = self
            .reread(&state_ref)
            .await?
            .ok_or(TransportErrorV0::Stale("state absent"))?;
        let fence = EffectFenceV0 {
            root,
            state,
            target: None,
        };
        fence.validate(preconditions, None)?;
        Ok(fence)
    }

    async fn reread_fence_with_target(
        &self,
        address: &RootAddressV0,
        preconditions: &PreconditionsV0,
        target: Option<&ObjectRefV0>,
    ) -> Result<EffectFenceV0, TransportErrorV0> {
        let (root_ref, state_ref) = address.references();
        let root = self
            .reread(&root_ref)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        let state = self
            .reread(&state_ref)
            .await?
            .ok_or(TransportErrorV0::Stale("state absent"))?;
        let target = match target {
            Some(reference) => Some(
                self.reread(reference)
                    .await?
                    .ok_or(TransportErrorV0::Stale("target absent"))?,
            ),
            None => None,
        };
        let target_ref = target.as_ref().map(|object| object.reference.clone());
        let fence = EffectFenceV0 {
            root,
            state,
            target,
        };
        fence.validate(preconditions, target_ref.as_ref())?;
        Ok(fence)
    }

    async fn ensure_finalizer(
        &self,
        object: &ObjectV0,
        finalizer: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        if has_finalizer(object, finalizer) {
            return Ok(object.clone());
        }
        let operation = if object.value.pointer("/metadata/finalizers").is_some() {
            serde_json::json!({"op":"add","path":"/metadata/finalizers/-","value":finalizer})
        } else {
            serde_json::json!({"op":"add","path":"/metadata/finalizers","value":[finalizer]})
        };
        let patch = serde_json::json!([{"op":"test","path":"/metadata/resourceVersion","value":object.metadata.resource_version},operation]);
        self.api(object.reference.resource, &object.reference.namespace)
            .patch(
                &object.reference.name,
                &PatchParams::default(),
                &Patch::<Value>::Json(
                    serde_json::from_value::<json_patch::Patch>(patch)
                        .map_err(|e| TransportErrorV0::Other(e.to_string()))?,
                ),
            )
            .await
            .map_err(|error| {
                map_write_error(
                    error,
                    WriteOperationV0::JsonPatch,
                    object.reference.resource,
                )
            })
            .and_then(|o| to_domain(object.reference.resource, &object.reference.namespace, o))
    }

    async fn remove_finalizer(
        &self,
        object: &ObjectV0,
        finalizer: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let Some(index) = finalizer_index(object, finalizer) else {
            return Err(TransportErrorV0::ManualRecoveryHold);
        };
        let patch = serde_json::json!([{"op":"test","path":"/metadata/uid","value":object.metadata.uid},{"op":"test","path":"/metadata/resourceVersion","value":object.metadata.resource_version},{"op":"test","path":format!("/metadata/finalizers/{index}"),"value":finalizer},{"op":"remove","path":format!("/metadata/finalizers/{index}")}]);
        self.api(object.reference.resource, &object.reference.namespace)
            .patch(
                &object.reference.name,
                &PatchParams::default(),
                &Patch::<Value>::Json(
                    serde_json::from_value::<json_patch::Patch>(patch)
                        .map_err(|_| ambiguous_delete_or_finalizer())?,
                ),
            )
            .await
            .map_err(|_| ambiguous_delete_or_finalizer())
            .and_then(|o| {
                to_domain(object.reference.resource, &object.reference.namespace, o)
                    .map_err(|_| ambiguous_delete_or_finalizer())
            })
    }

    async fn bootstrap_root_metadata(
        &self,
        root: &ObjectV0,
        token: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let mut annotations = root.metadata.annotations.clone();
        annotations.insert(ROOT_UID_ANNOTATION.into(), root.metadata.uid.clone());
        annotations.insert(TRANSACTION_ID_ANNOTATION.into(), token.into());
        annotations.insert(COMMAND_ID_ANNOTATION.into(), token.into());
        let mut finalizers = root
            .value
            .pointer("/metadata/finalizers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !finalizers
            .iter()
            .any(|v| v.as_str() == Some(ROOT_FINALIZER))
        {
            finalizers.push(Value::String(ROOT_FINALIZER.into()));
        }
        let patch = serde_json::json!([
          {"op":"test","path":"/metadata/uid","value":root.metadata.uid},
          {"op":"test","path":"/metadata/resourceVersion","value":root.metadata.resource_version},
          {"op":"add","path":"/metadata/annotations","value":annotations},
          {"op":"add","path":"/metadata/finalizers","value":finalizers}
        ]);
        self.json_patch(root, patch).await
    }
    async fn bootstrap_root_status(
        &self,
        root: &ObjectV0,
        state: &ObjectV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let body = serde_json::json!({"metadata":{"resourceVersion":root.metadata.resource_version},"status":{"state":StateLink { root_uid:root.metadata.uid.clone(), name:state.reference.name.clone(), uid:state.metadata.uid.clone() }}});
        let written = self
            .api(ResourceKindV0::WebApplication, &root.reference.namespace)
            .patch_status(
                &root.reference.name,
                &PatchParams::default(),
                &Patch::Merge(&body),
            )
            .await
            .map_err(|error| {
                map_write_error(
                    error,
                    WriteOperationV0::MergeStatus,
                    ResourceKindV0::WebApplication,
                )
            })?;
        to_domain(
            ResourceKindV0::WebApplication,
            &root.reference.namespace,
            written,
        )
    }
    async fn json_patch(
        &self,
        object: &ObjectV0,
        patch: Value,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let patch = serde_json::from_value::<json_patch::Patch>(patch)
            .map_err(|e| TransportErrorV0::Other(e.to_string()))?;
        let written = self
            .api(object.reference.resource, &object.reference.namespace)
            .patch(
                &object.reference.name,
                &PatchParams::default(),
                &Patch::<Value>::Json(patch),
            )
            .await
            .map_err(|error| {
                map_write_error(
                    error,
                    WriteOperationV0::JsonPatch,
                    object.reference.resource,
                )
            })?;
        to_domain(
            object.reference.resource,
            &object.reference.namespace,
            written,
        )
    }

    async fn mutation_create(
        &self,
        reference: &ObjectRefV0,
        body: &Value,
        root: &ObjectV0,
        tx: &str,
        command: &MutationCommandV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let desired = owned_mutation_object(reference, body, root, tx, command)?;
        let created = match reference.resource {
            ResourceKindV0::ConfigMap => self.create_config_map(&desired).await,
            ResourceKindV0::Service => self.create_service(&desired).await,
            _ => return Err(TransportErrorV0::UnsupportedTarget),
        };
        match created {
            Ok(o) => Ok(o),
            Err(error) if may_recover_response_loss(&error) => recover_dropped_write_exact(
                self.reread(reference).await?.as_ref(),
                &desired,
                root,
                None,
                tx,
                &command.command_id,
            ),
            Err(e) => Err(e),
        }
    }
    async fn mutation_apply(
        &self,
        reference: &ObjectRefV0,
        body: &Value,
        root: &ObjectV0,
        tx: &str,
        command: &MutationCommandV0,
        target: &ObjectV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let mut desired = owned_mutation_object(reference, body, root, tx, command)?;
        desired.metadata = target.metadata.clone();
        let applied = match reference.resource {
            ResourceKindV0::ConfigMap => {
                self.patch_config_map(&desired, &target.metadata.resource_version)
                    .await
            }
            ResourceKindV0::Service => {
                self.patch_service(&desired, &target.metadata.resource_version)
                    .await
            }
            _ => return Err(TransportErrorV0::UnsupportedTarget),
        };
        match applied {
            Ok(o) => Ok(o),
            Err(error) if may_recover_response_loss(&error) => recover_dropped_write_exact(
                self.reread(reference).await?.as_ref(),
                &desired,
                root,
                Some(&target.metadata.uid),
                tx,
                &command.command_id,
            ),
            Err(e) => Err(e),
        }
    }
    async fn mutation_delete(
        &self,
        reference: &ObjectRefV0,
        expected: &oxideloop_mutation_model::ObjectIdentity,
        target: &ObjectV0,
    ) -> Result<oxideloop_mutation_model::ObjectIdentity, TransportErrorV0> {
        if target.metadata.uid != expected.uid
            || target.metadata.resource_version != expected.resource_version
        {
            return Err(TransportErrorV0::Stale("mutation delete pins"));
        }
        let result = match reference.resource {
            ResourceKindV0::ConfigMap => {
                self.delete_config_map(reference, &expected.uid, &expected.resource_version)
                    .await
            }
            ResourceKindV0::Service => {
                self.delete_service(reference, &expected.uid, &expected.resource_version)
                    .await
            }
            _ => Err(TransportErrorV0::UnsupportedTarget),
        };
        result.map_err(|_| ambiguous_delete_or_finalizer())?;
        // DELETE returns only a Kubernetes Status object. The exact prepared
        // identity is the authoritative acknowledgement; never infer it by
        // rereading an object after deletion.
        Ok(expected.clone())
    }

    async fn execute_automation(
        &self,
        address: &RootAddressV0,
        token: &str,
        name: &str,
        spec: &AutomationRunSpec,
        preconditions: &PreconditionsV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let fence = self.reread_fence(address, preconditions).await?;
        if spec.root_uid != fence.root.metadata.uid {
            return Err(TransportErrorV0::Stale("automation root UID"));
        }
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: address.namespace.clone(),
            name: name.into(),
        };
        let desired = automation_object(&reference, spec, &fence.root, token)?;
        let run = match self.reread(&reference).await? {
            Some(existing) => recover_automation(existing, &desired, &fence.root, token)?,
            None => match self.create_automation_run(&desired).await {
                Ok(o) => o,
                Err(error) if may_recover_response_loss(&error) => recover_automation(
                    self.reread(&reference)
                        .await?
                        .ok_or(TransportErrorV0::ResponseLost)?,
                    &desired,
                    &fence.root,
                    token,
                )?,
                Err(e) => return Err(e),
            },
        };
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::AutomationRunEnsured {
                token: token.into(),
                name: name.into(),
                spec: spec.clone(),
                uid: run.metadata.uid,
                resource_version: run.metadata.resource_version,
            },
        )))
    }

    /// Lists an allowlisted resource and preserves its exact collection RV.
    ///
    /// # Errors
    ///
    /// Returns the API failure as a domain error.
    pub async fn list_exact(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
    ) -> Result<ListV0, TransportErrorV0> {
        validate_namespace(namespace)?;
        if resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        let listed = self
            .api(resource, namespace)
            .list(&ListParams::default())
            .await
            .map_err(|error| map_read_error(error, ReadOperationV0::List, resource))?;
        let resource_version =
            listed
                .metadata
                .resource_version
                .clone()
                .ok_or(TransportErrorV0::Other(
                    "list response lacks resourceVersion".into(),
                ))?;
        let items = listed
            .into_iter()
            .map(|object| to_domain(resource, namespace, object))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ListV0 {
            items,
            resource_version,
        })
    }

    /// Performs the exact deletion-observation read path for one owned root.
    ///
    /// The root/state fence is pinned to the in-flight command, deletion is
    /// rechecked from the live root, and only children having the exact sole
    /// controller owner are returned after an exact identity reread. This is
    /// intentionally the only transport API that produces a deletion
    /// revalidation snapshot.
    pub async fn observe_deletion_revalidation(
        &self,
        address: &RootAddressV0,
        preconditions: &PreconditionsV0,
    ) -> Result<DeletionObservationSnapshotV0, TransportErrorV0> {
        validate_address(address)?;
        let fence = self.reread_fence(address, preconditions).await?;
        let generation = fence
            .root
            .value
            .pointer("/metadata/generation")
            .and_then(Value::as_i64);
        if generation != Some(preconditions.root_generation)
            || fence.root.reference.resource != ResourceKindV0::WebApplication
            || fence.root.reference.namespace != address.namespace
            || fence.root.reference.name != address.name
            || fence
                .root
                .value
                .pointer("/metadata/deletionTimestamp")
                .is_none_or(Value::is_null)
        {
            return Err(TransportErrorV0::Stale("deletion observation root pins"));
        }

        let mut inventory = Vec::new();
        for resource in [ResourceKindV0::ConfigMap, ResourceKindV0::Service] {
            let listed = self.list_exact(resource, &address.namespace).await?;
            for object in listed.items {
                if !has_exact_canonical_owner(
                    &object,
                    &fence.root.reference,
                    &fence.root.metadata.uid,
                ) {
                    continue;
                }
                match self.reread(&object.reference).await? {
                    None => {}
                    Some(reread)
                        if reread.metadata.uid == object.metadata.uid
                            && reread.metadata.resource_version
                                == object.metadata.resource_version =>
                    {
                        inventory.push(reread);
                    }
                    Some(_) => return Err(TransportErrorV0::Stale("deletion child identity")),
                }
            }
        }

        let mut automation_runs = Vec::new();
        let listed = self
            .list_exact(ResourceKindV0::AutomationRun, &address.namespace)
            .await?;
        for object in listed.items {
            if !has_exact_canonical_owner(&object, &fence.root.reference, &fence.root.metadata.uid)
            {
                continue;
            }
            match self.reread(&object.reference).await? {
                None => {}
                Some(reread)
                    if reread.metadata.uid == object.metadata.uid
                        && reread.metadata.resource_version == object.metadata.resource_version =>
                {
                    automation_runs.push(reread);
                }
                Some(_) => return Err(TransportErrorV0::Stale("deletion AutomationRun identity")),
            }
        }
        Ok(DeletionObservationSnapshotV0 {
            root: fence.root,
            state: fence.state,
            inventory,
            automation_runs,
        })
    }

    /// Re-reads one exact allowlisted namespace-scoped object.
    ///
    /// # Errors
    ///
    /// Returns the API failure as a domain error.
    pub async fn reread(
        &self,
        reference: &ObjectRefV0,
    ) -> Result<Option<ObjectV0>, TransportErrorV0> {
        validate_reference(reference)?;
        if reference.resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        self.api(reference.resource, &reference.namespace)
            .get_opt(&reference.name)
            .await
            .map_err(|error| map_read_error(error, ReadOperationV0::Get, reference.resource))?
            .map(|object| to_domain(reference.resource, &reference.namespace, object))
            .transpose()
    }

    /// Materializes one deterministic Event, then creates it or accepts only
    /// an exact semantic recovery at its planned name. Kubernetes object types
    /// and generic bodies never cross this purpose-named boundary.
    ///
    /// # Errors
    ///
    /// Returns `ForeignCollision` for a present malformed or mismatching Event,
    /// and `ResponseLost` when an ambiguous create cannot be proven by a fresh
    /// exact-name read.
    pub async fn create_or_exact_recover_event(
        &self,
        plan: &PlannedEventV0,
        controller_supplied_event_time: &str,
    ) -> Result<EventMaterializationRefV0, TransportErrorV0> {
        let value = materialize_event(plan, controller_supplied_event_time)
            .map_err(|error| TransportErrorV0::Other(error.to_string()))?;
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::Event,
            namespace: plan.metadata.namespace.clone(),
            name: plan.metadata.name.clone(),
        };
        validate_reference(&reference)?;
        if let Some(observed) = self.reread_event(&reference).await? {
            recover_event(&value, &observed.value)?;
            return Ok(event_ref(&observed));
        }
        let desired = ObjectV0 {
            reference: reference.clone(),
            metadata: ObjectMetaV0 {
                uid: String::new(),
                resource_version: String::new(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value,
        };
        match self.create_event(&desired).await {
            Ok(written) => Ok(event_ref(&written)),
            Err(error)
                if may_recover_response_loss(&error)
                    || matches!(
                        error,
                        TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
                            operation: WriteOperationV0::Create,
                            resource: ResourceKindV0::Event,
                        })
                    ) =>
            {
                let observed = self
                    .reread_event(&reference)
                    .await?
                    .ok_or(TransportErrorV0::ResponseLost)?;
                recover_event(&desired.value, &observed.value)?;
                Ok(event_ref(&observed))
            }
            Err(error) => Err(error),
        }
    }

    /// Reads one bundle key from a same-namespace `ConfigMap`. The controller
    /// namespace is an explicit capability boundary: a cross-namespace
    /// request is rejected before any Kubernetes call. Only `binaryData` is
    /// accepted; text `data`, filesystem fallback, and implicit defaults are
    /// intentionally unavailable.
    pub async fn read_bundle_binary_data(
        &self,
        controller_namespace: &str,
        requested_namespace: &str,
        config_map_name: &str,
        key: &str,
        max_bytes: usize,
    ) -> Result<ConfigMapBinaryDataV0, TransportErrorV0> {
        validate_namespace(controller_namespace)?;
        validate_namespace(requested_namespace)?;
        if controller_namespace != requested_namespace
            || config_map_name.is_empty()
            || key.is_empty()
        {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        if max_bytes == 0 {
            return Err(TransportErrorV0::InvalidBundleData);
        }
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::ConfigMap,
            namespace: requested_namespace.into(),
            name: config_map_name.into(),
        };
        let object = self
            .reread(&reference)
            .await?
            .ok_or(TransportErrorV0::InvalidBundleData)?;
        let response_metadata = object.value.get("metadata").and_then(Value::as_object);
        if object.reference != reference
            || object.value.get("apiVersion").and_then(Value::as_str)
                != Some(ResourceKindV0::ConfigMap.api_version())
            || object.value.get("kind").and_then(Value::as_str)
                != Some(ResourceKindV0::ConfigMap.kind())
            || response_metadata
                .and_then(|metadata| metadata.get("name"))
                .and_then(Value::as_str)
                != Some(reference.name.as_str())
            || response_metadata
                .and_then(|metadata| metadata.get("namespace"))
                .and_then(Value::as_str)
                != Some(reference.namespace.as_str())
            || object.metadata.uid.is_empty()
            || object.metadata.uid.len() > MAX_BUNDLE_IDENTITY_BYTES
            || object.metadata.resource_version.is_empty()
            || object.metadata.resource_version.len() > MAX_BUNDLE_IDENTITY_BYTES
        {
            return Err(TransportErrorV0::InvalidBundleData);
        }
        let binary_data = object
            .value
            .get("binaryData")
            .and_then(Value::as_object)
            .ok_or(TransportErrorV0::InvalidBundleData)?;
        if object
            .value
            .get("data")
            .and_then(Value::as_object)
            .is_some_and(|data| data.contains_key(key))
        {
            return Err(TransportErrorV0::InvalidBundleData);
        }
        let encoded = binary_data
            .get(key)
            .and_then(Value::as_str)
            .ok_or(TransportErrorV0::InvalidBundleData)?;
        let bytes = decode_base64(encoded, max_bytes)?;
        Ok(ConfigMapBinaryDataV0 {
            bytes,
            uid: object.metadata.uid,
            resource_version: object.metadata.resource_version,
        })
    }

    async fn root_only_cleanup_fence(
        &self,
        address: &RootAddressV0,
        token: &str,
        preconditions: &PreconditionsV0,
        command: RootOnlyCleanupCommandV0,
    ) -> Result<(ObjectV0, DeletionCleanupReceipt), TransportErrorV0> {
        if preconditions.state_resource_version.is_empty() {
            // This branch is valid only for the deletion sentinel. All values
            // below are re-read from Kubernetes; none are cache authority.
        } else {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let (root_ref, _) = address.references();
        let root = self
            .reread(&root_ref)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let root_metadata = root.value.get("metadata").and_then(Value::as_object);
        if root.reference != root_ref
            || root.value.get("apiVersion").and_then(Value::as_str)
                != Some(ResourceKindV0::WebApplication.api_version())
            || root.value.get("kind").and_then(Value::as_str)
                != Some(ResourceKindV0::WebApplication.kind())
            || root_metadata
                .and_then(|metadata| metadata.get("name"))
                .and_then(Value::as_str)
                != Some(address.name.as_str())
            || root_metadata
                .and_then(|metadata| metadata.get("namespace"))
                .and_then(Value::as_str)
                != Some(address.namespace.as_str())
            || root.metadata.uid != preconditions.root_uid
            || root.metadata.resource_version != preconditions.root_resource_version
            || generation(&root)? != preconditions.root_generation
            || !has_finalizer(&root, ROOT_FINALIZER)
            || root
                .value
                .pointer("/metadata/deletionTimestamp")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let status = serde_json::from_value::<WebApplicationStatus>(
            root.value
                .get("status")
                .cloned()
                .ok_or(TransportErrorV0::ManualRecoveryHold)?,
        )
        .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let receipt = status
            .deletion_cleanup_receipt
            .clone()
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        receipt
            .validate()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let expected_state_name = oxideloop_controller_api::state_name(&root.metadata.uid)
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let state_link_matches = status.state.as_ref().is_none_or(|link| {
            link.root_uid == root.metadata.uid
                && link.name == receipt.state_name
                && link.uid == receipt.state_uid
        });
        let root_generation = generation(&root)?;
        if receipt.root_uid != root.metadata.uid
            || receipt.state_name != expected_state_name
            || receipt.state_name != address.state_name
            || receipt.state_uid != preconditions.state_uid
            || receipt.deletion_start_generation > root_generation
            || !state_link_matches
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let expected_token = match command {
            RootOnlyCleanupCommandV0::VerifyOwnedChildrenAbsent => {
                root_only_owned_children_verify_token(
                    &receipt.deletion_id,
                    receipt.deletion_start_generation,
                    &root.metadata.uid,
                    root_generation,
                    &root.metadata.resource_version,
                    &receipt.state_uid,
                )
            }
            RootOnlyCleanupCommandV0::RemoveRootFinalizer => root_finalizer_command_token(
                &root.metadata.uid,
                &root.metadata.resource_version,
                &receipt.state_uid,
                "",
            ),
        };
        if token != expected_token {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let state_ref = ObjectRefV0 {
            resource: ResourceKindV0::OxideLoopState,
            namespace: address.namespace.clone(),
            name: receipt.state_name.clone(),
        };
        if self.reread(&state_ref).await?.is_some() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok((root, receipt))
    }

    async fn verify_root_only_cleanup(
        &self,
        address: &RootAddressV0,
        token: &str,
        preconditions: &PreconditionsV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (root, receipt) = self
            .root_only_cleanup_fence(
                address,
                token,
                preconditions,
                RootOnlyCleanupCommandV0::VerifyOwnedChildrenAbsent,
            )
            .await?;
        for resource in [
            ResourceKindV0::ConfigMap,
            ResourceKindV0::Service,
            ResourceKindV0::AutomationRun,
        ] {
            if self
                .list_exact(resource, &address.namespace)
                .await?
                .items
                .into_iter()
                .any(|item| owned_by(&item, &root.metadata.uid))
            {
                return Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::OwnedChildrenVerificationFailed {
                        token: token.into(),
                        reason: "owned child remains present".into(),
                    },
                )));
            }
        }
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::OwnedChildrenVerifiedAbsent {
                token: token.into(),
                // The receipt proves cleanup that completed before the
                // linked state disappeared. It is distinct from this fresh,
                // deterministic VerifyOwnedChildrenAbsent command token.
                cleanup_token: receipt.cleanup_token,
                root_generation: generation(&root)?,
                root_uid: root.metadata.uid,
                root_resource_version: root.metadata.resource_version,
                state_uid: receipt.state_uid,
                state_resource_version: String::new(),
            },
        )))
    }

    async fn remove_root_only_finalizer(
        &self,
        address: &RootAddressV0,
        token: &str,
        preconditions: &PreconditionsV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (root, receipt) = self
            .root_only_cleanup_fence(
                address,
                token,
                preconditions,
                RootOnlyCleanupCommandV0::RemoveRootFinalizer,
            )
            .await?;
        for resource in [
            ResourceKindV0::ConfigMap,
            ResourceKindV0::Service,
            ResourceKindV0::AutomationRun,
        ] {
            if self
                .list_exact(resource, &address.namespace)
                .await?
                .items
                .into_iter()
                .any(|item| owned_by(&item, &root.metadata.uid))
            {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
        }
        let written = self
            .remove_finalizer(&root, ROOT_FINALIZER)
            .await
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::RootFinalizerRemoved {
                token: token.into(),
                root_uid: receipt.root_uid,
                old_root_resource_version: root.metadata.resource_version,
                new_root_resource_version: written.metadata.resource_version,
            },
        )))
    }

    /// Persists only an exact, validated `AutomationRun` completion on `/status`.
    /// It never patches or adopts immutable spec fields.
    pub async fn persist_automation_run_completion(
        &self,
        namespace: &str,
        name: &str,
        expected_uid: &str,
        expected_old_resource_version: &str,
        expected_spec: &AutomationRunSpec,
        desired_status: &AutomationRunStatus,
    ) -> Result<AutomationCompletionAckV0, TransportErrorV0> {
        expected_spec
            .validate()
            .map_err(|_| TransportErrorV0::ForeignCollision)?;
        desired_status
            .validate()
            .map_err(|_| TransportErrorV0::ForeignCollision)?;
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: namespace.into(),
            name: name.into(),
        };
        let observed = self
            .reread(&reference)
            .await?
            .ok_or(TransportErrorV0::ResponseLost)?;
        let observed_spec: AutomationRunSpec = serde_json::from_value(
            observed
                .value
                .get("spec")
                .cloned()
                .ok_or(TransportErrorV0::ForeignCollision)?,
        )
        .map_err(|_| TransportErrorV0::ForeignCollision)?;
        if observed.metadata.uid != expected_uid {
            return Err(TransportErrorV0::Stale("automation UID"));
        }
        if &observed_spec != expected_spec {
            return Err(TransportErrorV0::ForeignCollision);
        }
        if observed.metadata.resource_version != expected_old_resource_version {
            return completion_recovery(
                &observed,
                expected_uid,
                expected_old_resource_version,
                expected_spec,
                desired_status,
            );
        }
        let desired = ObjectV0 {
            reference,
            metadata: observed.metadata.clone(),
            value: serde_json::json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"AutomationRun","metadata":{"name":name,"namespace":namespace},"status":desired_status}),
        };
        match self
            .apply_status(
                &desired,
                ResourceKindV0::AutomationRun,
                expected_old_resource_version,
            )
            .await
        {
            Ok(written) => completion_recovery(
                &written,
                expected_uid,
                expected_old_resource_version,
                expected_spec,
                desired_status,
            ),
            Err(error) if may_recover_response_loss(&error) => {
                let recovered = self
                    .reread(&desired.reference)
                    .await?
                    .ok_or(TransportErrorV0::ResponseLost)?;
                completion_recovery(
                    &recovered,
                    expected_uid,
                    expected_old_resource_version,
                    expected_spec,
                    desired_status,
                )
            }
            Err(error) => Err(error),
        }
    }

    /// Starts an exact-resourceVersion watch for one allowlisted collection.
    ///
    /// # Errors
    ///
    /// Returns `WatchGone` for API 410 and a domain transport error otherwise.
    pub async fn watch_from(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        resource_version: &str,
    ) -> Result<WatchHandleV0, TransportErrorV0> {
        validate_namespace(namespace)?;
        if resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        if resource_version.is_empty() {
            return Err(TransportErrorV0::Stale("watch resourceVersion"));
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(mut watches) = self.test_watches.lock()
            && let Some(watch) = watches.pop_front()
        {
            return Ok(watch);
        }
        let stream = self
            .api(resource, namespace)
            .watch(&kube::api::WatchParams::default(), resource_version)
            .await
            .map_err(|error| map_watch_open_error(error, resource))?;
        let namespace = namespace.to_owned();
        let events = stream.map(move |event| match event {
            Ok(kube::api::WatchEvent::Added(object)) => {
                to_domain(resource, &namespace, object).map(WatchEventV0::Added)
            }
            Ok(kube::api::WatchEvent::Modified(object)) => {
                to_domain(resource, &namespace, object).map(WatchEventV0::Modified)
            }
            Ok(kube::api::WatchEvent::Deleted(object)) => {
                to_domain(resource, &namespace, object).map(WatchEventV0::Deleted)
            }
            Ok(kube::api::WatchEvent::Bookmark(bookmark))
                if !bookmark.metadata.resource_version.is_empty() =>
            {
                Ok(WatchEventV0::Bookmark {
                    resource_version: bookmark.metadata.resource_version,
                })
            }
            Ok(kube::api::WatchEvent::Bookmark(_)) => Err(TransportErrorV0::Other(
                "watch bookmark lacks resourceVersion".into(),
            )),
            Ok(kube::api::WatchEvent::Error(status)) if status.code == 410 => {
                Err(TransportErrorV0::WatchGone)
            }
            Ok(kube::api::WatchEvent::Error(status)) if status.code == 409 => Err(
                TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal {
                    stage: WatchStageV0::Stream,
                    resource,
                }),
            ),
            Ok(kube::api::WatchEvent::Error(status)) => {
                Err(TransportErrorV0::Other(status.message))
            }
            Err(error) => Err(map_watch_stream_error(error, resource)),
        });
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let overflowed = Arc::new(AtomicBool::new(false));
        let overflow_signal = Arc::clone(&overflowed);
        tokio::spawn(async move {
            futures_util::pin_mut!(events);
            while let Some(event) = events.next().await {
                if sender.try_send(event).is_err() {
                    overflow_signal.store(true, Ordering::Release);
                    break;
                }
            }
        });
        Ok(WatchHandleV0 {
            events: receiver,
            overflowed,
            terminal: None,
        })
    }

    /// Establishes an authenticated list/watch/reread barrier for one durable
    /// pending `AutomationRun`.  The list validates a same-name collision,
    /// the watch starts from that list's exact RV, and the exact GET plus
    /// buffered-event drain closes the creation race before the retained
    /// handle is returned.
    pub async fn observe_pending_automation_run(
        &self,
        expectation: AutomationRunExpectationV0,
    ) -> Result<AutomationRunObservationBarrierV0, TransportErrorV0> {
        validate_automation_expectation(&expectation)?;
        let list = self
            .list_exact(ResourceKindV0::AutomationRun, &expectation.root.namespace)
            .await?;
        let _ = validate_automation_run_list(&list, &expectation)?;
        let watch = self
            .watch_from(
                ResourceKindV0::AutomationRun,
                &expectation.root.namespace,
                &list.resource_version,
            )
            .await?;
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: expectation.root.namespace.clone(),
            name: expectation.run_name.clone(),
        };
        let current = validate_automation_run_get(self.reread(&reference).await?, &expectation)?;
        let mut barrier = AutomationRunObservationBarrierV0 {
            expectation,
            watch,
            current,
        };
        // A delete or malformed same-name event in this gap is a hold, not a
        // reason to use the reread value.  The final delivery wins without
        // assuming Kubernetes resourceVersions are lexically ordered.
        let _ = barrier.drain_buffered_events()?;
        Ok(barrier)
    }

    /// Creates only `OxideLoopState` objects.
    async fn create_state(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::OxideLoopState).await
    }
    /// Creates only `AutomationRun` objects.
    async fn create_automation_run(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::AutomationRun).await
    }
    /// Creates only core `ConfigMap` objects.
    async fn create_config_map(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::ConfigMap).await
    }
    /// Creates only core `Service` objects.
    async fn create_service(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::Service).await
    }
    /// Creates only an already-planned Event; this remains private so callers
    /// cannot submit an arbitrary Event body.
    async fn create_event(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::Event).await
    }
    /// Reads Events only for the typed create/recovery proof; callers cannot
    /// obtain an arbitrary Event body through the generic read surface.
    async fn reread_event(
        &self,
        reference: &ObjectRefV0,
    ) -> Result<Option<ObjectV0>, TransportErrorV0> {
        require_reference_resource(reference, ResourceKindV0::Event)?;
        self.api(ResourceKindV0::Event, &reference.namespace)
            .get_opt(&reference.name)
            .await
            .map_err(|error| map_read_error(error, ReadOperationV0::Get, ResourceKindV0::Event))?
            .map(|object| to_domain(ResourceKindV0::Event, &reference.namespace, object))
            .transpose()
    }
    /// Server-side applies only a `ConfigMap` with the exact expected RV.
    async fn patch_config_map(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.apply(object, ResourceKindV0::ConfigMap, expected_rv, false)
            .await
    }
    /// Server-side applies only a `Service` with the exact expected RV.
    async fn patch_service(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.apply(object, ResourceKindV0::Service, expected_rv, false)
            .await
    }
    /// Patches only an `OxideLoopState` with the exact expected RV.
    async fn patch_state(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.apply(object, ResourceKindV0::OxideLoopState, expected_rv, false)
            .await
    }

    /// Server-side applies only an `AutomationRun` with the exact expected RV.
    /// Server-side applies only the root status with the exact old RV.
    async fn patch_web_application_status(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.apply_status(object, ResourceKindV0::WebApplication, expected_rv)
            .await
    }

    /// Deletes only an `AutomationRun` with UID and RV preconditions.
    async fn delete_automation_run(
        &self,
        reference: &ObjectRefV0,
        uid: &str,
        rv: &str,
    ) -> Result<(), TransportErrorV0> {
        self.delete(reference, ResourceKindV0::AutomationRun, uid, rv)
            .await
    }
    /// Deletes only a core `ConfigMap` with UID and RV preconditions.
    async fn delete_config_map(
        &self,
        reference: &ObjectRefV0,
        uid: &str,
        rv: &str,
    ) -> Result<(), TransportErrorV0> {
        self.delete(reference, ResourceKindV0::ConfigMap, uid, rv)
            .await
    }
    /// Deletes only a core `Service` with UID and RV preconditions.
    async fn delete_service(
        &self,
        reference: &ObjectRefV0,
        uid: &str,
        rv: &str,
    ) -> Result<(), TransportErrorV0> {
        self.delete(reference, ResourceKindV0::Service, uid, rv)
            .await
    }

    async fn create(
        &self,
        object: &ObjectV0,
        expected: ResourceKindV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        require_resource(object, expected)?;
        validate_desired_object(&object.value, &object.reference)?;
        let dynamic: DynamicObject = serde_json::from_value(object.value.clone())
            .map_err(|error| TransportErrorV0::Other(error.to_string()))?;
        let written = self
            .api(expected, &object.reference.namespace)
            .create(&kube::api::PostParams::default(), &dynamic)
            .await
            .map_err(|error| map_write_error(error, WriteOperationV0::Create, expected))?;
        to_domain(expected, &object.reference.namespace, written)
    }

    async fn apply(
        &self,
        object: &ObjectV0,
        expected: ResourceKindV0,
        expected_rv: &str,
        force: bool,
    ) -> Result<ObjectV0, TransportErrorV0> {
        require_resource(object, expected)?;
        validate_desired_object(&object.value, &object.reference)?;
        if object.metadata.resource_version != expected_rv {
            return Err(TransportErrorV0::Stale("write resourceVersion"));
        }
        let params = if force {
            PatchParams::apply("oxideloop-controller").force()
        } else {
            PatchParams::apply("oxideloop-controller")
        };
        let mut body = object.value.clone();
        inject_resource_version(&mut body, expected_rv)?;
        let written = self
            .api(expected, &object.reference.namespace)
            .patch(&object.reference.name, &params, &Patch::Apply(&body))
            .await
            .map_err(|error| map_write_error(error, WriteOperationV0::Apply, expected))?;
        to_domain(expected, &object.reference.namespace, written)
    }

    async fn apply_status(
        &self,
        object: &ObjectV0,
        expected: ResourceKindV0,
        expected_rv: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        require_resource(object, expected)?;
        validate_desired_object(&object.value, &object.reference)?;
        if object.metadata.resource_version != expected_rv {
            return Err(TransportErrorV0::Stale("write resourceVersion"));
        }
        let mut body = object.value.clone();
        inject_resource_version(&mut body, expected_rv)?;
        let written = self
            .api(expected, &object.reference.namespace)
            .patch_status(
                &object.reference.name,
                &PatchParams::apply("oxideloop-controller"),
                &Patch::Apply(&body),
            )
            .await
            .map_err(|error| map_write_error(error, WriteOperationV0::ApplyStatus, expected))?;
        to_domain(expected, &object.reference.namespace, written)
    }

    async fn delete(
        &self,
        reference: &ObjectRefV0,
        expected: ResourceKindV0,
        uid: &str,
        rv: &str,
    ) -> Result<(), TransportErrorV0> {
        require_reference_resource(reference, expected)?;
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.into()),
                resource_version: Some(rv.into()),
            }),
            ..DeleteParams::default()
        };
        self.api(expected, &reference.namespace)
            .delete(&reference.name, &params)
            .await
            .map_err(|error| map_write_error(error, WriteOperationV0::Delete, expected))?;
        Ok(())
    }
}

fn api_resource(resource: ResourceKindV0) -> ApiResource {
    let (group, version, plural) = match resource {
        ResourceKindV0::WebApplication => ("demo.oxideloop.dev", "v1alpha1", "webapplications"),
        ResourceKindV0::OxideLoopState => ("runtime.oxideloop.dev", "v1alpha1", "oxideloopstates"),
        ResourceKindV0::AutomationRun => ("runtime.oxideloop.dev", "v1alpha1", "automationruns"),
        ResourceKindV0::ConfigMap => ("", "v1", "configmaps"),
        ResourceKindV0::Service => ("", "v1", "services"),
        ResourceKindV0::Event => ("events.k8s.io", "v1", "events"),
    };
    ApiResource {
        group: group.into(),
        version: version.into(),
        api_version: resource.api_version().into(),
        kind: resource.kind().into(),
        plural: plural.into(),
    }
}

fn validate_namespace(namespace: &str) -> Result<(), TransportErrorV0> {
    if namespace.is_empty()
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        Err(TransportErrorV0::UnsupportedTarget)
    } else {
        Ok(())
    }
}

fn decode_base64(encoded: &str, max_bytes: usize) -> Result<Vec<u8>, TransportErrorV0> {
    if encoded.is_empty() || !encoded.len().is_multiple_of(4) {
        return Err(TransportErrorV0::InvalidBundleData);
    }
    let max_encoded = max_bytes
        .saturating_add(2)
        .checked_div(3)
        .and_then(|groups| groups.checked_mul(4))
        .unwrap_or(usize::MAX);
    if encoded.len() > max_encoded {
        return Err(TransportErrorV0::InvalidBundleData);
    }

    let chunks = encoded.as_bytes().as_chunks::<4>().0;
    let mut decoded_len = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        let (_, chunk_len) = decode_base64_quartet(*chunk, index + 1 == chunks.len())?;
        decoded_len = decoded_len
            .checked_add(chunk_len)
            .ok_or(TransportErrorV0::InvalidBundleData)?;
    }
    if decoded_len == 0 || decoded_len > max_bytes {
        return Err(TransportErrorV0::InvalidBundleData);
    }

    let mut output = Vec::with_capacity(decoded_len);
    for (index, chunk) in chunks.iter().enumerate() {
        let ([a, b, c, d], chunk_len) = decode_base64_quartet(*chunk, index + 1 == chunks.len())?;
        output.push((a << 2) | (b >> 4));
        if chunk_len >= 2 {
            output.push((b << 4) | (c >> 2));
        }
        if chunk_len == 3 {
            output.push((c << 6) | d);
        }
    }
    Ok(output)
}

fn decode_base64_quartet(chunk: [u8; 4], last: bool) -> Result<([u8; 4], usize), TransportErrorV0> {
    let pad2 = chunk[2] == b'=';
    let pad3 = chunk[3] == b'=';
    if chunk[0] == b'=' || chunk[1] == b'=' || pad2 && !pad3 || pad3 && !last {
        return Err(TransportErrorV0::InvalidBundleData);
    }
    let a = decode_base64_char(chunk[0]).ok_or(TransportErrorV0::InvalidBundleData)?;
    let b = decode_base64_char(chunk[1]).ok_or(TransportErrorV0::InvalidBundleData)?;
    let c = if pad2 {
        0
    } else {
        decode_base64_char(chunk[2]).ok_or(TransportErrorV0::InvalidBundleData)?
    };
    let d = if pad3 {
        0
    } else {
        decode_base64_char(chunk[3]).ok_or(TransportErrorV0::InvalidBundleData)?
    };
    if (pad2 && b & 0x0f != 0) || (pad3 && !pad2 && c & 0x03 != 0) {
        return Err(TransportErrorV0::InvalidBundleData);
    }
    let decoded_len = if pad2 {
        1
    } else if pad3 {
        2
    } else {
        3
    };
    Ok(([a, b, c, d], decoded_len))
}

const fn decode_base64_char(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
fn validate_address(address: &RootAddressV0) -> Result<(), TransportErrorV0> {
    validate_namespace(&address.namespace)?;
    if address.name.is_empty() || address.state_name.is_empty() {
        Err(TransportErrorV0::UnsupportedTarget)
    } else {
        Ok(())
    }
}
fn validate_reference(reference: &ObjectRefV0) -> Result<(), TransportErrorV0> {
    validate_namespace(&reference.namespace)?;
    if reference.name.is_empty() {
        Err(TransportErrorV0::UnsupportedTarget)
    } else {
        Ok(())
    }
}
fn require_resource(object: &ObjectV0, resource: ResourceKindV0) -> Result<(), TransportErrorV0> {
    require_reference_resource(&object.reference, resource)
}
fn require_reference_resource(
    reference: &ObjectRefV0,
    resource: ResourceKindV0,
) -> Result<(), TransportErrorV0> {
    validate_reference(reference)?;
    if reference.resource == resource {
        Ok(())
    } else {
        Err(TransportErrorV0::UnsupportedTarget)
    }
}
fn to_domain(
    resource: ResourceKindV0,
    namespace: &str,
    object: DynamicObject,
) -> Result<ObjectV0, TransportErrorV0> {
    let metadata = object.metadata.clone();
    let name = metadata.name.ok_or(TransportErrorV0::Other(
        "response lacks metadata.name".into(),
    ))?;
    let uid = metadata.uid.ok_or(TransportErrorV0::Other(
        "response lacks metadata.uid".into(),
    ))?;
    let resource_version = metadata.resource_version.ok_or(TransportErrorV0::Other(
        "response lacks metadata.resourceVersion".into(),
    ))?;
    let value =
        serde_json::to_value(object).map_err(|error| TransportErrorV0::Other(error.to_string()))?;
    Ok(ObjectV0 {
        reference: ObjectRefV0 {
            resource,
            namespace: namespace.into(),
            name,
        },
        metadata: ObjectMetaV0 {
            uid,
            resource_version,
            annotations: metadata.annotations.unwrap_or_default(),
            owner_uids: metadata
                .owner_references
                .unwrap_or_default()
                .into_iter()
                .map(|owner| owner.uid)
                .collect(),
        },
        value,
    })
}
/// Maps setup errors without inferring HTTP status from rendered text. API
/// conflicts receive provenance only at a sealed read, write, or watch boundary.
fn map_kube_error(error: impl std::fmt::Display) -> TransportErrorV0 {
    TransportErrorV0::Other(error.to_string())
}

fn map_read_error(
    error: kube::Error,
    operation: ReadOperationV0,
    resource: ResourceKindV0,
) -> TransportErrorV0 {
    match error {
        kube::Error::Api(status) if status.code == 409 => {
            TransportErrorV0::Conflict(ConflictProvenanceV0::Read {
                operation,
                resource,
            })
        }
        error => TransportErrorV0::Other(error.to_string()),
    }
}

fn map_watch_open_error(error: kube::Error, resource: ResourceKindV0) -> TransportErrorV0 {
    match error {
        kube::Error::Api(status) if status.code == 409 => {
            TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal {
                stage: WatchStageV0::Open,
                resource,
            })
        }
        kube::Error::Api(status) if status.code == 410 => TransportErrorV0::WatchGone,
        error => TransportErrorV0::Other(error.to_string()),
    }
}

fn map_watch_stream_error(error: kube::Error, resource: ResourceKindV0) -> TransportErrorV0 {
    match error {
        kube::Error::Api(status) if status.code == 409 => {
            TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal {
                stage: WatchStageV0::Stream,
                resource,
            })
        }
        kube::Error::Api(status) if status.code == 410 => TransportErrorV0::WatchGone,
        error => TransportErrorV0::Other(error.to_string()),
    }
}

fn map_write_error(
    error: kube::Error,
    operation: WriteOperationV0,
    resource: ResourceKindV0,
) -> TransportErrorV0 {
    match error {
        kube::Error::Api(status) if status.code == 409 => {
            TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
                operation,
                resource,
            })
        }
        error => TransportErrorV0::Other(error.to_string()),
    }
}

fn is_root_status_write_conflict(error: &TransportErrorV0) -> bool {
    matches!(
        error,
        TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
            operation: WriteOperationV0::ApplyStatus,
            resource: ResourceKindV0::WebApplication,
        })
    )
}

fn recover_event(expected: &Value, observed: &Value) -> Result<(), TransportErrorV0> {
    match semantically_equal(expected, observed) {
        Ok(true) => Ok(()),
        Ok(false) | Err(_) => Err(TransportErrorV0::ForeignCollision),
    }
}

fn event_ref(object: &ObjectV0) -> EventMaterializationRefV0 {
    EventMaterializationRefV0 {
        name: object.reference.name.clone(),
        uid: object.metadata.uid.clone(),
        resource_version: object.metadata.resource_version.clone(),
    }
}

fn completion_recovery(
    object: &ObjectV0,
    expected_uid: &str,
    old_rv: &str,
    expected_spec: &AutomationRunSpec,
    desired: &AutomationRunStatus,
) -> Result<AutomationCompletionAckV0, TransportErrorV0> {
    if object.metadata.uid != expected_uid || object.metadata.resource_version == old_rv {
        return Err(TransportErrorV0::Stale("automation completion pins"));
    }
    let spec: AutomationRunSpec = serde_json::from_value(
        object
            .value
            .get("spec")
            .cloned()
            .ok_or(TransportErrorV0::ForeignCollision)?,
    )
    .map_err(|_| TransportErrorV0::ForeignCollision)?;
    if &spec != expected_spec {
        return Err(TransportErrorV0::ForeignCollision);
    }
    let status: AutomationRunStatus = serde_json::from_value(
        object
            .value
            .get("status")
            .cloned()
            .ok_or(TransportErrorV0::ForeignCollision)?,
    )
    .map_err(|_| TransportErrorV0::ForeignCollision)?;
    if &status != desired {
        return Err(TransportErrorV0::ForeignCollision);
    }
    let completion = status
        .completion
        .ok_or(TransportErrorV0::ForeignCollision)?
        .state;
    Ok(AutomationCompletionAckV0 {
        uid: object.metadata.uid.clone(),
        old_resource_version: old_rv.into(),
        new_resource_version: object.metadata.resource_version.clone(),
        attempts: status.attempts,
        materialization_name: status
            .materialization_name
            .ok_or(TransportErrorV0::ForeignCollision)?,
        materialization_uid: status
            .materialization_uid
            .ok_or(TransportErrorV0::ForeignCollision)?,
        completion,
    })
}

fn automation_observation_hold(reason: impl Into<String>) -> TransportErrorV0 {
    TransportErrorV0::AutomationObservationHold(reason.into())
}

fn validate_automation_expectation(
    expectation: &AutomationRunExpectationV0,
) -> Result<(), TransportErrorV0> {
    if expectation.root.resource != ResourceKindV0::WebApplication
        || expectation.root.namespace.is_empty()
        || expectation.root.name.is_empty()
        || expectation.root_uid.is_empty()
        || expectation.occurrence_id.is_empty()
        || expectation.run_name.is_empty()
        || expectation.run_uid.is_empty()
        || expectation.run_resource_version.is_empty()
        || expectation.spec_hash.len() != 64
        || !expectation
            .spec_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(automation_observation_hold(
            "durable pending AutomationRun expectation is invalid",
        ));
    }
    Ok(())
}

fn is_durable_pending_name(object: &ObjectV0, expectation: &AutomationRunExpectationV0) -> bool {
    object.reference.namespace == expectation.root.namespace
        && object.reference.name == expectation.run_name
}

/// The one shared authentication gate for every `AutomationRun` observation
/// route.  Objects outside the durable pending name are intentionally ignored
/// even if malformed.  A same-name object must meet every immutable pin or
/// becomes an explicit Hold instead of an adoption opportunity.
pub fn validate_automation_run_observation(
    object: &ObjectV0,
    expectation: &AutomationRunExpectationV0,
) -> Result<Option<ValidatedAutomationRunV0>, TransportErrorV0> {
    validate_automation_expectation(expectation)?;
    if !is_durable_pending_name(object, expectation) {
        return Ok(None);
    }
    if object.reference.resource != ResourceKindV0::AutomationRun
        || object.reference.namespace != expectation.root.namespace
        || object.reference.name != expectation.run_name
        || object.metadata.uid != expectation.run_uid
        || object.metadata.resource_version.is_empty()
    {
        return Err(automation_observation_hold(
            "same-name AutomationRun identity does not match durable pending pins",
        ));
    }
    let metadata = object
        .value
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| automation_observation_hold("AutomationRun metadata is malformed"))?;
    if object.value.get("apiVersion").and_then(Value::as_str)
        != Some(ResourceKindV0::AutomationRun.api_version())
        || object.value.get("kind").and_then(Value::as_str)
            != Some(ResourceKindV0::AutomationRun.kind())
        || metadata.get("name").and_then(Value::as_str) != Some(expectation.run_name.as_str())
        || metadata.get("namespace").and_then(Value::as_str)
            != Some(expectation.root.namespace.as_str())
        || metadata.get("uid").and_then(Value::as_str) != Some(object.metadata.uid.as_str())
        || metadata.get("resourceVersion").and_then(Value::as_str)
            != Some(object.metadata.resource_version.as_str())
    {
        return Err(automation_observation_hold(
            "AutomationRun GVK or metadata differs from the exact response identity",
        ));
    }
    // A journaled pending run is live only while its exact pinned object has
    // not entered deletion.  This applies uniformly to list, exact GET, and
    // Added/Modified watch observations because they all pass this gate.
    // `Deleted` is separately fail-closed below, so no route can turn a
    // disappearing pending occurrence into successful completion.
    if metadata
        .get("deletionTimestamp")
        .is_some_and(|timestamp| !timestamp.is_null())
    {
        return Err(automation_observation_hold(
            "durable pending AutomationRun is deleting",
        ));
    }
    let annotations = metadata
        .get("annotations")
        .map(|value| serde_json::from_value::<BTreeMap<String, String>>(value.clone()))
        .transpose()
        .map_err(|_| automation_observation_hold("AutomationRun annotations are malformed"))?
        .unwrap_or_default();
    if annotations != object.metadata.annotations
        || !has_exact_canonical_owner(object, &expectation.root, &expectation.root_uid)
    {
        return Err(automation_observation_hold(
            "AutomationRun metadata lacks the exact sole controller owner",
        ));
    }
    let spec: AutomationRunSpec = serde_json::from_value(
        object
            .value
            .get("spec")
            .cloned()
            .ok_or_else(|| automation_observation_hold("AutomationRun spec is absent"))?,
    )
    .map_err(|_| automation_observation_hold("AutomationRun immutable spec is malformed"))?;
    spec.validate()
        .map_err(|_| automation_observation_hold("AutomationRun immutable spec is invalid"))?;
    validate_automation_run_name(&expectation.run_name, &spec)
        .map_err(|_| automation_observation_hold("AutomationRun name is not deterministic"))?;
    let spec_value = serde_json::to_value(&spec)
        .map_err(|_| automation_observation_hold("AutomationRun spec cannot be canonicalized"))?;
    let canonical = oxideloop_renderer_abi::canonical_bytes(&spec_value)
        .map_err(|_| automation_observation_hold("AutomationRun spec cannot be canonicalized"))?;
    if oxideloop_renderer_abi::sha256_hex(&canonical) != expectation.spec_hash
        || spec.root_uid != expectation.root_uid
        || spec.occurrence_id != expectation.occurrence_id
    {
        return Err(automation_observation_hold(
            "AutomationRun immutable spec does not match durable pending occurrence",
        ));
    }
    let status: AutomationRunStatus = match object.value.get("status") {
        Some(status) => serde_json::from_value(status.clone())
            .map_err(|_| automation_observation_hold("AutomationRun status is malformed"))?,
        None => AutomationRunStatus::default(),
    };
    status
        .validate()
        .map_err(|_| automation_observation_hold("AutomationRun status is invalid"))?;
    match &status.completion {
        None if status == AutomationRunStatus::default() => {
            if object.metadata.resource_version != expectation.run_resource_version {
                return Err(automation_observation_hold(
                    "incomplete AutomationRun resourceVersion differs from durable pending pin",
                ));
            }
        }
        None => {
            return Err(automation_observation_hold(
                "incomplete AutomationRun status is not pristine",
            ));
        }
        Some(completion)
            if status.attempts == 0
                || status.attempts > 32
                || completion.message.len() > MAX_MESSAGE_BYTES
                || status.materialization_name.is_none()
                || status.materialization_uid.is_none()
                || status.materialization_name.as_deref()
                    == Some(expectation.run_name.as_str())
                || status.materialization_uid.as_deref() == Some(expectation.run_uid.as_str())
                || object.metadata.resource_version == expectation.run_resource_version =>
        {
            return Err(automation_observation_hold(
                "completed AutomationRun status/materialization is not exact",
            ));
        }
        Some(_) => {}
    }
    Ok(Some(ValidatedAutomationRunV0 {
        reference: object.reference.clone(),
        uid: object.metadata.uid.clone(),
        pending_resource_version: expectation.run_resource_version.clone(),
        resource_version: object.metadata.resource_version.clone(),
        spec,
        status,
    }))
}

/// Validates the LIST leg of a pending-run barrier.  A malformed unrelated
/// item has no authority; duplicate same-name candidates are a hold.
pub fn validate_automation_run_list(
    list: &ListV0,
    expectation: &AutomationRunExpectationV0,
) -> Result<Option<ValidatedAutomationRunV0>, TransportErrorV0> {
    if list.resource_version.is_empty() {
        return Err(automation_observation_hold(
            "AutomationRun list lacks a collection resourceVersion",
        ));
    }
    let mut exact = None;
    for object in &list.items {
        let Some(validated) = validate_automation_run_observation(object, expectation)? else {
            continue;
        };
        if exact.replace(validated).is_some() {
            return Err(automation_observation_hold(
                "AutomationRun list contains duplicate durable pending candidates",
            ));
        }
    }
    Ok(exact)
}

/// Validates the exact GET leg.  Absence of a durable pending run is never a
/// benign observation, because the journal has already recorded its UID/RV.
pub fn validate_automation_run_get(
    object: Option<ObjectV0>,
    expectation: &AutomationRunExpectationV0,
) -> Result<ValidatedAutomationRunV0, TransportErrorV0> {
    let object = object
        .ok_or_else(|| automation_observation_hold("durable pending AutomationRun is absent"))?;
    validate_automation_run_observation(&object, expectation)?.ok_or_else(|| {
        automation_observation_hold("exact AutomationRun GET returned an unrelated object")
    })
}

/// Validates the watch leg using the same object gate as LIST and GET.
/// Deletion of the durable pending candidate is always a hold, even when the
/// tombstone otherwise carries a valid immutable spec.
pub fn validate_automation_watch_event(
    event: &WatchEventV0,
    expectation: &AutomationRunExpectationV0,
) -> Result<Option<ValidatedAutomationRunV0>, TransportErrorV0> {
    match event {
        WatchEventV0::Added(object) | WatchEventV0::Modified(object) => {
            validate_automation_run_observation(object, expectation)
        }
        WatchEventV0::Deleted(object) => {
            let validated = validate_automation_run_observation(object, expectation)?;
            if validated.is_some() {
                Err(automation_observation_hold(
                    "durable pending AutomationRun was deleted",
                ))
            } else {
                Ok(None)
            }
        }
        WatchEventV0::Bookmark { .. } => Ok(None),
    }
}

fn inject_resource_version(
    value: &mut Value,
    resource_version: &str,
) -> Result<(), TransportErrorV0> {
    let metadata = value
        .as_object_mut()
        .and_then(|object| object.get_mut("metadata"))
        .and_then(Value::as_object_mut)
        .ok_or(TransportErrorV0::MetadataSmuggling)?;
    metadata.insert(
        "resourceVersion".into(),
        Value::String(resource_version.into()),
    );
    Ok(())
}
fn inject_operation_proof(
    value: &mut Value,
    root_uid: &str,
    token: &str,
) -> Result<(), TransportErrorV0> {
    let metadata = value
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)
        .ok_or(TransportErrorV0::MetadataSmuggling)?;
    let annotations = metadata
        .entry("annotations")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or(TransportErrorV0::MetadataSmuggling)?;
    annotations.insert(ROOT_UID_ANNOTATION.into(), Value::String(root_uid.into()));
    annotations.insert(
        TRANSACTION_ID_ANNOTATION.into(),
        Value::String(token.into()),
    );
    annotations.insert(COMMAND_ID_ANNOTATION.into(), Value::String(token.into()));
    Ok(())
}

fn state_object(observed: &ObjectV0, state: &OxideLoopStateSpec) -> ObjectV0 {
    let value = serde_json::json!({
        "apiVersion": ResourceKindV0::OxideLoopState.api_version(),
        "kind": ResourceKindV0::OxideLoopState.kind(),
        "metadata": { "name": observed.reference.name, "namespace": observed.reference.namespace },
        "spec": state,
    });
    ObjectV0 {
        reference: observed.reference.clone(),
        metadata: observed.metadata.clone(),
        value,
    }
}

fn root_status_object(observed: &ObjectV0, status: &WebApplicationStatus) -> ObjectV0 {
    let value = serde_json::json!({
        "apiVersion": ResourceKindV0::WebApplication.api_version(),
        "kind": ResourceKindV0::WebApplication.kind(),
        "metadata": { "name": observed.reference.name, "namespace": observed.reference.namespace },
        "status": status,
    });
    ObjectV0 {
        reference: observed.reference.clone(),
        metadata: observed.metadata.clone(),
        value,
    }
}

fn generation(object: &ObjectV0) -> Result<i64, TransportErrorV0> {
    object
        .value
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
        .ok_or(TransportErrorV0::Stale("root generation"))
}
fn finalizer_index(object: &ObjectV0, wanted: &str) -> Option<usize> {
    object
        .value
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)?
        .iter()
        .position(|v| v.as_str() == Some(wanted))
}
fn has_finalizer(object: &ObjectV0, wanted: &str) -> bool {
    finalizer_index(object, wanted).is_some()
}
fn has_state_link(root: &ObjectV0, state: &ObjectV0) -> bool {
    root.value.pointer("/status/state").is_some_and(|link| {
        link.get("rootUid").and_then(Value::as_str) == Some(root.metadata.uid.as_str())
            && link.get("name").and_then(Value::as_str) == Some(state.reference.name.as_str())
            && link.get("uid").and_then(Value::as_str) == Some(state.metadata.uid.as_str())
    })
}
fn owned_by(object: &ObjectV0, root_uid: &str) -> bool {
    object.metadata.owner_uids.iter().any(|uid| uid == root_uid)
}
fn has_bootstrap_root_metadata(root: &ObjectV0, token: &str) -> bool {
    has_finalizer(root, ROOT_FINALIZER)
        && root
            .metadata
            .annotations
            .get(ROOT_UID_ANNOTATION)
            .is_some_and(|v| v == &root.metadata.uid)
        && root
            .metadata
            .annotations
            .get(TRANSACTION_ID_ANNOTATION)
            .is_some_and(|v| v == token)
        && root
            .metadata
            .annotations
            .get(COMMAND_ID_ANNOTATION)
            .is_some_and(|v| v == token)
}
fn reference_from_identity(identity: &ObjectIdentity) -> Result<ObjectRefV0, TransportErrorV0> {
    let resource = match (identity.api_version.as_str(), identity.kind.as_str()) {
        ("v1", "ConfigMap") => ResourceKindV0::ConfigMap,
        ("v1", "Service") => ResourceKindV0::Service,
        _ => return Err(TransportErrorV0::UnsupportedTarget),
    };
    Ok(ObjectRefV0 {
        resource,
        namespace: identity.namespace.clone(),
        name: identity.name.clone(),
    })
}
fn owned_mutation_object(
    reference: &ObjectRefV0,
    body: &Value,
    root: &ObjectV0,
    tx: &str,
    command: &MutationCommandV0,
) -> Result<ObjectV0, TransportErrorV0> {
    validate_desired_object(body, reference)?;
    let mut value = body.clone();
    let metadata = value
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)
        .ok_or(TransportErrorV0::MetadataSmuggling)?;
    let annotations = metadata
        .entry("annotations")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or(TransportErrorV0::MetadataSmuggling)?;
    annotations.insert(
        ROOT_UID_ANNOTATION.into(),
        Value::String(root.metadata.uid.clone()),
    );
    annotations.insert(TRANSACTION_ID_ANNOTATION.into(), Value::String(tx.into()));
    annotations.insert(
        COMMAND_ID_ANNOTATION.into(),
        Value::String(command.command_id.clone()),
    );
    metadata.insert("ownerReferences".into(), serde_json::json!([{"apiVersion": ResourceKindV0::WebApplication.api_version(), "kind": ResourceKindV0::WebApplication.kind(), "name": root.reference.name, "uid": root.metadata.uid, "controller": true, "blockOwnerDeletion": false}]));
    Ok(ObjectV0 {
        reference: reference.clone(),
        metadata: ObjectMetaV0 {
            uid: String::new(),
            resource_version: String::new(),
            annotations: BTreeMap::new(),
            owner_uids: vec![],
        },
        value,
    })
}
fn result(
    command: &MutationCommandV0,
    outcome: ApplyOutcome,
    object: Option<ObjectV0>,
) -> ApplyResult {
    result_identity(
        command,
        outcome,
        object.as_ref().map(|o| o.metadata.uid.clone()),
        object.map(|o| o.metadata.resource_version),
    )
}
fn result_identity(
    command: &MutationCommandV0,
    outcome: ApplyOutcome,
    uid: Option<String>,
    resource_version: Option<String>,
) -> ApplyResult {
    ApplyResult {
        command_id: command.command_id.clone(),
        attempt_token: command.attempt_token.clone(),
        identity: command.identity.clone(),
        outcome,
        uid,
        resource_version,
    }
}
fn bootstrap_state(
    reference: &ObjectRefV0,
    state: &OxideLoopStateSpec,
    root: &ObjectV0,
    token: &str,
) -> Result<ObjectV0, TransportErrorV0> {
    // State creation carries only the metadata permitted by the constrained
    // create path.  The state finalizer is an intentionally separate fenced
    // bootstrap step (`ensure_finalizer`), so a real API server cannot reject
    // the initial object for metadata smuggling and the resumable sequence
    // remains observable/recoverable one write at a time.
    let value = serde_json::json!({"apiVersion":ResourceKindV0::OxideLoopState.api_version(),"kind":ResourceKindV0::OxideLoopState.kind(),"metadata":{"name":reference.name,"namespace":reference.namespace,"annotations":{ROOT_UID_ANNOTATION:root.metadata.uid,TRANSACTION_ID_ANNOTATION:token,COMMAND_ID_ANNOTATION:token},"ownerReferences":[{"apiVersion":ResourceKindV0::WebApplication.api_version(),"kind":ResourceKindV0::WebApplication.kind(),"name":root.reference.name,"uid":root.metadata.uid,"controller":true,"blockOwnerDeletion":false}]},"spec":state});
    Ok(ObjectV0 {
        reference: reference.clone(),
        metadata: ObjectMetaV0 {
            uid: String::new(),
            resource_version: String::new(),
            annotations: BTreeMap::new(),
            owner_uids: vec![],
        },
        value,
    })
}
fn recover_bootstrap_state(
    observed: Option<ObjectV0>,
    desired: &ObjectV0,
    root: &ObjectV0,
    token: &str,
) -> Result<ObjectV0, TransportErrorV0> {
    let object = observed.ok_or(TransportErrorV0::ResponseLost)?;
    if object.value.get("spec") != desired.value.get("spec")
        || !has_recovery_proof(&object, &root.metadata.uid, token, token)
        || !has_exact_canonical_owner(&object, &root.reference, &root.metadata.uid)
    {
        return Err(TransportErrorV0::ForeignCollision);
    }
    Ok(object)
}
fn automation_object(
    reference: &ObjectRefV0,
    spec: &AutomationRunSpec,
    root: &ObjectV0,
    token: &str,
) -> Result<ObjectV0, TransportErrorV0> {
    let body = serde_json::json!({"apiVersion":ResourceKindV0::AutomationRun.api_version(),"kind":ResourceKindV0::AutomationRun.kind(),"metadata":{"name":reference.name,"namespace":reference.namespace,"annotations":{ROOT_UID_ANNOTATION:root.metadata.uid,TRANSACTION_ID_ANNOTATION:"automation/v0",COMMAND_ID_ANNOTATION:token},"ownerReferences":[{"apiVersion":ResourceKindV0::WebApplication.api_version(),"kind":ResourceKindV0::WebApplication.kind(),"name":root.reference.name,"uid":root.metadata.uid,"controller":true,"blockOwnerDeletion":false}]},"spec":spec});
    Ok(ObjectV0 {
        reference: reference.clone(),
        metadata: ObjectMetaV0 {
            uid: String::new(),
            resource_version: String::new(),
            annotations: BTreeMap::new(),
            owner_uids: vec![],
        },
        value: body,
    })
}
fn recover_automation(
    observed: ObjectV0,
    desired: &ObjectV0,
    root: &ObjectV0,
    token: &str,
) -> Result<ObjectV0, TransportErrorV0> {
    if observed.value.get("spec") != desired.value.get("spec")
        || !has_recovery_proof(&observed, &root.metadata.uid, "automation/v0", token)
        || !has_exact_canonical_owner(&observed, &root.reference, &root.metadata.uid)
        || !exact_metadata(&observed.value, &desired.value)
    {
        Err(TransportErrorV0::ForeignCollision)
    } else {
        Ok(observed)
    }
}

/// Fetches fresh objects through a sealed port and rejects cache-derived
/// effects. P6 supplies the address envelope; P5 supplies the validation.
pub struct FenceReaderV0<'a, P> {
    port: &'a mut P,
    address: RootAddressV0,
}

impl<'a, P: DiscoveryPortV0> FenceReaderV0<'a, P> {
    #[must_use]
    pub fn new(port: &'a mut P, address: RootAddressV0) -> Self {
        Self { port, address }
    }

    /// Reads root, state, and target immediately before an effect.
    ///
    /// # Errors
    ///
    /// Returns a stale error when an object is absent or does not match the
    /// command's exact UID/resourceVersion pins, or a port error on I/O.
    pub fn reread_fence(
        &mut self,
        preconditions: &PreconditionsV0,
        target: Option<&ObjectRefV0>,
    ) -> Result<EffectFenceV0, TransportErrorV0> {
        let (root_ref, state_ref) = self.address.references();
        let root = self
            .port
            .reread(&root_ref)?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        let state = self
            .port
            .reread(&state_ref)?
            .ok_or(TransportErrorV0::Stale("state absent"))?;
        let target = match target {
            Some(reference) => Some(
                self.port
                    .reread(reference)?
                    .ok_or(TransportErrorV0::Stale("target absent"))?,
            ),
            None => None,
        };
        let target_reference = target.as_ref().map(|item| item.reference.clone());
        let fence = EffectFenceV0 {
            root,
            state,
            target,
        };
        fence.validate(preconditions, target_reference.as_ref())?;
        Ok(fence)
    }
}

/// Fresh root, state, and optional mutation target fetched immediately before
/// one effect. This is intentionally a domain snapshot, not a cache entry.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectFenceV0 {
    pub root: ObjectV0,
    pub state: ObjectV0,
    pub target: Option<ObjectV0>,
}

impl EffectFenceV0 {
    pub fn validate(
        &self,
        preconditions: &PreconditionsV0,
        target: Option<&ObjectRefV0>,
    ) -> Result<(), TransportErrorV0> {
        if self.root.metadata.uid != preconditions.root_uid {
            return Err(TransportErrorV0::Stale("root UID"));
        }
        if self.root.metadata.resource_version != preconditions.root_resource_version {
            return Err(TransportErrorV0::Stale("root resourceVersion"));
        }
        if self.state.metadata.uid != preconditions.state_uid {
            return Err(TransportErrorV0::Stale("state UID"));
        }
        if self.state.metadata.resource_version != preconditions.state_resource_version {
            return Err(TransportErrorV0::Stale("state resourceVersion"));
        }
        if target.is_some() != self.target.is_some() {
            return Err(TransportErrorV0::Stale("target presence"));
        }
        if let (Some(want), Some(got)) = (target, &self.target)
            && &got.reference != want
        {
            return Err(TransportErrorV0::Stale("target identity"));
        }
        Ok(())
    }
}

/// Checks desired mutation JSON before it can become a Kubernetes request.
/// Only identity, explicitly supplied labels/annotations, and owner refs are
/// permitted metadata; namespace and GVK must exactly match the command.
pub fn validate_desired_object(
    object: &Value,
    expected: &ObjectRefV0,
) -> Result<(), TransportErrorV0> {
    let Some(map) = object.as_object() else {
        return Err(TransportErrorV0::IdentityMismatch);
    };
    if map.get("apiVersion").and_then(Value::as_str) != Some(expected.resource.api_version())
        || map.get("kind").and_then(Value::as_str) != Some(expected.resource.kind())
    {
        return Err(TransportErrorV0::IdentityMismatch);
    }
    let Some(metadata) = map.get("metadata").and_then(Value::as_object) else {
        return Err(TransportErrorV0::MetadataSmuggling);
    };
    if metadata.get("name").and_then(Value::as_str) != Some(expected.name.as_str())
        || metadata.get("namespace").and_then(Value::as_str) != Some(expected.namespace.as_str())
    {
        return Err(TransportErrorV0::IdentityMismatch);
    }
    for key in metadata.keys() {
        if !matches!(
            key.as_str(),
            "name" | "namespace" | "labels" | "annotations" | "ownerReferences"
        ) {
            return Err(TransportErrorV0::MetadataSmuggling);
        }
    }
    if let Some(owners) = metadata.get("ownerReferences").and_then(Value::as_array)
        && owners.iter().any(|owner| {
            owner
                .get("blockOwnerDeletion")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
    {
        return Err(TransportErrorV0::MetadataSmuggling);
    }
    if expected.resource == ResourceKindV0::Service && !is_supported_service_v0(object) {
        return Err(TransportErrorV0::UnsupportedTarget);
    }
    Ok(())
}

/// P5 v0 deliberately supports only `ClusterIP` Service semantics. Headless
/// Services are ClusterIP-safe and retain an exact `clusterIP: None` proof.
fn is_supported_service_v0(object: &Value) -> bool {
    let Some(spec) = object.get("spec").and_then(Value::as_object) else {
        return false;
    };
    if !matches!(
        spec.get("type").and_then(Value::as_str),
        None | Some("ClusterIP")
    ) {
        return false;
    }
    ![
        "externalName",
        "loadBalancerClass",
        "allocateLoadBalancerNodePorts",
        "externalTrafficPolicy",
        "healthCheckNodePort",
    ]
    .iter()
    .any(|field| spec.contains_key(*field))
        && !spec
            .get("ports")
            .and_then(Value::as_array)
            .is_some_and(|ports| ports.iter().any(|port| port.get("nodePort").is_some()))
        && has_unambiguous_cluster_ip_allocation(spec)
}

fn has_unambiguous_cluster_ip_allocation(spec: &serde_json::Map<String, Value>) -> bool {
    let cluster_ip = match spec.get("clusterIP") {
        None => None,
        Some(Value::String(value)) if !value.is_empty() => Some(value.as_str()),
        Some(_) => return false,
    };
    let cluster_ips = match spec.get("clusterIPs") {
        None => None,
        Some(Value::Array(values)) if !values.is_empty() => Some(values),
        Some(_) => return false,
    };
    let Some(cluster_ips) = cluster_ips else {
        return true;
    };
    if cluster_ips
        .iter()
        .any(|value| value.as_str().is_none_or(str::is_empty))
    {
        return false;
    }
    match cluster_ip {
        Some("None") => cluster_ips.len() == 1 && cluster_ips[0].as_str() == Some("None"),
        Some(_) | None => cluster_ips[0].as_str() != Some("None"),
    }
}

/// Exact root-bound proof for a create/patch whose HTTP response was dropped.
#[must_use]
pub fn has_recovery_proof(
    object: &ObjectV0,
    root_uid: &str,
    transaction_id: &str,
    command_id: &str,
) -> bool {
    let a = &object.metadata.annotations;
    a.get(ROOT_UID_ANNOTATION).is_some_and(|v| v == root_uid)
        && a.get(TRANSACTION_ID_ANNOTATION)
            .is_some_and(|v| v == transaction_id)
        && a.get(COMMAND_ID_ANNOTATION)
            .is_some_and(|v| v == command_id)
}

/// Recovers only a dropped create/patch acknowledgement. A collision without
/// the complete proof is never adopted. Delete and finalizer acknowledgements
/// intentionally cannot use this path because success cannot prove absence.
pub fn recover_dropped_write(
    observed: Option<&ObjectV0>,
    root_uid: &str,
    transaction_id: &str,
    command_id: &str,
) -> Result<ObjectV0, TransportErrorV0> {
    let Some(object) = observed else {
        return Err(TransportErrorV0::ResponseLost);
    };
    if !has_recovery_proof(object, root_uid, transaction_id, command_id) {
        return Err(TransportErrorV0::ForeignCollision);
    }
    Ok(object.clone())
}

fn recover_dropped_write_exact(
    observed: Option<&ObjectV0>,
    desired: &ObjectV0,
    root: &ObjectV0,
    expected_uid: Option<&str>,
    transaction_id: &str,
    command_id: &str,
) -> Result<ObjectV0, TransportErrorV0> {
    let object = recover_dropped_write(observed, &root.metadata.uid, transaction_id, command_id)?;
    if object.reference != desired.reference
        || expected_uid.is_some_and(|uid| object.metadata.uid != uid)
        || !has_exact_canonical_owner(&object, &root.reference, &root.metadata.uid)
        || !same_desired_semantics(&object.value, &desired.value)
    {
        return Err(TransportErrorV0::ForeignCollision);
    }
    Ok(object)
}
fn has_exact_canonical_owner(object: &ObjectV0, root: &ObjectRefV0, root_uid: &str) -> bool {
    object
        .value
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|owners| {
            owners.len() == 1
                && owners[0].as_object().is_some_and(|owner| {
                    owner.get("apiVersion").and_then(Value::as_str)
                        == Some(ResourceKindV0::WebApplication.api_version())
                        && owner.get("kind").and_then(Value::as_str)
                            == Some(ResourceKindV0::WebApplication.kind())
                        && owner.get("name").and_then(Value::as_str) == Some(root.name.as_str())
                        && owner.get("uid").and_then(Value::as_str) == Some(root_uid)
                        && owner.get("controller").and_then(Value::as_bool) == Some(true)
                        && owner.get("blockOwnerDeletion").and_then(Value::as_bool) == Some(false)
                })
        })
}
fn same_desired_semantics(observed: &Value, desired: &Value) -> bool {
    exact_metadata(observed, desired)
        && ["data", "binaryData", "immutable"]
            .iter()
            .all(|field| observed.get(*field) == desired.get(*field))
        && if observed.get("kind").and_then(Value::as_str) == Some("Service") {
            same_service_semantics(observed, desired)
        } else {
            observed.get("spec") == desired.get("spec")
        }
}

// Kubernetes allocates only these Service fields. Every other field remains
// exact so response-loss recovery cannot silently adopt a caller mismatch.
fn same_service_semantics(observed: &Value, desired: &Value) -> bool {
    if observed.get("kind").and_then(Value::as_str) != Some("Service") {
        return true;
    }
    let Some(desired) = canonical_service_spec(desired, None) else {
        return observed.get("spec").is_none();
    };
    canonical_service_spec(observed, Some(&desired)) == Some(desired)
}

/// Canonicalizes omission-only Kubernetes Service defaults. Allocations are
/// stripped solely from the observed side when the caller omitted them; an
/// explicit desired allocation must remain exact.
fn canonical_service_spec(value: &Value, desired: Option<&Value>) -> Option<Value> {
    let mut spec = value.get("spec")?.clone();
    let fields = spec.as_object_mut()?;
    fields
        .entry("type")
        .or_insert_with(|| serde_json::json!("ClusterIP"));
    fields
        .entry("sessionAffinity")
        .or_insert_with(|| serde_json::json!("None"));
    fields
        .entry("internalTrafficPolicy")
        .or_insert_with(|| serde_json::json!("Cluster"));
    if let Some(ports) = fields.get_mut("ports").and_then(Value::as_array_mut) {
        for port in ports {
            let Some(port) = port.as_object_mut() else {
                continue;
            };
            port.entry("protocol")
                .or_insert_with(|| serde_json::json!("TCP"));
            if !port.contains_key("targetPort")
                && let Some(number) = port.get("port").and_then(Value::as_i64)
            {
                port.insert("targetPort".into(), serde_json::json!(number));
            }
        }
    }
    if let Some(desired) = desired.and_then(|desired| desired.as_object()) {
        for field in [
            "clusterIP",
            "clusterIPs",
            "ipFamilies",
            "ipFamilyPolicy",
            "healthCheckNodePort",
        ] {
            if !desired.contains_key(field) {
                fields.remove(field);
            }
        }
    }
    Some(spec)
}

fn exact_metadata(observed: &Value, desired: &Value) -> bool {
    let map = |value: &Value, field: &str| {
        value
            .pointer(&format!("/metadata/{field}"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}))
    };
    map(observed, "labels") == map(desired, "labels")
        && map(observed, "annotations") == map(desired, "annotations")
}

fn may_recover_response_loss(error: &TransportErrorV0) -> bool {
    matches!(error, TransportErrorV0::Other(message)
        if !message.contains(" 4") && !message.contains(" 5") && !message.contains("code:"))
}

/// A process crash after a DELETE or finalizer request has no trustworthy
/// response proof. The reducer must receive an explicit Hold/manual recovery.
#[must_use]
pub const fn ambiguous_delete_or_finalizer() -> TransportErrorV0 {
    TransportErrorV0::ManualRecoveryHold
}

fn empty_state_resource_version_sentinel(command: &CommandV0) -> bool {
    let preconditions = match command {
        CommandV0::Persist { preconditions, .. }
        | CommandV0::PersistRootStatus { preconditions, .. }
        | CommandV0::RequestDescriptor { preconditions, .. }
        | CommandV0::RequestRender { preconditions }
        | CommandV0::RequestDeletionObservation { preconditions }
        | CommandV0::EnsureAutomationRun { preconditions, .. }
        | CommandV0::DeleteAutomationRun { preconditions, .. }
        | CommandV0::VerifyOwnedChildrenAbsent { preconditions, .. }
        | CommandV0::RemoveStateFinalizer { preconditions, .. }
        | CommandV0::RemoveRootFinalizer { preconditions, .. } => Some(preconditions),
        CommandV0::Mutate { commands, .. } => {
            return commands
                .iter()
                .any(|command| command.preconditions.state_resource_version.is_empty());
        }
        _ => None,
    };
    preconditions.is_some_and(|value| value.state_resource_version.is_empty())
}

/// Commands that change Kubernetes state must be fenced; non-Kubernetes core
/// requests intentionally do not enter this transport.
#[must_use]
pub fn command_requires_effect_fence(command: &CommandV0) -> bool {
    matches!(
        command,
        CommandV0::EnsureBootstrap { .. }
            | CommandV0::Persist { .. }
            | CommandV0::PersistRootStatus { .. }
            | CommandV0::Mutate { .. }
            | CommandV0::EnsureAutomationRun { .. }
            | CommandV0::DeleteAutomationRun { .. }
            | CommandV0::RemoveStateFinalizer { .. }
            | CommandV0::RemoveRootFinalizer { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct FakePort(Vec<ObjectV0>);

    impl DiscoveryPortV0 for FakePort {
        fn list_exact(&mut self, _: ResourceKindV0, _: &str) -> Result<ListV0, TransportErrorV0> {
            Ok(ListV0 {
                items: self.0.clone(),
                resource_version: "list-rv".into(),
            })
        }
        fn watch_from(
            &mut self,
            _: ResourceKindV0,
            _: &str,
            resource_version: &str,
        ) -> Result<Vec<WatchEventV0>, TransportErrorV0> {
            if resource_version == "gone" {
                Err(TransportErrorV0::WatchGone)
            } else {
                Ok(vec![])
            }
        }
        fn reread(
            &mut self,
            reference: &ObjectRefV0,
        ) -> Result<Option<ObjectV0>, TransportErrorV0> {
            Ok(self
                .0
                .iter()
                .find(|object| &object.reference == reference)
                .cloned())
        }
    }

    fn reference() -> ObjectRefV0 {
        ObjectRefV0 {
            resource: ResourceKindV0::ConfigMap,
            namespace: "ns".into(),
            name: "x".into(),
        }
    }
    fn object() -> ObjectV0 {
        ObjectV0 {
            reference: reference(),
            metadata: ObjectMetaV0 {
                uid: "u".into(),
                resource_version: "1".into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({}),
        }
    }

    #[test]
    fn rejects_gvk_namespace_and_metadata_smuggling() {
        let expected = reference();
        assert!(validate_desired_object(&json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"x","namespace":"other"}}), &expected).is_err());
        assert_eq!(
            validate_desired_object(
                &json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"x","namespace":"ns","uid":"smuggled"}}),
                &expected
            ),
            Err(TransportErrorV0::MetadataSmuggling)
        );
        assert_eq!(
            validate_desired_object(
                &json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"x","namespace":"ns","ownerReferences":[{"blockOwnerDeletion":true}]}}),
                &expected
            ),
            Err(TransportErrorV0::MetadataSmuggling)
        );
    }

    #[test]
    fn v0_service_validation_is_cluster_ip_only_before_any_write() {
        let expected = ObjectRefV0 {
            resource: ResourceKindV0::Service,
            namespace: "ns".into(),
            name: "x".into(),
        };
        let service = |spec: Value| json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"x","namespace":"ns"},"spec":spec});
        for spec in [
            json!({"ports":[{"port":80}]}),
            json!({"type":"ClusterIP","ports":[{"port":80}]}),
            json!({"type":"ClusterIP","clusterIP":"None","ports":[{"port":80}]}),
            json!({"type":"ClusterIP","clusterIP":"None","clusterIPs":["None"],"ports":[{"port":80}]}),
        ] {
            assert_eq!(validate_desired_object(&service(spec), &expected), Ok(()));
        }
        for spec in [
            json!({"type":"NodePort","ports":[{"port":80}]}),
            json!({"type":"LoadBalancer","ports":[{"port":80}]}),
            json!({"type":"ExternalName","externalName":"example.test"}),
            json!({"externalName":"example.test","ports":[{"port":80}]}),
            json!({"loadBalancerClass":"example.test/controller","ports":[{"port":80}]}),
            json!({"allocateLoadBalancerNodePorts":true,"ports":[{"port":80}]}),
            json!({"externalTrafficPolicy":"Local","ports":[{"port":80}]}),
            json!({"healthCheckNodePort":30000,"ports":[{"port":80}]}),
            json!({"ports":[{"port":80,"nodePort":30080}]}),
            json!({"clusterIP":"","ports":[{"port":80}]}),
            json!({"clusterIPs":[],"ports":[{"port":80}]}),
            json!({"clusterIPs":[""],"ports":[{"port":80}]}),
        ] {
            assert_eq!(
                validate_desired_object(&service(spec), &expected),
                Err(TransportErrorV0::UnsupportedTarget)
            );
        }
    }

    #[test]
    fn recovery_needs_all_three_exact_annotations() {
        let mut actual = object();
        actual
            .metadata
            .annotations
            .insert(ROOT_UID_ANNOTATION.into(), "root".into());
        actual
            .metadata
            .annotations
            .insert(TRANSACTION_ID_ANNOTATION.into(), "tx".into());
        actual
            .metadata
            .annotations
            .insert(COMMAND_ID_ANNOTATION.into(), "cmd".into());
        assert!(has_recovery_proof(&actual, "root", "tx", "cmd"));
        assert!(!has_recovery_proof(&actual, "root", "other", "cmd"));
    }

    #[test]
    fn exact_recovery_rejects_owner_metadata_immutable_and_ssa_uid_mismatches() {
        let root = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "root".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "root-uid".into(),
                resource_version: "1".into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({}),
        };
        let desired_value = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"x","namespace":"ns","labels":{"wanted":"yes"},"annotations":{"oxideloop.dev/root-uid":"root-uid","oxideloop.dev/transaction-id":"tx","oxideloop.dev/command-id":"cmd"},"ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"root","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},"data":{"x":"y"},"immutable":true});
        let desired = ObjectV0 {
            reference: reference(),
            metadata: object().metadata,
            value: desired_value.clone(),
        };
        let actual = |value: Value, uid: &str| ObjectV0 {
            reference: reference(),
            metadata: ObjectMetaV0 {
                uid: uid.into(),
                resource_version: "2".into(),
                annotations: BTreeMap::from([
                    (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
                    (TRANSACTION_ID_ANNOTATION.into(), "tx".into()),
                    (COMMAND_ID_ANNOTATION.into(), "cmd".into()),
                ]),
                owner_uids: vec!["root-uid".into()],
            },
            value,
        };
        let cases = [
            (
                "owner name",
                "/metadata/ownerReferences/0/name",
                json!("other"),
            ),
            (
                "owner kind",
                "/metadata/ownerReferences/0/kind",
                json!("Other"),
            ),
            (
                "owner controller",
                "/metadata/ownerReferences/0/controller",
                json!(false),
            ),
            (
                "owner block",
                "/metadata/ownerReferences/0/blockOwnerDeletion",
                json!(true),
            ),
            ("labels", "/metadata/labels/wanted", json!("no")),
            ("annotations", "/metadata/annotations/extra", json!("no")),
            ("immutable", "/immutable", json!(false)),
        ];
        for (name, pointer, replacement) in cases {
            let mut value = desired_value.clone();
            if name == "annotations" {
                value
                    .pointer_mut("/metadata/annotations")
                    .and_then(Value::as_object_mut)
                    .unwrap()
                    .insert("extra".into(), replacement);
            } else {
                *value.pointer_mut(pointer).unwrap() = replacement;
            }
            assert_eq!(
                recover_dropped_write_exact(
                    Some(&actual(value, "target")),
                    &desired,
                    &root,
                    None,
                    "tx",
                    "cmd"
                ),
                Err(TransportErrorV0::ForeignCollision),
                "{name}"
            );
        }
        let mut extra_owner = desired_value.clone();
        extra_owner.pointer_mut("/metadata/ownerReferences").unwrap().as_array_mut().unwrap().push(json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"root","uid":"root-uid","controller":true,"blockOwnerDeletion":false}));
        assert_eq!(
            recover_dropped_write_exact(
                Some(&actual(extra_owner, "target")),
                &desired,
                &root,
                None,
                "tx",
                "cmd"
            ),
            Err(TransportErrorV0::ForeignCollision)
        );
        assert_eq!(
            recover_dropped_write_exact(
                Some(&actual(desired_value, "wrong")),
                &desired,
                &root,
                Some("target"),
                "tx",
                "cmd"
            ),
            Err(TransportErrorV0::ForeignCollision)
        );
    }

    #[test]
    fn automation_recovery_requires_exact_canonical_owner() {
        let root = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "root".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "root-uid".into(),
                resource_version: "1".into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({}),
        };
        let desired = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::AutomationRun,
                namespace: "ns".into(),
                name: "run".into(),
            },
            metadata: ObjectMetaV0 {
                uid: String::new(),
                resource_version: String::new(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({"metadata":{"annotations":{"oxideloop.dev/root-uid":"root-uid","oxideloop.dev/transaction-id":"automation/v0","oxideloop.dev/command-id":"cmd"},"ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"root","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},"spec":{}}),
        };
        let mut observed = desired.clone();
        observed.metadata.annotations = BTreeMap::from([
            (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
            (TRANSACTION_ID_ANNOTATION.into(), "automation/v0".into()),
            (COMMAND_ID_ANNOTATION.into(), "cmd".into()),
        ]);
        *observed
            .value
            .pointer_mut("/metadata/ownerReferences/0/name")
            .unwrap() = json!("other");
        assert_eq!(
            recover_automation(observed, &desired, &root, "cmd"),
            Err(TransportErrorV0::ForeignCollision)
        );
    }

    #[test]
    fn service_response_loss_normalizes_only_omitted_defaults_and_allocations() {
        let service = |spec: Value| json!({"kind":"Service","spec":spec});
        let desired =
            service(json!({"selector":{"app":"demo"},"ports":[{"name":"http","port":80}]}));
        let observed = service(json!({
            "type":"ClusterIP", "sessionAffinity":"None", "internalTrafficPolicy":"Cluster",
            "clusterIP":"10.0.0.1", "clusterIPs":["10.0.0.1"], "ipFamilies":["IPv4"], "ipFamilyPolicy":"SingleStack",
            "selector":{"app":"demo"}, "ports":[{"name":"http","port":80,"protocol":"TCP","targetPort":80}]
        }));
        assert!(same_service_semantics(&observed, &desired));

        let cases = [
            (
                "protocol",
                service(
                    json!({"selector":{"app":"demo"},"ports":[{"name":"http","port":80,"protocol":"UDP","targetPort":80}]}),
                ),
            ),
            (
                "targetPort",
                service(
                    json!({"selector":{"app":"demo"},"ports":[{"name":"http","port":80,"protocol":"TCP","targetPort":81}]}),
                ),
            ),
            (
                "type",
                service(
                    json!({"type":"NodePort","selector":{"app":"demo"},"ports":[{"name":"http","port":80,"protocol":"TCP","targetPort":80}]}),
                ),
            ),
            (
                "sessionAffinity",
                service(
                    json!({"sessionAffinity":"ClientIP","selector":{"app":"demo"},"ports":[{"name":"http","port":80,"protocol":"TCP","targetPort":80}]}),
                ),
            ),
            (
                "internalTrafficPolicy",
                service(
                    json!({"internalTrafficPolicy":"Local","selector":{"app":"demo"},"ports":[{"name":"http","port":80,"protocol":"TCP","targetPort":80}]}),
                ),
            ),
            (
                "extra port",
                service(
                    json!({"selector":{"app":"demo"},"ports":[{"name":"http","port":80,"protocol":"TCP","targetPort":80},{"name":"https","port":443,"protocol":"TCP","targetPort":443}]}),
                ),
            ),
            (
                "observed nodePort",
                service(
                    json!({"selector":{"app":"demo"},"ports":[{"name":"http","port":80,"protocol":"TCP","targetPort":80,"nodePort":30080}]}),
                ),
            ),
        ];
        for (name, observed) in cases {
            assert!(!same_service_semantics(&observed, &desired), "{name}");
        }

        let explicit = service(
            json!({"clusterIP":"10.0.0.2","selector":{"app":"demo"},"ports":[{"name":"http","port":80}]}),
        );
        assert!(
            !same_service_semantics(&observed, &explicit),
            "explicit allocation"
        );
        let reordered = service(
            json!({"selector":{"app":"demo"},"ports":[{"name":"https","port":443,"protocol":"TCP","targetPort":443},{"name":"http","port":80,"protocol":"TCP","targetPort":80}]}),
        );
        assert!(
            !same_service_semantics(&reordered, &desired),
            "port reordering"
        );
    }

    #[tokio::test]
    async fn every_watch_error_is_terminal_before_queued_events() {
        for error in [
            TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal {
                stage: WatchStageV0::Stream,
                resource: ResourceKindV0::AutomationRun,
            }),
            TransportErrorV0::Other("malformed bookmark".into()),
        ] {
            let (sender, receiver) = tokio::sync::mpsc::channel(2);
            sender.send(Err(error.clone())).await.unwrap();
            sender
                .send(Ok(WatchEventV0::Bookmark {
                    resource_version: "2".into(),
                }))
                .await
                .unwrap();
            let mut watch = WatchHandleV0 {
                events: receiver,
                overflowed: Arc::new(AtomicBool::new(false)),
                terminal: None,
            };
            assert_eq!(watch.next_event().await, Err(error.clone()));
            assert_eq!(watch.next_event().await, Err(error));
        }
    }

    #[test]
    fn fence_rejects_stale_root_state_and_target() {
        let root = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "r".into(),
            },
            ..object()
        };
        let state = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::OxideLoopState,
                namespace: "ns".into(),
                name: "s".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "state".into(),
                resource_version: "2".into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({}),
        };
        let p = PreconditionsV0 {
            root_uid: "u".into(),
            root_generation: 1,
            root_resource_version: "1".into(),
            state_uid: "state".into(),
            state_resource_version: "2".into(),
            observation_token: "o".into(),
            cache_epoch: "e".into(),
        };
        let fence = EffectFenceV0 {
            root,
            state,
            target: Some(object()),
        };
        assert!(fence.validate(&p, Some(&reference())).is_ok());
        let mut stale_pins = p;
        stale_pins.state_resource_version = "3".into();
        assert_eq!(
            fence.validate(&stale_pins, Some(&reference())),
            Err(TransportErrorV0::Stale("state resourceVersion"))
        );
    }

    #[test]
    fn list_dto_retains_empty_collection_resource_version() {
        let list = ListV0 {
            items: vec![],
            resource_version: "987".into(),
        };
        assert_eq!(list.resource_version, "987");
    }

    #[test]
    fn base64_decode_allocates_exact_bounded_output() {
        let one = decode_base64("AA==", 1).expect("one byte");
        assert_eq!(one, vec![0]);
        assert_eq!(one.capacity(), 1);
        let three = decode_base64("AAH/", 3).expect("three bytes");
        assert_eq!(three, vec![0, 1, 255]);
        assert_eq!(three.capacity(), 3);
        assert_eq!(
            decode_base64("AAE=", 1),
            Err(TransportErrorV0::InvalidBundleData)
        );
    }

    #[test]
    fn rbac_inventory_is_exact_and_excludes_event_update_and_cluster_scope() {
        assert_eq!(
            required_rbac_v0(),
            &[
                RequiredRbacV0 {
                    resource: ResourceKindV0::WebApplication,
                    subresource: None,
                    verbs: &["get", "list", "watch", "patch"]
                },
                RequiredRbacV0 {
                    resource: ResourceKindV0::WebApplication,
                    subresource: Some("status"),
                    verbs: &["patch"]
                },
                RequiredRbacV0 {
                    resource: ResourceKindV0::OxideLoopState,
                    subresource: None,
                    verbs: &["get", "list", "watch", "create", "patch"]
                },
                RequiredRbacV0 {
                    resource: ResourceKindV0::AutomationRun,
                    subresource: None,
                    verbs: &["get", "list", "watch", "create", "delete"]
                },
                RequiredRbacV0 {
                    resource: ResourceKindV0::AutomationRun,
                    subresource: Some("status"),
                    verbs: &["patch"]
                },
                RequiredRbacV0 {
                    resource: ResourceKindV0::ConfigMap,
                    subresource: None,
                    verbs: &["get", "list", "watch", "create", "patch", "delete"]
                },
                RequiredRbacV0 {
                    resource: ResourceKindV0::Service,
                    subresource: None,
                    verbs: &["get", "list", "watch", "create", "patch", "delete"]
                },
                RequiredRbacV0 {
                    resource: ResourceKindV0::Event,
                    subresource: None,
                    verbs: &["get", "create"]
                },
            ]
        );
        let inventory = required_rbac_v0();
        assert!(inventory.iter().any(|entry| {
            entry.resource == ResourceKindV0::WebApplication
                && entry.subresource == Some("status")
                && entry.verbs == ["patch"]
        }));
        let event = inventory
            .iter()
            .find(|entry| entry.resource == ResourceKindV0::Event)
            .expect("event inventory");
        assert_eq!(event.verbs, ["get", "create"]);
        assert!(
            !event
                .verbs
                .iter()
                .any(|verb| { matches!(*verb, "list" | "watch" | "update" | "patch" | "delete") })
        );
    }

    #[test]
    fn event_target_is_exact_and_recovery_never_adopts_a_foreign_body() {
        assert_eq!(ResourceKindV0::Event.api_version(), "events.k8s.io/v1");
        assert_eq!(ResourceKindV0::Event.kind(), "Event");
        let plan = oxideloop_event_planner::PlannedEventV0 {
            api_version: "events.k8s.io/v1".into(),
            kind: "Event".into(),
            metadata: oxideloop_event_planner::EventMetadata {
                name: "event".into(),
                namespace: "ns".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: vec![],
            },
            action: "act".into(),
            reason: "reason".into(),
            note: "note".into(),
            event_type: "Normal".into(),
            reporting_controller: "controller".into(),
            reporting_instance: "instance".into(),
            regarding: oxideloop_event_planner::EventRegarding {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "source".into(),
                uid: "uid".into(),
                resource_version: "1".into(),
            },
        };
        let expected = materialize_event(&plan, "2026-01-01T00:00:00Z").unwrap();
        // A caller cannot forge a plan: missing planner-authenticated metadata
        // is a collision even if the rest of the Event looks plausible.
        assert_eq!(
            recover_event(&expected, &expected),
            Err(TransportErrorV0::ForeignCollision)
        );
        let foreign = json!({"apiVersion":"events.k8s.io/v1","kind":"Event"});
        assert_eq!(
            recover_event(&expected, &foreign),
            Err(TransportErrorV0::ForeignCollision)
        );
    }

    #[test]
    fn dropped_write_never_adopts_a_foreign_collision() {
        let mut actual = object();
        actual
            .metadata
            .annotations
            .insert(ROOT_UID_ANNOTATION.into(), "root".into());
        assert_eq!(
            recover_dropped_write(Some(&actual), "root", "tx", "cmd"),
            Err(TransportErrorV0::ForeignCollision)
        );
        assert_eq!(
            recover_dropped_write(None, "root", "tx", "cmd"),
            Err(TransportErrorV0::ResponseLost)
        );
    }

    #[test]
    fn delete_response_loss_is_a_manual_hold_not_success() {
        assert_eq!(
            ambiguous_delete_or_finalizer(),
            TransportErrorV0::ManualRecoveryHold
        );
        assert_eq!(
            TransportErrorV0::WatchGone.to_string(),
            "watch resource version expired (410); invalidate the cache epoch"
        );
        assert_eq!(
            TransportErrorV0::Conflict(ConflictProvenanceV0::Read {
                operation: ReadOperationV0::Get,
                resource: ResourceKindV0::ConfigMap,
            })
            .to_string(),
            "Kubernetes conflict (409) at Read { operation: Get, resource: ConfigMap }"
        );
        assert_eq!(
            map_kube_error("fake client reported 409 in display text"),
            TransportErrorV0::Other("fake client reported 409 in display text".into())
        );
    }

    #[test]
    fn conflict_mappers_preserve_stage_and_fake_409_text_is_other() {
        let api_error = |code| {
            kube::Error::Api(Box::new(kube::core::Status {
                status: Some(kube::core::response::StatusSummary::Failure),
                message: "conflict".into(),
                reason: "Conflict".into(),
                code,
                details: None,
                metadata: None,
            }))
        };
        assert_eq!(
            map_watch_open_error(api_error(409), ResourceKindV0::ConfigMap),
            TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal {
                stage: WatchStageV0::Open,
                resource: ResourceKindV0::ConfigMap,
            })
        );
        assert_eq!(
            map_kube_error("fake client reported 409 in display text"),
            TransportErrorV0::Other("fake client reported 409 in display text".into())
        );
    }

    #[test]
    fn reader_requires_exact_reread_barrier_pins() {
        let root_ref = ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: "ns".into(),
            name: "root".into(),
        };
        let state_ref = ObjectRefV0 {
            resource: ResourceKindV0::OxideLoopState,
            namespace: "ns".into(),
            name: "state".into(),
        };
        let root = ObjectV0 {
            reference: root_ref,
            ..object()
        };
        let state = ObjectV0 {
            reference: state_ref,
            metadata: ObjectMetaV0 {
                uid: "state".into(),
                resource_version: "2".into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({}),
        };
        let p = PreconditionsV0 {
            root_uid: "u".into(),
            root_generation: 1,
            root_resource_version: "1".into(),
            state_uid: "state".into(),
            state_resource_version: "2".into(),
            observation_token: "o".into(),
            cache_epoch: "epoch".into(),
        };
        let mut port = FakePort(vec![root, state]);
        let mut reader = FenceReaderV0::new(
            &mut port,
            RootAddressV0 {
                namespace: "ns".into(),
                name: "root".into(),
                state_name: "state".into(),
            },
        );
        assert!(reader.reread_fence(&p, None).is_ok());
        let mut stale_pins = p;
        stale_pins.root_resource_version = "old".into();
        assert_eq!(
            reader.reread_fence(&stale_pins, None),
            Err(TransportErrorV0::Stale("root resourceVersion"))
        );
    }
}

#[cfg(test)]
mod wire_tests;
