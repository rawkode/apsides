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

#[cfg(any(test, feature = "test-support"))]
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use oxideloop_controller_api::{
    ApplyOperation, ApplyOutcome, ApplyResult, AutomationPending, AutomationRunCompletionState,
    AutomationRunSpec, AutomationRunStatus, DeletionCleanupReceipt, DurablePhase,
    MAX_MESSAGE_BYTES, NextCommandLifecycleStateV0, ObjectIdentity, OxideLoopStateSpec,
    PersistPurpose, ROOT_FINALIZER, STATE_FINALIZER, StateLink, WebApplicationStatus,
    validate_automation_run_name,
};
use oxideloop_controller_core::{
    CommandV0, MutationCommandV0, PreconditionsV0, RootSnapshotV0, root_finalizer_command_token,
    root_only_owned_children_verify_token,
};
#[cfg(test)]
use oxideloop_event_planner::semantically_equal;
use oxideloop_event_planner::{PlannedEventV0, materialize_event};
use oxideloop_mutation_model::{
    APPLY_PATCH_CONTENT_TYPE, CONTROLLER_FIELD_MANAGER, HttpMethod, MutationDecision,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;

use kube::{
    Api, Client, Config,
    api::{DeleteParams, ListParams, Patch, PatchParams, Preconditions},
    core::{ApiResource, DynamicObject},
};

pub const ROOT_UID_ANNOTATION: &str = "runtime.oxideloop.dev/root-uid";
pub const TRANSACTION_ID_ANNOTATION: &str = "runtime.oxideloop.dev/transaction-id";
pub const COMMAND_ID_ANNOTATION: &str = "runtime.oxideloop.dev/command-id";
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
    PreWrite,
    RootAuthPending,
    Invalidated,
    WritePolled,
    Proven,
    AmbiguousHeld,
    BootstrapPhasePending,
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

    /// A duplicate of an already-consumed bootstrap metadata echo is stale
    /// transport noise, not new root authority.  Re-open only the current
    /// epoch's phase barrier: unlike the first echo, it must not mint another
    /// retry epoch or another opportunity to poll a root-status write.
    #[doc(hidden)]
    #[must_use]
    pub fn resume_after_duplicate_owned_root_echo(&self, epoch: u32) -> bool {
        self.transition_epoch(
            epoch,
            Self::ROOT_AUTH_PENDING,
            Self::BOOTSTRAP_PHASE_PENDING,
        )
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
    StateFinalizer,
    RootMetadata,
    RootStateLink,
    Complete,
}

#[derive(Debug)]
struct BootstrapRootEchoV0 {
    uid: String,
    namespace: String,
    name: String,
    generation: i64,
    spec: oxideloop_controller_api::WebApplicationSpec,
    status: WebApplicationStatus,
    resource_version: String,
    root_finalizer_present: bool,
    deleting: bool,
}

#[derive(Debug)]
struct BootstrapOwnedRootEchoV0 {
    echo: BootstrapRootEchoV0,
    /// Keep the actual metadata-patch response, not merely its status-shaped
    /// projection.  `RootStateLink` may proceed only from this exact response;
    /// an annotation, finalizer, status, or State-link modification changes
    /// the object and therefore cannot inherit root authority.
    response: ObjectV0,
    phase: BootstrapRootPhaseV0,
    epoch: u32,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapRootEchoAuthenticationV0 {
    ExactFirstUse,
    ExactDuplicate,
    GenuineOrAmbiguous,
}

impl BootstrapRootEchoV0 {
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
            status: serde_json::from_value(
                root.value
                    .get("status")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            )
            .map_err(|_| TransportErrorV0::Stale("bootstrap root status"))?,
            resource_version: root.metadata.resource_version.clone(),
            root_finalizer_present: has_finalizer(root, ROOT_FINALIZER),
            deleting: root
                .value
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty())),
        })
    }

    fn matches_snapshot(&self, fresh: &RootSnapshotV0) -> bool {
        fresh.uid == self.uid
            && fresh.namespace == self.namespace
            && fresh.name == self.name
            && fresh.generation == self.generation
            && fresh.spec == self.spec
            && fresh.status == self.status
            && fresh.root_resource_version == self.resource_version
            && fresh.root_finalizer_present == self.root_finalizer_present
            && fresh.deleting == self.deleting
    }
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
        let echo = BootstrapRootEchoV0::from_root(root)?;
        Ok(Self {
            uid: echo.uid,
            namespace: echo.namespace,
            name: echo.name,
            generation: echo.generation,
            spec: echo.spec,
            deleting: echo.deleting,
        })
    }

    fn matches(&self, root: &ObjectV0) -> Result<bool, TransportErrorV0> {
        let current = BootstrapRootEchoV0::from_root(root)?;
        Ok(current.uid == self.uid
            && current.namespace == self.namespace
            && current.name == self.name
            && current.generation == self.generation
            && current.spec == self.spec
            && current.deleting == self.deleting)
    }
}

#[derive(Debug)]
struct BootstrapAuthorityV0 {
    active: bool,
    phase: Option<BootstrapRootPhaseV0>,
    frozen_root: Option<BootstrapFrozenRootV0>,
    /// A root response can become a watch record at the next subwrite
    /// boundary.  Only this exact response is safe to consume there.
    exact_owned_root_echo: Option<BootstrapOwnedRootEchoV0>,
    /// The first exact metadata echo is consumed atomically.  Retain its
    /// identity only to discard a duplicate delivery without granting a
    /// second restart or reusing it for a later phase.
    consumed_owned_root_echo: Option<BootstrapOwnedRootEchoV0>,
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
            frozen_root: None,
            exact_owned_root_echo: None,
            consumed_owned_root_echo: None,
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
        Self {
            budget,
            gate: PrewriteGateV0::new(),
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
            && Arc::ptr_eq(&self.bootstrap, &other.bootstrap)
            && self.collector_bound == other.collector_bound
    }

    fn begin_bootstrap_sequence(&self) -> Result<(), TransportErrorV0> {
        let mut authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        // A collector may authenticate one exact owned root echo by
        // restarting the *same* frozen EnsureBootstrap command.  That is not
        // a second bootstrap owner and must retain, rather than reset, its
        // phase witness.
        authority.active = true;
        Ok(())
    }

    fn is_bootstrap_sequence(&self) -> bool {
        self.bootstrap
            .lock()
            .is_ok_and(|authority| authority.active)
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
                | PrewriteGateStateV0::RootAuthPending => {
                    let _ = notified.changed().await;
                }
                PrewriteGateStateV0::WritePolled
                | PrewriteGateStateV0::Proven
                | PrewriteGateStateV0::AmbiguousHeld => {
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
        authority.phase = Some(phase);
        // A phase must never accept an echo from a future or unrelated root
        // write.  The immediately preceding direct root response is retained
        // only when moving into the next root-writing phase.
        if !matches!(
            phase,
            BootstrapRootPhaseV0::RootStateLink | BootstrapRootPhaseV0::Complete
        ) {
            authority.exact_owned_root_echo = None;
            authority.consumed_owned_root_echo = None;
        }
        Ok(())
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
            echo: BootstrapRootEchoV0::from_root(root)?,
            response: root.clone(),
            phase: next_phase,
            epoch: self.gate.epoch(),
        });
        authority.consumed_owned_root_echo = None;
        authority.phase = Some(next_phase);
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

    fn bootstrap_issued_state_matches(&self, state: &ObjectV0) -> bool {
        self.bootstrap.lock().is_ok_and(|authority| {
            authority.active
                && authority
                    .issued_state
                    .as_ref()
                    .is_some_and(|(reference, uid)| {
                        reference == &state.reference && uid == &state.metadata.uid
                    })
        })
    }

    /// Atomically consumes the one exact root response that bootstrap has
    /// proven for the supplied phase epoch. A duplicate delivery of that
    /// response is explicitly identified, but can never reopen another epoch.
    #[doc(hidden)]
    #[must_use]
    pub fn authenticate_bootstrap_root_echo(
        &self,
        fresh: &RootSnapshotV0,
        epoch: u32,
    ) -> BootstrapRootEchoAuthenticationV0 {
        let Ok(mut authority) = self.bootstrap.lock() else {
            return BootstrapRootEchoAuthenticationV0::GenuineOrAmbiguous;
        };
        if !authority.active || authority.phase != Some(BootstrapRootPhaseV0::RootStateLink) {
            return BootstrapRootEchoAuthenticationV0::GenuineOrAmbiguous;
        }
        if authority
            .exact_owned_root_echo
            .as_ref()
            .is_some_and(|echo| {
                echo.phase == BootstrapRootPhaseV0::RootStateLink
                    && echo.epoch == epoch
                    && echo.echo.matches_snapshot(fresh)
            })
        {
            let echo = authority
                .exact_owned_root_echo
                .take()
                .expect("exact root echo was just observed");
            authority.consumed_owned_root_echo = Some(echo);
            return BootstrapRootEchoAuthenticationV0::ExactFirstUse;
        }
        if authority
            .consumed_owned_root_echo
            .as_ref()
            .is_some_and(|echo| {
                echo.phase == BootstrapRootPhaseV0::RootStateLink
                    && echo.echo.matches_snapshot(fresh)
            })
        {
            BootstrapRootEchoAuthenticationV0::ExactDuplicate
        } else {
            BootstrapRootEchoAuthenticationV0::GenuineOrAmbiguous
        }
    }

    fn require_exact_root_metadata_response(
        &self,
        root: &ObjectV0,
    ) -> Result<(), TransportErrorV0> {
        let authority = self
            .bootstrap
            .lock()
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?;
        if !authority.active || authority.phase != Some(BootstrapRootPhaseV0::RootStateLink) {
            return Err(TransportErrorV0::Stale("bootstrap root metadata authority"));
        }
        let exact = authority
            .exact_owned_root_echo
            .as_ref()
            .or(authority.consumed_owned_root_echo.as_ref());
        if exact.is_some_and(|echo| {
            echo.phase == BootstrapRootPhaseV0::RootStateLink && echo.response == *root
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
            authority.active && authority.phase == Some(BootstrapRootPhaseV0::Complete)
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
        if !authority.active || authority.phase != Some(BootstrapRootPhaseV0::RootStateLink) {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        authority.phase = Some(BootstrapRootPhaseV0::Complete);
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
            | PrewriteGateStateV0::BootstrapPhasePending => TransportErrorV0::ManualRecoveryHold,
        })?;
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

const DELETION_BARRIER_LIVE: u8 = 0;
const DELETION_BARRIER_INVALIDATED: u8 = 1;
const DELETION_BARRIER_SEALED: u8 = 2;

/// A private point-in-time seal shared by deletion watch pumps and their
/// consumer.  A delivered relevant record wins before publication; a consumer
/// that loses `Live -> Sealed` has no acknowledgement capability.
#[derive(Clone)]
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
#[derive(Clone)]
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
            ResourceKindV0::ConfigMap | ResourceKindV0::Service | ResourceKindV0::AutomationRun => {
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
    pump_abort: Option<tokio::task::AbortHandle>,
    // Test-injected handles do not have a kube stream pump.  Keep the same
    // invalidation edge at the handle boundary so the production deletion
    // barrier is exercised with the only substituted component being its I/O
    // source.
    deletion_filter: Option<(ResourceKindV0, DeletionWatchFilterV0)>,
}

impl WatchHandleV0 {
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
        self.invalidate_deletion_filter(&event);
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
                self.invalidate_deletion_filter(&event);
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
                    session,
                )
                .await
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
                let written = self
                    .remove_finalizer_in_operation(&fence.state, STATE_FINALIZER, session)
                    .await
                    .map_err(|_| ambiguous_delete_or_finalizer())?;
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
                let fence = self
                    .reread_fence_in_operation(address, preconditions, session)
                    .await?;
                let written = self
                    .remove_finalizer_in_operation(&fence.root, ROOT_FINALIZER, session)
                    .await
                    .map_err(|_| ambiguous_delete_or_finalizer())?;
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
        token: &str,
        state: &OxideLoopStateSpec,
        root_uid: &str,
        root_generation: i64,
        root_rv: &str,
        state_name: &str,
        session: &OperationSessionV0,
    ) -> Result<CommandOutcomeV0, TransportErrorV0> {
        if address.state_name != state_name {
            return Err(TransportErrorV0::IdentityMismatch);
        }
        let (root_ref, state_ref) = address.references();
        let initial_root = self
            .reread_prewrite_in_operation(&root_ref, session)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        if initial_root.metadata.uid != root_uid
            || generation(&initial_root)? != root_generation
            || initial_root
                .value
                .pointer("/metadata/deletionTimestamp")
                .is_some_and(|value| value.as_str().is_some_and(|value| !value.is_empty()))
            || (initial_root.metadata.resource_version != root_rv
                && !has_bootstrap_root_metadata(&initial_root, token))
        {
            return Err(TransportErrorV0::Stale("bootstrap root pins"));
        }
        // A pre-existing same-name state has no create witness. It is safe to
        // continue only when the root already durably links this exact UID;
        // otherwise this is a collision, never an adoption candidate.
        let mut state = match self
            .reread_prewrite_in_operation(&state_ref, session)
            .await?
        {
            None => {
                // The state existence read is intentionally *before* this
                // root fence.  The POST gets no root resource-version
                // precondition, so its only valid authority is this immediate
                // epoch-bound root preflight plus the shared write lease.
                let root = self
                    .bootstrap_root_preflight(
                        &root_ref,
                        root_uid,
                        root_generation,
                        root_rv,
                        token,
                        BootstrapRootPhaseV0::StatePost,
                        session,
                    )
                    .await?;
                let desired = bootstrap_state(&state_ref, state, &root, token)?;
                match self.create_state_in_operation(&desired, session).await {
                    Ok(created) => {
                        let created =
                            recover_bootstrap_state(Some(created), &desired, &root, token)?;
                        session.record_bootstrap_issued_state(&created)?;
                        created
                    }
                    // Kubernetes assigns the State UID.  Once the CREATE response
                    // is lost there is no client-held issued UID to bind a later
                    // same-name object to this command.  In particular, a raw
                    // reread of that name would be adoption, not proof.  A later
                    // bootstrap invocation may resume only through the normal
                    // pre-existing-state path below, after the root durably links
                    // that exact State UID.
                    Err(error) if bootstrap_write_response_lost(&error) => {
                        return Err(TransportErrorV0::ManualRecoveryHold);
                    }
                    Err(TransportErrorV0::Conflict(ConflictProvenanceV0::Write {
                        operation: WriteOperationV0::Create,
                        resource: ResourceKindV0::OxideLoopState,
                    })) => {
                        return Err(TransportErrorV0::ForeignCollision);
                    }
                    Err(error) => return Err(error),
                }
            }
            Some(observed) => {
                if !has_state_link(&initial_root, &observed)
                    && !session.bootstrap_issued_state_matches(&observed)
                {
                    return Err(TransportErrorV0::ForeignCollision);
                }
                let desired = bootstrap_state(&state_ref, state, &initial_root, token)?;
                recover_bootstrap_state(Some(observed), &desired, &initial_root, token)?
            }
        };
        if !has_finalizer(&state, STATE_FINALIZER) {
            self.bootstrap_root_preflight(
                &root_ref,
                root_uid,
                root_generation,
                root_rv,
                token,
                BootstrapRootPhaseV0::StateFinalizer,
                session,
            )
            .await?;
            let before = state.clone();
            state = match self
                .ensure_finalizer(&before, STATE_FINALIZER, session)
                .await
            {
                Ok(written) if bootstrap_state_finalizer_proof(&written, &before) => written,
                Err(error) if bootstrap_write_response_lost(&error) => {
                    let proof = self.bootstrap_recovery_get(&state_ref, session).await?;
                    if bootstrap_state_finalizer_proof(&proof, &before) {
                        session.prove_bootstrap_after_exact_proof()?;
                        proof
                    } else {
                        return Err(TransportErrorV0::ManualRecoveryHold);
                    }
                }
                Ok(_) | Err(_) => return Err(TransportErrorV0::ManualRecoveryHold),
            };
        }
        let root = self
            .reread_prewrite_in_operation(&root_ref, session)
            .await?
            .ok_or(TransportErrorV0::Stale("root absent"))?;
        let root = if has_bootstrap_root_metadata(&root, token) {
            root
        } else {
            let before = self
                .bootstrap_root_preflight(
                    &root_ref,
                    root_uid,
                    root_generation,
                    root_rv,
                    token,
                    BootstrapRootPhaseV0::RootMetadata,
                    session,
                )
                .await?;
            match self.bootstrap_root_metadata(&before, token, session).await {
                Ok(written) if bootstrap_root_metadata_proof(&written, &before, token) => {
                    session.record_bootstrap_owned_root_echo(
                        &written,
                        BootstrapRootPhaseV0::RootStateLink,
                    )?;
                    written
                }
                Err(error) if bootstrap_write_response_lost(&error) => {
                    let proof = self.bootstrap_recovery_get(&root_ref, session).await?;
                    if bootstrap_root_metadata_proof(&proof, &before, token) {
                        session.prove_bootstrap_after_exact_proof()?;
                        session.record_bootstrap_owned_root_echo(
                            &proof,
                            BootstrapRootPhaseV0::RootStateLink,
                        )?;
                        proof
                    } else {
                        return Err(TransportErrorV0::ManualRecoveryHold);
                    }
                }
                Ok(_) | Err(_) => return Err(TransportErrorV0::ManualRecoveryHold),
            }
        };
        if !has_state_link(&root, &state) {
            let before = self
                .bootstrap_root_preflight(
                    &root_ref,
                    root_uid,
                    root_generation,
                    root_rv,
                    token,
                    BootstrapRootPhaseV0::RootStateLink,
                    session,
                )
                .await?;
            let linked = match self.bootstrap_root_status(&before, &state, session).await {
                Ok(written) if bootstrap_root_link_proof(&written, &before, &state) => {
                    session.record_bootstrap_owned_root_echo(
                        &written,
                        BootstrapRootPhaseV0::Complete,
                    )?;
                    written
                }
                Err(error) if bootstrap_write_response_lost(&error) => {
                    let proof = self.bootstrap_recovery_get(&root_ref, session).await?;
                    if bootstrap_root_link_proof(&proof, &before, &state) {
                        session.prove_bootstrap_after_exact_proof()?;
                        session.record_bootstrap_owned_root_echo(
                            &proof,
                            BootstrapRootPhaseV0::Complete,
                        )?;
                        proof
                    } else {
                        return Err(TransportErrorV0::ManualRecoveryHold);
                    }
                }
                Ok(_) | Err(_) => return Err(TransportErrorV0::ManualRecoveryHold),
            };
            if !has_state_link(&linked, &state) {
                return Err(TransportErrorV0::ManualRecoveryHold);
            }
        }
        let final_state = self
            .reread_in_operation(&state_ref, session)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        let final_root = self
            .reread_in_operation(&root_ref, session)
            .await?
            .ok_or(TransportErrorV0::ManualRecoveryHold)?;
        if final_state.metadata.uid != state.metadata.uid
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

    /// Uses the one raw, budget-consuming post-write proof GET.  Bootstrap
    /// deliberately never delegates this to a public reread helper: that
    /// would mint a new operation window after an ambiguous write.
    async fn bootstrap_recovery_get(
        &self,
        reference: &ObjectRefV0,
        session: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.recovery_reread(reference, session)
            .await
            .map_err(|_| TransportErrorV0::ManualRecoveryHold)?
            .ok_or(TransportErrorV0::ManualRecoveryHold)
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
        let fence = self
            .reread_fence_with_target_in_operation(
                address,
                &command.preconditions,
                target.as_ref(),
                session,
            )
            .await?;
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
                return Err(TransportErrorV0::Stale("mutation delete pins"));
            }
            MutationDecision::ConditionalApply { expected, .. }
                if fence.target.as_ref().is_some_and(|target| {
                    target.metadata.uid != expected.uid
                        || target.metadata.resource_version != expected.resource_version
                }) =>
            {
                return Ok(mutation_failed_outcome(
                    transaction_id,
                    attempt_token,
                    "conditional apply target identity is stale",
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

    async fn ensure_finalizer(
        &self,
        object: &ObjectV0,
        finalizer: &str,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        if has_finalizer(object, finalizer) {
            return Ok(object.clone());
        }
        let patch_operation = if object.value.pointer("/metadata/finalizers").is_some() {
            serde_json::json!({"op":"add","path":"/metadata/finalizers/-","value":finalizer})
        } else {
            serde_json::json!({"op":"add","path":"/metadata/finalizers","value":[finalizer]})
        };
        let patch = serde_json::json!([{"op":"test","path":"/metadata/resourceVersion","value":object.metadata.resource_version},patch_operation]);
        let patch = serde_json::from_value::<json_patch::Patch>(patch)
            .map_err(|e| TransportErrorV0::Other(e.to_string()))?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            let patch_params = bootstrap_controller_patch_params();
            self.api(object.reference.resource, &object.reference.namespace)
                .patch(
                    &object.reference.name,
                    &patch_params,
                    &Patch::<Value>::Json(patch),
                )
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::JsonPatch,
                        object.reference.resource,
                    )
                })
        })
        .await?;
        let written = to_domain(
            object.reference.resource,
            &object.reference.namespace,
            written,
        )?;
        permit.prove()?;
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
        token: &str,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let expected = bootstrap_root_metadata_expected_value(root, token)
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
        self.json_patch(root, patch, operation).await
    }
    async fn bootstrap_root_status(
        &self,
        root: &ObjectV0,
        state: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let body = serde_json::json!({"metadata":{"resourceVersion":root.metadata.resource_version},"status":{"state":StateLink { root_uid:root.metadata.uid.clone(), name:state.reference.name.clone(), uid:state.metadata.uid.clone() }}});
        let permit = claim_first_write(operation)?;
        operation.mark_bootstrap_final_write_polled()?;
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
        permit.prove_bootstrap_final()?;
        Ok(written)
    }
    async fn json_patch(
        &self,
        object: &ObjectV0,
        patch: Value,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        let patch = serde_json::from_value::<json_patch::Patch>(patch)
            .map_err(|e| TransportErrorV0::Other(e.to_string()))?;
        let permit = claim_first_write(operation)?;
        let written = within_operation(operation, FiniteIoStageV0::WriteResponse, async {
            let patch_params = bootstrap_controller_patch_params();
            self.api(object.reference.resource, &object.reference.namespace)
                .patch(
                    &object.reference.name,
                    &patch_params,
                    &Patch::<Value>::Json(patch),
                )
                .await
                .map_err(|error| {
                    map_write_error(
                        error,
                        WriteOperationV0::JsonPatch,
                        object.reference.resource,
                    )
                })
        })
        .await?;
        let written = to_domain(
            object.reference.resource,
            &object.reference.namespace,
            written,
        )?;
        permit.prove()?;
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
        session: &OperationSessionV0,
    ) -> Result<oxideloop_mutation_model::ObjectIdentity, TransportErrorV0> {
        if target.metadata.uid != expected.uid
            || target.metadata.resource_version != expected.resource_version
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
        for resource in [ResourceKindV0::ConfigMap, ResourceKindV0::Service] {
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
        if self
            .reread_prewrite_in_operation(&state_ref, session)
            .await?
            .is_some()
        {
            return Err(TransportErrorV0::ManualRecoveryHold);
        }
        Ok((root, receipt))
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
        )
        .await
    }

    async fn watch_from_in_operation_with_deletion_filter(
        &self,
        resource: ResourceKindV0,
        namespace: &str,
        resource_version: &str,
        operation: &OperationSessionV0,
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
        let stream = within_operation(operation, FiniteIoStageV0::WatchOpen, async {
            self.api(resource, namespace)
                .watch(&kube::api::WatchParams::default(), resource_version)
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
    async fn create_state_in_operation(
        &self,
        object: &ObjectV0,
        operation: &OperationSessionV0,
    ) -> Result<ObjectV0, TransportErrorV0> {
        self.create(object, ResourceKindV0::OxideLoopState, operation)
            .await
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

fn bootstrap_root_metadata_proof(observed: &ObjectV0, before: &ObjectV0, token: &str) -> bool {
    if observed.reference != before.reference || observed.metadata.uid != before.metadata.uid {
        return false;
    }
    let Some(expected) = bootstrap_root_metadata_expected_value(before, token) else {
        return false;
    };

    bootstrap_response_projection(&observed.value) == bootstrap_response_projection(&expected)
}

/// Builds the complete raw object shape intended by the bootstrap metadata
/// JSON patch. Keeping this shared with the request construction prevents a
/// response-loss proof from recognizing a broader transform than the write
/// that actually became ambiguous.
fn bootstrap_root_metadata_expected_value(before: &ObjectV0, token: &str) -> Option<Value> {
    let mut expected = before.value.clone();
    let metadata = expected
        .pointer_mut("/metadata")
        .and_then(Value::as_object_mut)?;

    let mut annotations = before.metadata.annotations.clone();
    annotations.insert(ROOT_UID_ANNOTATION.into(), before.metadata.uid.clone());
    annotations.insert(TRANSACTION_ID_ANNOTATION.into(), token.into());
    annotations.insert(COMMAND_ID_ANNOTATION.into(), token.into());
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
    if expected.resource == ResourceKindV0::Service && !is_supported_service_v0(object) {
        return Err(TransportErrorV0::UnsupportedTarget);
    }
    Ok(())
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

    fn test_session(
        attempt_cutoff: Duration,
        prewrite: Duration,
        response: Duration,
        outer: Duration,
        retries: u8,
    ) -> OperationSessionV0 {
        OperationSessionV0 {
            budget: OperationBudgetV0::test_shortened(
                attempt_cutoff,
                prewrite,
                response,
                outer,
                retries,
            )
            .expect("ordered test session"),
            gate: PrewriteGateV0::new(),
            bootstrap: Arc::new(Mutex::new(BootstrapAuthorityV0::new())),
            collector_bound: false,
            bootstrap_phase_wait_hook: None,
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
                    "message":"bootstrap authority", "expose":false,
                    "program":{"configMapName":"bundle", "key":"program.mjs", "digest":"0000000000000000000000000000000000000000000000000000000000000000"}
                },
                "status":{},
            }),
        }
    }

    fn bootstrap_authority_snapshot(root: &ObjectV0) -> RootSnapshotV0 {
        RootSnapshotV0 {
            uid: root.metadata.uid.clone(),
            namespace: root.reference.namespace.clone(),
            name: root.reference.name.clone(),
            generation: generation(root).expect("fixture generation"),
            spec: serde_json::from_value(root.value["spec"].clone()).expect("fixture root spec"),
            bundle_digest: "0".repeat(64),
            status: WebApplicationStatus::default(),
            state_name: "state".into(),
            state_uid: "state-uid".into(),
            state_resource_version: "state-rv".into(),
            root_resource_version: root.metadata.resource_version.clone(),
            root_finalizer_present: true,
            state_finalizer_present: true,
            deleting: false,
        }
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
        session
            .begin_bootstrap_sequence()
            .expect("bootstrap authority begins");
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

    #[test]
    fn bootstrap_metadata_echo_is_single_use_phase_epoch_and_raw_response_bound() {
        let session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("2");
        let fresh = bootstrap_authority_snapshot(&root);
        session
            .begin_bootstrap_sequence()
            .expect("bootstrap authority begins");
        session
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::RootMetadata, &root)
            .expect("metadata preflight freezes root");
        session
            .record_bootstrap_owned_root_echo(&root, BootstrapRootPhaseV0::RootStateLink)
            .expect("metadata response is recorded for State-link");

        assert_eq!(
            session.authenticate_bootstrap_root_echo(&fresh, 1),
            BootstrapRootEchoAuthenticationV0::GenuineOrAmbiguous,
            "an otherwise exact response belongs only to its recorded epoch"
        );
        assert_eq!(
            session.authenticate_bootstrap_root_echo(&fresh, 0),
            BootstrapRootEchoAuthenticationV0::ExactFirstUse
        );
        assert_eq!(
            session.authenticate_bootstrap_root_echo(&fresh, 1),
            BootstrapRootEchoAuthenticationV0::ExactDuplicate,
            "a duplicate delivery cannot consume a second restart"
        );
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
        let root = bootstrap_authority_root("2");
        let mut nonfinal = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        nonfinal.collector_bound = true;
        nonfinal
            .begin_bootstrap_sequence()
            .expect("bootstrap authority begins");
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
    fn bootstrap_metadata_self_echo_then_final_response_loss_proves_the_last_epoch() {
        let session = test_session(
            Duration::from_millis(5),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(50),
            3,
        );
        let root = bootstrap_authority_root("2");
        let fresh = bootstrap_authority_snapshot(&root);
        session
            .begin_bootstrap_sequence()
            .expect("bootstrap authority begins");
        session
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::RootMetadata, &root)
            .expect("metadata phase freezes root");
        session
            .record_bootstrap_owned_root_echo(&root, BootstrapRootPhaseV0::RootStateLink)
            .expect("metadata response is retained");
        let metadata_epoch = session
            .gate
            .begin_root_authentication()
            .expect("metadata echo owns its authentication epoch");
        assert_eq!(
            session.authenticate_bootstrap_root_echo(&fresh, metadata_epoch),
            BootstrapRootEchoAuthenticationV0::ExactFirstUse
        );
        assert!(
            session
                .gate
                .restart_after_exact_self_echo(metadata_epoch, 3)
        );
        assert_eq!(
            session.gate.state(),
            PrewriteGateStateV0::BootstrapPhasePending
        );
        assert!(session.gate.seal_bootstrap_phase());
        session
            .begin_bootstrap_root_phase(BootstrapRootPhaseV0::RootStateLink, &root)
            .expect("the consumed metadata response still authorizes its next phase");
        session
            .require_exact_root_metadata_response(&root)
            .expect("State-link uses the exact metadata response");

        let final_write = claim_first_write(&session).expect("terminal write polls once");
        session
            .mark_bootstrap_final_write_polled()
            .expect("terminal phase is marked before its response returns");
        drop(final_write);
        session
            .prove_bootstrap_after_exact_proof()
            .expect("exact final response-loss recovery proves the same last epoch");
        assert_eq!(session.gate.epoch(), 1);
        assert_eq!(session.gate.state(), PrewriteGateStateV0::Proven);
        assert!(matches!(
            claim_first_write(&session),
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
            value: json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":"app","namespace":"ns","uid":"root-uid","resourceVersion":"1","generation":1,"annotations":{},"finalizers":[]},"spec":{"program":{"digest":"bundle"}},"status":{}}),
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
        root_after_metadata.metadata.annotations = BTreeMap::from([
            (ROOT_UID_ANNOTATION.into(), "root-uid".into()),
            (TRANSACTION_ID_ANNOTATION.into(), "bootstrap-token".into()),
            (COMMAND_ID_ANNOTATION.into(), "bootstrap-token".into()),
        ]);
        root_after_metadata.value["metadata"]["resourceVersion"] = json!("2");
        root_after_metadata.value["metadata"]["annotations"] = json!({ROOT_UID_ANNOTATION:"root-uid",TRANSACTION_ID_ANNOTATION:"bootstrap-token",COMMAND_ID_ANNOTATION:"bootstrap-token"});
        root_after_metadata.value["metadata"]["finalizers"] = json!([ROOT_FINALIZER]);
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
        let session = OperationSessionV0 {
            budget,
            gate: PrewriteGateV0::new(),
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
        let session = OperationSessionV0 {
            budget,
            gate: PrewriteGateV0::new(),
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
