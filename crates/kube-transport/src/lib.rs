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

// Bootstrap diagnostics deliberately travel only through the runtime's
// debug-only test-failpoint feature. They are not part of the production
// transport surface, where a failure must remain the unadorned domain error.
#[cfg(all(feature = "bootstrap-diagnostics", not(debug_assertions)))]
compile_error!("bootstrap diagnostics are only available in debug builds");

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use oxideloop_controller_api::{
    ApplyOperation, ApplyOutcome, ApplyResult, AutomationPending, AutomationRunCompletionState,
    AutomationRunSpec, AutomationRunStatus, DeletionCleanupReceipt, DurablePhase,
    GenerationAdvanceClosurePurpose, MAX_MESSAGE_BYTES, NextCommandLifecycleStateV0,
    ObjectIdentity, OxideLoopStateSpec, PersistPurpose, ROOT_FINALIZER, STATE_FINALIZER, StateLink,
    WebApplicationStatus, validate_automation_run_name,
};
use oxideloop_controller_core::{
    CommandV0, MutationCommandV0, PreconditionsV0, linked_root_finalizer_command_token,
    linked_state_finalizer_command_token, retired_state_delete_command_token,
    root_only_finalizer_command_token, root_only_owned_children_verify_token,
};
#[cfg(test)]
use oxideloop_event_planner::semantically_equal;
use oxideloop_event_planner::{PlannedEventV0, materialize_event};
use oxideloop_mutation_model::{
    APPLY_PATCH_CONTENT_TYPE, CONTROLLER_FIELD_MANAGER, HttpMethod, MutationDecision,
};
use oxideloop_renderer_abi::{canonical_bytes, sha256_hex};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{oneshot, watch};

use kube::{
    Api, Client, Config,
    api::{DeleteParams, ListParams, Patch, PatchParams, Preconditions},
    core::{ApiResource, DynamicObject},
};

pub const ROOT_UID_ANNOTATION: &str = "runtime.oxideloop.dev/root-uid";
pub const TRANSACTION_ID_ANNOTATION: &str = "runtime.oxideloop.dev/transaction-id";
pub const COMMAND_ID_ANNOTATION: &str = "runtime.oxideloop.dev/command-id";
/// Immutable v1 bootstrap authority persisted on the root before its State
/// link can be installed.  These annotations bind the unlinked State that a
/// bootstrap invocation created to the exact root command that created it;
/// they are intentionally not inferred from a same-name State object.
pub const BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION: &str =
    "runtime.oxideloop.dev/bootstrap-authority-version";
pub const BOOTSTRAP_STATE_NAME_ANNOTATION: &str = "runtime.oxideloop.dev/bootstrap-state-name";
pub const BOOTSTRAP_STATE_UID_ANNOTATION: &str = "runtime.oxideloop.dev/bootstrap-state-uid";
pub const BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION: &str =
    "runtime.oxideloop.dev/bootstrap-state-spec-digest";
pub const BOOTSTRAP_PROOF_TOKEN_ANNOTATION: &str = "runtime.oxideloop.dev/bootstrap-proof-token";
pub const BOOTSTRAP_AUTHORITY_VERSION_V1: &str = "v1";
const BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1: &[u8] = b"oxideloop.bootstrap-authority/v1\0";
const MAX_BUNDLE_IDENTITY_BYTES: usize = 253;
#[cfg(not(test))]
const OPERATION_DEADLINE: Duration = Duration::from_secs(15);
#[cfg(test)]
const OPERATION_DEADLINE: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const WRITE_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
#[cfg(test)]
const WRITE_RESPONSE_DEADLINE: Duration = Duration::from_millis(85);
#[cfg(not(test))]
const PREWRITE_DEADLINE: Duration = Duration::from_secs(9);
#[cfg(test)]
const PREWRITE_DEADLINE: Duration = Duration::from_millis(75);
#[cfg(not(test))]
const ATTEMPT_PREWRITE_CUTOFF: Duration = Duration::from_secs(7);
#[cfg(test)]
const ATTEMPT_PREWRITE_CUTOFF: Duration = Duration::from_millis(50);
const MAX_PREWRITE_RETRIES: u8 = 4;

/// Immutable, transport-minted timing authority for one logical Kubernetes
/// command. Its deadlines are intentionally opaque outside this crate: a
/// caller may carry it into a collector session but cannot lengthen or reset
/// any deadline.
#[derive(Clone, Debug)]
pub struct OperationBudgetV0 {
    inner: Arc<OperationBudgetInnerV0>,
}

#[derive(Debug)]
struct OperationBudgetInnerV0 {
    attempt_prewrite_cutoffs: Box<[Instant]>,
    prewrite_deadline: Instant,
    write_response_deadline: Instant,
    operation_deadline: Instant,
    max_prewrite_retries: u8,
}

impl OperationBudgetV0 {
    fn production() -> Self {
        Self::from_durations(
            ATTEMPT_PREWRITE_CUTOFF,
            PREWRITE_DEADLINE,
            WRITE_RESPONSE_DEADLINE,
            OPERATION_DEADLINE,
            MAX_PREWRITE_RETRIES,
        )
        .expect("production operation budget is ordered")
    }

    fn from_durations(
        attempt_prewrite_cutoff: Duration,
        prewrite_deadline: Duration,
        write_response_deadline: Duration,
        operation_deadline: Duration,
        max_prewrite_retries: u8,
    ) -> Result<Self, &'static str> {
        if !(attempt_prewrite_cutoff < prewrite_deadline
            && prewrite_deadline < write_response_deadline
            && write_response_deadline < operation_deadline)
        {
            return Err("operation deadlines must be strictly ordered");
        }
        let now = Instant::now();
        // The retry schedule is frozen with the outer budget.  A pre-write
        // retry may move only to the next slot; it never creates a new
        // operation deadline.
        let retry_slots = usize::from(max_prewrite_retries) + 1;
        let remaining = prewrite_deadline
            .checked_sub(attempt_prewrite_cutoff)
            .expect("validated strictly ordered prewrite cutoffs");
        let step = remaining / u32::try_from(retry_slots).expect("retry slots fit u32");
        let attempt_prewrite_cutoffs = (0..retry_slots)
            .map(|slot| {
                now + attempt_prewrite_cutoff + step * u32::try_from(slot).expect("slot fits u32")
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self {
            inner: Arc::new(OperationBudgetInnerV0 {
                attempt_prewrite_cutoffs,
                prewrite_deadline: now + prewrite_deadline,
                write_response_deadline: now + write_response_deadline,
                operation_deadline: now + operation_deadline,
                max_prewrite_retries,
            }),
        })
    }

    fn is_strictly_ordered(&self) -> bool {
        let inner = &self.inner;
        inner
            .attempt_prewrite_cutoffs
            .iter()
            .all(|cutoff| *cutoff < inner.prewrite_deadline)
            && inner.prewrite_deadline < inner.write_response_deadline
            && inner.write_response_deadline < inner.operation_deadline
    }

    fn max_prewrite_retries(&self) -> u8 {
        self.inner.max_prewrite_retries
    }

    fn prewrite_cutoff(&self, epoch: u32) -> Option<Instant> {
        self.inner
            .attempt_prewrite_cutoffs
            .get(usize::try_from(epoch).ok()?)
            .copied()
    }

    #[cfg(test)]
    fn test_shortened(
        attempt_prewrite_cutoff: Duration,
        prewrite_deadline: Duration,
        write_response_deadline: Duration,
        operation_deadline: Duration,
        max_prewrite_retries: u8,
    ) -> Result<Self, &'static str> {
        Self::from_durations(
            attempt_prewrite_cutoff,
            prewrite_deadline,
            write_response_deadline,
            operation_deadline,
            max_prewrite_retries,
        )
    }
}

/// Collector-visible phase of a transport command before its first write has
/// been polled. The only legal transitions are one-way and atomically
/// arbitrated between the collector and transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrewriteGateStateV0 {
    /// A collected bootstrap has sealed its immutable command/address but has
    /// not yet closed its list/watch/reread baseline.  It is deliberately
    /// non-writable.
    BaselinePending,
    PreWrite,
    RootAuthPending,
    Invalidated,
    WritePolled,
    Proven,
    AmbiguousHeld,
    BootstrapPhasePending,
    /// The final root State-link response has been decoded, but its exact raw
    /// root echo has not yet been consumed from the retained bootstrap watch.
    /// This is non-writable and is not an acknowledgement state.
    FinalProofPending,
}

/// Provenance for a deadline on a finite Kubernetes request. Retained watch
/// reads are deliberately absent: only opening a watch is finite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FiniteIoStageV0 {
    ReadOnly,
    WatchOpen,
    PreWrite,
    WriteResponse,
    RecoveryProof,
}

#[derive(Clone, Debug)]
pub struct PrewriteGateV0 {
    state: Arc<AtomicU64>,
    /// Versioned phase publication closes the state-read/wait-registration
    /// race. Unlike a broadcast-only `Notify`, a subscriber observes a
    /// transition that happened after it subscribed even if it starts waiting
    /// slightly later.
    wake: watch::Sender<u64>,
}

impl PrewriteGateV0 {
    const PREWRITE: u8 = 0;
    const ROOT_AUTH_PENDING: u8 = 1;
    const INVALIDATED: u8 = 2;
    const WRITE_POLLED: u8 = 3;
    const PROVEN: u8 = 4;
    const AMBIGUOUS_HELD: u8 = 5;
    const BOOTSTRAP_PHASE_PENDING: u8 = 6;
    const BASELINE_PENDING: u8 = 7;
    const FINAL_PROOF_PENDING: u8 = 8;

    fn new() -> Self {
        let (wake, _wake_observer) = watch::channel(0_u64);
        Self {
            state: Arc::new(AtomicU64::new(Self::encode(0, Self::PREWRITE))),
            wake,
        }
    }

    /// Records an already-authenticated observation change while the command
    /// remains unpolled. Raw root watch records deliberately do not call this
    /// method: T2 authenticates them by an exact root reload so self-echoes
    /// cannot cancel a command.
    #[doc(hidden)]
    #[must_use]
    pub fn invalidate_authenticated_change(&self) -> bool {
        self.transition_current(Self::PREWRITE, Self::INVALIDATED)
    }

    #[doc(hidden)]
    pub fn try_mark_write_polled(&self) -> Result<(), PrewriteGateStateV0> {
        self.transition_current(Self::PREWRITE, Self::WRITE_POLLED)
            .then_some(())
            .ok_or_else(|| self.state())
    }

    #[doc(hidden)]
    #[must_use]
    pub fn arm_bootstrap_baseline(&self) -> bool {
        self.transition_current(Self::BASELINE_PENDING, Self::PREWRITE)
    }

    #[must_use]
    pub fn state(&self) -> PrewriteGateStateV0 {
        Self::decode(Self::phase(self.state.load(Ordering::Acquire)))
    }

    #[doc(hidden)]
    #[must_use]
    pub fn epoch(&self) -> u32 {
        Self::decode_epoch(self.state.load(Ordering::Acquire))
    }

    /// Reserves the current epoch while the runtime authenticates a root
    /// watch record.  A write cannot win this intermediate state.
    #[doc(hidden)]
    #[must_use]
    pub fn begin_root_authentication(&self) -> Option<u32> {
        let observed = self.state.load(Ordering::Acquire);
        let phase = Self::phase(observed);
        let transitioned = matches!(phase, Self::PREWRITE | Self::BOOTSTRAP_PHASE_PENDING)
            && self
                .state
                .compare_exchange(
                    observed,
                    Self::encode(Self::decode_epoch(observed), Self::ROOT_AUTH_PENDING),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
        if transitioned {
            self.notify_transition();
            Some(Self::decode_epoch(observed))
        } else {
            None
        }
    }

    /// A confirmed root change closes the current epoch permanently.
    #[doc(hidden)]
    #[must_use]
    pub fn invalidate_root_authentication(&self, epoch: u32) -> bool {
        self.transition_epoch(epoch, Self::ROOT_AUTH_PENDING, Self::INVALIDATED)
    }

    /// An exact self echo starts a *new* fixed attempt epoch. It never
    /// reopens the old state or resets the outer deadline.
    #[doc(hidden)]
    #[must_use]
    pub fn restart_after_exact_self_echo(&self, epoch: u32, max_epoch: u32) -> bool {
        if epoch >= max_epoch {
            return false;
        }
        let transitioned = self
            .state
            .compare_exchange(
                Self::encode(epoch, Self::ROOT_AUTH_PENDING),
                // An owned bootstrap echo never opens the next prewrite
                // epoch directly.  The collector must first drain and
                // authenticate anything that arrived during the exact-root
                // reload before the restarted command can poll its next
                // subwrite.
                Self::encode(epoch + 1, Self::BOOTSTRAP_PHASE_PENDING),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if transitioned {
            self.notify_transition();
        }
        transitioned
    }

    #[doc(hidden)]
    pub async fn wait_for_bootstrap_phase_pending(&self) {
        loop {
            // Subscribe before inspecting the state. `changed()` is retained
            // by this receiver, so the following phase read and wait form one
            // lossless observation cycle.
            let mut notified = self.wake.subscribe();
            if self.state() == PrewriteGateStateV0::BootstrapPhasePending {
                return;
            }
            let _ = notified.changed().await;
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn seal_bootstrap_phase(&self) -> bool {
        self.transition_current(Self::BOOTSTRAP_PHASE_PENDING, Self::PREWRITE)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn invalidate_bootstrap_phase(&self) -> bool {
        self.transition_current(Self::BOOTSTRAP_PHASE_PENDING, Self::INVALIDATED)
    }

    fn notify_transition(&self) {
        let next = (*self.wake.borrow()).wrapping_add(1);
        self.wake.send_replace(next);
    }

    fn transition_current(&self, from: u8, to: u8) -> bool {
        let observed = self.state.load(Ordering::Acquire);
        let transitioned = Self::phase(observed) == from
            && self
                .state
                .compare_exchange(
                    observed,
                    Self::encode(Self::decode_epoch(observed), to),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
        if transitioned {
            self.notify_transition();
        }
        transitioned
    }

    fn transition_epoch(&self, epoch: u32, from: u8, to: u8) -> bool {
        let transitioned = self
            .state
            .compare_exchange(
                Self::encode(epoch, from),
                Self::encode(epoch, to),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if transitioned {
            self.notify_transition();
        }
        transitioned
    }

    const fn encode(epoch: u32, phase: u8) -> u64 {
        ((epoch as u64) << 8) | phase as u64
    }

    fn decode_epoch(value: u64) -> u32 {
        u32::try_from(value >> 8).expect("operation epoch is packed into 32 bits")
    }

    fn phase(value: u64) -> u8 {
        u8::try_from(value & 0xff).expect("operation phase is packed into 8 bits")
    }

    fn decode(state: u8) -> PrewriteGateStateV0 {
        match state {
            Self::PREWRITE => PrewriteGateStateV0::PreWrite,
            Self::ROOT_AUTH_PENDING => PrewriteGateStateV0::RootAuthPending,
            Self::INVALIDATED => PrewriteGateStateV0::Invalidated,
            Self::WRITE_POLLED => PrewriteGateStateV0::WritePolled,
            Self::PROVEN => PrewriteGateStateV0::Proven,
            Self::AMBIGUOUS_HELD => PrewriteGateStateV0::AmbiguousHeld,
            Self::BOOTSTRAP_PHASE_PENDING => PrewriteGateStateV0::BootstrapPhasePending,
            Self::BASELINE_PENDING => PrewriteGateStateV0::BaselinePending,
            Self::FINAL_PROOF_PENDING => PrewriteGateStateV0::FinalProofPending,
            _ => unreachable!("prewrite gate state is sealed"),
        }
    }
}

/// Opaque transport session carried by the exact pinned command future and
/// its collector. Bootstrap and direct transport callers receive the same
/// operation-owned session but have no collector that can invalidate it.
#[derive(Clone, Debug)]
pub struct OperationSessionV0 {
    budget: OperationBudgetV0,
    gate: PrewriteGateV0,
    /// The bootstrap fence is process-local by design.  It never crosses a
    /// restart as authority; a recovered controller must derive any durable
    /// continuation from the v1 root metadata bundle instead.
    bootstrap_fence: Arc<Mutex<BootstrapFenceStateV0>>,
    bootstrap_fence_wake: watch::Sender<u64>,
    /// The operation-owned half of the retained root watch.  It is installed
    /// only for a sealed collected bootstrap and owns the forwarding pump's
    /// close handshake.  Keeping it separate from `WatchHandleV0` lets a
    /// completed operation hand the original retained watch back intact.
    bootstrap_pump: Arc<BootstrapPumpRuntimeV0>,
    bootstrap: Arc<Mutex<BootstrapAuthorityV0>>,
    collector_bound: bool,
    #[cfg(test)]
    bootstrap_phase_wait_hook: Option<Arc<BootstrapPhaseWaitHookV0>>,
}

#[cfg(test)]
#[derive(Debug)]
struct BootstrapPhaseWaitHookV0 {
    state_read: AtomicBool,
    release: Arc<tokio::sync::Barrier>,
}

/// The only root-write sequence whose individual writes may advance one
/// operation session.  Keeping the phase explicit makes an old root watch
/// echo distinguishable from a real root transition without granting a
/// generic "status changed" exemption to bootstrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapRootPhaseV0 {
    StatePost,
    RootMetadata,
    StateFinalizer,
    RootStateLink,
    FinalProofPending,
    Complete,
}

/// Lifecycle of the one bootstrap sequence owned by a collected operation.
/// The sealed pre-poll state is intentionally distinct from an uninitialized
/// authority: only the collector can install it for its exact freshly minted
/// `EnsureBootstrap` session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapAuthorityPhaseV0 {
    BootstrapNotYetPolled,
    Active(BootstrapRootPhaseV0),
}

/// The immutable raw baseline for one collected bootstrap.  Keeping raw
/// domain objects (rather than their reduced `RootSnapshotV0` projection)
/// prevents an annotation, finalizer, status, managed-field, or UID mutation
/// from being mistaken for the list/reread baseline.
#[derive(Clone, Debug)]
struct ObservedBootstrapBaselineV0 {
    root: ObjectV0,
    state: Option<ObjectV0>,
}

/// Process-local, sequenced bootstrap-watch state.  The driver owns the
/// actual retained root handle and calls `observe_bootstrap_root_watch` before
/// it can publish a root fact to the actor.  This lock is intentionally first
/// in every bootstrap transition; completion then takes the bootstrap lock
/// and finally performs the gate CAS, so a terminal/event and a write claim
/// have one linearization order.
// These are deliberately separate rather than a single lifecycle enum: a
// fence can be armed while the pump is inactive only in test setup, and final
// echo consumption is independent from both the pump and the arm/disarm bit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
struct BootstrapFenceStateV0 {
    command: Option<CommandV0>,
    address: Option<RootAddressV0>,
    baseline: Option<ObservedBootstrapBaselineV0>,
    /// Records committed by the independently polled fence pump but not yet
    /// consumed by the collector.  A consumer must match the next sequence
    /// exactly; raw values arriving outside this queue are never bootstrap
    /// authority.
    records: VecDeque<BootstrapFenceRecordV0>,
    /// Data records consumed while a write response is in flight.  They retain
    /// their sequence order until that response can classify the exact echo.
    pending: VecDeque<BootstrapFenceRecordV0>,
    terminal: Option<TransportErrorV0>,
    armed: bool,
    disarmed: bool,
    pump_active: bool,
    /// The forwarding task has made one real attempt to poll the upstream
    /// exact-name watch.  `pump_active` only means the task was installed;
    /// it is not freshness evidence.  This becomes true only after the first
    /// `try_next_event` returned Pending, or after a ready record/terminal
    /// was committed.  A zero-record watermark before this edge is invalid.
    first_poll_complete: bool,
    committed: u64,
    enqueued: u64,
    consumed: u64,
    metadata_echo: Option<ObjectV0>,
    /// A single sequenced record may acknowledge the exact `RootMetadata`
    /// response.  It is intentionally independent of publication: recording
    /// an HTTP response does not consume a watch record.
    metadata_echo_consumed: bool,
    /// Exact State-finalizer PATCH response. Final proof requires this full
    /// raw object unchanged; UID/digest alone would permit ownership, token,
    /// deletion, or resource-version drift between the finalizer write and
    /// bootstrap acknowledgement.
    final_state: Option<ObjectV0>,
    final_echo: Option<ObjectV0>,
    final_echo_consumed: bool,
}

#[derive(Debug)]
enum BootstrapFenceRecordKindV0 {
    Data(WatchEventV0),
    Terminal(TransportErrorV0),
}

#[derive(Debug)]
struct BootstrapFenceRecordV0 {
    sequence: u64,
    kind: BootstrapFenceRecordKindV0,
}

/// The single owner of the operation-specific forwarding task.  A regular
/// root watch long outlives one Kubernetes command, so graceful close returns
/// its upstream handle to the caller instead of aborting the root watch.
#[derive(Debug)]
struct BootstrapPumpControlV0 {
    close: oneshot::Sender<()>,
    returned_watch: oneshot::Receiver<WatchHandleV0>,
    abort: tokio::task::AbortHandle,
    join: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
struct BootstrapPumpRuntimeV0 {
    fence: Arc<Mutex<BootstrapFenceStateV0>>,
    gate: PrewriteGateV0,
    wake: watch::Sender<u64>,
    control: Mutex<Option<BootstrapPumpControlV0>>,
}

impl BootstrapFenceStateV0 {
    fn new() -> Self {
        Self {
            command: None,
            address: None,
            baseline: None,
            records: VecDeque::new(),
            pending: VecDeque::new(),
            terminal: None,
            armed: false,
            disarmed: false,
            pump_active: false,
            first_poll_complete: false,
            committed: 0,
            enqueued: 0,
            consumed: 0,
            metadata_echo: None,
            metadata_echo_consumed: false,
            final_state: None,
            final_echo: None,
            final_echo_consumed: false,
        }
    }

    fn same_sealed_input(&self, address: &RootAddressV0, command: &CommandV0) -> bool {
        self.command.as_ref() == Some(command) && self.address.as_ref() == Some(address)
    }

    fn commit(&mut self, kind: BootstrapFenceRecordKindV0) -> Result<u64, TransportErrorV0> {
        let sequence = self
            .committed
            .checked_add(1)
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        self.committed = sequence;
        self.records
            .push_back(BootstrapFenceRecordV0 { sequence, kind });
        Ok(sequence)
    }

    fn mark_enqueued(&mut self, sequence: u64) -> Result<(), TransportErrorV0> {
        if self.enqueued.checked_add(1) != Some(sequence) || sequence > self.committed {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        self.enqueued = sequence;
        Ok(())
    }

    fn take_next_data(&mut self, event: &WatchEventV0) -> Result<BootstrapFenceRecordV0, ()> {
        let Some(record) = self.records.pop_front() else {
            return Err(());
        };
        if record.sequence != self.consumed.saturating_add(1)
            || record.sequence > self.enqueued
            || !matches!(&record.kind, BootstrapFenceRecordKindV0::Data(expected) if expected == event)
        {
            return Err(());
        }
        self.consumed = record.sequence;
        Ok(record)
    }

    fn take_next_terminal(&mut self, error: &TransportErrorV0) -> Result<(), ()> {
        let Some(record) = self.records.pop_front() else {
            return Err(());
        };
        if record.sequence != self.consumed.saturating_add(1)
            || record.sequence > self.enqueued
            || !matches!(&record.kind, BootstrapFenceRecordKindV0::Terminal(expected) if expected == error)
        {
            return Err(());
        }
        self.consumed = record.sequence;
        Ok(())
    }

    fn all_committed_records_consumed(&self) -> bool {
        self.committed == self.enqueued && self.enqueued == self.consumed && self.records.is_empty()
    }

    /// Resolves records held while the current PATCH was already in flight.
    /// A root watch can precede its HTTP response, so only the response owner
    /// is allowed to decide whether the raw record was that response's echo.
    /// Any other record is sticky evidence that no subsequent bootstrap phase
    /// may be authorized.
    fn classify_write_polled_records(
        &mut self,
        phase: BootstrapRootPhaseV0,
        response: &ObjectV0,
    ) -> bool {
        let pending = std::mem::take(&mut self.pending);
        for record in pending {
            let event = match record.kind {
                BootstrapFenceRecordKindV0::Data(event) => event,
                BootstrapFenceRecordKindV0::Terminal(error) => {
                    self.terminal.get_or_insert(error);
                    return false;
                }
            };
            if phase == BootstrapRootPhaseV0::RootStateLink {
                match event {
                    WatchEventV0::Modified(observed)
                        if bootstrap_response_matches(&observed, response) =>
                    {
                        self.final_echo_consumed = true;
                        continue;
                    }
                    WatchEventV0::Modified(observed)
                        if !self.metadata_echo_consumed
                            && !self.final_echo_consumed
                            && self.metadata_echo.as_ref().is_some_and(|response| {
                                bootstrap_response_matches(&observed, response)
                            }) =>
                    {
                        self.metadata_echo_consumed = true;
                        continue;
                    }
                    WatchEventV0::Bookmark { .. } => continue,
                    WatchEventV0::Added(_)
                    | WatchEventV0::Modified(_)
                    | WatchEventV0::Deleted(_) => {
                        self.terminal
                            .get_or_insert(TransportErrorV0::PreWriteInvalidated);
                        return false;
                    }
                }
            }
            match event {
                WatchEventV0::Bookmark { .. } => {}
                WatchEventV0::Modified(observed)
                    if !self.metadata_echo_consumed
                        && self.metadata_echo.as_ref().is_some_and(|response| {
                            bootstrap_response_matches(&observed, response)
                        }) =>
                {
                    self.metadata_echo_consumed = true;
                }
                WatchEventV0::Added(observed) | WatchEventV0::Modified(observed)
                    if self.baseline.as_ref().is_some_and(|baseline| {
                        bootstrap_response_matches(&observed, &baseline.root)
                    }) => {}
                WatchEventV0::Added(_) | WatchEventV0::Modified(_) | WatchEventV0::Deleted(_) => {
                    self.terminal
                        .get_or_insert(TransportErrorV0::PreWriteInvalidated);
                    return false;
                }
            }
        }
        true
    }
}

fn notify_bootstrap_fence(sender: &watch::Sender<u64>) {
    let next = (*sender.borrow()).wrapping_add(1);
    sender.send_replace(next);
}

/// Marks the real first-upstream-poll edge after a `try_next_event` found no
/// ready record. A ready record/terminal sets this bit in the same fence-lock
/// critical section that commits it instead. This is intentionally separate
/// from task installation: a spawned-but-never-polled task cannot authorize
/// the zero watermark.
fn complete_bootstrap_first_poll(
    fence: &Arc<Mutex<BootstrapFenceStateV0>>,
    wake: &watch::Sender<u64>,
) {
    if let Ok(mut fence) = fence.lock()
        && fence.pump_active
        && !fence.disarmed
    {
        fence.first_poll_complete = true;
    }
    notify_bootstrap_fence(wake);
}

/// A bootstrap echo is deliberately stronger than a shaped root snapshot
/// comparison. Its raw UID/resourceVersion and every authority-bearing field
/// must match the write response. Kubernetes owns only `managedFields`, whose
/// watch serialization can differ from the PATCH response without changing
/// the stored object or granting a concurrent writer any authority.
fn bootstrap_response_matches(observed: &ObjectV0, response: &ObjectV0) -> bool {
    !response.metadata.uid.is_empty()
        && !response.metadata.resource_version.is_empty()
        && observed.reference == response.reference
        && observed.metadata.uid == response.metadata.uid
        && observed.metadata.resource_version == response.metadata.resource_version
        && raw_metadata_pins_match(observed)
        && raw_metadata_pins_match(response)
        && bootstrap_response_projection(&observed.value)
            == bootstrap_response_projection(&response.value)
}

/// An operation-owned bootstrap watch is an exact-name capability.  Keep the
/// target test beside the consumer as a second line of defence for scripted
/// handles and future pump changes: a foreign resource, namespace, or name
/// is terminal evidence, never a record that the driver may skip.
fn bootstrap_watch_targets_address(event: &WatchEventV0, address: &RootAddressV0) -> bool {
    matches!(
        event,
        WatchEventV0::Added(object) | WatchEventV0::Modified(object) | WatchEventV0::Deleted(object)
            if object.reference.resource == ResourceKindV0::WebApplication
                && object.reference.namespace == address.namespace
                && object.reference.name == address.name
    )
}

/// The post-close bridge retains the exact-name capability until the ordinary
/// retained watch has taken over. A bookmark is deliberately harmless here:
/// named bootstrap watches request bookmarks disabled, and a defensive one
/// carries no object identity that could be mistaken for a root transition.
fn bootstrap_bridge_event_targets_address(event: &WatchEventV0, address: &RootAddressV0) -> bool {
    matches!(event, WatchEventV0::Bookmark { .. })
        || bootstrap_watch_targets_address(event, address)
}

fn bootstrap_bridge_record_targets_address(
    record: &Result<WatchEventV0, TransportErrorV0>,
    address: &RootAddressV0,
) -> bool {
    match record {
        Ok(event) => bootstrap_bridge_event_targets_address(event, address),
        // Upstream terminal evidence is already a retained-watch rebarrier;
        // it has no object identity to validate.
        Err(_) => true,
    }
}

/// The State side of the final proof is deliberately at least as strict as
/// deletion authority.  The finalizer response fixes its entire raw object,
/// including owner references, both proof tokens, root UID annotation and
/// resourceVersion.  A later State mutation is not a harmless reread drift:
/// it invalidates bootstrap acknowledgement.
fn final_bootstrap_state_matches(
    root: &ObjectV0,
    state: &ObjectV0,
    expected: &ObjectV0,
    token: &str,
) -> bool {
    state == expected
        && has_exact_object_identity(
            state,
            ResourceKindV0::OxideLoopState,
            &root.reference.namespace,
            &expected.reference.name,
        )
        && !state.metadata.uid.is_empty()
        && !state.metadata.resource_version.is_empty()
        && state
            .value
            .pointer("/metadata/deletionTimestamp")
            .is_none_or(|value| value.as_str().is_none_or(str::is_empty))
        && has_finalizer(state, STATE_FINALIZER)
        && has_exact_canonical_owner(state, &root.reference, &root.metadata.uid)
        && has_recovery_proof(state, &root.metadata.uid, token, token)
        && bootstrap_root_metadata_matches_state(root, state, token)
        && has_state_link(root, state)
}

/// Latches a terminal observation while the fence lock is held.  The gate CAS
/// deliberately does not notify here: callers must release every fence lock
/// before publishing either wakeup or pump-close signal.
fn latch_bootstrap_terminal_locked(
    fence: &mut BootstrapFenceStateV0,
    gate: &PrewriteGateV0,
    error: TransportErrorV0,
) -> bool {
    // A proven operation has atomically disarmed the fence before its
    // retained watch is handed back.  Subsequent retained-watch failures are
    // still delivered through that ordinary watch, but cannot retroactively
    // mutate the completed operation or reopen its counters.
    if fence.disarmed {
        return false;
    }
    fence.terminal.get_or_insert(error);
    let encoded = gate.state.load(Ordering::Acquire);
    if matches!(
        PrewriteGateV0::decode(PrewriteGateV0::phase(encoded)),
        PrewriteGateStateV0::BaselinePending
            | PrewriteGateStateV0::PreWrite
            | PrewriteGateStateV0::BootstrapPhasePending
            | PrewriteGateStateV0::RootAuthPending
    ) {
        gate.state
            .compare_exchange(
                encoded,
                PrewriteGateV0::encode(
                    PrewriteGateV0::decode_epoch(encoded),
                    PrewriteGateV0::INVALIDATED,
                ),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    } else {
        false
    }
}

impl BootstrapPumpRuntimeV0 {
    fn new(
        fence: Arc<Mutex<BootstrapFenceStateV0>>,
        gate: PrewriteGateV0,
        wake: watch::Sender<u64>,
    ) -> Self {
        Self {
            fence,
            gate,
            wake,
            control: Mutex::new(None),
        }
    }

    fn latch_terminal(&self, error: TransportErrorV0) {
        latch_bootstrap_terminal(&self.fence, &self.gate, &self.wake, error);
    }

    fn install(&self, control: BootstrapPumpControlV0) -> Result<(), TransportErrorV0> {
        let mut slot = self
            .control
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if slot.is_some() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        *slot = Some(control);
        Ok(())
    }

    fn take(&self) -> Option<BootstrapPumpControlV0> {
        self.control.lock().ok()?.take()
    }
}

/// Latches an operation-fence terminal without making the forwarding task
/// own the runtime object.  The latter is essential: the runtime stores the
/// task's `JoinHandle`, so a strong task→runtime edge would keep an abandoned
/// operation alive forever and make the drop reaper unreachable.
fn latch_bootstrap_terminal(
    fence: &Arc<Mutex<BootstrapFenceStateV0>>,
    gate: &PrewriteGateV0,
    wake: &watch::Sender<u64>,
    error: TransportErrorV0,
) {
    let transitioned = match fence.lock() {
        Ok(mut fence) => latch_bootstrap_terminal_locked(&mut fence, gate, error),
        Err(_) => false,
    };
    if transitioned {
        gate.notify_transition();
    }
    notify_bootstrap_fence(wake);
}

impl Drop for BootstrapPumpRuntimeV0 {
    fn drop(&mut self) {
        // No session clone remains to perform the explicit async close.  The
        // original operation is no longer allowed to prove anything, so first
        // make that fact sticky, then abort without holding any fence lock.
        self.latch_terminal(TransportErrorV0::ManualRecoveryHold);
        let Some(control) = self.take() else {
            return;
        };
        let BootstrapPumpControlV0 {
            close, abort, join, ..
        } = control;
        let _ = close.send(());
        abort.abort();
        // Tokio owns cancellation after `abort`; retaining the JoinHandle in
        // a bounded reaper keeps panic/cancellation cleanup observable without
        // blocking `Drop` or leaking a task indefinitely.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), join).await;
            });
        }
    }
}

/// A panic or cancellation in the forwarding task must not look like a quiet
/// empty watch.  This guard runs during unwinding and abort cancellation and
/// latches the same terminal fence state as an ordinary queue failure.
struct BootstrapPumpExitGuardV0 {
    fence: Arc<Mutex<BootstrapFenceStateV0>>,
    gate: PrewriteGateV0,
    wake: watch::Sender<u64>,
    output_failure: Arc<AtomicBool>,
    clean: bool,
}

impl Drop for BootstrapPumpExitGuardV0 {
    fn drop(&mut self) {
        if !self.clean {
            self.output_failure.store(true, Ordering::Release);
            latch_bootstrap_terminal(
                &self.fence,
                &self.gate,
                &self.wake,
                TransportErrorV0::ManualRecoveryHold,
            );
        }
    }
}

#[derive(Debug)]
struct BootstrapOwnedRootEchoV0 {
    /// Keep the actual metadata-patch response, not merely its status-shaped
    /// projection.  `RootStateLink` may proceed only from this exact response;
    /// an annotation, finalizer, status, or State-link modification changes
    /// the object and therefore cannot inherit root authority.
    response: ObjectV0,
    phase: BootstrapRootPhaseV0,
}

#[derive(Debug)]
struct BootstrapFrozenRootV0 {
    uid: String,
    namespace: String,
    name: String,
    generation: i64,
    spec: oxideloop_controller_api::WebApplicationSpec,
    deleting: bool,
}

impl BootstrapFrozenRootV0 {
    fn from_root(root: &ObjectV0) -> Result<Self, TransportErrorV0> {
        Ok(Self {
            uid: root.metadata.uid.clone(),
            namespace: root.reference.namespace.clone(),
            name: root.reference.name.clone(),
            generation: generation(root)?,
            spec: serde_json::from_value(
                root.value
                    .get("spec")
                    .cloned()
                    .ok_or(TransportErrorV0::Stale("bootstrap root spec"))?,
            )
            .map_err(|_| TransportErrorV0::Stale("bootstrap root spec"))?,
            deleting: root
                .value
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty())),
        })
    }

    fn matches(&self, root: &ObjectV0) -> Result<bool, TransportErrorV0> {
        Ok(Self::from_root(root).is_ok_and(|current| {
            current.uid == self.uid
                && current.namespace == self.namespace
                && current.name == self.name
                && current.generation == self.generation
                && current.spec == self.spec
                && current.deleting == self.deleting
        }))
    }
}

#[derive(Debug)]
struct BootstrapAuthorityV0 {
    active: bool,
    phase: Option<BootstrapAuthorityPhaseV0>,
    /// The first phase is frozen by its root preflight while the explicit
    /// collected pre-poll authority remains sealed. `claim_first_write`
    /// consumes it atomically with the first write lease.
    pending_first_phase: Option<BootstrapRootPhaseV0>,
    frozen_root: Option<BootstrapFrozenRootV0>,
    /// A direct (non-collected) root response can be required by the immediate
    /// following root write.  Collected bootstrap never exposes this as a
    /// runtime duplicate-authority path: it uses the sequenced raw fence.
    exact_owned_root_echo: Option<BootstrapOwnedRootEchoV0>,
    /// Kubernetes assigned this UID in a direct State CREATE response within
    /// this exact operation. It is the sole exception to the normal
    /// no-adoption rule when an owned root echo restarts bootstrap before the
    /// root State-link has been written.
    issued_state: Option<(ObjectRefV0, String)>,
}

impl BootstrapAuthorityV0 {
    fn new() -> Self {
        Self {
            active: false,
            phase: None,
            pending_first_phase: None,
            frozen_root: None,
            exact_owned_root_echo: None,
            issued_state: None,
        }
    }
}

/// Exclusive, non-cloneable authority to poll one frozen write in one exact
/// operation epoch.  It is deliberately private: callers may carry a session
/// but cannot manufacture or reuse a subwrite lease.
#[derive(Debug)]
struct SubwritePermitV0 {
    gate: PrewriteGateV0,
    epoch: u32,
    bootstrap: bool,
    bootstrap_collected: bool,
}

impl OperationSessionV0 {
    fn production(collector_bound: bool) -> Self {
        let budget = OperationBudgetV0::production();
        debug_assert!(budget.is_strictly_ordered());
        let (bootstrap_fence_wake, _bootstrap_fence_observer) = watch::channel(0_u64);
        let gate = PrewriteGateV0::new();
        let bootstrap_fence = Arc::new(Mutex::new(BootstrapFenceStateV0::new()));
        let bootstrap_pump = Arc::new(BootstrapPumpRuntimeV0::new(
            Arc::clone(&bootstrap_fence),
            gate.clone(),
            bootstrap_fence_wake.clone(),
        ));
        Self {
            budget,
            gate,
            bootstrap_fence,
            bootstrap_fence_wake,
            bootstrap_pump,
            bootstrap: Arc::new(Mutex::new(BootstrapAuthorityV0::new())),
            collector_bound,
            #[cfg(test)]
            bootstrap_phase_wait_hook: None,
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn prewrite_gate(&self) -> &PrewriteGateV0 {
        &self.gate
    }

    #[doc(hidden)]
    #[must_use]
    pub fn budget(&self) -> &OperationBudgetV0 {
        &self.budget
    }

    #[doc(hidden)]
    #[must_use]
    pub fn max_prewrite_retries(&self) -> u8 {
        self.budget.max_prewrite_retries()
    }

    /// Tests identity of the opaque session minted for one pinned collector
    /// future.  This is intentionally stronger than comparing deadlines or
    /// gate state: a later operation may have identical policy but never the
    /// authority to consume this operation's no-write capability.
    #[doc(hidden)]
    #[must_use]
    pub fn matches_exact_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.budget.inner, &other.budget.inner)
            && Arc::ptr_eq(&self.gate.state, &other.gate.state)
            && Arc::ptr_eq(&self.bootstrap_fence, &other.bootstrap_fence)
            && Arc::ptr_eq(&self.bootstrap_pump, &other.bootstrap_pump)
            && Arc::ptr_eq(&self.bootstrap, &other.bootstrap)
            && self.collector_bound == other.collector_bound
    }

    fn begin_bootstrap_sequence(&self) -> Result<(), TransportErrorV0> {
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        match (authority.active, authority.phase) {
            // Direct (non-collector) callers retain the old internal-only
            // bootstrap setup. They never receive collector duplicate
            // authority.
            (false, None) if !self.collector_bound => {
                authority.active = true;
                Ok(())
            }
            // The collector sealed this exact session before its select loop.
            // Entering the dispatch future does not erase that witness: its
            // first phase preflight advances it immediately before the first
            // subwrite can claim a permit.
            (
                true,
                Some(
                    BootstrapAuthorityPhaseV0::BootstrapNotYetPolled
                    | BootstrapAuthorityPhaseV0::Active(_),
                ),
            ) => Ok(()),
            _ => Err(TransportErrorV0::ManualRecoveryHold),
        }
    }

    /// Seals the exact command and address that may own one collected
    /// bootstrap.  This performs no Kubernetes I/O.  A cross-command or
    /// cross-address reuse fails before any list, watch, GET, or write.
    #[doc(hidden)]
    pub fn seal_collected_bootstrap_fence(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
        root: &ObjectV0,
        state: Option<&ObjectV0>,
    ) -> Result<(), TransportErrorV0> {
        if !self.collector_bound || !matches!(command, CommandV0::EnsureBootstrap { .. }) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        validate_address(address)?;
        validate_collected_bootstrap_baseline(address, command, root, state)?;
        // Fence -> bootstrap -> gate is the only bootstrap lock order.
        let mut fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if fence.command.is_some() || fence.address.is_some() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if authority.active || authority.phase.is_some() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if self.gate.state() != PrewriteGateStateV0::PreWrite {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let transitioned = self
            .gate
            .state
            .compare_exchange(
                PrewriteGateV0::encode(0, PrewriteGateV0::PREWRITE),
                PrewriteGateV0::encode(0, PrewriteGateV0::BASELINE_PENDING),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if transitioned {
            fence.command = Some(command.clone());
            fence.address = Some(address.clone());
            // This is the actor-side, operation-owned raw cut.  It is
            // immutable for the rest of the command: effect-time GETs may
            // compare against it but may never replace it as bootstrap
            // authority.
            fence.baseline = Some(ObservedBootstrapBaselineV0 {
                root: root.clone(),
                state: state.cloned(),
            });
            authority.active = true;
            authority.phase = Some(BootstrapAuthorityPhaseV0::BootstrapNotYetPolled);
        }
        drop(authority);
        drop(fence);
        if !transitioned {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        self.gate.notify_transition();
        self.notify_bootstrap_fence();
        Ok(())
    }

    /// Rejects a collected bootstrap whose effect input is not the immutable
    /// command/address pair sealed before the collector was allowed to poll.
    /// This is called before the first transport read, so session reuse cannot
    /// turn a different root into Kubernetes I/O.
    fn validate_collected_bootstrap_fence_input(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
    ) -> Result<(), TransportErrorV0> {
        if !self.collector_bound {
            return Ok(());
        }
        let fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !fence.same_sealed_input(address, command)
            || fence.disarmed
            || fence.terminal.is_some()
            || !fence.pump_active
            || !fence.first_poll_complete
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok(())
    }

    fn is_bootstrap_sequence(&self) -> bool {
        self.bootstrap
            .lock()
            .is_ok_and(|authority| authority.active)
    }

    /// A collected bootstrap is distinguished from every other collected
    /// command by the fence seal, rather than by a generic active-bootstrap
    /// bit.  The seal is read first so callers never invert the bootstrap
    /// transition lock order.
    #[doc(hidden)]
    #[must_use]
    pub fn has_collected_bootstrap_fence(&self) -> bool {
        self.collector_bound
            && self
                .bootstrap_fence
                .lock()
                .is_ok_and(|fence| fence.command.is_some() && !fence.disarmed)
    }

    fn notify_bootstrap_fence(&self) {
        notify_bootstrap_fence(&self.bootstrap_fence_wake);
    }

    fn install_bootstrap_pump(
        &self,
        control: BootstrapPumpControlV0,
    ) -> Result<(), TransportErrorV0> {
        self.bootstrap_pump.install(control)
    }

    fn bootstrap_pump_runtime(&self) -> Arc<BootstrapPumpRuntimeV0> {
        Arc::clone(&self.bootstrap_pump)
    }

    /// Returns the highest record that is fully committed and enqueued by the
    /// independent watch pump.  A record in the narrow commit→enqueue gap is
    /// deliberately not a completion edge and must be retried under the
    /// original operation deadline.
    #[doc(hidden)]
    pub async fn capture_bootstrap_fence_watermark(&self) -> Result<u64, TransportErrorV0> {
        loop {
            // Even an immediately stable watermark must remain within the
            // original operation deadline; a hot producer must not turn this
            // retry loop into an unbounded CPU-only acknowledgement path.
            let _ = self.budget.remaining(FiniteIoStageV0::ReadOnly)?;
            let mut wake = self.bootstrap_fence_wake.subscribe();
            let capture = {
                let fence = self
                    .bootstrap_fence
                    .lock()
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                if fence.terminal.is_some() || fence.disarmed || !fence.pump_active {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                (fence.first_poll_complete && fence.committed == fence.enqueued)
                    .then_some(fence.committed)
            };
            if let Some(watermark) = capture {
                return Ok(watermark);
            }
            let remaining = self.budget.remaining(FiniteIoStageV0::ReadOnly)?;
            tokio::time::timeout(remaining, wake.changed())
                .await
                .map_err(|_| TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::ReadOnly))?
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        }
    }

    #[doc(hidden)]
    pub fn bootstrap_fence_consumed_through(
        &self,
        watermark: u64,
    ) -> Result<bool, TransportErrorV0> {
        let fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        Ok(fence.consumed >= watermark)
    }

    /// Exposes only the terminal fact needed by the collector to turn a
    /// consumed fence record into its existing prewrite/rebarrier outcome.
    /// It is deliberately not acknowledgement authority: all write claims
    /// and final proof paths continue to reject a terminal fence directly.
    #[doc(hidden)]
    #[must_use]
    pub fn bootstrap_fence_terminal(&self) -> Option<TransportErrorV0> {
        self.bootstrap_fence
            .lock()
            .ok()
            .and_then(|fence| fence.terminal.clone())
    }

    #[doc(hidden)]
    pub fn bootstrap_fence_records_until(&self, watermark: u64) -> Result<u64, TransportErrorV0> {
        let fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if fence.terminal.is_some() || watermark > fence.enqueued || watermark < fence.consumed {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok(watermark - fence.consumed)
    }

    #[doc(hidden)]
    pub fn bootstrap_fence_consumer_gap(&self) {
        self.bootstrap_pump
            .latch_terminal(TransportErrorV0::WatchOverflow);
    }

    /// Closes the per-operation forwarding pump under the same immutable
    /// operation deadline.  An undisarmed operation is terminal first; an
    /// already-proven operation returns its upstream retained watch without
    /// converting normal completion into a false terminal failure.
    #[doc(hidden)]
    pub async fn close_collected_bootstrap_fence(
        &self,
        watch: &mut WatchHandleV0,
    ) -> Result<(), TransportErrorV0> {
        let Some(control) = self.bootstrap_pump.take() else {
            return Ok(());
        };
        let (disarmed, bootstrap_address) = if let Ok(fence) = self.bootstrap_fence.lock() {
            (fence.disarmed, fence.address.clone())
        } else {
            let BootstrapPumpControlV0 { abort, join, .. } = control;
            abort.abort();
            Self::reap_bootstrap_join(join);
            return Err(TransportErrorV0::ManualRecoveryHold);
        };
        if !disarmed {
            self.bootstrap_pump
                .latch_terminal(TransportErrorV0::ManualRecoveryHold);
        }
        // Signal before waiting.  The sender, abort handle, and join are all
        // used after the control mutex and fence locks have been released.
        let BootstrapPumpControlV0 {
            close,
            mut returned_watch,
            abort,
            mut join,
        } = control;
        let Some(bootstrap_address) = bootstrap_address else {
            abort.abort();
            Self::reap_bootstrap_join(join);
            return Err(TransportErrorV0::ManualRecoveryHold);
        };
        let _ = close.send(());
        let remaining = match self.budget.remaining(FiniteIoStageV0::ReadOnly) {
            Ok(remaining) => remaining,
            Err(error) => {
                abort.abort();
                Self::reap_bootstrap_join(join);
                return Err(error);
            }
        };
        let upstream = match tokio::time::timeout(remaining, &mut returned_watch).await {
            Ok(Ok(upstream)) => upstream,
            Ok(Err(_)) => {
                abort.abort();
                Self::reap_bootstrap_join(join);
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
            Err(_) => {
                abort.abort();
                Self::reap_bootstrap_join(join);
                return Err(TransportErrorV0::FiniteIoDeadline(
                    FiniteIoStageV0::ReadOnly,
                ));
            }
        };
        // The task has released the upstream handle, so restoring it cannot
        // race the forwarding loop.  Still await its final guard under the
        // original operation deadline: cancellation/panic after handoff is a
        // terminal operation outcome, not a successful close.
        let join_remaining = match self.budget.remaining(FiniteIoStageV0::ReadOnly) {
            Ok(remaining) => remaining,
            Err(error) => {
                abort.abort();
                Self::reap_bootstrap_join(join);
                return Err(error);
            }
        };
        let joined = tokio::time::timeout(join_remaining, &mut join).await;
        let restored = match self.budget.remaining(FiniteIoStageV0::ReadOnly) {
            Ok(remaining) => {
                watch
                    .restore_after_bootstrap_fence(upstream, &bootstrap_address, remaining)
                    .await
            }
            Err(error) => Err(error),
        };
        match joined {
            Ok(Ok(())) => {
                restored?;
                if let Ok(mut fence) = self.bootstrap_fence.lock() {
                    fence.pump_active = false;
                }
                // A cancelled/failed operation deliberately arrives here with
                // a sticky terminal already latched.  Cleanup itself still
                // succeeded; preserve the collector's original error or
                // prewrite-invalidated capability instead of replacing it
                // with a synthetic close failure.
                Ok(())
            }
            Ok(Err(_)) => {
                abort.abort();
                Self::reap_bootstrap_join(join);
                Err(TransportErrorV0::ManualRecoveryHold)
            }
            Err(_) => {
                abort.abort();
                Self::reap_bootstrap_join(join);
                Err(TransportErrorV0::FiniteIoDeadline(
                    FiniteIoStageV0::ReadOnly,
                ))
            }
        }
    }

    fn reap_bootstrap_join(mut join: tokio::task::JoinHandle<()>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), &mut join).await;
            });
        }
    }

    /// Activates the immutable raw baseline sealed at the operation-owned
    /// list/watch/reread cut.  Effect-time reads are allowed to compare with
    /// that cut, but cannot mint or replace it. Records that arrived from the
    /// already-installed exact-name watch are classified under the same fence
    /// before `PreWrite` is armed. A differing object, deletion, terminal, or
    /// missing first-poll handshake therefore makes zero bootstrap writes.
    fn arm_collected_bootstrap_baseline(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
        root: &ObjectV0,
        state: Option<&ObjectV0>,
    ) -> Result<(), TransportErrorV0> {
        if !self.collector_bound {
            return Ok(());
        }
        let mut fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !fence.same_sealed_input(address, command)
            || fence.disarmed
            || fence.armed
            || (fence.pump_active && !fence.first_poll_complete)
            || fence
                .baseline
                .as_ref()
                .is_none_or(|baseline| baseline.root != *root || baseline.state.as_ref() != state)
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !authority.active
            || authority.phase != Some(BootstrapAuthorityPhaseV0::BootstrapNotYetPolled)
            || self.gate.state() != PrewriteGateStateV0::BaselinePending
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let mut terminal = fence.terminal.is_some()
            || (fence.pump_active && !fence.all_committed_records_consumed());
        let pending = std::mem::take(&mut fence.pending);
        for record in pending {
            let event = match record.kind {
                BootstrapFenceRecordKindV0::Data(event) => event,
                BootstrapFenceRecordKindV0::Terminal(error) => {
                    fence.terminal.get_or_insert(error);
                    terminal = true;
                    continue;
                }
            };
            match event {
                WatchEventV0::Added(observed) | WatchEventV0::Modified(observed)
                    if fence.baseline.as_ref().is_some_and(|baseline| {
                        bootstrap_response_matches(&observed, &baseline.root)
                    }) => {}
                // Bookmarks never become bootstrap authority.  The retained
                // named watch has bookmarks disabled, but tolerate an injected
                // one as a no-op rather than promoting it into a freshness
                // claim.
                WatchEventV0::Bookmark { .. } => {}
                WatchEventV0::Added(_) | WatchEventV0::Modified(_) | WatchEventV0::Deleted(_) => {
                    terminal = true;
                }
            }
        }
        fence.armed = !terminal;
        if terminal {
            fence
                .terminal
                .get_or_insert(TransportErrorV0::PreWriteInvalidated);
        }
        let transitioned = if terminal {
            self.gate.state.compare_exchange(
                PrewriteGateV0::encode(0, PrewriteGateV0::BASELINE_PENDING),
                PrewriteGateV0::encode(0, PrewriteGateV0::INVALIDATED),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
        } else {
            self.gate.state.compare_exchange(
                PrewriteGateV0::encode(0, PrewriteGateV0::BASELINE_PENDING),
                PrewriteGateV0::encode(0, PrewriteGateV0::PREWRITE),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
        };
        drop(authority);
        drop(fence);
        if transitioned.is_err() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        self.gate.notify_transition();
        self.notify_bootstrap_fence();
        if terminal {
            Err(TransportErrorV0::PreWriteInvalidated)
        } else {
            Ok(())
        }
    }

    /// Returns the sole raw baseline sealed by the collector's fresh
    /// list/watch/reread cut.  This intentionally has no setter: the effect
    /// must not turn a later GET into replacement bootstrap authority.
    fn sealed_collected_bootstrap_baseline(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
    ) -> Result<(ObjectV0, Option<ObjectV0>), TransportErrorV0> {
        if !self.collector_bound {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !fence.same_sealed_input(address, command) || fence.disarmed {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let baseline = fence
            .baseline
            .as_ref()
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        Ok((baseline.root.clone(), baseline.state.clone()))
    }

    /// Classifies one named-root retained-watch record before the runtime can
    /// buffer it for actor reduction. `true` means the record was an exact
    /// bootstrap baseline/owned echo and has been consumed by this operation;
    /// `false` leaves it for the normal retained-root path after also making
    /// the bootstrap fence terminal when appropriate.
    #[doc(hidden)]
    #[must_use]
    pub fn observe_bootstrap_root_watch(&self, event: &WatchEventV0) -> bool {
        if !self.collector_bound {
            return false;
        }
        let mut notify_gate = false;
        let mut notify_fence = false;
        let consumed = {
            let Ok(mut fence) = self.bootstrap_fence.lock() else {
                return false;
            };
            let Ok(authority) = self.bootstrap.lock() else {
                return false;
            };
            if !authority.active || fence.disarmed {
                false
            } else if matches!(event, WatchEventV0::Bookmark { .. }) {
                // The exact-name bootstrap watch disables bookmarks.  Ignore a
                // defensive/test-injected bookmark rather than assigning it a
                // fence sequence or using it as acknowledgement authority.
                true
            } else if !fence
                .address
                .as_ref()
                .is_some_and(|address| bootstrap_watch_targets_address(event, address))
            {
                // A correctly installed pump rejects this before enqueueing,
                // but do not let a scripted handle or a future relay turn a
                // leaked record into an ignored driver observation.
                notify_gate |= latch_bootstrap_terminal_locked(
                    &mut fence,
                    &self.gate,
                    TransportErrorV0::PreWriteInvalidated,
                );
                notify_fence = true;
                false
            } else {
                let record = {
                    #[cfg(test)]
                    {
                        if fence.pump_active {
                            fence.take_next_data(event)
                        } else {
                            let setup = fence
                                .commit(BootstrapFenceRecordKindV0::Data(event.clone()))
                                .and_then(|sequence| {
                                    fence.mark_enqueued(sequence)?;
                                    Ok(())
                                });
                            if setup.is_err() {
                                Err(())
                            } else {
                                fence.take_next_data(event)
                            }
                        }
                    }
                    #[cfg(not(test))]
                    {
                        if fence.pump_active {
                            fence.take_next_data(event)
                        } else {
                            Err(())
                        }
                    }
                };
                match record {
                    Err(()) => {
                        notify_gate |= latch_bootstrap_terminal_locked(
                            &mut fence,
                            &self.gate,
                            TransportErrorV0::ManualRecoveryHold,
                        );
                        notify_fence = true;
                        false
                    }
                    Ok(record) => match event {
                        WatchEventV0::Deleted(_) => {
                            notify_gate |= latch_bootstrap_terminal_locked(
                                &mut fence,
                                &self.gate,
                                TransportErrorV0::PreWriteInvalidated,
                            );
                            notify_fence = true;
                            false
                        }
                        WatchEventV0::Added(observed) | WatchEventV0::Modified(observed) => {
                            // Final proof has a deliberately tiny consumer
                            // language. It accepts the exact `Modified`
                            // record carrying the full final State-link
                            // response, plus one earlier exact RootMetadata
                            // echo. The latter is only a response ordering
                            // allowance: it cannot change the phase or final
                            // acknowledgement, and is closed once either
                            // echo has been consumed.
                            if self.gate.state() == PrewriteGateStateV0::FinalProofPending {
                                if matches!(event, WatchEventV0::Modified(_))
                                    && fence.final_echo.as_ref().is_some_and(|final_echo| {
                                        bootstrap_response_matches(observed, final_echo)
                                    })
                                {
                                    fence.final_echo_consumed = true;
                                    notify_fence = true;
                                    true
                                } else if matches!(event, WatchEventV0::Modified(_))
                                    && !fence.metadata_echo_consumed
                                    && !fence.final_echo_consumed
                                    && fence.metadata_echo.as_ref().is_some_and(|response| {
                                        bootstrap_response_matches(observed, response)
                                    })
                                {
                                    fence.metadata_echo_consumed = true;
                                    notify_fence = true;
                                    true
                                } else {
                                    notify_gate |= latch_bootstrap_terminal_locked(
                                        &mut fence,
                                        &self.gate,
                                        TransportErrorV0::PreWriteInvalidated,
                                    );
                                    notify_fence = true;
                                    false
                                }
                            } else if !fence.armed {
                                fence.pending.push_back(record);
                                notify_fence = true;
                                true
                            } else if matches!(event, WatchEventV0::Modified(_))
                                && !fence.metadata_echo_consumed
                                && fence.metadata_echo.as_ref().is_some_and(|response| {
                                    bootstrap_response_matches(observed, response)
                                })
                            {
                                fence.metadata_echo_consumed = true;
                                notify_fence = true;
                                true
                            } else if fence.baseline.as_ref().is_some_and(|baseline| {
                                bootstrap_response_matches(observed, &baseline.root)
                            }) {
                                notify_fence = true;
                                true
                            } else if self.gate.state() == PrewriteGateStateV0::WritePolled {
                                // The already-polled request wins.  Hold the
                                // raw record until its response can classify
                                // the exact echo, never replay or cancel it.
                                fence.pending.push_back(record);
                                notify_fence = true;
                                true
                            } else {
                                notify_gate |= latch_bootstrap_terminal_locked(
                                    &mut fence,
                                    &self.gate,
                                    TransportErrorV0::PreWriteInvalidated,
                                );
                                notify_fence = true;
                                false
                            }
                        }
                        WatchEventV0::Bookmark { .. } => unreachable!("bookmark handled above"),
                    },
                }
            }
        };
        if notify_gate {
            self.gate.notify_transition();
        }
        if notify_fence {
            self.notify_bootstrap_fence();
        }
        consumed
    }

    /// Terminal stream outcomes are sticky.  They invalidate only an
    /// unpolled/bootstrap-phase request; a write that already won stays
    /// `WritePolled` so it can never be replayed, but later phases and final
    /// acknowledgement remain blocked by the fence terminal.
    #[doc(hidden)]
    pub fn observe_bootstrap_root_terminal(&self, error: TransportErrorV0) {
        if !self.collector_bound {
            return;
        }
        let mut notify_gate = false;
        {
            let Ok(mut fence) = self.bootstrap_fence.lock() else {
                return;
            };
            let Ok(authority) = self.bootstrap.lock() else {
                return;
            };
            if !authority.active || fence.disarmed {
                return;
            }
            #[cfg(test)]
            if !fence.pump_active {
                let setup = fence
                    .commit(BootstrapFenceRecordKindV0::Terminal(error.clone()))
                    .and_then(|sequence| {
                        fence.mark_enqueued(sequence)?;
                        Ok(())
                    });
                if setup.is_err() {
                    notify_gate |= latch_bootstrap_terminal_locked(
                        &mut fence,
                        &self.gate,
                        TransportErrorV0::ManualRecoveryHold,
                    );
                }
            }
            #[cfg(not(test))]
            if !fence.pump_active {
                notify_gate |= latch_bootstrap_terminal_locked(
                    &mut fence,
                    &self.gate,
                    TransportErrorV0::ManualRecoveryHold,
                );
            }
            if fence.take_next_terminal(&error).is_err() && fence.terminal.is_none() {
                notify_gate |= latch_bootstrap_terminal_locked(
                    &mut fence,
                    &self.gate,
                    TransportErrorV0::ManualRecoveryHold,
                );
            }
            notify_gate |= latch_bootstrap_terminal_locked(&mut fence, &self.gate, error);
        }
        if notify_gate {
            self.gate.notify_transition();
        }
        self.notify_bootstrap_fence();
    }

    async fn wait_for_bootstrap_phase_authority(&self) -> Result<(), TransportErrorV0> {
        if !self.is_bootstrap_sequence() || !self.collector_bound {
            return Ok(());
        }
        loop {
            // Register first.  A phase seal/authentication transition between
            // the old state read and `notified()` was otherwise a lost wake
            // that could leave a bootstrap preflight asleep forever.
            // Register the versioned phase observer before reading state.
            // A seal/authentication transition in the old lost-wake window is
            // retained by `changed()` and forces this loop to re-read state.
            let mut notified = self.gate.wake.subscribe();
            let state = self.gate.state();
            #[cfg(test)]
            if let Some(hook) = self.bootstrap_phase_wait_hook.as_ref()
                && !hook.state_read.swap(true, Ordering::AcqRel)
            {
                hook.release.wait().await;
            }
            match state {
                PrewriteGateStateV0::PreWrite => return Ok(()),
                PrewriteGateStateV0::Invalidated => {
                    return Err(TransportErrorV0::PreWriteInvalidated);
                }
                PrewriteGateStateV0::BootstrapPhasePending
                | PrewriteGateStateV0::RootAuthPending
                | PrewriteGateStateV0::BaselinePending => {
                    let _ = notified.changed().await;
                }
                PrewriteGateStateV0::WritePolled
                | PrewriteGateStateV0::Proven
                | PrewriteGateStateV0::AmbiguousHeld
                | PrewriteGateStateV0::FinalProofPending => {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
            }
        }
    }

    /// Installs a phase-local root authority immediately after the exact
    /// root preflight that protects a bootstrap subwrite.  Any UID, spec,
    /// generation, deletion, namespace, or name change is rejected before a
    /// later phase can reach its write lease.
    fn begin_bootstrap_root_phase(
        &self,
        phase: BootstrapRootPhaseV0,
        root: &ObjectV0,
    ) -> Result<(), TransportErrorV0> {
        if self.has_collected_bootstrap_fence() {
            let fence = self
                .bootstrap_fence
                .lock()
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
            if !fence.armed || fence.terminal.is_some() || fence.disarmed {
                return Err(TransportErrorV0::PreWriteInvalidated);
            }
            let mut authority = self
                .bootstrap
                .lock()
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
            if !authority.active {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
            match authority.frozen_root.as_ref() {
                Some(frozen) if !frozen.matches(root)? => {
                    return Err(TransportErrorV0::Stale("bootstrap root authority"));
                }
                Some(_) => {}
                None => authority.frozen_root = Some(BootstrapFrozenRootV0::from_root(root)?),
            }
            if authority.phase == Some(BootstrapAuthorityPhaseV0::BootstrapNotYetPolled) {
                authority.pending_first_phase = Some(phase);
            } else if authority.phase != Some(BootstrapAuthorityPhaseV0::Active(phase)) {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
            return Ok(());
        }
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !authority.active {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        match authority.frozen_root.as_ref() {
            Some(frozen) if !frozen.matches(root)? => {
                return Err(TransportErrorV0::Stale("bootstrap root authority"));
            }
            Some(_) => {}
            None => authority.frozen_root = Some(BootstrapFrozenRootV0::from_root(root)?),
        }
        if authority.phase == Some(BootstrapAuthorityPhaseV0::BootstrapNotYetPolled) {
            authority.pending_first_phase = Some(phase);
        } else {
            authority.phase = Some(BootstrapAuthorityPhaseV0::Active(phase));
        }
        // A phase must never accept an echo from a future or unrelated root
        // write.  The immediately preceding direct root response is retained
        // only when moving into the next root-writing phase.
        if !matches!(
            phase,
            BootstrapRootPhaseV0::StateFinalizer
                | BootstrapRootPhaseV0::RootStateLink
                | BootstrapRootPhaseV0::Complete
        ) {
            authority.exact_owned_root_echo = None;
        }
        Ok(())
    }

    /// Claims a collected bootstrap write with the fence, bootstrap authority,
    /// and phase gate in one fixed order.  A terminal delivered before this
    /// point wins; a terminal delivered afterwards cannot replay this already
    /// polled write, but blocks its completion and every later phase.
    fn claim_collected_bootstrap_write(
        &self,
        epoch: u32,
    ) -> Result<SubwritePermitV0, TransportErrorV0> {
        let fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !fence.armed
            || fence.disarmed
            || fence.terminal.is_some()
            || !fence.all_committed_records_consumed()
            || !authority.active
        {
            return Err(TransportErrorV0::PreWriteInvalidated);
        }
        let encoded = PrewriteGateV0::encode(epoch, PrewriteGateV0::PREWRITE);
        if self
            .gate
            .state
            .compare_exchange(
                encoded,
                PrewriteGateV0::encode(epoch, PrewriteGateV0::WRITE_POLLED),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(match self.gate.state() {
                PrewriteGateStateV0::Invalidated => TransportErrorV0::PreWriteInvalidated,
                _ => TransportErrorV0::ManualRecoveryHold,
            });
        }
        if authority.phase == Some(BootstrapAuthorityPhaseV0::BootstrapNotYetPolled) {
            let phase = authority
                .pending_first_phase
                .take()
                .ok_or(TransportErrorV0::ManualRecoveryHold)?;
            authority.phase = Some(BootstrapAuthorityPhaseV0::Active(phase));
        }
        drop(authority);
        drop(fence);
        self.gate.notify_transition();
        Ok(SubwritePermitV0 {
            gate: self.gate.clone(),
            epoch,
            bootstrap: true,
            bootstrap_collected: true,
        })
    }

    /// Publishes an already-validated bootstrap subwrite response and the
    /// authority it grants before the phase gate is released.  Callers perform
    /// all JSON/UID/spec proof validation before entering this method; it takes
    /// no async action, sends no notification while locked, and is the only
    /// collected-bootstrap path that turns `WritePolled` into another phase.
    fn complete_bootstrap_subwrite(
        &self,
        permit: SubwritePermitV0,
        phase: BootstrapRootPhaseV0,
        response: &ObjectV0,
    ) -> Result<(), TransportErrorV0> {
        if !permit.bootstrap || !permit.bootstrap_collected || !self.collector_bound {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let mut fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !fence.armed
            || fence.disarmed
            || !authority.active
            || authority.phase != Some(BootstrapAuthorityPhaseV0::Active(phase))
            || self.gate.state.load(Ordering::Acquire)
                != PrewriteGateV0::encode(permit.epoch, PrewriteGateV0::WRITE_POLLED)
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }

        let (next_authority, next_gate) = match phase {
            BootstrapRootPhaseV0::StatePost => {
                if response.reference.resource != ResourceKindV0::OxideLoopState
                    || response.metadata.uid.is_empty()
                    || authority.issued_state.is_some()
                    || fence
                        .baseline
                        .as_ref()
                        .is_some_and(|baseline| baseline.state.is_some())
                {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                authority.issued_state =
                    Some((response.reference.clone(), response.metadata.uid.clone()));
                (
                    BootstrapAuthorityPhaseV0::Active(BootstrapRootPhaseV0::RootMetadata),
                    PrewriteGateV0::encode(
                        permit.epoch + 1,
                        PrewriteGateV0::BOOTSTRAP_PHASE_PENDING,
                    ),
                )
            }
            BootstrapRootPhaseV0::RootMetadata => {
                if response.reference.resource != ResourceKindV0::WebApplication {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                fence.metadata_echo = Some(response.clone());
                (
                    BootstrapAuthorityPhaseV0::Active(BootstrapRootPhaseV0::StateFinalizer),
                    PrewriteGateV0::encode(
                        permit.epoch + 1,
                        PrewriteGateV0::BOOTSTRAP_PHASE_PENDING,
                    ),
                )
            }
            BootstrapRootPhaseV0::StateFinalizer => {
                if response.reference.resource != ResourceKindV0::OxideLoopState
                    || response.metadata.uid.is_empty()
                    || response.metadata.resource_version.is_empty()
                    || fence.final_state.is_some()
                {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                fence.final_state = Some(response.clone());
                (
                    BootstrapAuthorityPhaseV0::Active(BootstrapRootPhaseV0::RootStateLink),
                    PrewriteGateV0::encode(
                        permit.epoch + 1,
                        PrewriteGateV0::BOOTSTRAP_PHASE_PENDING,
                    ),
                )
            }
            BootstrapRootPhaseV0::RootStateLink => {
                if response.reference.resource != ResourceKindV0::WebApplication {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                fence.final_echo = Some(response.clone());
                fence.final_echo_consumed = false;
                (
                    BootstrapAuthorityPhaseV0::Active(BootstrapRootPhaseV0::FinalProofPending),
                    PrewriteGateV0::encode(permit.epoch, PrewriteGateV0::FINAL_PROOF_PENDING),
                )
            }
            BootstrapRootPhaseV0::FinalProofPending | BootstrapRootPhaseV0::Complete => {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
        };

        // Publish the response-owned echo before resolving records that were
        // delivered while this write was already polled. This accepts the
        // required response-before/after-watch ordering without granting a
        // generic freshness exemption to another root mutation.
        if !fence.classify_write_polled_records(phase, response) || fence.terminal.is_some() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }

        // Publication above precedes this CAS under the same fence.  No
        // observer can see an open gate without all issued-state/echo/link
        // authority needed by the following phase.
        if self
            .gate
            .state
            .compare_exchange(
                PrewriteGateV0::encode(permit.epoch, PrewriteGateV0::WRITE_POLLED),
                next_gate,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        authority.phase = Some(next_authority);
        drop(authority);
        drop(fence);
        // `permit` drops after `WritePolled` has been transitioned, so its
        // ambiguity fallback is inert.
        drop(permit);
        self.gate.notify_transition();
        self.notify_bootstrap_fence();
        Ok(())
    }

    /// Publishes the only recoverable nonfinal bootstrap response loss.  The
    /// root authority bundle is a complete, deterministic JSON-patch
    /// transform, so an exact post-write reread can prove this metadata write
    /// without adopting a same-name State or reopening the original write.
    /// State creation/finalization and the final link deliberately do not use
    /// this path: their missing response remains ambiguous.
    fn complete_recovered_bootstrap_root_metadata(
        &self,
        recovered: &ObjectV0,
    ) -> Result<(), TransportErrorV0> {
        if !self.collector_bound {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let mut fence = self
            .bootstrap_fence
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        let epoch = self.gate.epoch();
        if !fence.armed
            || fence.disarmed
            || fence.terminal.is_some()
            || !authority.active
            || authority.phase
                != Some(BootstrapAuthorityPhaseV0::Active(
                    BootstrapRootPhaseV0::RootMetadata,
                ))
            || self.gate.state.load(Ordering::Acquire)
                != PrewriteGateV0::encode(epoch, PrewriteGateV0::AMBIGUOUS_HELD)
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        fence.metadata_echo = Some(recovered.clone());
        if !fence.classify_write_polled_records(BootstrapRootPhaseV0::RootMetadata, recovered)
            || fence.terminal.is_some()
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let next_epoch = epoch
            .checked_add(1)
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        self.gate
            .state
            .compare_exchange(
                PrewriteGateV0::encode(epoch, PrewriteGateV0::AMBIGUOUS_HELD),
                PrewriteGateV0::encode(next_epoch, PrewriteGateV0::BOOTSTRAP_PHASE_PENDING),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
            BootstrapRootPhaseV0::StateFinalizer,
        ));
        drop(authority);
        drop(fence);
        self.gate.notify_transition();
        self.notify_bootstrap_fence();
        Ok(())
    }

    /// Waits for the collector to consume the exact raw final State-link
    /// response, then atomically disarms the process-local fence.  Each pass
    /// re-reads both objects under the original operation budget; a root
    /// mutation, replacement State, malformed object, terminal stream, or an
    /// event arriving before the captured watermark is consumed can therefore
    /// never become a bootstrap acknowledgement.
    async fn complete_collected_bootstrap_final_proof(
        &self,
        transport: &KubeTransportV0,
        root_ref: &ObjectRefV0,
        state_ref: &ObjectRefV0,
        token: &str,
    ) -> Result<(ObjectV0, ObjectV0), TransportErrorV0> {
        if !self.has_collected_bootstrap_fence() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        loop {
            let mut wake = self.bootstrap_fence_wake.subscribe();
            let watermark = {
                let fence = self
                    .bootstrap_fence
                    .lock()
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                if fence.terminal.is_some() || !fence.pump_active {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                if fence.disarmed || self.gate.state() != PrewriteGateStateV0::FinalProofPending {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                (fence.committed == fence.enqueued).then_some(fence.committed)
            };
            let Some(watermark) = watermark else {
                let remaining = self.budget.remaining(FiniteIoStageV0::ReadOnly)?;
                tokio::time::timeout(remaining, wake.changed())
                    .await
                    .map_err(|_| TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::ReadOnly))?
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                continue;
            };

            // The collector owns consumption.  Do not begin final rereads
            // until every record through this fully-enqueued watermark has
            // been matched in exact arrival order.
            let (ready_for_reread, expected_state) = {
                let fence = self
                    .bootstrap_fence
                    .lock()
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                (
                    fence.terminal.is_none()
                        && fence.consumed == watermark
                        && fence.final_echo_consumed
                        && fence.records.is_empty(),
                    fence.final_state.clone(),
                )
            };
            let Some(expected_state) = expected_state else {
                return Err(TransportErrorV0::ManualRecoveryHold);
            };
            if !ready_for_reread {
                let remaining = self.budget.remaining(FiniteIoStageV0::ReadOnly)?;
                tokio::time::timeout(remaining, wake.changed())
                    .await
                    .map_err(|_| TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::ReadOnly))?
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                continue;
            }

            let state = transport
                .reread_in_operation(state_ref, self)
                .await?
                .ok_or(TransportErrorV0::ManualRecoveryHold)?;
            let root = transport
                .reread_in_operation(root_ref, self)
                .await?
                .ok_or(TransportErrorV0::ManualRecoveryHold)?;
            if !final_bootstrap_state_matches(&root, &state, &expected_state, token) {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
            let sealed = {
                let mut fence = self
                    .bootstrap_fence
                    .lock()
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                let mut authority = self
                    .bootstrap
                    .lock()
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                if fence.terminal.is_some()
                    || !fence.armed
                    || fence.disarmed
                    || !authority.active
                    || authority.phase
                        != Some(BootstrapAuthorityPhaseV0::Active(
                            BootstrapRootPhaseV0::FinalProofPending,
                        ))
                    || self.gate.state.load(Ordering::Acquire)
                        != PrewriteGateV0::encode(
                            self.gate.epoch(),
                            PrewriteGateV0::FINAL_PROOF_PENDING,
                        )
                {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                if !fence.final_echo_consumed
                    || fence.final_state.as_ref() != Some(&state)
                    || !fence
                        .final_echo
                        .as_ref()
                        .is_some_and(|response| bootstrap_response_matches(&root, response))
                    || fence.committed != watermark
                    || fence.enqueued != watermark
                    || fence.consumed != watermark
                    || !fence.records.is_empty()
                    || !fence.pending.is_empty()
                {
                    false
                } else {
                    let encoded = self.gate.state.load(Ordering::Acquire);
                    // Publish the disarm and authority completion while the
                    // fence/bootstrap locks still exclude a concurrent pump
                    // record.  If the CAS unexpectedly loses, restore the
                    // local state rather than leaving a half-disarmed fence.
                    let previous_armed = fence.armed;
                    let previous_disarmed = fence.disarmed;
                    let previous_phase = authority.phase;
                    fence.armed = false;
                    fence.disarmed = true;
                    authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                        BootstrapRootPhaseV0::Complete,
                    ));
                    if self
                        .gate
                        .state
                        .compare_exchange(
                            encoded,
                            PrewriteGateV0::encode(
                                PrewriteGateV0::decode_epoch(encoded),
                                PrewriteGateV0::PROVEN,
                            ),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        fence.armed = previous_armed;
                        fence.disarmed = previous_disarmed;
                        authority.phase = previous_phase;
                        return Err(TransportErrorV0::ManualRecoveryHold);
                    }
                    true
                }
            };
            if sealed {
                self.gate.notify_transition();
                self.notify_bootstrap_fence();
                return Ok((root, state));
            }
            // A record committed after the pre-reread capture changes the
            // watermark and forces another complete proof pass.  Yield before
            // waiting so a hot watch cannot starve the original deadline.
            tokio::task::yield_now().await;
            let remaining = self.budget.remaining(FiniteIoStageV0::ReadOnly)?;
            tokio::time::timeout(remaining, wake.changed())
                .await
                .map_err(|_| TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::ReadOnly))?
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        }
    }

    fn collected_final_state_is_exact(
        &self,
        root: &ObjectV0,
        state: &ObjectV0,
        token: &str,
    ) -> bool {
        self.bootstrap_fence.lock().is_ok_and(|fence| {
            fence
                .final_state
                .as_ref()
                .is_some_and(|expected| final_bootstrap_state_matches(root, state, expected, token))
        })
    }

    fn record_bootstrap_owned_root_echo(
        &self,
        root: &ObjectV0,
        next_phase: BootstrapRootPhaseV0,
    ) -> Result<(), TransportErrorV0> {
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !authority.active {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        authority.exact_owned_root_echo = Some(BootstrapOwnedRootEchoV0 {
            response: root.clone(),
            phase: next_phase,
        });
        authority.phase = Some(BootstrapAuthorityPhaseV0::Active(next_phase));
        Ok(())
    }

    fn record_bootstrap_issued_state(&self, state: &ObjectV0) -> Result<(), TransportErrorV0> {
        if state.metadata.uid.is_empty() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !authority.active || authority.issued_state.is_some() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        authority.issued_state = Some((state.reference.clone(), state.metadata.uid.clone()));
        Ok(())
    }

    /// True only for the collector-sealed bootstrap authority before its
    /// first subwrite polls. This is not a generic inactive-session probe.
    #[doc(hidden)]
    #[must_use]
    pub fn bootstrap_not_yet_polled(&self) -> bool {
        self.bootstrap.lock().is_ok_and(|authority| {
            authority.active
                && authority.phase == Some(BootstrapAuthorityPhaseV0::BootstrapNotYetPolled)
        })
    }

    fn consume_bootstrap_not_yet_polled_for_write(&self) -> Result<(), TransportErrorV0> {
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if authority.phase != Some(BootstrapAuthorityPhaseV0::BootstrapNotYetPolled) {
            return Ok(());
        }
        let phase = authority
            .pending_first_phase
            .take()
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        authority.phase = Some(BootstrapAuthorityPhaseV0::Active(phase));
        Ok(())
    }

    fn require_exact_root_metadata_response(
        &self,
        root: &ObjectV0,
    ) -> Result<(), TransportErrorV0> {
        if self.has_collected_bootstrap_fence() {
            let fence = self
                .bootstrap_fence
                .lock()
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
            if fence.terminal.is_some()
                || fence.disarmed
                || !fence
                    .metadata_echo
                    .as_ref()
                    .is_some_and(|response| bootstrap_response_matches(root, response))
            {
                return Err(TransportErrorV0::Stale("bootstrap root metadata response"));
            }
            return Ok(());
        }
        let authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !authority.active
            || authority.phase
                != Some(BootstrapAuthorityPhaseV0::Active(
                    BootstrapRootPhaseV0::RootStateLink,
                ))
        {
            return Err(TransportErrorV0::Stale("bootstrap root metadata authority"));
        }
        let exact = authority.exact_owned_root_echo.as_ref();
        if exact.is_some_and(|echo| {
            echo.phase == BootstrapRootPhaseV0::RootStateLink
                && bootstrap_response_matches(root, &echo.response)
        }) {
            Ok(())
        } else {
            Err(TransportErrorV0::Stale("bootstrap root metadata response"))
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn bootstrap_phase_is_complete(&self) -> bool {
        self.bootstrap.lock().is_ok_and(|authority| {
            authority.active
                && authority.phase
                    == Some(BootstrapAuthorityPhaseV0::Active(
                        BootstrapRootPhaseV0::Complete,
                    ))
        })
    }

    /// The State-link/status patch is bootstrap's final subwrite.  Mark that
    /// fact immediately after its write lease is claimed, rather than after
    /// its response is decoded: a root event in the response window may be
    /// buffered, but it cannot revoke this already-polled final write or
    /// manufacture another bootstrap subwrite.
    fn mark_bootstrap_final_write_polled(&self) -> Result<(), TransportErrorV0> {
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !authority.active
            || authority.phase
                != Some(BootstrapAuthorityPhaseV0::Active(
                    BootstrapRootPhaseV0::RootStateLink,
                ))
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
            BootstrapRootPhaseV0::Complete,
        ));
        Ok(())
    }

    fn retry_prewrite_epoch(&self) -> Result<u32, TransportErrorV0> {
        let epoch = self.gate.epoch();
        let max_epoch = u32::from(self.budget.max_prewrite_retries());
        let state = self.gate.state();
        if state == PrewriteGateStateV0::Invalidated {
            return Err(TransportErrorV0::PreWriteInvalidated);
        }
        if epoch >= max_epoch || state != PrewriteGateStateV0::PreWrite {
            return Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::PreWrite,
            ));
        }
        self.gate
            .state
            .compare_exchange(
                PrewriteGateV0::encode(epoch, PrewriteGateV0::PREWRITE),
                PrewriteGateV0::encode(epoch + 1, PrewriteGateV0::PREWRITE),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| {
                self.gate.notify_transition();
                epoch + 1
            })
            .map_err(|_| TransportErrorV0::PreWriteInvalidated)
    }

    #[cfg(test)]
    fn prove_bootstrap_after_exact_proof(&self) -> Result<(), TransportErrorV0> {
        if !self.is_bootstrap_sequence() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let epoch = self.gate.epoch();
        if self.bootstrap_phase_is_complete() {
            return self
                .gate
                .transition_epoch(
                    epoch,
                    PrewriteGateV0::AMBIGUOUS_HELD,
                    PrewriteGateV0::PROVEN,
                )
                .then_some(())
                .ok_or(TransportErrorV0::ManualRecoveryHold);
        }
        let next_epoch = epoch
            .checked_add(1)
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        if self.budget.prewrite_cutoff(next_epoch).is_none() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        self.gate
            .state
            .compare_exchange(
                PrewriteGateV0::encode(epoch, PrewriteGateV0::AMBIGUOUS_HELD),
                PrewriteGateV0::encode(
                    next_epoch,
                    if self.collector_bound {
                        PrewriteGateV0::BOOTSTRAP_PHASE_PENDING
                    } else {
                        PrewriteGateV0::PREWRITE
                    },
                ),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| self.gate.notify_transition())
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)
    }

    /// A response-loss proof is acknowledgement authority only when it closes
    /// the exact epoch whose write permit became ambiguous.  A later epoch,
    /// an already-proven direct response, or a timed-out proof is never
    /// interchangeable with that permit.
    fn prove_exact_response_loss_recovery(&self) -> Result<(), TransportErrorV0> {
        if self.is_bootstrap_sequence() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let epoch = self.gate.epoch();
        self.gate
            .state
            .compare_exchange(
                PrewriteGateV0::encode(epoch, PrewriteGateV0::AMBIGUOUS_HELD),
                PrewriteGateV0::encode(epoch, PrewriteGateV0::PROVEN),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)
    }
}

impl OperationBudgetV0 {
    fn deadline_for(&self, stage: FiniteIoStageV0) -> Instant {
        match stage {
            FiniteIoStageV0::ReadOnly
            | FiniteIoStageV0::WatchOpen
            | FiniteIoStageV0::RecoveryProof => self.inner.operation_deadline,
            // `PreWrite` is resolved through the session's current fixed
            // attempt epoch, never this aggregate deadline directly.
            FiniteIoStageV0::PreWrite => self.inner.prewrite_deadline,
            FiniteIoStageV0::WriteResponse => self.inner.write_response_deadline,
        }
    }

    fn remaining(&self, stage: FiniteIoStageV0) -> Result<Duration, TransportErrorV0> {
        self.deadline_for(stage)
            .checked_duration_since(Instant::now())
            .ok_or(TransportErrorV0::FiniteIoDeadline(stage))
    }

    fn has_response_proof_reserve(&self) -> bool {
        let now = Instant::now();
        now < self.inner.write_response_deadline
            && self.inner.write_response_deadline < self.inner.operation_deadline
    }
}

async fn within_operation<T>(
    operation: &OperationSessionV0,
    stage: FiniteIoStageV0,
    future: impl std::future::Future<Output = Result<T, TransportErrorV0>>,
) -> Result<T, TransportErrorV0> {
    let remaining = if stage == FiniteIoStageV0::PreWrite {
        operation
            .budget
            .prewrite_cutoff(operation.gate.epoch())
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            .ok_or(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::PreWrite,
            ))?
    } else {
        operation.budget.remaining(stage)?
    };
    tokio::time::timeout(remaining, future)
        .await
        .map_err(|_| TransportErrorV0::FiniteIoDeadline(stage))?
}

fn claim_first_write(operation: &OperationSessionV0) -> Result<SubwritePermitV0, TransportErrorV0> {
    let epoch = operation.gate.epoch();
    if operation
        .budget
        .prewrite_cutoff(epoch)
        .is_none_or(|cutoff| Instant::now() >= cutoff)
        || !operation.budget.has_response_proof_reserve()
    {
        return Err(TransportErrorV0::FiniteIoDeadline(
            FiniteIoStageV0::PreWrite,
        ));
    }
    if operation.has_collected_bootstrap_fence() {
        return operation.claim_collected_bootstrap_write(epoch);
    }
    operation
        .prewrite_gate()
        .try_mark_write_polled()
        .map_err(|state| match state {
            PrewriteGateStateV0::Invalidated => TransportErrorV0::PreWriteInvalidated,
            PrewriteGateStateV0::PreWrite
            | PrewriteGateStateV0::RootAuthPending
            | PrewriteGateStateV0::WritePolled
            | PrewriteGateStateV0::Proven
            | PrewriteGateStateV0::AmbiguousHeld
            | PrewriteGateStateV0::BootstrapPhasePending
            | PrewriteGateStateV0::BaselinePending
            | PrewriteGateStateV0::FinalProofPending => TransportErrorV0::ManualRecoveryHold,
        })?;
    // The collector-installed BootstrapNotYetPolled authority may advance
    // only after this exact write lease has won. A root event that wins the
    // preceding CAS still prevents the poll; after it succeeds, write-wins
    // semantics apply as for every later bootstrap subwrite.
    operation.consume_bootstrap_not_yet_polled_for_write()?;
    Ok(SubwritePermitV0 {
        gate: operation.gate.clone(),
        epoch,
        bootstrap: operation.is_bootstrap_sequence(),
        bootstrap_collected: operation.collector_bound,
    })
}

impl SubwritePermitV0 {
    fn prove(self) -> Result<(), TransportErrorV0> {
        if self.bootstrap {
            let next_epoch = self
                .epoch
                .checked_add(1)
                .ok_or(TransportErrorV0::ManualRecoveryHold)?;
            let next_phase = if self.bootstrap_collected {
                PrewriteGateV0::BOOTSTRAP_PHASE_PENDING
            } else {
                PrewriteGateV0::PREWRITE
            };
            self.gate
                .state
                .compare_exchange(
                    PrewriteGateV0::encode(self.epoch, PrewriteGateV0::WRITE_POLLED),
                    PrewriteGateV0::encode(next_epoch, next_phase),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map(|_| self.gate.notify_transition())
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)
        } else {
            self.gate
                .transition_epoch(
                    self.epoch,
                    PrewriteGateV0::WRITE_POLLED,
                    PrewriteGateV0::PROVEN,
                )
                .then_some(())
                .ok_or(TransportErrorV0::ManualRecoveryHold)
        }
    }

    /// Bootstrap's State-link is the one terminal root subwrite.  It proves
    /// the same write epoch directly instead of creating an unusable N+1
    /// phase slot.  A second claim therefore sees `Proven`, never a reopened
    /// prewrite state.
    fn prove_bootstrap_final(self) -> Result<(), TransportErrorV0> {
        if !self.bootstrap {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        self.gate
            .transition_epoch(
                self.epoch,
                PrewriteGateV0::WRITE_POLLED,
                PrewriteGateV0::PROVEN,
            )
            .then_some(())
            .ok_or(TransportErrorV0::ManualRecoveryHold)
    }
}

impl Drop for SubwritePermitV0 {
    fn drop(&mut self) {
        // A permit is consumed by `prove` only after a direct response or an
        // exact bootstrap proof. Any other exit happened after the write was
        // polled and therefore remains explicitly ambiguous.
        let _ = self.gate.transition_epoch(
            self.epoch,
            PrewriteGateV0::WRITE_POLLED,
            PrewriteGateV0::AMBIGUOUS_HELD,
        );
    }
}

/// The only namespace-scoped resources this transport will address.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ResourceKindV0 {
    WebApplication,
    OxideLoopState,
    AutomationRun,
    ConfigMap,
    /// Exactly the constrained apps/v1 Deployment v1 projection emitted by
    /// the controller TSX compiler.
    Deployment,
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
            verbs: &["get", "list", "watch", "create", "patch", "delete"],
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
            resource: ResourceKindV0::Deployment,
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
            Self::Deployment => "apps/v1",
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
            Self::Deployment => "Deployment",
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

/// The durable, versioned authority bundle left on an unlinked root between
/// `RootMetadata` and Root `StateLink`. It carries no process-local permission:
/// callers must still authenticate any extant State and use fresh API pins.
/// It exists so a restart during deletion can derive only the bounded cleanup
/// path, never adopt or relink a same-name replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapAuthorityBundleV1V0 {
    pub state_name: String,
    pub state_uid: String,
    pub state_spec_digest: String,
    pub proof_token: String,
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

const DELETION_BARRIER_LIVE: u8 = 0;
const DELETION_BARRIER_INVALIDATED: u8 = 1;
const DELETION_BARRIER_SEALED: u8 = 2;

/// A private point-in-time seal shared by deletion watch pumps and their
/// consumer.  A delivered relevant record wins before publication; a consumer
/// that loses `Live -> Sealed` has no acknowledgement capability.
#[derive(Clone, Debug)]
struct DeletionBarrierGateV0(Arc<AtomicU8>);

impl DeletionBarrierGateV0 {
    fn new() -> Self {
        Self(Arc::new(AtomicU8::new(DELETION_BARRIER_LIVE)))
    }

    #[must_use]
    fn invalidate(&self) -> bool {
        self.0
            .compare_exchange(
                DELETION_BARRIER_LIVE,
                DELETION_BARRIER_INVALIDATED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn is_live(&self) -> bool {
        self.0.load(Ordering::Acquire) == DELETION_BARRIER_LIVE
    }

    fn seal(&self) -> bool {
        self.0
            .compare_exchange(
                DELETION_BARRIER_LIVE,
                DELETION_BARRIER_SEALED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

/// Filters only facts which intersect the exact deletion authority. Unrelated
/// namespace churn remains outside this point-in-time snapshot.
#[derive(Clone, Debug)]
struct DeletionWatchFilterV0 {
    gate: DeletionBarrierGateV0,
    root_uid: String,
    root_name: String,
    state_name: String,
    tracked_names: Arc<Mutex<BTreeMap<ResourceKindV0, BTreeSet<String>>>>,
}

impl DeletionWatchFilterV0 {
    fn relevant(
        &self,
        resource: ResourceKindV0,
        event: &Result<WatchEventV0, TransportErrorV0>,
    ) -> bool {
        let Ok(event) = event else {
            return true;
        };
        let object = match event {
            WatchEventV0::Added(object)
            | WatchEventV0::Modified(object)
            | WatchEventV0::Deleted(object) => object,
            WatchEventV0::Bookmark { .. } => return false,
        };
        match resource {
            ResourceKindV0::WebApplication => object.reference.name == self.root_name,
            ResourceKindV0::OxideLoopState => object.reference.name == self.state_name,
            ResourceKindV0::ConfigMap
            | ResourceKindV0::Deployment
            | ResourceKindV0::Service
            | ResourceKindV0::AutomationRun => {
                object
                    .metadata
                    .owner_uids
                    .iter()
                    .any(|uid| uid == &self.root_uid)
                    || self
                        .tracked_names
                        .lock()
                        .expect("deletion filter lock")
                        .get(&resource)
                        .is_some_and(|names| names.contains(&object.reference.name))
            }
            ResourceKindV0::Event => false,
        }
    }
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
    #[error("finite Kubernetes I/O deadline expired at {0:?}")]
    FiniteIoDeadline(FiniteIoStageV0),
    #[error("unpolled prewrite request failed and may retry in its fixed epoch schedule: {0}")]
    PreWriteRetryable(String),
    #[error("an authenticated observation invalidated the command before its first write poll")]
    PreWriteInvalidated,
    #[error(
        "the fresh bootstrap exact-name list/watch/reread cut was superseded before it could seal"
    )]
    FreshBootstrapCutSuperseded,
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

/// Result of the bounded cleanup path for a root deleted between durable
/// bootstrap metadata and installation of its State link.  It is intentionally
/// narrower than normal bootstrap recovery: this path can only tear down the
/// exact State already committed by the v1 bundle, never complete or adopt a
/// bootstrap.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnlinkedBootstrapDeletionCleanupV0 {
    /// The root carries no v1 bootstrap marker, so ordinary loader recovery
    /// remains responsible for it.
    NotApplicable,
    /// The original State was proven absent and the exact root finalizer was
    /// removed (or its response was exactly proven absent).
    Completed,
}

/// Root-only recovery permits exactly two state-absence follow-up effects.
/// Their tokens are separate from the durable receipt proof and are checked
/// against fresh root/state pins before any child list or finalizer write.
#[derive(Clone, Copy)]
enum RootOnlyCleanupCommandV0 {
    VerifyOwnedChildrenAbsent,
    RemoveRootFinalizer,
}

/// Private phase result for the bounded unlinked-v1 cleanup loop. It is kept
/// separate from the public completion result so no caller can mistake a
/// still-present State for a successful deletion acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnlinkedBootstrapDeletionStepV0 {
    NotApplicable,
    StateFinalizerRemoved,
    AwaitingStateAbsence,
    Completed,
}

/// Opaque watch stream with no Kubernetes stream or event in its public API.
#[derive(Debug)]
pub struct WatchHandleV0 {
    events: tokio::sync::mpsc::Receiver<Result<WatchEventV0, TransportErrorV0>>,
    overflowed: Arc<AtomicBool>,
    terminal: Option<TransportErrorV0>,
    pump_abort: Option<tokio::task::AbortHandle>,
    // Test-injected handles do not have a kube stream pump.  Keep the same
    // invalidation edge at the handle boundary so the production deletion
    // barrier is exercised with the only substituted component being its I/O
    // source.
    deletion_filter: Option<(ResourceKindV0, DeletionWatchFilterV0)>,
}

impl WatchHandleV0 {
    /// Upper bound for one retained-watch completion/pre-command drain.  It is
    /// derived from the bounded channel itself, so callers never spin until a
    /// concurrent pump happens to go quiet.
    #[must_use]
    pub fn drain_capacity(&self) -> usize {
        self.events.max_capacity()
    }

    fn terminal_before_read(&mut self) -> Option<TransportErrorV0> {
        self.terminal.clone()
    }

    fn finish_read(
        &mut self,
        event: Result<WatchEventV0, TransportErrorV0>,
    ) -> Result<WatchEventV0, TransportErrorV0> {
        if self.overflowed.load(Ordering::Acquire) {
            self.terminal = Some(TransportErrorV0::WatchOverflow);
            return Err(TransportErrorV0::WatchOverflow);
        }
        // Any stream failure invalidates the epoch; never yield queued events
        // after a malformed bookmark, conflict, or decode/transport failure.
        if let Err(error) = &event {
            self.terminal = Some(error.clone());
        }
        self.invalidate_deletion_filter(&event);
        event
    }

    /// Test-only watch injection. This is feature-gated and unavailable from
    /// the production transport capability surface.
    #[cfg(any(test, feature = "test-support"))]
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
                pump_abort: None,
                deletion_filter: None,
            },
        )
    }

    /// Marks a scripted retained stream as overflowed. This is deliberately
    /// test-only; production overflow is set only by the bounded pump.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_mark_overflow(&self) {
        self.overflowed.store(true, Ordering::Release);
    }

    #[cfg(any(test, feature = "test-support"))]
    fn install_deletion_filter(&mut self, resource: ResourceKindV0, filter: DeletionWatchFilterV0) {
        self.deletion_filter = Some((resource, filter));
    }

    fn invalidate_deletion_filter(&self, event: &Result<WatchEventV0, TransportErrorV0>) {
        if self
            .deletion_filter
            .as_ref()
            .is_some_and(|(resource, filter)| filter.relevant(*resource, event))
        {
            // Real kube pumps perform this CAS before enqueueing.  A scripted
            // handle has no pump, so it performs the identical CAS before the
            // barrier consumer receives the injected record.
            let _ = self
                .deletion_filter
                .as_ref()
                .expect("checked deletion filter")
                .1
                .gate
                .invalidate();
        }
    }

    /// Reads the next event. A 410 or an upstream error is mapped to a domain
    /// failure so the caller must invalidate its cache epoch.
    pub async fn next_event(&mut self) -> Result<WatchEventV0, TransportErrorV0> {
        if let Some(error) = self.terminal_before_read() {
            return Err(error);
        }
        let event = self
            .events
            .recv()
            .await
            .unwrap_or(Err(TransportErrorV0::WatchGone));
        self.finish_read(event)
    }

    /// Returns one already-buffered event without waiting.  Closing a watch is
    /// terminal just as it is for [`Self::next_event`]; callers must restart
    /// the epoch instead of treating an empty local buffer as authority.
    pub fn try_next_event(&mut self) -> Result<Option<WatchEventV0>, TransportErrorV0> {
        if let Some(error) = self.terminal_before_read() {
            return Err(error);
        }
        if self.overflowed.load(Ordering::Acquire) {
            self.terminal = Some(TransportErrorV0::WatchOverflow);
            return Err(TransportErrorV0::WatchOverflow);
        }
        match self.events.try_recv() {
            Ok(event) => self.finish_read(event).map(Some),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                self.terminal = Some(TransportErrorV0::WatchGone);
                Err(TransportErrorV0::WatchGone)
            }
        }
    }

    /// Replaces this handle with an operation-specific forwarding boundary.
    /// The pump owns the original receiver until close, commits target records
    /// before attempting output publication, and returns the original handle
    /// once the collector has completed its one command.
    #[doc(hidden)]
    pub fn start_collected_bootstrap_fence(
        &mut self,
        operation: &OperationSessionV0,
    ) -> Result<(), TransportErrorV0> {
        if !operation.has_collected_bootstrap_fence() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let bootstrap_address = {
            let mut fence = operation
                .bootstrap_fence
                .lock()
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
            if fence.pump_active || fence.terminal.is_some() || fence.disarmed {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
            let address = fence
                .address
                .clone()
                .ok_or(TransportErrorV0::ManualRecoveryHold)?;
            fence.pump_active = true;
            address
        };

        let capacity = self.drain_capacity().max(1);
        let (placeholder_sender, placeholder_receiver) = tokio::sync::mpsc::channel(1);
        drop(placeholder_sender);
        let upstream = Self {
            events: std::mem::replace(&mut self.events, placeholder_receiver),
            overflowed: Arc::clone(&self.overflowed),
            terminal: self.terminal.take(),
            pump_abort: self.pump_abort.take(),
            deletion_filter: self.deletion_filter.take(),
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        self.events = receiver;
        let output_failure = Arc::new(AtomicBool::new(false));
        self.overflowed = Arc::clone(&output_failure);
        self.terminal = None;

        let (close_sender, mut close_receiver) = oneshot::channel();
        let (returned_sender, returned_receiver) = oneshot::channel();
        let runtime = operation.bootstrap_pump_runtime();
        // Do not move `runtime` into the task. `runtime` owns this task's
        // JoinHandle, and a strong reverse edge would make Drop incapable of
        // reaching its abort/reaper fallback.
        let task_fence = Arc::clone(&runtime.fence);
        let task_gate = runtime.gate.clone();
        let task_wake = runtime.wake.clone();
        let task_output_failure = Arc::clone(&output_failure);
        let pump = tokio::spawn(async move {
            let mut upstream = upstream;
            let mut exit = BootstrapPumpExitGuardV0 {
                fence: Arc::clone(&task_fence),
                gate: task_gate.clone(),
                wake: task_wake.clone(),
                output_failure: Arc::clone(&task_output_failure),
                clean: false,
            };
            // A first `try_next_event` is a real poll of the upstream
            // receiver.  Pending is a valid handshake edge; a ready record
            // is handled below and marks the edge only after it is committed.
            let mut first_ready = match upstream.try_next_event() {
                Ok(None) => {
                    complete_bootstrap_first_poll(&task_fence, &task_wake);
                    None
                }
                Ok(Some(event)) => Some(Ok(event)),
                Err(error) => Some(Err(error)),
            };
            loop {
                let (next, is_first_ready) = if let Some(ready) = first_ready.take() {
                    (ready, true)
                } else {
                    let event = upstream.next_event();
                    futures_util::pin_mut!(event);
                    match futures_util::future::select(&mut close_receiver, event).await {
                        futures_util::future::Either::Left((_closed, _pending_event)) => {
                            break;
                        }
                        futures_util::future::Either::Right((event, _pending_close)) => {
                            (event, false)
                        }
                    }
                };
                match next {
                    Ok(WatchEventV0::Bookmark { .. }) => {
                        // Named bootstrap watches disable bookmarks.  Treat a
                        // defensive injected one as a no-op, never as a fence
                        // record or output observation.
                        if is_first_ready {
                            complete_bootstrap_first_poll(&task_fence, &task_wake);
                        }
                    }
                    Ok(event) => {
                        let targets_root =
                            bootstrap_watch_targets_address(&event, &bootstrap_address);
                        // An exact-name watch is a capability, not a broad
                        // advisory stream. A leaked object means the API
                        // selection contract has failed. Never forward or
                        // ignore it: it invalidates an armed operation, and
                        // after disarm it remains a retained-watch terminal
                        // so the driver must rebarrier before another command.
                        if !targets_root {
                            task_output_failure.store(true, Ordering::Release);
                            let disarmed = task_fence.lock().is_ok_and(|mut fence| {
                                if is_first_ready {
                                    fence.first_poll_complete = true;
                                }
                                fence.disarmed
                            });
                            if disarmed {
                                upstream.terminal = Some(TransportErrorV0::ManualRecoveryHold);
                            } else {
                                latch_bootstrap_terminal(
                                    &task_fence,
                                    &task_gate,
                                    &task_wake,
                                    TransportErrorV0::PreWriteInvalidated,
                                );
                            }
                            notify_bootstrap_fence(&task_wake);
                            break;
                        }
                        // The final proof disarms while it still holds this
                        // fence lock.  Later retained-watch traffic belongs
                        // to the restored ordinary watch, so it must never
                        // reopen this operation's committed/enqueued counters.
                        let (sequence, commit_failed) = if targets_root {
                            match task_fence.lock() {
                                Ok(fence) if fence.disarmed => (None, false),
                                Ok(mut fence) => {
                                    let committed = fence
                                        .commit(BootstrapFenceRecordKindV0::Data(event.clone()));
                                    if committed.is_ok() && is_first_ready {
                                        fence.first_poll_complete = true;
                                    }
                                    match committed {
                                        Ok(sequence) => (Some(sequence), false),
                                        Err(_) => (None, true),
                                    }
                                }
                                Err(_) => (None, true),
                            }
                        } else {
                            (None, false)
                        };
                        if commit_failed {
                            task_output_failure.store(true, Ordering::Release);
                            latch_bootstrap_terminal(
                                &task_fence,
                                &task_gate,
                                &task_wake,
                                TransportErrorV0::ManualRecoveryHold,
                            );
                            break;
                        }
                        if sequence.is_some() {
                            notify_bootstrap_fence(&task_wake);
                        }
                        match sender.try_send(Ok(event)) {
                            Ok(()) => {
                                if let Some(sequence) = sequence {
                                    let marked = task_fence
                                        .lock()
                                        .ok()
                                        .and_then(|mut fence| fence.mark_enqueued(sequence).ok())
                                        .is_some();
                                    if !marked {
                                        task_output_failure.store(true, Ordering::Release);
                                        latch_bootstrap_terminal(
                                            &task_fence,
                                            &task_gate,
                                            &task_wake,
                                            TransportErrorV0::ManualRecoveryHold,
                                        );
                                        break;
                                    }
                                    notify_bootstrap_fence(&task_wake);
                                }
                            }
                            Err(error) => {
                                let terminal_error = match error {
                                    tokio::sync::mpsc::error::TrySendError::Full(_) => {
                                        TransportErrorV0::WatchOverflow
                                    }
                                    tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                                        TransportErrorV0::ManualRecoveryHold
                                    }
                                };
                                task_output_failure.store(true, Ordering::Release);
                                latch_bootstrap_terminal(
                                    &task_fence,
                                    &task_gate,
                                    &task_wake,
                                    terminal_error,
                                );
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        let (committed, commit_failed) = match task_fence.lock() {
                            Ok(fence) if fence.disarmed => (None, false),
                            Ok(mut fence) => {
                                let committed = fence
                                    .commit(BootstrapFenceRecordKindV0::Terminal(error.clone()));
                                if committed.is_ok() && is_first_ready {
                                    fence.first_poll_complete = true;
                                }
                                match committed {
                                    Ok(sequence) => (Some(sequence), false),
                                    Err(_) => (None, true),
                                }
                            }
                            Err(_) => (None, true),
                        };
                        if commit_failed {
                            task_output_failure.store(true, Ordering::Release);
                            latch_bootstrap_terminal(
                                &task_fence,
                                &task_gate,
                                &task_wake,
                                TransportErrorV0::ManualRecoveryHold,
                            );
                            break;
                        }
                        if committed.is_some() {
                            latch_bootstrap_terminal(
                                &task_fence,
                                &task_gate,
                                &task_wake,
                                error.clone(),
                            );
                            notify_bootstrap_fence(&task_wake);
                        }
                        if let Some(sequence) = committed {
                            if sender.try_send(Err(error.clone())).is_ok() {
                                let marked = task_fence
                                    .lock()
                                    .ok()
                                    .and_then(|mut fence| fence.mark_enqueued(sequence).ok())
                                    .is_some();
                                if !marked {
                                    latch_bootstrap_terminal(
                                        &task_fence,
                                        &task_gate,
                                        &task_wake,
                                        TransportErrorV0::ManualRecoveryHold,
                                    );
                                }
                                notify_bootstrap_fence(&task_wake);
                            } else {
                                task_output_failure.store(true, Ordering::Release);
                            }
                        } else if sender.try_send(Err(error)).is_err() {
                            // The completed operation is already disarmed;
                            // retain this as an ordinary-watch overflow rather
                            // than mutating the finished operation fence.
                            task_output_failure.store(true, Ordering::Release);
                        }
                        break;
                    }
                }
            }
            if returned_sender.send(upstream).is_ok() {
                exit.clean = true;
            }
        });
        self.pump_abort = Some(pump.abort_handle());
        if operation
            .install_bootstrap_pump(BootstrapPumpControlV0 {
                close: close_sender,
                returned_watch: returned_receiver,
                abort: pump.abort_handle(),
                join: pump,
            })
            .is_err()
        {
            if let Some(abort) = self.pump_abort.take() {
                abort.abort();
            }
            operation
                .bootstrap_pump
                .latch_terminal(TransportErrorV0::ManualRecoveryHold);
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok(())
    }

    async fn restore_after_bootstrap_fence(
        &mut self,
        upstream: WatchHandleV0,
        bootstrap_address: &RootAddressV0,
        remaining: Duration,
    ) -> Result<(), TransportErrorV0> {
        let _ = self.pump_abort.take();
        let (placeholder_sender, placeholder_receiver) = tokio::sync::mpsc::channel(1);
        drop(placeholder_sender);
        let output = std::mem::replace(&mut self.events, placeholder_receiver);
        let capacity = output.max_capacity().max(1);
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        // Keep the pump's overflow bit. A post-disarm publication failure is
        // retained-watch terminal evidence, not something a successful close
        // may erase by allocating a fresh flag.
        let overflowed = Arc::clone(&self.overflowed);
        let bootstrap_address = bootstrap_address.clone();
        let (ready_sender, mut ready_receiver) = oneshot::channel();
        let bridge = tokio::spawn(async move {
            let mut output = output;
            let mut upstream = upstream;
            // Drain every already-published pump record first, then perform
            // a real nonblocking poll of the returned upstream receiver
            // before declaring the ordinary watch usable.  The extra yield
            // and second poll closes the handoff scheduling edge: an event
            // made ready by the pump's close turn cannot hide behind a fresh
            // bridge until the driver's next command has already started.
            while let Ok(event) = output.try_recv() {
                if !bootstrap_bridge_record_targets_address(&event, &bootstrap_address) {
                    overflowed.store(true, Ordering::Release);
                    let _ = ready_sender.send(());
                    return;
                }
                let terminal = event.is_err();
                if sender.try_send(event).is_err() {
                    overflowed.store(true, Ordering::Release);
                    let _ = ready_sender.send(());
                    return;
                }
                if terminal {
                    // The receiver will return its own terminal result;
                    // no upstream record may be read past it.
                    let _ = ready_sender.send(());
                    return;
                }
            }
            for delayed_poll in 0..2 {
                match upstream.try_next_event() {
                    Ok(Some(event)) => {
                        if !bootstrap_bridge_event_targets_address(&event, &bootstrap_address) {
                            overflowed.store(true, Ordering::Release);
                            let _ = ready_sender.send(());
                            return;
                        }
                        if sender.try_send(Ok(event)).is_err() {
                            overflowed.store(true, Ordering::Release);
                            let _ = ready_sender.send(());
                            return;
                        }
                    }
                    Ok(None) if delayed_poll == 0 => {
                        tokio::task::yield_now().await;
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if sender.try_send(Err(error)).is_err() {
                            overflowed.store(true, Ordering::Release);
                        }
                        let _ = ready_sender.send(());
                        return;
                    }
                }
            }
            let _ = ready_sender.send(());
            while let Some(event) = output.recv().await {
                if !bootstrap_bridge_record_targets_address(&event, &bootstrap_address) {
                    overflowed.store(true, Ordering::Release);
                    return;
                }
                let terminal = event.is_err();
                if sender.try_send(event).is_err() {
                    overflowed.store(true, Ordering::Release);
                    return;
                }
                if terminal {
                    return;
                }
            }
            loop {
                let event = upstream.next_event().await;
                if !bootstrap_bridge_record_targets_address(&event, &bootstrap_address) {
                    overflowed.store(true, Ordering::Release);
                    return;
                }
                let terminal = event.is_err();
                if sender.try_send(event).is_err() {
                    overflowed.store(true, Ordering::Release);
                    return;
                }
                if terminal {
                    return;
                }
            }
        });
        self.events = receiver;
        self.terminal = None;
        self.pump_abort = Some(bridge.abort_handle());
        self.deletion_filter = None;
        match tokio::time::timeout(remaining, &mut ready_receiver).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(TransportErrorV0::ManualRecoveryHold),
            Err(_) => {
                if let Some(abort) = self.pump_abort.take() {
                    abort.abort();
                }
                Err(TransportErrorV0::FiniteIoDeadline(
                    FiniteIoStageV0::ReadOnly,
                ))
            }
        }
    }
}

impl Drop for WatchHandleV0 {
    fn drop(&mut self) {
        if let Some(abort) = self.pump_abort.take() {
            abort.abort();
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
    /// Mints one opaque, immutable operation budget and its private
    /// pre-write cancellation gate.  The caller may carry this session only
    /// while it owns one sealed command future; deadline construction remains
    /// transport authority.
    #[must_use]
    pub fn begin_operation_session(&self) -> OperationSessionV0 {
        OperationSessionV0::production(false)
    }

    /// Mints the private session used only by the runtime's pinned command
    /// collector. Its gate may be invalidated by authenticated buffered
    /// observations before the first write poll.
    #[doc(hidden)]
    #[must_use]
    pub fn begin_collected_operation_session(&self) -> OperationSessionV0 {
        OperationSessionV0::production(true)
    }

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

    /// Completes only the deletion side of an interrupted bootstrap.  A v1
    /// bundle is a cleanup capability, never permission to install the
    /// missing State link or resume normal bootstrap after the root starts
    /// deleting.  Every pass re-reads the raw root and exact deterministic
    /// State before it considers a write; an old UID, changed digest, partial
    /// bundle, or same-name replacement is therefore a zero-write hold.
    #[doc(hidden)]
    pub async fn cleanup_unlinked_deleting_bootstrap_v1(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<UnlinkedBootstrapDeletionCleanupV0, TransportErrorV0> {
        let root_ref = ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: namespace.into(),
            name: name.into(),
        };
        validate_reference(&root_ref)?;
        // Cleanup can require two independently conditional writes (State
        // finalizer, then root finalizer).  Each gets a fresh one-write lease,
        // while this outer budget prevents a disappearing child from turning
        // the restart path into an unbounded retry loop.
        let cleanup_budget = OperationBudgetV0::production();
        loop {
            cleanup_budget.remaining(FiniteIoStageV0::ReadOnly)?;
            let operation = self.begin_operation_session();
            match self
                .cleanup_unlinked_deleting_bootstrap_v1_once(&root_ref, &operation)
                .await
            {
                Ok(UnlinkedBootstrapDeletionStepV0::NotApplicable) => {
                    return Ok(UnlinkedBootstrapDeletionCleanupV0::NotApplicable);
                }
                Ok(UnlinkedBootstrapDeletionStepV0::Completed) => {
                    return Ok(UnlinkedBootstrapDeletionCleanupV0::Completed);
                }
                Ok(
                    UnlinkedBootstrapDeletionStepV0::StateFinalizerRemoved
                    | UnlinkedBootstrapDeletionStepV0::AwaitingStateAbsence,
                ) => {
                    // Yield before the next exact GET. This gives the API
                    // server/garbage collector a scheduling edge and avoids
                    // monopolising a worker while preserving the fixed outer
                    // deadline.
                    tokio::task::yield_now().await;
                    let remaining = cleanup_budget.remaining(FiniteIoStageV0::ReadOnly)?;
                    tokio::time::sleep(Duration::from_millis(2).min(remaining)).await;
                }
                Err(
                    TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::PreWrite)
                    | TransportErrorV0::PreWriteRetryable(_),
                ) => {
                    // This pass never polled a write. Start a fresh exact
                    // read/lease pass, still bounded by `cleanup_budget`.
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn cleanup_unlinked_deleting_bootstrap_v1_once(
        &self,
        root_ref: &ObjectRefV0,
        operation: &OperationSessionV0,
    ) -> Result<UnlinkedBootstrapDeletionStepV0, TransportErrorV0> {
        let Some(root) = self
            .reread_prewrite_in_operation(root_ref, operation)
            .await?
        else {
            // The generic loader will return `None`; no cleanup write is
            // appropriate when the root itself has already disappeared.
            return Ok(UnlinkedBootstrapDeletionStepV0::NotApplicable);
        };
        let Some(bundle) = validate_unlinked_deleting_bootstrap_root(&root)? else {
            return Ok(UnlinkedBootstrapDeletionStepV0::NotApplicable);
        };
        let state_ref = ObjectRefV0 {
            resource: ResourceKindV0::OxideLoopState,
            namespace: root.reference.namespace.clone(),
            name: bundle.state_name.clone(),
        };
        let state = self
            .reread_prewrite_in_operation(&state_ref, operation)
            .await?;
        let Some(state) = state else {
            self.remove_unlinked_bootstrap_root_finalizer_in_operation(&root, &bundle, operation)
                .await?;
            return Ok(UnlinkedBootstrapDeletionStepV0::Completed);
        };
        validate_unlinked_deleting_bootstrap_state(&root, &state, &bundle)?;
        if !strict_finalizers(&state)?.contains(&STATE_FINALIZER) {
            // The exact old UID still exists.  It may be in the GC handoff
            // after an earlier process removed its finalizer, but the root
            // finalizer remains protected until a later pass proves absence.
            return Ok(UnlinkedBootstrapDeletionStepV0::AwaitingStateAbsence);
        }
        self.remove_unlinked_bootstrap_state_finalizer_in_operation(
            &root, &state, &bundle, operation,
        )
        .await?;
        Ok(UnlinkedBootstrapDeletionStepV0::StateFinalizerRemoved)
    }

    async fn remove_unlinked_bootstrap_state_finalizer_in_operation(
        &self,
        root: &ObjectV0,
        state: &ObjectV0,
        bundle: &BootstrapAuthorityBundleV1V0,
        operation: &OperationSessionV0,
    ) -> Result<(), TransportErrorV0> {
        let index =
            finalizer_index(state, STATE_FINALIZER).ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let patch = finalizer_removal_patch(state, index, STATE_FINALIZER)?;
        let permit = claim_first_write(operation)?;
        let result = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(ResourceKindV0::OxideLoopState, &state.reference.namespace)
                .patch(
                    &state.reference.name,
                    &PatchParams::default(),
                    &Patch::<Value>::Json(patch),
                )
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::JsonPatch,
                        ResourceKindV0::OxideLoopState,
                    )
                })
        })
        .await;
        match result {
            Ok(written) => {
                let written = to_domain(
                    ResourceKindV0::OxideLoopState,
                    &state.reference.namespace,
                    written,
                )?;
                if !bootstrap_finalizer_removal_proof(&written, state, STATE_FINALIZER)
                    || !bootstrap_authority_bundle_v1_matches_state(root, &written, bundle)
                {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                permit.prove()?;
                Ok(())
            }
            Err(error) if bootstrap_write_response_lost(&error) => {
                drop(permit);
                let observed = self.recovery_reread(&state.reference, operation).await?;
                match observed {
                    None => operation.prove_exact_response_loss_recovery(),
                    Some(observed)
                        if bootstrap_finalizer_removal_proof(&observed, state, STATE_FINALIZER)
                            && bootstrap_authority_bundle_v1_matches_state(
                                root, &observed, bundle,
                            ) =>
                    {
                        operation.prove_exact_response_loss_recovery()
                    }
                    Some(_) => Err(TransportErrorV0::ManualRecoveryHold),
                }
            }
            Err(_) => Err(TransportErrorV0::ManualRecoveryHold),
        }
    }

    async fn remove_unlinked_bootstrap_root_finalizer_in_operation(
        &self,
        root: &ObjectV0,
        bundle: &BootstrapAuthorityBundleV1V0,
        operation: &OperationSessionV0,
    ) -> Result<(), TransportErrorV0> {
        // The immediately preceding exact name GET established absence of the
        // bundle's original UID. Revalidate the root/bundle before binding
        // this final patch to its current UID and resourceVersion.
        validate_unlinked_deleting_bootstrap_root_with_bundle(root, bundle)?;
        let index =
            finalizer_index(root, ROOT_FINALIZER).ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let patch = finalizer_removal_patch(root, index, ROOT_FINALIZER)?;
        let permit = claim_first_write(operation)?;
        let result = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(ResourceKindV0::WebApplication, &root.reference.namespace)
                .patch(
                    &root.reference.name,
                    &PatchParams::default(),
                    &Patch::<Value>::Json(patch),
                )
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::JsonPatch,
                        ResourceKindV0::WebApplication,
                    )
                })
        })
        .await;
        match result {
            Ok(written) => {
                let written = to_domain(
                    ResourceKindV0::WebApplication,
                    &root.reference.namespace,
                    written,
                )?;
                if !bootstrap_finalizer_removal_proof(&written, root, ROOT_FINALIZER)
                    || !bootstrap_unlinked_root_after_finalizer_removal_matches(
                        &written, root, bundle,
                    )
                {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                permit.prove()?;
                Ok(())
            }
            Err(error) if bootstrap_write_response_lost(&error) => {
                drop(permit);
                let observed = self.recovery_reread(&root.reference, operation).await?;
                match observed {
                    None => operation.prove_exact_response_loss_recovery(),
                    Some(observed)
                        if bootstrap_finalizer_removal_proof(&observed, root, ROOT_FINALIZER)
                            && bootstrap_unlinked_root_after_finalizer_removal_matches(
                                &observed, root, bundle,
                            ) =>
                    {
                        operation.prove_exact_response_loss_recovery()
                    }
                    Some(_) => Err(TransportErrorV0::ManualRecoveryHold),
                }
            }
            Err(_) => Err(TransportErrorV0::ManualRecoveryHold),
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
        self.dispatch_in_operation(address, command, &self.begin_operation_session())
            .await
    }

    /// Executes one sealed command under one transport-minted operation
    /// session.  Only the runtime's pinned collector receives this entry
    /// point; direct callers use [`Self::dispatch`] and get a noninvalidating
    /// operation-owned session.
    pub async fn dispatch_in_operation(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        if matches!(command, CommandV0::EnsureBootstrap { .. }) {
            // The collector seal is the sole duplicate authority.  Check it
            // before `begin_bootstrap_sequence` and, crucially, before this
            // dispatch can perform the first list/watch/GET operation.
            session.validate_collected_bootstrap_fence_input(address, command)?;
            session.begin_bootstrap_sequence()?;
        }
        loop {
            match self
                .dispatch_once_in_operation(address, command, session)
                .await
            {
                Err(
                    TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::PreWrite)
                    | TransportErrorV0::PreWriteRetryable(_),
                ) => {
                    // The command has not reached `WritePolled`; retry only
                    // its original frozen request under the next budget slot.
                    // This loop remains inside the caller's pinned future, so
                    // the watch collector keeps every buffered fact.
                    session.retry_prewrite_epoch()?;
                }
                outcome => return outcome,
            }
        }
    }

    async fn dispatch_once_in_operation(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        validate_address(address)?;
        if empty_state_resource_version_sentinel(command) {
            return match command {
                CommandV0::VerifyOwnedChildrenAbsent {
                    token,
                    preconditions,
                } => {
                    self.verify_root_only_cleanup(address, token, preconditions, session)
                        .await
                }
                CommandV0::RemoveRootFinalizer {
                    token,
                    preconditions,
                } => {
                    self.remove_root_only_finalizer(address, token, preconditions, session)
                        .await
                }
                _ => Err(TransportErrorV0::ManualRecoveryHold),
            };
        }
        match command {
            CommandV0::EnsureBootstrap { .. } => {
                self.execute_bootstrap(address, command, session).await
            }
            CommandV0::Persist {
                state,
                preconditions,
                operation,
            } => {
                let fence = match self
                    .reread_fence_in_operation(address, preconditions, session)
                    .await
                {
                    Ok(fence) => fence,
                    Err(error) => return Err(error),
                };
                validate_persist_force_authority(address, &fence, preconditions)?;
                let mut desired = state_object(&fence.state, state);
                inject_operation_proof(
                    &mut desired.value,
                    &fence.root.metadata.uid,
                    &operation.token,
                )?;
                let written = match self
                    .apply(
                        &desired,
                        ResourceKindV0::OxideLoopState,
                        &preconditions.state_resource_version,
                        true,
                        session,
                    )
                    .await
                {
                    Ok(written) => written,
                    // The request was issued exactly once. A timeout is as
                    // ambiguous as a truncated response, so prove the patch
                    // by an exact reread rather than replaying it.
                    Err(TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)) => {
                        return self
                            .recover_persist(address, state, preconditions, operation, session)
                            .await;
                    }
                    Err(error)
                        if may_recover_response_loss(&error) || is_state_write_conflict(&error) =>
                    {
                        return self
                            .recover_persist(address, state, preconditions, operation, session)
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
                let fence = match self
                    .reread_fence_in_operation(address, preconditions, session)
                    .await
                {
                    Ok(fence) => fence,
                    Err(error) => return Err(error),
                };
                if fence.root.metadata.uid != *root_uid
                    || old_root_resource_version != &preconditions.root_resource_version
                {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                validate_root_status_force_authority(
                    address,
                    &fence,
                    preconditions,
                    token,
                    status,
                )?;
                let desired = root_status_object(&fence.root, status);
                let written = match self
                    .apply_status(
                        &desired,
                        ResourceKindV0::WebApplication,
                        old_root_resource_version,
                        true,
                        session,
                    )
                    .await
                {
                    Ok(written) => written,
                    Err(error)
                        if matches!(
                            error,
                            TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)
                        ) || may_recover_response_loss(&error)
                            || is_root_status_write_conflict(&error) =>
                    {
                        return self
                            .recover_root_status_in_operation(
                                address,
                                token,
                                root_uid,
                                old_root_resource_version,
                                status,
                                session,
                                true,
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
                self.execute_mutation(
                    address,
                    transaction_id,
                    attempt_token,
                    &commands[0],
                    session,
                )
                .await
            }
            CommandV0::EnsureAutomationRun {
                token,
                name,
                spec,
                preconditions,
            } => {
                self.execute_automation(address, token, name, spec, preconditions, session)
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
                let _fence = self
                    .reread_fence_in_operation(address, preconditions, session)
                    .await?;
                let reference = ObjectRefV0 {
                    resource: ResourceKindV0::AutomationRun,
                    namespace: address.namespace.clone(),
                    name: name.clone(),
                };
                if reference.namespace != address.namespace {
                    return Err(TransportErrorV0::UnsupportedTarget);
                }
                self.delete(
                    &reference,
                    ResourceKindV0::AutomationRun,
                    uid,
                    resource_version,
                    session,
                )
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
                let fence = self
                    .reread_fence_in_operation(address, preconditions, session)
                    .await?;
                for resource in [
                    ResourceKindV0::ConfigMap,
                    ResourceKindV0::Deployment,
                    ResourceKindV0::Service,
                    ResourceKindV0::AutomationRun,
                ] {
                    if self
                        .list_exact_in_operation(resource, &address.namespace, session)
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
                let fence = self
                    .reread_fence_in_operation(address, preconditions, session)
                    .await?;
                validate_linked_deletion_state_finalizer_authority(
                    address,
                    &fence,
                    preconditions,
                    token,
                )?;
                let written = self
                    .remove_finalizer_in_operation(&fence.state, STATE_FINALIZER, session)
                    .await
                    .map_err(|_| ambiguous_delete_or_finalizer())?;
                if !finalizer_response_matches(&fence.state, &written, STATE_FINALIZER) {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::StateFinalizerRemoved {
                        token: token.clone(),
                        state_uid: fence.state.metadata.uid,
                        old_state_resource_version: fence.state.metadata.resource_version,
                        new_state_resource_version: written.metadata.resource_version,
                    },
                )))
            }
            CommandV0::DeleteRetiredState {
                token,
                preconditions,
            } => {
                self.delete_retired_state(address, token, preconditions, session)
                    .await
            }
            CommandV0::RemoveRootFinalizer {
                token,
                preconditions,
            } => {
                let (root_ref, state_ref) = address.references();
                let root = self
                    .reread_prewrite_in_operation(&root_ref, session)
                    .await?
                    .ok_or(TransportErrorV0::ManualRecoveryHold)?;
                validate_linked_root_finalizer_after_state_absence(
                    address,
                    &root,
                    preconditions,
                    token,
                )?;
                // The original UID must be absent, not merely unlinked or
                // finalizer-free.  A same-name object (including a replacement
                // UID) is a hard hold and remains untouched.
                if self
                    .reread_prewrite_in_operation(&state_ref, session)
                    .await?
                    .is_some()
                {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                let written = self
                    .remove_finalizer_in_operation(&root, ROOT_FINALIZER, session)
                    .await
                    .map_err(|_| ambiguous_delete_or_finalizer())?;
                if !finalizer_response_matches(&root, &written, ROOT_FINALIZER) {
                    return Err(TransportErrorV0::ManualRecoveryHold);
                }
                Ok(CommandOutcomeV0::Event(Box::new(
                    oxideloop_controller_core::EventV0::RootFinalizerRemoved {
                        token: token.clone(),
                        root_uid: root.metadata.uid,
                        old_root_resource_version: root.metadata.resource_version,
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
                match self.reread_in_operation(&root_ref, session).await? {
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
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (root_ref, state_ref) = address.references();
        let observed = self
            .recovery_reread(&state_ref, session)
            .await
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        if observed.metadata.uid != preconditions.state_uid
            || observed.metadata.resource_version == preconditions.state_resource_version
            || !has_finalizer(&observed, STATE_FINALIZER)
            || !has_exact_canonical_owner(&observed, &root_ref, &preconditions.root_uid)
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
        session.prove_exact_response_loss_recovery()?;
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::Persisted {
                token: operation.token.clone(),
                old_state_resource_version: preconditions.state_resource_version.clone(),
                new_state_resource_version: observed.metadata.resource_version,
                state_uid: observed.metadata.uid,
            },
        )))
    }

    #[cfg(test)]
    async fn recover_root_status(
        &self,
        address: &RootAddressV0,
        token: &str,
        root_uid: &str,
        old_rv: &str,
        status: &WebApplicationStatus,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let session = self.begin_operation_session();
        self.recover_root_status_in_operation(
            address, token, root_uid, old_rv, status, &session, false,
        )
        .await
    }

    async fn recover_root_status_in_operation(
        &self,
        address: &RootAddressV0,
        token: &str,
        root_uid: &str,
        old_rv: &str,
        status: &WebApplicationStatus,
        session: &OperationSessionV0,
        prove_ambiguous_write: bool,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (root_ref, _) = address.references();
        let observed = self
            .recovery_reread(&root_ref, session)
            .await
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?
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
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if observed.value.get("status") != Some(&intended)
            || status.last_status_persist_token.as_deref() != Some(token)
            || !durable_ownership
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if prove_ambiguous_write {
            session.prove_exact_response_loss_recovery()?;
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

    /// Re-opens the root preflight immediately before one bootstrap subwrite.
    /// This is intentionally not shared with the initial discovery read: a
    /// State POST cannot inherit root authority from a read that happened
    /// before its state existence probe, and later patches cannot inherit it
    /// from an earlier bootstrap phase.
    async fn bootstrap_root_preflight(
        &self,
        root_ref: &ObjectRefV0,
        root_uid: &str,
        root_generation: i64,
        root_rv: &str,
        token: &str,
        phase: BootstrapRootPhaseV0,
        session: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        session.wait_for_bootstrap_phase_authority().await?;
        let root = self
            .reread_prewrite_in_operation(root_ref, session)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        if root.metadata.uid != root_uid
            || generation(&root)? != root_generation
            || root
                .value
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty()))
            || (root.metadata.resource_version != root_rv
                && !has_bootstrap_root_metadata(&root, token))
        {
            let _ = session.prewrite_gate().invalidate_authenticated_change();
            return Err(TransportErrorV0::PreWriteInvalidated);
        }
        if let Err(error) = session.begin_bootstrap_root_phase(phase, &root) {
            if matches!(error, TransportErrorV0::Stale(_)) {
                let _ = session.prewrite_gate().invalidate_authenticated_change();
                return Err(TransportErrorV0::PreWriteInvalidated);
            }
            return Err(error);
        }
        // RootStateLink is justified by the exact response to our metadata
        // patch, not by the broad "bootstrap annotations exist" predicate.
        // The latter is sufficient only to recognize durable bootstrap state;
        // it cannot authorize a status write after a status/finalizer/link or
        // annotation change raced the next phase.
        if phase == BootstrapRootPhaseV0::RootStateLink
            && let Err(error) = session.require_exact_root_metadata_response(&root)
        {
            if matches!(error, TransportErrorV0::Stale(_)) {
                let _ = session.prewrite_gate().invalidate_authenticated_change();
                return Err(TransportErrorV0::PreWriteInvalidated);
            }
            return Err(error);
        }
        Ok(root)
    }

    async fn execute_bootstrap(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let CommandV0::EnsureBootstrap {
            token,
            state: desired_spec,
            root_uid,
            root_generation,
            root_resource_version: root_rv,
            state_name,
        } = command
        else {
            return Err(TransportErrorV0::ManualRecoveryHold);
        };
        if address.state_name != *state_name {
            return Err(TransportErrorV0::IdentityMismatch);
        }
        let (root_ref, state_ref) = address.references();
        let (initial_root, observed_state) = if session.has_collected_bootstrap_fence() {
            // The collector's exact-name list/watch/reread cut is the sole
            // raw baseline for a collected bootstrap.  Do not let the first
            // effect GET rebind that baseline after the actor has selected
            // its command.
            session.sealed_collected_bootstrap_baseline(address, command)?
        } else {
            (
                self.reread_prewrite_in_operation(&root_ref, session)
                    .await?
                    .ok_or(TransportErrorV0::Stale("root absent"))?,
                self.reread_prewrite_in_operation(&state_ref, session)
                    .await?,
            )
        };
        if initial_root.metadata.uid != *root_uid
            || generation(&initial_root)? != *root_generation
            || initial_root
                .value
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty()))
            || (initial_root.metadata.resource_version != *root_rv
                && !has_bootstrap_root_metadata(&initial_root, token))
        {
            return Err(TransportErrorV0::Stale("bootstrap root pins"));
        }
        // A root that is not yet linked must either be genuinely fresh or be
        // handled by the bounded durable recovery path. In particular, never
        // overwrite a partial, malformed, inconsistent, or stale complete
        // v1 bundle with a new POST/metadata patch: that would erase the only
        // evidence needed to distinguish an old issued State from a
        // same-name replacement after a crash.
        if !observed_state
            .as_ref()
            .is_some_and(|state| has_state_link(&initial_root, state))
        {
            match bootstrap_authority_bundle_v1(&initial_root) {
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => return Err(TransportErrorV0::ManualRecoveryHold),
            }
        }
        // This is the one point at which the list/watch/reread fence can arm
        // a write.  A cross-bound input, a queued differing root, a delete, or
        // a terminal stream fails here before any POST/PATCH request.
        session.arm_collected_bootstrap_baseline(
            address,
            command,
            &initial_root,
            observed_state.as_ref(),
        )?;

        let mut state = match observed_state {
            None => {
                let root = self
                    .bootstrap_root_preflight(
                        &root_ref,
                        root_uid,
                        *root_generation,
                        root_rv,
                        token,
                        BootstrapRootPhaseV0::StatePost,
                        session,
                    )
                    .await?;
                let desired = bootstrap_state(&state_ref, desired_spec, &root, token)?;
                match self
                    .bootstrap_create_state_in_operation(&desired, &root, token, session)
                    .await
                {
                    Ok(created) => created,
                    Err(TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
                        operation: WriteOperationV0::Create,
                        resource: ResourceKindV0::OxideLoopState,
                    })) => return Err(TransportErrorV0::ForeignCollision),
                    Err(error) if bootstrap_write_response_lost(&error) => {
                        return Err(TransportErrorV0::ManualRecoveryHold);
                    }
                    Err(error) => return Err(error),
                }
            }
            Some(observed) => {
                // There is no create-response witness in this session for an
                // unlinked same-name object. It is a collision, never a
                // recoverable adoption candidate.
                if !has_state_link(&initial_root, &observed)
                    || !bootstrap_root_metadata_matches_state(&initial_root, &observed, token)
                {
                    return Err(TransportErrorV0::ForeignCollision);
                }
                let desired = bootstrap_state(&state_ref, desired_spec, &initial_root, token)?;
                recover_bootstrap_state(Some(observed), &desired, &initial_root, token)?
            }
        };

        // R17 deliberately persists the full root authority bundle before the
        // State finalizer.  An interrupted sequence may therefore leave an
        // unlinked, exact old-UID child, but it cannot adopt/link/ack one.
        let root_before_metadata = self
            .reread_prewrite_in_operation(&root_ref, session)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        let root = if bootstrap_root_metadata_matches_state(&root_before_metadata, &state, token) {
            root_before_metadata
        } else {
            let before = self
                .bootstrap_root_preflight(
                    &root_ref,
                    root_uid,
                    *root_generation,
                    root_rv,
                    token,
                    BootstrapRootPhaseV0::RootMetadata,
                    session,
                )
                .await?;
            let metadata_result = match self
                .bootstrap_root_metadata(&before, &state, token, session)
                .await
            {
                Ok(written) => Ok(written),
                Err(error) if bootstrap_write_response_lost(&error) => {
                    self.recover_collected_bootstrap_root_metadata_response_loss(
                        &before, &state, token, session,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            match metadata_result {
                Ok(written) => written,
                Err(error) => {
                    emit_bootstrap_diagnostic(
                        "root_metadata_patch_or_proof",
                        &error,
                        Some(&before),
                        Some(&state),
                        session,
                    );
                    return Err(error);
                }
            }
        };

        if !has_finalizer(&state, STATE_FINALIZER) {
            if let Err(error) = self
                .bootstrap_root_preflight(
                    &root_ref,
                    root_uid,
                    *root_generation,
                    root_rv,
                    token,
                    BootstrapRootPhaseV0::StateFinalizer,
                    session,
                )
                .await
            {
                emit_bootstrap_diagnostic(
                    "state_finalizer_preflight",
                    &error,
                    Some(&root),
                    Some(&state),
                    session,
                );
                return Err(error);
            }
            let before = state.clone();
            let finalizer_result = self
                .bootstrap_ensure_state_finalizer(&before, session)
                .await
                .map_err(|error| {
                    if bootstrap_write_response_lost(&error) {
                        TransportErrorV0::ManualRecoveryHold
                    } else {
                        error
                    }
                });
            state = match finalizer_result {
                Ok(written) => written,
                Err(error) => {
                    emit_bootstrap_diagnostic(
                        "state_finalizer_patch_or_proof",
                        &error,
                        Some(&root),
                        Some(&before),
                        session,
                    );
                    return Err(error);
                }
            };
        }

        if has_state_link(&root, &state) {
            // A durable link from another/earlier process does not carry this
            // session's raw final response echo. Refuse to acknowledge it.
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let before = self
            .bootstrap_root_preflight(
                &root_ref,
                root_uid,
                *root_generation,
                root_rv,
                token,
                BootstrapRootPhaseV0::RootStateLink,
                session,
            )
            .await?;
        let linked = self
            .bootstrap_root_status(&before, &state, session)
            .await
            .map_err(|error| {
                if bootstrap_write_response_lost(&error) {
                    TransportErrorV0::ManualRecoveryHold
                } else {
                    error
                }
            })?;
        if !has_state_link(&linked, &state) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }

        let (final_root, final_state) = if session.has_collected_bootstrap_fence() {
            session
                .complete_collected_bootstrap_final_proof(self, &root_ref, &state_ref, token)
                .await?
        } else {
            let final_state = self
                .reread_in_operation(&state_ref, session)
                .await?
                .ok_or(TransportErrorV0::ManualRecoveryHold)?;
            let final_root = self
                .reread_in_operation(&root_ref, session)
                .await?
                .ok_or(TransportErrorV0::ManualRecoveryHold)?;
            (final_root, final_state)
        };
        if final_state.metadata.uid != state.metadata.uid
            || !has_finalizer(&final_state, STATE_FINALIZER)
            || !bootstrap_root_metadata_matches_state(&final_root, &final_state, token)
            || !has_state_link(&final_root, &final_state)
            || (session.has_collected_bootstrap_fence()
                && !session.collected_final_state_is_exact(&final_root, &final_state, token))
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::BootstrapAcknowledged {
                token: token.clone(),
                state_name: state_name.clone(),
                state_uid: final_state.metadata.uid,
                state_resource_version: final_state.metadata.resource_version,
                old_root_resource_version: root_rv.clone(),
                new_root_resource_version: final_root.metadata.resource_version,
                status_state_link: StateLink {
                    root_uid: root_uid.clone(),
                    name: state_name.clone(),
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
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let reference = reference_from_identity(&command.identity)?;
        if reference.namespace != address.namespace {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        match &command.decision {
            MutationDecision::Post { body, .. } => {
                // This validation deliberately precedes the fence rereads:
                // unsupported Service modes must make zero Kubernetes calls.
                validate_desired_object(body, &reference)?;
            }
            MutationDecision::ConditionalApply { body, expected, .. }
                if !conditional_apply_envelope_authorized(command)
                    || validate_conditional_apply_body(body, &reference, expected).is_err() =>
            {
                return Ok(mutation_failed_outcome(
                    transaction_id,
                    attempt_token,
                    "conditional apply envelope is not authorized",
                ));
            }
            _ => {}
        }
        let target = match &command.decision {
            MutationDecision::Post { .. } => None,
            _ => Some(reference.clone()),
        };
        let fence = match self
            .reread_fence_with_target_in_operation(
                address,
                &command.preconditions,
                target.as_ref(),
                session,
            )
            .await
        {
            Ok(fence) => fence,
            // The target/root/state fence is entirely before
            // `authorize_dispatch` and before the transport claims its one
            // child write. A stale read is therefore definitive no-write
            // evidence for this exact command, not a response-loss boundary.
            Err(TransportErrorV0::Stale(reason)) => {
                return Ok(mutation_not_issued_outcome(
                    transaction_id,
                    attempt_token,
                    &command.command_id,
                    reason,
                ));
            }
            Err(error) => return Err(error),
        };
        // Reject stale target identity claims before the final State gate. No
        // authorization read is needed when the command is already known to
        // be non-mutating, and this preserves the zero-write stale-target
        // contract.
        match &command.decision {
            MutationDecision::PreconditionedDelete { preconditions, .. }
                if fence.target.as_ref().is_some_and(|target| {
                    target.metadata.uid != preconditions.uid
                        || target.metadata.resource_version != preconditions.resource_version
                }) =>
            {
                return Ok(mutation_not_issued_outcome(
                    transaction_id,
                    attempt_token,
                    &command.command_id,
                    "mutation delete pins",
                ));
            }
            MutationDecision::ConditionalApply { expected, .. }
                if fence.target.as_ref().is_some_and(|target| {
                    target.metadata.uid != expected.uid
                        || target.metadata.resource_version != expected.resource_version
                }) =>
            {
                return Ok(mutation_not_issued_outcome(
                    transaction_id,
                    attempt_token,
                    &command.command_id,
                    "conditional apply target identity is stale",
                ));
            }
            MutationDecision::ConditionalApply { .. }
            | MutationDecision::PreconditionedDelete { .. }
                if fence.target.as_ref().is_some_and(|target| {
                    !has_root_uid_annotation(target, &fence.root.metadata.uid)
                        || !has_exact_canonical_owner(
                            target,
                            &fence.root.reference,
                            &fence.root.metadata.uid,
                        )
                }) =>
            {
                // This is still before durable dispatch authorization and the
                // sole child write.  A near-miss is definitive no-write
                // evidence, so it must invalidate rather than entering the
                // response-loss path.
                return Ok(mutation_not_issued_outcome(
                    transaction_id,
                    attempt_token,
                    &command.command_id,
                    "managed target owner/root proof mismatch",
                ));
            }
            _ => {}
        }
        if matches!(
            &command.decision,
            MutationDecision::Post {
                method: HttpMethod::Post,
                ..
            } | MutationDecision::ConditionalApply {
                method: HttpMethod::Patch,
                force: true,
                ..
            } | MutationDecision::PreconditionedDelete {
                method: HttpMethod::Delete,
                ..
            }
        ) {
            // The target reread above is intentionally followed by this final
            // State reread/authentication gate. No network operation may be
            // inserted between this check and the managed child write.
            self.authorize_dispatch(
                address,
                &fence.root,
                &command.preconditions,
                transaction_id,
                attempt_token,
                command,
                session,
            )
            .await?;
        }
        let outcome = match &command.decision {
            MutationDecision::Post {
                method: HttpMethod::Post,
                body,
            } => self
                .mutation_create(
                    &reference,
                    body,
                    &fence.root,
                    transaction_id,
                    command,
                    session,
                )
                .await
                .map(|o| result(command, ApplyOutcome::Applied, Some(o))),
            MutationDecision::ConditionalApply {
                method: HttpMethod::Patch,
                content_type,
                field_manager,
                force: true,
                body,
                expected,
            } if conditional_apply_force_authorized(
                command,
                content_type,
                field_manager,
                expected,
                &fence.root,
                fence.target.as_ref().expect("fenced"),
            ) =>
            {
                Ok(
                    match self
                        .mutation_apply(
                            &reference,
                            body,
                            &fence.root,
                            transaction_id,
                            command,
                            &fence.target.expect("fenced"),
                            session,
                        )
                        .await
                    {
                        Ok(object) => result(command, ApplyOutcome::Applied, Some(object)),
                        Err(TransportErrorV0::Stale("child apply HTTP 409")) => {
                            // A single child PATCH returned a definitive
                            // rejection. It cannot have changed the target,
                            // so invalidate this authorized dispatch rather
                            // than treating it as response-loss recovery.
                            return Ok(mutation_not_issued_outcome(
                                transaction_id,
                                attempt_token,
                                &command.command_id,
                                "child apply HTTP 409",
                            ));
                        }
                        Err(TransportErrorV0::ManualRecoveryHold) => {
                            return Ok(mutation_failed_outcome(
                                transaction_id,
                                attempt_token,
                                "conditional apply conflict could not prove prior success",
                            ));
                        }
                        Err(error) => return Err(error),
                    },
                )
            }
            MutationDecision::PreconditionedDelete {
                method: HttpMethod::Delete,
                preconditions,
            } => self
                .mutation_delete(
                    &reference,
                    preconditions,
                    &fence.target.expect("fenced"),
                    &fence.root,
                    session,
                )
                .await
                .map(|identity| {
                    result_identity(
                        command,
                        ApplyOutcome::Deleted,
                        Some(identity.uid),
                        Some(identity.resource_version),
                    )
                }),
            MutationDecision::ConditionalApply { .. } => {
                return Ok(mutation_failed_outcome(
                    transaction_id,
                    attempt_token,
                    "conditional apply force authority proof failed",
                ));
            }
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

    async fn reread_fence_in_operation(
        &self,
        address: &RootAddressV0,
        preconditions: &PreconditionsV0,
        session: &OperationSessionV0,
    ) -> Result<EffectFenceV0, TransportErrorV0> {
        let (root_ref, state_ref) = address.references();
        let root = self
            .reread_prewrite_in_operation(&root_ref, session)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        let state = self
            .reread_prewrite_in_operation(&state_ref, session)
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

    #[cfg(test)]
    #[allow(dead_code)]
    async fn reread_fence_with_target(
        &self,
        address: &RootAddressV0,
        preconditions: &PreconditionsV0,
        target: Option<&ObjectRefV0>,
    ) -> Result<EffectFenceV0, TransportErrorV0> {
        let session = self.begin_operation_session();
        self.reread_fence_with_target_in_operation(address, preconditions, target, &session)
            .await
    }

    async fn reread_fence_with_target_in_operation(
        &self,
        address: &RootAddressV0,
        preconditions: &PreconditionsV0,
        target: Option<&ObjectRefV0>,
        session: &OperationSessionV0,
    ) -> Result<EffectFenceV0, TransportErrorV0> {
        let (root_ref, state_ref) = address.references();
        let root = self
            .reread_prewrite_in_operation(&root_ref, session)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        let state = self
            .reread_prewrite_in_operation(&state_ref, session)
            .await?
            .ok_or(TransportErrorV0::Stale("state absent"))?;
        let target = match target {
            Some(reference) => Some(
                self.reread_prewrite_in_operation(reference, session)
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

    /// Performs the final State read and durable dispatch authorization check.
    /// This method must remain the last network operation before a managed
    /// child write in `execute_mutation`.
    async fn authorize_dispatch(
        &self,
        address: &RootAddressV0,
        root: &ObjectV0,
        preconditions: &PreconditionsV0,
        transaction_id: &str,
        attempt_token: &str,
        command: &MutationCommandV0,
        session: &OperationSessionV0,
    ) -> Result<(), TransportErrorV0> {
        let (_, state_ref) = address.references();
        let state = self
            .reread_prewrite_in_operation(&state_ref, session)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        validate_dispatch_authority(
            address,
            root,
            &state,
            preconditions,
            transaction_id,
            attempt_token,
            command,
        )
    }

    async fn bootstrap_ensure_state_finalizer(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        if has_finalizer(object, STATE_FINALIZER) {
            return Ok(object.clone());
        }
        let patch_operation = if object.value.pointer("/metadata/finalizers").is_some() {
            serde_json::json!({"op":"add","path":"/metadata/finalizers/-","value":STATE_FINALIZER})
        } else {
            serde_json::json!({"op":"add","path":"/metadata/finalizers","value":[STATE_FINALIZER]})
        };
        let patch = serde_json::json!([
            {"op":"test","path":"/metadata/uid","value":object.metadata.uid},
            {"op":"test","path":"/metadata/resourceVersion","value":object.metadata.resource_version},
            patch_operation
        ]);
        let patch = serde_json::from_value::<json_patch::Patch>(patch)
            .map_err(|error| TransportErrorV0::Other(error.to_string()))?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(ResourceKindV0::OxideLoopState, &object.reference.namespace)
                .patch(
                    &object.reference.name,
                    &bootstrap_controller_patch_params(),
                    &Patch::<Value>::Json(patch),
                )
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::JsonPatch,
                        ResourceKindV0::OxideLoopState,
                    )
                })
        })
        .await?;
        let written = to_domain(
            ResourceKindV0::OxideLoopState,
            &object.reference.namespace,
            written,
        )?;
        if !bootstrap_state_finalizer_proof(&written, object) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if operation.has_collected_bootstrap_fence() {
            operation.complete_bootstrap_subwrite(
                permit,
                BootstrapRootPhaseV0::StateFinalizer,
                &written,
            )?;
        } else {
            permit.prove()?;
        }
        Ok(written)
    }

    #[cfg(test)]
    async fn remove_finalizer(
        &self,
        object: &ObjectV0,
        finalizer: &str,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.remove_finalizer_in_operation(object, finalizer, &operation)
            .await
    }

    async fn remove_finalizer_in_operation(
        &self,
        object: &ObjectV0,
        finalizer: &str,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let Some(index) = finalizer_index(object, finalizer) else {
            return Err(TransportErrorV0::ManualRecoveryHold);
        };
        let patch = serde_json::json!([{"op":"test","path":"/metadata/uid","value":object.metadata.uid},{"op":"test","path":"/metadata/resourceVersion","value":object.metadata.resource_version},{"op":"test","path":format!("/metadata/finalizers/{index}"),"value":finalizer},{"op":"remove","path":format!("/metadata/finalizers/{index}")}]);
        let patch = serde_json::from_value::<json_patch::Patch>(patch)
            .map_err(|_| ambiguous_delete_or_finalizer())?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(object.reference.resource, &object.reference.namespace)
                .patch(
                    &object.reference.name,
                    &PatchParams::default(),
                    &Patch::<Value>::Json(patch),
                )
                .await
                .map_err(|_| ambiguous_delete_or_finalizer())
        })
        .await?;
        let written = to_domain(
            object.reference.resource,
            &object.reference.namespace,
            written,
        )
        .map_err(|_| ambiguous_delete_or_finalizer())?;
        permit.prove()?;
        Ok(written)
    }

    async fn bootstrap_root_metadata(
        &self,
        root: &ObjectV0,
        state: &ObjectV0,
        token: &str,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let expected = bootstrap_root_metadata_expected_value(root, state, token)
            .ok_or(TransportErrorV0::Stale("bootstrap root metadata"))?;
        let annotations = expected
            .pointer("/metadata/annotations")
            .cloned()
            .ok_or(TransportErrorV0::Stale("bootstrap root annotations"))?;
        let finalizers = expected
            .pointer("/metadata/finalizers")
            .cloned()
            .ok_or(TransportErrorV0::Stale("bootstrap root finalizers"))?;
        let patch = serde_json::json!([
          {"op":"test","path":"/metadata/uid","value":root.metadata.uid},
          {"op":"test","path":"/metadata/resourceVersion","value":root.metadata.resource_version},
          {"op":"add","path":"/metadata/annotations","value":annotations},
          {"op":"add","path":"/metadata/finalizers","value":finalizers}
        ]);
        let patch = serde_json::from_value::<json_patch::Patch>(patch)
            .map_err(|error| TransportErrorV0::Other(error.to_string()))?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(ResourceKindV0::WebApplication, &root.reference.namespace)
                .patch(
                    &root.reference.name,
                    &bootstrap_controller_patch_params(),
                    &Patch::<Value>::Json(patch),
                )
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::JsonPatch,
                        ResourceKindV0::WebApplication,
                    )
                })
        })
        .await?;
        let written = to_domain(
            ResourceKindV0::WebApplication,
            &root.reference.namespace,
            written,
        )?;
        if !bootstrap_root_metadata_proof(&written, root, state, token) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if operation.has_collected_bootstrap_fence() {
            operation.complete_bootstrap_subwrite(
                permit,
                BootstrapRootPhaseV0::RootMetadata,
                &written,
            )?;
        } else {
            permit.prove()?;
            operation
                .record_bootstrap_owned_root_echo(&written, BootstrapRootPhaseV0::RootStateLink)?;
        }
        Ok(written)
    }

    async fn recover_collected_bootstrap_root_metadata_response_loss(
        &self,
        before: &ObjectV0,
        state: &ObjectV0,
        token: &str,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        if !operation.has_collected_bootstrap_fence() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let recovered = self
            .recovery_reread(&before.reference, operation)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        if !bootstrap_root_metadata_proof(&recovered, before, state, token) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        operation.complete_recovered_bootstrap_root_metadata(&recovered)?;
        Ok(recovered)
    }

    async fn bootstrap_root_status(
        &self,
        root: &ObjectV0,
        state: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let body = serde_json::json!({"metadata":{"resourceVersion":root.metadata.resource_version},"status":{"state":StateLink { root_uid:root.metadata.uid.clone(), name:state.reference.name.clone(), uid:state.metadata.uid.clone() }}});
        let permit = claim_first_write(operation)?;
        if !operation.has_collected_bootstrap_fence() {
            operation.mark_bootstrap_final_write_polled()?;
        }
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            let patch_params = bootstrap_controller_patch_params();
            self.api(ResourceKindV0::WebApplication, &root.reference.namespace)
                .patch_status(&root.reference.name, &patch_params, &Patch::Merge(&body))
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::MergeStatus,
                        ResourceKindV0::WebApplication,
                    )
                })
        })
        .await?;
        let written = to_domain(
            ResourceKindV0::WebApplication,
            &root.reference.namespace,
            written,
        )?;
        if !bootstrap_root_link_proof(&written, root, state) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if operation.has_collected_bootstrap_fence() {
            operation.complete_bootstrap_subwrite(
                permit,
                BootstrapRootPhaseV0::RootStateLink,
                &written,
            )?;
        } else {
            permit.prove_bootstrap_final()?;
            operation.record_bootstrap_owned_root_echo(&written, BootstrapRootPhaseV0::Complete)?;
        }
        Ok(written)
    }
    async fn mutation_create(
        &self,
        reference: &ObjectRefV0,
        body: &Value,
        root: &ObjectV0,
        tx: &str,
        command: &MutationCommandV0,
        session: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let desired = owned_mutation_object(reference, body, root, tx, command)?;
        let created = match reference.resource {
            ResourceKindV0::ConfigMap => {
                self.create_config_map_in_operation(&desired, session).await
            }
            ResourceKindV0::Deployment => {
                self.create_deployment_in_operation(&desired, session).await
            }
            ResourceKindV0::Service => self.create_service_in_operation(&desired, session).await,
            _ => return Err(TransportErrorV0::UnsupportedTarget),
        };
        // A POST response is the sole initial witness for an unpinned create.
        // A same-name reread after response loss cannot distinguish our write
        // from a foreign object (or a replay), so fail closed without a read.
        created.map_err(|error| {
            if may_recover_response_loss(&error)
                || matches!(
                    error,
                    TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)
                )
            {
                TransportErrorV0::ResponseLost
            } else {
                error
            }
        })
    }
    async fn mutation_apply(
        &self,
        reference: &ObjectRefV0,
        body: &Value,
        root: &ObjectV0,
        tx: &str,
        command: &MutationCommandV0,
        target: &ObjectV0,
        session: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let mut desired = owned_mutation_object(reference, body, root, tx, command)?;
        desired.metadata = target.metadata.clone();
        let applied = match reference.resource {
            ResourceKindV0::ConfigMap => {
                self.apply(
                    &desired,
                    ResourceKindV0::ConfigMap,
                    &target.metadata.resource_version,
                    true,
                    session,
                )
                .await
            }
            ResourceKindV0::Deployment => {
                self.apply(
                    &desired,
                    ResourceKindV0::Deployment,
                    &target.metadata.resource_version,
                    true,
                    session,
                )
                .await
            }
            ResourceKindV0::Service => {
                self.apply(
                    &desired,
                    ResourceKindV0::Service,
                    &target.metadata.resource_version,
                    true,
                    session,
                )
                .await
            }
            _ => return Err(TransportErrorV0::UnsupportedTarget),
        };
        match applied {
            Ok(o) => Ok(o),
            Err(error) if is_child_apply_write_conflict(&error, reference.resource) => {
                Err(TransportErrorV0::Stale("child apply HTTP 409"))
            }
            Err(error)
                if may_recover_response_loss(&error)
                    || matches!(
                        error,
                        TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)
                    ) =>
            {
                let recovered = recover_dropped_write_exact(
                    self.recovery_reread(reference, session)
                        .await
                        .map_err(|_| TransportErrorV0::ManualRecoveryHold)?
                        .as_ref(),
                    &desired,
                    root,
                    Some(&target.metadata.uid),
                    tx,
                    &command.command_id,
                )
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                session.prove_exact_response_loss_recovery()?;
                Ok(recovered)
            }
            Err(e) => Err(e),
        }
    }
    async fn mutation_delete(
        &self,
        reference: &ObjectRefV0,
        expected: &oxideloop_mutation_model::ObjectIdentity,
        target: &ObjectV0,
        root: &ObjectV0,
        session: &OperationSessionV0,
    ) -> Result<oxideloop_mutation_model::ObjectIdentity, TransportErrorV0> {
        if target.metadata.uid != expected.uid
            || target.metadata.resource_version != expected.resource_version
            || !has_exact_canonical_owner(target, &root.reference, &root.metadata.uid)
            || target.metadata.annotations.get(ROOT_UID_ANNOTATION) != Some(&root.metadata.uid)
        {
            return Err(TransportErrorV0::Stale("mutation delete pins"));
        }
        let result = match reference.resource {
            ResourceKindV0::ConfigMap => {
                self.delete(
                    reference,
                    ResourceKindV0::ConfigMap,
                    &expected.uid,
                    &expected.resource_version,
                    session,
                )
                .await
            }
            ResourceKindV0::Deployment => {
                self.delete(
                    reference,
                    ResourceKindV0::Deployment,
                    &expected.uid,
                    &expected.resource_version,
                    session,
                )
                .await
            }
            ResourceKindV0::Service => {
                self.delete(
                    reference,
                    ResourceKindV0::Service,
                    &expected.uid,
                    &expected.resource_version,
                    session,
                )
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
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let fence = self
            .reread_fence_in_operation(address, preconditions, session)
            .await?;
        if spec.root_uid != fence.root.metadata.uid {
            return Err(TransportErrorV0::Stale("automation root UID"));
        }
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: address.namespace.clone(),
            name: name.into(),
        };
        let desired = automation_object(&reference, spec, &fence.root, token)?;
        let run = match self
            .reread_prewrite_in_operation(&reference, session)
            .await?
        {
            // EnsureAutomationRun is intentionally unpinned. A same-name
            // object therefore has no durable UID witness and is always a
            // collision, even when its semantic body looks identical.
            Some(_) => return Err(TransportErrorV0::ForeignCollision),
            None => match self
                .create_automation_run_in_operation(&desired, session)
                .await
            {
                Ok(o) => o,
                Err(error)
                    if may_recover_response_loss(&error)
                        || matches!(
                            error,
                            TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)
                        ) =>
                {
                    return Err(TransportErrorV0::ResponseLost);
                }
                Err(TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
                    operation: WriteOperationV0::Create,
                    resource: ResourceKindV0::AutomationRun,
                })) => {
                    return Err(TransportErrorV0::ForeignCollision);
                }
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
        let operation = self.begin_operation_session();
        self.list_exact_in_operation(resource, namespace, &operation)
            .await
    }

    #[doc(hidden)]
    pub async fn list_exact_in_operation(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        operation: &OperationSessionV0,
    ) -> Result<ListV0, TransportErrorV0> {
        validate_namespace(namespace)?;
        if resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        let listed = within_operation(operation, FiniteIoStageV0::ReadOnly, async {
            self.api(resource, namespace)
                .list(&ListParams::default())
                .await
                .map_err(|error| map_read_error(error, ReadOperationV0::List, resource))
        })
        .await?;
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

    /// Lists only one exact object name while retaining the collection RV used
    /// to open its matching watch.  Bootstrap/root barriers use this rather
    /// than a broad namespace collection so unrelated objects cannot consume
    /// a retained-watch handoff slot.
    #[doc(hidden)]
    pub async fn list_named_in_operation(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        name: &str,
        operation: &OperationSessionV0,
    ) -> Result<ListV0, TransportErrorV0> {
        validate_namespace(namespace)?;
        if resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        validate_reference(&ObjectRefV0 {
            resource,
            namespace: namespace.into(),
            name: name.into(),
        })?;
        let selector = format!("metadata.name={name}");
        let listed = within_operation(operation, FiniteIoStageV0::ReadOnly, async {
            self.api(resource, namespace)
                .list(&ListParams::default().fields(&selector))
                .await
                .map_err(|error| map_read_error(error, ReadOperationV0::List, resource))
        })
        .await?;
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
        if resource_version.is_empty() || items.iter().any(|item| item.reference.name != name) {
            // An exact-name capability is all-or-nothing.  A collection
            // response without an RV cannot seed its matching watch, and a
            // selector leak could otherwise let a foreign record consume a
            // bootstrap or retained-watch slot.  Neither condition is a
            // normal root supersession.
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok(ListV0 {
            items,
            resource_version,
        })
    }

    /// A child inventory read immediately preceding a destructive root-only
    /// action is part of that action's prewrite fence, not a free-standing
    /// observation.  It shares the exact operation epoch and cannot outlive
    /// the write lease it is intended to justify.
    async fn list_exact_prewrite_in_operation(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        operation: &OperationSessionV0,
    ) -> Result<ListV0, TransportErrorV0> {
        validate_namespace(namespace)?;
        if resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        let listed = within_operation(operation, FiniteIoStageV0::PreWrite, async {
            self.api(resource, namespace)
                .list(&ListParams::default())
                .await
                .map_err(|error| map_read_error(error, ReadOperationV0::List, resource))
        })
        .await;
        let listed = match listed {
            Err(TransportErrorV0::Other(message)) => {
                return Err(TransportErrorV0::PreWriteRetryable(message));
            }
            other => other,
        }?;
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
        let operation = self.begin_operation_session();
        self.observe_deletion_revalidation_in_operation(address, preconditions, &operation)
            .await
    }

    /// The deletion fence is one logical authority acquisition.  Its fence,
    /// every list, and every exact child reread therefore share a single
    /// immutable budget rather than each reopening a fresh read window.
    #[doc(hidden)]
    pub async fn observe_deletion_revalidation_in_operation(
        &self,
        address: &RootAddressV0,
        preconditions: &PreconditionsV0,
        operation: &OperationSessionV0,
    ) -> Result<DeletionObservationSnapshotV0, TransportErrorV0> {
        validate_address(address)?;
        let gate = DeletionBarrierGateV0::new();
        let tracked_names = Arc::new(Mutex::new(
            BTreeMap::<ResourceKindV0, BTreeSet<String>>::new(),
        ));
        let filter = DeletionWatchFilterV0 {
            gate: gate.clone(),
            root_uid: preconditions.root_uid.clone(),
            root_name: address.name.clone(),
            state_name: address.state_name.clone(),
            tracked_names: Arc::clone(&tracked_names),
        };
        let resources = [
            ResourceKindV0::WebApplication,
            ResourceKindV0::OxideLoopState,
            ResourceKindV0::ConfigMap,
            ResourceKindV0::Deployment,
            ResourceKindV0::Service,
            ResourceKindV0::AutomationRun,
        ];
        let mut watches = Vec::new();
        #[cfg(any(test, feature = "test-support"))]
        let scripted_live_watches = self
            .test_watches
            .lock()
            .is_ok_and(|watches| !watches.is_empty());
        #[cfg(not(any(test, feature = "test-support")))]
        let scripted_live_watches = true;
        if scripted_live_watches {
            // LIST -> exact-RV watch-open for every authority source. The
            // pump owns the Live -> Invalidated CAS before it can publish a
            // relevant record; later closing reads establish the snapshot.
            let mut initial = BTreeMap::new();
            for resource in resources {
                let list = self
                    .list_exact_in_operation(resource, &address.namespace, operation)
                    .await?;
                if matches!(
                    resource,
                    ResourceKindV0::ConfigMap
                        | ResourceKindV0::Deployment
                        | ResourceKindV0::Service
                        | ResourceKindV0::AutomationRun
                ) {
                    let mut names = tracked_names.lock().expect("deletion filter lock");
                    names.entry(resource).or_default().extend(
                        list.items
                            .iter()
                            .filter(|object| {
                                has_exact_canonical_owner(
                                    object,
                                    &ObjectRefV0 {
                                        resource: ResourceKindV0::WebApplication,
                                        namespace: address.namespace.clone(),
                                        name: address.name.clone(),
                                    },
                                    &preconditions.root_uid,
                                )
                            })
                            .map(|object| object.reference.name.clone()),
                    );
                }
                initial.insert(resource, list);
            }
            for resource in resources {
                let list = initial.get(&resource).expect("initial list exists");
                watches.push(
                    self.watch_from_in_operation_with_deletion_filter(
                        resource,
                        &address.namespace,
                        &list.resource_version,
                        operation,
                        None,
                        Some(filter.clone()),
                    )
                    .await?,
                );
            }
        }
        let fence = self
            .reread_fence_in_operation(address, preconditions, operation)
            .await?;
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
        for resource in [
            ResourceKindV0::ConfigMap,
            ResourceKindV0::Deployment,
            ResourceKindV0::Service,
        ] {
            let listed = self
                .list_exact_in_operation(resource, &address.namespace, operation)
                .await?;
            tracked_names
                .lock()
                .expect("deletion filter lock")
                .entry(resource)
                .or_default()
                .extend(
                    listed
                        .items
                        .iter()
                        .filter(|object| {
                            has_exact_canonical_owner(
                                object,
                                &fence.root.reference,
                                &fence.root.metadata.uid,
                            )
                        })
                        .map(|object| object.reference.name.clone()),
                );
            for object in listed.items {
                if !has_exact_canonical_owner(
                    &object,
                    &fence.root.reference,
                    &fence.root.metadata.uid,
                ) {
                    continue;
                }
                match self
                    .reread_in_operation(&object.reference, operation)
                    .await?
                {
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
            .list_exact_in_operation(ResourceKindV0::AutomationRun, &address.namespace, operation)
            .await?;
        tracked_names
            .lock()
            .expect("deletion filter lock")
            .entry(ResourceKindV0::AutomationRun)
            .or_default()
            .extend(
                listed
                    .items
                    .iter()
                    .filter(|object| {
                        has_exact_canonical_owner(
                            object,
                            &fence.root.reference,
                            &fence.root.metadata.uid,
                        )
                    })
                    .map(|object| object.reference.name.clone()),
            );
        for object in listed.items {
            if !has_exact_canonical_owner(&object, &fence.root.reference, &fence.root.metadata.uid)
            {
                continue;
            }
            match self
                .reread_in_operation(&object.reference, operation)
                .await?
            {
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
        // Drain only records already delivered by the stream pumps, then
        // linearize this explicitly point-in-time authority. A later record
        // is not claimed by this snapshot and is re-fenced by the next
        // durable deletion command.
        for watch in &mut watches {
            while watch.try_next_event()?.is_some() {}
        }
        if !gate.is_live() || !gate.seal() {
            return Err(TransportErrorV0::Stale(
                "deletion observation watch changed",
            ));
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
        let operation = self.begin_operation_session();
        self.reread_in_operation(reference, &operation).await
    }

    #[doc(hidden)]
    pub async fn reread_in_operation(
        &self,
        reference: &ObjectRefV0,
        operation: &OperationSessionV0,
    ) -> Result<Option<ObjectV0>, TransportErrorV0> {
        validate_reference(reference)?;
        if reference.resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        within_operation(operation, FiniteIoStageV0::ReadOnly, async {
            self.api(reference.resource, &reference.namespace)
                .get_opt(&reference.name)
                .await
                .map_err(|error| map_read_error(error, ReadOperationV0::Get, reference.resource))
        })
        .await?
        .map(|object| to_domain(reference.resource, &reference.namespace, object))
        .transpose()
    }

    /// Exact write preflight reads use the current frozen prewrite attempt
    /// cutoff.  They cannot consume the later recovery-proof window before a
    /// first write has been authorized.
    #[doc(hidden)]
    pub async fn reread_prewrite_in_operation(
        &self,
        reference: &ObjectRefV0,
        operation: &OperationSessionV0,
    ) -> Result<Option<ObjectV0>, TransportErrorV0> {
        validate_reference(reference)?;
        if reference.resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        let result = within_operation(operation, FiniteIoStageV0::PreWrite, async {
            self.api(reference.resource, &reference.namespace)
                .get_opt(&reference.name)
                .await
                .map_err(|error| map_read_error(error, ReadOperationV0::Get, reference.resource))
        })
        .await;
        let result = match result {
            Err(TransportErrorV0::Other(message)) => {
                return Err(TransportErrorV0::PreWriteRetryable(message));
            }
            other => other,
        }?;
        result
            .map(|object| to_domain(reference.resource, &reference.namespace, object))
            .transpose()
    }

    /// Performs the one post-write proof read without delegating to a public
    /// helper, so it consumes the original operation budget and cannot mint a
    /// fresh deadline or authority session.
    async fn recovery_reread(
        &self,
        reference: &ObjectRefV0,
        operation: &OperationSessionV0,
    ) -> Result<Option<ObjectV0>, TransportErrorV0> {
        validate_reference(reference)?;
        if reference.resource == ResourceKindV0::Event {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        within_operation(operation, FiniteIoStageV0::RecoveryProof, async {
            self.api(reference.resource, &reference.namespace)
                .get_opt(&reference.name)
                .await
                .map_err(|error| map_read_error(error, ReadOperationV0::Get, reference.resource))
        })
        .await?
        .map(|object| to_domain(reference.resource, &reference.namespace, object))
        .transpose()
    }

    /// Materializes one deterministic Event, then creates it. A planned name
    /// is unpinned in this interface, so a pre-existing object and an
    /// ambiguous response are both fail-closed collisions/holds; no semantic
    /// adoption is attempted.
    ///
    /// # Errors
    ///
    /// Returns `ForeignCollision` for any pre-existing Event and `ResponseLost`
    /// when the direct create response is ambiguous.
    pub async fn create_or_exact_recover_event(
        &self,
        plan: &PlannedEventV0,
        controller_supplied_event_time: &str,
    ) -> Result<EventMaterializationRefV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        for _ in 0..=operation.max_prewrite_retries() {
            match self
                .create_event_in_operation_session(plan, controller_supplied_event_time, &operation)
                .await
            {
                Err(
                    TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::PreWrite)
                    | TransportErrorV0::PreWriteRetryable(_),
                ) => {
                    operation.retry_prewrite_epoch()?;
                }
                result => return result,
            }
        }
        Err(TransportErrorV0::FiniteIoDeadline(
            FiniteIoStageV0::PreWrite,
        ))
    }

    /// Event materialization is deliberately one read plus one POST under one
    /// session. A post-response timeout is terminal: Events have no durable
    /// UID witness, so this path must never reread, adopt, or replay.
    async fn create_event_in_operation_session(
        &self,
        plan: &PlannedEventV0,
        controller_supplied_event_time: &str,
        operation: &OperationSessionV0,
    ) -> Result<EventMaterializationRefV0, TransportErrorV0> {
        let value = materialize_event(plan, controller_supplied_event_time)
            .map_err(|error| TransportErrorV0::Other(error.to_string()))?;
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::Event,
            namespace: plan.metadata.namespace.clone(),
            name: plan.metadata.name.clone(),
        };
        validate_reference(&reference)?;
        if self
            .reread_event_in_operation(&reference, operation)
            .await?
            .is_some()
        {
            return Err(TransportErrorV0::ForeignCollision);
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
        self.create_event_in_operation(&desired, operation)
            .await
            .map(|written| event_ref(&written))
            .map_err(|error| {
                if may_recover_response_loss(&error)
                    || matches!(
                        error,
                        TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)
                    )
                    || matches!(
                        error,
                        TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
                            operation: WriteOperationV0::Create,
                            resource: ResourceKindV0::Event,
                        })
                    )
                {
                    if matches!(
                        error,
                        TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
                            operation: WriteOperationV0::Create,
                            resource: ResourceKindV0::Event,
                        })
                    ) {
                        TransportErrorV0::ForeignCollision
                    } else {
                        TransportErrorV0::ResponseLost
                    }
                } else {
                    error
                }
            })
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
        session: &OperationSessionV0,
    ) -> Result<(ObjectV0, DeletionCleanupReceipt), TransportErrorV0> {
        if preconditions.state_resource_version.is_empty() {
            // This branch is valid only for the deletion sentinel. All values
            // below are re-read from Kubernetes; none are cache authority.
        } else {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let (root_ref, _) = address.references();
        let root = self
            .reread_prewrite_in_operation(&root_ref, session)
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
            || !raw_metadata_pins_match(&root)
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
            RootOnlyCleanupCommandV0::RemoveRootFinalizer => root_only_finalizer_command_token(
                &root.metadata.uid,
                root_generation,
                &root.metadata.resource_version,
                &receipt.state_name,
                &receipt.state_uid,
                &receipt.deletion_id,
                receipt.deletion_start_generation,
                &receipt.cleanup_token,
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
        if self
            .reread_prewrite_in_operation(&state_ref, session)
            .await?
            .is_some()
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok((root, receipt))
    }

    /// Fresh prewrite fence dedicated to the retired-State DELETE. This is
    /// deliberately not the generic linked fence: the State body is no
    /// longer authority, and both the root receipt and raw tombstone shape
    /// must be re-established immediately before the one irreversible DELETE.
    async fn retired_state_delete_fence(
        &self,
        address: &RootAddressV0,
        token: &str,
        preconditions: &PreconditionsV0,
        session: &OperationSessionV0,
    ) -> Result<(ObjectV0, ObjectV0, DeletionCleanupReceipt), TransportErrorV0> {
        if preconditions.state_uid.is_empty() || preconditions.state_resource_version.is_empty() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let (root_ref, state_ref) = address.references();
        let root = self
            .reread_prewrite_in_operation(&root_ref, session)
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
            || !raw_metadata_pins_match(&root)
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
        let Some(link) = status.state.as_ref() else {
            return Err(TransportErrorV0::ManualRecoveryHold);
        };
        if receipt.root_uid != root.metadata.uid
            || receipt.state_name != expected_state_name
            || receipt.state_name != address.state_name
            || receipt.state_uid != preconditions.state_uid
            || receipt.deletion_start_generation > generation(&root)?
            || link.root_uid != root.metadata.uid
            || link.name != receipt.state_name
            || link.uid != receipt.state_uid
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let state = self
            .reread_prewrite_in_operation(&state_ref, session)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let metadata = state
            .value
            .get("metadata")
            .and_then(Value::as_object)
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let raw_finalizers = match metadata.get("finalizers") {
            None => Some(Vec::new()),
            Some(Value::Array(values)) => {
                values.iter().map(Value::as_str).collect::<Option<Vec<_>>>()
            }
            Some(_) => None,
        };
        let root_uid_annotation = metadata
            .get("annotations")
            .and_then(Value::as_object)
            .and_then(|annotations| annotations.get(ROOT_UID_ANNOTATION))
            .and_then(Value::as_str);
        if state.reference != state_ref
            || state.metadata.uid != preconditions.state_uid
            || state.metadata.resource_version != preconditions.state_resource_version
            || !raw_metadata_pins_match(&state)
            || state.value.get("apiVersion").and_then(Value::as_str)
                != Some(ResourceKindV0::OxideLoopState.api_version())
            || state.value.get("kind").and_then(Value::as_str)
                != Some(ResourceKindV0::OxideLoopState.kind())
            || metadata.get("name").and_then(Value::as_str) != Some(address.state_name.as_str())
            || metadata.get("namespace").and_then(Value::as_str) != Some(address.namespace.as_str())
            || metadata.contains_key("deletionTimestamp")
            || raw_finalizers.is_none_or(|finalizers| !finalizers.is_empty())
            || root_uid_annotation != Some(root.metadata.uid.as_str())
            || state
                .metadata
                .annotations
                .get(ROOT_UID_ANNOTATION)
                .is_none_or(|value| value != root.metadata.uid.as_str())
            || !has_exact_canonical_owner(&state, &root_ref, &root.metadata.uid)
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let expected_token = retired_state_delete_command_token(
            &root.metadata.uid,
            generation(&root)?,
            &root.metadata.resource_version,
            &receipt.state_name,
            &state.metadata.uid,
            &state.metadata.resource_version,
            &receipt.deletion_id,
            receipt.deletion_start_generation,
            &receipt.cleanup_token,
        );
        if token != expected_token {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok((root, state, receipt))
    }

    /// Deletes exactly one finalizer-free retired State in this immutable
    /// operation session. A Kubernetes 2xx is not acknowledgement: both it
    /// and every ambiguous result must be followed by an in-session 404
    /// proof, and this method never replays the DELETE after the write poll.
    /// A later process can make a new attempt only after its fresh raw
    /// classifier proves the same UID/RV, finalizer-free tombstone and the
    /// unchanged root receipt/link. A landed DELETE yields absence or a
    /// deletionTimestamp/resourceVersion change, neither of which can
    /// reclassify as a fresh tombstone.
    async fn delete_retired_state(
        &self,
        address: &RootAddressV0,
        token: &str,
        preconditions: &PreconditionsV0,
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (_root, state, _receipt) = self
            .retired_state_delete_fence(address, token, preconditions, session)
            .await?;
        let state_ref = ObjectRefV0 {
            resource: ResourceKindV0::OxideLoopState,
            namespace: address.namespace.clone(),
            name: address.state_name.clone(),
        };
        let delete_result = self
            .delete(
                &state_ref,
                ResourceKindV0::OxideLoopState,
                &state.metadata.uid,
                &state.metadata.resource_version,
                session,
            )
            .await;
        // Do not distinguish a 2xx from a lost/truncated result as an
        // acknowledgement. In either case, exactly one bounded proof read is
        // the only accepted evidence of deletion. No second DELETE exists.
        let observed = self
            .recovery_reread(&state_ref, session)
            .await
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if observed.is_some() {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        if delete_result.is_err() {
            session.prove_exact_response_loss_recovery()?;
        }
        Ok(CommandOutcomeV0::Event(Box::new(
            oxideloop_controller_core::EventV0::RetiredStateDeleted {
                token: token.into(),
                state_uid: preconditions.state_uid.clone(),
                state_resource_version: preconditions.state_resource_version.clone(),
            },
        )))
    }

    async fn verify_root_only_cleanup(
        &self,
        address: &RootAddressV0,
        token: &str,
        preconditions: &PreconditionsV0,
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (root, receipt) = self
            .root_only_cleanup_fence(
                address,
                token,
                preconditions,
                RootOnlyCleanupCommandV0::VerifyOwnedChildrenAbsent,
                session,
            )
            .await?;
        for resource in [
            ResourceKindV0::ConfigMap,
            ResourceKindV0::Deployment,
            ResourceKindV0::Service,
            ResourceKindV0::AutomationRun,
        ] {
            if self
                .list_exact_prewrite_in_operation(resource, &address.namespace, session)
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
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        let (root, receipt) = self
            .root_only_cleanup_fence(
                address,
                token,
                preconditions,
                RootOnlyCleanupCommandV0::RemoveRootFinalizer,
                session,
            )
            .await?;
        for resource in [
            ResourceKindV0::ConfigMap,
            ResourceKindV0::Deployment,
            ResourceKindV0::Service,
            ResourceKindV0::AutomationRun,
        ] {
            if self
                .list_exact_prewrite_in_operation(resource, &address.namespace, session)
                .await?
                .items
                .into_iter()
                .any(|item| owned_by(&item, &root.metadata.uid))
            {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
        }
        let written = self
            .remove_finalizer_in_operation(&root, ROOT_FINALIZER, session)
            .await
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !finalizer_response_matches(&root, &written, ROOT_FINALIZER) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
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
        let operation = self.begin_operation_session();
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
        for _ in 0..=operation.max_prewrite_retries() {
            match self
                .persist_automation_run_completion_in_operation(
                    &reference,
                    expected_uid,
                    expected_old_resource_version,
                    expected_spec,
                    desired_status,
                    &operation,
                )
                .await
            {
                Err(
                    TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::PreWrite)
                    | TransportErrorV0::PreWriteRetryable(_),
                ) => {
                    operation.retry_prewrite_epoch()?;
                }
                result => return result,
            }
        }
        Err(TransportErrorV0::FiniteIoDeadline(
            FiniteIoStageV0::PreWrite,
        ))
    }

    async fn persist_automation_run_completion_in_operation(
        &self,
        reference: &ObjectRefV0,
        expected_uid: &str,
        expected_old_resource_version: &str,
        expected_spec: &AutomationRunSpec,
        desired_status: &AutomationRunStatus,
        operation: &OperationSessionV0,
    ) -> Result<AutomationCompletionAckV0, TransportErrorV0> {
        let observed = self
            .reread_prewrite_in_operation(reference, operation)
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
            reference: reference.clone(),
            metadata: observed.metadata.clone(),
            value: serde_json::json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"AutomationRun","metadata":{"name":reference.name,"namespace":reference.namespace},"status":desired_status}),
        };
        match self
            .apply_status(
                &desired,
                ResourceKindV0::AutomationRun,
                expected_old_resource_version,
                false,
                operation,
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
            Err(TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)) => {
                let recovered = self
                    .recovery_reread(&desired.reference, operation)
                    .await
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?
                    .ok_or(TransportErrorV0::ManualRecoveryHold)?;
                let ack = completion_recovery(
                    &recovered,
                    expected_uid,
                    expected_old_resource_version,
                    expected_spec,
                    desired_status,
                )
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                operation.prove_exact_response_loss_recovery()?;
                Ok(ack)
            }
            Err(error) if may_recover_response_loss(&error) => {
                let recovered = self
                    .recovery_reread(&desired.reference, operation)
                    .await
                    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?
                    .ok_or(TransportErrorV0::ManualRecoveryHold)?;
                let ack = completion_recovery(
                    &recovered,
                    expected_uid,
                    expected_old_resource_version,
                    expected_spec,
                    desired_status,
                )
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
                operation.prove_exact_response_loss_recovery()?;
                Ok(ack)
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
        let operation = self.begin_operation_session();
        self.watch_from_in_operation(resource, namespace, resource_version, &operation)
            .await
    }

    #[doc(hidden)]
    pub async fn watch_from_in_operation(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        resource_version: &str,
        operation: &OperationSessionV0,
    ) -> Result<WatchHandleV0, TransportErrorV0> {
        self.watch_from_in_operation_with_deletion_filter(
            resource,
            namespace,
            resource_version,
            operation,
            None,
            None,
        )
        .await
    }

    /// Opens the exact-name counterpart of [`Self::list_named_in_operation`]
    /// at the list's collection RV. Bookmarks are disabled because they are
    /// neither object authority nor a bootstrap completion acknowledgement.
    #[doc(hidden)]
    pub async fn watch_named_from_in_operation(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        name: &str,
        resource_version: &str,
        operation: &OperationSessionV0,
    ) -> Result<WatchHandleV0, TransportErrorV0> {
        validate_reference(&ObjectRefV0 {
            resource,
            namespace: namespace.into(),
            name: name.into(),
        })?;
        let selector = format!("metadata.name={name}");
        self.watch_from_in_operation_with_deletion_filter(
            resource,
            namespace,
            resource_version,
            operation,
            Some(&selector),
            None,
        )
        .await
    }

    /// Acquires the only operation-owned raw bootstrap cut.  It deliberately
    /// does not reuse the driver's retained root watch: each
    /// `EnsureBootstrap` gets a new exact-name LIST/WATCH/REREAD boundary,
    /// and the raw root plus deterministic State snapshot are sealed before
    /// the effect future can poll.  Any list/reread disagreement is a
    /// fail-closed no-write outcome, never a chance to rebind the actor's
    /// reduced root snapshot.
    #[doc(hidden)]
    pub async fn open_collected_bootstrap_cut_in_operation(
        &self,
        address: &RootAddressV0,
        command: &CommandV0,
        operation: &OperationSessionV0,
    ) -> Result<(WatchHandleV0, ObjectV0, Option<ObjectV0>), TransportErrorV0> {
        validate_address(address)?;
        if !operation.collector_bound || !matches!(command, CommandV0::EnsureBootstrap { .. }) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        let root_list = self
            .list_named_in_operation(
                ResourceKindV0::WebApplication,
                &address.namespace,
                &address.name,
                operation,
            )
            .await?;
        // `list_named_in_operation` already validates that the collection
        // supplied a nonempty RV and no foreign object leaked through its
        // selector. Only a genuinely empty exact-name list is a benign
        // no-write supersession; every other malformed collection shape is
        // sticky hold authority.
        let listed_root = match root_list.items.as_slice() {
            [] => return Err(TransportErrorV0::FreshBootstrapCutSuperseded),
            [root] => root.clone(),
            _ => return Err(TransportErrorV0::ManualRecoveryHold),
        };
        let watch = self
            .watch_named_from_in_operation(
                ResourceKindV0::WebApplication,
                &address.namespace,
                &address.name,
                &root_list.resource_version,
                operation,
            )
            .await?;
        let (root_ref, state_ref) = address.references();
        let Some(root) = self
            .reread_prewrite_in_operation(&root_ref, operation)
            .await?
        else {
            return Err(TransportErrorV0::FreshBootstrapCutSuperseded);
        };
        let state = self
            .reread_prewrite_in_operation(&state_ref, operation)
            .await?;
        if root != listed_root {
            return Err(TransportErrorV0::FreshBootstrapCutSuperseded);
        }
        validate_collected_bootstrap_baseline(address, command, &root, state.as_ref())?;
        Ok((watch, root, state))
    }

    async fn watch_from_in_operation_with_deletion_filter(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        resource_version: &str,
        operation: &OperationSessionV0,
        field_selector: Option<&str>,
        deletion_filter: Option<DeletionWatchFilterV0>,
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
            && let Some(mut watch) = watches.pop_front()
        {
            if let Some(filter) = deletion_filter {
                watch.install_deletion_filter(resource, filter);
            }
            return Ok(watch);
        }
        let watch_params =
            field_selector.map_or_else(kube::api::WatchParams::default, |selector| {
                // The named root bootstrap watch has no bookmark authority.
                // Keep the established broad-watch behavior for non-root
                // barriers so their wire contract remains unchanged.
                kube::api::WatchParams::default()
                    .fields(selector)
                    .disable_bookmarks()
            });
        let stream = within_operation(operation, FiniteIoStageV0::WatchOpen, async {
            self.api(resource, namespace)
                .watch(&watch_params, resource_version)
                .await
                .map_err(|error| map_watch_open_error(error, resource))
        })
        .await?;
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
        let pump = tokio::spawn(async move {
            futures_util::pin_mut!(events);
            while let Some(event) = events.next().await {
                if deletion_filter
                    .as_ref()
                    .is_some_and(|filter| filter.relevant(resource, &event))
                {
                    // The invalidation CAS precedes publication. A consumer
                    // may seal only while this gate remains Live.
                    let _ = deletion_filter
                        .as_ref()
                        .expect("checked deletion filter")
                        .gate
                        .invalidate();
                }
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
            pump_abort: Some(pump.abort_handle()),
            deletion_filter: None,
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
        let operation = self.begin_operation_session();
        let list = self
            .list_exact_in_operation(
                ResourceKindV0::AutomationRun,
                &expectation.root.namespace,
                &operation,
            )
            .await?;
        let _ = validate_automation_run_list(&list, &expectation)?;
        let watch = self
            .watch_from_in_operation(
                ResourceKindV0::AutomationRun,
                &expectation.root.namespace,
                &list.resource_version,
                &operation,
            )
            .await?;
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::AutomationRun,
            namespace: expectation.root.namespace.clone(),
            name: expectation.run_name.clone(),
        };
        let current = validate_automation_run_get(
            self.reread_in_operation(&reference, &operation).await?,
            &expectation,
        )?;
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
    #[cfg(test)]
    #[allow(dead_code)]
    async fn create_state(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.create_state_in_operation(object, &operation).await
    }
    #[cfg(test)]
    async fn create_state_in_operation(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::OxideLoopState, operation)
            .await
    }

    /// Bootstrap's State POST cannot use the generic create helper because
    /// the issued UID must be published with the fence before another phase
    /// can observe an open prewrite gate.
    async fn bootstrap_create_state_in_operation(
        &self,
        desired: &ObjectV0,
        root: &ObjectV0,
        token: &str,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        require_resource(desired, ResourceKindV0::OxideLoopState)?;
        validate_desired_object(&desired.value, &desired.reference)?;
        let dynamic: DynamicObject = serde_json::from_value(desired.value.clone())
            .map_err(|error| TransportErrorV0::Other(error.to_string()))?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(ResourceKindV0::OxideLoopState, &desired.reference.namespace)
                .create(&kube::api::PostParams::default(), &dynamic)
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::Create,
                        ResourceKindV0::OxideLoopState,
                    )
                })
        })
        .await?;
        let written = to_domain(
            ResourceKindV0::OxideLoopState,
            &desired.reference.namespace,
            written,
        )?;
        let written = recover_bootstrap_state(Some(written), desired, root, token)?;
        if operation.has_collected_bootstrap_fence() {
            operation.complete_bootstrap_subwrite(
                permit,
                BootstrapRootPhaseV0::StatePost,
                &written,
            )?;
        } else {
            permit.prove()?;
            operation.record_bootstrap_issued_state(&written)?;
        }
        Ok(written)
    }
    /// Creates only `AutomationRun` objects.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn create_automation_run(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.create_automation_run_in_operation(object, &operation)
            .await
    }
    async fn create_automation_run_in_operation(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::AutomationRun, operation)
            .await
    }
    /// Creates only core `ConfigMap` objects.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn create_config_map(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.create_config_map_in_operation(object, &operation)
            .await
    }
    async fn create_config_map_in_operation(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::ConfigMap, operation)
            .await
    }
    async fn create_deployment_in_operation(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::Deployment, operation)
            .await
    }
    /// Creates only core `Service` objects.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn create_service(&self, object: &ObjectV0) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.create_service_in_operation(object, &operation).await
    }
    async fn create_service_in_operation(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::Service, operation)
            .await
    }
    /// Creates only an already-planned Event; this remains private so callers
    /// cannot submit an arbitrary Event body.
    async fn create_event_in_operation(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::Event, operation).await
    }
    /// Reads Events only for the typed create/recovery proof; callers cannot
    /// obtain an arbitrary Event body through the generic read surface.
    async fn reread_event_in_operation(
        &self,
        reference: &ObjectRefV0,
        operation: &OperationSessionV0,
    ) -> Result<Option<ObjectV0>, TransportErrorV0> {
        require_reference_resource(reference, ResourceKindV0::Event)?;
        let result = within_operation(operation, FiniteIoStageV0::PreWrite, async {
            self.api(ResourceKindV0::Event, &reference.namespace)
                .get_opt(&reference.name)
                .await
                .map_err(|error| map_read_error(error, ReadOperationV0::Get, ResourceKindV0::Event))
        })
        .await;
        let result = match result {
            Err(TransportErrorV0::Other(message)) => {
                return Err(TransportErrorV0::PreWriteRetryable(message));
            }
            other => other,
        }?;
        result
            .map(|object| to_domain(ResourceKindV0::Event, &reference.namespace, object))
            .transpose()
    }
    /// Server-side applies only a `ConfigMap` with the exact expected RV.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn patch_config_map(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
        force_authorized: bool,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.apply(
            object,
            ResourceKindV0::ConfigMap,
            expected_rv,
            force_authorized,
            &operation,
        )
        .await
    }
    /// Server-side applies only a `Service` with the exact expected RV.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn patch_service(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
        force_authorized: bool,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.apply(
            object,
            ResourceKindV0::Service,
            expected_rv,
            force_authorized,
            &operation,
        )
        .await
    }
    /// Patches only an `OxideLoopState` with the exact expected RV.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn patch_state(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
        force_authorized: bool,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.apply(
            object,
            ResourceKindV0::OxideLoopState,
            expected_rv,
            force_authorized,
            &operation,
        )
        .await
    }

    /// Server-side applies only an `AutomationRun` with the exact expected RV.
    /// Server-side applies only the root status with the exact old RV.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn patch_web_application_status(
        &self,
        object: &ObjectV0,
        expected_rv: &str,
        force_authorized: bool,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.apply_status(
            object,
            ResourceKindV0::WebApplication,
            expected_rv,
            force_authorized,
            &operation,
        )
        .await
    }

    /// Deletes only an `AutomationRun` with UID and RV preconditions.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn delete_automation_run(
        &self,
        reference: &ObjectRefV0,
        uid: &str,
        rv: &str,
    ) -> Result<(), TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.delete(
            reference,
            ResourceKindV0::AutomationRun,
            uid,
            rv,
            &operation,
        )
        .await
    }
    /// Deletes only a core `ConfigMap` with UID and RV preconditions.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn delete_config_map(
        &self,
        reference: &ObjectRefV0,
        uid: &str,
        rv: &str,
    ) -> Result<(), TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.delete(reference, ResourceKindV0::ConfigMap, uid, rv, &operation)
            .await
    }
    /// Deletes only a core `Service` with UID and RV preconditions.
    #[cfg(test)]
    #[allow(dead_code)]
    async fn delete_service(
        &self,
        reference: &ObjectRefV0,
        uid: &str,
        rv: &str,
    ) -> Result<(), TransportErrorV0> {
        let operation = self.begin_operation_session();
        self.delete(reference, ResourceKindV0::Service, uid, rv, &operation)
            .await
    }

    async fn create(
        &self,
        object: &ObjectV0,
        expected: ResourceKindV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        require_resource(object, expected)?;
        validate_desired_object(&object.value, &object.reference)?;
        let dynamic: DynamicObject = serde_json::from_value(object.value.clone())
            .map_err(|error| TransportErrorV0::Other(error.to_string()))?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(expected, &object.reference.namespace)
                .create(&kube::api::PostParams::default(), &dynamic)
                .await
                .map_err(|error| map_write_error(error, WriteOperationV0::Create, expected))
        })
        .await?;
        let written = to_domain(expected, &object.reference.namespace, written)?;
        permit.prove()?;
        Ok(written)
    }

    async fn apply(
        &self,
        object: &ObjectV0,
        expected: ResourceKindV0,
        expected_rv: &str,
        force: bool,
        operation: &OperationSessionV0,
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
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(expected, &object.reference.namespace)
                .patch(&object.reference.name, &params, &Patch::Apply(&body))
                .await
                .map_err(|error| map_write_error(error, WriteOperationV0::Apply, expected))
        })
        .await?;
        let written = to_domain(expected, &object.reference.namespace, written)?;
        permit.prove()?;
        Ok(written)
    }

    async fn apply_status(
        &self,
        object: &ObjectV0,
        expected: ResourceKindV0,
        expected_rv: &str,
        force_authorized: bool,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        require_resource(object, expected)?;
        validate_desired_object(&object.value, &object.reference)?;
        if object.metadata.resource_version != expected_rv {
            return Err(TransportErrorV0::Stale("write resourceVersion"));
        }
        let mut body = object.value.clone();
        inject_resource_version(&mut body, expected_rv)?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(expected, &object.reference.namespace)
                .patch_status(
                    &object.reference.name,
                    &if force_authorized {
                        PatchParams::apply("oxideloop-controller").force()
                    } else {
                        PatchParams::apply("oxideloop-controller")
                    },
                    &Patch::Apply(&body),
                )
                .await
                .map_err(|error| map_write_error(error, WriteOperationV0::ApplyStatus, expected))
        })
        .await?;
        let written = to_domain(expected, &object.reference.namespace, written)?;
        permit.prove()?;
        Ok(written)
    }

    async fn delete(
        &self,
        reference: &ObjectRefV0,
        expected: ResourceKindV0,
        uid: &str,
        rv: &str,
        operation: &OperationSessionV0,
    ) -> Result<(), TransportErrorV0> {
        require_reference_resource(reference, expected)?;
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.into()),
                resource_version: Some(rv.into()),
            }),
            ..DeleteParams::default()
        };
        let permit = claim_first_write(operation)?;
        within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            self.api(expected, &reference.namespace)
                .delete(&reference.name, &params)
                .await
                .map_err(|error| map_write_error(error, WriteOperationV0::Delete, expected))
        })
        .await?;
        permit.prove()?;
        Ok(())
    }
}

/// A successful finalizer response is itself the only acknowledgement proof.
/// Kubernetes may update the resource version and managed fields, but every
/// other raw field must still be the exact prewrite witness with just the
/// requested finalizer removed.
fn finalizer_response_matches(before: &ObjectV0, written: &ObjectV0, removed: &str) -> bool {
    if written.reference != before.reference
        || written.metadata.uid != before.metadata.uid
        || written.metadata.resource_version.is_empty()
        || written.metadata.resource_version == before.metadata.resource_version
        || !raw_metadata_pins_match(written)
        || has_finalizer(written, removed)
    {
        return false;
    }
    let mut expected = before.value.clone();
    let mut actual = written.value.clone();
    let Some(expected_metadata) = expected.get_mut("metadata").and_then(Value::as_object_mut)
    else {
        return false;
    };
    let Some(finalizers) = expected_metadata
        .get_mut("finalizers")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let Some(mut remaining_finalizers) = finalizers
        .iter()
        .map(|finalizer| finalizer.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let Some(index) = remaining_finalizers
        .iter()
        .position(|finalizer| finalizer == removed)
    else {
        return false;
    };
    remaining_finalizers.remove(index);
    let actual_finalizers = match actual
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("finalizers"))
    {
        None => Vec::new(),
        Some(Value::Array(finalizers)) => match finalizers
            .iter()
            .map(|finalizer| finalizer.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
        {
            Some(finalizers) => finalizers,
            None => return false,
        },
        Some(_) => return false,
    };
    if actual_finalizers != remaining_finalizers {
        return false;
    }
    // Kubernetes may omit an empty finalizer list. Compare its normalized
    // absence after the exact remaining sequence has already been checked.
    expected_metadata.remove("finalizers");
    expected_metadata.remove("resourceVersion");
    expected_metadata.remove("managedFields");
    let Some(actual_metadata) = actual.get_mut("metadata").and_then(Value::as_object_mut) else {
        return false;
    };
    actual_metadata.remove("finalizers");
    actual_metadata.remove("resourceVersion");
    actual_metadata.remove("managedFields");
    actual == expected
}

fn api_resource(resource: ResourceKindV0) -> ApiResource {
    let (group, version, plural) = match resource {
        ResourceKindV0::WebApplication => ("demo.oxideloop.dev", "v1alpha1", "webapplications"),
        ResourceKindV0::OxideLoopState => ("runtime.oxideloop.dev", "v1alpha1", "oxideloopstates"),
        ResourceKindV0::AutomationRun => ("runtime.oxideloop.dev", "v1alpha1", "automationruns"),
        ResourceKindV0::ConfigMap => ("", "v1", "configmaps"),
        ResourceKindV0::Deployment => ("apps", "v1", "deployments"),
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
    if is_dns_label(namespace) {
        Ok(())
    } else {
        Err(TransportErrorV0::UnsupportedTarget)
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
    if !is_dns_subdomain(&address.name) || !is_dns_subdomain(&address.state_name) {
        Err(TransportErrorV0::UnsupportedTarget)
    } else {
        Ok(())
    }
}
fn validate_reference(reference: &ObjectRefV0) -> Result<(), TransportErrorV0> {
    validate_namespace(&reference.namespace)?;
    if is_dns_subdomain(&reference.name) {
        Ok(())
    } else {
        Err(TransportErrorV0::UnsupportedTarget)
    }
}

/// Kubernetes namespaces are DNS labels: lowercase ASCII alphanumerics and
/// interior hyphens only, with the API's 63-byte maximum.  Do this before a
/// namespace reaches an API path or a field selector is constructed.
fn is_dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .next_back()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Kubernetes object names accepted by this transport are DNS subdomains.
/// A selector must never be composed from a name that could inject a comma,
/// equality predicate, whitespace, or an invalid label boundary.
fn is_dns_subdomain(value: &str) -> bool {
    !value.is_empty() && value.len() <= 253 && value.split('.').all(is_dns_label)
}

fn validate_collected_bootstrap_baseline(
    address: &RootAddressV0,
    command: &CommandV0,
    root: &ObjectV0,
    state: Option<&ObjectV0>,
) -> Result<(), TransportErrorV0> {
    let CommandV0::EnsureBootstrap {
        root_uid,
        root_generation,
        root_resource_version,
        state_name,
        ..
    } = command
    else {
        return Err(TransportErrorV0::ManualRecoveryHold);
    };
    if state_name != &address.state_name
        || !has_exact_object_identity(
            root,
            ResourceKindV0::WebApplication,
            &address.namespace,
            &address.name,
        )
        || root.metadata.uid != *root_uid
        || root.metadata.resource_version != *root_resource_version
        || root.value.pointer("/metadata/uid").and_then(Value::as_str) != Some(root_uid.as_str())
        || root
            .value
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            != Some(root_resource_version.as_str())
        || generation(root)? != *root_generation
        || root
            .value
            .pointer("/metadata/deletionTimestamp")
            .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty()))
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    if state.is_some_and(|state| {
        !has_exact_object_identity(
            state,
            ResourceKindV0::OxideLoopState,
            &address.namespace,
            &address.state_name,
        ) || state.metadata.uid.is_empty()
            || state.metadata.resource_version.is_empty()
            || state.value.pointer("/metadata/uid").and_then(Value::as_str)
                != Some(state.metadata.uid.as_str())
            || state
                .value
                .pointer("/metadata/resourceVersion")
                .and_then(Value::as_str)
                != Some(state.metadata.resource_version.as_str())
    }) {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(())
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

fn is_state_write_conflict(error: &TransportErrorV0) -> bool {
    matches!(
        error,
        TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
            operation: WriteOperationV0::Apply,
            resource: ResourceKindV0::OxideLoopState,
        })
    )
}

fn is_child_apply_write_conflict(error: &TransportErrorV0, resource: ResourceKindV0) -> bool {
    matches!(
        error,
        TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
            operation: WriteOperationV0::Apply,
            resource: actual,
        }) if *actual == resource
    )
}

#[cfg(test)]
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

/// The forced state/status paths are intentionally narrower than the normal
/// effect fence: every ownership and durable-link claim must be present in the
/// fresh objects which immediately precede the write.
fn validate_persist_force_authority(
    address: &RootAddressV0,
    fence: &EffectFenceV0,
    preconditions: &PreconditionsV0,
) -> Result<(), TransportErrorV0> {
    if !has_force_root_authority(address, &fence.root, preconditions)
        || !has_exact_object_identity(
            &fence.state,
            ResourceKindV0::OxideLoopState,
            &address.namespace,
            &address.state_name,
        )
        || fence.state.metadata.uid != preconditions.state_uid
        || fence.state.metadata.resource_version != preconditions.state_resource_version
        || !has_finalizer(&fence.state, STATE_FINALIZER)
        || !has_root_uid_annotation(&fence.state, &fence.root.metadata.uid)
        || !has_exact_canonical_owner(
            &fence.state,
            &fence.root.reference,
            &fence.root.metadata.uid,
        )
        || !has_state_link(&fence.root, &fence.state)
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(())
}

fn validate_root_status_force_authority(
    address: &RootAddressV0,
    fence: &EffectFenceV0,
    preconditions: &PreconditionsV0,
    token: &str,
    status: &WebApplicationStatus,
) -> Result<(), TransportErrorV0> {
    if status.last_status_persist_token.as_deref() != Some(token) {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    validate_persist_force_authority(address, fence, preconditions)
}

fn has_force_root_authority(
    address: &RootAddressV0,
    root: &ObjectV0,
    preconditions: &PreconditionsV0,
) -> bool {
    has_exact_object_identity(
        root,
        ResourceKindV0::WebApplication,
        &address.namespace,
        &address.name,
    ) && root.metadata.uid == preconditions.root_uid
        && root.metadata.resource_version == preconditions.root_resource_version
        && generation(root).ok() == Some(preconditions.root_generation)
        && has_finalizer(root, ROOT_FINALIZER)
        && has_root_uid_annotation(root, &root.metadata.uid)
}

fn has_exact_object_identity(
    object: &ObjectV0,
    resource: ResourceKindV0,
    namespace: &str,
    name: &str,
) -> bool {
    object.reference.resource == resource
        && object.reference.namespace == namespace
        && object.reference.name == name
        && object.value.get("apiVersion").and_then(Value::as_str) == Some(resource.api_version())
        && object.value.get("kind").and_then(Value::as_str) == Some(resource.kind())
        && object
            .value
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            == Some(name)
        && object
            .value
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            == Some(namespace)
}

fn has_root_uid_annotation(object: &ObjectV0, root_uid: &str) -> bool {
    object
        .metadata
        .annotations
        .get(ROOT_UID_ANNOTATION)
        .is_some_and(|value| value == root_uid)
}

/// Authenticates the durable one-command dispatch capability. This check is
/// deliberately made from the exact State object returned by the final fresh
/// read, not from an informer/cache snapshot or command-local claims.
fn validate_dispatch_authority(
    address: &RootAddressV0,
    root: &ObjectV0,
    state: &ObjectV0,
    preconditions: &PreconditionsV0,
    transaction_id: &str,
    attempt_token: &str,
    command: &MutationCommandV0,
) -> Result<(), TransportErrorV0> {
    let (_, state_ref) = address.references();
    if !has_exact_object_identity(
        root,
        ResourceKindV0::WebApplication,
        &address.namespace,
        &address.name,
    ) || root.metadata.uid != preconditions.root_uid
        || root.metadata.resource_version != preconditions.root_resource_version
        || !has_exact_object_identity(
            state,
            ResourceKindV0::OxideLoopState,
            &state_ref.namespace,
            &state_ref.name,
        )
        || state.metadata.uid != preconditions.state_uid
        || state.metadata.resource_version != preconditions.state_resource_version
        || !has_finalizer(state, STATE_FINALIZER)
        || !has_root_uid_annotation(state, &root.metadata.uid)
        || !has_exact_canonical_owner(state, &root.reference, &root.metadata.uid)
        || !has_state_link(root, state)
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }

    let spec: OxideLoopStateSpec = serde_json::from_value(
        state
            .value
            .get("spec")
            .cloned()
            .ok_or(TransportErrorV0::ManualRecoveryHold)?,
    )
    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
    spec.validate()
        .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
    if spec.root.uid != root.metadata.uid {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let transaction = spec
        .state
        .apply_transaction
        .as_ref()
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    if transaction.id != transaction_id
        || transaction.attempt_token != attempt_token
        || command.attempt_token != attempt_token
        || transaction.next_command_lifecycle.state
            != NextCommandLifecycleStateV0::DispatchAuthorizedMayHaveIssued
        || transaction.next_command_lifecycle.command_id.as_deref()
            != Some(command.command_id.as_str())
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let intent = transaction
        .prepared_intents
        .get(transaction.results.len())
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    if intent.command_id != command.command_id
        || intent.logical_id != command.logical_id
        || intent.identity.api_version != command.identity.api_version
        || intent.identity.kind != command.identity.kind
        || intent.identity.namespace != command.identity.namespace
        || intent.identity.name != command.identity.name
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let expected_operation = match &command.decision {
        MutationDecision::PreconditionedDelete { .. } => ApplyOperation::Delete,
        MutationDecision::Post { .. } | MutationDecision::ConditionalApply { .. } => {
            ApplyOperation::Apply
        }
        _ => return Err(TransportErrorV0::ManualRecoveryHold),
    };
    if intent.operation != expected_operation {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let claim = transaction
        .recovery_claims
        .iter()
        .find(|claim| claim.command_id == command.command_id)
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    if claim.root_uid != root.metadata.uid
        || claim.transaction_id != transaction_id
        || claim.command_id != command.command_id
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    match &command.decision {
        MutationDecision::PreconditionedDelete { preconditions, .. }
            if intent.expected_uid.as_deref() != Some(preconditions.uid.as_str())
                || intent.expected_resource_version.as_deref()
                    != Some(preconditions.resource_version.as_str()) =>
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        MutationDecision::ConditionalApply { expected, .. }
            if intent.expected_uid.as_deref() != Some(expected.uid.as_str())
                || intent.expected_resource_version.as_deref()
                    != Some(expected.resource_version.as_str()) =>
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        _ => {}
    }
    let pending = spec
        .state
        .pending_persist
        .as_ref()
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    if pending.purpose != PersistPurpose::DispatchAuthorization
        || pending.phase != DurablePhase::DispatchAuthorization
        || pending.root_generation != preconditions.root_generation
        || state.metadata.annotations.get(TRANSACTION_ID_ANNOTATION) != Some(&pending.token)
        || state.metadata.annotations.get(COMMAND_ID_ANNOTATION) != Some(&pending.token)
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(())
}

fn conditional_apply_force_authorized(
    command: &MutationCommandV0,
    content_type: &str,
    field_manager: &str,
    expected: &oxideloop_mutation_model::ObjectIdentity,
    root: &ObjectV0,
    target: &ObjectV0,
) -> bool {
    conditional_apply_envelope_authorized(command)
        && content_type == APPLY_PATCH_CONTENT_TYPE
        && field_manager == CONTROLLER_FIELD_MANAGER
        && target.metadata.uid == expected.uid
        && target.metadata.resource_version == expected.resource_version
        && has_root_uid_annotation(target, &root.metadata.uid)
        && has_exact_canonical_owner(target, &root.reference, &root.metadata.uid)
}

fn conditional_apply_envelope_authorized(command: &MutationCommandV0) -> bool {
    matches!(
        &command.decision,
        MutationDecision::ConditionalApply {
            method: HttpMethod::Patch,
            content_type,
            field_manager,
            force: true,
            expected,
            ..
        } if content_type == APPLY_PATCH_CONTENT_TYPE
            && field_manager == CONTROLLER_FIELD_MANAGER
            && command.prepared_expected_target.as_ref() == Some(expected)
    )
}

fn mutation_failed_outcome(
    transaction_id: &str,
    attempt_token: &str,
    reason: &str,
) -> CommandOutcomeV0 {
    CommandOutcomeV0::Event(Box::new(
        oxideloop_controller_core::EventV0::MutationFailed {
            attempt_token: attempt_token.into(),
            transaction_id: transaction_id.into(),
            reason: reason.into(),
        },
    ))
}

fn mutation_not_issued_outcome(
    transaction_id: &str,
    attempt_token: &str,
    command_id: &str,
    reason: &str,
) -> CommandOutcomeV0 {
    CommandOutcomeV0::Event(Box::new(
        oxideloop_controller_core::EventV0::MutationNotIssued {
            transaction_id: transaction_id.into(),
            attempt_token: attempt_token.into(),
            command_id: command_id.into(),
            reason: reason.into(),
        },
    ))
}

fn owned_by(object: &ObjectV0, root_uid: &str) -> bool {
    object.metadata.owner_uids.iter().any(|uid| uid == root_uid)
}

/// A linked deletion removes the State finalizer only after fresh root and
/// State witnesses prove one current durable deletion transaction. Bootstrap
/// authority is intentionally not consulted here: it describes the separate
/// unlinked cleanup route, and the current State persist legitimately changes
/// its spec and operation token as deletion progresses.
fn validate_linked_deletion_state_finalizer_authority(
    address: &RootAddressV0,
    fence: &EffectFenceV0,
    preconditions: &PreconditionsV0,
    command_token: &str,
) -> Result<(), TransportErrorV0> {
    let root = &fence.root;
    let state = &fence.state;
    let receipt = linked_deletion_receipt(root, address, preconditions)?;
    if !has_linked_deleting_root_authority(address, root, preconditions)
        || !has_exact_object_identity(
            state,
            ResourceKindV0::OxideLoopState,
            &address.namespace,
            &address.state_name,
        )
        || state.metadata.uid != preconditions.state_uid
        || state.metadata.resource_version != preconditions.state_resource_version
        || !raw_metadata_pins_match(state)
        || !has_finalizer(state, STATE_FINALIZER)
        || !has_root_uid_annotation(state, &root.metadata.uid)
        || !has_exact_canonical_owner(state, &root.reference, &root.metadata.uid)
        || !has_state_link(root, state)
        || !linked_deletion_state_spec_matches(root, state, preconditions, &receipt)
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let expected = linked_state_finalizer_command_token(
        &root.metadata.uid,
        preconditions.root_generation,
        &root.metadata.resource_version,
        &address.state_name,
        &state.metadata.uid,
        &state.metadata.resource_version,
        &receipt.deletion_id,
        receipt.deletion_start_generation,
        &receipt.cleanup_token,
    );
    if command_token != expected {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(())
}

/// The linked root finalizer may be removed only after the deterministic State
/// name has been reread as absent. Its authority is retained in the root's
/// status receipt and the original State pins, not in a bootstrap bundle.
fn validate_linked_root_finalizer_after_state_absence(
    address: &RootAddressV0,
    root: &ObjectV0,
    preconditions: &PreconditionsV0,
    command_token: &str,
) -> Result<(), TransportErrorV0> {
    let receipt = linked_deletion_receipt(root, address, preconditions)?;
    if !has_linked_deleting_root_authority(address, root, preconditions)
        || preconditions.state_uid.is_empty()
        || preconditions.state_resource_version.is_empty()
        || !root.value.pointer("/status/state").is_some_and(|link| {
            link.get("rootUid").and_then(Value::as_str) == Some(root.metadata.uid.as_str())
                && link.get("name").and_then(Value::as_str) == Some(address.state_name.as_str())
                && link.get("uid").and_then(Value::as_str) == Some(preconditions.state_uid.as_str())
        })
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let expected = linked_root_finalizer_command_token(
        &root.metadata.uid,
        preconditions.root_generation,
        &root.metadata.resource_version,
        &address.state_name,
        &preconditions.state_uid,
        &preconditions.state_resource_version,
        &receipt.deletion_id,
        receipt.deletion_start_generation,
        &receipt.cleanup_token,
    );
    if command_token != expected {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(())
}

fn has_linked_deleting_root_authority(
    address: &RootAddressV0,
    root: &ObjectV0,
    preconditions: &PreconditionsV0,
) -> bool {
    let expected_state_name = oxideloop_controller_api::state_name(&root.metadata.uid).ok();
    has_exact_object_identity(
        root,
        ResourceKindV0::WebApplication,
        &address.namespace,
        &address.name,
    ) && root.metadata.uid == preconditions.root_uid
        && root.metadata.resource_version == preconditions.root_resource_version
        && generation(root).ok() == Some(preconditions.root_generation)
        && raw_metadata_pins_match(root)
        && root
            .value
            .pointer("/metadata/deletionTimestamp")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && has_finalizer(root, ROOT_FINALIZER)
        && has_root_uid_annotation(root, &root.metadata.uid)
        && expected_state_name.as_deref() == Some(address.state_name.as_str())
}

fn linked_deletion_receipt(
    root: &ObjectV0,
    address: &RootAddressV0,
    preconditions: &PreconditionsV0,
) -> Result<DeletionCleanupReceipt, TransportErrorV0> {
    let status: WebApplicationStatus = serde_json::from_value(
        root.value
            .get("status")
            .cloned()
            .ok_or(TransportErrorV0::ManualRecoveryHold)?,
    )
    .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
    let receipt = status
        .deletion_cleanup_receipt
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    receipt
        .validate()
        .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
    if receipt.root_uid != root.metadata.uid
        || receipt.state_name != address.state_name
        || receipt.state_uid != preconditions.state_uid
        || receipt.deletion_start_generation > preconditions.root_generation
        || preconditions.state_resource_version.is_empty()
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(receipt)
}

fn linked_deletion_state_spec_matches(
    root: &ObjectV0,
    state: &ObjectV0,
    preconditions: &PreconditionsV0,
    receipt: &DeletionCleanupReceipt,
) -> bool {
    let spec: OxideLoopStateSpec =
        match serde_json::from_value(state.value.get("spec").cloned().unwrap_or(Value::Null)) {
            Ok(spec) => spec,
            Err(_) => return false,
        };
    if spec.validate().is_err() || spec.root.uid != root.metadata.uid {
        return false;
    }
    let Some(deletion) = spec.state.deletion.as_ref() else {
        return false;
    };
    let Some(pending) = spec.state.pending_persist.as_ref() else {
        return false;
    };
    let deletion_purpose = matches!(
        pending.purpose,
        PersistPurpose::DeletionStart
            | PersistPurpose::DeletionPrepare
            | PersistPurpose::DeletionMutationResult
            | PersistPurpose::DeletionAutomationCleanup
    ) || (pending.purpose == PersistPurpose::GenerationAdvanceClosure
        && spec
            .state
            .generation_advance_closure
            .as_ref()
            .is_some_and(|closure| {
                matches!(
                    closure.purpose,
                    GenerationAdvanceClosurePurpose::DeletionMutationResult
                        | GenerationAdvanceClosurePurpose::DeletionAutomationCleanup
                )
            }));
    deletion.id == receipt.deletion_id
        && deletion.deletion_start_generation == receipt.deletion_start_generation
        && pending.phase == pending.purpose.durable_phase()
        && pending.root_generation == preconditions.root_generation
        && deletion_purpose
        && has_recovery_proof(state, &root.metadata.uid, &pending.token, &pending.token)
}

const BOOTSTRAP_AUTHORITY_ANNOTATIONS_V1: [&str; 8] = [
    ROOT_UID_ANNOTATION,
    TRANSACTION_ID_ANNOTATION,
    COMMAND_ID_ANNOTATION,
    BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION,
    BOOTSTRAP_STATE_NAME_ANNOTATION,
    BOOTSTRAP_STATE_UID_ANNOTATION,
    BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION,
    BOOTSTRAP_PROOF_TOKEN_ANNOTATION,
];

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Parses all-or-nothing v1 authority from an unlinked root. `Ok(None)` is a
/// genuinely fresh root with no bootstrap authority fields; any partial,
/// malformed, inconsistent, or unsupported version is an error. This is used
/// both before bootstrap creates anything and by restart/deletion recovery so
/// neither path can overwrite a suspicious partial bundle.
pub fn bootstrap_authority_bundle_v1(
    root: &ObjectV0,
) -> Result<Option<BootstrapAuthorityBundleV1V0>, TransportErrorV0> {
    parse_bootstrap_authority_bundle_v1(root, true)
}

/// Recognizes the one durable shape left after an exact unlinked-v1 cleanup
/// has already removed *our* root finalizer while another controller still
/// retains the deleting root. This is deliberately not a general relaxed
/// authority parser: it is read-only restart evidence only. Callers must
/// still prove the bundle's original State UID is absent before treating this
/// root as a completed cleanup.
#[doc(hidden)]
pub fn completed_unlinked_bootstrap_cleanup_bundle_v1(
    root: &ObjectV0,
) -> Result<Option<BootstrapAuthorityBundleV1V0>, TransportErrorV0> {
    let bundle = parse_bootstrap_authority_bundle_v1(root, false)?;
    let Some(bundle) = bundle else {
        return Ok(None);
    };
    let finalizers = strict_finalizers(root)?;
    let expected_state_name = oxideloop_controller_api::state_name(&root.metadata.uid)
        .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
    // The normal unlinked-v1 cleanup authority still owns roots carrying our
    // finalizer. This relaxed parser is only for its already-completed tail.
    if finalizers.contains(&ROOT_FINALIZER) {
        return Ok(None);
    }
    if !has_exact_object_identity(
        root,
        ResourceKindV0::WebApplication,
        &root.reference.namespace,
        &root.reference.name,
    ) || root.metadata.uid.is_empty()
        || root.metadata.resource_version.is_empty()
        || !raw_metadata_pins_match(root)
        || root
            .value
            .pointer("/metadata/deletionTimestamp")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || root.value.pointer("/status/state").is_some()
        || root
            .value
            .pointer("/status/deletionCleanupReceipt")
            .is_some()
        || generation(root).is_err()
        || bundle.state_name != expected_state_name
        || finalizers.is_empty()
        || finalizers.iter().any(|finalizer| finalizer.is_empty())
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(Some(bundle))
}

/// Parses the immutable fields independently of the root-finalizer bit. The
/// latter is intentionally omitted only while proving the exact response to
/// removal of that finalizer; every authority-acquiring caller uses the
/// public, finalizer-required parser above.
fn parse_bootstrap_authority_bundle_v1(
    root: &ObjectV0,
    require_root_finalizer: bool,
) -> Result<Option<BootstrapAuthorityBundleV1V0>, TransportErrorV0> {
    let annotations = &root.metadata.annotations;
    if BOOTSTRAP_AUTHORITY_ANNOTATIONS_V1
        .iter()
        .all(|key| !annotations.contains_key(*key))
    {
        return Ok(None);
    }
    let get = |key| {
        annotations
            .get(key)
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .ok_or(TransportErrorV0::ManualRecoveryHold)
    };
    let root_uid = get(ROOT_UID_ANNOTATION)?;
    let transaction = get(TRANSACTION_ID_ANNOTATION)?;
    let command = get(COMMAND_ID_ANNOTATION)?;
    let version = get(BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION)?;
    let state_name = get(BOOTSTRAP_STATE_NAME_ANNOTATION)?;
    let state_uid = get(BOOTSTRAP_STATE_UID_ANNOTATION)?;
    let state_spec_digest = get(BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION)?;
    let proof_token = get(BOOTSTRAP_PROOF_TOKEN_ANNOTATION)?;
    if root.reference.resource != ResourceKindV0::WebApplication
        || root.metadata.uid.is_empty()
        || root_uid != root.metadata.uid
        || transaction != command
        || command != proof_token
        || version != BOOTSTRAP_AUTHORITY_VERSION_V1
        || !is_dns_subdomain(state_name)
        || state_uid.is_empty()
        || state_uid.len() > MAX_BUNDLE_IDENTITY_BYTES
        || !is_lower_hex_sha256(state_spec_digest)
        || (require_root_finalizer && !has_finalizer(root, ROOT_FINALIZER))
        || !raw_metadata_pins_match(root)
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let raw_annotations = root
        .value
        .pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    if BOOTSTRAP_AUTHORITY_ANNOTATIONS_V1.iter().any(|key| {
        raw_annotations.get(*key).and_then(Value::as_str)
            != annotations.get(*key).map(String::as_str)
    }) {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(Some(BootstrapAuthorityBundleV1V0 {
        state_name: state_name.into(),
        state_uid: state_uid.into(),
        state_spec_digest: state_spec_digest.into(),
        proof_token: proof_token.into(),
    }))
}

/// Validates the extant State against the complete durable v1 bundle. This is
/// deliberately usable by the runtime loader, which must decide whether an
/// unlinked deleting root may enter bounded cleanup after a restart. It does
/// not grant link or bootstrap authority.
#[must_use]
pub fn bootstrap_authority_bundle_v1_matches_state(
    root: &ObjectV0,
    state: &ObjectV0,
    bundle: &BootstrapAuthorityBundleV1V0,
) -> bool {
    has_exact_object_identity(
        state,
        ResourceKindV0::OxideLoopState,
        &root.reference.namespace,
        &bundle.state_name,
    ) && state.metadata.uid == bundle.state_uid
        && !state.metadata.resource_version.is_empty()
        && raw_metadata_pins_match(state)
        && has_exact_canonical_owner(state, &root.reference, &root.metadata.uid)
        && has_recovery_proof(
            state,
            &root.metadata.uid,
            &bundle.proof_token,
            &bundle.proof_token,
        )
        && bootstrap_state_spec_digest(state).is_ok_and(|digest| digest == bundle.state_spec_digest)
}

fn has_bootstrap_root_metadata(root: &ObjectV0, token: &str) -> bool {
    bootstrap_authority_bundle_v1(root)
        .ok()
        .flatten()
        .is_some_and(|bundle| bundle.proof_token == token)
}

/// Emits a minimal, machine-readable witness for a bootstrap error in the
/// debug-only failpoint build. The domain error is deliberately not wrapped or
/// otherwise changed: callers still receive exactly the error that caused the
/// controller to hold.
#[cfg(feature = "bootstrap-diagnostics")]
fn emit_bootstrap_diagnostic(
    boundary: &str,
    error: &TransportErrorV0,
    root: Option<&ObjectV0>,
    state: Option<&ObjectV0>,
    session: &OperationSessionV0,
) {
    eprintln!(
        "{}",
        bootstrap_diagnostic_value(boundary, error, root, state, session)
    );
}

#[cfg(feature = "bootstrap-diagnostics")]
fn bootstrap_diagnostic_value(
    boundary: &str,
    error: &TransportErrorV0,
    root: Option<&ObjectV0>,
    state: Option<&ObjectV0>,
    session: &OperationSessionV0,
) -> Value {
    let object_summary = |object: &ObjectV0, finalizer: &str| {
        serde_json::json!({
            "uid": object.metadata.uid.as_str(),
            "resourceVersion": object.metadata.resource_version.as_str(),
            "finalizerPresent": has_finalizer(object, finalizer),
        })
    };
    serde_json::json!({
        "event": "oxideloop_bootstrap_diagnostic_v1",
        "stage": boundary,
        "error": error.to_string(),
        "collectedBootstrapFence": session.has_collected_bootstrap_fence(),
        "prewriteGate": format!("{:?}", session.prewrite_gate().state()),
        "root": root.map(|object| object_summary(object, ROOT_FINALIZER)),
        "state": state.map(|object| object_summary(object, STATE_FINALIZER)),
    })
}

#[cfg(not(feature = "bootstrap-diagnostics"))]
fn emit_bootstrap_diagnostic(
    _boundary: &str,
    _error: &TransportErrorV0,
    _root: Option<&ObjectV0>,
    _state: Option<&ObjectV0>,
    _session: &OperationSessionV0,
) {
}

/// Confirms the durable v1 bootstrap bundle against the exact State response
/// issued by this operation.  A same-name child, a token-only annotation, or
/// an unbound digest is deliberately not recovery/link authority.
fn bootstrap_root_metadata_matches_state(root: &ObjectV0, state: &ObjectV0, token: &str) -> bool {
    bootstrap_authority_bundle_v1(root)
        .ok()
        .flatten()
        .is_some_and(|bundle| {
            bundle.proof_token == token
                && bootstrap_authority_bundle_v1_matches_state(root, state, &bundle)
        })
}

/// The raw JSON response is authoritative. Test DTOs can be constructed with
/// a typed metadata copy that does not match their wire value, so every v1
/// proof checks that identity and annotations still agree with the exact raw
/// object before using them as cleanup or final-proof authority.
fn raw_metadata_pins_match(object: &ObjectV0) -> bool {
    let Some(metadata) = object.value.get("metadata").and_then(Value::as_object) else {
        return false;
    };
    if metadata.get("uid").and_then(Value::as_str) != Some(object.metadata.uid.as_str())
        || metadata.get("resourceVersion").and_then(Value::as_str)
            != Some(object.metadata.resource_version.as_str())
    {
        return false;
    }
    match metadata.get("annotations") {
        None => object.metadata.annotations.is_empty(),
        Some(Value::Object(annotations)) => {
            serde_json::from_value::<BTreeMap<String, String>>(Value::Object(annotations.clone()))
                .is_ok_and(|annotations| annotations == object.metadata.annotations)
        }
        Some(_) => false,
    }
}

fn strict_finalizers(object: &ObjectV0) -> Result<Vec<&str>, TransportErrorV0> {
    let metadata = object
        .value
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    let Some(finalizers) = metadata.get("finalizers") else {
        return Ok(Vec::new());
    };
    finalizers
        .as_array()
        .ok_or(TransportErrorV0::ManualRecoveryHold)?
        .iter()
        .map(|value| value.as_str().ok_or(TransportErrorV0::ManualRecoveryHold))
        .collect()
}

fn validate_unlinked_deleting_bootstrap_root(
    root: &ObjectV0,
) -> Result<Option<BootstrapAuthorityBundleV1V0>, TransportErrorV0> {
    let bundle = bootstrap_authority_bundle_v1(root)?;
    let Some(bundle) = bundle else {
        return Ok(None);
    };
    validate_unlinked_deleting_bootstrap_root_with_bundle(root, &bundle)?;
    Ok(Some(bundle))
}

fn validate_unlinked_deleting_bootstrap_root_with_bundle(
    root: &ObjectV0,
    bundle: &BootstrapAuthorityBundleV1V0,
) -> Result<(), TransportErrorV0> {
    if parse_bootstrap_authority_bundle_v1(root, true)? != Some(bundle.clone())
        || !has_exact_object_identity(
            root,
            ResourceKindV0::WebApplication,
            &root.reference.namespace,
            &root.reference.name,
        )
        || !raw_metadata_pins_match(root)
        || root.metadata.uid.is_empty()
        || root.metadata.resource_version.is_empty()
        || root
            .value
            .pointer("/metadata/deletionTimestamp")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || root.value.pointer("/status/state").is_some()
        || root
            .value
            .pointer("/status/deletionCleanupReceipt")
            .is_some()
        || bundle.state_name
            != oxideloop_controller_api::state_name(&root.metadata.uid)
                .map_err(|_| TransportErrorV0::ManualRecoveryHold)?
        || generation(root).is_err()
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(())
}

fn validate_unlinked_deleting_bootstrap_state(
    root: &ObjectV0,
    state: &ObjectV0,
    bundle: &BootstrapAuthorityBundleV1V0,
) -> Result<(), TransportErrorV0> {
    if !bootstrap_authority_bundle_v1_matches_state(root, state, bundle)
        || strict_finalizers(state).is_err()
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    Ok(())
}

fn bootstrap_unlinked_root_after_finalizer_removal_matches(
    observed: &ObjectV0,
    before: &ObjectV0,
    bundle: &BootstrapAuthorityBundleV1V0,
) -> bool {
    parse_bootstrap_authority_bundle_v1(observed, false)
        .ok()
        .flatten()
        .as_ref()
        == Some(bundle)
        && has_exact_object_identity(
            observed,
            ResourceKindV0::WebApplication,
            &before.reference.namespace,
            &before.reference.name,
        )
        && observed.metadata.uid == before.metadata.uid
        && !observed.metadata.resource_version.is_empty()
        && raw_metadata_pins_match(observed)
        && observed
            .value
            .pointer("/metadata/deletionTimestamp")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && observed.value.pointer("/status/state").is_none()
        && observed
            .value
            .pointer("/status/deletionCleanupReceipt")
            .is_none()
        && !has_finalizer(observed, ROOT_FINALIZER)
}

fn finalizer_removal_patch(
    object: &ObjectV0,
    index: usize,
    finalizer: &str,
) -> Result<json_patch::Patch, TransportErrorV0> {
    serde_json::from_value(serde_json::json!([
        {"op":"test","path":"/metadata/uid","value":object.metadata.uid},
        {"op":"test","path":"/metadata/resourceVersion","value":object.metadata.resource_version},
        {"op":"test","path":format!("/metadata/finalizers/{index}"),"value":finalizer},
        {"op":"remove","path":format!("/metadata/finalizers/{index}")}
    ]))
    .map_err(|_| TransportErrorV0::ManualRecoveryHold)
}

/// A bootstrap write is recoverable only when its response may have been
/// applied.  The caller must immediately perform the phase-specific exact
/// proof; this predicate never authorizes a retry.
fn bootstrap_write_response_lost(error: &TransportErrorV0) -> bool {
    matches!(
        error,
        TransportErrorV0::FiniteIoDeadline(FiniteIoStageV0::WriteResponse)
    ) || may_recover_response_loss(error)
}

/// Kubernetes may advance `resourceVersion` and rewrite `managedFields` for
/// any bootstrap patch response. Those are the only server-owned differences
/// excluded from bootstrap response proofs; every other raw field must match
/// the phase-specific transform exactly.
fn bootstrap_response_projection(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(metadata) = value
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)
    {
        metadata.remove("resourceVersion");
        metadata.remove("managedFields");
    }
    value
}

fn bootstrap_controller_patch_params() -> PatchParams {
    PatchParams {
        field_manager: Some(CONTROLLER_FIELD_MANAGER.into()),
        ..PatchParams::default()
    }
}

fn bootstrap_state_finalizer_proof(observed: &ObjectV0, before: &ObjectV0) -> bool {
    if observed.reference != before.reference || observed.metadata.uid != before.metadata.uid {
        return false;
    }
    let mut expected = bootstrap_response_projection(&before.value);
    let Some(metadata) = expected
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let finalizers = metadata
        .entry("finalizers")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(finalizers) = finalizers.as_array_mut() else {
        return false;
    };
    if !finalizers
        .iter()
        .any(|finalizer| finalizer.as_str() == Some(STATE_FINALIZER))
    {
        finalizers.push(Value::String(STATE_FINALIZER.into()));
    }
    has_finalizer(observed, STATE_FINALIZER)
        && bootstrap_response_projection(&observed.value) == expected
}

/// Exact response/recovery proof for removing one existing finalizer. Unlike
/// an ordinary deletion acknowledgement, this permits continuation only when
/// the full raw object is the original object with precisely that array entry
/// removed (plus Kubernetes-owned resourceVersion/managedFields changes).
fn bootstrap_finalizer_removal_proof(
    observed: &ObjectV0,
    before: &ObjectV0,
    finalizer: &str,
) -> bool {
    if observed.reference != before.reference
        || observed.metadata.uid != before.metadata.uid
        || observed.metadata.resource_version.is_empty()
    {
        return false;
    }
    let Some(index) = finalizer_index(before, finalizer) else {
        return false;
    };
    let mut expected = bootstrap_response_projection(&before.value);
    let Some(finalizers) = expected
        .pointer_mut("/metadata/finalizers")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    if finalizers.get(index).and_then(Value::as_str) != Some(finalizer) {
        return false;
    }
    finalizers.remove(index);
    !has_finalizer(observed, finalizer)
        && raw_metadata_pins_match(observed)
        && bootstrap_response_projection(&observed.value) == expected
}

fn bootstrap_root_metadata_proof(
    observed: &ObjectV0,
    before: &ObjectV0,
    state: &ObjectV0,
    token: &str,
) -> bool {
    if observed.reference != before.reference || observed.metadata.uid != before.metadata.uid {
        return false;
    }
    let Some(expected) = bootstrap_root_metadata_expected_value(before, state, token) else {
        return false;
    };

    bootstrap_response_projection(&observed.value) == bootstrap_response_projection(&expected)
}

/// Builds the complete raw object shape intended by the bootstrap metadata
/// JSON patch. Keeping this shared with the request construction prevents a
/// response-loss proof from recognizing a broader transform than the write
/// that actually became ambiguous.
fn bootstrap_root_metadata_expected_value(
    before: &ObjectV0,
    state: &ObjectV0,
    token: &str,
) -> Option<Value> {
    let mut expected = before.value.clone();
    let metadata = expected
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)?;

    let mut annotations = before.metadata.annotations.clone();
    annotations.insert(ROOT_UID_ANNOTATION.into(), before.metadata.uid.clone());
    annotations.insert(TRANSACTION_ID_ANNOTATION.into(), token.into());
    annotations.insert(COMMAND_ID_ANNOTATION.into(), token.into());
    annotations.insert(
        BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION.into(),
        BOOTSTRAP_AUTHORITY_VERSION_V1.into(),
    );
    annotations.insert(
        BOOTSTRAP_STATE_NAME_ANNOTATION.into(),
        state.reference.name.clone(),
    );
    annotations.insert(
        BOOTSTRAP_STATE_UID_ANNOTATION.into(),
        state.metadata.uid.clone(),
    );
    annotations.insert(
        BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION.into(),
        bootstrap_state_spec_digest(state).ok()?,
    );
    annotations.insert(BOOTSTRAP_PROOF_TOKEN_ANNOTATION.into(), token.into());
    let annotations = serde_json::to_value(annotations).ok()?;
    metadata.insert("annotations".into(), annotations);

    let mut finalizers = before
        .value
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !finalizers
        .iter()
        .any(|finalizer| finalizer.as_str() == Some(ROOT_FINALIZER))
    {
        finalizers.push(Value::String(ROOT_FINALIZER.into()));
    }
    metadata.insert("finalizers".into(), Value::Array(finalizers));

    Some(expected)
}

fn bootstrap_state_spec_digest(state: &ObjectV0) -> Result<String, TransportErrorV0> {
    let spec = state
        .value
        .get("spec")
        .ok_or(TransportErrorV0::ManualRecoveryHold)?;
    let canonical = canonical_bytes(spec).map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
    let mut bytes = Vec::with_capacity(BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1.len() + canonical.len());
    bytes.extend_from_slice(BOOTSTRAP_SPEC_DIGEST_DOMAIN_V1);
    bytes.extend_from_slice(&canonical);
    Ok(sha256_hex(&bytes))
}

fn bootstrap_root_link_proof(observed: &ObjectV0, before: &ObjectV0, state: &ObjectV0) -> bool {
    if observed.reference != before.reference || observed.metadata.uid != before.metadata.uid {
        return false;
    }
    let mut expected = bootstrap_response_projection(&before.value);
    let Some(root) = expected.as_object_mut() else {
        return false;
    };
    let status = root
        .entry("status")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(status) = status.as_object_mut() else {
        return false;
    };
    status.insert(
        "state".into(),
        serde_json::to_value(StateLink {
            root_uid: before.metadata.uid.clone(),
            name: state.reference.name.clone(),
            uid: state.metadata.uid.clone(),
        })
        .expect("StateLink serialization is infallible"),
    );
    has_state_link(observed, state) && bootstrap_response_projection(&observed.value) == expected
}
fn reference_from_identity(identity: &ObjectIdentity) -> Result<ObjectRefV0, TransportErrorV0> {
    let resource = match (identity.api_version.as_str(), identity.kind.as_str()) {
        ("v1", "ConfigMap") => ResourceKindV0::ConfigMap,
        ("apps/v1", "Deployment") => ResourceKindV0::Deployment,
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
    let mut value = body.clone();
    {
        let metadata = value
            .pointer_mut("/metadata")
            .and_then(Value::as_object_mut)
            .ok_or(TransportErrorV0::MetadataSmuggling)?;
        // The model's conditional-apply envelope carries its observed UID/RV
        // as an authority claim. Transport checks those claims against the
        // fresh target before this point; neither field is desired state.
        metadata.remove("uid");
        metadata.remove("resourceVersion");
    }
    validate_desired_object(&value, reference)?;
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

#[cfg(test)]
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
    match expected.resource {
        ResourceKindV0::Service if !is_supported_service_v0(object) => {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        ResourceKindV0::Deployment if !is_supported_deployment_v1(object) => {
            return Err(TransportErrorV0::UnsupportedTarget);
        }
        _ => {}
    }
    Ok(())
}

fn only_keys(map: &serde_json::Map<String, Value>, allowed: &[&str]) -> bool {
    map.keys().all(|key| allowed.contains(&key.as_str()))
}

fn nonempty_string_map(value: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    let map = value?.as_object()?;
    if map.is_empty()
        || map
            .iter()
            .any(|(key, value)| key.is_empty() || value.as_str().is_none_or(str::is_empty))
    {
        None
    } else {
        Some(map)
    }
}

/// Admission for precisely the Deployment shape emitted by controller.tsx/v1.
/// It deliberately rejects the broader Kubernetes Deployment surface: new
/// fields must first gain an explicit compiler and projection contract.
fn is_supported_deployment_v1(object: &Value) -> bool {
    let Some(root) = object.as_object() else {
        return false;
    };
    if !only_keys(root, &["apiVersion", "kind", "metadata", "spec"]) {
        return false;
    }
    let Some(metadata) = root.get("metadata").and_then(Value::as_object) else {
        return false;
    };
    let Some(labels) = nonempty_string_map(metadata.get("labels")) else {
        return false;
    };
    let Some(spec) = root.get("spec").and_then(Value::as_object) else {
        return false;
    };
    if !only_keys(spec, &["replicas", "selector", "template"])
        || spec
            .get("replicas")
            .is_some_and(|value| value.as_u64().is_none())
    {
        return false;
    }
    let Some(selector) = spec.get("selector").and_then(Value::as_object) else {
        return false;
    };
    let Some(match_labels) = selector.get("matchLabels").and_then(Value::as_object) else {
        return false;
    };
    if !only_keys(selector, &["matchLabels"]) || match_labels != labels {
        return false;
    }
    let Some(template) = spec.get("template").and_then(Value::as_object) else {
        return false;
    };
    let Some(template_metadata) = template.get("metadata").and_then(Value::as_object) else {
        return false;
    };
    let Some(template_labels) = template_metadata.get("labels").and_then(Value::as_object) else {
        return false;
    };
    if !only_keys(template, &["metadata", "spec"])
        || !only_keys(template_metadata, &["labels"])
        || template_labels != labels
    {
        return false;
    }
    let Some(pod) = template.get("spec").and_then(Value::as_object) else {
        return false;
    };
    if !only_keys(pod, &["securityContext", "containers"]) {
        return false;
    }
    if let Some(security) = pod.get("securityContext") {
        let Some(security) = security.as_object() else {
            return false;
        };
        if !only_keys(security, &["runAsNonRoot"])
            || security
                .get("runAsNonRoot")
                .and_then(Value::as_bool)
                .is_none()
        {
            return false;
        }
    }
    let Some(containers) = pod.get("containers").and_then(Value::as_array) else {
        return false;
    };
    if containers.is_empty() {
        return false;
    }
    let mut names = BTreeSet::new();
    containers.iter().all(|container| {
        let Some(container) = container.as_object() else {
            return false;
        };
        if !only_keys(container, &["name", "image", "ports", "env", "resources"])
            || container
                .get("name")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || container
                .get("image")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || !names.insert(
                container
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        {
            return false;
        }
        if let Some(ports) = container.get("ports") {
            let Some(ports) = ports.as_array() else {
                return false;
            };
            let mut port_names = BTreeSet::new();
            if ports.is_empty()
                || !ports.iter().all(|port| {
                    let Some(port) = port.as_object() else {
                        return false;
                    };
                    only_keys(port, &["name", "containerPort"])
                        && port
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.is_empty() && port_names.insert(name))
                        && port
                            .get("containerPort")
                            .and_then(Value::as_u64)
                            .is_some_and(|number| (1..=65_535).contains(&number))
                })
            {
                return false;
            }
        }
        if let Some(environment) = container.get("env") {
            let Some(environment) = environment.as_array() else {
                return false;
            };
            let mut environment_names = BTreeSet::new();
            if environment.is_empty()
                || !environment.iter().all(|entry| {
                    let Some(entry) = entry.as_object() else {
                        return false;
                    };
                    let reference = entry
                        .get("valueFrom")
                        .and_then(|value| value.get("configMapKeyRef"))
                        .and_then(Value::as_object);
                    only_keys(entry, &["name", "valueFrom"])
                        && entry
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.is_empty() && environment_names.insert(name))
                        && reference.is_some_and(|reference| {
                            only_keys(reference, &["name", "key"])
                                && reference
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .is_some_and(|name| !name.is_empty())
                                && reference
                                    .get("key")
                                    .and_then(Value::as_str)
                                    .is_some_and(|key| !key.is_empty())
                        })
                })
            {
                return false;
            }
        }
        if let Some(resources) = container.get("resources") {
            let Some(resources) = resources.as_object() else {
                return false;
            };
            if !only_keys(resources, &["requests", "limits"])
                || resources.is_empty()
                || !resources.values().all(|requirements| {
                    nonempty_string_map(Some(requirements)).is_some_and(|requirements| {
                        requirements
                            .keys()
                            .all(|key| matches!(key.as_str(), "cpu" | "memory"))
                    })
                })
            {
                return false;
            }
        }
        true
    })
}

/// Conditional-apply inputs may carry only the observed UID/RV supplied by
/// the sealed decision. They are checked again against the fresh target at
/// the force gate and removed from the desired apply document before writing.
fn validate_conditional_apply_body(
    object: &Value,
    expected: &ObjectRefV0,
    target: &oxideloop_mutation_model::ObjectIdentity,
) -> Result<(), TransportErrorV0> {
    let metadata = object
        .pointer("/metadata")
        .and_then(Value::as_object)
        .ok_or(TransportErrorV0::MetadataSmuggling)?;
    if metadata.get("uid").and_then(Value::as_str) != Some(target.uid.as_str())
        || metadata.get("resourceVersion").and_then(Value::as_str)
            != Some(target.resource_version.as_str())
    {
        return Err(TransportErrorV0::ManualRecoveryHold);
    }
    let mut sanitized = object.clone();
    let metadata = sanitized
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)
        .ok_or(TransportErrorV0::MetadataSmuggling)?;
    metadata.remove("uid");
    metadata.remove("resourceVersion");
    validate_desired_object(&sanitized, expected)
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
/// Compares only controller-owned desired fields after callers have separately
/// authenticated the exact controller owner and root-UID annotation. Runtime
/// annotations are write/recovery plumbing, not renderer semantics.
#[must_use]
pub fn same_desired_semantics(observed: &Value, desired: &Value) -> bool {
    let Some(fields) = controller_apply_fields(observed) else {
        // An object without a valid controller SSA fieldset is not a safe
        // baseline: force the first SSA after POST or a watch/status echo.
        return false;
    };
    // Evaluate omission against the raw API objects.  Canonicalizing defaults
    // before this step would turn a previously controller-owned default into
    // an invisible omission, leaving the manager unable to relinquish it.
    if owned_field_omitted(&fields, observed, desired, "")
        || owned_atomic_object_diff(&fields, observed, desired, "")
    {
        return false;
    }
    // Coverage is measured against the authored object, rather than its
    // default-expanded projection. Kubernetes owns many defaulted fields;
    // requiring our fieldset to cover those would make an unchanged Service
    // or Deployment apply forever after the first successful SSA.
    if !fields_cover_desired(&fields, desired, "") {
        return false;
    }
    let (observed, desired) = canonical_semantic_pair(observed, desired);
    desired_is_subset(&observed, &desired, "")
}

/// Returns the union of every applicable controller Apply fieldset.  A
/// controller entry that looks applicable but is malformed, has the wrong
/// version, or targets a subresource poisons the proof instead of allowing a
/// convenient sibling entry to mint authority.
fn controller_apply_fields(observed: &Value) -> Option<serde_json::Map<String, Value>> {
    let api_version = observed.get("apiVersion")?.as_str()?;
    let entries = observed.pointer("/metadata/managedFields")?.as_array()?;
    let mut result = serde_json::Map::new();
    let mut found = false;
    for entry in entries {
        let entry = entry.as_object()?;
        if entry.get("manager")?.as_str() != Some(CONTROLLER_FIELD_MANAGER)
            || entry.get("operation")?.as_str() != Some("Apply")
        {
            continue;
        }
        found = true;
        if entry.get("fieldsType")?.as_str() != Some("FieldsV1")
            || entry.get("apiVersion")?.as_str() != Some(api_version)
            || !entry
                .get("subresource")
                .is_none_or(|value| value.is_null() || value.as_str() == Some(""))
        {
            return None;
        }
        let fields = entry.get("fieldsV1")?.as_object()?;
        if !valid_fields_v1(fields) {
            return None;
        }
        merge_fields_v1(&mut result, fields);
    }
    found.then_some(result)
}

fn merge_fields_v1(
    into: &mut serde_json::Map<String, Value>,
    from: &serde_json::Map<String, Value>,
) {
    for (key, value) in from {
        match (into.get_mut(key), value.as_object()) {
            (Some(existing), Some(child)) if existing.as_object().is_some() => {
                merge_fields_v1(existing.as_object_mut().expect("checked object"), child);
            }
            _ => {
                into.insert(key.clone(), value.clone());
            }
        }
    }
}

fn valid_fields_v1(fields: &serde_json::Map<String, Value>) -> bool {
    fields.iter().all(|(key, value)| {
        (key == "."
            || key.strip_prefix("f:").is_some()
            || key.strip_prefix("k:").is_some_and(|selector| {
                serde_json::from_str::<Value>(selector)
                    .ok()
                    .and_then(|value| value.as_object().map(|_| ()))
                    .is_some()
            }))
            && value.as_object().is_some_and(valid_fields_v1)
    })
}

fn canonical_semantic_pair(observed: &Value, desired: &Value) -> (Value, Value) {
    let mut observed = observed.clone();
    let mut desired = desired.clone();
    for value in [&mut observed, &mut desired] {
        if let Some(object) = value.as_object_mut() {
            object.remove("status");
        }
        if let Some(metadata) = value
            .pointer_mut("/metadata")
            .and_then(Value::as_object_mut)
        {
            for field in [
                "uid",
                "resourceVersion",
                "generation",
                "creationTimestamp",
                "deletionTimestamp",
                "managedFields",
                "ownerReferences",
                "selfLink",
                "finalizers",
            ] {
                metadata.remove(field);
            }
            if let Some(annotations) = metadata
                .get_mut("annotations")
                .and_then(Value::as_object_mut)
            {
                annotations.remove(ROOT_UID_ANNOTATION);
                annotations.remove(TRANSACTION_ID_ANNOTATION);
                annotations.remove(COMMAND_ID_ANNOTATION);
                annotations.remove("deployment.kubernetes.io/revision");
            }
        }
    }
    let kind = observed.get("kind").and_then(Value::as_str);
    if kind == Some("Deployment") {
        if let Some(spec) = canonical_deployment_spec(&observed, desired.get("spec")) {
            observed["spec"] = spec;
        }
        if let Some(spec) = canonical_deployment_spec(&desired, None) {
            desired["spec"] = spec;
        }
        normalize_deployment_lists(&mut observed);
        normalize_deployment_lists(&mut desired);
    } else if kind == Some("Service") {
        if let Some(spec) = canonical_service_spec(&observed, desired.get("spec")) {
            observed["spec"] = spec;
        }
        if let Some(spec) = canonical_service_spec(&desired, None) {
            desired["spec"] = spec;
        }
        normalize_service_ports(&mut observed);
        normalize_service_ports(&mut desired);
    }
    (observed, desired)
}

fn desired_is_subset(observed: &Value, desired: &Value, path: &str) -> bool {
    match (observed, desired) {
        (_, Value::Null) => observed.is_null(),
        (Value::Object(observed), Value::Object(desired)) => {
            desired.iter().all(|(key, desired)| {
                observed
                    .get(key)
                    .is_some_and(|actual| desired_is_subset(actual, desired, &join_path(path, key)))
            })
        }
        (Value::Array(observed), Value::Array(desired)) => {
            let item_path = format!("{path}[]");
            match associative_keys(path) {
                Some(keys) => desired.iter().all(|wanted| {
                    observed
                        .iter()
                        .find(|actual| keyed_match(actual, wanted, keys))
                        .is_some_and(|actual| desired_is_subset(actual, wanted, &item_path))
                }),
                None => observed == desired,
            }
        }
        _ => observed == desired,
    }
}

fn owned_field_omitted(
    fields: &serde_json::Map<String, Value>,
    observed: &Value,
    desired: &Value,
    path: &str,
) -> bool {
    fields.iter().any(|(key, child)| {
        if key == "." {
            return false;
        }
        if let Some(field) = key.strip_prefix("f:") {
            // Metadata identity and the transport's runtime annotations are
            // not renderer semantics, even if older SSA entries claim them.
            let field_path = join_path(path, field);
            if ignored_semantic_path(&field_path) {
                return false;
            }
            let actual = observed.get(field);
            let wanted = desired.get(field);
            match (actual, wanted) {
                (Some(actual), None) => !actual.is_null(),
                (Some(actual), Some(wanted)) => child
                    .as_object()
                    .is_none_or(|child| owned_field_omitted(child, actual, wanted, &field_path)),
                _ => false,
            }
        } else if let Some(selector) = key.strip_prefix("k:") {
            let Ok(selector) = serde_json::from_str::<Value>(selector) else {
                return true;
            };
            let Some(selector) = selector.as_object() else {
                return true;
            };
            let Some(actual) = observed
                .as_array()
                .and_then(|items| items.iter().find(|item| selector_match(item, selector)))
            else {
                return false;
            };
            let wanted = desired
                .as_array()
                .and_then(|items| items.iter().find(|item| selector_match(item, selector)));
            wanted.is_none()
                || child.as_object().is_none_or(|child| {
                    owned_field_omitted(child, actual, wanted.unwrap(), &format!("{path}[]"))
                })
        } else {
            true
        }
    })
}

fn fields_cover_desired(
    fields: &serde_json::Map<String, Value>,
    desired: &Value,
    path: &str,
) -> bool {
    let Some(desired) = desired.as_object() else {
        return true;
    };
    desired
        .iter()
        .filter(|(key, value)| {
            !unmanaged_declared_field(path, key, value)
                && !ignored_semantic_path(&join_path(path, key))
        })
        .all(|(key, value)| {
            let Some(field) = fields.get(&format!("f:{key}")) else {
                return false;
            };
            let Some(children) = field.as_object() else {
                return false;
            };
            match value {
                Value::Object(_) | Value::Array(_) => {
                    fields_cover_value(children, value, &join_path(path, key))
                }
                _ => true,
            }
        })
}

fn unmanaged_identity_field(path: &str, field: &str) -> bool {
    matches!(field, "apiVersion" | "kind" | "status")
        || (path == "/metadata" && matches!(field, "name" | "namespace"))
}

fn unmanaged_declared_field(path: &str, field: &str, value: &Value) -> bool {
    unmanaged_identity_field(path, field)
        || (path.is_empty()
            && field == "metadata"
            && value.as_object().is_some_and(|metadata| {
                metadata
                    .keys()
                    .all(|key| matches!(key.as_str(), "name" | "namespace"))
            }))
}

fn fields_cover_value(
    fields: &serde_json::Map<String, Value>,
    desired: &Value,
    path: &str,
) -> bool {
    match desired {
        // An empty map is FieldsV1's atomic-object ownership form (for
        // example Deployment.spec.selector). `owned_atomic_object_diff`
        // supplies the required exact comparison for that case.
        Value::Object(_) if fields.is_empty() => true,
        Value::Object(_) => fields_cover_desired(fields, desired, path),
        Value::Array(items) => match associative_keys(path) {
            Some(_) => items.iter().all(|item| {
                fields
                    .iter()
                    .filter_map(|(key, value)| key.strip_prefix("k:").zip(value.as_object()))
                    .any(|(selector, child)| {
                        serde_json::from_str::<Value>(selector)
                            .ok()
                            .and_then(|v| v.as_object().cloned())
                            .is_some_and(|selector| {
                                selector_match(item, &selector)
                                    && fields_cover_value(child, item, &format!("{path}[]"))
                            })
                    })
            }),
            None => fields.is_empty() || fields.contains_key("."),
        },
        _ => true,
    }
}

/// `{}` in `FieldsV1` owns an atomic value, not every descendant of a map or
/// associative-list member. For an atomic object we must compare the whole
/// value so an owned selector/member cannot silently retain removed fields.
fn owned_atomic_object_diff(
    fields: &serde_json::Map<String, Value>,
    observed: &Value,
    desired: &Value,
    path: &str,
) -> bool {
    fields.iter().any(|(key, child)| {
        if key == "." {
            return false;
        }
        if let Some(field) = key.strip_prefix("f:") {
            let field_path = join_path(path, field);
            if ignored_semantic_path(&field_path) {
                return false;
            }
            let actual = observed.get(field);
            let wanted = desired.get(field);
            let Some(child) = child.as_object() else { return true; };
            if child.is_empty() {
                return matches!((actual, wanted), (Some(Value::Object(actual)), Some(Value::Object(wanted))) if actual != wanted);
            }
            match (actual, wanted) {
                (Some(actual), Some(wanted)) => owned_atomic_object_diff(child, actual, wanted, &field_path),
                _ => false,
            }
        } else if let Some(selector) = key.strip_prefix("k:") {
            let Ok(selector) = serde_json::from_str::<Value>(selector) else { return true; };
            let Some(selector) = selector.as_object() else { return true; };
            let Some(actual) = observed.as_array().and_then(|items| items.iter().find(|item| selector_match(item, selector))) else { return false; };
            let Some(wanted) = desired.as_array().and_then(|items| items.iter().find(|item| selector_match(item, selector))) else { return false; };
            child.as_object().is_none_or(|child| owned_atomic_object_diff(child, actual, wanted, &format!("{path}[]")))
        } else {
            true
        }
    })
}

fn join_path(path: &str, field: &str) -> String {
    format!("{path}/{field}")
}

fn associative_keys(path: &str) -> Option<&'static [&'static str]> {
    match path {
        "/spec/template/spec/containers" | "/spec/template/spec/containers[]/env" => {
            Some(&["name"])
        }
        "/spec/template/spec/containers[]/ports" => Some(&["containerPort", "protocol"]),
        "/spec/ports" => Some(&["port", "protocol"]),
        _ => None,
    }
}

fn selector_match(item: &Value, selector: &serde_json::Map<String, Value>) -> bool {
    selector
        .iter()
        .all(|(key, value)| semantic_field_eq(item.get(key), Some(value), key))
}

fn keyed_match(actual: &Value, desired: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .all(|key| semantic_field_eq(actual.get(key), desired.get(key), key))
}

fn semantic_field_eq(actual: Option<&Value>, desired: Option<&Value>, key: &str) -> bool {
    actual == desired
        || (key == "protocol"
            && protocol_is_tcp_or_missing(actual)
            && protocol_is_tcp_or_missing(desired))
}

fn protocol_is_tcp_or_missing(value: Option<&Value>) -> bool {
    value.is_none() || value.and_then(Value::as_str) == Some("TCP")
}

fn ignored_semantic_path(path: &str) -> bool {
    matches!(
        path,
        "/status"
            | "/metadata/uid"
            | "/metadata/resourceVersion"
            | "/metadata/generation"
            | "/metadata/creationTimestamp"
            | "/metadata/deletionTimestamp"
            | "/metadata/managedFields"
            | "/metadata/ownerReferences"
            | "/metadata/selfLink"
            | "/metadata/finalizers"
            | "/metadata/annotations/runtime.oxideloop.dev/root-uid"
            | "/metadata/annotations/runtime.oxideloop.dev/transaction-id"
            | "/metadata/annotations/runtime.oxideloop.dev/command-id"
            | "/metadata/annotations/deployment.kubernetes.io/revision"
    )
}

fn normalize_deployment_lists(value: &mut Value) {
    let Some(containers) = value
        .pointer_mut("/spec/template/spec/containers")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for container in containers.iter_mut() {
        if let Some(ports) = container.get_mut("ports").and_then(Value::as_array_mut) {
            ports.sort_by_key(|port| {
                (
                    port.get("containerPort").and_then(Value::as_i64),
                    port.get("protocol")
                        .and_then(Value::as_str)
                        .unwrap_or("TCP")
                        .to_owned(),
                )
            });
        }
        if let Some(env) = container.get_mut("env").and_then(Value::as_array_mut) {
            env.sort_by_key(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            });
        }
    }
    containers.sort_by_key(|container| {
        container
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    });
}

fn normalize_service_ports(value: &mut Value) {
    if let Some(ports) = value
        .pointer_mut("/spec/ports")
        .and_then(Value::as_array_mut)
    {
        ports.sort_by_key(|port| {
            (
                port.get("port").and_then(Value::as_i64),
                port.get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("TCP")
                    .to_owned(),
            )
        });
    }
}

#[allow(dead_code)]
fn exact_deployment_metadata(observed: &Value, desired: &Value) -> bool {
    let mut observed_annotations = observed
        .pointer("/metadata/annotations")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(annotations) = observed_annotations.as_object_mut() else {
        return false;
    };
    annotations.remove("deployment.kubernetes.io/revision");
    annotations.remove(ROOT_UID_ANNOTATION);
    annotations.remove(TRANSACTION_ID_ANNOTATION);
    annotations.remove(COMMAND_ID_ANNOTATION);
    let mut desired_annotations = desired
        .pointer("/metadata/annotations")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(annotations) = desired_annotations.as_object_mut() {
        annotations.remove(ROOT_UID_ANNOTATION);
        annotations.remove(TRANSACTION_ID_ANNOTATION);
        annotations.remove(COMMAND_ID_ANNOTATION);
    }
    let labels = |value: &Value| {
        value
            .pointer("/metadata/labels")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}))
    };
    observed_annotations == desired_annotations && labels(observed) == labels(desired)
}

// Kubernetes allocates only these Service fields. Every other field remains
// exact so response-loss recovery cannot silently adopt a caller mismatch.
#[allow(dead_code)]
fn same_service_semantics(observed: &Value, desired: &Value) -> bool {
    if observed.get("kind").and_then(Value::as_str) != Some("Service") {
        return true;
    }
    let Some(desired) = canonical_service_spec(desired, None) else {
        return observed.get("spec").is_none();
    };
    canonical_service_spec(observed, Some(&desired)) == Some(desired)
}

#[allow(dead_code)]
fn same_deployment_semantics(observed: &Value, desired: &Value) -> bool {
    let Some(desired) = canonical_deployment_spec(desired, None) else {
        return observed.get("spec").is_none();
    };
    canonical_deployment_spec(observed, Some(&desired)) == Some(desired)
}

/// Removes only Kubernetes' documented default fields from an observed v1
/// Deployment. DSL-owned fields remain exact; the projection is therefore
/// suitable for both a harmless fresh-RV acknowledgement and response-loss
/// recovery, never for adoption of a different Deployment.
fn canonical_deployment_spec(value: &Value, desired: Option<&Value>) -> Option<Value> {
    let mut spec = value.get("spec")?.clone();
    let fields = spec.as_object_mut()?;
    if desired.is_none() {
        fields
            .entry("replicas")
            .or_insert_with(|| serde_json::json!(1));
    } else {
        let desired = desired?.as_object()?;
        for (field, default) in [
            ("progressDeadlineSeconds", serde_json::json!(600)),
            ("revisionHistoryLimit", serde_json::json!(10)),
            (
                "strategy",
                serde_json::json!({"type":"RollingUpdate","rollingUpdate":{"maxSurge":"25%","maxUnavailable":"25%"}}),
            ),
        ] {
            if !desired.contains_key(field) && fields.get(field) == Some(&default) {
                fields.remove(field);
            }
        }
    }
    let desired_template = desired.and_then(|desired| desired.pointer("/template/spec"));
    let pod = fields
        .get_mut("template")?
        .get_mut("spec")?
        .as_object_mut()?;
    for (field, default) in [
        ("dnsPolicy", serde_json::json!("ClusterFirst")),
        ("restartPolicy", serde_json::json!("Always")),
        ("schedulerName", serde_json::json!("default-scheduler")),
        ("terminationGracePeriodSeconds", serde_json::json!(30)),
        ("enableServiceLinks", serde_json::json!(true)),
    ] {
        if desired_template.is_none_or(|desired| desired.get(field).is_none())
            && pod.get(field) == Some(&default)
        {
            pod.remove(field);
        }
    }
    let desired_containers = desired_template
        .and_then(|template| template.get("containers"))
        .and_then(Value::as_array);
    if let Some(containers) = pod.get_mut("containers").and_then(Value::as_array_mut) {
        for (index, container) in containers.iter_mut().enumerate() {
            let container = container.as_object_mut()?;
            let desired_container = desired_containers.and_then(|items| items.get(index));
            for (field, default) in [
                (
                    "terminationMessagePath",
                    serde_json::json!("/dev/termination-log"),
                ),
                ("terminationMessagePolicy", serde_json::json!("File")),
            ] {
                if desired_container.is_none_or(|desired| desired.get(field).is_none())
                    && container.get(field) == Some(&default)
                {
                    container.remove(field);
                }
            }
            if desired_container.is_none_or(|desired| desired.get("imagePullPolicy").is_none()) {
                container.remove("imagePullPolicy");
            }
            let desired_ports = desired_container
                .and_then(|container| container.get("ports"))
                .and_then(Value::as_array);
            if let Some(ports) = container.get_mut("ports").and_then(Value::as_array_mut) {
                for (port_index, port) in ports.iter_mut().enumerate() {
                    let port = port.as_object_mut()?;
                    if desired_ports
                        .and_then(|ports| ports.get(port_index))
                        .is_none_or(|desired| desired.get("protocol").is_none())
                        && port.get("protocol") == Some(&serde_json::json!("TCP"))
                    {
                        port.remove("protocol");
                    }
                }
            }
        }
    }
    Some(spec)
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

#[allow(dead_code)]
fn exact_metadata(observed: &Value, desired: &Value) -> bool {
    let map = |value: &Value, field: &str| {
        value
            .pointer(&format!("/metadata/{field}"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}))
    };
    let strip_runtime_annotations = |mut annotations: Value| {
        if let Some(annotations) = annotations.as_object_mut() {
            annotations.remove(ROOT_UID_ANNOTATION);
            annotations.remove(TRANSACTION_ID_ANNOTATION);
            annotations.remove(COMMAND_ID_ANNOTATION);
        }
        annotations
    };
    map(observed, "labels") == map(desired, "labels")
        && strip_runtime_annotations(map(observed, "annotations"))
            == strip_runtime_annotations(map(desired, "annotations"))
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
            | CommandV0::DeleteRetiredState { .. }
            | CommandV0::RemoveRootFinalizer { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response};
    use kube::{Client, client::Body};
    use serde_json::json;
    use std::convert::Infallible;
    use std::sync::atomic::AtomicUsize;
    use tower::service_fn;

    /// Produces a captured-style `FieldsV1` shape for the sealed test object:
    /// maps recurse by `f:` fields and supported associative lists use `k:`
    /// members plus `.` membership. Identity is intentionally absent because
    /// Kubernetes does not normally attribute it to an Apply manager.
    fn fields_v1_for(value: &Value, path: &str) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| !unmanaged_identity_field(path, key))
                    .filter_map(|(key, value)| {
                        let next = join_path(path, key);
                        let child = match value {
                            Value::Object(_) | Value::Array(_) => fields_v1_for(value, &next),
                            _ => json!({}),
                        };
                        if path.is_empty() && key == "metadata" && child == json!({}) {
                            None
                        } else {
                            Some((format!("f:{key}"), child))
                        }
                    })
                    .collect(),
            ),
            Value::Array(items) => match associative_keys(path) {
                Some(keys) => Value::Object(
                    items
                        .iter()
                        .filter_map(|item| {
                            let item = item.as_object()?;
                            let selector =
                                keys.iter()
                                    .try_fold(serde_json::Map::new(), |mut out, key| {
                                        let value = item.get(*key).cloned().or_else(|| {
                                            (*key == "protocol").then(|| json!("TCP"))
                                        })?;
                                        out.insert((*key).into(), value);
                                        Some(out)
                                    })?;
                            let mut child =
                                fields_v1_for(&Value::Object(item.clone()), &format!("{path}[]"));
                            child.as_object_mut()?.insert(".".into(), json!({}));
                            Some((format!("k:{}", Value::Object(selector)), child))
                        })
                        .collect(),
                ),
                None => json!({}),
            },
            _ => json!({}),
        }
    }

    struct FakePort(Vec<ObjectV0>);

    fn test_pump(
        fence: &Arc<Mutex<BootstrapFenceStateV0>>,
        gate: &PrewriteGateV0,
        wake: &watch::Sender<u64>,
    ) -> Arc<BootstrapPumpRuntimeV0> {
        Arc::new(BootstrapPumpRuntimeV0::new(
            Arc::clone(fence),
            gate.clone(),
            wake.clone(),
        ))
    }

    fn test_session(
        attempt_cutoff: Duration,
        prewrite: Duration,
        response: Duration,
        outer: Duration,
        retries: u8,
    ) -> OperationSessionV0 {
        let (bootstrap_fence_wake, _bootstrap_fence_observer) = watch::channel(0_u64);
        let gate = PrewriteGateV0::new();
        let bootstrap_fence = Arc::new(Mutex::new(BootstrapFenceStateV0::new()));
        let bootstrap_pump = test_pump(&bootstrap_fence, &gate, &bootstrap_fence_wake);
        OperationSessionV0 {
            budget: OperationBudgetV0::test_shortened(
                attempt_cutoff,
                prewrite,
                response,
                outer,
                retries,
            )
            .expect("ordered test session"),
            gate,
            bootstrap_fence,
            bootstrap_fence_wake,
            bootstrap_pump,
            bootstrap: Arc::new(Mutex::new(BootstrapAuthorityV0::new())),
            collector_bound: false,
            bootstrap_phase_wait_hook: None,
        }
    }

    fn test_bootstrap_command() -> CommandV0 {
        CommandV0::EnsureBootstrap {
            token: "bootstrap-token".into(),
            state: serde_json::from_value(json!({
                "stateVersion": 1,
                "root": {
                    "uid": "root-uid",
                    "bootstrapId": "test-bootstrap-id",
                    "bundleDigest": "0000000000000000000000000000000000000000000000000000000000000000"
                },
                "state": {}
            }))
            .expect("test bootstrap command state deserializes"),
            root_uid: "root-uid".into(),
            root_generation: 1,
            root_resource_version: "1".into(),
            state_name: "state".into(),
        }
    }

    fn bootstrap_authority_root(resource_version: &str) -> ObjectV0 {
        let annotations = BTreeMap::from([
            (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
            (TRANSACTION_ID_ANNOTATION.into(), "bootstrap-token".into()),
            (COMMAND_ID_ANNOTATION.into(), "bootstrap-token".into()),
        ]);
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "app".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "root-uid".into(),
                resource_version: resource_version.into(),
                annotations: annotations.clone(),
                owner_uids: vec![],
            },
            value: json!({
                "apiVersion":"demo.oxideloop.dev/v1alpha1",
                "kind":"WebApplication",
                "metadata":{
                    "name":"app", "namespace":"ns", "uid":"root-uid",
                    "resourceVersion":resource_version, "generation":1,
                    "annotations":annotations, "finalizers":[ROOT_FINALIZER],
                },
                "spec":{
                    "image":"example.invalid/web:latest", "replicas":1,
                    "message":"bootstrap authority", "expose":false,
                    "program":{"configMapName":"bundle", "key":"program.mjs", "digest":"0000000000000000000000000000000000000000000000000000000000000000"}
                },
                "status":{},
            }),
        }
    }

    fn arm_test_collected_bootstrap(session: &mut OperationSessionV0, root: &ObjectV0) {
        session.collector_bound = true;
        let address = RootAddressV0 {
            namespace: "ns".into(),
            name: "app".into(),
            state_name: "state".into(),
        };
        let command = test_bootstrap_command();
        session
            .seal_collected_bootstrap_fence(&address, &command, root, None)
            .expect("collector seals an exact bootstrap fence");
        session
            .arm_collected_bootstrap_baseline(&address, &command, root, None)
            .expect("test root arms the exact raw baseline");
    }

    fn sealed_test_collected_bootstrap() -> (OperationSessionV0, RootAddressV0, CommandV0, ObjectV0)
    {
        let mut session = test_session(
            Duration::from_millis(10),
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(500),
            3,
        );
        session.collector_bound = true;
        let address = RootAddressV0 {
            namespace: "ns".into(),
            name: "app".into(),
            state_name: "state".into(),
        };
        let command = test_bootstrap_command();
        let root = bootstrap_authority_root("1");
        session
            .seal_collected_bootstrap_fence(&address, &command, &root, None)
            .expect("collector seals its exact test bootstrap");
        (session, address, command, root)
    }

    fn bootstrap_test_state() -> ObjectV0 {
        ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::OxideLoopState,
                namespace: "ns".into(),
                name: "state".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "state-uid".into(),
                resource_version: "state-rv".into(),
                annotations: BTreeMap::from([
                    (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
                    (TRANSACTION_ID_ANNOTATION.into(), "bootstrap-token".into()),
                    (COMMAND_ID_ANNOTATION.into(), "bootstrap-token".into()),
                ]),
                owner_uids: vec!["root-uid".into()],
            },
            value: json!({
                "apiVersion":"runtime.oxideloop.dev/v1alpha1",
                "kind":"OxideLoopState",
                "metadata":{
                    "name":"state", "namespace":"ns", "uid":"state-uid",
                    "resourceVersion":"state-rv",
                    "annotations":{
                        ROOT_UID_ANNOTATION:"root-uid",
                        TRANSACTION_ID_ANNOTATION:"bootstrap-token",
                        COMMAND_ID_ANNOTATION:"bootstrap-token"
                    },
                    "ownerReferences":[{
                        "apiVersion":"demo.oxideloop.dev/v1alpha1",
                        "kind":"WebApplication", "name":"app", "uid":"root-uid",
                        "controller":true, "blockOwnerDeletion":false
                    }]
                },
                "spec":{"stateVersion":1}
            }),
        }
    }

    #[cfg(feature = "bootstrap-diagnostics")]
    #[test]
    fn bootstrap_diagnostic_marks_finalizer_preflight_without_serializing_authority() {
        let root = bootstrap_authority_root("root-rv");
        let state = bootstrap_test_state();
        let session = test_session(
            Duration::from_millis(10),
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(500),
            3,
        );

        let diagnostic = bootstrap_diagnostic_value(
            "state_finalizer_preflight",
            &TransportErrorV0::ManualRecoveryHold,
            Some(&root),
            Some(&state),
            &session,
        );

        assert_eq!(
            diagnostic.pointer("/event").and_then(Value::as_str),
            Some("oxideloop_bootstrap_diagnostic_v1")
        );
        assert_eq!(
            diagnostic.pointer("/stage").and_then(Value::as_str),
            Some("state_finalizer_preflight")
        );
        assert_eq!(
            diagnostic.pointer("/root/uid").and_then(Value::as_str),
            Some("root-uid")
        );
        assert_eq!(
            diagnostic
                .pointer("/state/finalizerPresent")
                .and_then(Value::as_bool),
            Some(false)
        );
        assert!(!diagnostic.to_string().contains("bootstrap-token"));
    }

    fn final_proof_fixture(initial_root: &ObjectV0) -> (ObjectV0, ObjectV0) {
        let mut state = bootstrap_test_state();
        state.metadata.resource_version = "state-final-rv".into();
        state.value["metadata"]["resourceVersion"] = json!("state-final-rv");
        state.value["metadata"]["finalizers"] = json!([STATE_FINALIZER]);

        let mut root = initial_root.clone();
        root.metadata.resource_version = "root-final-rv".into();
        root.value =
            bootstrap_root_metadata_expected_value(initial_root, &state, "bootstrap-token")
                .expect("fixture root bundle is representable");
        root.value["metadata"]["resourceVersion"] = json!("root-final-rv");
        root.value["status"]["state"] = json!({
            "rootUid":"root-uid", "name":"state", "uid":"state-uid"
        });
        root.metadata.annotations =
            serde_json::from_value(root.value["metadata"]["annotations"].clone())
                .expect("fixture annotations are strings");
        (root, state)
    }

    fn linked_deletion_fixture() -> (RootAddressV0, ObjectV0, ObjectV0, PreconditionsV0, String) {
        let state_name = oxideloop_controller_api::state_name("root-uid")
            .expect("fixture State name is deterministic");
        let cleanup_token = "c".repeat(64);
        let persist_token = "a".repeat(64);
        let deletion_id = "d".repeat(64);
        let root_annotations = BTreeMap::from([(ROOT_UID_ANNOTATION.into(), "root-uid".into())]);
        let root = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "ns".into(),
                name: "app".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "root-uid".into(),
                resource_version: "root-rv".into(),
                annotations: root_annotations.clone(),
                owner_uids: vec![],
            },
            value: json!({
                "apiVersion":"demo.oxideloop.dev/v1alpha1",
                "kind":"WebApplication",
                "metadata":{
                    "name":"app", "namespace":"ns", "uid":"root-uid",
                    "resourceVersion":"root-rv", "generation":3,
                    "annotations":root_annotations, "deletionTimestamp":"2026-08-31T00:00:00Z",
                    "finalizers":[ROOT_FINALIZER],
                },
                "spec":{"image":"example.invalid/web:latest", "replicas":1, "message":"linked deletion", "expose":false,
                    "program":{"configMapName":"bundle", "key":"program.mjs", "digest":"0".repeat(64)}},
                "status":{
                    "state":{"rootUid":"root-uid", "name":state_name, "uid":"state-uid"},
                    "deletionCleanupReceipt":{
                        "rootUid":"root-uid", "deletionId":deletion_id,
                        "deletionStartGeneration":3, "stateName":state_name,
                        "stateUid":"state-uid", "cleanupToken":cleanup_token,
                    }
                },
            }),
        };
        let state_annotations = BTreeMap::from([
            (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
            (TRANSACTION_ID_ANNOTATION.into(), persist_token.clone()),
            (COMMAND_ID_ANNOTATION.into(), persist_token.clone()),
        ]);
        let state = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::OxideLoopState,
                namespace: "ns".into(),
                name: state_name.clone(),
            },
            metadata: ObjectMetaV0 {
                uid: "state-uid".into(),
                resource_version: "state-rv".into(),
                annotations: state_annotations.clone(),
                owner_uids: vec!["root-uid".into()],
            },
            value: json!({
                "apiVersion":"runtime.oxideloop.dev/v1alpha1", "kind":"OxideLoopState",
                "metadata":{
                    "name":state_name, "namespace":"ns", "uid":"state-uid",
                    "resourceVersion":"state-rv", "annotations":state_annotations,
                    "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1",
                        "kind":"WebApplication", "name":"app", "uid":"root-uid",
                        "controller":true, "blockOwnerDeletion":false}],
                    "finalizers":[STATE_FINALIZER],
                },
                "spec":{"stateVersion":1,
                    "root":{"uid":"root-uid", "bootstrapId":oxideloop_controller_api::bootstrap_id("root-uid").expect("bootstrap id"), "bundleDigest":"0".repeat(64)},
                    "state":{"pendingPersist":{"token":persist_token, "purpose":"DeletionStart", "phase":"DeletionStart", "oldStateResourceVersion":"state-rv-0", "rootGeneration":3},
                        "deletion":{"id":deletion_id, "deletionStartGeneration":3}, "automation":{}}},
                "status":{},
            }),
        };
        let address = RootAddressV0 {
            namespace: "ns".into(),
            name: "app".into(),
            state_name: state.reference.name.clone(),
        };
        let preconditions = PreconditionsV0 {
            root_uid: root.metadata.uid.clone(),
            root_generation: 3,
            root_resource_version: root.metadata.resource_version.clone(),
            state_uid: state.metadata.uid.clone(),
            state_resource_version: state.metadata.resource_version.clone(),
            observation_token: "observation".into(),
            cache_epoch: "epoch".into(),
        };
        let token = linked_state_finalizer_command_token(
            &preconditions.root_uid,
            preconditions.root_generation,
            &preconditions.root_resource_version,
            &address.state_name,
            &preconditions.state_uid,
            &preconditions.state_resource_version,
            &deletion_id,
            3,
            &cleanup_token,
        );
        (address, root, state, preconditions, token)
    }

    fn test_object_response(object: &ObjectV0) -> Response<Body> {
        Response::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&object.value).expect("fixture object serializes"),
            ))
            .expect("fixture response builds")
    }

    fn test_not_found_response() -> Response<Body> {
        Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                br#"{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404}"#
                    .to_vec(),
            ))
            .expect("not-found response builds")
    }

    fn test_list_response(resource_version: &str, items: &[ObjectV0]) -> Response<Body> {
        let values = items
            .iter()
            .map(|object| object.value.clone())
            .collect::<Vec<_>>();
        Response::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplicationList",
                    "metadata":{"resourceVersion":resource_version},
                    "items":values,
                }))
                .expect("fixture list serializes"),
            ))
            .expect("fixture list response builds")
    }

    fn set_test_annotation(object: &mut ObjectV0, key: &str, value: &str) {
        object.metadata.annotations.insert(key.into(), value.into());
        object.value["metadata"]["annotations"][key] = json!(value);
    }

    fn remove_test_annotation(object: &mut ObjectV0, key: &str) {
        object.metadata.annotations.remove(key);
        object
            .value
            .pointer_mut("/metadata/annotations")
            .and_then(Value::as_object_mut)
            .expect("fixture annotations are an object")
            .remove(key);
    }

    fn unlinked_deleting_bootstrap_fixture() -> (ObjectV0, ObjectV0) {
        let initial_root = bootstrap_authority_root("1");
        let (mut root, mut state) = final_proof_fixture(&initial_root);
        let state_name = oxideloop_controller_api::state_name(&root.metadata.uid)
            .expect("fixture root UID has a deterministic State name");
        state.reference.name.clone_from(&state_name);
        state.value["metadata"]["name"] = json!(state_name);
        root.metadata.annotations.insert(
            BOOTSTRAP_STATE_NAME_ANNOTATION.into(),
            state.reference.name.clone(),
        );
        root.value["metadata"]["annotations"][BOOTSTRAP_STATE_NAME_ANNOTATION] =
            json!(state.reference.name);
        root.value["metadata"]["deletionTimestamp"] = json!("2026-08-31T00:00:00Z");
        root.value["status"] = json!({});
        (root, state)
    }

    struct UnlinkedCleanupWireV0 {
        root: Option<ObjectV0>,
        state: Option<ObjectV0>,
        lose_state_finalizer_response: bool,
        lose_root_finalizer_response: bool,
        state_finalizer_patches: usize,
        root_finalizer_patches: usize,
    }

    fn cleanup_transport(wire: Arc<Mutex<UnlinkedCleanupWireV0>>) -> KubeTransportV0 {
        let service = service_fn(move |request: Request<Body>| {
            let wire = Arc::clone(&wire);
            async move {
                let path = request.uri().path();
                if request.method() == http::Method::GET && path.ends_with("/webapplications/app") {
                    let root = wire.lock().expect("cleanup wire is readable").root.clone();
                    return Ok::<_, Infallible>(
                        root.map_or_else(test_not_found_response, |root| {
                            test_object_response(&root)
                        }),
                    );
                }
                if request.method() == http::Method::GET && path.contains("/oxideloopstates/") {
                    let state = wire.lock().expect("cleanup wire is readable").state.clone();
                    return Ok::<_, Infallible>(
                        state.map_or_else(test_not_found_response, |state| {
                            test_object_response(&state)
                        }),
                    );
                }
                if request.method() == http::Method::PATCH && path.contains("/oxideloopstates/") {
                    let (written, lose_response) = {
                        let mut wire = wire.lock().expect("cleanup wire is writable");
                        wire.state_finalizer_patches += 1;
                        let mut written = wire
                            .state
                            .clone()
                            .expect("State patch requires an extant State");
                        written.metadata.resource_version = "state-after-finalizer".into();
                        written.value["metadata"]["resourceVersion"] =
                            json!("state-after-finalizer");
                        written.value["metadata"]["finalizers"] = json!([]);
                        let lose_response = wire.lose_state_finalizer_response;
                        // The deterministic test model makes the controller
                        // owner deletion complete immediately after this
                        // exact finalizer write. The returned response remains
                        // the sole proof of the patched State shape.
                        wire.state = None;
                        (written, lose_response)
                    };
                    if lose_response {
                        return std::future::pending::<Result<Response<Body>, Infallible>>().await;
                    }
                    return Ok(test_object_response(&written));
                }
                if request.method() == http::Method::PATCH && path.ends_with("/webapplications/app")
                {
                    let (written, lose_response) = {
                        let mut wire = wire.lock().expect("cleanup wire is writable");
                        wire.root_finalizer_patches += 1;
                        assert!(
                            wire.state.is_none(),
                            "root finalizer cannot be patched before the old State UID is absent"
                        );
                        let mut written = wire
                            .root
                            .clone()
                            .expect("root patch requires an extant root");
                        written.metadata.resource_version = "root-after-finalizer".into();
                        written.value["metadata"]["resourceVersion"] =
                            json!("root-after-finalizer");
                        written.value["metadata"]["finalizers"] = json!([]);
                        let lose_response = wire.lose_root_finalizer_response;
                        wire.root = Some(written.clone());
                        (written, lose_response)
                    };
                    if lose_response {
                        return std::future::pending::<Result<Response<Body>, Infallible>>().await;
                    }
                    return Ok(test_object_response(&written));
                }
                panic!(
                    "unexpected unlinked-bootstrap cleanup request: {} {path}",
                    request.method()
                );
            }
        });
        KubeTransportV0::test_from_client(Client::new(service, "default"))
    }

    fn final_proof_transport(root: ObjectV0, state: ObjectV0) -> KubeTransportV0 {
        let service = service_fn(move |request: Request<Body>| {
            let root = root.clone();
            let state = state.clone();
            async move {
                assert_eq!(request.method(), http::Method::GET);
                let object = if request.uri().path().ends_with("/oxideloopstates/state") {
                    state
                } else if request.uri().path().ends_with("/webapplications/app") {
                    root
                } else {
                    panic!(
                        "unexpected final-proof fixture request: {}",
                        request.uri().path()
                    );
                };
                Ok::<_, Infallible>(test_object_response(&object))
            }
        });
        KubeTransportV0::test_from_client(Client::new(service, "default"))
    }

    async fn wait_for_bootstrap_first_poll(session: &OperationSessionV0) {
        tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                if session
                    .bootstrap_fence
                    .lock()
                    .expect("fence is readable")
                    .first_poll_complete
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pump must publish its real first-poll handshake");
    }

    async fn wait_for_bootstrap_terminal(session: &OperationSessionV0, expected: TransportErrorV0) {
        tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                if session.bootstrap_fence_terminal() == Some(expected.clone()) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pump must publish its sticky terminal");
    }

    async fn stage_final_proof(
        session: &OperationSessionV0,
        address: &RootAddressV0,
        command: &CommandV0,
        initial_root: &ObjectV0,
        final_root: &ObjectV0,
        final_state: &ObjectV0,
    ) {
        wait_for_bootstrap_first_poll(session).await;
        session
            .arm_collected_bootstrap_baseline(address, command, initial_root, None)
            .expect("exact baseline arms before final-proof fixture");
        let mut fence = session.bootstrap_fence.lock().expect("fence lock");
        let mut authority = session.bootstrap.lock().expect("bootstrap lock");
        fence.final_state = Some(final_state.clone());
        fence.final_echo = Some(final_root.clone());
        fence.final_echo_consumed = false;
        authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
            BootstrapRootPhaseV0::FinalProofPending,
        ));
        session.gate.state.store(
            PrewriteGateV0::encode(0, PrewriteGateV0::FINAL_PROOF_PENDING),
            Ordering::Release,
        );
    }

    #[test]
    fn operation_budget_is_strict_and_test_shortening_cannot_relax_it() {
        let production = OperationBudgetV0::production();
        assert!(production.is_strictly_ordered());
        assert_eq!(production.max_prewrite_retries(), MAX_PREWRITE_RETRIES);

        assert!(
            OperationBudgetV0::test_shortened(
                Duration::from_millis(4),
                Duration::from_millis(3),
                Duration::from_millis(5),
                Duration::from_millis(6),
                1,
            )
            .is_err()
        );
        let shortened = OperationBudgetV0::test_shortened(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
            Duration::from_millis(4),
            1,
        )
        .expect("test-only policy is still strictly ordered");
        assert!(shortened.is_strictly_ordered());
    }

    #[test]
    fn prewrite_gate_has_one_irreversible_winner() {
        let gate = PrewriteGateV0::new();
        assert_eq!(gate.state(), PrewriteGateStateV0::PreWrite);
        assert!(gate.invalidate_authenticated_change());
        assert_eq!(gate.state(), PrewriteGateStateV0::Invalidated);
        assert_eq!(
            gate.try_mark_write_polled(),
            Err(PrewriteGateStateV0::Invalidated)
        );

        let gate = PrewriteGateV0::new();
        assert_eq!(gate.try_mark_write_polled(), Ok(()));
        assert_eq!(gate.state(), PrewriteGateStateV0::WritePolled);
        assert!(!gate.invalidate_authenticated_change());
    }

    #[test]
    fn collected_metadata_response_loss_requires_the_exact_v1_bundle_before_opening_next_phase() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("1");
        let state = bootstrap_test_state();
        arm_test_collected_bootstrap(&mut session, &root);
        session
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::RootMetadata, &root)
            .expect("metadata preflight is frozen before its one write");
        let permit = claim_first_write(&session).expect("metadata write is polled once");
        drop(permit);
        assert_eq!(session.gate.state(), PrewriteGateStateV0::AmbiguousHeld);

        let mut recovered = root.clone();
        recovered.metadata.resource_version = "2".into();
        recovered.value = bootstrap_root_metadata_expected_value(&root, &state, "bootstrap-token")
            .expect("v1 root bundle is representable");
        recovered.value["metadata"]["resourceVersion"] = json!("2");
        recovered.metadata.annotations =
            serde_json::from_value(recovered.value["metadata"]["annotations"].clone())
                .expect("recovered annotations are strings");
        assert!(bootstrap_root_metadata_proof(
            &recovered,
            &root,
            &state,
            "bootstrap-token"
        ));

        session
            .complete_recovered_bootstrap_root_metadata(&recovered)
            .expect("only the exact bundle can publish the recovered metadata authority");
        assert_eq!(
            session.gate.state(),
            PrewriteGateStateV0::BootstrapPhasePending
        );
        assert_eq!(session.gate.epoch(), 1);
        let fence = session
            .bootstrap_fence
            .lock()
            .expect("fence remains usable");
        assert_eq!(fence.metadata_echo.as_ref(), Some(&recovered));
        assert!(fence.terminal.is_none());
    }

    fn complete_root_metadata_write_with_pending_exact_echo(
        session: &OperationSessionV0,
        root: &ObjectV0,
    ) -> ObjectV0 {
        {
            let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
            authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                BootstrapRootPhaseV0::RootMetadata,
            ));
        }
        let mut metadata_echo = root.clone();
        metadata_echo.metadata.resource_version = "root-metadata-rv".into();
        metadata_echo.value["metadata"]["resourceVersion"] = json!("root-metadata-rv");
        let permit = claim_first_write(session).expect("metadata write polls once");
        assert!(
            session.observe_bootstrap_root_watch(&WatchEventV0::Modified(metadata_echo.clone(),))
        );
        session
            .complete_bootstrap_subwrite(permit, BootstrapRootPhaseV0::RootMetadata, &metadata_echo)
            .expect("the queued exact metadata echo is consumed after response publication");
        metadata_echo
    }

    #[test]
    fn delayed_server_shaped_baseline_echoes_do_not_make_root_metadata_ambiguous() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("1");
        arm_test_collected_bootstrap(&mut session, &root);
        {
            let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
            authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                BootstrapRootPhaseV0::RootMetadata,
            ));
        }
        let permit = claim_first_write(&session).expect("metadata write polls once");

        // Kubernetes may deliver the list/RV's initial Added or Modified
        // after the PATCH has polled, with only server-owned managedFields
        // reshaped. Both remain the same sealed baseline and are idempotent.
        for (event_kind, manager) in [("added", "watch-cache-a"), ("modified", "watch-cache-b")] {
            let mut delayed_baseline = root.clone();
            delayed_baseline.value["metadata"]["managedFields"] = json!([{
                "manager": manager,
                "operation":"Update",
                "apiVersion":"demo.oxideloop.dev/v1alpha1",
                "fieldsType":"FieldsV1",
                "fieldsV1":{"f:metadata":{}},
            }]);
            let event = if event_kind == "added" {
                WatchEventV0::Added(delayed_baseline)
            } else {
                WatchEventV0::Modified(delayed_baseline)
            };
            assert!(
                session.observe_bootstrap_root_watch(&event),
                "the delayed {event_kind} baseline echo is idempotent"
            );
        }

        let mut metadata_response = root.clone();
        metadata_response.metadata.resource_version = "root-metadata-rv".into();
        metadata_response.value["metadata"]["resourceVersion"] = json!("root-metadata-rv");
        metadata_response.value["metadata"]["managedFields"] = json!([{
            "manager":"oxideloop-controller",
            "operation":"Update",
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:metadata":{"f:annotations":{},"f:finalizers":{}}},
        }]);
        let mut metadata_watch_echo = metadata_response.clone();
        metadata_watch_echo.value["metadata"]["managedFields"] = json!([{
            "manager":"watch-cache",
            "operation":"Update",
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:metadata":{"f:annotations":{},"f:finalizers":{}}},
        }]);
        assert!(
            session.observe_bootstrap_root_watch(&WatchEventV0::Modified(metadata_watch_echo)),
            "the in-flight metadata echo is sequenced until its response arrives"
        );
        session
            .complete_bootstrap_subwrite(
                permit,
                BootstrapRootPhaseV0::RootMetadata,
                &metadata_response,
            )
            .expect("baseline and matching metadata echo prove the metadata write");
        let fence = session
            .bootstrap_fence
            .lock()
            .expect("fence remains usable");
        assert!(fence.metadata_echo_consumed);
        assert!(fence.terminal.is_none());
        assert_eq!(
            session.gate.state(),
            PrewriteGateStateV0::BootstrapPhasePending
        );
    }

    #[test]
    fn server_shaped_baseline_echoes_reject_authority_bearing_drift() {
        for changed_field in [
            "resourceVersion",
            "spec",
            "status",
            "annotation",
            "finalizer",
        ] {
            let mut session = test_session(
                Duration::from_millis(5),
                Duration::from_millis(30),
                Duration::from_millis(40),
                Duration::from_millis(50),
                3,
            );
            let root = bootstrap_authority_root("1");
            arm_test_collected_bootstrap(&mut session, &root);
            {
                let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
                authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                    BootstrapRootPhaseV0::RootMetadata,
                ));
            }
            let permit = claim_first_write(&session).expect("metadata write polls once");
            let mut drifted = root.clone();
            drifted.value["metadata"]["managedFields"] = json!([{"manager":"watch-cache"}]);
            match changed_field {
                "resourceVersion" => {
                    drifted.metadata.resource_version = "forged-rv".into();
                    drifted.value["metadata"]["resourceVersion"] = json!("forged-rv");
                }
                "spec" => drifted.value["spec"]["image"] = json!("foreign.invalid/app:latest"),
                "status" => drifted.value["status"]["observedGeneration"] = json!(1),
                "annotation" => {
                    drifted
                        .metadata
                        .annotations
                        .insert("foreign".into(), "changed".into());
                    drifted.value["metadata"]["annotations"]["foreign"] = json!("changed");
                }
                "finalizer" => {
                    drifted.value["metadata"]["finalizers"] = json!([ROOT_FINALIZER, "foreign"]);
                }
                _ => unreachable!("test mutations are exhaustive"),
            }
            assert!(
                session.observe_bootstrap_root_watch(&WatchEventV0::Modified(drifted)),
                "{changed_field} drift remains sequenced until the polled response classifies it"
            );
            let mut metadata_response = root.clone();
            metadata_response.metadata.resource_version = "root-metadata-rv".into();
            metadata_response.value["metadata"]["resourceVersion"] = json!("root-metadata-rv");
            assert!(matches!(
                session.complete_bootstrap_subwrite(
                    permit,
                    BootstrapRootPhaseV0::RootMetadata,
                    &metadata_response,
                ),
                Err(TransportErrorV0::ManualRecoveryHold)
            ));
            assert_eq!(
                session.prewrite_gate().state(),
                PrewriteGateStateV0::AmbiguousHeld,
                "{changed_field} cannot reopen the phase gate after an ambiguous response"
            );
            assert_eq!(
                session.bootstrap_fence_terminal(),
                Some(TransportErrorV0::PreWriteInvalidated),
                "{changed_field} drift stays terminal while the write is polled"
            );
        }
    }

    #[test]
    fn collected_root_metadata_echo_before_response_is_classified_once() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("1");
        arm_test_collected_bootstrap(&mut session, &root);
        let metadata_echo = complete_root_metadata_write_with_pending_exact_echo(&session, &root);
        assert_eq!(
            session.gate.state(),
            PrewriteGateStateV0::BootstrapPhasePending
        );
        let fence = session
            .bootstrap_fence
            .lock()
            .expect("fence remains usable");
        assert_eq!(fence.metadata_echo.as_ref(), Some(&metadata_echo));
        assert!(fence.metadata_echo_consumed);
        assert!(fence.terminal.is_none());
    }

    #[test]
    fn classified_root_metadata_echo_rejects_a_second_modified_or_added_record() {
        for record_kind in ["modified", "added"] {
            let mut session = test_session(
                Duration::from_millis(5),
                Duration::from_millis(30),
                Duration::from_millis(40),
                Duration::from_millis(50),
                3,
            );
            let root = bootstrap_authority_root("1");
            arm_test_collected_bootstrap(&mut session, &root);
            {
                let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
                authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                    BootstrapRootPhaseV0::RootMetadata,
                ));
            }
            let mut metadata_echo = root.clone();
            metadata_echo.metadata.resource_version = "root-metadata-rv".into();
            metadata_echo.value["metadata"]["resourceVersion"] = json!("root-metadata-rv");
            let permit = claim_first_write(&session).expect("metadata write polls once");
            assert!(
                session
                    .observe_bootstrap_root_watch(&WatchEventV0::Modified(metadata_echo.clone(),))
            );
            let second = match record_kind {
                "modified" => WatchEventV0::Modified(metadata_echo.clone()),
                "added" => WatchEventV0::Added(metadata_echo.clone()),
                _ => unreachable!("test table is exhaustive"),
            };
            assert!(
                session.observe_bootstrap_root_watch(&second),
                "an in-flight record remains sequenced until its response classifies it"
            );
            assert!(matches!(
                session.complete_bootstrap_subwrite(
                    permit,
                    BootstrapRootPhaseV0::RootMetadata,
                    &metadata_echo
                ),
                Err(TransportErrorV0::ManualRecoveryHold)
            ));
            assert!(
                session
                    .bootstrap_fence
                    .lock()
                    .expect("fence is readable")
                    .metadata_echo_consumed,
                "the first exact Modified record was consumed before the second was rejected"
            );
            assert_eq!(
                session.bootstrap_fence_terminal(),
                Some(TransportErrorV0::PreWriteInvalidated)
            );
        }
    }

    #[test]
    fn consumed_root_metadata_echo_cannot_cross_later_root_state_link_or_final_proof() {
        for later_phase in ["root-state-link", "final-proof"] {
            let mut session = test_session(
                Duration::from_millis(5),
                Duration::from_millis(30),
                Duration::from_millis(40),
                Duration::from_millis(50),
                3,
            );
            let root = bootstrap_authority_root("1");
            arm_test_collected_bootstrap(&mut session, &root);
            let metadata_echo =
                complete_root_metadata_write_with_pending_exact_echo(&session, &root);
            {
                let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
                authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                    if later_phase == "root-state-link" {
                        BootstrapRootPhaseV0::RootStateLink
                    } else {
                        BootstrapRootPhaseV0::FinalProofPending
                    },
                ));
            }
            if later_phase == "final-proof" {
                let (final_root, _) = final_proof_fixture(&root);
                let mut fence = session.bootstrap_fence.lock().expect("fence is writable");
                fence.final_echo = Some(final_root);
                fence.final_echo_consumed = false;
                session.gate.state.store(
                    PrewriteGateV0::encode(1, PrewriteGateV0::FINAL_PROOF_PENDING),
                    Ordering::Release,
                );
            }
            assert!(
                !session.observe_bootstrap_root_watch(&WatchEventV0::Modified(metadata_echo)),
                "a consumed metadata echo cannot cross {later_phase}"
            );
            assert_eq!(
                session.bootstrap_fence_terminal(),
                Some(TransportErrorV0::PreWriteInvalidated)
            );
        }
    }

    #[test]
    fn final_echo_between_response_and_reread_is_exact_and_repeated_echoes_are_idempotent() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("1");
        arm_test_collected_bootstrap(&mut session, &root);
        {
            let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
            authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                BootstrapRootPhaseV0::RootStateLink,
            ));
        }
        let mut final_root = root.clone();
        final_root.metadata.resource_version = "2".into();
        final_root.value["metadata"]["resourceVersion"] = json!("2");
        final_root.value["status"]["state"] = json!({
            "rootUid":"root-uid", "name":"state", "uid":"state-uid"
        });

        let permit = claim_first_write(&session).expect("final write polls once");
        session
            .complete_bootstrap_subwrite(permit, BootstrapRootPhaseV0::RootStateLink, &final_root)
            .expect("response publishes final-proof state before rereads");
        let epoch = session.prewrite_gate().epoch();
        assert!(session.observe_bootstrap_root_watch(&WatchEventV0::Modified(final_root.clone())));
        assert!(session.observe_bootstrap_root_watch(&WatchEventV0::Modified(final_root.clone())));
        let fence = session
            .bootstrap_fence
            .lock()
            .expect("fence remains usable");
        assert!(fence.final_echo_consumed);
        assert_eq!((fence.committed, fence.enqueued, fence.consumed), (2, 2, 2));
        assert!(fence.terminal.is_none());
        assert_eq!(
            session.prewrite_gate().epoch(),
            epoch,
            "echoes mint no retry epoch"
        );
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::FinalProofPending
        );
    }

    #[test]
    fn final_proof_rejects_external_mutation_and_terminal_records() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("1");
        arm_test_collected_bootstrap(&mut session, &root);
        {
            let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
            authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                BootstrapRootPhaseV0::RootStateLink,
            ));
        }
        let mut final_root = root.clone();
        final_root.metadata.resource_version = "2".into();
        final_root.value["metadata"]["resourceVersion"] = json!("2");
        final_root.value["status"]["state"] = json!({
            "rootUid":"root-uid", "name":"state", "uid":"state-uid"
        });
        let permit = claim_first_write(&session).expect("final write polls once");
        session
            .complete_bootstrap_subwrite(permit, BootstrapRootPhaseV0::RootStateLink, &final_root)
            .expect("response opens final proof");

        let mut external = final_root.clone();
        external.metadata.resource_version = "external-rv".into();
        external.value["metadata"]["resourceVersion"] = json!("external-rv");
        external.value["spec"]["message"] = json!("external mutation");
        assert!(!session.observe_bootstrap_root_watch(&WatchEventV0::Modified(external)));
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::PreWriteInvalidated)
        );
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::FinalProofPending
        );

        // A terminal after final-proof entry remains sticky too. It cannot
        // move the final gate back to a writable state or become an echo.
        session.observe_bootstrap_root_terminal(TransportErrorV0::WatchGone);
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::PreWriteInvalidated)
        );
        let fence = session
            .bootstrap_fence
            .lock()
            .expect("fence remains readable");
        assert!(fence.records.is_empty());
        assert_eq!(
            fence.consumed, 2,
            "external and terminal records were consumed in order"
        );
    }

    #[test]
    fn final_proof_rejects_deleted_root_records() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("1");
        arm_test_collected_bootstrap(&mut session, &root);
        {
            let mut authority = session.bootstrap.lock().expect("bootstrap authority lock");
            authority.phase = Some(BootstrapAuthorityPhaseV0::Active(
                BootstrapRootPhaseV0::RootStateLink,
            ));
        }
        let mut final_root = root.clone();
        final_root.metadata.resource_version = "2".into();
        final_root.value["metadata"]["resourceVersion"] = json!("2");
        final_root.value["status"]["state"] = json!({
            "rootUid":"root-uid", "name":"state", "uid":"state-uid"
        });
        let permit = claim_first_write(&session).expect("final write polls once");
        session
            .complete_bootstrap_subwrite(permit, BootstrapRootPhaseV0::RootStateLink, &final_root)
            .expect("response opens final proof");

        assert!(!session.observe_bootstrap_root_watch(&WatchEventV0::Deleted(final_root)));
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::PreWriteInvalidated),
            "a delete is terminal evidence, never a final acknowledgement"
        );
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::FinalProofPending
        );
    }

    #[tokio::test]
    async fn final_proof_accepts_the_final_watch_echo_after_server_owned_metadata_reshaping() {
        let (session, address, command, initial_root) = sealed_test_collected_bootstrap();
        let (final_root, final_state) = final_proof_fixture(&initial_root);
        let transport = final_proof_transport(final_root.clone(), final_state.clone());
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("final-proof pump installs");
        stage_final_proof(
            &session,
            &address,
            &command,
            &initial_root,
            &final_root,
            &final_state,
        )
        .await;

        // The API server may serialize `managedFields` differently in the
        // delayed watch event than in the PATCH response. This is the live
        // response-before-watch ordering: UID/RV and every authority-bearing
        // field remain exact, while server-owned bookkeeping is reshaped.
        let mut watch_echo = final_root.clone();
        watch_echo.value["metadata"]["managedFields"] = json!([{
            "manager":"watch-cache",
            "operation":"Update",
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:status":{"f:state":{}}},
        }]);
        assert!(bootstrap_response_matches(&watch_echo, &final_root));
        for changed_field in ["resourceVersion", "status", "spec"] {
            let mut forged = watch_echo.clone();
            match changed_field {
                "resourceVersion" => {
                    forged.metadata.resource_version = "forged-rv".into();
                    forged.value["metadata"]["resourceVersion"] = json!("forged-rv");
                }
                "status" => forged.value["status"]["state"]["uid"] = json!("forged-state"),
                "spec" => forged.value["spec"]["image"] = json!("foreign.invalid/app:latest"),
                _ => unreachable!("test mutations are exhaustive"),
            }
            assert!(
                !bootstrap_response_matches(&forged, &final_root),
                "{changed_field} remains acknowledgement-critical"
            );
        }
        source
            .send(Ok(WatchEventV0::Modified(watch_echo)))
            .await
            .expect("server-shaped final echo fits the source");
        let event = tokio::time::timeout(Duration::from_millis(100), watch.next_event())
            .await
            .expect("pump forwards final echo")
            .expect("final echo is data");
        assert!(session.observe_bootstrap_root_watch(&event));
        let (root_ref, state_ref) = address.references();
        let (root, state) = session
            .complete_collected_bootstrap_final_proof(
                &transport,
                &root_ref,
                &state_ref,
                "bootstrap-token",
            )
            .await
            .expect("server-owned watch reshaping plus exact rereads proves final acknowledgement");
        assert_eq!(root, final_root);
        assert_eq!(state, final_state);
        assert_eq!(session.prewrite_gate().state(), PrewriteGateStateV0::Proven);
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert!(fence.disarmed && !fence.armed);
            assert_eq!(fence.committed, fence.enqueued);
            assert_eq!(fence.enqueued, fence.consumed);
        }
        session
            .close_collected_bootstrap_fence(&mut watch)
            .await
            .expect("proven pump closes cleanly");
    }

    #[tokio::test]
    async fn final_proof_repeats_when_an_exact_echo_arrives_after_its_watermark() {
        let (session, address, command, initial_root) = sealed_test_collected_bootstrap();
        let (final_root, final_state) = final_proof_fixture(&initial_root);
        let state_read_started = Arc::new(tokio::sync::Notify::new());
        let release_state_read = Arc::new(tokio::sync::Notify::new());
        let first_state_read = Arc::new(AtomicBool::new(true));
        let state_reads = Arc::new(AtomicUsize::new(0));
        let root_reads = Arc::new(AtomicUsize::new(0));
        let service = service_fn({
            let service_root = final_root.clone();
            let service_state = final_state.clone();
            let state_read_started = Arc::clone(&state_read_started);
            let release_state_read = Arc::clone(&release_state_read);
            let first_state_read = Arc::clone(&first_state_read);
            let state_reads = Arc::clone(&state_reads);
            let root_reads = Arc::clone(&root_reads);
            move |request: Request<Body>| {
                let root = service_root.clone();
                let state = service_state.clone();
                let state_read_started = Arc::clone(&state_read_started);
                let release_state_read = Arc::clone(&release_state_read);
                let first_state_read = Arc::clone(&first_state_read);
                let state_reads = Arc::clone(&state_reads);
                let root_reads = Arc::clone(&root_reads);
                async move {
                    let path = request.uri().path();
                    if path.ends_with("/oxideloopstates/state") {
                        state_reads.fetch_add(1, Ordering::SeqCst);
                        if first_state_read.swap(false, Ordering::SeqCst) {
                            state_read_started.notify_one();
                            release_state_read.notified().await;
                        }
                        Ok::<_, Infallible>(test_object_response(&state))
                    } else if path.ends_with("/webapplications/app") {
                        root_reads.fetch_add(1, Ordering::SeqCst);
                        Ok(test_object_response(&root))
                    } else {
                        panic!("unexpected post-watermark request: {path}");
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let (source, mut watch) = WatchHandleV0::test_channel(3);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("final-proof pump installs");
        stage_final_proof(
            &session,
            &address,
            &command,
            &initial_root,
            &final_root,
            &final_state,
        )
        .await;

        source
            .send(Ok(WatchEventV0::Modified(final_root.clone())))
            .await
            .expect("initial echo fits source");
        let initial = watch.next_event().await.expect("initial echo is data");
        assert!(session.observe_bootstrap_root_watch(&initial));
        let (root_ref, state_ref) = address.references();
        let proof = session.complete_collected_bootstrap_final_proof(
            &transport,
            &root_ref,
            &state_ref,
            "bootstrap-token",
        );
        let publish_after_capture = async {
            state_read_started.notified().await;
            source
                .send(Ok(WatchEventV0::Modified(final_root.clone())))
                .await
                .expect("post-watermark echo fits source");
            let late = watch
                .next_event()
                .await
                .expect("post-watermark echo is data");
            assert!(session.observe_bootstrap_root_watch(&late));
            release_state_read.notify_one();
        };
        let (proof, ()) = tokio::join!(proof, publish_after_capture);
        proof.expect("late exact echo forces a fresh proof pass, not a stale acknowledgement");
        assert!(
            state_reads.load(Ordering::SeqCst) >= 2 && root_reads.load(Ordering::SeqCst) >= 2,
            "the record committed after capture forces a complete reread pass"
        );
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert_eq!((fence.committed, fence.enqueued, fence.consumed), (2, 2, 2));
            assert!(fence.disarmed);
        }
        session
            .close_collected_bootstrap_fence(&mut watch)
            .await
            .expect("proven pump closes cleanly");
    }

    #[tokio::test]
    async fn final_proof_deadline_rejects_a_missing_exact_echo() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(20),
            Duration::from_millis(40),
            Duration::from_millis(100),
            1,
        );
        session.collector_bound = true;
        let address = RootAddressV0 {
            namespace: "ns".into(),
            name: "app".into(),
            state_name: "state".into(),
        };
        let command = test_bootstrap_command();
        let initial_root = bootstrap_authority_root("1");
        session
            .seal_collected_bootstrap_fence(&address, &command, &initial_root, None)
            .expect("deadline fixture seals");
        let (final_root, final_state) = final_proof_fixture(&initial_root);
        let transport = final_proof_transport(final_root.clone(), final_state.clone());
        let (_source, mut watch) = WatchHandleV0::test_channel(1);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("deadline fixture pump installs");
        stage_final_proof(
            &session,
            &address,
            &command,
            &initial_root,
            &final_root,
            &final_state,
        )
        .await;
        let (root_ref, state_ref) = address.references();
        assert!(matches!(
            session
                .complete_collected_bootstrap_final_proof(
                    &transport,
                    &root_ref,
                    &state_ref,
                    "bootstrap-token",
                )
                .await,
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::ReadOnly
            ))
        ));
        assert_ne!(session.prewrite_gate().state(), PrewriteGateStateV0::Proven);
        let _ = session.close_collected_bootstrap_fence(&mut watch).await;
    }

    #[tokio::test]
    async fn unlinked_deleting_v1_bundle_finishes_exact_child_cleanup_then_root_finalizer() {
        let (root, state) = unlinked_deleting_bootstrap_fixture();
        let bundle = bootstrap_authority_bundle_v1(&root).expect("fixture bundle parses");
        validate_unlinked_deleting_bootstrap_root(&root)
            .expect("fixture root is cleanup-authoritative");
        validate_unlinked_deleting_bootstrap_state(
            &root,
            &state,
            bundle.as_ref().expect("fixture carries a bundle"),
        )
        .expect("fixture State is cleanup-authoritative");
        let wire = Arc::new(Mutex::new(UnlinkedCleanupWireV0 {
            root: Some(root),
            state: Some(state),
            lose_state_finalizer_response: false,
            lose_root_finalizer_response: false,
            state_finalizer_patches: 0,
            root_finalizer_patches: 0,
        }));
        let transport = cleanup_transport(Arc::clone(&wire));

        assert_eq!(
            transport
                .cleanup_unlinked_deleting_bootstrap_v1("ns", "app")
                .await,
            Ok(UnlinkedBootstrapDeletionCleanupV0::Completed)
        );
        let wire = wire.lock().expect("cleanup wire is readable");
        assert_eq!(wire.state_finalizer_patches, 1);
        assert_eq!(wire.root_finalizer_patches, 1);
        assert!(wire.state.is_none());
        assert!(
            !has_finalizer(
                wire.root.as_ref().expect("root survives response proof"),
                ROOT_FINALIZER
            ),
            "only the exact root finalizer is removed after State absence"
        );
    }

    #[tokio::test]
    async fn unlinked_deleting_v1_bundle_response_loss_exactly_proves_state_then_root_cleanup() {
        let (root, state) = unlinked_deleting_bootstrap_fixture();
        let wire = Arc::new(Mutex::new(UnlinkedCleanupWireV0 {
            root: Some(root),
            state: Some(state),
            lose_state_finalizer_response: true,
            lose_root_finalizer_response: true,
            state_finalizer_patches: 0,
            root_finalizer_patches: 0,
        }));
        let transport = cleanup_transport(Arc::clone(&wire));

        assert_eq!(
            transport
                .cleanup_unlinked_deleting_bootstrap_v1("ns", "app")
                .await,
            Ok(UnlinkedBootstrapDeletionCleanupV0::Completed)
        );
        let wire = wire.lock().expect("cleanup wire is readable");
        assert_eq!(wire.state_finalizer_patches, 1);
        assert_eq!(wire.root_finalizer_patches, 1);
    }

    #[tokio::test]
    async fn unlinked_deleting_v1_bundle_replacement_holds_without_a_patch() {
        let (root, mut replacement) = unlinked_deleting_bootstrap_fixture();
        replacement.metadata.uid = "same-name-replacement".into();
        replacement.value["metadata"]["uid"] = json!("same-name-replacement");
        replacement.metadata.resource_version = "replacement-rv".into();
        replacement.value["metadata"]["resourceVersion"] = json!("replacement-rv");
        let wire = Arc::new(Mutex::new(UnlinkedCleanupWireV0 {
            root: Some(root),
            state: Some(replacement),
            lose_state_finalizer_response: false,
            lose_root_finalizer_response: false,
            state_finalizer_patches: 0,
            root_finalizer_patches: 0,
        }));
        let transport = cleanup_transport(Arc::clone(&wire));

        assert_eq!(
            transport
                .cleanup_unlinked_deleting_bootstrap_v1("ns", "app")
                .await,
            Err(TransportErrorV0::ManualRecoveryHold)
        );
        let wire = wire.lock().expect("cleanup wire is readable");
        assert_eq!(wire.state_finalizer_patches, 0);
        assert_eq!(wire.root_finalizer_patches, 0);
    }

    #[tokio::test]
    async fn unlinked_deleting_v1_bundle_absent_child_removes_only_the_root_finalizer() {
        let (root, _state) = unlinked_deleting_bootstrap_fixture();
        let wire = Arc::new(Mutex::new(UnlinkedCleanupWireV0 {
            root: Some(root),
            state: None,
            lose_state_finalizer_response: false,
            lose_root_finalizer_response: false,
            state_finalizer_patches: 0,
            root_finalizer_patches: 0,
        }));
        let transport = cleanup_transport(Arc::clone(&wire));

        assert_eq!(
            transport
                .cleanup_unlinked_deleting_bootstrap_v1("ns", "app")
                .await,
            Ok(UnlinkedBootstrapDeletionCleanupV0::Completed)
        );
        let wire = wire.lock().expect("cleanup wire is readable");
        assert_eq!(wire.state_finalizer_patches, 0);
        assert_eq!(wire.root_finalizer_patches, 1);
    }

    #[tokio::test]
    async fn unlinked_deleting_partial_v1_bundle_holds_without_a_patch() {
        let (mut root, state) = unlinked_deleting_bootstrap_fixture();
        root.metadata
            .annotations
            .remove(BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION);
        root.value["metadata"]["annotations"]
            .as_object_mut()
            .expect("fixture annotations")
            .remove(BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION);
        let wire = Arc::new(Mutex::new(UnlinkedCleanupWireV0 {
            root: Some(root),
            state: Some(state),
            lose_state_finalizer_response: false,
            lose_root_finalizer_response: false,
            state_finalizer_patches: 0,
            root_finalizer_patches: 0,
        }));
        let transport = cleanup_transport(Arc::clone(&wire));

        assert_eq!(
            transport
                .cleanup_unlinked_deleting_bootstrap_v1("ns", "app")
                .await,
            Err(TransportErrorV0::ManualRecoveryHold)
        );
        let wire = wire.lock().expect("cleanup wire is readable");
        assert_eq!(wire.state_finalizer_patches, 0);
        assert_eq!(wire.root_finalizer_patches, 0);
    }

    #[test]
    fn linked_state_finalizer_requires_current_state_and_receipt_witnesses() {
        let (address, root, state, preconditions, token) = linked_deletion_fixture();
        let fence = EffectFenceV0 {
            root: root.clone(),
            state: state.clone(),
            target: None,
        };
        assert!(has_linked_deleting_root_authority(
            &address,
            &root,
            &preconditions
        ));
        let receipt = linked_deletion_receipt(&root, &address, &preconditions)
            .expect("fixture receipt is valid");
        let fixture_spec: OxideLoopStateSpec = serde_json::from_value(
            state
                .value
                .get("spec")
                .cloned()
                .expect("fixture State spec"),
        )
        .expect("fixture State spec deserializes");
        assert!(
            fixture_spec.validate().is_ok(),
            "fixture State spec validates: {:?}",
            fixture_spec.validate()
        );
        assert!(linked_deletion_state_spec_matches(
            &root,
            &state,
            &preconditions,
            &receipt
        ));
        validate_linked_deletion_state_finalizer_authority(
            &address,
            &fence,
            &preconditions,
            &token,
        )
        .expect("legacy linked State is deletable without a bootstrap-v1 bundle");

        let mut missing_command_token = state.clone();
        missing_command_token
            .metadata
            .annotations
            .insert(COMMAND_ID_ANNOTATION.into(), "other".into());
        missing_command_token.value["metadata"]["annotations"][COMMAND_ID_ANNOTATION] =
            json!("other");
        assert!(
            validate_linked_deletion_state_finalizer_authority(
                &address,
                &EffectFenceV0 {
                    root: root.clone(),
                    state: missing_command_token,
                    target: None,
                },
                &preconditions,
                &token,
            )
            .is_err()
        );

        for (purpose, phase, closure) in [
            ("MutationResult", "MutationResult", None),
            ("AutomationPending", "AutomationPending", None),
            (
                "GenerationAdvanceClosure",
                "GenerationAdvanceClosure",
                Some(json!({
                    "rootUid":"root-uid", "sourceGeneration":2,
                    "sourceRootResourceVersion":"root-rv-previous",
                    "sourceStateUid":"state-uid",
                    "sourceStateResourceVersion":"state-rv-previous",
                    "rebaseCount":0, "purpose":"MutationResult",
                })),
            ),
        ] {
            let mut unrelated_purpose = state.clone();
            unrelated_purpose.value["spec"]["state"]["pendingPersist"]["purpose"] = json!(purpose);
            unrelated_purpose.value["spec"]["state"]["pendingPersist"]["phase"] = json!(phase);
            if let Some(closure) = closure {
                unrelated_purpose.value["spec"]["state"]["generationAdvanceClosure"] = closure;
            }
            assert!(
                validate_linked_deletion_state_finalizer_authority(
                    &address,
                    &EffectFenceV0 {
                        root: root.clone(),
                        state: unrelated_purpose,
                        target: None,
                    },
                    &preconditions,
                    &token,
                )
                .is_err(),
                "non-deletion current persist purpose {purpose} cannot authorize finalization"
            );
        }

        let mut replacement = state.clone();
        replacement.metadata.uid = "replacement-uid".into();
        replacement.value["metadata"]["uid"] = json!("replacement-uid");
        assert!(
            validate_linked_deletion_state_finalizer_authority(
                &address,
                &EffectFenceV0 {
                    root: root.clone(),
                    state: replacement,
                    target: None,
                },
                &preconditions,
                &token,
            )
            .is_err()
        );

        let mut changed_receipt = root;
        changed_receipt.value["status"]["deletionCleanupReceipt"]["cleanupToken"] =
            json!("e".repeat(64));
        assert!(
            validate_linked_deletion_state_finalizer_authority(
                &address,
                &EffectFenceV0 {
                    root: changed_receipt,
                    state,
                    target: None,
                },
                &preconditions,
                &token,
            )
            .is_err()
        );
    }

    #[test]
    fn linked_root_finalizer_after_state_absence_keeps_receipt_identity() {
        let (address, root, _state, preconditions, _) = linked_deletion_fixture();
        let token = linked_root_finalizer_command_token(
            &preconditions.root_uid,
            preconditions.root_generation,
            &preconditions.root_resource_version,
            &address.state_name,
            &preconditions.state_uid,
            &preconditions.state_resource_version,
            &"d".repeat(64),
            3,
            &"c".repeat(64),
        );
        validate_linked_root_finalizer_after_state_absence(&address, &root, &preconditions, &token)
            .expect("legacy linked root retains sufficient receipt authority");

        let mut replacement_pins = preconditions.clone();
        replacement_pins.state_uid = "replacement-uid".into();
        assert!(
            validate_linked_root_finalizer_after_state_absence(
                &address,
                &root,
                &replacement_pins,
                &token
            )
            .is_err()
        );

        let mut malformed_receipt = root;
        malformed_receipt.value["status"]["deletionCleanupReceipt"]["deletionId"] = json!("bad");
        assert!(
            validate_linked_root_finalizer_after_state_absence(
                &address,
                &malformed_receipt,
                &preconditions,
                &token
            )
            .is_err()
        );
        let mut future_receipt = malformed_receipt;
        future_receipt.value["status"]["deletionCleanupReceipt"]["deletionId"] =
            json!("d".repeat(64));
        future_receipt.value["status"]["deletionCleanupReceipt"]["deletionStartGeneration"] =
            json!(4);
        assert!(
            validate_linked_root_finalizer_after_state_absence(
                &address,
                &future_receipt,
                &preconditions,
                &token
            )
            .is_err(),
            "a receipt cannot begin after the fresh root generation"
        );
    }

    #[test]
    fn linked_finalizer_responses_may_change_only_finalizers_rv_and_managed_fields() {
        let (_address, root, state, _preconditions, _token) = linked_deletion_fixture();
        for (before, finalizer) in [(state, STATE_FINALIZER), (root, ROOT_FINALIZER)] {
            let mut written = before.clone();
            written.metadata.resource_version = "after-finalizer".into();
            written.value["metadata"]["resourceVersion"] = json!("after-finalizer");
            written.value["metadata"]["finalizers"] = json!(Vec::<String>::new());
            written.value["metadata"]["managedFields"] = json!([{"manager":"server"}]);
            assert!(finalizer_response_matches(&before, &written, finalizer));

            written.value["spec"]["stateVersion"] = json!(2);
            assert!(
                !finalizer_response_matches(&before, &written, finalizer),
                "a finalizer response cannot smuggle a spec mutation"
            );

            let mut malformed_prewrite = before.clone();
            malformed_prewrite.value["metadata"]["finalizers"] = json!([finalizer, 7]);
            let mut exact_response = before.clone();
            exact_response.metadata.resource_version = "after-finalizer".into();
            exact_response.value["metadata"]["resourceVersion"] = json!("after-finalizer");
            exact_response.value["metadata"]["finalizers"] = json!(Vec::<String>::new());
            assert!(
                !finalizer_response_matches(&malformed_prewrite, &exact_response, finalizer),
                "a malformed prewrite finalizer witness cannot authorize an acknowledgement"
            );

            let mut mirrored_malformed_response = malformed_prewrite.clone();
            mirrored_malformed_response.metadata.resource_version = "after-finalizer".into();
            mirrored_malformed_response.value["metadata"]["resourceVersion"] =
                json!("after-finalizer");
            mirrored_malformed_response.value["metadata"]["finalizers"] = json!([7]);
            assert!(
                !finalizer_response_matches(
                    &malformed_prewrite,
                    &mirrored_malformed_response,
                    finalizer
                ),
                "a mirrored malformed finalizer pair cannot acknowledge removal"
            );
        }
    }

    #[tokio::test]
    async fn deletion_state_finalizer_response_loss_is_a_hold_not_an_ack() {
        let (address, root, state, preconditions, token) = linked_deletion_fixture();
        let patch_calls = Arc::new(AtomicUsize::new(0));
        let service = service_fn({
            let root = root.clone();
            let state = state.clone();
            let state_name = state.reference.name.clone();
            let patch_calls = Arc::clone(&patch_calls);
            move |request: Request<Body>| {
                let root = root.clone();
                let state = state.clone();
                let state_name = state_name.clone();
                let patch_calls = Arc::clone(&patch_calls);
                async move {
                    let path = request.uri().path();
                    if request.method() == http::Method::GET
                        && path.ends_with("/webapplications/app")
                    {
                        Ok::<_, Infallible>(test_object_response(&root))
                    } else if request.method() == http::Method::GET
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        Ok(test_object_response(&state))
                    } else if request.method() == http::Method::PATCH
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        patch_calls.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<Result<Response<Body>, Infallible>>().await
                    } else {
                        panic!(
                            "unexpected state-finalizer response-loss request: {} {path}",
                            request.method()
                        );
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let operation = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(80),
            0,
        );
        let command = CommandV0::RemoveStateFinalizer {
            token,
            preconditions,
        };

        assert!(matches!(
            transport
                .dispatch_in_operation(&address, &command, &operation)
                .await,
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        assert_eq!(
            patch_calls.load(Ordering::SeqCst),
            1,
            "the exact State finalizer patch was polled once but a lost response cannot acknowledge deletion"
        );
    }

    #[tokio::test]
    async fn crash_after_root_metadata_with_same_name_replacement_holds_root_delete() {
        let (address, root, original_state, preconditions, _) = linked_deletion_fixture();
        let token = linked_root_finalizer_command_token(
            &preconditions.root_uid,
            preconditions.root_generation,
            &preconditions.root_resource_version,
            &address.state_name,
            &preconditions.state_uid,
            &preconditions.state_resource_version,
            &"d".repeat(64),
            3,
            &"c".repeat(64),
        );
        let mut replacement = original_state;
        replacement.metadata.uid = "replacement-state-uid".into();
        replacement.value["metadata"]["uid"] = json!("replacement-state-uid");
        replacement.metadata.resource_version = "replacement-state-rv".into();
        replacement.value["metadata"]["resourceVersion"] = json!("replacement-state-rv");
        let patch_calls = Arc::new(AtomicUsize::new(0));
        let service = service_fn({
            let root = root.clone();
            let replacement = replacement.clone();
            let state_name = address.state_name.clone();
            let patch_calls = Arc::clone(&patch_calls);
            move |request: Request<Body>| {
                let root = root.clone();
                let replacement = replacement.clone();
                let state_name = state_name.clone();
                let patch_calls = Arc::clone(&patch_calls);
                async move {
                    let path = request.uri().path();
                    if request.method() == http::Method::GET
                        && path.ends_with("/webapplications/app")
                    {
                        Ok::<_, Infallible>(test_object_response(&root))
                    } else if request.method() == http::Method::GET
                        && path.ends_with(&format!("/oxideloopstates/{state_name}"))
                    {
                        Ok(test_object_response(&replacement))
                    } else if request.method() == http::Method::PATCH {
                        patch_calls.fetch_add(1, Ordering::SeqCst);
                        panic!("a replacement State must block root-finalizer patching")
                    } else {
                        panic!(
                            "unexpected post-RootMetadata replacement request: {} {path}",
                            request.method()
                        );
                    }
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let command = CommandV0::RemoveRootFinalizer {
            token,
            preconditions,
        };

        assert!(matches!(
            transport.dispatch(&address, &command).await,
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        assert_eq!(
            patch_calls.load(Ordering::SeqCst),
            0,
            "a same-name replacement is never treated as the original State's absence"
        );
    }

    #[tokio::test]
    async fn bootstrap_pump_commits_then_enqueues_and_gracefully_returns_the_watch() {
        let (session, address, command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(4);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("fence pump installs before the first poll");

        source
            .send(Ok(WatchEventV0::Bookmark {
                resource_version: "bookmark-rv".into(),
            }))
            .await
            .expect("source stays open");
        tokio::task::yield_now().await;
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert_eq!((fence.committed, fence.enqueued, fence.consumed), (0, 0, 0));
        }

        source
            .send(Ok(WatchEventV0::Modified(root.clone())))
            .await
            .expect("target record fits source");
        let event = tokio::time::timeout(Duration::from_millis(100), watch.next_event())
            .await
            .expect("pump forwards target record")
            .expect("target record is not terminal");
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert_eq!((fence.committed, fence.enqueued, fence.consumed), (1, 1, 0));
        }
        assert!(session.observe_bootstrap_root_watch(&event));
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert_eq!((fence.committed, fence.enqueued, fence.consumed), (1, 1, 1));
        }
        session
            .arm_collected_bootstrap_baseline(&address, &command, &root, None)
            .expect("only the fully consumed exact baseline can arm");

        // This deliberately closes an unproven operation: cleanup latches a
        // terminal, returns the retained source, and never leaves a task
        // behind. A later ordinary event remains readable through the bridge.
        session
            .close_collected_bootstrap_fence(&mut watch)
            .await
            .expect("unproven cleanup still joins its pump");
        source
            .send(Ok(WatchEventV0::Modified(root.clone())))
            .await
            .expect("returned source remains usable");
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), watch.next_event())
                .await
                .expect("restored watch forwards later traffic")
                .expect("restored traffic is ordinary data"),
            WatchEventV0::Modified(root)
        );
    }

    #[tokio::test]
    async fn bootstrap_pump_cancellation_before_write_latches_a_sticky_terminal() {
        let (session, _address, _command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("fence pump installs");
        source
            .send(Ok(WatchEventV0::Modified(root)))
            .await
            .expect("source record fits");
        let _ = tokio::time::timeout(Duration::from_millis(100), watch.next_event())
            .await
            .expect("pump started")
            .expect("pump record");

        watch
            .pump_abort
            .as_ref()
            .expect("pump abort handle")
            .abort();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::ManualRecoveryHold)
        );
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::Invalidated
        );
        assert!(matches!(
            session.close_collected_bootstrap_fence(&mut watch).await,
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
    }

    #[tokio::test]
    async fn bootstrap_pump_terminal_during_write_polled_keeps_write_wins_sticky() {
        let (session, address, command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("fence pump installs");
        wait_for_bootstrap_first_poll(&session).await;
        session
            .arm_collected_bootstrap_baseline(&address, &command, &root, None)
            .expect("empty exact baseline arms");
        session
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::StatePost, &root)
            .expect("first phase freezes root authority");
        let permit = claim_first_write(&session).expect("write lease wins before cancellation");
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::WritePolled
        );

        // Force the task to enter its receive loop before cancellation. This
        // record is held pending because the write lease already won.
        source
            .send(Ok(WatchEventV0::Modified(root.clone())))
            .await
            .expect("source record fits");
        let event = tokio::time::timeout(Duration::from_millis(100), watch.next_event())
            .await
            .expect("pump forwards the in-flight record")
            .expect("in-flight record is data");
        assert!(session.observe_bootstrap_root_watch(&event));

        watch
            .pump_abort
            .as_ref()
            .expect("pump abort handle")
            .abort();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::ManualRecoveryHold)
        );
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::WritePolled,
            "the already-polled write is never replayed or cancelled"
        );
        drop(permit);
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::AmbiguousHeld
        );
    }

    #[test]
    fn bootstrap_pump_exit_guard_latches_panic_and_drop() {
        let (session, _address, _command, _root) = sealed_test_collected_bootstrap();
        let output_failure = Arc::new(AtomicBool::new(false));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = BootstrapPumpExitGuardV0 {
                fence: Arc::clone(&session.bootstrap_fence),
                gate: session.gate.clone(),
                wake: session.bootstrap_fence_wake.clone(),
                output_failure: Arc::clone(&output_failure),
                clean: false,
            };
            panic!("test-only pump unwind");
        }));
        assert!(result.is_err());
        assert!(output_failure.load(Ordering::Acquire));
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::ManualRecoveryHold)
        );
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::Invalidated
        );
    }

    #[tokio::test]
    async fn bootstrap_pump_runtime_drop_aborts_an_orphaned_operation() {
        let (session, _address, _command, _root) = sealed_test_collected_bootstrap();
        let fence = Arc::clone(&session.bootstrap_fence);
        let gate = session.gate.clone();
        let (_source, mut watch) = WatchHandleV0::test_channel(1);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("fence pump installs");
        drop(session);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(
            fence.lock().expect("orphaned fence is readable").terminal,
            Some(TransportErrorV0::ManualRecoveryHold)
        );
        assert_eq!(gate.state(), PrewriteGateStateV0::Invalidated);
        drop(watch);
    }

    #[tokio::test]
    async fn bootstrap_pump_after_disarm_forwards_without_reopening_counters() {
        let (session, _address, _command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("fence pump installs");
        {
            let mut fence = session.bootstrap_fence.lock().expect("fence is writable");
            fence.armed = false;
            fence.disarmed = true;
        }
        session.gate.state.store(
            PrewriteGateV0::encode(0, PrewriteGateV0::PROVEN),
            Ordering::Release,
        );
        source
            .send(Ok(WatchEventV0::Modified(root.clone())))
            .await
            .expect("post-disarm event fits source");
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), watch.next_event())
                .await
                .expect("ordinary event remains forwarded")
                .expect("ordinary event is data"),
            WatchEventV0::Modified(root)
        );
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert_eq!((fence.committed, fence.enqueued, fence.consumed), (0, 0, 0));
            assert!(fence.terminal.is_none());
        }
        session
            .close_collected_bootstrap_fence(&mut watch)
            .await
            .expect("proven close joins cleanly");
    }

    #[test]
    fn exact_response_loss_proof_closes_only_the_ambiguous_write_epoch() {
        let session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
            1,
        );
        assert!(matches!(
            session.prove_exact_response_loss_recovery(),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        let permit = claim_first_write(&session).expect("one write lease");
        drop(permit);
        assert_eq!(session.gate.state(), PrewriteGateStateV0::AmbiguousHeld);
        session
            .prove_exact_response_loss_recovery()
            .expect("exact proof closes its own epoch");
        assert_eq!(session.gate.state(), PrewriteGateStateV0::Proven);
        assert!(matches!(
            session.prove_exact_response_loss_recovery(),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
    }

    #[tokio::test]
    async fn expired_current_attempt_cutoff_cannot_poll_a_write() {
        let session = test_session(
            Duration::from_millis(1),
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(30),
            0,
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(matches!(
            claim_first_write(&session),
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::PreWrite
            ))
        ));
        assert_eq!(session.gate.state(), PrewriteGateStateV0::PreWrite);
    }

    #[tokio::test]
    async fn prewrite_attempts_are_bounded_and_retry_without_resetting_the_outer_session() {
        let session = test_session(
            Duration::from_millis(2),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
            2,
        );
        let original = session.clone();
        assert_eq!(
            within_operation::<()>(&session, FiniteIoStageV0::PreWrite, std::future::pending())
                .await,
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::PreWrite
            ))
        );
        assert_eq!(session.retry_prewrite_epoch(), Ok(1));
        assert!(session.matches_exact_session(&original));
        assert_eq!(session.gate.epoch(), 1);
        assert_eq!(
            within_operation::<()>(&session, FiniteIoStageV0::PreWrite, std::future::pending())
                .await,
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::PreWrite
            ))
        );
        assert_eq!(session.retry_prewrite_epoch(), Ok(2));
        assert_eq!(
            session.retry_prewrite_epoch(),
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::PreWrite
            ))
        );
    }

    #[test]
    fn stale_epochs_and_concurrent_session_clones_cannot_poll_a_second_write() {
        let session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
            2,
        );
        let stale = SubwritePermitV0 {
            gate: session.gate.clone(),
            epoch: 0,
            bootstrap: false,
            bootstrap_collected: false,
        };
        assert_eq!(session.retry_prewrite_epoch(), Ok(1));
        assert!(matches!(
            stale.prove(),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));

        let session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
            1,
        );
        let clone = session.clone();
        let permit = claim_first_write(&session).expect("first clone owns the only write lease");
        assert!(matches!(
            claim_first_write(&clone),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        assert!(matches!(
            session.retry_prewrite_epoch(),
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::PreWrite
            ))
        ));
        drop(permit);
        assert_eq!(session.gate.state(), PrewriteGateStateV0::AmbiguousHeld);
    }

    #[test]
    fn bootstrap_advances_only_after_direct_or_exact_proven_subwrites() {
        let session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            4,
        );
        session
            .begin_bootstrap_sequence()
            .expect("first bootstrap owner");
        let direct = claim_first_write(&session).expect("first bootstrap subwrite");
        direct.prove().expect("direct response proves subwrite");
        assert_eq!(session.gate.state(), PrewriteGateStateV0::PreWrite);
        assert_eq!(session.gate.epoch(), 1);

        let ambiguous = claim_first_write(&session).expect("second bootstrap subwrite");
        drop(ambiguous);
        assert_eq!(session.gate.state(), PrewriteGateStateV0::AmbiguousHeld);
        assert!(matches!(
            claim_first_write(&session),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        session
            .prove_bootstrap_after_exact_proof()
            .expect("only an exact recovery proof may advance bootstrap");
        assert_eq!(session.gate.state(), PrewriteGateStateV0::PreWrite);
        assert_eq!(session.gate.epoch(), 2);
    }

    #[tokio::test]
    async fn bootstrap_phase_wait_registers_before_the_state_read_window() {
        let mut session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            2,
        );
        session.collector_bound = true;
        let address = RootAddressV0 {
            namespace: "ns".into(),
            name: "app".into(),
            state_name: "state".into(),
        };
        let command = test_bootstrap_command();
        let root = bootstrap_authority_root("1");
        session
            .seal_collected_bootstrap_fence(&address, &command, &root, None)
            .expect("collector seals the exact bootstrap authority");
        session.gate.state.store(
            PrewriteGateV0::encode(0, PrewriteGateV0::BOOTSTRAP_PHASE_PENDING),
            Ordering::Release,
        );
        let hook = Arc::new(BootstrapPhaseWaitHookV0 {
            state_read: AtomicBool::new(false),
            release: Arc::new(tokio::sync::Barrier::new(2)),
        });
        session.bootstrap_phase_wait_hook = Some(Arc::clone(&hook));
        let waiting = tokio::spawn({
            let session = session.clone();
            async move { session.wait_for_bootstrap_phase_authority().await }
        });
        while !hook.state_read.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        // This transition occupies the old implementation's lost-wake gap:
        // it occurred after the state read but before that implementation
        // registered its waiter.  The fixed implementation registered first,
        // so the parked preflight completes once released.
        assert!(session.prewrite_gate().seal_bootstrap_phase());
        hook.release.wait().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(20), waiting)
                .await
                .expect("phase waiter must not lose the seal wake")
                .expect("phase waiter task joins"),
            Ok(())
        );
    }

    #[tokio::test]
    async fn collected_bootstrap_binding_mismatch_performs_zero_io() {
        let (session, address, command, _root) = sealed_test_collected_bootstrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let service_calls = Arc::clone(&calls);
        let service = service_fn(move |_request: Request<Body>| {
            let service_calls = Arc::clone(&service_calls);
            async move {
                service_calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(Response::new(Body::empty()))
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let mut mismatched_address = address;
        mismatched_address.name = "other-root".into();

        assert!(matches!(
            transport
                .dispatch_in_operation(&mismatched_address, &command, &session)
                .await,
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a different full root address is rejected before any list, watch, GET, or write"
        );
    }

    #[test]
    fn bootstrap_direct_metadata_response_is_raw_response_bound() {
        let session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("2");
        session
            .begin_bootstrap_sequence()
            .expect("bootstrap authority begins");
        session
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::RootMetadata, &root)
            .expect("metadata preflight freezes root");
        session
            .record_bootstrap_owned_root_echo(&root, BootstrapRootPhaseV0::RootStateLink)
            .expect("metadata response is recorded for State-link");

        session
            .require_exact_root_metadata_response(&root)
            .expect("the exact recorded metadata response authorizes State-link");

        for label in ["status", "State-link", "finalizer", "annotation"] {
            let mut changed = root.clone();
            match label {
                "status" => changed.value["status"]["observedGeneration"] = json!(1),
                "State-link" => {
                    changed.value["status"]["state"] =
                        json!({"rootUid":"root-uid","name":"state","uid":"state-uid"});
                }
                "finalizer" => {
                    changed.value["metadata"]["finalizers"] = json!([ROOT_FINALIZER, "foreign"]);
                }
                "annotation" => {
                    changed
                        .metadata
                        .annotations
                        .insert("foreign".into(), "changed".into());
                    changed.value["metadata"]["annotations"]["foreign"] = json!("changed");
                }
                _ => unreachable!("test mutations are exhaustive"),
            }
            assert!(
                matches!(
                    session.require_exact_root_metadata_response(&changed),
                    Err(TransportErrorV0::Stale(_))
                ),
                "a {label} change must reject the RootStateLink preflight"
            );
        }
    }

    #[test]
    fn bootstrap_nonfinal_recovery_waits_for_phase_and_final_epoch_closes_once() {
        let root = bootstrap_authority_root("1");
        let mut nonfinal = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        arm_test_collected_bootstrap(&mut nonfinal, &root);
        nonfinal
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::StateFinalizer, &root)
            .expect("nonfinal phase freezes root");
        let ambiguous = claim_first_write(&nonfinal).expect("nonfinal write polled");
        drop(ambiguous);
        nonfinal
            .prove_bootstrap_after_exact_proof()
            .expect("exact nonfinal recovery advances only to a phase barrier");
        assert_eq!(
            nonfinal.gate.state(),
            PrewriteGateStateV0::BootstrapPhasePending
        );
        assert!(matches!(
            claim_first_write(&nonfinal),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));

        let terminal = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        terminal
            .begin_bootstrap_sequence()
            .expect("terminal bootstrap authority begins");
        terminal
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::RootStateLink, &root)
            .expect("terminal phase freezes root");
        let direct = claim_first_write(&terminal).expect("final write polled");
        terminal
            .mark_bootstrap_final_write_polled()
            .expect("terminal phase marks after its poll");
        direct
            .prove_bootstrap_final()
            .expect("direct final response proves its same epoch");
        assert_eq!(terminal.gate.state(), PrewriteGateStateV0::Proven);
        assert!(matches!(
            claim_first_write(&terminal),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));

        let recovered = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        recovered
            .begin_bootstrap_sequence()
            .expect("recovery bootstrap authority begins");
        recovered
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::RootStateLink, &root)
            .expect("recovery final phase freezes root");
        let ambiguous = claim_first_write(&recovered).expect("final response is ambiguous");
        recovered
            .mark_bootstrap_final_write_polled()
            .expect("final response loss is terminal");
        drop(ambiguous);
        recovered
            .prove_bootstrap_after_exact_proof()
            .expect("exact final recovery proves its matching epoch");
        assert_eq!(recovered.gate.state(), PrewriteGateStateV0::Proven);
        assert!(matches!(
            recovered.prove_bootstrap_after_exact_proof(),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
        assert!(matches!(
            claim_first_write(&recovered),
            Err(TransportErrorV0::ManualRecoveryHold)
        ));
    }

    #[test]
    fn bootstrap_ambiguous_write_proofs_are_phase_frozen_and_state_needs_a_root_link() {
        let root_ref = ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: "ns".into(),
            name: "app".into(),
        };
        let state_ref = ObjectRefV0 {
            resource: ResourceKindV0::OxideLoopState,
            namespace: "ns".into(),
            name: "state".into(),
        };
        let mut root_before = ObjectV0 {
            reference: root_ref.clone(),
            metadata: ObjectMetaV0 {
                uid: "root-uid".into(),
                resource_version: "1".into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"app","namespace":"ns","uid":"root-uid","resourceVersion":"1","generation":1,"annotations":{},"finalizers":[]},"spec":{"image":"example.invalid/web:latest","expose":false,"program":{"digest":"bundle"}},"status":{}}),
        };
        let mut state_before = ObjectV0 {
            reference: state_ref.clone(),
            metadata: ObjectMetaV0 {
                uid: "state-uid".into(),
                resource_version: "2".into(),
                annotations: BTreeMap::from([
                    (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
                    (TRANSACTION_ID_ANNOTATION.into(), "bootstrap-token".into()),
                    (COMMAND_ID_ANNOTATION.into(), "bootstrap-token".into()),
                ]),
                owner_uids: vec!["root-uid".into()],
            },
            value: json!({"apiVersion":"runtime.oxideloop.dev/v1alpha1","kind":"OxideLoopState","metadata":{"name":"state","namespace":"ns","uid":"state-uid","resourceVersion":"2","annotations":{ROOT_UID_ANNOTATION:"root-uid",TRANSACTION_ID_ANNOTATION:"bootstrap-token",COMMAND_ID_ANNOTATION:"bootstrap-token"},"ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"app","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]},"spec":{"stateVersion":1}}),
        };

        // A State CREATE response loss has no client-held issued UID. An
        // unlinked same-name object is therefore never a resume candidate.
        assert!(!has_state_link(&root_before, &state_before));

        root_before.value["metadata"]["managedFields"] = json!([{
            "manager":"kubectl-client-side-apply",
            "operation":"Apply",
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:spec":{}},
        }]);
        state_before.value["metadata"]["managedFields"] = json!([{
            "manager":"kubectl-client-side-apply",
            "operation":"Apply",
            "apiVersion":"runtime.oxideloop.dev/v1alpha1",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:spec":{}},
        }]);

        let mut state_after_finalizer = state_before.clone();
        state_after_finalizer.metadata.resource_version = "3".into();
        state_after_finalizer.value["metadata"]["resourceVersion"] = json!("3");
        state_after_finalizer.value["metadata"]["finalizers"] = json!([STATE_FINALIZER]);
        state_after_finalizer.value["metadata"]["managedFields"] = json!([{
            "manager":"oxideloop-controller",
            "operation":"Update",
            "apiVersion":"runtime.oxideloop.dev/v1alpha1",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:metadata":{"f:finalizers":{}}},
        }]);
        assert!(bootstrap_state_finalizer_proof(
            &state_after_finalizer,
            &state_before
        ));
        state_after_finalizer.value["spec"]["unexpected"] = json!(true);
        assert!(!bootstrap_state_finalizer_proof(
            &state_after_finalizer,
            &state_before
        ));

        let mut root_after_metadata = root_before.clone();
        root_after_metadata.metadata.resource_version = "2".into();
        root_after_metadata.value =
            bootstrap_root_metadata_expected_value(&root_before, &state_before, "bootstrap-token")
                .expect("v1 bootstrap authority bundle is representable");
        root_after_metadata.value["metadata"]["resourceVersion"] = json!("2");
        root_after_metadata.metadata.annotations =
            serde_json::from_value(root_after_metadata.value["metadata"]["annotations"].clone())
                .expect("metadata annotations are strings");
        root_after_metadata.value["metadata"]["managedFields"] = json!([{
            "manager":"oxideloop-controller",
            "operation":"Update",
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:metadata":{"f:annotations":{},"f:finalizers":{}}},
        }]);
        assert!(bootstrap_root_metadata_proof(
            &root_after_metadata,
            &root_before,
            &state_before,
            "bootstrap-token"
        ));
        for changed_field in [
            "spec",
            "annotation",
            "finalizer",
            "label",
            "owner",
            "deletion metadata",
            "status",
            "identity",
        ] {
            let mut root_with_unexpected_metadata = root_after_metadata.clone();
            match changed_field {
                "spec" => root_with_unexpected_metadata.value["spec"]["unexpected"] = json!(true),
                "annotation" => {
                    root_with_unexpected_metadata
                        .metadata
                        .annotations
                        .insert("foreign".into(), "changed".into());
                    root_with_unexpected_metadata.value["metadata"]["annotations"]["foreign"] =
                        json!("changed");
                }
                "finalizer" => {
                    root_with_unexpected_metadata.value["metadata"]["finalizers"] =
                        json!([ROOT_FINALIZER, "foreign"]);
                }
                "label" => {
                    root_with_unexpected_metadata.value["metadata"]["labels"] =
                        json!({"foreign":"changed"});
                }
                "owner" => {
                    root_with_unexpected_metadata.value["metadata"]["ownerReferences"] = json!([{
                        "apiVersion":"v1", "kind":"ConfigMap", "name":"foreign",
                        "uid":"foreign-uid",
                    }]);
                }
                "deletion metadata" => {
                    root_with_unexpected_metadata.value["metadata"]["deletionTimestamp"] =
                        json!("2026-08-31T00:00:00Z");
                }
                "status" => {
                    root_with_unexpected_metadata.value["status"]["observedGeneration"] = json!(1);
                }
                "identity" => {
                    root_with_unexpected_metadata.metadata.uid = "foreign-root-uid".into();
                    root_with_unexpected_metadata.value["metadata"]["uid"] =
                        json!("foreign-root-uid");
                }
                _ => unreachable!("metadata mutation fixture is exhaustive"),
            }
            assert!(
                !bootstrap_root_metadata_proof(
                    &root_with_unexpected_metadata,
                    &root_before,
                    &state_before,
                    "bootstrap-token"
                ),
                "{changed_field} is not part of the intended metadata patch"
            );
        }

        let mut root_after_link = root_after_metadata.clone();
        root_after_link.metadata.resource_version = "3".into();
        root_after_link.value["metadata"]["resourceVersion"] = json!("3");
        root_after_link.value["metadata"]["managedFields"] = json!([{
            "manager":"oxideloop-controller",
            "operation":"Update",
            "apiVersion":"demo.oxideloop.dev/v1alpha1",
            "subresource":"status",
            "fieldsType":"FieldsV1",
            "fieldsV1":{"f:status":{"f:state":{}}},
        }]);
        root_after_link.value["status"]["state"] =
            json!({"rootUid":"root-uid","name":"state","uid":"state-uid"});
        assert!(bootstrap_root_link_proof(
            &root_after_link,
            &root_after_metadata,
            &state_before
        ));
        assert!(has_state_link(&root_after_link, &state_before));
        root_after_link.value["status"]["state"]["uid"] = json!("foreign-state");
        assert!(!bootstrap_root_link_proof(
            &root_after_link,
            &root_after_metadata,
            &state_before
        ));
    }

    #[tokio::test]
    async fn finite_io_deadlines_keep_provenance_and_prewrite_never_polls_after_invalidation() {
        let budget = OperationBudgetV0::test_shortened(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
            Duration::from_millis(4),
            1,
        )
        .expect("ordered test budget");
        let gate = PrewriteGateV0::new();
        let bootstrap_fence = Arc::new(Mutex::new(BootstrapFenceStateV0::new()));
        let bootstrap_fence_wake = watch::channel(0_u64).0;
        let session = OperationSessionV0 {
            budget,
            bootstrap_pump: test_pump(&bootstrap_fence, &gate, &bootstrap_fence_wake),
            gate,
            bootstrap_fence,
            bootstrap_fence_wake,
            bootstrap: Arc::new(Mutex::new(BootstrapAuthorityV0::new())),
            collector_bound: false,
            bootstrap_phase_wait_hook: None,
        };
        assert!(session.prewrite_gate().invalidate_authenticated_change());
        assert!(matches!(
            claim_first_write(&session),
            Err(TransportErrorV0::PreWriteInvalidated)
        ));

        let budget = OperationBudgetV0::test_shortened(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
            Duration::from_millis(4),
            1,
        )
        .expect("ordered test budget");
        let gate = PrewriteGateV0::new();
        let bootstrap_fence = Arc::new(Mutex::new(BootstrapFenceStateV0::new()));
        let bootstrap_fence_wake = watch::channel(0_u64).0;
        let session = OperationSessionV0 {
            budget,
            bootstrap_pump: test_pump(&bootstrap_fence, &gate, &bootstrap_fence_wake),
            gate,
            bootstrap_fence,
            bootstrap_fence_wake,
            bootstrap: Arc::new(Mutex::new(BootstrapAuthorityV0::new())),
            collector_bound: false,
            bootstrap_phase_wait_hook: None,
        };
        assert_eq!(
            within_operation::<()>(&session, FiniteIoStageV0::WatchOpen, std::future::pending(),)
                .await,
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::WatchOpen
            ))
        );
    }

    #[tokio::test]
    async fn dropping_partial_watch_barrier_aborts_every_pump() {
        let make_watch = || {
            let (_sender, receiver) = tokio::sync::mpsc::channel(1);
            let pump = tokio::spawn(std::future::pending::<()>());
            let abort = pump.abort_handle();
            (
                WatchHandleV0 {
                    events: receiver,
                    overflowed: Arc::new(AtomicBool::new(false)),
                    terminal: None,
                    pump_abort: Some(abort),
                    deletion_filter: None,
                },
                pump,
            )
        };
        let (first_watch, first_pump) = make_watch();
        let (second_watch, second_pump) = make_watch();
        drop((first_watch, second_watch));
        assert!(first_pump.await.expect_err("pump aborted").is_cancelled());
        assert!(second_pump.await.expect_err("pump aborted").is_cancelled());
    }

    #[test]
    fn deletion_watch_filter_invalidates_only_deletion_authority_sources() {
        let gate = DeletionBarrierGateV0::new();
        let tracked_names = Arc::new(Mutex::new(BTreeMap::from([(
            ResourceKindV0::ConfigMap,
            BTreeSet::from(["tracked".to_owned()]),
        )])));
        let filter = DeletionWatchFilterV0 {
            gate: gate.clone(),
            root_uid: "root-uid".into(),
            root_name: "root".into(),
            state_name: "state".into(),
            tracked_names,
        };
        let event = |resource: ResourceKindV0, name: &str, owner_uids: Vec<String>| {
            Ok(WatchEventV0::Modified(ObjectV0 {
                reference: ObjectRefV0 {
                    resource,
                    namespace: "ns".into(),
                    name: name.into(),
                },
                metadata: ObjectMetaV0 {
                    uid: "candidate-uid".into(),
                    resource_version: "candidate-rv".into(),
                    annotations: BTreeMap::new(),
                    owner_uids,
                },
                value: json!({}),
            }))
        };
        assert!(
            !filter.relevant(
                ResourceKindV0::ConfigMap,
                &event(ResourceKindV0::ConfigMap, "unrelated", vec![])
            ),
            "proven unrelated namespace churn is ignored"
        );
        assert!(filter.relevant(
            ResourceKindV0::ConfigMap,
            &event(ResourceKindV0::ConfigMap, "tracked", vec![])
        ));
        assert!(filter.relevant(
            ResourceKindV0::Service,
            &event(ResourceKindV0::Service, "owned", vec!["root-uid".into()])
        ));
        assert!(filter.relevant(
            ResourceKindV0::Deployment,
            &event(
                ResourceKindV0::Deployment,
                "owned-deployment",
                vec!["root-uid".into()]
            )
        ));
        assert!(filter.relevant(
            ResourceKindV0::AutomationRun,
            &event(
                ResourceKindV0::AutomationRun,
                "owned-run",
                vec!["root-uid".into()]
            )
        ));
        assert!(filter.relevant(
            ResourceKindV0::WebApplication,
            &event(ResourceKindV0::WebApplication, "root", vec![])
        ));
        assert!(filter.relevant(
            ResourceKindV0::OxideLoopState,
            &event(ResourceKindV0::OxideLoopState, "state", vec![])
        ));
        assert!(filter.relevant(ResourceKindV0::Service, &Err(TransportErrorV0::WatchGone)));
        assert!(gate.invalidate(), "the first relevant record wins Live");
        assert!(
            !gate.is_live(),
            "the first relevant record wins the Live -> Invalidated CAS"
        );
        assert!(
            !gate.seal(),
            "consumer cannot acknowledge after losing Live"
        );
    }

    #[test]
    fn deletion_barrier_quiet_snapshot_seals_once() {
        let gate = DeletionBarrierGateV0::new();
        assert!(gate.is_live());
        assert!(gate.seal());
        assert!(!gate.is_live());
        assert!(
            !gate.seal(),
            "a second consumer cannot seal the same snapshot"
        );
    }

    #[test]
    fn deletion_barrier_pump_invalidation_and_consumer_seal_have_one_winner() {
        // The pump's only authority edge is `invalidate`; race it against the
        // consumer's final `seal` on the exact shared gate. Whichever CAS
        // wins permanently excludes the other capability.
        for _ in 0..128 {
            let gate = DeletionBarrierGateV0::new();
            let start = Arc::new(std::sync::Barrier::new(2));
            let pump_gate = gate.clone();
            let pump_start = Arc::clone(&start);
            let pump = std::thread::spawn(move || {
                pump_start.wait();
                pump_gate.invalidate()
            });
            start.wait();
            let sealed = gate.seal();
            let invalidated = pump.join().expect("pump thread joins");
            assert_ne!(
                invalidated, sealed,
                "only one Live transition may mint authority"
            );
            if invalidated {
                assert!(
                    !gate.seal(),
                    "invalidation winner has no snapshot capability"
                );
            } else {
                assert!(sealed, "a lost pump CAS implies the consumer sealed first");
                assert!(
                    !gate.invalidate(),
                    "a later event is outside the sealed point-in-time capability"
                );
            }
        }
    }

    #[tokio::test]
    async fn expired_deletion_barrier_session_stops_before_any_later_request() {
        use std::{convert::Infallible, sync::atomic::AtomicUsize};

        let calls = Arc::new(AtomicUsize::new(0));
        let service_calls = Arc::clone(&calls);
        let service = tower::service_fn(move |_request| {
            let service_calls = Arc::clone(&service_calls);
            async move {
                service_calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(http::Response::new(kube::client::Body::empty()))
            }
        });
        let transport = KubeTransportV0::test_from_client(kube::Client::new(service, "default"));
        let mut senders = Vec::new();
        for _ in 0..5 {
            let (sender, watch) = WatchHandleV0::test_channel(1);
            transport.test_push_watch(watch);
            senders.push(sender);
        }
        let session = test_session(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
            Duration::from_millis(4),
            0,
        );
        tokio::time::sleep(Duration::from_millis(8)).await;
        let result = transport
            .observe_deletion_revalidation_in_operation(
                &RootAddressV0 {
                    namespace: "ns".into(),
                    name: "root".into(),
                    state_name: "state".into(),
                },
                &PreconditionsV0 {
                    root_uid: "root-uid".into(),
                    root_generation: 1,
                    root_resource_version: "root-rv".into(),
                    state_uid: "state-uid".into(),
                    state_resource_version: "state-rv".into(),
                    observation_token: "token".into(),
                    cache_epoch: "epoch".into(),
                },
                &session,
            )
            .await;
        assert_eq!(
            result,
            Err(TransportErrorV0::FiniteIoDeadline(
                FiniteIoStageV0::ReadOnly
            ))
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the one exhausted barrier budget must stop before its initial LIST and every later stage"
        );
        drop(senders);
    }

    #[test]
    fn sealed_bootstrap_raw_baseline_cannot_be_rebound_by_effect_input() {
        let (session, address, command, root) = sealed_test_collected_bootstrap();
        let mut later_root = root.clone();
        later_root.metadata.resource_version = "later-rv".into();
        later_root.value["metadata"]["resourceVersion"] = json!("later-rv");

        assert_eq!(
            session.arm_collected_bootstrap_baseline(&address, &command, &later_root, None),
            Err(TransportErrorV0::ManualRecoveryHold),
            "a later effect read cannot mint a replacement bootstrap baseline"
        );
        let fence = session.bootstrap_fence.lock().expect("fence is readable");
        assert_eq!(
            fence.baseline.as_ref().map(|baseline| &baseline.root),
            Some(&root),
            "the raw collector cut remains immutable after the failed rebind"
        );
        assert!(!fence.armed);
    }

    #[tokio::test]
    async fn fresh_bootstrap_cut_rejects_list_reread_mismatch_before_any_write() {
        let listed = bootstrap_authority_root("1");
        let mut reread = listed.clone();
        reread.metadata.resource_version = "2".into();
        reread.value["metadata"]["resourceVersion"] = json!("2");
        let writes = Arc::new(AtomicUsize::new(0));
        let service = service_fn({
            let listed = listed.clone();
            let reread = reread.clone();
            let writes = Arc::clone(&writes);
            move |request: Request<Body>| {
                let listed = listed.clone();
                let reread = reread.clone();
                let writes = Arc::clone(&writes);
                async move {
                    let path = request.uri().path();
                    if request.method() == http::Method::GET && path.ends_with("/webapplications") {
                        return Ok::<_, Infallible>(test_list_response("list-rv", &[listed]));
                    }
                    if request.method() == http::Method::GET
                        && path.ends_with("/webapplications/app")
                    {
                        return Ok(test_object_response(&reread));
                    }
                    if request.method() == http::Method::GET
                        && path.ends_with("/oxideloopstates/state")
                    {
                        return Ok(test_not_found_response());
                    }
                    writes.fetch_add(1, Ordering::SeqCst);
                    panic!(
                        "the mismatched fresh cut must not issue a write: {} {path}",
                        request.method()
                    );
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let (_watch_sender, watch) = WatchHandleV0::test_channel(1);
        transport.test_push_watch(watch);
        let operation = transport.begin_collected_operation_session();
        let address = RootAddressV0 {
            namespace: "ns".into(),
            name: "app".into(),
            state_name: "state".into(),
        };

        assert!(matches!(
            transport
                .open_collected_bootstrap_cut_in_operation(
                    &address,
                    &test_bootstrap_command(),
                    &operation,
                )
                .await,
            Err(TransportErrorV0::FreshBootstrapCutSuperseded)
        ));
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fresh_bootstrap_cut_malformed_named_lists_hold_before_any_watch_or_write() {
        for (label, list) in [
            ("selector leak", {
                let mut leaked = bootstrap_authority_root("1").value;
                leaked["metadata"]["name"] = json!("foreign");
                json!({
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplicationList",
                    "metadata":{"resourceVersion":"list-rv"},
                    "items":[leaked],
                })
            }),
            (
                "empty collection resourceVersion",
                json!({
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplicationList",
                    "metadata":{"resourceVersion":""},
                    "items":[bootstrap_authority_root("1").value],
                }),
            ),
            ("duplicate exact-name objects", {
                let root = bootstrap_authority_root("1").value;
                json!({
                    "apiVersion":"demo.oxideloop.dev/v1alpha1",
                    "kind":"WebApplicationList",
                    "metadata":{"resourceVersion":"list-rv"},
                    "items":[root.clone(), root],
                })
            }),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let service = service_fn({
                let calls = Arc::clone(&calls);
                move |request: Request<Body>| {
                    let calls = Arc::clone(&calls);
                    let list = list.clone();
                    async move {
                        let path = request.uri().path();
                        if request.method() == http::Method::GET
                            && path.ends_with("/webapplications")
                        {
                            calls.fetch_add(1, Ordering::SeqCst);
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .header(http::header::CONTENT_TYPE, "application/json")
                                    .body(Body::from(
                                        serde_json::to_vec(&list).expect("list serializes"),
                                    ))
                                    .expect("list response"),
                            );
                        }
                        panic!(
                            "malformed fresh bootstrap {label} must stop before {} {path}",
                            request.method()
                        );
                    }
                }
            });
            let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
            let operation = transport.begin_collected_operation_session();
            let address = RootAddressV0 {
                namespace: "ns".into(),
                name: "app".into(),
                state_name: "state".into(),
            };

            assert!(
                matches!(
                    transport
                        .open_collected_bootstrap_cut_in_operation(
                            &address,
                            &test_bootstrap_command(),
                            &operation,
                        )
                        .await,
                    Err(TransportErrorV0::ManualRecoveryHold)
                ),
                "{label} is not a fresh-cut supersession"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "{label} must not open a watch, reread, or write"
            );
        }
    }

    #[tokio::test]
    async fn named_bootstrap_targets_reject_invalid_namespaces_and_selector_names_before_io() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = service_fn({
            let calls = Arc::clone(&calls);
            move |_request: Request<Body>| {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(Response::new(Body::empty()))
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let operation = transport.begin_operation_session();
        let invalid_names = [
            "app,metadata.name=other".to_owned(),
            "app=other".to_owned(),
            "app space".to_owned(),
            "-app".to_owned(),
            "app-".to_owned(),
            "App".to_owned(),
            "a".repeat(254),
        ];
        for name in invalid_names {
            assert!(matches!(
                transport
                    .list_named_in_operation(
                        ResourceKindV0::WebApplication,
                        "ns",
                        &name,
                        &operation,
                    )
                    .await,
                Err(TransportErrorV0::UnsupportedTarget)
            ));
            assert!(matches!(
                transport
                    .watch_named_from_in_operation(
                        ResourceKindV0::WebApplication,
                        "ns",
                        &name,
                        "rv",
                        &operation,
                    )
                    .await,
                Err(TransportErrorV0::UnsupportedTarget)
            ));
        }
        let invalid_namespaces = [
            "bad,ns".to_owned(),
            "bad=ns".to_owned(),
            "bad.ns".to_owned(),
            "-ns".to_owned(),
            "ns-".to_owned(),
            "Ns".to_owned(),
            "n".repeat(64),
        ];
        for namespace in invalid_namespaces {
            assert!(matches!(
                transport
                    .list_named_in_operation(
                        ResourceKindV0::WebApplication,
                        &namespace,
                        "app",
                        &operation,
                    )
                    .await,
                Err(TransportErrorV0::UnsupportedTarget)
            ));
            assert!(matches!(
                transport
                    .watch_named_from_in_operation(
                        ResourceKindV0::WebApplication,
                        &namespace,
                        "app",
                        "rv",
                        &operation,
                    )
                    .await,
                Err(TransportErrorV0::UnsupportedTarget)
            ));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "selector/path validation must finish before any client I/O"
        );
    }

    #[tokio::test]
    async fn named_bootstrap_list_and_watch_use_one_exact_field_selector() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let root = bootstrap_authority_root("1");
        let service = service_fn({
            let seen = Arc::clone(&seen);
            let root = root.clone();
            move |request: Request<Body>| {
                let seen = Arc::clone(&seen);
                let root = root.clone();
                async move {
                    let query = request.uri().query().unwrap_or_default().to_owned();
                    seen.lock()
                        .expect("wire log is writable")
                        .push(query.clone());
                    if query.contains("watch=true") {
                        return Ok::<_, Infallible>(
                            Response::builder()
                                .header(http::header::CONTENT_TYPE, "application/json")
                                .body(Body::empty())
                                .expect("empty watch response builds"),
                        );
                    }
                    Ok(test_list_response("list-rv", &[root]))
                }
            }
        });
        let transport = KubeTransportV0::test_from_client(Client::new(service, "default"));
        let operation = transport.begin_operation_session();
        let list = transport
            .list_named_in_operation(ResourceKindV0::WebApplication, "ns", "app", &operation)
            .await
            .expect("exact-name list opens");
        let watch = transport
            .watch_named_from_in_operation(
                ResourceKindV0::WebApplication,
                "ns",
                "app",
                &list.resource_version,
                &operation,
            )
            .await
            .expect("exact-name watch opens");
        drop(watch);
        let seen = seen.lock().expect("wire log is readable");
        assert_eq!(seen.len(), 2);
        assert!(
            seen.iter()
                .all(|query| query.contains("fieldSelector=metadata.name%3Dapp")),
            "both collection requests carry only the exact-name selector: {seen:?}"
        );
        assert!(
            seen.iter().any(|query| query.contains("watch=true")),
            "the second request is the matching watch"
        );
    }

    #[tokio::test]
    async fn bootstrap_pump_first_ready_change_delete_or_terminal_blocks_the_first_write() {
        let (session, address, command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        let mut changed = root.clone();
        changed.metadata.resource_version = "changed-rv".into();
        changed.value["metadata"]["resourceVersion"] = json!("changed-rv");
        source
            .send(Ok(WatchEventV0::Modified(changed)))
            .await
            .expect("preloaded change fits source");
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("pump installs after the ready change");
        let event = watch.next_event().await.expect("change is forwarded");
        assert!(session.observe_bootstrap_root_watch(&event));
        assert!(matches!(
            session.arm_collected_bootstrap_baseline(&address, &command, &root, None),
            Err(TransportErrorV0::PreWriteInvalidated)
        ));
        assert!(matches!(
            claim_first_write(&session),
            Err(TransportErrorV0::PreWriteInvalidated | TransportErrorV0::ManualRecoveryHold)
        ));
        let _ = session.close_collected_bootstrap_fence(&mut watch).await;

        let (session, _address, _command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        source
            .send(Ok(WatchEventV0::Deleted(root)))
            .await
            .expect("preloaded delete fits source");
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("pump installs after the ready delete");
        let event = watch.next_event().await.expect("delete is forwarded");
        assert!(!session.observe_bootstrap_root_watch(&event));
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::PreWriteInvalidated)
        );
        assert!(matches!(
            claim_first_write(&session),
            Err(TransportErrorV0::PreWriteInvalidated | TransportErrorV0::ManualRecoveryHold)
        ));
        let _ = session.close_collected_bootstrap_fence(&mut watch).await;

        let (session, _address, _command, _root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        source
            .send(Err(TransportErrorV0::WatchGone))
            .await
            .expect("preloaded terminal fits source");
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("pump installs after the ready terminal");
        assert_eq!(watch.next_event().await, Err(TransportErrorV0::WatchGone));
        session.observe_bootstrap_root_terminal(TransportErrorV0::WatchGone);
        assert_eq!(
            session.bootstrap_fence_terminal(),
            Some(TransportErrorV0::WatchGone)
        );
        assert!(matches!(
            claim_first_write(&session),
            Err(TransportErrorV0::PreWriteInvalidated | TransportErrorV0::ManualRecoveryHold)
        ));
        let _ = session.close_collected_bootstrap_fence(&mut watch).await;
    }

    #[tokio::test]
    async fn bootstrap_exact_name_leak_is_terminal_and_never_forwarded_to_the_driver() {
        let (session, _address, _command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("exact-name pump installs");
        let mut foreign = root;
        foreign.reference.namespace = "other".into();
        foreign.value["metadata"]["namespace"] = json!("other");
        source
            .send(Ok(WatchEventV0::Modified(foreign)))
            .await
            .expect("leaked record fits source");

        wait_for_bootstrap_terminal(&session, TransportErrorV0::PreWriteInvalidated).await;
        assert_eq!(
            session.prewrite_gate().state(),
            PrewriteGateStateV0::Invalidated
        );
        assert_eq!(
            watch.try_next_event(),
            Err(TransportErrorV0::WatchOverflow),
            "the leaked record is not delivered as an ordinary root event"
        );
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert_eq!((fence.committed, fence.enqueued, fence.consumed), (0, 0, 0));
        }
        let _ = session.close_collected_bootstrap_fence(&mut watch).await;
    }

    #[tokio::test]
    async fn post_close_foreign_exact_name_record_requires_a_retained_watch_rebarrier() {
        let (session, _address, _command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(2);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("exact-name pump installs");
        {
            let mut fence = session.bootstrap_fence.lock().expect("fence is writable");
            fence.armed = false;
            fence.disarmed = true;
        }
        session.gate.state.store(
            PrewriteGateV0::encode(0, PrewriteGateV0::PROVEN),
            Ordering::Release,
        );
        session
            .close_collected_bootstrap_fence(&mut watch)
            .await
            .expect("proven operation returns its retained watch before the close-boundary event");
        let mut foreign = root;
        foreign.reference.name = "other".into();
        foreign.value["metadata"]["name"] = json!("other");
        source
            .send(Ok(WatchEventV0::Modified(foreign)))
            .await
            .expect("post-close foreign record fits the returned upstream watch");
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), watch.next_event())
                .await
                .expect("bridge must consume the foreign close-boundary record"),
            Err(TransportErrorV0::WatchOverflow),
            "the next command must rebarrier instead of consuming a leaked record"
        );
        let fence = session.bootstrap_fence.lock().expect("fence is readable");
        assert_eq!((fence.committed, fence.enqueued, fence.consumed), (0, 0, 0));
        assert!(fence.disarmed);
    }

    #[tokio::test]
    async fn bootstrap_close_bridge_polls_returned_watch_before_reporting_ready() {
        let (output_sender, mut watch) = WatchHandleV0::test_channel(2);
        drop(output_sender);
        let (upstream_sender, upstream) = WatchHandleV0::test_channel(2);
        let root = bootstrap_authority_root("1");
        let address = RootAddressV0 {
            namespace: "ns".into(),
            name: "app".into(),
            state_name: "state".into(),
        };
        upstream_sender
            .send(Ok(WatchEventV0::Modified(root.clone())))
            .await
            .expect("returned watch already has a close-boundary record");

        watch
            .restore_after_bootstrap_fence(upstream, &address, Duration::from_millis(100))
            .await
            .expect("bridge confirms a real returned-watch poll before close completes");
        assert_eq!(
            watch.next_event().await,
            Ok(WatchEventV0::Modified(root)),
            "a close-boundary record is available before a later command can drain"
        );
    }

    #[tokio::test]
    async fn bootstrap_close_bridge_preserves_overflow_as_a_terminal_rebarrier() {
        let (session, _address, _command, root) = sealed_test_collected_bootstrap();
        let (source, mut watch) = WatchHandleV0::test_channel(1);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("pump installs with a bounded output");
        source
            .send(Ok(WatchEventV0::Modified(root.clone())))
            .await
            .expect("first record fills the pump output");
        source
            .send(Ok(WatchEventV0::Modified(root)))
            .await
            .expect("second record reaches the upstream source");
        wait_for_bootstrap_terminal(&session, TransportErrorV0::WatchOverflow).await;
        session
            .close_collected_bootstrap_fence(&mut watch)
            .await
            .expect("close restores the bounded retained watch");
        assert_eq!(
            watch.next_event().await,
            Err(TransportErrorV0::WatchOverflow),
            "saturation cannot be erased by the post-disarm bridge"
        );
    }

    #[test]
    fn final_state_proof_requires_exact_owner_tokens_root_uid_deletion_and_rv() {
        let initial_root = bootstrap_authority_root("1");
        let (root, state) = final_proof_fixture(&initial_root);
        assert!(final_bootstrap_state_matches(
            &root,
            &state,
            &state,
            "bootstrap-token"
        ));

        let mut owner_mutation = state.clone();
        owner_mutation.value["metadata"]["ownerReferences"][0]["uid"] = json!("other-root");
        assert!(!final_bootstrap_state_matches(
            &root,
            &owner_mutation,
            &owner_mutation,
            "bootstrap-token"
        ));

        for token_annotation in [TRANSACTION_ID_ANNOTATION, COMMAND_ID_ANNOTATION] {
            let mut token_mutation = state.clone();
            set_test_annotation(&mut token_mutation, token_annotation, "other-token");
            assert!(!final_bootstrap_state_matches(
                &root,
                &token_mutation,
                &token_mutation,
                "bootstrap-token"
            ));
        }

        let mut root_uid_mutation = state.clone();
        set_test_annotation(&mut root_uid_mutation, ROOT_UID_ANNOTATION, "other-root");
        assert!(!final_bootstrap_state_matches(
            &root,
            &root_uid_mutation,
            &root_uid_mutation,
            "bootstrap-token"
        ));

        let mut deleting_state = state.clone();
        deleting_state.value["metadata"]["deletionTimestamp"] = json!("2026-08-31T00:00:00Z");
        assert!(!final_bootstrap_state_matches(
            &root,
            &deleting_state,
            &deleting_state,
            "bootstrap-token"
        ));

        let mut rv_drift = state.clone();
        rv_drift.metadata.resource_version = "state-rv-after-finalizer".into();
        rv_drift.value["metadata"]["resourceVersion"] = json!("state-rv-after-finalizer");
        assert!(!final_bootstrap_state_matches(
            &root,
            &rv_drift,
            &state,
            "bootstrap-token"
        ));
    }

    #[tokio::test]
    async fn final_proof_consumes_one_delayed_metadata_echo_then_proves_from_exact_rereads() {
        let (session, address, command, initial_root) = sealed_test_collected_bootstrap();
        let (final_root, final_state) = final_proof_fixture(&initial_root);
        let transport = final_proof_transport(final_root.clone(), final_state.clone());
        let (source, mut watch) = WatchHandleV0::test_channel(3);
        watch
            .start_collected_bootstrap_fence(&session)
            .expect("final-proof pump installs");
        stage_final_proof(
            &session,
            &address,
            &command,
            &initial_root,
            &final_root,
            &final_state,
        )
        .await;

        let mut metadata_echo = final_root.clone();
        metadata_echo.metadata.resource_version = "root-metadata-rv".into();
        metadata_echo.value["metadata"]["resourceVersion"] = json!("root-metadata-rv");
        metadata_echo.value["status"] = json!({});
        session
            .bootstrap_fence
            .lock()
            .expect("fence is writable")
            .metadata_echo = Some(metadata_echo.clone());

        for event in [
            WatchEventV0::Modified(metadata_echo.clone()),
            WatchEventV0::Modified(final_root.clone()),
        ] {
            source.send(Ok(event)).await.expect("echo fits source");
            let observed = watch.next_event().await.expect("echo is data");
            assert!(session.observe_bootstrap_root_watch(&observed));
        }
        let (root_ref, state_ref) = address.references();
        let (root, state) = session
            .complete_collected_bootstrap_final_proof(
                &transport,
                &root_ref,
                &state_ref,
                "bootstrap-token",
            )
            .await
            .expect("one delayed metadata echo and exact rereads prove without another write");
        assert_eq!((root, state), (final_root, final_state));
        assert_eq!(session.prewrite_gate().state(), PrewriteGateStateV0::Proven);
        {
            let fence = session.bootstrap_fence.lock().expect("fence is readable");
            assert!(fence.metadata_echo_consumed && fence.final_echo_consumed);
            assert_eq!((fence.committed, fence.enqueued, fence.consumed), (2, 2, 2));
        }
        session
            .close_collected_bootstrap_fence(&mut watch)
            .await
            .expect("proven pump closes cleanly");
    }

    #[tokio::test]
    async fn final_proof_rejects_duplicate_or_after_final_metadata_echoes() {
        for after_final in [false, true] {
            let (session, address, command, initial_root) = sealed_test_collected_bootstrap();
            let (final_root, final_state) = final_proof_fixture(&initial_root);
            let (source, mut watch) = WatchHandleV0::test_channel(3);
            watch
                .start_collected_bootstrap_fence(&session)
                .expect("final-proof pump installs");
            stage_final_proof(
                &session,
                &address,
                &command,
                &initial_root,
                &final_root,
                &final_state,
            )
            .await;

            let mut metadata_echo = final_root.clone();
            metadata_echo.metadata.resource_version = "root-metadata-rv".into();
            metadata_echo.value["metadata"]["resourceVersion"] = json!("root-metadata-rv");
            metadata_echo.value["status"] = json!({});
            session
                .bootstrap_fence
                .lock()
                .expect("fence is writable")
                .metadata_echo = Some(metadata_echo.clone());

            if after_final {
                source
                    .send(Ok(WatchEventV0::Modified(final_root)))
                    .await
                    .expect("final echo fits source");
                let final_event = watch.next_event().await.expect("final echo is data");
                assert!(session.observe_bootstrap_root_watch(&final_event));
            } else {
                source
                    .send(Ok(WatchEventV0::Modified(metadata_echo.clone())))
                    .await
                    .expect("first metadata echo fits source");
                let first_metadata = watch.next_event().await.expect("metadata echo is data");
                assert!(session.observe_bootstrap_root_watch(&first_metadata));
            }
            source
                .send(Ok(WatchEventV0::Modified(metadata_echo)))
                .await
                .expect("repeated metadata echo fits source");
            let repeated = watch.next_event().await.expect("repeated echo is data");
            assert!(
                !session.observe_bootstrap_root_watch(&repeated),
                "metadata echo must be rejected after {}",
                if after_final {
                    "the final echo"
                } else {
                    "its own consumption"
                }
            );
            assert_eq!(
                session.bootstrap_fence_terminal(),
                Some(TransportErrorV0::PreWriteInvalidated)
            );
            assert_eq!(
                session.prewrite_gate().state(),
                PrewriteGateStateV0::FinalProofPending
            );
            let _ = session.close_collected_bootstrap_fence(&mut watch).await;
        }
    }

    #[tokio::test]
    async fn final_proof_rejects_baseline_added_and_forged_metadata_records() {
        for record_kind in ["baseline", "added-metadata", "forged-metadata"] {
            let (session, address, command, initial_root) = sealed_test_collected_bootstrap();
            let (final_root, final_state) = final_proof_fixture(&initial_root);
            let (source, mut watch) = WatchHandleV0::test_channel(2);
            watch
                .start_collected_bootstrap_fence(&session)
                .expect("final-proof pump installs");
            stage_final_proof(
                &session,
                &address,
                &command,
                &initial_root,
                &final_root,
                &final_state,
            )
            .await;

            let mut metadata_echo = final_root.clone();
            metadata_echo.metadata.resource_version = "root-metadata-rv".into();
            metadata_echo.value["metadata"]["resourceVersion"] = json!("root-metadata-rv");
            metadata_echo.value["status"] = json!({});
            session
                .bootstrap_fence
                .lock()
                .expect("fence is writable")
                .metadata_echo = Some(metadata_echo.clone());
            let event = match record_kind {
                "baseline" => WatchEventV0::Modified(initial_root),
                "added-metadata" => WatchEventV0::Added(metadata_echo),
                "forged-metadata" => {
                    let mut forged = metadata_echo;
                    forged.metadata.resource_version = "forged-rv".into();
                    forged.value["metadata"]["resourceVersion"] = json!("forged-rv");
                    WatchEventV0::Modified(forged)
                }
                _ => unreachable!("test table is exhaustive"),
            };
            source.send(Ok(event)).await.expect("record fits source");
            let observed = watch.next_event().await.expect("record is data");
            assert!(
                !session.observe_bootstrap_root_watch(&observed),
                "{record_kind} is not final-proof acknowledgement authority"
            );
            assert_eq!(
                session.bootstrap_fence_terminal(),
                Some(TransportErrorV0::PreWriteInvalidated)
            );
            let _ = session.close_collected_bootstrap_fence(&mut watch).await;
        }
    }

    #[tokio::test]
    async fn unlinked_deleting_v1_before_state_finalizer_waits_for_old_uid_absence_without_linking()
    {
        let (root, mut state) = unlinked_deleting_bootstrap_fixture();
        state.value["metadata"]["finalizers"] = json!([]);
        let wire = Arc::new(Mutex::new(UnlinkedCleanupWireV0 {
            root: Some(root.clone()),
            state: Some(state),
            lose_state_finalizer_response: false,
            lose_root_finalizer_response: false,
            state_finalizer_patches: 0,
            root_finalizer_patches: 0,
        }));
        let transport = cleanup_transport(Arc::clone(&wire));
        let operation = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            1,
        );

        assert!(matches!(
            transport
                .cleanup_unlinked_deleting_bootstrap_v1_once(&root.reference, &operation)
                .await,
            Ok(UnlinkedBootstrapDeletionStepV0::AwaitingStateAbsence)
        ));
        let wire = wire.lock().expect("cleanup wire is readable");
        assert_eq!(wire.state_finalizer_patches, 0);
        assert_eq!(wire.root_finalizer_patches, 0);
        assert!(
            root.value.pointer("/status/state").is_none(),
            "deleting unlinked recovery never repairs the missing StateLink"
        );
    }

    #[tokio::test]
    async fn unlinked_deleting_partial_bad_or_inconsistent_v1_bundles_hold_without_patches() {
        for state_present in [false, true] {
            for malformed in [
                "partial-version",
                "bad-version",
                "mismatched-proof-token",
                "bad-digest",
                "empty-state-uid",
                "root-uid-only",
            ] {
                let (mut root, state) = unlinked_deleting_bootstrap_fixture();
                match malformed {
                    "partial-version" => {
                        remove_test_annotation(&mut root, BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION);
                    }
                    "bad-version" => {
                        set_test_annotation(
                            &mut root,
                            BOOTSTRAP_AUTHORITY_VERSION_ANNOTATION,
                            "v2",
                        );
                    }
                    "mismatched-proof-token" => {
                        set_test_annotation(
                            &mut root,
                            BOOTSTRAP_PROOF_TOKEN_ANNOTATION,
                            "different-token",
                        );
                    }
                    "bad-digest" => {
                        set_test_annotation(
                            &mut root,
                            BOOTSTRAP_STATE_SPEC_DIGEST_ANNOTATION,
                            "not-a-domain-separated-sha256",
                        );
                    }
                    "empty-state-uid" => {
                        set_test_annotation(&mut root, BOOTSTRAP_STATE_UID_ANNOTATION, "");
                    }
                    "root-uid-only" => {
                        for key in BOOTSTRAP_AUTHORITY_ANNOTATIONS_V1 {
                            if key != ROOT_UID_ANNOTATION {
                                remove_test_annotation(&mut root, key);
                            }
                        }
                    }
                    _ => unreachable!("test case is closed"),
                }
                let wire = Arc::new(Mutex::new(UnlinkedCleanupWireV0 {
                    root: Some(root),
                    state: state_present.then_some(state),
                    lose_state_finalizer_response: false,
                    lose_root_finalizer_response: false,
                    state_finalizer_patches: 0,
                    root_finalizer_patches: 0,
                }));
                let transport = cleanup_transport(Arc::clone(&wire));
                assert_eq!(
                    transport
                        .cleanup_unlinked_deleting_bootstrap_v1("ns", "app")
                        .await,
                    Err(TransportErrorV0::ManualRecoveryHold),
                    "{malformed} bundle must hold whether the exact State is present or absent"
                );
                let wire = wire.lock().expect("cleanup wire is readable");
                assert_eq!(
                    (wire.state_finalizer_patches, wire.root_finalizer_patches),
                    (0, 0),
                    "{malformed} bundle must not patch either object"
                );
            }
        }
    }

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
    fn v1_deployment_admission_and_projection_are_sealed_to_the_generated_shape() {
        let expected = ObjectRefV0 {
            resource: ResourceKindV0::Deployment,
            namespace: "demo".into(),
            name: "portal".into(),
        };
        let desired = json!({
            "apiVersion":"apps/v1",
            "kind":"Deployment",
            "metadata":{"name":"portal","namespace":"demo","labels":{"app":"portal"}},
            "spec":{
                "replicas":2,
                "selector":{"matchLabels":{"app":"portal"}},
                "template":{
                    "metadata":{"labels":{"app":"portal"}},
                    "spec":{
                        "securityContext":{"runAsNonRoot":true},
                        "containers":[{
                            "name":"web",
                            "image":"ghcr.io/example/web:v1",
                            "ports":[{"name":"http","containerPort":8080}],
                            "env":[{"name":"MESSAGE","valueFrom":{"configMapKeyRef":{"name":"portal-config","key":"message"}}}],
                            "resources":{"requests":{"cpu":"10m","memory":"32Mi"},"limits":{"memory":"64Mi"}}
                        }]
                    }
                }
            }
        });
        assert_eq!(validate_desired_object(&desired, &expected), Ok(()));

        let mut observed = desired.clone();
        observed["metadata"]["uid"] = json!("deployment-uid");
        observed["metadata"]["resourceVersion"] = json!("99");
        observed["metadata"]["annotations"] = json!({"deployment.kubernetes.io/revision":"4"});
        observed["status"] = json!({"readyReplicas":2});
        observed["spec"]["progressDeadlineSeconds"] = json!(600);
        observed["spec"]["revisionHistoryLimit"] = json!(10);
        observed["spec"]["strategy"] = json!({"type":"RollingUpdate","rollingUpdate":{"maxSurge":"25%","maxUnavailable":"25%"}});
        observed["spec"]["template"]["spec"]["dnsPolicy"] = json!("ClusterFirst");
        observed["spec"]["template"]["spec"]["restartPolicy"] = json!("Always");
        observed["spec"]["template"]["spec"]["schedulerName"] = json!("default-scheduler");
        observed["spec"]["template"]["spec"]["terminationGracePeriodSeconds"] = json!(30);
        observed["spec"]["template"]["spec"]["enableServiceLinks"] = json!(true);
        observed["spec"]["template"]["spec"]["containers"][0]["imagePullPolicy"] =
            json!("IfNotPresent");
        observed["spec"]["template"]["spec"]["containers"][0]["terminationMessagePath"] =
            json!("/dev/termination-log");
        observed["spec"]["template"]["spec"]["containers"][0]["terminationMessagePolicy"] =
            json!("File");
        observed["spec"]["template"]["spec"]["containers"][0]["ports"][0]["protocol"] =
            json!("TCP");
        observed["metadata"]["managedFields"] = json!([{
            "manager":"oxideloop-controller", "operation":"Apply", "fieldsType":"FieldsV1",
            "apiVersion":"apps/v1", "fieldsV1":fields_v1_for(&desired, "")
        }]);
        assert!(same_desired_semantics(&observed, &desired));
        observed["spec"]["template"]["spec"]["containers"][0]["image"] = json!("other");
        assert!(!same_desired_semantics(&observed, &desired));

        for mutate in [
            |value: &mut Value| value["spec"]["unexpected"] = json!(true),
            |value: &mut Value| value["spec"]["selector"]["matchLabels"] = json!({"app":"other"}),
            |value: &mut Value| {
                value["spec"]["template"]["spec"]["containers"][0]["command"] = json!(["sh"]);
            },
            |value: &mut Value| {
                value["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"] =
                    json!("smuggled");
            },
            |value: &mut Value| {
                value["spec"]["template"]["spec"]["containers"][0]["resources"]["limits"]["nvidia.com/gpu"] =
                    json!(1);
            },
        ] {
            let mut invalid = desired.clone();
            mutate(&mut invalid);
            assert_eq!(
                validate_desired_object(&invalid, &expected),
                Err(TransportErrorV0::UnsupportedTarget)
            );
        }
    }

    #[test]
    fn managed_fields_projection_requires_complete_unambiguous_controller_coverage() {
        let desired = json!({
            "apiVersion":"v1", "kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"demo","labels":{"owned":"yes"}},
            "data":{"message":"hello"}
        });
        let fields = json!({
            "f:metadata":{"f:labels":{"f:owned":{}}},
            "f:data":{"f:message":{}}
        });
        let observed = |managed_fields: Value| {
            json!({
                "apiVersion":"v1", "kind":"ConfigMap",
                "metadata":{"name":"config","namespace":"demo","labels":{"owned":"yes","co-writer":"kept"},"managedFields":managed_fields},
                "data":{"message":"hello","co-writer":"kept"}
            })
        };
        let entry = |fields: Value| {
            json!({
                "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1",
                "apiVersion":"v1", "fieldsV1":fields
            })
        };

        assert!(same_desired_semantics(
            &observed(json!([
                entry(json!({"f:metadata":{"f:labels":{"f:owned":{}}}})),
                entry(json!({"f:data":{"f:message":{}}}))
            ])),
            &desired
        ));
        assert!(!same_desired_semantics(
            &observed(json!([entry(json!({"f:metadata":{}}))])),
            &desired
        ));
        assert!(!same_desired_semantics(
            &observed(json!([{
                "manager":"other", "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"v1", "fieldsV1":fields
            }])),
            &desired
        ));
        assert!(!same_desired_semantics(
            &observed(json!([{
                "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"v2", "fieldsV1":fields
            }])),
            &desired
        ));
        assert!(!same_desired_semantics(
            &observed(json!([{
                "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"v1", "subresource":"status", "fieldsV1":fields
            }])),
            &desired
        ));
        assert!(!same_desired_semantics(
            &observed(json!([{
                "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"v1", "fieldsV1":{"bad":{}}
            }])),
            &desired
        ));
    }

    #[test]
    fn managed_fields_omission_uses_raw_values_before_default_canonicalization() {
        let desired = json!({
            "apiVersion":"v1", "kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"demo","labels":{"wanted":"yes"}},
            "data":{"wanted":"yes"}
        });
        let mut observed = desired.clone();
        observed["metadata"]["labels"]["retired"] = json!("remove");
        observed["data"]["retired"] = json!("remove");
        observed["metadata"]["managedFields"] = json!([{
            "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"v1",
            "fieldsV1":{"f:metadata":{"f:labels":{"f:wanted":{},"f:retired":{}}},"f:data":{"f:wanted":{},"f:retired":{}}}
        }]);
        assert!(!same_desired_semantics(&observed, &desired));
    }

    #[test]
    fn associative_lists_ignore_unowned_co_writer_members() {
        let desired = json!({
            "apiVersion":"apps/v1", "kind":"Deployment",
            "metadata":{"name":"web","namespace":"demo"},
            "spec":{"replicas":1,"template":{"spec":{"containers":[{
                "name":"web", "image":"example/web",
                "env":[{"name":"MESSAGE","value":"hello"}],
                "ports":[{"containerPort":8080}]
            }]}}}
        });
        let mut observed = desired.clone();
        observed["spec"]["template"]["spec"]["containers"].as_array_mut().unwrap().push(json!({
            "name":"sidecar", "image":"example/sidecar", "env":[{"name":"EXTRA","value":"kept"}], "ports":[{"containerPort":9090}]
        }));
        observed["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"EXTRA","value":"kept"}));
        observed["spec"]["template"]["spec"]["containers"][0]["ports"]
            .as_array_mut()
            .unwrap()
            .push(json!({"containerPort":9090}));
        observed["metadata"]["managedFields"] = json!([{
            "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"apps/v1",
            "fieldsV1":fields_v1_for(&desired, "")
        }]);
        assert!(same_desired_semantics(&observed, &desired));
    }

    #[test]
    fn captured_service_fields_ignore_kubernetes_defaults_and_tcp_selector_default() {
        let desired = json!({
            "apiVersion":"v1", "kind":"Service", "metadata":{"name":"web","namespace":"demo"},
            "spec":{"selector":{"app":"web"},"ports":[{"name":"http","port":80,"targetPort":80}]}
        });
        let mut observed = desired.clone();
        observed["spec"]["type"] = json!("ClusterIP");
        observed["spec"]["sessionAffinity"] = json!("None");
        observed["spec"]["internalTrafficPolicy"] = json!("Cluster");
        observed["spec"]["ports"][0]["protocol"] = json!("TCP");
        observed["metadata"]["managedFields"] = json!([{
            "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"v1",
            "fieldsV1":{"f:spec":{"f:selector":{},"f:ports":{"k:{\"port\":80,\"protocol\":\"TCP\"}":{".":{},"f:name":{},"f:port":{},"f:targetPort":{}}}}}
        }]);
        assert!(same_desired_semantics(&observed, &desired));
        observed["spec"]["selector"]["app"] = json!("other");
        assert!(!same_desired_semantics(&observed, &desired));
    }

    #[test]
    fn prior_owned_associative_env_and_port_removals_do_not_match() {
        let desired = json!({
            "apiVersion":"apps/v1", "kind":"Deployment", "metadata":{"name":"web","namespace":"demo"},
            "spec":{"replicas":1,"template":{"spec":{"containers":[{"name":"web","image":"example/web","env":[{"name":"KEEP","value":"yes"}],"ports":[{"containerPort":8080}]}]}}}
        });
        let mut observed = desired.clone();
        observed["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"RETIRED","value":"remove"}));
        observed["spec"]["template"]["spec"]["containers"][0]["ports"]
            .as_array_mut()
            .unwrap()
            .push(json!({"containerPort":9090}));
        observed["metadata"]["managedFields"] = json!([{
            "manager":CONTROLLER_FIELD_MANAGER, "operation":"Apply", "fieldsType":"FieldsV1", "apiVersion":"apps/v1",
            "fieldsV1":fields_v1_for(&observed, "")
        }]);
        assert!(!same_desired_semantics(&observed, &desired));
    }

    #[test]
    fn captured_deployment_fields_require_our_owned_child_not_a_co_writer() {
        let desired = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","namespace":"demo","labels":{"app":"web"}},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"web"}},"template":{"metadata":{"labels":{"app":"web"}},"spec":{"containers":[{"name":"web","image":"example/web","env":[{"name":"MESSAGE","value":"hello"}],"ports":[{"name":"http","containerPort":8080}]}]}}}});
        let fields: Value = serde_json::from_str(r#"
        {
          "f:metadata": {"f:labels": {"f:app": {}}},
          "f:spec": {
            "f:replicas": {}, "f:selector": {},
            "f:template": {
              "f:metadata": {"f:labels": {"f:app": {}}},
              "f:spec": {"f:containers": {
                "k:{\"name\":\"web\"}": {
                  ".": {}, "f:image": {}, "f:name": {},
                  "f:env": {"k:{\"name\":\"MESSAGE\"}": {".": {}, "f:name": {}, "f:value": {}}},
                  "f:ports": {"k:{\"containerPort\":8080,\"protocol\":\"TCP\"}": {".": {}, "f:containerPort": {}, "f:name": {}}}
                }
              }}
            }
          }
        }
        "#).expect("captured fields JSON");
        let mut observed = desired.clone();
        observed["metadata"]["managedFields"] = json!([{"manager":CONTROLLER_FIELD_MANAGER,"operation":"Apply","fieldsType":"FieldsV1","apiVersion":"apps/v1","fieldsV1":fields.clone()}]);
        assert!(same_desired_semantics(&observed, &desired));
        // A matching value claimed only by another manager cannot establish
        // our projection equality after our leaf ownership disappears.
        let mut ours = fields;
        ours["f:spec"]["f:template"]["f:spec"]["f:containers"]["k:{\"name\":\"web\"}"]["f:image"] =
            Value::Null;
        ours["f:spec"]["f:template"]["f:spec"]["f:containers"]["k:{\"name\":\"web\"}"]
            .as_object_mut()
            .unwrap()
            .remove("f:image");
        observed["metadata"]["managedFields"] = json!([
            {"manager":CONTROLLER_FIELD_MANAGER,"operation":"Apply","fieldsType":"FieldsV1","apiVersion":"apps/v1","fieldsV1":ours},
            {"manager":"co-writer","operation":"Apply","fieldsType":"FieldsV1","apiVersion":"apps/v1","fieldsV1":{"f:spec":{"f:template":{"f:spec":{"f:containers":{"k:{\"name\":\"web\"}":{"f:image":{}}}}}}}}
        ]);
        assert!(!same_desired_semantics(&observed, &desired));
    }

    #[test]
    fn deployment_response_loss_recovers_only_the_same_uid_projected_deployment() {
        let root = ObjectV0 {
            reference: ObjectRefV0 {
                resource: ResourceKindV0::WebApplication,
                namespace: "demo".into(),
                name: "portal".into(),
            },
            metadata: ObjectMetaV0 {
                uid: "root-uid".into(),
                resource_version: "1".into(),
                annotations: BTreeMap::new(),
                owner_uids: vec![],
            },
            value: json!({}),
        };
        let reference = ObjectRefV0 {
            resource: ResourceKindV0::Deployment,
            namespace: "demo".into(),
            name: "portal".into(),
        };
        let proof = json!({
            "oxideloop.dev/root-uid":"root-uid",
            "oxideloop.dev/transaction-id":"tx",
            "oxideloop.dev/command-id":"command"
        });
        let body = json!({
            "apiVersion":"apps/v1", "kind":"Deployment",
            "metadata":{
                "name":"portal", "namespace":"demo", "labels":{"app":"portal"},
                "annotations":proof,
                "ownerReferences":[{"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","name":"portal","uid":"root-uid","controller":true,"blockOwnerDeletion":false}]
            },
            "spec":{"selector":{"matchLabels":{"app":"portal"}},"template":{"metadata":{"labels":{"app":"portal"}},"spec":{"containers":[{"name":"web","image":"ghcr.io/example/web:v1"}]}}}
        });
        let desired = ObjectV0 {
            reference: reference.clone(),
            metadata: ObjectMetaV0 {
                uid: "deployment-uid".into(),
                resource_version: "1".into(),
                annotations: BTreeMap::from([
                    (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
                    (TRANSACTION_ID_ANNOTATION.into(), "tx".into()),
                    (COMMAND_ID_ANNOTATION.into(), "command".into()),
                ]),
                owner_uids: vec!["root-uid".into()],
            },
            value: body.clone(),
        };
        let mut observed = desired.clone();
        observed.metadata.resource_version = "2".into();
        observed.value["metadata"]["resourceVersion"] = json!("2");
        observed.value["metadata"]["annotations"]["deployment.kubernetes.io/revision"] = json!("1");
        observed.value["status"] = json!({"readyReplicas":1});
        observed.value["spec"]["replicas"] = json!(1);
        observed.value["metadata"]["managedFields"] = json!([{
            "manager":"oxideloop-controller", "operation":"Apply", "fieldsType":"FieldsV1",
            "apiVersion":"apps/v1", "fieldsV1":fields_v1_for(&desired.value, "")
        }]);
        assert!(
            recover_dropped_write_exact(
                Some(&observed),
                &desired,
                &root,
                Some("deployment-uid"),
                "tx",
                "command"
            )
            .is_ok()
        );

        let mut wrong_uid = observed.clone();
        wrong_uid.metadata.uid = "replacement".into();
        assert!(
            recover_dropped_write_exact(
                Some(&wrong_uid),
                &desired,
                &root,
                Some("deployment-uid"),
                "tx",
                "command"
            )
            .is_err()
        );
        let mut drift = observed;
        drift.value["spec"]["template"]["spec"]["containers"][0]["image"] = json!("other");
        assert!(
            recover_dropped_write_exact(
                Some(&drift),
                &desired,
                &root,
                Some("deployment-uid"),
                "tx",
                "command"
            )
            .is_err()
        );
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
                pump_abort: None,
                deletion_filter: None,
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
    fn retired_state_delete_is_an_effect_fenced_command() {
        assert!(command_requires_effect_fence(
            &CommandV0::DeleteRetiredState {
                token: "retired-delete".into(),
                preconditions: PreconditionsV0 {
                    root_uid: "root-uid".into(),
                    root_generation: 1,
                    root_resource_version: "9".into(),
                    state_uid: "state-uid".into(),
                    state_resource_version: "4".into(),
                    observation_token: "observation".into(),
                    cache_epoch: "epoch".into(),
                },
            }
        ));
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
                    verbs: &["get", "list", "watch", "create", "patch", "delete"]
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
                    resource: ResourceKindV0::Deployment,
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
