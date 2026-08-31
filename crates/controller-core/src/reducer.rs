#![allow(
    clippy::cast_sign_loss,
    clippy::if_not_else,
    clippy::large_enum_variant,
    clippy::manual_string_new,
    clippy::match_same_arms,
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::semicolon_if_nothing_returned,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::unnecessary_wraps
)]

use std::collections::{BTreeMap, BTreeSet};

use oxideloop_controller_api::{
    ApplyIntent, ApplyOperation, ApplyOutcome, ApplyRecoveryPolicy, ApplyResult, ApplyTransaction,
    AutomationBaseline, AutomationCompletion, AutomationCompletionOutcome, AutomationPending,
    AutomationRunCompletionState, AutomationRunSpec, DeletionCleanupReceipt, DeletionTransaction,
    DependencySnapshot, DependencySnapshotType, DurablePhase, GenerationAdvanceClosure,
    GenerationAdvanceClosurePurpose, InventoryEntry, MAX_ACTION_INPUT_CANONICAL_BYTES,
    MAX_CLOSURE_REBASES, MAX_IDENTIFIER_BYTES, MAX_LIST_ENTRIES, NextCommandLifecycleStateV0,
    NextCommandLifecycleV0, ObjectIdentity, OxideLoopStateSpec, PersistOperation, PersistPurpose,
    PreparedRecoveryClaim, PruneCandidate, PruneObservation, StateBody, WebApplicationSpec,
    WebApplicationStatus, automation_run_name, bootstrap_id, state_name,
    validate_automation_run_name, validate_root_state_link,
};
use oxideloop_mutation_model::{
    APPLY_PATCH_CONTENT_TYPE, BackendQualification, CONTROLLER_FIELD_MANAGER, HttpMethod,
    MutationDecision, MutationIntent, MutationPlanInputV0, ObjectIdentity as MutationIdentity,
    Observation, PROBE_VERSION_V0, plan,
};
use oxideloop_renderer_abi::model::{Dependency, RenderDisposition};
use oxideloop_renderer_abi::{
    ActionInputV0, RenderInputV0, canonical_bytes, parse_validate_render_input,
    parse_validate_render_output, sha256_hex,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const MAX_COMMANDS: usize = 1024;
const MAX_AUTOMATION_ATTEMPTS: u16 = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootSnapshotV0 {
    pub uid: String,
    pub namespace: String,
    /// Immutable Kubernetes object name for the observed root UID.
    pub name: String,
    pub generation: i64,
    /// Exact validated authored spec projected to the renderer and used to
    /// authorize program source fetches.
    pub spec: WebApplicationSpec,
    pub bundle_digest: String,
    pub status: WebApplicationStatus,
    pub state_name: String,
    pub state_uid: String,
    pub state_resource_version: String,
    pub root_resource_version: String,
    pub root_finalizer_present: bool,
    pub state_finalizer_present: bool,
    /// True only when a fresh root read contains `deletionTimestamp`.
    #[serde(default)]
    pub deleting: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenInputV0 {
    pub root_generation: i64,
    pub state_resource_version: String,
    pub observation_token: String,
    pub cache_epoch: String,
    /// Exact completed final-observation fence used for destructive prune
    /// confirmation. It is intentionally distinct from the renderer's
    /// global observation token, which may advance for unrelated changes.
    pub final_observation_fence_token: String,
    pub render_input_hash: String,
}

/// Each effectful acknowledgement has exactly one admissible phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhaseV0 {
    /// A deserialized state has no executable continuation until a fresh Start.
    Recovery,
    Bootstrap,
    BootstrapPending,
    DescriptorPending,
    InformersPending,
    RenderPending,
    PersistPending,
    DispatchAuthorizationPersistPending,
    RevalidatePending,
    MutationPending,
    Ready,
    AutomationEnsurePending,
    AutomationPersistPending,
    DeletionStartPersistPending,
    DeletionPreparePersistPending,
    DeletionMutationPending,
    DeletionMutationPersistPending,
    DeletionAutomationCleanupPending,
    DeletionAutomationPersistPending,
    GenerationAdvanceClosurePersistPending,
    DeletionObservationPending,
    OwnedChildrenVerifyPending,
    StateFinalizerPending,
    RootFinalizerPending,
    RootDeletionVerifyPending,
    RootStatusPersistPending,
    Terminal,
    Hold,
}

/// Never serialized as an executable continuation.  It is public (there is no
/// inaccessible private reducer state), and serde recovery always resets it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeStateV0 {
    pub requires_revalidation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_automation: Option<PendingAutomationV0>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_root_status: Option<PendingRootStatusV0>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_bootstrap: Option<PendingBootstrapV0>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_descriptor_token: Option<String>,
    /// Root watch notifications have no authority to cancel an in-flight
    /// external effect. They are coalesced until its acknowledgement has been
    /// accepted and can be durably closed without replaying the effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_root_refresh: Option<RootSnapshotV0>,
    /// A retained dependency barrier may supersede an observation-only render
    /// before it has had any durable effect.  This stays in the skipped
    /// runtime continuation: recovery always obtains a fresh informer fence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_observation: Option<PreconditionsV0>,
    /// Exact one-command capability retained only between the durable
    /// dispatch-authorization acknowledgement and the Kubernetes request.
    /// It is never serialized or reconstructed after restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_dispatch: Option<PendingMutationDispatchV0>,
}

impl Default for RuntimeStateV0 {
    fn default() -> Self {
        Self {
            requires_revalidation: true,
            pending_automation: None,
            pending_root_status: None,
            pending_bootstrap: None,
            pending_descriptor_token: None,
            pending_root_refresh: None,
            pending_observation: None,
            pending_dispatch: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingMutationDispatchV0 {
    pub transaction_id: String,
    pub attempt_token: String,
    pub command: MutationCommandV0,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingAutomationV0 {
    pub token: String,
    pub name: String,
    pub spec: AutomationRunSpec,
    pub next_baseline_token: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingRootStatusV0 {
    pub token: String,
    pub old_resource_version: String,
    pub status: WebApplicationStatus,
    pub continuation: RootStatusContinuationV0,
    /// Exact render observation identity sealed by the completed render. This
    /// is deliberately skipped by recovery with the rest of runtime state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_input: Option<RootStatusContinuationPayloadV0>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingBootstrapV0 {
    pub token: String,
    pub state: OxideLoopStateSpec,
    pub state_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootStatusContinuationV0 {
    Informers,
    Render,
    DeletionStateFinalizer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootStatusContinuationPayloadV0 {
    pub observation_token: String,
    pub cache_epoch: String,
}

/// Validated root-plus-durable-journal state for the pure reducer.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineStateV0 {
    root: RootSnapshotV0,
    journal: OxideLoopStateSpec,
    phase: PhaseV0,
    frozen: Option<FrozenInputV0>,
    /// True only for the receipt-backed recovery path after the linked state
    /// object is freshly observed absent. This mode can verify and remove the
    /// root finalizer, but it can never bootstrap or persist a replacement
    /// state object.
    #[serde(default)]
    root_only_deletion_recovery: bool,
    #[serde(skip, default)]
    runtime: RuntimeStateV0,
}

/// Wire representation accepted from durable storage. Runtime-only issuance
/// state and a serialized phase are deliberately not trusted on recovery.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MachineStateWireV0 {
    root: RootSnapshotV0,
    journal: OxideLoopStateSpec,
    #[allow(dead_code)]
    phase: PhaseV0,
    #[allow(dead_code)]
    frozen: Option<FrozenInputV0>,
    #[serde(default)]
    root_only_deletion_recovery: bool,
}

impl<'de> Deserialize<'de> for MachineStateV0 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = MachineStateWireV0::deserialize(deserializer)?;
        // Never revive a serialized phase, frozen input, or runtime issuance
        // capability. The first legal event after deserialization is Start.
        Ok(Self {
            root: wire.root,
            journal: wire.journal,
            phase: PhaseV0::Recovery,
            frozen: None,
            root_only_deletion_recovery: wire.root_only_deletion_recovery,
            runtime: RuntimeStateV0::default(),
        })
    }
}

impl MachineStateV0 {
    /// Creates an active reducer only after validating all durable links.
    pub fn new(root: RootSnapshotV0, journal: OxideLoopStateSpec) -> Result<Self, ReconcileError> {
        validate_root_and_journal(&root, &journal)?;
        Ok(Self {
            root,
            journal,
            phase: PhaseV0::Bootstrap,
            frozen: None,
            root_only_deletion_recovery: false,
            runtime: RuntimeStateV0 {
                requires_revalidation: false,
                pending_automation: None,
                pending_root_status: None,
                pending_bootstrap: None,
                pending_descriptor_token: None,
                pending_root_refresh: None,
                pending_observation: None,
                pending_dispatch: None,
            },
        })
    }

    /// Starts from a root which may have no state object, status link, or
    /// finalizers. None of those partial observations are trusted: Start emits
    /// one root-pinned bootstrap command and only its exact acknowledgement
    /// creates an active linked machine.
    pub fn bootstrap(mut root: RootSnapshotV0) -> Result<Self, ReconcileError> {
        validate_bootstrap_root(&root)?;
        // Partial links, finalizers and prior status are observations, not a
        // bootstrap authority. The acknowledgement below replaces all of
        // these fields atomically after checking its root/state RV pins.
        root.state_name.clear();
        root.state_uid.clear();
        root.state_resource_version.clear();
        root.root_finalizer_present = false;
        root.state_finalizer_present = false;
        // Every controller-owned status field is an untrusted partial
        // observation at bootstrap. In particular, a spoofed Ready/Hold
        // condition must not survive until the exact descriptor verification.
        root.status = WebApplicationStatus::default();
        let state = OxideLoopStateSpec {
            state_version: oxideloop_controller_api::STATE_VERSION_V1,
            root: oxideloop_controller_api::RootBinding {
                uid: root.uid.clone(),
                bootstrap_id: bootstrap_id(&root.uid)
                    .map_err(|error| ReconcileError::Link(error.to_string()))?,
                bundle_digest: root.bundle_digest.clone(),
            },
            state: StateBody::default(),
        };
        state
            .validate()
            .map_err(|error| ReconcileError::Link(error.to_string()))?;
        Ok(Self {
            root,
            journal: state,
            phase: PhaseV0::Bootstrap,
            frozen: None,
            root_only_deletion_recovery: false,
            runtime: RuntimeStateV0 {
                requires_revalidation: false,
                pending_automation: None,
                pending_root_status: None,
                pending_bootstrap: None,
                pending_descriptor_token: None,
                pending_root_refresh: None,
                pending_observation: None,
                pending_dispatch: None,
            },
        })
    }

    /// Creates a conservative recovered reducer. No operation can be issued
    /// until an adapter has supplied fresh descriptor/render observations.
    pub fn recover(
        root: RootSnapshotV0,
        journal: OxideLoopStateSpec,
    ) -> Result<Self, ReconcileError> {
        let mut state = Self::new(root, journal)?;
        state.phase = PhaseV0::Recovery;
        state.frozen = None;
        state.runtime = RuntimeStateV0::default();
        Ok(state)
    }

    /// Recovers deletion after the linked state object has been freshly
    /// observed absent. The durable status receipt is the sole journal
    /// authority; this constructor never creates bootstrap capability.
    pub fn recover_deleting_root_without_state(
        mut root: RootSnapshotV0,
    ) -> Result<Self, ReconcileError> {
        validate_bootstrap_root(&root)?;
        let receipt = validate_deleting_root_receipt(&root)?.clone();
        root.state_name.clone_from(&receipt.state_name);
        root.state_uid.clone_from(&receipt.state_uid);
        root.state_resource_version.clear();
        root.state_finalizer_present = false;
        let journal = OxideLoopStateSpec {
            state_version: oxideloop_controller_api::STATE_VERSION_V1,
            root: oxideloop_controller_api::RootBinding {
                uid: root.uid.clone(),
                bootstrap_id: bootstrap_id(&root.uid)
                    .map_err(|error| ReconcileError::Recovery(error.to_string()))?,
                bundle_digest: root.bundle_digest.clone(),
            },
            state: StateBody {
                deletion: Some(DeletionTransaction {
                    id: receipt.deletion_id,
                    deletion_start_generation: receipt.deletion_start_generation,
                }),
                ..StateBody::default()
            },
        };
        let state = Self {
            root,
            journal,
            phase: PhaseV0::Recovery,
            frozen: None,
            root_only_deletion_recovery: true,
            runtime: RuntimeStateV0::default(),
        };
        validate_root_only_deletion_machine(&state)?;
        Ok(state)
    }

    /// Safe serde recovery entry point. It never restores runtime issuance.
    pub fn recover_json(serialized: &str) -> Result<Self, ReconcileError> {
        let state: Self = serde_json::from_str(serialized)
            .map_err(|e| ReconcileError::Recovery(e.to_string()))?;
        if state.root_only_deletion_recovery {
            validate_root_only_deletion_machine(&state)?;
        } else if validate_root_and_journal(&state.root, &state.journal).is_err() {
            validate_bootstrap_machine(&state)?;
        }
        Ok(state)
    }

    /// Current immutable root snapshot for observers that need to inspect a
    /// transition without acquiring mutation authority.
    #[must_use]
    pub fn root(&self) -> &RootSnapshotV0 {
        &self.root
    }

    /// Current durable journal for observers that need to persist a returned
    /// transition.
    #[must_use]
    pub fn journal(&self) -> &OxideLoopStateSpec {
        &self.journal
    }

    /// Whether this machine was reconstructed solely from a deleting root's
    /// durable cleanup receipt after its linked state object was confirmed
    /// absent. Such a machine has authority only for root-only deletion
    /// completion; adapters must discard any normal reconciliation caches
    /// before replacing an active actor with it.
    #[must_use]
    pub const fn is_root_only_deletion_recovery(&self) -> bool {
        self.root_only_deletion_recovery
    }

    /// Current reducer phase. A deserialized state always reports Recovery.
    #[must_use]
    pub const fn phase(&self) -> PhaseV0 {
        self.phase
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesiredObservationV0 {
    pub identity: ObjectIdentity,
    pub observation: Observation,
    pub qualification: BackendQualification,
    /// Proof injected by the adapter only after a fresh GET has validated the
    /// controller labels/owner identity. A same-name foreign object has no
    /// proof and is never adopted by recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_proof: Option<RecoveryProofV0>,
    /// Explicit proof for a response-lost DELETE which observed absence. An
    /// absent name alone is never evidence that this transaction deleted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_recovery_proof: Option<DeletionRecoveryProofV0>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryProofV0 {
    pub root_uid: String,
    pub transaction_id: String,
    pub command_id: String,
    pub uid: String,
    pub resource_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeletionRecoveryProofV0 {
    pub root_uid: String,
    pub transaction_id: String,
    pub command_id: String,
    pub expected_uid: String,
    pub expected_resource_version: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionTriggerV0 {
    pub automation_identity: String,
    pub automation_version: String,
    pub action_module_identity: String,
    pub action_module_digest: String,
    pub source: ObjectIdentity,
    pub source_uid: String,
    pub source_resource_version: String,
    pub observation_token: String,
    pub event_metadata: Value,
}

/// Internal reason for a root-local render request. This is deliberately
/// separate from renderer observation tokens: a prune confirmation must not
/// manufacture a source transition for an automation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InternalReconcileCauseV0 {
    DependencyWatch,
    PruneConfirmation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum EventV0 {
    Start,
    /// Exact fresh root reread. This is distinct from a dependency wakeup:
    /// only this event may replace root UID/generation/resourceVersion pins.
    RootRefreshed {
        root: RootSnapshotV0,
    },
    /// Token-only dependency/prune wakeup. It never carries root authority.
    ReconcileRequested {
        cause: InternalReconcileCauseV0,
        observation_token: String,
        cache_epoch: String,
    },
    /// Actor-local replacement of an observation-only informer/render pass.
    /// This event is never persisted and deliberately carries exact root/state
    /// pins, rather than a token-only wakeup that could bypass `Ready`.
    ObservationSuperseded {
        preconditions: PreconditionsV0,
    },
    BootstrapAcknowledged {
        token: String,
        state_name: String,
        state_uid: String,
        state_resource_version: String,
        old_root_resource_version: String,
        new_root_resource_version: String,
        status_state_link: oxideloop_controller_api::StateLink,
        root_finalizer_present: bool,
        state_finalizer_present: bool,
    },
    DescriptorReady {
        token: String,
        bundle_digest: String,
    },
    InformersReady,
    Rendered {
        frozen: FrozenInputV0,
        render_input_json: String,
        render_output_json: String,
        observed: Vec<DesiredObservationV0>,
    },
    Revalidated {
        frozen: FrozenInputV0,
        render_input_json: String,
        render_output_json: String,
        observed: Vec<DesiredObservationV0>,
    },
    Persisted {
        token: String,
        old_state_resource_version: String,
        new_state_resource_version: String,
        state_uid: String,
    },
    MutationAcknowledged {
        attempt_token: String,
        transaction_id: String,
        results: Vec<ApplyResult>,
    },
    MutationFailed {
        attempt_token: String,
        transaction_id: String,
        reason: String,
    },
    EstablishAutomationBaseline {
        trigger: ActionTriggerV0,
    },
    TriggerAutomation {
        trigger: ActionTriggerV0,
    },
    AutomationRunEnsured {
        token: String,
        name: String,
        spec: AutomationRunSpec,
        uid: String,
        resource_version: String,
    },
    AutomationCompleted {
        /// The exact immutable spec reread with the completed status. The
        /// reducer canonicalizes it before accepting the durable pending run.
        spec: AutomationRunSpec,
        occurrence_id: String,
        name: String,
        uid: String,
        old_resource_version: String,
        new_resource_version: String,
        completion: AutomationRunCompletionState,
        attempts: u16,
        materialization_name: String,
        materialization_uid: String,
    },
    BeginDeletion,
    DeletionRevalidated {
        observed_inventory: Vec<InventoryEntry>,
        observed_automation_runs: Vec<AutomationRunCleanupV0>,
    },
    AutomationRunDeleted {
        token: String,
        occurrence_id: String,
        name: String,
        uid: String,
        resource_version: String,
    },
    AutomationRunDeleteFailed {
        token: String,
        reason: String,
    },
    OwnedChildrenVerifiedAbsent {
        token: String,
        root_uid: String,
        root_generation: i64,
        root_resource_version: String,
        state_uid: String,
        state_resource_version: String,
        cleanup_token: String,
    },
    OwnedChildrenVerificationFailed {
        token: String,
        reason: String,
    },
    RootStatusPersisted {
        token: String,
        old_root_resource_version: String,
        new_root_resource_version: String,
        root_uid: String,
    },
    StateFinalizerRemoved {
        token: String,
        state_uid: String,
        old_state_resource_version: String,
        new_state_resource_version: String,
    },
    RootFinalizerRemoved {
        token: String,
        root_uid: String,
        old_root_resource_version: String,
        new_root_resource_version: String,
    },
    RootUidVerifiedAbsent {
        token: String,
        root_uid: String,
    },
    RootUidAbsenceFailed {
        token: String,
        observed_uid: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationRunCleanupV0 {
    pub occurrence_id: String,
    pub name: String,
    pub uid: String,
    pub resource_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreconditionsV0 {
    pub root_uid: String,
    pub root_generation: i64,
    pub root_resource_version: String,
    pub state_uid: String,
    pub state_resource_version: String,
    pub observation_token: String,
    pub cache_epoch: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MutationCommandV0 {
    pub command_id: String,
    pub attempt_token: String,
    pub preconditions: PreconditionsV0,
    pub identity: ObjectIdentity,
    pub logical_id: String,
    /// The target identity recorded in the durable prepared intent. This is
    /// intentionally independent from `decision.expected`: transport must
    /// compare both values with its fresh target read before a conditional
    /// apply is authorized. `None` represents POST; `Some` represents an
    /// existing target for conditional apply or delete.
    ///
    /// The default preserves decoding of pre-P13 command envelopes while
    /// `deny_unknown_fields` continues to reject unrecognized fields. Such a
    /// legacy envelope cannot satisfy the later transport force gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_expected_target: Option<MutationIdentity>,
    pub decision: MutationDecision,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum CommandV0 {
    EnsureBootstrap {
        token: String,
        state: OxideLoopStateSpec,
        root_uid: String,
        root_generation: i64,
        root_resource_version: String,
        state_name: String,
    },
    Persist {
        state: OxideLoopStateSpec,
        preconditions: PreconditionsV0,
        operation: PersistOperation,
    },
    PersistRootStatus {
        token: String,
        root_uid: String,
        old_root_resource_version: String,
        preconditions: PreconditionsV0,
        status: WebApplicationStatus,
    },
    RequestDescriptor {
        token: String,
        preconditions: PreconditionsV0,
    },
    RequestRender {
        preconditions: PreconditionsV0,
    },
    RequestDeletionObservation {
        preconditions: PreconditionsV0,
    },
    Mutate {
        transaction_id: String,
        attempt_token: String,
        commands: Vec<MutationCommandV0>,
    },
    EnsureAutomationRun {
        token: String,
        name: String,
        spec: AutomationRunSpec,
        preconditions: PreconditionsV0,
    },
    DeleteAutomationRun {
        token: String,
        occurrence_id: String,
        name: String,
        uid: String,
        resource_version: String,
        preconditions: PreconditionsV0,
    },
    VerifyOwnedChildrenAbsent {
        token: String,
        preconditions: PreconditionsV0,
    },
    RemoveStateFinalizer {
        token: String,
        preconditions: PreconditionsV0,
    },
    RemoveRootFinalizer {
        token: String,
        preconditions: PreconditionsV0,
    },
    VerifyRootUidAbsent {
        token: String,
        root_uid: String,
        preconditions: PreconditionsV0,
    },
    Hold {
        reason: String,
    },
    IgnoredStale {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransitionV0 {
    pub state: MachineStateV0,
    pub commands: Vec<CommandV0>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReconcileError {
    #[error("root/state link is invalid: {0}")]
    Link(String),
    #[error("recovered state is invalid: {0}")]
    Recovery(String),
    #[error("renderer document is invalid: {0}")]
    Renderer(String),
    #[error("render pins do not match root state")]
    FrozenPinMismatch,
    #[error("desired graph is invalid: {0}")]
    Desired(String),
    #[error("mutation model rejected desired graph: {0}")]
    Mutation(String),
    #[error("automation protocol rejected input: {0}")]
    Automation(String),
    #[error("mutation acknowledgement is invalid: {0}")]
    MutationAck(String),
    #[error("command bound exceeded")]
    CommandBound,
}

/// Pure deterministic transition. Deletion is evaluated first, preempting all
/// normal in-flight phases and producing only a durable transition write.
#[allow(clippy::too_many_lines)]
pub fn reduce(mut state: MachineStateV0, event: EventV0) -> Result<TransitionV0, ReconcileError> {
    if state.phase == PhaseV0::Recovery && !matches!(event, EventV0::Start) {
        return Err(ReconcileError::Recovery(
            "recovered state accepts only Start".into(),
        ));
    }
    if state.root_only_deletion_recovery {
        validate_root_only_deletion_machine(&state)?;
    } else if matches!(
        state.phase,
        PhaseV0::Bootstrap | PhaseV0::BootstrapPending | PhaseV0::Recovery
    ) {
        validate_bootstrap_machine(&state)?;
    } else {
        validate_root_and_journal(&state.root, &state.journal)?;
    }
    if state.root_only_deletion_recovery && !root_only_event_allowed(&event) {
        state.phase = PhaseV0::Hold;
        return finish(
            state,
            vec![CommandV0::Hold {
                reason: "event is outside root-only deletion recovery authority".into(),
            }],
        );
    }
    if matches!(event, EventV0::BeginDeletion) {
        return begin_deletion(state);
    }
    let mut commands = Vec::new();
    match event {
        EventV0::Start => start(&mut state, &mut commands),
        EventV0::RootRefreshed { root } => root_refreshed(&mut state, root, &mut commands)?,
        EventV0::ReconcileRequested {
            cause,
            observation_token,
            cache_epoch,
        } => reconcile_requested(
            &mut state,
            cause,
            &observation_token,
            &cache_epoch,
            &mut commands,
        ),
        EventV0::ObservationSuperseded { preconditions } => {
            observation_superseded(&mut state, preconditions, &mut commands)
        }
        EventV0::BootstrapAcknowledged {
            token,
            state_name,
            state_uid,
            state_resource_version,
            old_root_resource_version,
            new_root_resource_version,
            status_state_link,
            root_finalizer_present,
            state_finalizer_present,
        } => bootstrap_ack(
            &mut state,
            &token,
            &state_name,
            &state_uid,
            &state_resource_version,
            &old_root_resource_version,
            &new_root_resource_version,
            &status_state_link,
            root_finalizer_present,
            state_finalizer_present,
            &mut commands,
        ),
        EventV0::DescriptorReady {
            token,
            bundle_digest,
        } => descriptor_ready(&mut state, &token, &bundle_digest, &mut commands),
        EventV0::InformersReady => {
            if state.phase == PhaseV0::InformersPending {
                state.phase = if state.journal.state.apply_transaction.is_some() {
                    PhaseV0::RevalidatePending
                } else {
                    PhaseV0::RenderPending
                };
                commands.push(CommandV0::RequestRender {
                    preconditions: state
                        .runtime
                        .pending_observation
                        .take()
                        .unwrap_or_else(|| pins(&state.root)),
                });
            } else {
                stale(&mut commands, "informers acknowledgement phase mismatch");
            }
        }
        EventV0::Rendered {
            frozen,
            render_input_json,
            render_output_json,
            observed,
        } => {
            if state.phase != PhaseV0::RenderPending || !matches_root(&state, &frozen) {
                stale(&mut commands, "render phase or pins mismatch");
            } else {
                let status = root_status_for(
                    &state.root,
                    Some(&frozen),
                    state.root.status.verified_bundle_digest.as_deref()
                        == Some(state.root.bundle_digest.as_str()),
                );
                if queue_root_status(
                    &mut state,
                    status,
                    RootStatusContinuationV0::Render,
                    Some(&frozen),
                    &mut commands,
                ) {
                    // The status acknowledgement must land before this render can
                    // contribute a durable state transition. The adapter will
                    // supply a fresh render on the continuation.
                } else {
                    let plan = plan_graph(
                        &state,
                        &frozen,
                        &render_input_json,
                        &render_output_json,
                        observed,
                    )?;
                    if plan.disposition == RenderDisposition::Hold {
                        state.phase = PhaseV0::Hold;
                        state.frozen = Some(frozen);
                        commands.push(CommandV0::Hold {
                            reason: "renderer requested hold".into(),
                        });
                    } else {
                        prepare(&mut state, frozen, plan, &mut commands)?;
                    }
                }
            }
        }
        EventV0::Revalidated {
            frozen,
            render_input_json,
            render_output_json,
            observed,
        } => revalidate(
            &mut state,
            frozen,
            &render_input_json,
            &render_output_json,
            observed,
            &mut commands,
        )?,
        EventV0::Persisted {
            token,
            old_state_resource_version,
            new_state_resource_version,
            state_uid,
        } => persist_ack(
            &mut state,
            &token,
            &old_state_resource_version,
            &new_state_resource_version,
            &state_uid,
            &mut commands,
        )?,
        EventV0::MutationAcknowledged {
            attempt_token,
            transaction_id,
            results,
        } => mutation_ack(
            &mut state,
            &attempt_token,
            &transaction_id,
            results,
            &mut commands,
        )?,
        EventV0::MutationFailed {
            attempt_token,
            transaction_id,
            reason,
        } => mutation_failed(
            &mut state,
            &attempt_token,
            &transaction_id,
            reason,
            &mut commands,
        ),
        EventV0::EstablishAutomationBaseline { trigger } => {
            if state.phase != PhaseV0::Ready {
                stale(&mut commands, "automation baseline phase mismatch");
            } else {
                validate_action_trigger(&trigger)?;
                baseline(&mut state, trigger);
                queue_persist(
                    &mut state,
                    PersistPurpose::AutomationBaseline,
                    DurablePhase::AutomationBaseline,
                    PhaseV0::AutomationPersistPending,
                    &mut commands,
                )?;
            }
        }
        EventV0::TriggerAutomation { trigger } => {
            trigger_automation(&mut state, trigger, &mut commands)?
        }
        EventV0::AutomationRunEnsured {
            token,
            name,
            spec,
            uid,
            resource_version,
        } => automation_ensured(
            &mut state,
            &token,
            &name,
            &spec,
            &uid,
            &resource_version,
            &mut commands,
        )?,
        EventV0::AutomationCompleted {
            spec,
            occurrence_id,
            name,
            uid,
            old_resource_version,
            new_resource_version,
            completion,
            attempts,
            materialization_name,
            materialization_uid,
        } => automation_completed(
            &mut state,
            &spec,
            occurrence_id,
            name,
            uid,
            old_resource_version,
            new_resource_version,
            completion,
            attempts,
            materialization_name,
            materialization_uid,
            &mut commands,
        )?,
        EventV0::DeletionRevalidated {
            observed_inventory,
            observed_automation_runs,
        } => deletion_revalidated(
            &mut state,
            observed_inventory,
            observed_automation_runs,
            &mut commands,
        )?,
        EventV0::AutomationRunDeleted {
            token,
            occurrence_id,
            name,
            uid,
            resource_version,
        } => automation_deleted(
            &mut state,
            &token,
            &occurrence_id,
            &name,
            &uid,
            &resource_version,
            &mut commands,
        )?,
        EventV0::AutomationRunDeleteFailed { token, reason } => {
            if state.phase == PhaseV0::DeletionAutomationCleanupPending
                && cleanup_token(&state).as_deref() == Some(&token)
            {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: format!("automation cleanup failed: {reason}"),
                });
            } else {
                stale(&mut commands, "automation cleanup failure token mismatch");
            }
        }
        EventV0::OwnedChildrenVerifiedAbsent {
            token,
            root_uid,
            root_generation,
            root_resource_version,
            state_uid,
            state_resource_version,
            cleanup_token,
        } => owned_children_verified(
            &mut state,
            &token,
            &root_uid,
            root_generation,
            &root_resource_version,
            &state_uid,
            &state_resource_version,
            &cleanup_token,
            &mut commands,
        ),
        EventV0::OwnedChildrenVerificationFailed { token, reason } => {
            if state.phase == PhaseV0::OwnedChildrenVerifyPending
                && owned_children_token(&state) == Some(token)
            {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: format!("owned-child absence verification failed: {reason}"),
                });
            } else {
                stale(
                    &mut commands,
                    "owned-child verification failure token mismatch",
                );
            }
        }
        EventV0::RootStatusPersisted {
            token,
            old_root_resource_version,
            new_root_resource_version,
            root_uid,
        } => root_status_ack(
            &mut state,
            &token,
            &old_root_resource_version,
            &new_root_resource_version,
            &root_uid,
            &mut commands,
        ),
        EventV0::StateFinalizerRemoved {
            token,
            state_uid,
            old_state_resource_version,
            new_state_resource_version,
        } => state_finalizer_ack(
            &mut state,
            &token,
            &state_uid,
            &old_state_resource_version,
            &new_state_resource_version,
            &mut commands,
        ),
        EventV0::RootFinalizerRemoved {
            token,
            root_uid,
            old_root_resource_version,
            new_root_resource_version,
        } => root_finalizer_ack(
            &mut state,
            &token,
            &root_uid,
            &old_root_resource_version,
            &new_root_resource_version,
            &mut commands,
        ),
        EventV0::RootUidVerifiedAbsent { token, root_uid } => {
            if state.phase == PhaseV0::RootDeletionVerifyPending
                && token == root_absence_token(&state)
                && root_uid == state.root.uid
            {
                state.phase = PhaseV0::Terminal;
            } else {
                stale(&mut commands, "root absence acknowledgement mismatch");
            }
        }
        EventV0::RootUidAbsenceFailed {
            token,
            observed_uid,
        } => {
            if state.phase == PhaseV0::RootDeletionVerifyPending
                && token == root_absence_token(&state)
            {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: format!(
                        "original root UID remains after finalizer removal: {observed_uid}"
                    ),
                });
            } else {
                stale(&mut commands, "root absence failure token mismatch");
            }
        }
        EventV0::BeginDeletion => unreachable!(),
    }
    finish(state, commands)
}

fn effect_closure_purpose(purpose: PersistPurpose) -> Option<GenerationAdvanceClosurePurpose> {
    match purpose {
        PersistPurpose::MutationResult => Some(GenerationAdvanceClosurePurpose::MutationResult),
        PersistPurpose::AutomationPending => {
            Some(GenerationAdvanceClosurePurpose::AutomationPending)
        }
        PersistPurpose::AutomationCompletion => {
            Some(GenerationAdvanceClosurePurpose::AutomationCompletion)
        }
        PersistPurpose::DeletionMutationResult => {
            Some(GenerationAdvanceClosurePurpose::DeletionMutationResult)
        }
        PersistPurpose::DeletionAutomationCleanup => {
            Some(GenerationAdvanceClosurePurpose::DeletionAutomationCleanup)
        }
        _ => None,
    }
}

fn root_only_event_allowed(event: &EventV0) -> bool {
    matches!(
        event,
        EventV0::Start
            | EventV0::RootRefreshed { .. }
            | EventV0::OwnedChildrenVerifiedAbsent { .. }
            | EventV0::OwnedChildrenVerificationFailed { .. }
            | EventV0::RootFinalizerRemoved { .. }
            | EventV0::RootUidVerifiedAbsent { .. }
            | EventV0::RootUidAbsenceFailed { .. }
    )
}

fn root_only_command_allowed(command: &CommandV0) -> bool {
    matches!(
        command,
        CommandV0::VerifyOwnedChildrenAbsent { .. }
            | CommandV0::RemoveRootFinalizer { .. }
            | CommandV0::VerifyRootUidAbsent { .. }
            | CommandV0::Hold { .. }
            | CommandV0::IgnoredStale { .. }
    )
}

fn is_effect_ack_pending(phase: PhaseV0) -> bool {
    matches!(
        phase,
        PhaseV0::DispatchAuthorizationPersistPending
            | PhaseV0::MutationPending
            | PhaseV0::AutomationEnsurePending
            | PhaseV0::DeletionMutationPending
            | PhaseV0::DeletionAutomationCleanupPending
    )
}

fn root_refresh_compatible(
    state: &MachineStateV0,
    root: &RootSnapshotV0,
) -> Result<(), &'static str> {
    if root.uid != state.root.uid {
        return Err("fresh root UID replacement is not adopted");
    }
    if root.namespace != state.root.namespace {
        return Err("fresh root namespace changed");
    }
    if root.name != state.root.name {
        return Err("fresh root name changed");
    }
    if !state.root.state_name.is_empty()
        && (root.state_name != state.root.state_name || root.state_uid != state.root.state_uid)
    {
        return Err("fresh root state identity changed");
    }
    let deleting = state.journal.state.deletion.is_some() || state.root.deleting || root.deleting;
    if !deleting && root.bundle_digest != state.root.bundle_digest {
        return Err("nondeleting root changed immutable bundle digest");
    }
    if !deleting && root.spec.program != state.root.spec.program {
        return Err("nondeleting root changed immutable program reference");
    }
    Ok(())
}

fn install_root_refresh(state: &mut MachineStateV0, mut root: RootSnapshotV0) {
    // Once deletion is observed it is absorbing for this root UID. A later
    // same-UID update may advance generation/RV/spec but cannot revive normal
    // reconciliation.
    root.deleting |= state.root.deleting || state.journal.state.deletion.is_some();
    state.root = root;
}

fn root_refreshed(
    state: &mut MachineStateV0,
    mut root: RootSnapshotV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    validate_bootstrap_root(&root)?;
    if state.root_only_deletion_recovery {
        if matches!(
            state.phase,
            PhaseV0::RootDeletionVerifyPending | PhaseV0::Terminal | PhaseV0::Hold
        ) {
            stale(
                commands,
                "root refresh cannot replace post-finalizer UID absence verification",
            );
            return Ok(());
        }
        let current_receipt = validate_deleting_root_receipt(&state.root)?.clone();
        let receipt = match validate_deleting_root_receipt(&root) {
            Ok(receipt)
                if root.uid == state.root.uid
                    && root.namespace == state.root.namespace
                    && root.root_finalizer_present == state.root.root_finalizer_present
                    && receipt == &current_receipt =>
            {
                receipt.clone()
            }
            Ok(_) | Err(_) => {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: "root-only deletion refresh changed UID, namespace, finalizer, deletion state, or cleanup receipt".into(),
                });
                return Ok(());
            }
        };
        root.state_name.clone_from(&receipt.state_name);
        root.state_uid.clone_from(&receipt.state_uid);
        root.state_resource_version.clear();
        root.state_finalizer_present = false;
        state
            .journal
            .root
            .bundle_digest
            .clone_from(&root.bundle_digest);
        state.root = root;
        if state.root.root_finalizer_present {
            let token = owned_children_token(state).expect("receipt recovery has deletion record");
            state.phase = PhaseV0::OwnedChildrenVerifyPending;
            commands.push(CommandV0::VerifyOwnedChildrenAbsent {
                token,
                preconditions: pins(&state.root),
            });
        } else {
            state.phase = PhaseV0::RootDeletionVerifyPending;
            commands.push(CommandV0::VerifyRootUidAbsent {
                token: root_absence_token(state),
                root_uid: state.root.uid.clone(),
                preconditions: pins(&state.root),
            });
        }
        return Ok(());
    }
    if let Err(reason) = root_refresh_compatible(state, &root) {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: reason.into(),
        });
        return Ok(());
    }
    if root == state.root {
        stale(commands, "duplicate root refresh");
        return Ok(());
    }
    if matches!(
        state.phase,
        PhaseV0::RootDeletionVerifyPending | PhaseV0::Terminal
    ) {
        stale(
            commands,
            "root refresh cannot replace post-finalizer UID absence verification",
        );
        return Ok(());
    }
    if has_may_have_issued(state) {
        install_root_refresh(state, root);
        state.runtime.pending_dispatch = None;
        state.runtime.pending_root_refresh = None;
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "root refresh preserves a may-have-issued mutation; manual recovery required"
                .into(),
        });
        return Ok(());
    }
    if is_effect_ack_pending(state.phase) {
        state.runtime.pending_root_refresh = Some(root);
        stale(
            commands,
            "root refresh coalesced behind effect acknowledgement",
        );
        return Ok(());
    }
    if let Some(operation) = state.journal.state.pending_persist.clone()
        && let Some(purpose) = effect_closure_purpose(operation.purpose)
    {
        let source = state.root.clone();
        install_root_refresh(state, root);
        state.journal.state.generation_advance_closure = Some(GenerationAdvanceClosure {
            root_uid: state.root.uid.clone(),
            source_generation: source.generation,
            source_root_resource_version: source.root_resource_version,
            source_state_uid: source.state_uid,
            source_state_resource_version: source.state_resource_version,
            rebase_count: 0,
            purpose,
        });
        return queue_persist(
            state,
            PersistPurpose::GenerationAdvanceClosure,
            DurablePhase::GenerationAdvanceClosure,
            PhaseV0::GenerationAdvanceClosurePersistPending,
            commands,
        );
    }
    if state.phase == PhaseV0::GenerationAdvanceClosurePersistPending {
        let Some(mut closure) = state.journal.state.generation_advance_closure.clone() else {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: "generation closure phase has no closure".into(),
            });
            return Ok(());
        };
        if closure.rebase_count >= MAX_CLOSURE_REBASES {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: "generation acknowledgement closure rebase bound exceeded".into(),
            });
            return Ok(());
        }
        install_root_refresh(state, root);
        closure.rebase_count += 1;
        state.journal.state.generation_advance_closure = Some(closure);
        return queue_persist(
            state,
            PersistPurpose::GenerationAdvanceClosure,
            DurablePhase::GenerationAdvanceClosure,
            PhaseV0::GenerationAdvanceClosurePersistPending,
            commands,
        );
    }

    let deleting = state.journal.state.deletion.is_some() || root.deleting;
    install_root_refresh(state, root);
    state.runtime.pending_root_refresh = None;
    if deleting {
        state.phase = PhaseV0::DeletionObservationPending;
        commands.push(CommandV0::RequestDeletionObservation {
            preconditions: pins(&state.root),
        });
        return Ok(());
    }

    // A fresh same-bundle root generation invalidates every plan computed
    // under the old pins. The durable invalidate lands before a new render.
    state.frozen = None;
    state.journal.state.apply_transaction = None;
    state.journal.state.pending_dependencies.clear();
    state.journal.state.prune_candidate = None;
    queue_persist(
        state,
        PersistPurpose::Invalidate,
        DurablePhase::Invalidate,
        PhaseV0::PersistPending,
        commands,
    )
}

fn reconcile_requested(
    state: &mut MachineStateV0,
    cause: InternalReconcileCauseV0,
    observation_token: &str,
    cache_epoch: &str,
    commands: &mut Vec<CommandV0>,
) {
    if observation_token.is_empty()
        || observation_token.len() > MAX_IDENTIFIER_BYTES
        || cache_epoch.is_empty()
        || cache_epoch.len() > MAX_IDENTIFIER_BYTES
    {
        stale(commands, "dependency reconcile token is invalid");
        return;
    }
    if state.phase != PhaseV0::Ready || state.root.deleting {
        stale(commands, "dependency reconcile request phase mismatch");
        return;
    }
    match cause {
        InternalReconcileCauseV0::DependencyWatch | InternalReconcileCauseV0::PruneConfirmation => {
        }
    }
    state.phase = PhaseV0::RenderPending;
    let mut preconditions = pins(&state.root);
    preconditions.observation_token = observation_token.into();
    preconditions.cache_epoch = cache_epoch.into();
    commands.push(CommandV0::RequestRender { preconditions });
}

/// Replace a non-durable informer/render pass with the exact authoritative
/// pins from a retained-watch snapshot.  In particular, this intentionally
/// does not use `ReconcileRequested`: that token-only event is legal only in
/// `Ready`, where no command ownership needs to be cancelled.
fn observation_superseded(
    state: &mut MachineStateV0,
    preconditions: PreconditionsV0,
    commands: &mut Vec<CommandV0>,
) {
    if state.root.deleting
        || !matches!(
            state.phase,
            PhaseV0::InformersPending | PhaseV0::RenderPending | PhaseV0::RevalidatePending
        )
        || preconditions.root_uid != state.root.uid
        || preconditions.root_generation != state.root.generation
        || preconditions.root_resource_version != state.root.root_resource_version
        || preconditions.state_uid != state.root.state_uid
        || preconditions.state_resource_version != state.root.state_resource_version
        || preconditions.observation_token.is_empty()
        || preconditions.observation_token.len() > MAX_IDENTIFIER_BYTES
        || preconditions.cache_epoch.is_empty()
        || preconditions.cache_epoch.len() > MAX_IDENTIFIER_BYTES
    {
        stale(commands, "observation supersession phase or pins mismatch");
        return;
    }
    state.frozen = None;
    state.runtime.pending_observation = Some(preconditions);
    state.phase = PhaseV0::InformersPending;
}

fn start(state: &mut MachineStateV0, commands: &mut Vec<CommandV0>) {
    if state.root_only_deletion_recovery {
        if state.phase != PhaseV0::Recovery {
            stale(commands, "root-only deletion recovery is already active");
            return;
        }
        state.runtime = RuntimeStateV0::default();
        if state.root.root_finalizer_present {
            let token = owned_children_token(state).expect("receipt recovery has deletion record");
            state.phase = PhaseV0::OwnedChildrenVerifyPending;
            commands.push(CommandV0::VerifyOwnedChildrenAbsent {
                token,
                preconditions: pins(&state.root),
            });
        } else {
            state.phase = PhaseV0::RootDeletionVerifyPending;
            commands.push(CommandV0::VerifyRootUidAbsent {
                token: root_absence_token(state),
                root_uid: state.root.uid.clone(),
                preconditions: pins(&state.root),
            });
        }
        return;
    }
    if state.phase == PhaseV0::Recovery {
        // Recovery deliberately throws away serialized continuations. It will
        // start again at the descriptor boundary after verifying the durable
        // link, never issue a remembered mutation or status write.
        state.phase = PhaseV0::Bootstrap;
    }
    if state.runtime.requires_revalidation || state.phase == PhaseV0::Bootstrap {
        state.runtime = RuntimeStateV0 {
            requires_revalidation: false,
            pending_automation: None,
            pending_root_status: None,
            pending_bootstrap: None,
            pending_descriptor_token: None,
            pending_root_refresh: None,
            pending_observation: None,
            pending_dispatch: None,
        };
        if !bootstrap_confirmed(state) {
            issue_bootstrap(state, commands);
            return;
        }
        if state
            .journal
            .state
            .apply_transaction
            .as_ref()
            .is_some_and(|tx| {
                tx.next_command_lifecycle.state == NextCommandLifecycleStateV0::LegacyUnknown
            })
        {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: "legacy prepared transaction lacks issuance lifecycle; refusing replay"
                    .into(),
            });
            return;
        }
        if let Some(closure) = state.journal.state.generation_advance_closure.take() {
            // Loading a journal which contains the closure proves the
            // persistence-only handoff landed. Clear the marker with another
            // effect-free write; never recreate the acknowledged effect.
            state.journal.state.pending_persist = None;
            let normal_closure = matches!(
                closure.purpose,
                GenerationAdvanceClosurePurpose::MutationResult
                    | GenerationAdvanceClosurePurpose::AutomationPending
                    | GenerationAdvanceClosurePurpose::AutomationCompletion
            );
            let deletion_closure = matches!(
                closure.purpose,
                GenerationAdvanceClosurePurpose::DeletionMutationResult
                    | GenerationAdvanceClosurePurpose::DeletionAutomationCleanup
            );
            let result = if state.root.deleting && normal_closure {
                // Deletion is absorbing even when the closure write landed but
                // its acknowledgement was lost to a crash. The accepted facts
                // remain in the journal, but no normal invalidate/completion
                // work may resume under a deleting root.
                begin_deletion_durable(state, commands)
            } else if deletion_closure
                && (!state.root.deleting || state.journal.state.deletion.is_none())
            {
                Err(ReconcileError::Recovery(
                    "deletion closure recovered without durable deletion authority".into(),
                ))
            } else {
                match closure.purpose {
                    GenerationAdvanceClosurePurpose::MutationResult => {
                        state.journal.state.apply_transaction = None;
                        state.journal.state.pending_dependencies.clear();
                        state.journal.state.prune_candidate = None;
                        queue_persist(
                            state,
                            PersistPurpose::Invalidate,
                            DurablePhase::Invalidate,
                            PhaseV0::PersistPending,
                            commands,
                        )
                    }
                    GenerationAdvanceClosurePurpose::AutomationPending
                    | GenerationAdvanceClosurePurpose::AutomationCompletion => queue_persist(
                        state,
                        PersistPurpose::Completion,
                        DurablePhase::Completion,
                        PhaseV0::PersistPending,
                        commands,
                    ),
                    GenerationAdvanceClosurePurpose::DeletionMutationResult => queue_persist(
                        state,
                        PersistPurpose::DeletionMutationResult,
                        DurablePhase::DeletionMutationResult,
                        PhaseV0::DeletionMutationPersistPending,
                        commands,
                    ),
                    GenerationAdvanceClosurePurpose::DeletionAutomationCleanup => queue_persist(
                        state,
                        PersistPurpose::DeletionAutomationCleanup,
                        DurablePhase::DeletionAutomationCleanup,
                        PhaseV0::DeletionAutomationPersistPending,
                        commands,
                    ),
                }
            };
            if let Err(error) = result {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: error.to_string(),
                });
            }
            return;
        }
        if state.root.deleting && state.journal.state.deletion.is_none() {
            if let Err(error) = begin_deletion_durable(state, commands) {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: error.to_string(),
                });
            }
            return;
        }
        if state.journal.state.deletion.is_some() {
            if has_may_have_issued(state) {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason:
                        "deletion preserves a may-have-issued mutation; manual recovery required"
                            .into(),
                });
            } else if state.journal.state.apply_transaction.is_some() {
                state.phase = PhaseV0::DeletionObservationPending;
                commands.push(CommandV0::RequestDeletionObservation {
                    preconditions: pins(&state.root),
                });
            } else if let Err(error) = deletion_prepare(state, commands) {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: error.to_string(),
                });
            }
            return;
        }
        request_descriptor(state, commands);
    } else {
        stale(commands, "start phase already active");
    }
}

fn issue_bootstrap(state: &mut MachineStateV0, commands: &mut Vec<CommandV0>) {
    let state_name = match state_name(&state.root.uid) {
        Ok(name) => name,
        Err(error) => {
            stale(
                commands,
                &format!("cannot derive bootstrap state name: {error}"),
            );
            return;
        }
    };
    let token = hash(&[
        "bootstrap/v0",
        &state.root.uid,
        &state.root.generation.to_string(),
        &state.root.root_resource_version,
        &state.root.bundle_digest,
        &state_name,
    ]);
    state.runtime.pending_bootstrap = Some(PendingBootstrapV0 {
        token: token.clone(),
        state: state.journal.clone(),
        state_name: state_name.clone(),
    });
    state.phase = PhaseV0::BootstrapPending;
    commands.push(CommandV0::EnsureBootstrap {
        token,
        state: state.journal.clone(),
        root_uid: state.root.uid.clone(),
        root_generation: state.root.generation,
        root_resource_version: state.root.root_resource_version.clone(),
        state_name,
    });
}

#[allow(clippy::too_many_arguments)]
fn bootstrap_ack(
    state: &mut MachineStateV0,
    token: &str,
    state_name: &str,
    state_uid: &str,
    state_resource_version: &str,
    old_root_resource_version: &str,
    new_root_resource_version: &str,
    status_state_link: &oxideloop_controller_api::StateLink,
    root_finalizer_present: bool,
    state_finalizer_present: bool,
    commands: &mut Vec<CommandV0>,
) {
    let Some(pending) = state.runtime.pending_bootstrap.clone() else {
        stale(
            commands,
            "bootstrap acknowledgement has no issued operation",
        );
        return;
    };
    let valid = state.phase == PhaseV0::BootstrapPending
        && pending.token == token
        && pending.state_name == state_name
        && oxideloop_controller_api::state_name(state.root.uid.as_str())
            .is_ok_and(|expected| expected == state_name)
        && !state_uid.is_empty()
        && !state_resource_version.is_empty()
        && old_root_resource_version == state.root.root_resource_version
        && !new_root_resource_version.is_empty()
        && new_root_resource_version != old_root_resource_version
        && root_finalizer_present
        && state_finalizer_present
        && status_state_link.root_uid == state.root.uid
        && status_state_link.name == state_name
        && status_state_link.uid == state_uid;
    if !valid {
        stale(
            commands,
            "bootstrap acknowledgement token/link/finalizer/RV mismatch",
        );
        return;
    }
    state.root.state_name = state_name.into();
    state.root.state_uid = state_uid.into();
    state.root.state_resource_version = state_resource_version.into();
    state.root.root_resource_version = new_root_resource_version.into();
    state.root.root_finalizer_present = true;
    state.root.state_finalizer_present = true;
    state.root.status.state = Some(status_state_link.clone());
    state.runtime.pending_bootstrap = None;
    request_descriptor(state, commands);
}

fn request_descriptor(state: &mut MachineStateV0, commands: &mut Vec<CommandV0>) {
    let token = hash(&[
        "descriptor/v0",
        &state.root.uid,
        &state.root.generation.to_string(),
        &state.root.root_resource_version,
        &state.root.state_uid,
        &state.root.state_resource_version,
        &state.root.bundle_digest,
    ]);
    state.runtime.pending_descriptor_token = Some(token.clone());
    state.phase = PhaseV0::DescriptorPending;
    commands.push(CommandV0::RequestDescriptor {
        token,
        preconditions: pins(&state.root),
    });
}

fn descriptor_ready(
    state: &mut MachineStateV0,
    token: &str,
    bundle_digest: &str,
    commands: &mut Vec<CommandV0>,
) {
    if state.phase != PhaseV0::DescriptorPending
        || state.runtime.pending_descriptor_token.as_deref() != Some(token)
        || bundle_digest != state.root.bundle_digest
        || !is_raw_sha256(bundle_digest)
    {
        stale(commands, "descriptor acknowledgement token/digest mismatch");
        return;
    }
    state.runtime.pending_descriptor_token = None;
    let status = root_status_for(&state.root, None, true);
    if !queue_root_status(
        state,
        status,
        RootStatusContinuationV0::Informers,
        None,
        commands,
    ) {
        state.phase = PhaseV0::InformersPending;
    }
}

fn root_status_for(
    root: &RootSnapshotV0,
    frozen: Option<&FrozenInputV0>,
    descriptor_verified: bool,
) -> WebApplicationStatus {
    let mut status = root.status.clone();
    status.observed_generation = Some(root.generation);
    if descriptor_verified {
        status.verified_bundle_digest = Some(root.bundle_digest.clone());
    }
    if let Some(frozen) = frozen {
        status.input_token = Some(frozen.observation_token.clone());
        status.cache_epoch = frozen.cache_epoch.parse().ok();
    }
    status.conditions.sort_by(|left, right| {
        (format!("{:?}", left.type_), &left.reason)
            .cmp(&(format!("{:?}", right.type_), &right.reason))
    });
    status
}

fn queue_root_status(
    state: &mut MachineStateV0,
    status: WebApplicationStatus,
    continuation: RootStatusContinuationV0,
    frozen: Option<&FrozenInputV0>,
    commands: &mut Vec<CommandV0>,
) -> bool {
    if status == state.root.status {
        return false;
    }
    // Do not include this operation proof in its own hash: that would make
    // token construction circular and would cause a perpetual status diff.
    let status_hash = sha256_hex(
        &canonical_bytes(&serde_json::to_value(&status).expect("root status serializable"))
            .expect("root status canonical"),
    );
    let token = hash(&[
        "root-status/v0",
        &state.root.uid,
        &state.root.generation.to_string(),
        &state.root.root_resource_version,
        &status_hash,
    ]);
    let mut status = status;
    status.last_status_persist_token = Some(token.clone());
    state.runtime.pending_root_status = Some(PendingRootStatusV0 {
        token: token.clone(),
        old_resource_version: state.root.root_resource_version.clone(),
        status: status.clone(),
        continuation,
        render_input: frozen.map(|frozen| RootStatusContinuationPayloadV0 {
            observation_token: frozen.observation_token.clone(),
            cache_epoch: frozen.cache_epoch.clone(),
        }),
    });
    state.phase = PhaseV0::RootStatusPersistPending;
    commands.push(CommandV0::PersistRootStatus {
        token,
        root_uid: state.root.uid.clone(),
        old_root_resource_version: state.root.root_resource_version.clone(),
        preconditions: pins(&state.root),
        status,
    });
    true
}

fn root_status_ack(
    state: &mut MachineStateV0,
    token: &str,
    old: &str,
    new: &str,
    uid: &str,
    commands: &mut Vec<CommandV0>,
) {
    let Some(pending) = state.runtime.pending_root_status.clone() else {
        stale(
            commands,
            "root status acknowledgement has no issued operation",
        );
        return;
    };
    if state.phase != PhaseV0::RootStatusPersistPending
        || pending.token != token
        || pending.old_resource_version != old
        || old != state.root.root_resource_version
        || new.is_empty()
        || new == old
        || uid != state.root.uid
    {
        stale(
            commands,
            "root status acknowledgement token/RV/UID mismatch",
        );
        return;
    }
    let render_input = pending.render_input.as_ref();
    if render_input.is_some() != matches!(pending.continuation, RootStatusContinuationV0::Render)
        || matches!(pending.continuation, RootStatusContinuationV0::Render)
            && !valid_render_continuation(render_input, &pending.status)
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "root status continuation payload is invalid or mismatched".into(),
        });
        return;
    }
    state.root.root_resource_version = new.into();
    state.root.status = pending.status;
    state.runtime.pending_root_status = None;
    match pending.continuation {
        RootStatusContinuationV0::Informers => state.phase = PhaseV0::InformersPending,
        RootStatusContinuationV0::Render => {
            state.phase = PhaseV0::RenderPending;
            let mut preconditions = pins(&state.root);
            let payload = render_input.expect("validated render continuation payload");
            preconditions
                .observation_token
                .clone_from(&payload.observation_token);
            preconditions.cache_epoch.clone_from(&payload.cache_epoch);
            commands.push(CommandV0::RequestRender { preconditions });
        }
        RootStatusContinuationV0::DeletionStateFinalizer => {
            state.phase = PhaseV0::StateFinalizerPending;
            commands.push(CommandV0::RemoveStateFinalizer {
                token: state_finalizer_token(state),
                preconditions: pins(&state.root),
            });
        }
    }
}

fn valid_render_continuation(
    payload: Option<&RootStatusContinuationPayloadV0>,
    pending_status: &WebApplicationStatus,
) -> bool {
    let Some(payload) = payload else { return false };
    let Some(status_token) = pending_status.input_token.as_deref() else {
        return false;
    };
    let Some(status_epoch) = pending_status.cache_epoch else {
        return false;
    };
    !payload.observation_token.is_empty()
        && payload.observation_token.len() <= MAX_IDENTIFIER_BYTES
        && !payload.cache_epoch.is_empty()
        && payload.cache_epoch.len() <= MAX_IDENTIFIER_BYTES
        && payload
            .cache_epoch
            .parse::<u64>()
            .is_ok_and(|epoch| epoch.to_string() == payload.cache_epoch)
        && payload.observation_token == status_token
        && payload.cache_epoch.parse::<u64>().ok() == Some(status_epoch)
}

fn begin_deletion(mut state: MachineStateV0) -> Result<TransitionV0, ReconcileError> {
    state.root.deleting = true;
    // A deleting root is never bootstrapped. If its internal state is already
    // gone, only the bounded status receipt is admissible recovery evidence;
    // missing or mismatched evidence holds without creating anything.
    if !bootstrap_confirmed(&state) {
        let reason = validate_deleting_root_receipt(&state.root)
            .err()
            .map_or_else(
                || "deleting root state is absent; receipt recovery requires a fresh owned-child verification".into(),
                |error| error.to_string(),
            );
        state.phase = PhaseV0::Hold;
        return finish(state, vec![CommandV0::Hold { reason }]);
    }
    if state.journal.state.deletion.is_some() {
        return finish(
            state,
            vec![CommandV0::IgnoredStale {
                reason: "deletion is already durable".into(),
            }],
        );
    }
    let mut commands = Vec::new();
    begin_deletion_durable(&mut state, &mut commands)?;
    finish(state, commands)
}

fn begin_deletion_durable(
    state: &mut MachineStateV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if state.journal.state.deletion.is_some() {
        if has_may_have_issued(state) {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: "deletion preserves a may-have-issued mutation; manual recovery required"
                    .into(),
            });
            return Ok(());
        }
        state.phase = PhaseV0::DeletionObservationPending;
        commands.push(CommandV0::RequestDeletionObservation {
            preconditions: pins(&state.root),
        });
        return Ok(());
    }
    // Remove every normal issuance capability. A may-have-issued normal
    // mutation is the one exception: deletion must preserve it and stop,
    // rather than incorrectly claiming that no normal request could land.
    let preserve_may_have_issued = has_may_have_issued(state);
    state.runtime.pending_automation = None;
    state.runtime.pending_root_refresh = None;
    state.runtime.pending_dispatch = None;
    state.frozen = None;
    if !preserve_may_have_issued {
        state.journal.state.apply_transaction = None;
    }
    state.journal.state.pending_dependencies.clear();
    if !preserve_may_have_issued {
        state.journal.state.prune_candidate = None;
    }
    state.journal.state.deletion = Some(DeletionTransaction {
        id: hash(&[
            "deletion/v0",
            &state.root.uid,
            &state.root.generation.to_string(),
            &state.root.root_resource_version,
            &state.root.state_resource_version,
        ]),
        deletion_start_generation: state.root.generation,
    });
    queue_persist(
        state,
        PersistPurpose::DeletionStart,
        DurablePhase::DeletionStart,
        PhaseV0::DeletionStartPersistPending,
        commands,
    )
}

/// Validates the only bounded evidence accepted when a deleting root's
/// internal state object is already absent. This function is pure and never
/// grants mutation authority by itself; an adapter must still perform a fresh
/// owned-child absence check before asking to remove the root finalizer.
pub fn validate_deleting_root_receipt(
    root: &RootSnapshotV0,
) -> Result<&DeletionCleanupReceipt, ReconcileError> {
    if !root.deleting {
        return Err(ReconcileError::Recovery(
            "receipt recovery requires a deleting root".into(),
        ));
    }
    let receipt = root
        .status
        .deletion_cleanup_receipt
        .as_ref()
        .ok_or_else(|| ReconcileError::Recovery("deleting root has no cleanup receipt".into()))?;
    receipt
        .validate()
        .map_err(|error| ReconcileError::Recovery(error.to_string()))?;
    let expected_state_name =
        state_name(&root.uid).map_err(|error| ReconcileError::Recovery(error.to_string()))?;
    if receipt.root_uid != root.uid
        || receipt.state_name != expected_state_name
        || (!root.state_name.is_empty() && receipt.state_name != root.state_name)
        || (!root.state_uid.is_empty() && receipt.state_uid != root.state_uid)
    {
        return Err(ReconcileError::Recovery(
            "deletion cleanup receipt identity mismatch".into(),
        ));
    }
    if let Some(link) = &root.status.state
        && (link.root_uid != root.uid
            || link.name != receipt.state_name
            || link.uid != receipt.state_uid)
    {
        return Err(ReconcileError::Recovery(
            "deletion cleanup receipt state link mismatch".into(),
        ));
    }
    Ok(receipt)
}

fn queue_persist(
    state: &mut MachineStateV0,
    mut purpose: PersistPurpose,
    mut durable_phase: DurablePhase,
    mut phase: PhaseV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if let Some(closure_purpose) = effect_closure_purpose(purpose)
        && let Some(root) = state.runtime.pending_root_refresh.take()
    {
        if let Err(reason) = root_refresh_compatible(state, &root) {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: reason.into(),
            });
            return Ok(());
        }
        let source = state.root.clone();
        install_root_refresh(state, root);
        state.journal.state.generation_advance_closure = Some(GenerationAdvanceClosure {
            root_uid: state.root.uid.clone(),
            source_generation: source.generation,
            source_root_resource_version: source.root_resource_version,
            source_state_uid: source.state_uid,
            source_state_resource_version: source.state_resource_version,
            rebase_count: 0,
            purpose: closure_purpose,
        });
        purpose = PersistPurpose::GenerationAdvanceClosure;
        durable_phase = DurablePhase::GenerationAdvanceClosure;
        phase = PhaseV0::GenerationAdvanceClosurePersistPending;
    }
    state.journal.state.pending_persist = None;
    let body = serde_json::to_value(&state.journal.state)
        .map_err(|e| ReconcileError::Link(e.to_string()))?;
    let body_hash =
        sha256_hex(&canonical_bytes(&body).map_err(|e| ReconcileError::Link(e.to_string()))?);
    let operation = PersistOperation {
        token: hash(&[
            "persist/v0",
            &format!("{durable_phase:?}"),
            &format!("{purpose:?}"),
            &state.root.uid,
            &state.root.generation.to_string(),
            &state.root.root_resource_version,
            &state.root.state_resource_version,
            &body_hash,
        ]),
        purpose,
        phase: durable_phase,
        old_state_resource_version: state.root.state_resource_version.clone(),
        root_generation: state.root.generation,
    };
    state.journal.state.pending_persist = Some(operation.clone());
    state.phase = phase;
    commands.push(CommandV0::Persist {
        state: state.journal.clone(),
        preconditions: pins(&state.root),
        operation,
    });
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn persist_ack(
    state: &mut MachineStateV0,
    token: &str,
    old: &str,
    new: &str,
    uid: &str,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    let Some(operation) = state.journal.state.pending_persist.clone() else {
        stale(commands, "persist acknowledgement has no operation");
        return Ok(());
    };
    // This complete check deliberately happens before a single state mutation.
    if token != operation.token
        || old != operation.old_state_resource_version
        || old != state.root.state_resource_version
        || new.is_empty()
        || new == old
        || uid != state.root.state_uid
        || operation.root_generation != state.root.generation
        || operation.phase != operation.purpose.durable_phase()
        || !phase_matches(state.phase, operation.purpose)
    {
        stale(
            commands,
            "persist acknowledgement token/RV/UID/phase mismatch",
        );
        return Ok(());
    }
    state.root.state_resource_version = new.into();
    state.journal.state.pending_persist = None;
    match operation.purpose {
        PersistPurpose::Prepare | PersistPurpose::MutationResult => {
            if let Some(frozen) = &mut state.frozen {
                frozen
                    .state_resource_version
                    .clone_from(&state.root.state_resource_version);
            }
            state.phase = PhaseV0::RevalidatePending;
            commands.push(CommandV0::RequestRender {
                preconditions: pins(&state.root),
            });
        }
        PersistPurpose::DispatchAuthorization => {
            let Some(mut pending) = state.runtime.pending_dispatch.take() else {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason:
                        "dispatch authorization acknowledgement lost its exact command capability"
                            .into(),
                });
                return Ok(());
            };
            let Some(tx) = state.journal.state.apply_transaction.as_ref() else {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: "dispatch authorization acknowledgement has no transaction".into(),
                });
                return Ok(());
            };
            let exact = matches!(
                &tx.next_command_lifecycle,
                NextCommandLifecycleV0 {
                    state: NextCommandLifecycleStateV0::DispatchAuthorizedMayHaveIssued,
                    command_id: Some(command_id),
                } if command_id == &pending.command.command_id
            ) && next_unrecorded_intent(tx)
                .is_some_and(|intent| intent.command_id == pending.command.command_id)
                && tx.id == pending.transaction_id
                && tx.attempt_token == pending.attempt_token;
            if !exact {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason:
                        "dispatch authorization acknowledgement does not match the retained command"
                            .into(),
                });
                return Ok(());
            }
            pending
                .command
                .preconditions
                .state_resource_version
                .clone_from(&state.root.state_resource_version);
            state.phase = if state.journal.state.deletion.is_some() {
                PhaseV0::DeletionMutationPending
            } else {
                PhaseV0::MutationPending
            };
            commands.push(CommandV0::Mutate {
                transaction_id: pending.transaction_id,
                attempt_token: pending.attempt_token,
                commands: vec![pending.command],
            });
        }
        PersistPurpose::Invalidate => {
            state.phase = PhaseV0::RenderPending;
            commands.push(CommandV0::RequestRender {
                preconditions: pins(&state.root),
            });
        }
        PersistPurpose::Completion
        | PersistPurpose::AutomationBaseline
        | PersistPurpose::AutomationPending
        | PersistPurpose::AutomationCompletion => state.phase = PhaseV0::Ready,
        PersistPurpose::DeletionStart => {
            if has_may_have_issued(state) {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason:
                        "deletion preserves a may-have-issued mutation; manual recovery required"
                            .into(),
                });
            } else {
                deletion_prepare(state, commands)?;
            }
        }
        PersistPurpose::DeletionPrepare => {
            state.phase = PhaseV0::DeletionObservationPending;
            commands.push(CommandV0::RequestDeletionObservation {
                preconditions: pins(&state.root),
            });
        }
        PersistPurpose::DeletionMutationResult | PersistPurpose::DeletionAutomationCleanup => {
            deletion_advance(state, commands)?
        }
        PersistPurpose::GenerationAdvanceClosure => {
            let Some(closure) = state.journal.state.generation_advance_closure.take() else {
                state.phase = PhaseV0::Hold;
                commands.push(CommandV0::Hold {
                    reason: "generation closure acknowledgement has no durable closure".into(),
                });
                return Ok(());
            };
            match closure.purpose {
                GenerationAdvanceClosurePurpose::MutationResult => {
                    if state.root.deleting {
                        begin_deletion_durable(state, commands)?;
                    } else {
                        state.frozen = None;
                        state.journal.state.apply_transaction = None;
                        state.journal.state.pending_dependencies.clear();
                        state.journal.state.prune_candidate = None;
                        queue_persist(
                            state,
                            PersistPurpose::Invalidate,
                            DurablePhase::Invalidate,
                            PhaseV0::PersistPending,
                            commands,
                        )?;
                    }
                }
                GenerationAdvanceClosurePurpose::AutomationPending
                | GenerationAdvanceClosurePurpose::AutomationCompletion => {
                    if state.root.deleting {
                        begin_deletion_durable(state, commands)?;
                    } else {
                        state.phase = PhaseV0::Ready;
                    }
                }
                GenerationAdvanceClosurePurpose::DeletionMutationResult
                | GenerationAdvanceClosurePurpose::DeletionAutomationCleanup => {
                    deletion_advance(state, commands)?;
                }
            }
        }
    }
    Ok(())
}

fn phase_matches(phase: PhaseV0, purpose: PersistPurpose) -> bool {
    match purpose {
        PersistPurpose::Prepare
        | PersistPurpose::Invalidate
        | PersistPurpose::MutationResult
        | PersistPurpose::Completion => phase == PhaseV0::PersistPending,
        PersistPurpose::DispatchAuthorization => {
            phase == PhaseV0::DispatchAuthorizationPersistPending
        }
        PersistPurpose::AutomationBaseline
        | PersistPurpose::AutomationPending
        | PersistPurpose::AutomationCompletion => phase == PhaseV0::AutomationPersistPending,
        PersistPurpose::DeletionStart => phase == PhaseV0::DeletionStartPersistPending,
        PersistPurpose::DeletionPrepare => phase == PhaseV0::DeletionPreparePersistPending,
        PersistPurpose::DeletionMutationResult => phase == PhaseV0::DeletionMutationPersistPending,
        PersistPurpose::DeletionAutomationCleanup => {
            phase == PhaseV0::DeletionAutomationPersistPending
        }
        PersistPurpose::GenerationAdvanceClosure => {
            phase == PhaseV0::GenerationAdvanceClosurePersistPending
        }
    }
}

fn deletion_prepare(
    state: &mut MachineStateV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    let deletion_id = state
        .journal
        .state
        .deletion
        .as_ref()
        .ok_or_else(|| ReconcileError::Link("deletion record disappeared".into()))?
        .id
        .clone();
    let deletion_start_generation = state
        .journal
        .state
        .deletion
        .as_ref()
        .expect("deletion checked above")
        .deletion_start_generation;
    let mut intents = state
        .journal
        .state
        .inventory
        .iter()
        .map(|entry| ApplyIntent {
            command_id: hash(&[
                "deletion-command/v0",
                &deletion_id,
                &identity_key(&entry.identity),
                &entry.uid,
                &entry.resource_version,
            ]),
            operation: ApplyOperation::Delete,
            identity: entry.identity.clone(),
            logical_id: entry.logical_id.clone(),
            expected_uid: Some(entry.uid.clone()),
            expected_resource_version: Some(entry.resource_version.clone()),
        })
        .collect::<Vec<_>>();
    intents.sort_by(|a, b| a.command_id.cmp(&b.command_id));
    let recovery_claims = intents
        .iter()
        .map(|intent| PreparedRecoveryClaim {
            root_uid: state.root.uid.clone(),
            transaction_id: deletion_id.clone(),
            command_id: intent.command_id.clone(),
        })
        .collect();
    let next_command_lifecycle = match intents.first() {
        Some(intent) => NextCommandLifecycleV0::prepared(intent.command_id.clone()),
        None => NextCommandLifecycleV0::complete(),
    };
    state.journal.state.apply_transaction = Some(ApplyTransaction {
        id: deletion_id,
        generation: deletion_start_generation,
        frozen_render_input_hash: "0".repeat(64),
        desired_graph_hash: hash(&[
            "deletion-graph/v0",
            &state.root.uid,
            &state.root.root_resource_version,
            &inventory_hash(&state.journal.state.inventory),
        ]),
        recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
        attempt_token: hash(&[
            "deletion-attempt/v0",
            &state.root.uid,
            &state.root.root_resource_version,
            &state.root.state_resource_version,
        ]),
        prepared_intents: intents,
        next_command_lifecycle,
        recovery_claims,
        results: vec![],
        unresolved_identities: vec![],
    });
    queue_persist(
        state,
        PersistPurpose::DeletionPrepare,
        DurablePhase::DeletionPrepare,
        PhaseV0::DeletionPreparePersistPending,
        commands,
    )
}

fn deletion_revalidated(
    state: &mut MachineStateV0,
    observed_inventory: Vec<InventoryEntry>,
    observed_automation_runs: Vec<AutomationRunCleanupV0>,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if state.phase != PhaseV0::DeletionObservationPending || state.journal.state.deletion.is_none()
    {
        stale(commands, "deletion revalidation phase mismatch");
        return Ok(());
    }
    let known_inventory = state
        .journal
        .state
        .inventory
        .iter()
        .map(|item| (identity_key(&item.identity), item.clone()))
        .collect::<BTreeMap<_, _>>();
    if observed_inventory.len() > MAX_LIST_ENTRIES
        || observed_automation_runs.len() > MAX_LIST_ENTRIES
    {
        stale(commands, "deletion revalidation exceeds durable bounds");
        return Ok(());
    }
    let mut known_runs = BTreeMap::new();
    for pending in &state.journal.state.automation.pending {
        known_runs.insert(
            pending.occurrence_id.clone(),
            (
                pending.run_name.clone(),
                pending.run_uid.clone(),
                pending.run_resource_version.clone(),
            ),
        );
    }
    for completion in &state.journal.state.automation.completions {
        if known_runs
            .insert(
                completion.occurrence_id.clone(),
                (
                    completion.run_name.clone(),
                    completion.run_uid.clone(),
                    completion.completed_resource_version.clone(),
                ),
            )
            .is_some()
        {
            stale(
                commands,
                "deletion journal has duplicate automation occurrence",
            );
            return Ok(());
        }
    }
    let observed_inventory_by_key = observed_inventory
        .iter()
        .map(|item| (identity_key(&item.identity), item))
        .collect::<BTreeMap<_, _>>();
    let observed_runs = observed_automation_runs
        .iter()
        .map(|item| item.occurrence_id.as_str())
        .collect::<BTreeSet<_>>();
    if observed_inventory_by_key.len() != observed_inventory.len()
        || observed_runs.len() != observed_automation_runs.len()
        || !observed_inventory_by_key
            .keys()
            .all(|key| known_inventory.contains_key(key))
        || observed_automation_runs.iter().any(|item| {
            item.occurrence_id.is_empty()
                || item.name.is_empty()
                || item.uid.is_empty()
                || item.resource_version.is_empty()
                || known_runs.get(&item.occurrence_id)
                    != Some(&(
                        item.name.clone(),
                        item.uid.clone(),
                        item.resource_version.clone(),
                    ))
        })
        || observed_inventory
            .iter()
            .any(|item| item.bundle_digest != state.journal.root.bundle_digest)
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "deletion revalidation contains unowned or duplicate child".into(),
        });
        return Ok(());
    }

    // Replacement UIDs and changed resourceVersions are never adopted, and
    // AutomationRun absence is not covered by the managed-child DELETE
    // recovery exception.
    if observed_inventory_by_key.iter().any(|(key, observed)| {
        known_inventory.get(key).is_none_or(|known| {
            known.uid != observed.uid || known.resource_version != observed.resource_version
        })
    }) || observed_runs.len() != known_runs.len()
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "deletion child identity changed or AutomationRun disappeared".into(),
        });
        return Ok(());
    }
    let Some(tx) = state.journal.state.apply_transaction.clone() else {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "deletion observation has no durable prepared transaction".into(),
        });
        return Ok(());
    };
    let Some(intent) = next_unrecorded_intent(&tx) else {
        return deletion_advance(state, commands);
    };
    let key = identity_key(&intent.identity);
    if observed_inventory_by_key.contains_key(&key) {
        if has_may_have_issued(state) {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: "may-have-issued DELETE cannot be replayed during deletion".into(),
            });
            return Ok(());
        }
        return deletion_issue(state, commands);
    }
    if !claim_matches(state, &tx, intent)
        || intent.operation != ApplyOperation::Delete
        || intent.expected_uid.is_none()
        || intent.expected_resource_version.is_none()
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "absent deletion child lacks an exact durable prepared claim".into(),
        });
        return Ok(());
    }
    let recovered = ApplyResult {
        command_id: intent.command_id.clone(),
        attempt_token: tx.attempt_token.clone(),
        identity: intent.identity.clone(),
        outcome: ApplyOutcome::AlreadyAbsent,
        uid: None,
        resource_version: None,
    };
    record_mutation_results(state, &[recovered])?;
    advance_next_command_lifecycle(state)?;
    queue_persist(
        state,
        PersistPurpose::DeletionMutationResult,
        DurablePhase::DeletionMutationResult,
        PhaseV0::DeletionMutationPersistPending,
        commands,
    )
}

fn deletion_issue(
    state: &mut MachineStateV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    let Some(tx) = state.journal.state.apply_transaction.clone() else {
        return deletion_advance(state, commands);
    };
    let effects = deletion_commands(state, &tx);
    if effects.is_empty() {
        deletion_advance(state, commands)
    } else {
        authorize_dispatch(state, &tx, effects, commands)
    }
}

fn deletion_commands(state: &MachineStateV0, tx: &ApplyTransaction) -> Vec<MutationCommandV0> {
    let done = tx
        .results
        .iter()
        .map(|r| r.command_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut commands = tx
        .prepared_intents
        .iter()
        .filter(|i| !done.contains(i.command_id.as_str()))
        .map(|intent| MutationCommandV0 {
            command_id: intent.command_id.clone(),
            attempt_token: tx.attempt_token.clone(),
            preconditions: pins(&state.root),
            identity: intent.identity.clone(),
            logical_id: intent.logical_id.clone(),
            prepared_expected_target: prepared_expected_target(intent)
                .expect("validated delete intent has an exact target identity"),
            decision: MutationDecision::PreconditionedDelete {
                method: HttpMethod::Delete,
                preconditions: MutationIdentity {
                    uid: intent.expected_uid.clone().unwrap_or_default(),
                    resource_version: intent.expected_resource_version.clone().unwrap_or_default(),
                },
            },
        })
        .collect::<Vec<_>>();
    commands.sort_by(|a, b| a.command_id.cmp(&b.command_id));
    commands.into_iter().take(1).collect()
}

fn revalidate(
    state: &mut MachineStateV0,
    frozen: FrozenInputV0,
    input: &str,
    output: &str,
    observed: Vec<DesiredObservationV0>,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if state.phase != PhaseV0::RevalidatePending || !matches_root(state, &frozen) {
        stale(commands, "revalidation phase or frozen pins mismatch");
        return Ok(());
    }
    let tx = state
        .journal
        .state
        .apply_transaction
        .clone()
        .ok_or_else(|| ReconcileError::Mutation("revalidation has no transaction".into()))?;
    let exact_effect_recovery = tx.generation != state.root.generation || state.frozen.is_none();
    if tx.next_command_lifecycle.state == NextCommandLifecycleStateV0::LegacyUnknown {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "legacy prepared transaction lacks issuance lifecycle; refusing replay".into(),
        });
        return Ok(());
    }
    let next_is_post = next_unrecorded_intent(&tx).is_some_and(|intent| {
        intent.operation == ApplyOperation::Apply
            && intent.expected_uid.is_none()
            && intent.expected_resource_version.is_none()
    });
    if matches!(
        tx.next_command_lifecycle.state,
        NextCommandLifecycleStateV0::DispatchAuthorizedMayHaveIssued
    ) && next_is_post
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "may-have-issued POST cannot be recovered without a trusted admission witness"
                .into(),
        });
        return Ok(());
    }
    if !exact_effect_recovery
        && (tx.frozen_render_input_hash != frozen.render_input_hash
            || state
                .frozen
                .as_ref()
                .is_some_and(|previous| previous != &frozen))
    {
        stale(commands, "revalidation frozen hash mismatch");
        return Ok(());
    }
    // Rebuild the complete fresh graph before considering an unpersisted API
    // response. This validates canonical render bytes, root/frozen pins,
    // backend qualification and the desired-plan hash without mutating state.
    // A qualified response-lost proof is projected only for that pure graph
    // check; the actual fresh observation is retained for result recovery.
    let planning_observed = recovered_observation_projection(state, &tx, &observed);
    let plan = plan_graph(state, &frozen, input, output, planning_observed)?;
    if exact_effect_recovery {
        if matches!(
            tx.next_command_lifecycle.state,
            NextCommandLifecycleStateV0::PreparedNotIssued
        ) {
            state.journal.state.apply_transaction = None;
            state.journal.state.pending_dependencies.clear();
            state.journal.state.prune_candidate = None;
            return queue_persist(
                state,
                PersistPurpose::Invalidate,
                DurablePhase::Invalidate,
                PhaseV0::PersistPending,
                commands,
            );
        }
        let recovered = recover_prepared_result(state, &tx, &observed)?;
        if !recovered.is_empty() {
            state.frozen = Some(frozen);
            record_mutation_results(state, &recovered)?;
            return queue_persist(
                state,
                PersistPurpose::MutationResult,
                DurablePhase::MutationResult,
                PhaseV0::PersistPending,
                commands,
            );
        }
        let desired_flipped_present = next_unrecorded_intent(&tx).is_some_and(|intent| {
            intent.operation == ApplyOperation::Delete && plan.hash != tx.desired_graph_hash
        });
        if desired_flipped_present {
            state.journal.state.apply_transaction = None;
            state.journal.state.pending_dependencies.clear();
            state.journal.state.prune_candidate = None;
            return queue_persist(
                state,
                PersistPurpose::Invalidate,
                DurablePhase::Invalidate,
                PhaseV0::PersistPending,
                commands,
            );
        }
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "recovered effect cannot be exactly proven; refusing replay".into(),
        });
        return Ok(());
    }
    if plan.hash != tx.desired_graph_hash || tx.frozen_render_input_hash != frozen.render_input_hash
    {
        state.journal.state.apply_transaction = None;
        state.journal.state.pending_dependencies.clear();
        return queue_persist(
            state,
            PersistPurpose::Invalidate,
            DurablePhase::Invalidate,
            PhaseV0::PersistPending,
            commands,
        );
    }
    if let Some(intent) = next_unrecorded_intent(&tx)
        && intent.operation == ApplyOperation::Delete
    {
        let observation = observed
            .iter()
            .find(|item| item.identity == intent.identity);
        let authorized = observation.is_some_and(|item| match &item.observation {
            Observation::Absent => deletion_recovery_matches(state, &tx, intent, item),
            Observation::Existing { identity } => {
                intent.expected_uid.as_deref() == Some(identity.uid.as_str())
                    && intent.expected_resource_version.as_deref()
                        == Some(identity.resource_version.as_str())
            }
        });
        if !authorized {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: "prepared DELETE fresh read is absent without proof or has changed UID/RV"
                    .into(),
            });
            return Ok(());
        }
    }
    let Ok(effects) = plan.remaining_effects(state, &tx) else {
        state.journal.state.apply_transaction = None;
        state.journal.state.pending_dependencies.clear();
        return queue_persist(
            state,
            PersistPurpose::Invalidate,
            DurablePhase::Invalidate,
            PhaseV0::PersistPending,
            commands,
        );
    };
    let recovered = if matches!(
        tx.next_command_lifecycle.state,
        NextCommandLifecycleStateV0::DispatchAuthorizedMayHaveIssued
    ) {
        recover_prepared_result(state, &tx, &observed)?
    } else {
        Vec::new()
    };
    if !recovered.is_empty() {
        state.frozen = Some(frozen);
        record_mutation_results(state, &recovered)?;
        return queue_persist(
            state,
            PersistPurpose::MutationResult,
            DurablePhase::MutationResult,
            PhaseV0::PersistPending,
            commands,
        );
    }
    state.frozen = Some(frozen);
    if effects.is_empty() {
        state.journal.state.apply_transaction = None;
        state.journal.state.committed_dependencies =
            std::mem::take(&mut state.journal.state.pending_dependencies);
        queue_persist(
            state,
            PersistPurpose::Completion,
            DurablePhase::Completion,
            PhaseV0::PersistPending,
            commands,
        )
    } else {
        authorize_dispatch(state, &tx, effects, commands)
    }
}

fn mutation_ack(
    state: &mut MachineStateV0,
    attempt: &str,
    id: &str,
    results: Vec<ApplyResult>,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    let deleting = state.phase == PhaseV0::DeletionMutationPending;
    if !deleting && state.phase != PhaseV0::MutationPending {
        stale(commands, "mutation acknowledgement phase mismatch");
        return Ok(());
    }
    let Some(tx) = state.journal.state.apply_transaction.as_ref() else {
        stale(commands, "mutation acknowledgement has no transaction");
        return Ok(());
    };
    if let Err(error) = validate_mutation_ack(tx, attempt, id, &results) {
        stale(commands, &error.to_string());
        return Ok(());
    }
    record_mutation_results(state, &results)?;
    advance_next_command_lifecycle(state)?;
    if deleting {
        queue_persist(
            state,
            PersistPurpose::DeletionMutationResult,
            DurablePhase::DeletionMutationResult,
            PhaseV0::DeletionMutationPersistPending,
            commands,
        )
    } else {
        queue_persist(
            state,
            PersistPurpose::MutationResult,
            DurablePhase::MutationResult,
            PhaseV0::PersistPending,
            commands,
        )
    }
}

fn record_mutation_results(
    state: &mut MachineStateV0,
    results: &[ApplyResult],
) -> Result<(), ReconcileError> {
    let tx = state
        .journal
        .state
        .apply_transaction
        .clone()
        .ok_or_else(|| ReconcileError::Mutation("result has no transaction".into()))?;
    for result in results {
        let intent = tx
            .prepared_intents
            .iter()
            .find(|intent| intent.command_id == result.command_id)
            .ok_or_else(|| ReconcileError::Mutation("result command is not prepared".into()))?;
        if intent.operation == ApplyOperation::Delete {
            state
                .journal
                .state
                .inventory
                .retain(|entry| entry.identity != intent.identity);
            if state
                .journal
                .state
                .prune_candidate
                .as_ref()
                .is_some_and(|candidate| candidate.inventory.identity == intent.identity)
            {
                state.journal.state.prune_candidate = None;
            }
        } else {
            let (uid, resource_version) = match (&result.uid, &result.resource_version) {
                (Some(uid), Some(resource_version)) => (uid.clone(), resource_version.clone()),
                _ => {
                    return Err(ReconcileError::Mutation(
                        "applied result lacks UID/resourceVersion".into(),
                    ));
                }
            };
            state
                .journal
                .state
                .inventory
                .retain(|entry| entry.identity != intent.identity);
            state.journal.state.inventory.push(InventoryEntry {
                identity: intent.identity.clone(),
                logical_id: intent.logical_id.clone(),
                uid,
                resource_version,
                bundle_digest: state.root.bundle_digest.clone(),
            });
        }
    }
    state
        .journal
        .state
        .inventory
        .sort_by_key(|entry| identity_key(&entry.identity));
    state
        .journal
        .state
        .apply_transaction
        .as_mut()
        .expect("transaction cloned from state")
        .results
        .extend(results.iter().cloned());
    Ok(())
}

fn recover_prepared_result(
    state: &MachineStateV0,
    tx: &ApplyTransaction,
    observed: &[DesiredObservationV0],
) -> Result<Vec<ApplyResult>, ReconcileError> {
    if observed.len() > MAX_LIST_ENTRIES {
        return Err(ReconcileError::Desired("observations exceed bound".into()));
    }
    let Some(intent) = next_unrecorded_intent(tx) else {
        return Ok(Vec::new());
    };
    let Some(observation) = observed
        .iter()
        .find(|observation| observation.identity == intent.identity)
    else {
        return Ok(Vec::new());
    };
    match (&intent.operation, &observation.observation) {
        (ApplyOperation::Delete, Observation::Absent)
            if deletion_recovery_matches(state, tx, intent, observation) =>
        {
            Ok(vec![ApplyResult {
                command_id: intent.command_id.clone(),
                attempt_token: tx.attempt_token.clone(),
                identity: intent.identity.clone(),
                outcome: ApplyOutcome::AlreadyAbsent,
                uid: None,
                resource_version: None,
            }])
        }
        (ApplyOperation::Apply, Observation::Existing { identity }) => {
            if apply_recovery_matches(state, tx, intent, observation, identity) {
                Ok(vec![ApplyResult {
                    command_id: intent.command_id.clone(),
                    attempt_token: tx.attempt_token.clone(),
                    identity: intent.identity.clone(),
                    outcome: ApplyOutcome::Applied,
                    uid: Some(identity.uid.clone()),
                    resource_version: Some(identity.resource_version.clone()),
                }])
            } else {
                Ok(Vec::new())
            }
        }
        _ => Ok(Vec::new()),
    }
}

fn recovered_observation_projection(
    state: &MachineStateV0,
    tx: &ApplyTransaction,
    observed: &[DesiredObservationV0],
) -> Vec<DesiredObservationV0> {
    if !matches!(
        tx.next_command_lifecycle.state,
        NextCommandLifecycleStateV0::DispatchAuthorizedMayHaveIssued
    ) {
        return observed.to_vec();
    }
    let Some(intent) = next_unrecorded_intent(tx) else {
        return observed.to_vec();
    };
    observed
        .iter()
        .cloned()
        .map(|mut item| {
            if item.identity != intent.identity {
                return item;
            }
            match (&intent.operation, &item.observation) {
                (ApplyOperation::Apply, Observation::Existing { identity })
                    if apply_recovery_matches(state, tx, intent, &item, identity) =>
                {
                    // The original prepared graph was based on the exact
                    // pre-effect observation. For SSA, project that old
                    // identity/RV only while validating the stored plan; the
                    // recovery result below retains the returned new RV.
                    item.observation =
                        match (&intent.expected_uid, &intent.expected_resource_version) {
                            (Some(uid), Some(resource_version)) => Observation::Existing {
                                identity: MutationIdentity {
                                    uid: uid.clone(),
                                    resource_version: resource_version.clone(),
                                },
                            },
                            (None, None) => Observation::Absent,
                            _ => return item,
                        };
                }
                (ApplyOperation::Delete, Observation::Absent)
                    if deletion_recovery_matches(state, tx, intent, &item) =>
                {
                    item.observation = Observation::Existing {
                        identity: MutationIdentity {
                            uid: intent.expected_uid.clone().unwrap_or_default(),
                            resource_version: intent
                                .expected_resource_version
                                .clone()
                                .unwrap_or_default(),
                        },
                    };
                }
                _ => {}
            }
            item
        })
        .collect()
}

fn next_unrecorded_intent(tx: &ApplyTransaction) -> Option<&ApplyIntent> {
    tx.prepared_intents
        .iter()
        .filter(|intent| {
            !tx.results
                .iter()
                .any(|result| result.command_id == intent.command_id)
        })
        .min_by(|left, right| left.command_id.cmp(&right.command_id))
}

fn has_may_have_issued(state: &MachineStateV0) -> bool {
    state
        .journal
        .state
        .apply_transaction
        .as_ref()
        .is_some_and(|tx| {
            matches!(
                tx.next_command_lifecycle.state,
                NextCommandLifecycleStateV0::DispatchAuthorizedMayHaveIssued
            )
        })
}

fn lifecycle_for_next(tx: &ApplyTransaction) -> NextCommandLifecycleV0 {
    match tx.prepared_intents.get(tx.results.len()) {
        Some(intent) => NextCommandLifecycleV0::prepared(intent.command_id.clone()),
        None => NextCommandLifecycleV0::complete(),
    }
}

fn advance_next_command_lifecycle(state: &mut MachineStateV0) -> Result<(), ReconcileError> {
    let tx = state
        .journal
        .state
        .apply_transaction
        .as_mut()
        .ok_or_else(|| ReconcileError::Mutation("result has no transaction".into()))?;
    let lifecycle = lifecycle_for_next(tx);
    tx.next_command_lifecycle = lifecycle;
    Ok(())
}

fn authorize_dispatch(
    state: &mut MachineStateV0,
    tx: &ApplyTransaction,
    mut effects: Vec<MutationCommandV0>,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if effects.len() != 1 {
        return Err(ReconcileError::Mutation(
            "dispatch authorization requires exactly one mutation command".into(),
        ));
    }
    let command = effects.pop().expect("length checked");
    let Some(intent) = next_unrecorded_intent(tx) else {
        return Err(ReconcileError::Mutation(
            "dispatch authorization has no next prepared intent".into(),
        ));
    };
    if !matches!(
        &tx.next_command_lifecycle,
        NextCommandLifecycleV0 {
            state: NextCommandLifecycleStateV0::PreparedNotIssued,
            command_id: Some(command_id),
        } if command_id == &command.command_id && command_id == &intent.command_id
    ) {
        return Err(ReconcileError::Mutation(
            "dispatch authorization lifecycle does not match the next command".into(),
        ));
    }
    state
        .journal
        .state
        .apply_transaction
        .as_mut()
        .expect("transaction cloned from state")
        .next_command_lifecycle = NextCommandLifecycleV0::authorized(command.command_id.clone());
    state.runtime.pending_dispatch = Some(PendingMutationDispatchV0 {
        transaction_id: tx.id.clone(),
        attempt_token: tx.attempt_token.clone(),
        command,
    });
    queue_persist(
        state,
        PersistPurpose::DispatchAuthorization,
        DurablePhase::DispatchAuthorization,
        PhaseV0::DispatchAuthorizationPersistPending,
        commands,
    )
}

fn claim_matches(state: &MachineStateV0, tx: &ApplyTransaction, intent: &ApplyIntent) -> bool {
    tx.recovery_claims.iter().any(|claim| {
        claim.root_uid == state.root.uid
            && claim.transaction_id == tx.id
            && claim.command_id == intent.command_id
    })
}

fn apply_recovery_matches(
    state: &MachineStateV0,
    tx: &ApplyTransaction,
    intent: &ApplyIntent,
    observation: &DesiredObservationV0,
    identity: &MutationIdentity,
) -> bool {
    let Some(proof) = &observation.recovery_proof else {
        return false;
    };
    let exact_proof = claim_matches(state, tx, intent)
        && proof.root_uid == state.root.uid
        && proof.transaction_id == tx.id
        && proof.command_id == intent.command_id
        && proof.uid == identity.uid
        && proof.resource_version == identity.resource_version
        && !proof.uid.is_empty()
        && !proof.resource_version.is_empty();
    if !exact_proof {
        return false;
    }
    match (&intent.expected_uid, &intent.expected_resource_version) {
        // An SSA response-lost recovery may only report the same object with
        // a nonempty, changed RV. Kubernetes RVs are opaque: distinctness is
        // the only ordering property the pure reducer can establish.
        (Some(expected_uid), Some(expected_rv)) => {
            identity.uid == *expected_uid
                && !identity.resource_version.is_empty()
                && identity.resource_version != *expected_rv
        }
        // POST response loss has no pre-existing UID/RV. The adapter's exact
        // ownership proof is its no-adoption boundary.
        (None, None) => true,
        _ => false,
    }
}

fn deletion_recovery_matches(
    state: &MachineStateV0,
    tx: &ApplyTransaction,
    intent: &ApplyIntent,
    observation: &DesiredObservationV0,
) -> bool {
    let Some(proof) = &observation.deletion_recovery_proof else {
        return false;
    };
    claim_matches(state, tx, intent)
        && proof.root_uid == state.root.uid
        && proof.transaction_id == tx.id
        && proof.command_id == intent.command_id
        && proof.expected_uid == intent.expected_uid.as_deref().unwrap_or_default()
        && proof.expected_resource_version
            == intent
                .expected_resource_version
                .as_deref()
                .unwrap_or_default()
        && !proof.expected_uid.is_empty()
        && !proof.expected_resource_version.is_empty()
}

fn validate_mutation_ack(
    tx: &ApplyTransaction,
    attempt: &str,
    id: &str,
    results: &[ApplyResult],
) -> Result<(), ReconcileError> {
    if tx.id != id || tx.attempt_token != attempt || results.is_empty() {
        return Err(ReconcileError::MutationAck(
            "transaction or attempt mismatch".into(),
        ));
    }
    let mut expected = tx
        .prepared_intents
        .iter()
        .filter(|intent| {
            !tx.results
                .iter()
                .any(|result| result.command_id == intent.command_id)
        })
        .collect::<Vec<_>>();
    // Exactly one external mutation is ever in flight. Its result is durably
    // journaled before the next intent is emitted, so a process crash cannot
    // turn a partial batch into implicit adoption.
    expected.sort_by(|left, right| left.command_id.cmp(&right.command_id));
    let Some(expected) = expected.first() else {
        return Err(ReconcileError::MutationAck(
            "ack has no remaining issued command".into(),
        ));
    };
    if results.len() != 1 {
        return Err(ReconcileError::MutationAck(
            "ack must contain the one exact issued command".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    for result in results {
        if expected.command_id != result.command_id {
            return Err(ReconcileError::MutationAck(
                "ack contains an unissued command".into(),
            ));
        }
        let intent = expected;
        if !seen.insert(&result.command_id)
            || result.attempt_token != tx.attempt_token
            || result.identity != intent.identity
        {
            return Err(ReconcileError::MutationAck(
                "ack command ID/token/identity mismatch".into(),
            ));
        }
        match intent.operation {
            ApplyOperation::Apply => {
                if result.outcome != ApplyOutcome::Applied
                    || result.uid.as_deref().unwrap_or_default().is_empty()
                    || result
                        .resource_version
                        .as_deref()
                        .unwrap_or_default()
                        .is_empty()
                {
                    return Err(ReconcileError::MutationAck(
                        "POST/SSA must be Applied with nonempty UID/RV".into(),
                    ));
                }
                if let Some(uid) = &intent.expected_uid
                    && result.uid.as_deref() != Some(uid)
                {
                    return Err(ReconcileError::MutationAck(
                        "conditional apply changed UID".into(),
                    ));
                }
                if let Some(expected_rv) = &intent.expected_resource_version
                    && result.resource_version.as_deref() == Some(expected_rv)
                {
                    return Err(ReconcileError::MutationAck(
                        "conditional apply did not return a changed resourceVersion".into(),
                    ));
                }
            }
            ApplyOperation::Delete => {
                let exact_delete = result.outcome == ApplyOutcome::Deleted
                    && result.uid.as_deref() == intent.expected_uid.as_deref()
                    && result.resource_version.as_deref()
                        == intent.expected_resource_version.as_deref();
                let already_absent = result.outcome == ApplyOutcome::AlreadyAbsent
                    && result.uid.is_none()
                    && result.resource_version.is_none();
                if !exact_delete && !already_absent {
                    return Err(ReconcileError::MutationAck(
                        "DELETE must be exact or already absent without a replacement identity"
                            .into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn mutation_failed(
    state: &mut MachineStateV0,
    attempt: &str,
    id: &str,
    reason: String,
    commands: &mut Vec<CommandV0>,
) {
    if matches!(
        state.phase,
        PhaseV0::MutationPending | PhaseV0::DeletionMutationPending
    ) && state
        .journal
        .state
        .apply_transaction
        .as_ref()
        .is_some_and(|tx| tx.id == id && tx.attempt_token == attempt)
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: format!("mutation conflict/failed blocks progress: {reason}"),
        });
    } else {
        stale(commands, "mutation failure token mismatch");
    }
}

fn deletion_advance(
    state: &mut MachineStateV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if !state.journal.state.inventory.is_empty() {
        state.phase = PhaseV0::DeletionObservationPending;
        commands.push(CommandV0::RequestDeletionObservation {
            preconditions: pins(&state.root),
        });
        return Ok(());
    }
    state.journal.state.apply_transaction = None;
    sort_automation(state);
    let Some(run) = next_automation_cleanup(state) else {
        let token = owned_children_token(state).expect("deletion record exists");
        state.phase = PhaseV0::OwnedChildrenVerifyPending;
        commands.push(CommandV0::VerifyOwnedChildrenAbsent {
            token,
            preconditions: pins(&state.root),
        });
        return Ok(());
    };
    state.phase = PhaseV0::DeletionAutomationCleanupPending;
    commands.push(CommandV0::DeleteAutomationRun {
        token: cleanup_token_for(state, &run),
        occurrence_id: run.occurrence_id,
        name: run.name,
        uid: run.uid,
        resource_version: run.resource_version,
        preconditions: pins(&state.root),
    });
    Ok(())
}

fn owned_children_token(state: &MachineStateV0) -> Option<String> {
    state.journal.state.deletion.as_ref().map(|deletion| {
        if state.root_only_deletion_recovery {
            return root_only_owned_children_verify_token(
                &deletion.id,
                deletion.deletion_start_generation,
                &state.root.uid,
                state.root.generation,
                &state.root.root_resource_version,
                &state.root.state_uid,
            );
        }
        let finalizer_barrier_stage = if state.root.state_finalizer_present {
            "before-state-finalizer"
        } else {
            "before-root-finalizer"
        };
        hash(&[
            "verify-owned-children-absent/v0",
            finalizer_barrier_stage,
            &deletion.id,
            &deletion.deletion_start_generation.to_string(),
            &state.root.uid,
            &state.root.generation.to_string(),
            &state.root.root_resource_version,
            &state.root.state_uid,
            &state.root.state_resource_version,
        ])
    })
}

/// Deterministic token for a fresh owned-child proof after a deleting root's
/// linked state object has already been confirmed absent. The durable receipt
/// is an input to this command, but its `cleanupToken` is proof of earlier
/// work and is deliberately not reused as this token.
#[must_use]
pub fn root_only_owned_children_verify_token(
    deletion_id: &str,
    deletion_start_generation: i64,
    root_uid: &str,
    root_generation: i64,
    root_resource_version: &str,
    state_uid: &str,
) -> String {
    hash(&[
        "verify-owned-children-absent/v0",
        "before-root-finalizer",
        deletion_id,
        &deletion_start_generation.to_string(),
        root_uid,
        &root_generation.to_string(),
        root_resource_version,
        state_uid,
        "",
    ])
}

fn owned_children_verified(
    state: &mut MachineStateV0,
    token: &str,
    root_uid: &str,
    root_generation: i64,
    root_resource_version: &str,
    state_uid: &str,
    state_resource_version: &str,
    cleanup_token: &str,
    commands: &mut Vec<CommandV0>,
) {
    // A receipt-bound recovery is proving that earlier cleanup still holds;
    // its receipt token is deliberately not the token of this new
    // verification command. Normal deletion writes the fresh verification
    // token into the receipt immediately below, so its acknowledgement still
    // requires the two values to agree.
    let receipt_cleanup_token = state
        .root
        .status
        .deletion_cleanup_receipt
        .as_ref()
        .map(|receipt| receipt.cleanup_token.as_str());
    let expected_cleanup_token = if state.root_only_deletion_recovery {
        receipt_cleanup_token
    } else {
        Some(token)
    };
    if state.phase != PhaseV0::OwnedChildrenVerifyPending
        || owned_children_token(state).as_deref() != Some(token)
        || root_uid != state.root.uid
        || root_generation != state.root.generation
        || root_resource_version != state.root.root_resource_version
        || state_uid != state.root.state_uid
        || state_resource_version != state.root.state_resource_version
        || expected_cleanup_token != Some(cleanup_token)
    {
        stale(
            commands,
            "owned-child absence acknowledgement pins mismatch",
        );
        return;
    }
    if state.root.state_finalizer_present {
        let deletion = state
            .journal
            .state
            .deletion
            .as_ref()
            .expect("owned-child verification requires deletion");
        let mut status = state.root.status.clone();
        status.deletion_cleanup_receipt = Some(DeletionCleanupReceipt {
            root_uid: state.root.uid.clone(),
            deletion_id: deletion.id.clone(),
            deletion_start_generation: deletion.deletion_start_generation,
            state_name: state.root.state_name.clone(),
            state_uid: state.root.state_uid.clone(),
            cleanup_token: cleanup_token.into(),
        });
        if !queue_root_status(
            state,
            status,
            RootStatusContinuationV0::DeletionStateFinalizer,
            None,
            commands,
        ) {
            state.phase = PhaseV0::StateFinalizerPending;
            commands.push(CommandV0::RemoveStateFinalizer {
                token: state_finalizer_token(state),
                preconditions: pins(&state.root),
            });
        }
    } else {
        state.phase = PhaseV0::RootFinalizerPending;
        commands.push(CommandV0::RemoveRootFinalizer {
            token: root_finalizer_token(state),
            preconditions: pins(&state.root),
        });
    }
}

fn state_finalizer_token(state: &MachineStateV0) -> String {
    hash(&[
        "state-finalizer/v0",
        &state.root.uid,
        &state.root.state_uid,
        &state.root.state_resource_version,
        &state.root.root_resource_version,
    ])
}

fn root_finalizer_token(state: &MachineStateV0) -> String {
    root_finalizer_command_token(
        &state.root.uid,
        &state.root.root_resource_version,
        &state.root.state_uid,
        &state.root.state_resource_version,
    )
}

/// Deterministic token for the exact root-finalizer removal command. Adapters
/// recompute it from freshly reread pins before issuing the irreversible
/// finalizer write.
#[must_use]
pub fn root_finalizer_command_token(
    root_uid: &str,
    root_resource_version: &str,
    state_uid: &str,
    state_resource_version: &str,
) -> String {
    hash(&[
        "root-finalizer/v0",
        root_uid,
        root_resource_version,
        state_uid,
        state_resource_version,
    ])
}

fn root_absence_token(state: &MachineStateV0) -> String {
    hash(&[
        "root-uid-absent/v0",
        &state.root.uid,
        &state.root.root_resource_version,
    ])
}

fn state_finalizer_ack(
    state: &mut MachineStateV0,
    token: &str,
    state_uid: &str,
    old: &str,
    new: &str,
    commands: &mut Vec<CommandV0>,
) {
    if state.phase != PhaseV0::StateFinalizerPending
        || token != state_finalizer_token(state)
        || state_uid != state.root.state_uid
        || old != state.root.state_resource_version
        || new.is_empty()
        || new == old
    {
        stale(
            commands,
            "state finalizer acknowledgement token/UID/RV mismatch",
        );
        return;
    }
    state.root.state_resource_version = new.into();
    state.root.state_finalizer_present = false;
    let token = owned_children_token(state).expect("deletion record exists");
    state.phase = PhaseV0::OwnedChildrenVerifyPending;
    commands.push(CommandV0::VerifyOwnedChildrenAbsent {
        token,
        preconditions: pins(&state.root),
    });
}

fn root_finalizer_ack(
    state: &mut MachineStateV0,
    token: &str,
    root_uid: &str,
    old: &str,
    new: &str,
    commands: &mut Vec<CommandV0>,
) {
    if state.phase != PhaseV0::RootFinalizerPending
        || token != root_finalizer_token(state)
        || root_uid != state.root.uid
        || old != state.root.root_resource_version
        || !state.root.deleting
        || !state.root.root_finalizer_present
        || validate_deleting_root_receipt(&state.root).is_err()
        || new.is_empty()
        || new == old
    {
        stale(
            commands,
            "root finalizer acknowledgement token/UID/RV mismatch",
        );
        return;
    }
    state.root.root_resource_version = new.into();
    state.root.root_finalizer_present = false;
    state.phase = PhaseV0::RootDeletionVerifyPending;
    commands.push(CommandV0::VerifyRootUidAbsent {
        token: root_absence_token(state),
        root_uid: state.root.uid.clone(),
        preconditions: pins(&state.root),
    });
}

fn automation_deleted(
    state: &mut MachineStateV0,
    token: &str,
    occurrence: &str,
    name: &str,
    uid: &str,
    rv: &str,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    let Some(run) = next_automation_cleanup(state) else {
        stale(
            commands,
            "automation delete acknowledgement has no durable pending run",
        );
        return Ok(());
    };
    let valid = state.phase == PhaseV0::DeletionAutomationCleanupPending
        && token == cleanup_token_for(state, &run)
        && occurrence == run.occurrence_id
        && name == run.name
        && uid == run.uid
        && rv == run.resource_version
        && !uid.is_empty()
        && !rv.is_empty();
    if !valid {
        stale(
            commands,
            "automation delete acknowledgement token/name/UID/RV mismatch",
        );
        return Ok(());
    }
    if let Some(index) = state
        .journal
        .state
        .automation
        .pending
        .iter()
        .position(|item| item.occurrence_id == occurrence)
    {
        state.journal.state.automation.pending.remove(index);
    } else if let Some(index) = state
        .journal
        .state
        .automation
        .completions
        .iter()
        .position(|item| item.occurrence_id == occurrence)
    {
        state.journal.state.automation.completions.remove(index);
    } else {
        stale(
            commands,
            "automation delete acknowledgement occurrence disappeared",
        );
        return Ok(());
    }
    queue_persist(
        state,
        PersistPurpose::DeletionAutomationCleanup,
        DurablePhase::DeletionAutomationCleanup,
        PhaseV0::DeletionAutomationPersistPending,
        commands,
    )
}

fn cleanup_token(state: &MachineStateV0) -> Option<String> {
    next_automation_cleanup(state).map(|run| cleanup_token_for(state, &run))
}

fn next_automation_cleanup(state: &MachineStateV0) -> Option<AutomationRunCleanupV0> {
    let mut runs = state
        .journal
        .state
        .automation
        .pending
        .iter()
        .map(|pending| AutomationRunCleanupV0 {
            occurrence_id: pending.occurrence_id.clone(),
            name: pending.run_name.clone(),
            uid: pending.run_uid.clone(),
            resource_version: pending.run_resource_version.clone(),
        })
        .chain(
            state
                .journal
                .state
                .automation
                .completions
                .iter()
                .map(|completion| AutomationRunCleanupV0 {
                    occurrence_id: completion.occurrence_id.clone(),
                    name: completion.run_name.clone(),
                    uid: completion.run_uid.clone(),
                    resource_version: completion.completed_resource_version.clone(),
                }),
        )
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| left.occurrence_id.cmp(&right.occurrence_id));
    runs.into_iter().next()
}

fn cleanup_token_for(state: &MachineStateV0, run: &AutomationRunCleanupV0) -> String {
    hash(&[
        "automation-delete/v0",
        &state.root.uid,
        &state.root.generation.to_string(),
        &state.root.root_resource_version,
        &run.occurrence_id,
        &run.name,
        &run.uid,
        &run.resource_version,
    ])
}

fn baseline(state: &mut MachineStateV0, trigger: ActionTriggerV0) {
    if !state.journal.state.automation.baselines.iter().any(|item| {
        item.automation_id == trigger.automation_identity && item.source == trigger.source
    }) {
        state
            .journal
            .state
            .automation
            .baselines
            .push(AutomationBaseline {
                automation_id: trigger.automation_identity,
                source: trigger.source,
                token: trigger.observation_token,
            });
        sort_automation(state);
    }
}

fn trigger_automation(
    state: &mut MachineStateV0,
    trigger: ActionTriggerV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if state.phase != PhaseV0::Ready {
        stale(commands, "automation trigger phase mismatch");
        return Ok(());
    }
    validate_action_trigger(&trigger)?;
    let found = state
        .journal
        .state
        .automation
        .baselines
        .iter()
        .find(|item| {
            item.automation_id == trigger.automation_identity && item.source == trigger.source
        })
        .cloned();
    let Some(current) = found else {
        baseline(state, trigger);
        return queue_persist(
            state,
            PersistPurpose::AutomationBaseline,
            DurablePhase::AutomationBaseline,
            PhaseV0::AutomationPersistPending,
            commands,
        );
    };
    if current.token == trigger.observation_token {
        return Ok(());
    }
    let transition_id = hash(&[
        "source-transition/v0",
        &trigger.automation_identity,
        &trigger.automation_version,
        &identity_key(&trigger.source),
        &current.token,
        &trigger.observation_token,
        &trigger.source_uid,
        &trigger.source_resource_version,
    ]);
    let occurrence_id = hash(&[
        "occurrence/v0",
        &state.root.uid,
        &trigger.automation_identity,
        &trigger.automation_version,
        &trigger.source_uid,
        &transition_id,
        &trigger.source_resource_version,
    ]);
    if state
        .journal
        .state
        .automation
        .pending
        .iter()
        .any(|item| item.occurrence_id == occurrence_id)
        || state
            .journal
            .state
            .automation
            .completions
            .iter()
            .any(|item| item.occurrence_id == occurrence_id)
    {
        return Ok(());
    }
    let action = ActionInputV0 {
        abi_version: oxideloop_renderer_abi::model::ABI_VERSION_V0.into(),
        bundle_digest: state.root.bundle_digest.clone(),
        automation_identity: trigger.automation_identity.clone(),
        automation_version: trigger.automation_version.clone(),
        action_module_identity: trigger.action_module_identity.clone(),
        action_module_digest: trigger.action_module_digest.clone(),
        occurrence_id: occurrence_id.clone(),
        source_transition_id: transition_id.clone(),
        source_uid: trigger.source_uid.clone(),
        source_resource_version: trigger.source_resource_version.clone(),
        event_metadata: trigger.event_metadata,
    };
    let action_input_v0 = String::from_utf8(
        canonical_bytes(
            &serde_json::to_value(&action)
                .map_err(|e| ReconcileError::Automation(e.to_string()))?,
        )
        .map_err(|e| ReconcileError::Automation(e.to_string()))?,
    )
    .map_err(|e| ReconcileError::Automation(e.to_string()))?;
    let spec = AutomationRunSpec {
        occurrence_id: occurrence_id.clone(),
        root_uid: state.root.uid.clone(),
        source: trigger.source.clone(),
        source_uid: trigger.source_uid,
        source_resource_version: trigger.source_resource_version,
        source_transition_id: transition_id,
        automation_id: trigger.automation_identity,
        automation_version: trigger.automation_version,
        bundle_digest: state.root.bundle_digest.clone(),
        program: state.root.spec.program.clone(),
        action_module_identity: trigger.action_module_identity,
        action_module_digest: trigger.action_module_digest,
        renderer_abi_version: oxideloop_renderer_abi::model::ABI_VERSION_V0.into(),
        action_input_hash: sha256_hex(action_input_v0.as_bytes()),
        event_metadata_hash: sha256_hex(
            &canonical_bytes(&action.event_metadata)
                .map_err(|e| ReconcileError::Automation(e.to_string()))?,
        ),
        action_input_v0,
    };
    spec.validate()
        .map_err(|e| ReconcileError::Automation(e.to_string()))?;
    let name = automation_run_name(&state.root.uid, &occurrence_id)
        .map_err(|e| ReconcileError::Automation(e.to_string()))?;
    validate_automation_run_name(&name, &spec)
        .map_err(|e| ReconcileError::Automation(e.to_string()))?;
    let token = hash(&[
        "automation-ensure/v0",
        &state.root.uid,
        &state.root.generation.to_string(),
        &state.root.root_resource_version,
        &state.root.state_resource_version,
        &name,
        &spec.action_input_hash,
    ]);
    state.runtime.pending_automation = Some(PendingAutomationV0 {
        token: token.clone(),
        name: name.clone(),
        spec: spec.clone(),
        next_baseline_token: trigger.observation_token,
    });
    state.phase = PhaseV0::AutomationEnsurePending;
    commands.push(CommandV0::EnsureAutomationRun {
        token,
        name,
        spec,
        preconditions: pins(&state.root),
    });
    Ok(())
}

fn validate_action_trigger(trigger: &ActionTriggerV0) -> Result<(), ReconcileError> {
    for (field, value, maximum) in [
        (
            "automationIdentity",
            trigger.automation_identity.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        (
            "automationVersion",
            trigger.automation_version.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        (
            "actionModuleIdentity",
            trigger.action_module_identity.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        (
            "sourceUid",
            trigger.source_uid.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        (
            "sourceResourceVersion",
            trigger.source_resource_version.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        (
            "observationToken",
            trigger.observation_token.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        (
            "source.apiVersion",
            trigger.source.api_version.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        (
            "source.kind",
            trigger.source.kind.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
        ("source.namespace", trigger.source.namespace.as_str(), 63),
        (
            "source.name",
            trigger.source.name.as_str(),
            MAX_IDENTIFIER_BYTES,
        ),
    ] {
        if value.is_empty() || value.len() > maximum {
            return Err(ReconcileError::Automation(format!("{field} is invalid")));
        }
    }
    if !is_raw_sha256(&trigger.action_module_digest) {
        return Err(ReconcileError::Automation(
            "actionModuleDigest must be a lowercase SHA-256".into(),
        ));
    }
    if trigger.observation_token != automation_source_transition_token(trigger) {
        return Err(ReconcileError::Automation(
            "automation observation token is not source-specific".into(),
        ));
    }
    let bytes = canonical_bytes(&trigger.event_metadata)
        .map_err(|error| ReconcileError::Automation(error.to_string()))?;
    if bytes.len() > MAX_ACTION_INPUT_CANONICAL_BYTES {
        return Err(ReconcileError::Automation(
            "eventMetadata exceeds action protocol bound".into(),
        ));
    }
    Ok(())
}

/// Derives the automation baseline token exclusively from the automation and
/// its exact source transition. A global render/dependency barrier token is
/// intentionally absent, so unrelated cluster changes cannot create an
/// occurrence.
#[must_use]
pub fn automation_source_transition_token(trigger: &ActionTriggerV0) -> String {
    hash(&[
        "automation-source-observation/v0",
        &trigger.automation_identity,
        &trigger.automation_version,
        &identity_key(&trigger.source),
        &trigger.source_uid,
        &trigger.source_resource_version,
    ])
}

fn automation_ensured(
    state: &mut MachineStateV0,
    token: &str,
    name: &str,
    spec: &AutomationRunSpec,
    uid: &str,
    rv: &str,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    let Some(pending) = state.runtime.pending_automation.clone() else {
        stale(
            commands,
            "automation ensure acknowledgement has no issued run",
        );
        return Ok(());
    };
    // All pins are checked before pending runtime state is consumed.
    if state.phase != PhaseV0::AutomationEnsurePending
        || pending.token != token
        || pending.name != name
        || pending.spec != *spec
        || uid.is_empty()
        || rv.is_empty()
        || spec.validate().is_err()
        || validate_automation_run_name(name, spec).is_err()
    {
        stale(
            commands,
            "automation ensure token/name/spec/UID/RV mismatch",
        );
        return Ok(());
    }
    let Some(item) = state
        .journal
        .state
        .automation
        .baselines
        .iter_mut()
        .find(|item| item.automation_id == spec.automation_id && item.source == spec.source)
    else {
        return Err(ReconcileError::Automation(
            "automation baseline disappeared".into(),
        ));
    };
    let spec_hash = automation_run_spec_hash(spec)?;
    item.token = pending.next_baseline_token;
    state
        .journal
        .state
        .automation
        .pending
        .push(AutomationPending {
            occurrence_id: spec.occurrence_id.clone(),
            automation_id: spec.automation_id.clone(),
            source: spec.source.clone(),
            run_name: name.into(),
            run_uid: uid.into(),
            run_resource_version: rv.into(),
            spec_hash,
        });
    sort_automation(state);
    state.runtime.pending_automation = None;
    queue_persist(
        state,
        PersistPurpose::AutomationPending,
        DurablePhase::AutomationPending,
        PhaseV0::AutomationPersistPending,
        commands,
    )
}

/// Hashes the complete immutable run specification, rather than a selected
/// subset of its fields. This durable authentication value intentionally has
/// no migration path because older pending records cannot prove which source
/// program authority was used.
fn automation_run_spec_hash(spec: &AutomationRunSpec) -> Result<String, ReconcileError> {
    spec.validate()
        .map_err(|error| ReconcileError::Automation(error.to_string()))?;
    let value = serde_json::to_value(spec)
        .map_err(|error| ReconcileError::Automation(error.to_string()))?;
    let bytes =
        canonical_bytes(&value).map_err(|error| ReconcileError::Automation(error.to_string()))?;
    Ok(sha256_hex(&bytes))
}

#[allow(clippy::too_many_arguments)]
fn automation_completed(
    state: &mut MachineStateV0,
    spec: &AutomationRunSpec,
    occurrence: String,
    name: String,
    uid: String,
    old_rv: String,
    new_rv: String,
    completion: AutomationRunCompletionState,
    attempts: u16,
    materialization_name: String,
    materialization_uid: String,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    if state.phase != PhaseV0::Ready {
        stale(commands, "automation completion phase/bounds mismatch");
        return Ok(());
    }
    // A stale completion owns no current pending record.  Its complete spec is
    // therefore untrusted data and must not be parsed or validated before this
    // exact correlation check: a foreign malformed object cannot poison the
    // active occurrence by driving it to Hold.
    let Some(index) = state
        .journal
        .state
        .automation
        .pending
        .iter()
        .position(|item| {
            item.occurrence_id == occurrence
                && item.run_name == name
                && item.run_uid == uid
                && item.run_resource_version == old_rv
        })
    else {
        stale(
            commands,
            "automation completion does not match current UID/RV",
        );
        return Ok(());
    };
    if attempts == 0
        || attempts > MAX_AUTOMATION_ATTEMPTS
        || name.is_empty()
        || uid.is_empty()
        || old_rv.is_empty()
        || new_rv.is_empty()
        || old_rv == new_rv
        || materialization_name.is_empty()
        || materialization_uid.is_empty()
        || name == materialization_name
        || uid == materialization_uid
    {
        stale(commands, "automation completion phase/bounds mismatch");
        return Ok(());
    }
    let supplied_spec_hash = match automation_run_spec_hash(spec) {
        Ok(hash) => hash,
        Err(reason) => {
            state.phase = PhaseV0::Hold;
            commands.push(CommandV0::Hold {
                reason: format!("completed AutomationRun spec is invalid: {reason}"),
            });
            return Ok(());
        }
    };
    if validate_automation_run_name(&name, spec).is_err()
        || spec.occurrence_id != occurrence
        || spec.root_uid != state.root.uid
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "completed AutomationRun spec identity is invalid".into(),
        });
        return Ok(());
    }
    let pending = &state.journal.state.automation.pending[index];
    if pending.spec_hash != supplied_spec_hash
        || spec.automation_id != pending.automation_id
        || spec.source != pending.source
    {
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "completed AutomationRun spec hash does not match pending occurrence".into(),
        });
        return Ok(());
    }
    let outcome = match completion {
        AutomationRunCompletionState::Succeeded => AutomationCompletionOutcome::Succeeded,
        AutomationRunCompletionState::Failed => AutomationCompletionOutcome::Failed,
        AutomationRunCompletionState::DeadLettered => AutomationCompletionOutcome::DeadLettered,
    };
    state.journal.state.automation.pending.remove(index);
    state
        .journal
        .state
        .automation
        .completions
        .push(AutomationCompletion {
            occurrence_id: occurrence,
            outcome,
            run_name: name,
            run_uid: uid,
            old_resource_version: old_rv,
            completed_resource_version: new_rv,
            materialization_name,
            materialization_uid,
        });
    sort_automation(state);
    queue_persist(
        state,
        PersistPurpose::AutomationCompletion,
        DurablePhase::AutomationCompletion,
        PhaseV0::AutomationPersistPending,
        commands,
    )
}

struct Plan {
    disposition: RenderDisposition,
    hash: String,
    intents: Vec<ApplyIntent>,
    decisions: BTreeMap<String, MutationDecision>,
    dependencies: Vec<DependencySnapshot>,
    candidate: Option<PruneCandidate>,
    absent: Vec<ObjectIdentity>,
}

impl Plan {
    fn validate_initial_effects(&self) -> Result<(), ReconcileError> {
        for intent in &self.intents {
            let decision = self.decisions.get(&intent.command_id).ok_or_else(|| {
                ReconcileError::Mutation("prepared intent lacks a mutation decision".into())
            })?;
            if !safe_mutation_decision(decision) {
                return Err(ReconcileError::Mutation(
                    "unsafe desired mutation decision".into(),
                ));
            }
        }
        Ok(())
    }

    fn remaining_effects(
        &self,
        state: &MachineStateV0,
        tx: &ApplyTransaction,
    ) -> Result<Vec<MutationCommandV0>, ReconcileError> {
        let done = tx
            .results
            .iter()
            .map(|result| result.command_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut effects = Vec::new();
        for intent in tx
            .prepared_intents
            .iter()
            .filter(|intent| !done.contains(intent.command_id.as_str()))
        {
            let fresh_intent = self
                .intents
                .iter()
                .find(|fresh| fresh.command_id == intent.command_id)
                .ok_or_else(|| {
                    ReconcileError::Mutation(
                        "revalidated graph is missing a prepared remaining intent".into(),
                    )
                })?;
            if fresh_intent != intent {
                return Err(ReconcileError::Mutation(
                    "revalidated remaining intent changed identity or preconditions".into(),
                ));
            }
            let decision = self
                .decisions
                .get(&intent.command_id)
                .ok_or_else(|| {
                    ReconcileError::Mutation("revalidated intent lacks a safe decision".into())
                })?
                .clone();
            if !safe_mutation_decision(&decision) {
                return Err(ReconcileError::Mutation(
                    "revalidated remaining intent is not safely executable".into(),
                ));
            }
            effects.push(MutationCommandV0 {
                command_id: intent.command_id.clone(),
                attempt_token: tx.attempt_token.clone(),
                preconditions: pins(&state.root),
                identity: intent.identity.clone(),
                logical_id: intent.logical_id.clone(),
                prepared_expected_target: prepared_expected_target(intent)?,
                decision,
            });
        }
        effects.sort_by(|a, b| a.command_id.cmp(&b.command_id));
        Ok(effects.into_iter().take(1).collect())
    }
}

fn safe_mutation_decision(decision: &MutationDecision) -> bool {
    match decision {
        MutationDecision::Post {
            method: HttpMethod::Post,
            ..
        }
        | MutationDecision::PreconditionedDelete {
            method: HttpMethod::Delete,
            ..
        } => true,
        MutationDecision::ConditionalApply {
            method: HttpMethod::Patch,
            content_type,
            field_manager,
            force: true,
            ..
        } => content_type == APPLY_PATCH_CONTENT_TYPE && field_manager == CONTROLLER_FIELD_MANAGER,
        _ => false,
    }
}

fn prepared_expected_target(
    intent: &ApplyIntent,
) -> Result<Option<MutationIdentity>, ReconcileError> {
    match (
        intent.expected_uid.as_deref(),
        intent.expected_resource_version.as_deref(),
    ) {
        (Some(uid), Some(resource_version)) => Ok(Some(MutationIdentity {
            uid: uid.into(),
            resource_version: resource_version.into(),
        })),
        (None, None) => Ok(None),
        _ => Err(ReconcileError::Mutation(
            "prepared intent has a partial target UID/resourceVersion pin".into(),
        )),
    }
}

fn plan_graph(
    state: &MachineStateV0,
    frozen: &FrozenInputV0,
    input_json: &str,
    output_json: &str,
    observed: Vec<DesiredObservationV0>,
) -> Result<Plan, ReconcileError> {
    if observed.len() > MAX_LIST_ENTRIES {
        return Err(ReconcileError::Desired("observations exceed bound".into()));
    }
    let input = parse_validate_render_input(input_json)
        .map_err(|e| ReconcileError::Renderer(e.to_string()))?;
    let output = parse_validate_render_output(output_json)
        .map_err(|e| ReconcileError::Renderer(e.to_string()))?;
    validate_input(state, frozen, &input.document)?;
    if input.sha256 != frozen.render_input_hash {
        return Err(ReconcileError::FrozenPinMismatch);
    }
    let dependencies = dependencies(&output.document.dependencies)?;
    let mut observations = observed
        .into_iter()
        .map(|item| (identity_key(&item.identity), item))
        .collect::<Vec<_>>();
    observations.sort_by(|left, right| left.0.cmp(&right.0));
    if observations
        .windows(2)
        .any(|items| items[0].0 == items[1].0)
    {
        return Err(ReconcileError::Desired(
            "observations contain duplicate identities".into(),
        ));
    }
    let mut observations = observations.into_iter().collect::<BTreeMap<_, _>>();
    let mut identities = BTreeSet::new();
    let mut logical = BTreeSet::new();
    let mut intents = Vec::new();
    let mut decisions = BTreeMap::new();
    if output.document.disposition != RenderDisposition::Hold {
        for resource in &output.document.resources {
            let identity = identity_from_object(&resource.object, &state.root.namespace)?;
            let key = identity_key(&identity);
            if !identities.insert(key.clone()) || !logical.insert(resource.logical_id.clone()) {
                return Err(ReconcileError::Desired(
                    "duplicate logical ID or physical identity".into(),
                ));
            }
            let Some(observed) = observations.remove(&key) else {
                return Err(ReconcileError::Desired(
                    "missing desired observation".into(),
                ));
            };
            let inventory = state
                .journal
                .state
                .inventory
                .iter()
                .find(|item| item.identity == identity);
            if matches!(
                &observed.qualification,
                BackendQualification::Unsupported { .. }
            ) {
                return Err(ReconcileError::Mutation(
                    "desired observation uses an unsupported backend".into(),
                ));
            }
            let intent = match (&observed.observation, inventory) {
                (Observation::Absent, _) => MutationIntent::Create {
                    observation: observed.observation.clone(),
                    object: resource.object.clone(),
                },
                (Observation::Existing { identity: actual }, Some(known))
                    if actual.uid == known.uid
                        && actual.resource_version == known.resource_version =>
                {
                    MutationIntent::Apply {
                        observation: observed.observation.clone(),
                        expected: MutationIdentity {
                            uid: known.uid.clone(),
                            resource_version: known.resource_version.clone(),
                        },
                        field_manager: "oxideloop-controller".into(),
                        object: resource.object.clone(),
                    }
                }
                (Observation::Existing { .. }, _) => MutationIntent::Create {
                    observation: observed.observation.clone(),
                    object: resource.object.clone(),
                },
            };
            let decision = plan(&MutationPlanInputV0 {
                probe_version: PROBE_VERSION_V0.into(),
                qualification: observed.qualification,
                intent,
            })
            .map_err(|e| ReconcileError::Mutation(e.to_string()))?;
            let command_id = hash(&[
                "apply-command/v0",
                &state.root.uid,
                &frozen.observation_token,
                &key,
                &resource.logical_id,
            ]);
            intents.push(ApplyIntent {
                command_id: command_id.clone(),
                operation: ApplyOperation::Apply,
                identity: identity.clone(),
                logical_id: resource.logical_id.clone(),
                expected_uid: inventory.map(|item| item.uid.clone()),
                expected_resource_version: inventory.map(|item| item.resource_version.clone()),
            });
            decisions.insert(command_id, decision);
        }
    }
    let mut absent = Vec::new();
    let mut candidate = None;
    if output.document.disposition != RenderDisposition::Hold {
        for inventory in &state.journal.state.inventory {
            if identities.contains(&identity_key(&inventory.identity)) {
                continue;
            }
            let Some(observed) = observations.remove(&identity_key(&inventory.identity)) else {
                return Err(ReconcileError::Desired(
                    "missing inventory observation".into(),
                ));
            };
            match observed.observation {
                Observation::Absent => absent.push(inventory.identity.clone()),
                Observation::Existing { identity: actual }
                    if actual.uid == inventory.uid
                        && actual.resource_version == inventory.resource_version =>
                {
                    let complete_key = prune_key(state, inventory);
                    let matching_candidate = state
                        .journal
                        .state
                        .prune_candidate
                        .as_ref()
                        .is_some_and(|old| {
                            old.complete_key == complete_key
                                && old.inventory == *inventory
                                && old.first_observation.observed_resource_version
                                    == actual.resource_version
                                && old.first_observation.observed_generation
                                    == frozen.root_generation
                                && old.second_observation.as_ref().map_or_else(
                                    || {
                                        old.first_observation.barrier_token
                                            != frozen.final_observation_fence_token
                                    },
                                    |second| {
                                        second.observed_generation == frozen.root_generation
                                            && second.observed_resource_version
                                                == actual.resource_version
                                            && second.barrier_token
                                                == frozen.final_observation_fence_token
                                    },
                                )
                        });
                    if matching_candidate {
                        let command_id = hash(&[
                            "prune-command/v0",
                            &state.root.uid,
                            &complete_key,
                            &actual.uid,
                            &actual.resource_version,
                        ]);
                        intents.push(ApplyIntent {
                            command_id: command_id.clone(),
                            operation: ApplyOperation::Delete,
                            identity: inventory.identity.clone(),
                            logical_id: inventory.logical_id.clone(),
                            expected_uid: Some(inventory.uid.clone()),
                            expected_resource_version: Some(inventory.resource_version.clone()),
                        });
                        decisions.insert(
                            command_id,
                            MutationDecision::PreconditionedDelete {
                                method: HttpMethod::Delete,
                                preconditions: MutationIdentity {
                                    uid: inventory.uid.clone(),
                                    resource_version: inventory.resource_version.clone(),
                                },
                            },
                        );
                        // Once the second matching observation is durable,
                        // retain it verbatim during revalidation. Rebuilding a
                        // different candidate here would invalidate the very
                        // prepared delete it is meant to authorize.
                        candidate = Some(state.journal.state.prune_candidate.clone().map_or_else(
                            || PruneCandidate {
                                complete_key,
                                inventory: inventory.clone(),
                                first_observation: PruneObservation {
                                    observed_resource_version: actual.resource_version.clone(),
                                    observed_generation: frozen.root_generation,
                                    barrier_token: frozen.final_observation_fence_token.clone(),
                                },
                                second_observation: Some(PruneObservation {
                                    observed_resource_version: actual.resource_version.clone(),
                                    observed_generation: frozen.root_generation,
                                    barrier_token: frozen.final_observation_fence_token.clone(),
                                }),
                            },
                            |mut old| {
                                old.second_observation = Some(PruneObservation {
                                    observed_resource_version: actual.resource_version.clone(),
                                    observed_generation: frozen.root_generation,
                                    barrier_token: frozen.final_observation_fence_token.clone(),
                                });
                                old
                            },
                        ));
                    } else if candidate.is_none() {
                        candidate = Some(PruneCandidate {
                            complete_key,
                            inventory: inventory.clone(),
                            first_observation: PruneObservation {
                                observed_resource_version: actual.resource_version,
                                observed_generation: frozen.root_generation,
                                barrier_token: frozen.final_observation_fence_token.clone(),
                            },
                            second_observation: None,
                        });
                    }
                }
                // A UID/RV mismatch clears the old candidate below, ensuring
                // the next matching observation starts a new consecutive pair.
                Observation::Existing { .. } => {}
            }
        }
    }
    intents.sort_by(|a, b| a.command_id.cmp(&b.command_id));
    // The durable graph pin deliberately excludes live inventory,
    // observations, effects and prune progress. Those evolve after each
    // command acknowledgement; remaining prepared intents are checked below
    // against a fresh plan before an additional effect can issue.
    let desired_graph = serde_json::to_value(&output.document)
        .map_err(|e| ReconcileError::Renderer(e.to_string()))?;
    let hash = sha256_hex(
        &canonical_bytes(&desired_graph).map_err(|e| ReconcileError::Renderer(e.to_string()))?,
    );
    Ok(Plan {
        disposition: output.document.disposition,
        hash,
        intents,
        decisions,
        dependencies,
        candidate,
        absent,
    })
}

fn prepare(
    state: &mut MachineStateV0,
    frozen: FrozenInputV0,
    plan: Plan,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    plan.validate_initial_effects()?;
    state
        .journal
        .state
        .inventory
        .retain(|item| !plan.absent.contains(&item.identity));
    // An absent or UID/RV-mismatched object explicitly removes a candidate;
    // only this plan's exact observation may establish a new first/second one.
    state.journal.state.prune_candidate = plan.candidate;
    let attempt = hash(&[
        "attempt/v0",
        &state.root.uid,
        &state.root.generation.to_string(),
        &state.root.root_resource_version,
        &state.root.state_resource_version,
        &plan.hash,
    ]);
    let transaction_id = hash(&[
        "transaction/v0",
        &state.root.uid,
        &frozen.observation_token,
        &plan.hash,
    ]);
    let recovery_claims = plan
        .intents
        .iter()
        .map(|intent| PreparedRecoveryClaim {
            root_uid: state.root.uid.clone(),
            transaction_id: transaction_id.clone(),
            command_id: intent.command_id.clone(),
        })
        .collect();
    let next_command_lifecycle = match plan.intents.first() {
        Some(intent) => NextCommandLifecycleV0::prepared(intent.command_id.clone()),
        None => NextCommandLifecycleV0::complete(),
    };
    state.journal.state.apply_transaction = Some(ApplyTransaction {
        id: transaction_id,
        generation: frozen.root_generation,
        frozen_render_input_hash: frozen.render_input_hash.clone(),
        desired_graph_hash: plan.hash,
        recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
        attempt_token: attempt,
        prepared_intents: plan.intents,
        next_command_lifecycle,
        recovery_claims,
        results: vec![],
        unresolved_identities: vec![],
    });
    state.journal.state.pending_dependencies = plan.dependencies;
    state.frozen = Some(frozen);
    queue_persist(
        state,
        PersistPurpose::Prepare,
        DurablePhase::Prepare,
        PhaseV0::PersistPending,
        commands,
    )
}

fn validate_input(
    state: &MachineStateV0,
    frozen: &FrozenInputV0,
    input: &RenderInputV0,
) -> Result<(), ReconcileError> {
    if input.root.uid != state.root.uid
        || input.root.namespace != state.root.namespace
        || input.root.name != state.root.name
        || input.root.spec
            != serde_json::to_value(&state.root.spec)
                .map_err(|error| ReconcileError::Renderer(error.to_string()))?
        || input.bundle_digest != state.root.bundle_digest
        || input.root.generation != u64::try_from(frozen.root_generation).unwrap_or(u64::MAX)
        || input.observation_token != frozen.observation_token
        || input.cache_epoch != frozen.cache_epoch
    {
        return Err(ReconcileError::FrozenPinMismatch);
    }
    Ok(())
}

fn dependencies(items: &[Dependency]) -> Result<Vec<DependencySnapshot>, ReconcileError> {
    let mut snapshots = Vec::new();
    for item in items {
        match item {
            Dependency::Object {
                api_version,
                kind,
                namespace,
                name,
                uid: Some(uid),
                resource_version: Some(resource_version),
                observation_token,
            } => snapshots.push(DependencySnapshot {
                type_: DependencySnapshotType::ObjectPresent,
                identity: ObjectIdentity {
                    api_version: api_version.clone(),
                    kind: kind.clone(),
                    namespace: namespace.clone(),
                    name: name.clone(),
                },
                uid: Some(uid.clone()),
                resource_version: Some(resource_version.clone()),
                selector: None,
                collection_revision: None,
                observation_token: observation_token.clone(),
            }),
            Dependency::Object {
                api_version,
                kind,
                namespace,
                name,
                uid: None,
                resource_version: None,
                observation_token,
            } => snapshots.push(DependencySnapshot {
                type_: DependencySnapshotType::ObjectAbsent,
                identity: ObjectIdentity {
                    api_version: api_version.clone(),
                    kind: kind.clone(),
                    namespace: namespace.clone(),
                    name: name.clone(),
                },
                uid: None,
                resource_version: None,
                selector: None,
                collection_revision: None,
                observation_token: observation_token.clone(),
            }),
            Dependency::Collection {
                api_version,
                kind,
                namespace,
                selector,
                collection_revision,
                observation_token,
            } => snapshots.push(DependencySnapshot {
                type_: DependencySnapshotType::Collection,
                identity: ObjectIdentity {
                    api_version: api_version.clone(),
                    kind: kind.clone(),
                    namespace: namespace.clone(),
                    name: "collection".into(),
                },
                uid: None,
                resource_version: None,
                selector: Some(selector.clone()),
                collection_revision: Some(collection_revision.clone()),
                observation_token: observation_token.clone(),
            }),
            Dependency::Object { .. } => {
                return Err(ReconcileError::Desired(
                    "dependency object must be wholly present or absent".into(),
                ));
            }
        }
    }
    snapshots.sort_by_key(dependency_key);
    Ok(snapshots)
}

fn validate_root_and_journal(
    root: &RootSnapshotV0,
    journal: &OxideLoopStateSpec,
) -> Result<(), ReconcileError> {
    if root.root_resource_version.is_empty() {
        return Err(ReconcileError::Link(
            "root resourceVersion is required for status writes".into(),
        ));
    }
    journal
        .validate()
        .map_err(|e| ReconcileError::Link(e.to_string()))?;
    if journal.state.deletion.is_none() && root.bundle_digest != journal.root.bundle_digest {
        return Err(ReconcileError::Link(
            "nondeleting root bundle digest changed".into(),
        ));
    }
    validate_journal_generation(root, journal)?;
    validate_root_state_link(
        &root.uid,
        &root.status,
        &root.state_name,
        &root.state_uid,
        journal,
    )
    .map_err(|e| ReconcileError::Link(e.to_string()))
}

fn validate_root_only_deletion_machine(state: &MachineStateV0) -> Result<(), ReconcileError> {
    validate_bootstrap_root(&state.root)?;
    let receipt = validate_deleting_root_receipt(&state.root)?;
    if state.root.state_name != receipt.state_name
        || state.root.state_uid != receipt.state_uid
        || !state.root.state_resource_version.is_empty()
        || state.root.state_finalizer_present
    {
        return Err(ReconcileError::Recovery(
            "root-only deletion state pins do not match the cleanup receipt".into(),
        ));
    }
    state
        .journal
        .validate()
        .map_err(|error| ReconcileError::Recovery(error.to_string()))?;
    if state.journal.root.uid != state.root.uid
        || state.journal.root.bundle_digest != state.root.bundle_digest
        || state.journal.state
            != (StateBody {
                deletion: Some(DeletionTransaction {
                    id: receipt.deletion_id.clone(),
                    deletion_start_generation: receipt.deletion_start_generation,
                }),
                ..StateBody::default()
            })
    {
        return Err(ReconcileError::Recovery(
            "root-only deletion journal exceeds receipt authority".into(),
        ));
    }
    Ok(())
}

fn validate_bootstrap_root(root: &RootSnapshotV0) -> Result<(), ReconcileError> {
    for (field, value) in [
        ("root.uid", root.uid.as_str()),
        ("root.namespace", root.namespace.as_str()),
        ("root.name", root.name.as_str()),
        ("root.bundleDigest", root.bundle_digest.as_str()),
        ("root.resourceVersion", root.root_resource_version.as_str()),
    ] {
        if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
            return Err(ReconcileError::Link(format!(
                "{field} is invalid for bootstrap"
            )));
        }
    }
    if root.generation < 0 {
        return Err(ReconcileError::Link(
            "root generation is invalid for bootstrap".into(),
        ));
    }
    root.spec
        .validate()
        .map_err(|error| ReconcileError::Link(error.to_string()))?;
    if root.spec.program.digest != root.bundle_digest {
        return Err(ReconcileError::Link(
            "root program digest does not match root bundle digest".into(),
        ));
    }
    Ok(())
}

fn validate_journal_generation(
    root: &RootSnapshotV0,
    journal: &OxideLoopStateSpec,
) -> Result<(), ReconcileError> {
    if let Some(transaction) = &journal.state.apply_transaction {
        let allowed_old_generation =
            journal.state.deletion.as_ref().is_some_and(|deletion| {
                transaction.generation == deletion.deletion_start_generation
            }) || journal
                .state
                .generation_advance_closure
                .as_ref()
                .is_some_and(|closure| transaction.generation == closure.source_generation);
        let recoverable_old_generation = journal.state.deletion.is_none()
            && transaction.generation >= 0
            && transaction.generation < root.generation;
        if transaction.generation != root.generation
            && !allowed_old_generation
            && !recoverable_old_generation
        {
            return Err(ReconcileError::Link(
                "apply transaction generation does not match root or closure".into(),
            ));
        }
    }
    if let Some(operation) = &journal.state.pending_persist
        && operation.root_generation != root.generation
    {
        return Err(ReconcileError::Link(
            "pending persist generation does not match root generation".into(),
        ));
    }
    Ok(())
}

fn validate_bootstrap_machine(state: &MachineStateV0) -> Result<(), ReconcileError> {
    validate_bootstrap_root(&state.root)?;
    state
        .journal
        .validate()
        .map_err(|error| ReconcileError::Link(error.to_string()))?;
    validate_journal_generation(&state.root, &state.journal)?;
    if state.journal.root.uid != state.root.uid
        || state.journal.root.bundle_digest != state.root.bundle_digest
    {
        return Err(ReconcileError::Link(
            "bootstrap journal does not bind the observed root".into(),
        ));
    }
    Ok(())
}

fn bootstrap_confirmed(state: &MachineStateV0) -> bool {
    state.root.root_finalizer_present
        && state.root.state_finalizer_present
        && validate_root_and_journal(&state.root, &state.journal).is_ok()
}

fn is_raw_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn pins(root: &RootSnapshotV0) -> PreconditionsV0 {
    PreconditionsV0 {
        root_uid: root.uid.clone(),
        root_generation: root.generation,
        root_resource_version: root.root_resource_version.clone(),
        state_uid: root.state_uid.clone(),
        state_resource_version: root.state_resource_version.clone(),
        observation_token: String::new(),
        cache_epoch: String::new(),
    }
}
fn matches_root(state: &MachineStateV0, frozen: &FrozenInputV0) -> bool {
    frozen.root_generation == state.root.generation
        && frozen.state_resource_version == state.root.state_resource_version
        && is_raw_sha256(&frozen.final_observation_fence_token)
}
fn stale(commands: &mut Vec<CommandV0>, reason: &str) {
    commands.push(CommandV0::IgnoredStale {
        reason: reason.into(),
    });
}
fn finish(state: MachineStateV0, commands: Vec<CommandV0>) -> Result<TransitionV0, ReconcileError> {
    if commands.len() > MAX_COMMANDS {
        Err(ReconcileError::CommandBound)
    } else {
        if state.root_only_deletion_recovery {
            validate_root_only_deletion_machine(&state)?;
            if commands
                .iter()
                .any(|command| !root_only_command_allowed(command))
            {
                return Err(ReconcileError::Recovery(
                    "command exceeds root-only deletion recovery authority".into(),
                ));
            }
        } else if matches!(
            state.phase,
            PhaseV0::Bootstrap | PhaseV0::BootstrapPending | PhaseV0::Recovery
        ) {
            validate_bootstrap_machine(&state)?;
        } else {
            validate_root_and_journal(&state.root, &state.journal)?;
        }
        Ok(TransitionV0 { state, commands })
    }
}
fn identity_from_object(object: &Value, namespace: &str) -> Result<ObjectIdentity, ReconcileError> {
    let metadata = object
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| ReconcileError::Desired("resource.metadata must be an object".into()))?;
    if metadata
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or(namespace)
        != namespace
    {
        return Err(ReconcileError::Desired(
            "resource namespace escapes root namespace".into(),
        ));
    }
    Ok(ObjectIdentity {
        api_version: object
            .get("apiVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        kind: object
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        namespace: namespace.into(),
        name: metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
    })
}
fn identity_key(identity: &ObjectIdentity) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        identity.api_version, identity.kind, identity.namespace, identity.name
    )
}
fn dependency_key(item: &DependencySnapshot) -> String {
    format!(
        "{:?}\0{}\0{}\0{}",
        item.type_,
        identity_key(&item.identity),
        item.selector.as_deref().unwrap_or_default(),
        item.collection_revision.as_deref().unwrap_or_default()
    )
}
fn prune_key(state: &MachineStateV0, item: &InventoryEntry) -> String {
    hash(&[
        "prune/v0",
        &state.root.uid,
        &state.root.bundle_digest,
        &identity_key(&item.identity),
        &item.uid,
        &item.resource_version,
    ])
}
fn inventory_hash(items: &[InventoryEntry]) -> String {
    sha256_hex(
        &canonical_bytes(&serde_json::to_value(items).expect("inventory serializable"))
            .expect("inventory canonical"),
    )
}
fn hash(parts: &[&str]) -> String {
    sha256_hex(parts.join("\0").as_bytes())
}
fn sort_automation(state: &mut MachineStateV0) {
    state.journal.state.automation.baselines.sort_by(|a, b| {
        (&a.automation_id, identity_key(&a.source))
            .cmp(&(&b.automation_id, identity_key(&b.source)))
    });
    state
        .journal
        .state
        .automation
        .pending
        .sort_by(|a, b| a.occurrence_id.cmp(&b.occurrence_id));
    state
        .journal
        .state
        .automation
        .completions
        .sort_by(|a, b| a.occurrence_id.cmp(&b.occurrence_id));
}

#[cfg(test)]
#[allow(clippy::too_many_lines)]
mod tests {
    use super::*;
    use oxideloop_controller_api::{
        ConditionStatus, ProgramBundleReference, RootBinding, RootCondition, RootConditionType,
        STATE_VERSION_V1, StateBody, StateLink, WebApplicationSpec, bootstrap_id, state_name,
    };
    use oxideloop_renderer_abi::RenderOutputV0;
    use oxideloop_renderer_abi::model::{ABI_VERSION_V0, ResourceDeclaration, RootProjection};

    fn web_application_spec(digest: impl Into<String>) -> WebApplicationSpec {
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

    fn state() -> MachineStateV0 {
        let root_uid = "root-uid";
        let state_name = state_name(root_uid).unwrap();
        MachineStateV0::new(
            RootSnapshotV0 {
                uid: root_uid.into(),
                namespace: "demo".into(),
                name: "demo".into(),
                generation: 7,
                spec: web_application_spec("a".repeat(64)),
                bundle_digest: "a".repeat(64),
                state_name: state_name.clone(),
                state_uid: "state-uid".into(),
                state_resource_version: "11".into(),
                root_resource_version: "20".into(),
                root_finalizer_present: true,
                state_finalizer_present: true,
                deleting: false,
                status: WebApplicationStatus {
                    state: Some(StateLink {
                        root_uid: root_uid.into(),
                        name: state_name,
                        uid: "state-uid".into(),
                    }),
                    ..WebApplicationStatus::default()
                },
            },
            OxideLoopStateSpec {
                state_version: STATE_VERSION_V1,
                root: RootBinding {
                    uid: root_uid.into(),
                    bootstrap_id: bootstrap_id(root_uid).unwrap(),
                    bundle_digest: "a".repeat(64),
                },
                state: StateBody::default(),
            },
        )
        .unwrap()
    }

    #[test]
    fn mutation_command_keeps_prepared_target_separate_from_decision_expected() {
        let command = MutationCommandV0 {
            command_id: "c".repeat(64),
            attempt_token: "a".repeat(64),
            preconditions: PreconditionsV0 {
                root_uid: "root-uid".into(),
                root_generation: 7,
                root_resource_version: "root-rv".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "state-rv".into(),
                observation_token: "observation".into(),
                cache_epoch: "epoch".into(),
            },
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "demo".into(),
                name: "config".into(),
            },
            logical_id: "config".into(),
            prepared_expected_target: Some(MutationIdentity {
                uid: "durable-uid".into(),
                resource_version: "durable-rv".into(),
            }),
            decision: MutationDecision::ConditionalApply {
                method: HttpMethod::Patch,
                content_type: APPLY_PATCH_CONTENT_TYPE.into(),
                field_manager: CONTROLLER_FIELD_MANAGER.into(),
                force: true,
                expected: MutationIdentity {
                    uid: "decision-uid".into(),
                    resource_version: "decision-rv".into(),
                },
                body: serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": "config"}
                }),
            },
        };

        let encoded = serde_json::to_value(&command).unwrap();
        assert_eq!(encoded["preparedExpectedTarget"]["uid"], "durable-uid");
        assert_eq!(encoded["decision"]["expected"]["uid"], "decision-uid");
        assert_eq!(
            serde_json::from_value::<MutationCommandV0>(encoded).unwrap(),
            command
        );

        let mut legacy = serde_json::to_value(&command).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("preparedExpectedTarget");
        assert!(
            serde_json::from_value::<MutationCommandV0>(legacy.clone())
                .unwrap()
                .prepared_expected_target
                .is_none()
        );
        legacy
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Null);
        assert!(serde_json::from_value::<MutationCommandV0>(legacy).is_err());
    }

    #[test]
    fn prepared_target_option_tracks_the_durable_intent_pin_pair() {
        let identity = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "config".into(),
        };
        let post = ApplyIntent {
            command_id: "p".repeat(64),
            operation: ApplyOperation::Apply,
            identity: identity.clone(),
            logical_id: "config".into(),
            expected_uid: None,
            expected_resource_version: None,
        };
        let pinned = ApplyIntent {
            command_id: "a".repeat(64),
            operation: ApplyOperation::Apply,
            identity: identity.clone(),
            logical_id: "config".into(),
            expected_uid: Some("uid".into()),
            expected_resource_version: Some("rv".into()),
        };
        let delete = ApplyIntent {
            command_id: "d".repeat(64),
            operation: ApplyOperation::Delete,
            identity,
            logical_id: "config".into(),
            expected_uid: Some("uid".into()),
            expected_resource_version: Some("rv".into()),
        };

        assert_eq!(prepared_expected_target(&post).unwrap(), None);
        for intent in [&pinned, &delete] {
            assert_eq!(
                prepared_expected_target(intent).unwrap(),
                Some(MutationIdentity {
                    uid: "uid".into(),
                    resource_version: "rv".into(),
                })
            );
        }
    }

    fn boot_to_render(mut machine: MachineStateV0) -> MachineStateV0 {
        let started = reduce(machine, EventV0::Start).unwrap();
        let descriptor_token = match &started.commands[0] {
            CommandV0::RequestDescriptor { token, .. } => token.clone(),
            _ => panic!("expected descriptor request"),
        };
        let descriptor = reduce(
            started.state,
            EventV0::DescriptorReady {
                token: descriptor_token.clone(),
                bundle_digest: "a".repeat(64),
            },
        )
        .unwrap();
        let (token, old) = match &descriptor.commands[0] {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            } => (token.clone(), old_root_resource_version.clone()),
            _ => panic!("expected root status"),
        };
        machine = reduce(
            descriptor.state,
            EventV0::RootStatusPersisted {
                token,
                old_root_resource_version: old,
                new_root_resource_version: "21".into(),
                root_uid: "root-uid".into(),
            },
        )
        .unwrap()
        .state;
        let mut machine = reduce(machine, EventV0::InformersReady).unwrap().state;
        // Keep ordinary protocol tests focused on state persistence; the
        // dedicated root-status test covers the status continuation branch.
        machine.root.status.input_token = Some("observation-1".into());
        machine
    }

    fn rendered(
        root: &RootSnapshotV0,
        dependencies: Vec<Dependency>,
    ) -> (FrozenInputV0, String, String, Vec<DesiredObservationV0>) {
        let input = RenderInputV0 {
            abi_version: ABI_VERSION_V0.into(),
            root: RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: root.namespace.clone(),
                name: root.name.clone(),
                uid: root.uid.clone(),
                generation: root.generation as u64,
                spec: serde_json::to_value(&root.spec).unwrap(),
            },
            bundle_digest: root.bundle_digest.clone(),
            cache_epoch: "epoch-1".into(),
            observation_token: "observation-1".into(),
            observations: vec![],
        };
        let input_json = serde_json::to_string(&input).unwrap();
        let output = RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::Apply,
            resources: vec![ResourceDeclaration {
                logical_id: "object".into(),
                object: serde_json::json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"namespace":"demo","name":"object"}}),
                readiness: None,
            }],
            dependencies,
            automations: vec![],
            diagnostics: vec![],
        };
        let identity = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "object".into(),
        };
        (
            FrozenInputV0 {
                root_generation: root.generation,
                state_resource_version: root.state_resource_version.clone(),
                observation_token: "observation-1".into(),
                cache_epoch: "epoch-1".into(),
                final_observation_fence_token: "f".repeat(64),
                render_input_hash: parse_validate_render_input(&input_json).unwrap().sha256,
            },
            input_json,
            serde_json::to_string(&output).unwrap(),
            vec![DesiredObservationV0 {
                identity,
                observation: Observation::Absent,
                qualification: BackendQualification::BuiltInTested {
                    kubernetes_minor: "1.36".into(),
                    gvr: "configmaps".into(),
                },
                recovery_proof: None,
                deletion_recovery_proof: None,
            }],
        )
    }

    #[test]
    fn rendered_root_projection_requires_exact_name_and_spec_authority() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let mut wrong_name: RenderInputV0 = serde_json::from_str(&input).unwrap();
        wrong_name.root.name = "forged-name".into();
        let wrong_name = serde_json::to_string(&wrong_name).unwrap();
        let wrong_name_frozen = FrozenInputV0 {
            render_input_hash: parse_validate_render_input(&wrong_name).unwrap().sha256,
            ..frozen.clone()
        };
        assert_eq!(
            reduce(
                machine.clone(),
                EventV0::Rendered {
                    frozen: wrong_name_frozen,
                    render_input_json: wrong_name,
                    render_output_json: output.clone(),
                    observed: observed.clone(),
                }
            ),
            Err(ReconcileError::FrozenPinMismatch)
        );

        let mut wrong_spec: RenderInputV0 = serde_json::from_str(&input).unwrap();
        wrong_spec.root.spec = serde_json::json!({"message":"forged"});
        let wrong_spec = serde_json::to_string(&wrong_spec).unwrap();
        let wrong_spec_frozen = FrozenInputV0 {
            render_input_hash: parse_validate_render_input(&wrong_spec).unwrap().sha256,
            ..frozen
        };
        assert_eq!(
            reduce(
                machine,
                EventV0::Rendered {
                    frozen: wrong_spec_frozen,
                    render_input_json: wrong_spec,
                    render_output_json: output,
                    observed,
                }
            ),
            Err(ReconcileError::FrozenPinMismatch)
        );
    }

    fn complete_single_apply_to_ready(
        machine: MachineStateV0,
        frozen: FrozenInputV0,
        input: String,
        output: String,
        observed: Vec<DesiredObservationV0>,
        uid: &str,
        resource_version: &str,
    ) -> MachineStateV0 {
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let prepare = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare.token,
                old_state_resource_version: prepare.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let issued = reduce(
            revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let issued = acknowledge_dispatch_authorization(issued, "13");
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
            command => panic!("single apply must issue its one prepared effect: {command:?}"),
        };
        let acknowledged = reduce(
            issued.state,
            EventV0::MutationAcknowledged {
                transaction_id,
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: command.command_id.clone(),
                    attempt_token,
                    identity: command.identity.clone(),
                    outcome: ApplyOutcome::Applied,
                    uid: Some(uid.into()),
                    resource_version: Some(resource_version.into()),
                }],
            },
        )
        .unwrap();
        let mutation_result = match &acknowledged.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let revalidate_completion = reduce(
            acknowledged.state,
            EventV0::Persisted {
                token: mutation_result.token,
                old_state_resource_version: mutation_result.old_state_resource_version,
                new_state_resource_version: "14".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let mut fresh_observed = observed;
        fresh_observed
            .iter_mut()
            .find(|item| item.identity == command.identity)
            .unwrap()
            .observation = Observation::Existing {
            identity: MutationIdentity {
                uid: uid.into(),
                resource_version: resource_version.into(),
            },
        };
        let completion = reduce(
            revalidate_completion.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate_completion.state.frozen.clone().unwrap(),
                render_input_json: input,
                render_output_json: output,
                observed: fresh_observed,
            },
        )
        .unwrap();
        let completion_persist = match &completion.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!("single result must revalidate to completion persistence"),
        };
        reduce(
            completion.state,
            EventV0::Persisted {
                token: completion_persist.token,
                old_state_resource_version: completion_persist.old_state_resource_version,
                new_state_resource_version: "15".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap()
        .state
    }

    fn acknowledge_dispatch_authorization(
        authorized: TransitionV0,
        new_state_resource_version: &str,
    ) -> TransitionV0 {
        let operation = match &authorized.commands[0] {
            CommandV0::Persist { operation, .. }
                if operation.purpose == PersistPurpose::DispatchAuthorization =>
            {
                operation.clone()
            }
            command => panic!("expected dispatch authorization persist, got {command:?}"),
        };
        reduce(
            authorized.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: new_state_resource_version.into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap()
    }

    #[test]
    fn phase_guards_and_deletion_preemption_emit_no_old_effect() {
        let machine = state();
        assert!(matches!(
            reduce(machine.clone(), EventV0::InformersReady)
                .unwrap()
                .commands
                .as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
        let deletion = reduce(machine, EventV0::BeginDeletion).unwrap();
        assert_eq!(deletion.state.phase, PhaseV0::DeletionStartPersistPending);
        assert!(matches!(
            deletion.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
    }

    #[test]
    fn deletion_requires_owned_children_absence_ack_before_state_finalizer() {
        let started = reduce(state(), EventV0::BeginDeletion).unwrap();
        let restarted =
            MachineStateV0::recover_json(&serde_json::to_string(&started.state).unwrap()).unwrap();
        let restart_start = reduce(restarted, EventV0::Start).unwrap();
        assert!(matches!(
            restart_start.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
        let start_operation = match &started.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let prepared = reduce(
            started.state,
            EventV0::Persisted {
                token: start_operation.token,
                old_state_resource_version: start_operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let prepared_restart = reduce(
            MachineStateV0::recover_json(&serde_json::to_string(&prepared.state).unwrap()).unwrap(),
            EventV0::Start,
        )
        .unwrap();
        assert!(matches!(
            prepared_restart.commands.as_slice(),
            [CommandV0::RequestDeletionObservation { .. }]
        ));
        let prepare_operation = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let observe = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare_operation.token,
                old_state_resource_version: prepare_operation.old_state_resource_version,
                new_state_resource_version: "13".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            observe.commands.as_slice(),
            [CommandV0::RequestDeletionObservation { .. }]
        ));
        let verify = reduce(
            observe.state,
            EventV0::DeletionRevalidated {
                observed_inventory: vec![],
                observed_automation_runs: vec![],
            },
        )
        .unwrap();
        let token = match &verify.commands[0] {
            CommandV0::VerifyOwnedChildrenAbsent { token, .. } => token.clone(),
            _ => panic!("finalizer must wait for owned-child verification"),
        };
        let verification_restart = reduce(
            MachineStateV0::recover_json(&serde_json::to_string(&verify.state).unwrap()).unwrap(),
            EventV0::Start,
        )
        .unwrap();
        assert!(matches!(
            verification_restart.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
        let stale = reduce(
            verify.state.clone(),
            EventV0::OwnedChildrenVerifiedAbsent {
                token: "0".repeat(64),
                cleanup_token: "0".repeat(64),
                root_uid: "root-uid".into(),
                root_generation: 7,
                root_resource_version: "20".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "13".into(),
            },
        )
        .unwrap();
        assert_eq!(stale.state.phase, PhaseV0::OwnedChildrenVerifyPending);
        let receipt = reduce(
            verify.state,
            EventV0::OwnedChildrenVerifiedAbsent {
                token: token.clone(),
                cleanup_token: token,
                root_uid: "root-uid".into(),
                root_generation: 7,
                root_resource_version: "20".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "13".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            receipt.commands.as_slice(),
            [CommandV0::PersistRootStatus { .. }]
        ));
        let (status_token, old_root_rv) = match &receipt.commands[0] {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            } => (token.clone(), old_root_resource_version.clone()),
            _ => unreachable!(),
        };
        let finalizer = reduce(
            receipt.state,
            EventV0::RootStatusPersisted {
                token: status_token,
                old_root_resource_version: old_root_rv,
                new_root_resource_version: "21".into(),
                root_uid: "root-uid".into(),
            },
        )
        .unwrap();
        let token = match &finalizer.commands[0] {
            CommandV0::RemoveStateFinalizer { token, .. } => token.clone(),
            _ => unreachable!(),
        };
        let stale_finalizer = reduce(
            finalizer.state.clone(),
            EventV0::StateFinalizerRemoved {
                token: "0".repeat(64),
                state_uid: "state-uid".into(),
                old_state_resource_version: "13".into(),
                new_state_resource_version: "14".into(),
            },
        )
        .unwrap();
        assert!(stale_finalizer.state.root.state_finalizer_present);
        let root_finalizer = reduce(
            finalizer.state,
            EventV0::StateFinalizerRemoved {
                token,
                state_uid: "state-uid".into(),
                old_state_resource_version: "13".into(),
                new_state_resource_version: "14".into(),
            },
        )
        .unwrap();
        let second_token = match &root_finalizer.commands[0] {
            CommandV0::VerifyOwnedChildrenAbsent {
                token,
                preconditions,
            } => {
                assert_eq!(preconditions.state_resource_version, "14");
                token.clone()
            }
            _ => panic!("state acknowledgement must trigger a fresh absence barrier"),
        };
        let root_finalizer = reduce(
            root_finalizer.state,
            EventV0::OwnedChildrenVerifiedAbsent {
                token: second_token.clone(),
                cleanup_token: second_token,
                root_uid: "root-uid".into(),
                root_generation: 7,
                root_resource_version: "21".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "14".into(),
            },
        )
        .unwrap();
        let root_finalizer_token = match &root_finalizer.commands[0] {
            CommandV0::RemoveRootFinalizer {
                token,
                preconditions,
            } => {
                assert_eq!(preconditions.state_resource_version, "14");
                token.clone()
            }
            _ => panic!("state acknowledgement must pin its new RV before root finalization"),
        };
        let absence = reduce(
            root_finalizer.state,
            EventV0::RootFinalizerRemoved {
                token: root_finalizer_token,
                root_uid: "root-uid".into(),
                old_root_resource_version: "21".into(),
                new_root_resource_version: "22".into(),
            },
        )
        .unwrap();
        let absence_token = match &absence.commands[0] {
            CommandV0::VerifyRootUidAbsent {
                token, root_uid, ..
            } => {
                assert_eq!(root_uid, "root-uid");
                token.clone()
            }
            _ => panic!("root finalizer acknowledgement must be followed by an exact UID read"),
        };
        let terminal = reduce(
            absence.state,
            EventV0::RootUidVerifiedAbsent {
                token: absence_token,
                root_uid: "root-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(terminal.state.phase, PhaseV0::Terminal);
        assert!(terminal.commands.is_empty());
    }

    #[test]
    fn deleting_state_present_restarts_without_receipt_in_every_pre_receipt_phase() {
        let mut machine = state();
        let inventory = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "demo".into(),
                name: "managed".into(),
            },
            logical_id: "managed".into(),
            uid: "managed-uid".into(),
            resource_version: "managed-rv-1".into(),
            bundle_digest: "a".repeat(64),
        };
        machine.journal.state.inventory.push(inventory.clone());

        let deletion_start = reduce(machine, EventV0::BeginDeletion).unwrap();
        let deletion_start_operation = match &deletion_start.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            command => panic!("unexpected deletion-start command: {command:?}"),
        };
        let deletion_prepare = reduce(
            deletion_start.state.clone(),
            EventV0::Persisted {
                token: deletion_start_operation.token,
                old_state_resource_version: deletion_start_operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let deletion_prepare_operation = match &deletion_prepare.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            command => panic!("unexpected deletion-prepare command: {command:?}"),
        };
        let deletion_observation = reduce(
            deletion_prepare.state.clone(),
            EventV0::Persisted {
                token: deletion_prepare_operation.token,
                old_state_resource_version: deletion_prepare_operation.old_state_resource_version,
                new_state_resource_version: "13".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let deletion_mutation = reduce(
            deletion_observation.state.clone(),
            EventV0::DeletionRevalidated {
                observed_inventory: vec![inventory.clone()],
                observed_automation_runs: vec![],
            },
        )
        .unwrap();
        let deletion_mutation = acknowledge_dispatch_authorization(deletion_mutation, "14");
        let (transaction_id, attempt_token, command) = match &deletion_mutation.commands[0] {
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
        let deletion_result = reduce(
            deletion_mutation.state.clone(),
            EventV0::MutationAcknowledged {
                transaction_id,
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: command.command_id,
                    attempt_token,
                    identity: inventory.identity,
                    outcome: ApplyOutcome::Deleted,
                    uid: Some("managed-uid".into()),
                    resource_version: Some("managed-rv-1".into()),
                }],
            },
        )
        .unwrap();
        let deletion_result_operation = match &deletion_result.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            command => panic!("unexpected deletion-result command: {command:?}"),
        };
        let owned_children = reduce(
            deletion_result.state.clone(),
            EventV0::Persisted {
                token: deletion_result_operation.token,
                old_state_resource_version: deletion_result_operation.old_state_resource_version,
                new_state_resource_version: "14".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();

        let restart_phases = vec![
            ("deletion-start", deletion_start.state),
            ("deletion-prepare", deletion_prepare.state),
            ("deletion-observation", deletion_observation.state),
            ("deletion-mutation", deletion_mutation.state),
            ("deletion-result", deletion_result.state),
            ("owned-children", owned_children.state),
        ]
        .into_boxed_slice();
        for (phase, persisted_state) in restart_phases {
            assert!(
                persisted_state
                    .root
                    .status
                    .deletion_cleanup_receipt
                    .is_none()
            );
            assert!(!persisted_state.root.state_uid.is_empty());
            let recreated =
                MachineStateV0::recover_json(&serde_json::to_string(&persisted_state).unwrap())
                    .unwrap_or_else(|error| panic!("{phase} state did not recover: {error}"));
            let resumed = reduce(recreated, EventV0::Start)
                .unwrap_or_else(|error| panic!("{phase} state did not resume: {error}"));
            assert!(
                matches!(
                    resumed.commands.as_slice(),
                    [CommandV0::Persist { .. }
                        | CommandV0::RequestDeletionObservation { .. }
                        | CommandV0::Hold { .. }]
                ),
                "{phase} resumed outside durable deletion: {:?}",
                resumed.commands
            );
            assert!(resumed.commands.iter().all(|command| !matches!(
                command,
                CommandV0::EnsureBootstrap { .. }
                    | CommandV0::RequestDescriptor { .. }
                    | CommandV0::Mutate { .. }
            )));
        }
    }

    #[test]
    fn deletion_mutation_failure_is_terminal_until_revalidated() {
        let mut machine = state();
        let identity = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "old".into(),
        };
        machine.journal.state.deletion = Some(DeletionTransaction {
            id: "d".repeat(64),
            deletion_start_generation: 7,
        });
        machine.journal.state.apply_transaction = Some(ApplyTransaction {
            id: "d".repeat(64),
            generation: 7,
            frozen_render_input_hash: "0".repeat(64),
            desired_graph_hash: "e".repeat(64),
            recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
            attempt_token: "f".repeat(64),
            prepared_intents: vec![ApplyIntent {
                command_id: "1".repeat(64),
                operation: ApplyOperation::Delete,
                identity,
                logical_id: "old".into(),
                expected_uid: Some("old-uid".into()),
                expected_resource_version: Some("old-rv".into()),
            }],
            next_command_lifecycle: NextCommandLifecycleV0::authorized("1".repeat(64)),
            recovery_claims: vec![PreparedRecoveryClaim {
                root_uid: "root-uid".into(),
                transaction_id: "d".repeat(64),
                command_id: "1".repeat(64),
            }],
            results: vec![],
            unresolved_identities: vec![],
        });
        machine.phase = PhaseV0::DeletionMutationPending;
        let failed = reduce(
            machine,
            EventV0::MutationFailed {
                attempt_token: "f".repeat(64),
                transaction_id: "d".repeat(64),
                reason: "conflict".into(),
            },
        )
        .unwrap();
        assert_eq!(failed.state.phase, PhaseV0::Hold);
        assert!(
            failed
                .commands
                .iter()
                .all(|command| !matches!(command, CommandV0::RemoveStateFinalizer { .. }))
        );
    }

    #[test]
    fn root_status_ack_is_exact_and_recovery_reissues_without_accepting_old_ack() {
        let started = reduce(state(), EventV0::Start).unwrap();
        let descriptor_token = match &started.commands[0] {
            CommandV0::RequestDescriptor { token, .. } => token.clone(),
            _ => panic!(),
        };
        let issued = reduce(
            started.state,
            EventV0::DescriptorReady {
                token: descriptor_token.clone(),
                bundle_digest: "a".repeat(64),
            },
        )
        .unwrap();
        let (token, old) = match &issued.commands[0] {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            } => (token.clone(), old_root_resource_version.clone()),
            _ => panic!(),
        };
        let stale = reduce(
            issued.state.clone(),
            EventV0::RootStatusPersisted {
                token: "0".repeat(64),
                old_root_resource_version: old.clone(),
                new_root_resource_version: "21".into(),
                root_uid: "root-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(stale.state.root.root_resource_version, "20");
        let json = serde_json::to_string(&issued.state).unwrap();
        let recovered = MachineStateV0::recover_json(&json).unwrap();
        let recovered_stale = reduce(
            recovered.clone(),
            EventV0::RootStatusPersisted {
                token: token.clone(),
                old_root_resource_version: old.clone(),
                new_root_resource_version: "21".into(),
                root_uid: "root-uid".into(),
            },
        );
        assert!(matches!(recovered_stale, Err(ReconcileError::Recovery(_))));
        let recovery_start = reduce(recovered, EventV0::Start).unwrap();
        assert!(matches!(
            recovery_start.commands.as_slice(),
            [CommandV0::RequestDescriptor { .. }]
        ));
        assert_eq!(
            descriptor_token.as_str(),
            match &recovery_start.commands[0] {
                CommandV0::RequestDescriptor { token, .. } => token.as_str(),
                _ => unreachable!(),
            }
        );
    }

    #[test]
    fn render_status_continuation_requires_exact_canonical_payload() {
        let machine = boot_to_render(state());
        let (mut frozen, input, output, observed) = rendered(&machine.root, vec![]);
        frozen.cache_epoch = "1".into();
        let issued = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let (token, old) = match &issued.commands[0] {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                ..
            } => (token.clone(), old_root_resource_version.clone()),
            _ => panic!("render must queue root status"),
        };
        let mutators: [fn(&mut PendingRootStatusV0); 3] = [
            |pending| pending.render_input = None,
            |pending| pending.render_input.as_mut().unwrap().observation_token = "other".into(),
            |pending| pending.render_input.as_mut().unwrap().cache_epoch = "01".into(),
        ];
        for mutate in mutators {
            let mut state = issued.state.clone();
            mutate(state.runtime.pending_root_status.as_mut().unwrap());
            let held = reduce(
                state,
                EventV0::RootStatusPersisted {
                    token: token.clone(),
                    old_root_resource_version: old.clone(),
                    new_root_resource_version: "22".into(),
                    root_uid: "root-uid".into(),
                },
            )
            .unwrap();
            assert_eq!(held.state.phase, PhaseV0::Hold);
            assert!(
                held.commands
                    .iter()
                    .all(|command| !matches!(command, CommandV0::RequestRender { .. }))
            );
        }
    }

    #[test]
    fn persist_ack_is_correlated_before_rv_mutation() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen: frozen.clone(),
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let operation = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let stale = reduce(
            prepared.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: "11".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(stale.state.root.state_resource_version, "11");
        assert!(matches!(
            stale.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
    }

    #[test]
    fn mutation_ack_cannot_add_unissued_commands_or_mutate_durable_inventory() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
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
            _ => panic!(),
        };
        let ready_to_revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let revalidated = reduce(
            ready_to_revalidate.state,
            EventV0::Revalidated {
                frozen: FrozenInputV0 {
                    state_resource_version: "12".into(),
                    ..frozen
                },
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let revalidated = acknowledge_dispatch_authorization(revalidated, "13");
        let (transaction_id, attempt_token) = match &revalidated.commands[0] {
            CommandV0::Mutate {
                transaction_id,
                attempt_token,
                ..
            } => (transaction_id.clone(), attempt_token.clone()),
            _ => panic!("expected mutation batch"),
        };
        let stale = reduce(
            revalidated.state.clone(),
            EventV0::MutationAcknowledged {
                attempt_token,
                transaction_id,
                results: vec![ApplyResult {
                    command_id: "0".repeat(64),
                    attempt_token: "0".repeat(64),
                    identity: ObjectIdentity {
                        api_version: "v1".into(),
                        kind: "ConfigMap".into(),
                        namespace: "demo".into(),
                        name: "object".into(),
                    },
                    outcome: ApplyOutcome::Applied,
                    uid: Some("new-uid".into()),
                    resource_version: Some("new-rv".into()),
                }],
            },
        )
        .unwrap();
        assert_eq!(stale.state.journal, revalidated.state.journal);
        assert!(matches!(
            stale.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
    }

    #[test]
    fn automation_ensure_and_completion_require_current_exact_pins() {
        let mut machine = state();
        machine.phase = PhaseV0::Ready;
        let source = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "source".into(),
        };
        let mut trigger = ActionTriggerV0 {
            automation_identity: "smoke".into(),
            automation_version: "v1".into(),
            action_module_identity: "actions/smoke.mjs".into(),
            action_module_digest: "b".repeat(64),
            source: source.clone(),
            source_uid: "source-uid".into(),
            source_resource_version: "7".into(),
            observation_token: String::new(),
            event_metadata: serde_json::json!({"reason":"changed"}),
        };
        trigger.observation_token = automation_source_transition_token(&trigger);
        let mut prior = trigger.clone();
        prior.source_resource_version = "6".into();
        let prior_token = automation_source_transition_token(&prior);
        machine
            .journal
            .state
            .automation
            .baselines
            .push(AutomationBaseline {
                automation_id: "smoke".into(),
                source: source.clone(),
                token: prior_token,
            });
        let mut unrelated_global = trigger.clone();
        unrelated_global.observation_token = "global-barrier-changed".into();
        assert!(matches!(
            reduce(
                machine.clone(),
                EventV0::TriggerAutomation {
                    trigger: unrelated_global
                }
            ),
            Err(ReconcileError::Automation(_))
        ));
        let issued = reduce(machine, EventV0::TriggerAutomation { trigger }).unwrap();
        let (token, name, spec) = match &issued.commands[0] {
            CommandV0::EnsureAutomationRun {
                token, name, spec, ..
            } => (token.clone(), name.clone(), spec.clone()),
            _ => panic!("expected automation ensure"),
        };
        let stale = reduce(
            issued.state.clone(),
            EventV0::AutomationRunEnsured {
                token: "0".repeat(64),
                name: name.clone(),
                spec: spec.clone(),
                uid: "run-uid".into(),
                resource_version: "8".into(),
            },
        )
        .unwrap();
        assert_eq!(
            stale.state.runtime.pending_automation,
            issued.state.runtime.pending_automation
        );
        assert!(matches!(
            stale.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
        let mut completion_state = state();
        completion_state.phase = PhaseV0::Ready;
        completion_state
            .journal
            .state
            .automation
            .pending
            .push(AutomationPending {
                occurrence_id: spec.occurrence_id.clone(),
                automation_id: spec.automation_id.clone(),
                source: spec.source.clone(),
                run_name: name.clone(),
                run_uid: "run-uid".into(),
                run_resource_version: "8".into(),
                spec_hash: automation_run_spec_hash(&spec).unwrap(),
            });
        let mut historic_program_mismatch = spec.clone();
        historic_program_mismatch.program.digest = "c".repeat(64);
        // A foreign UID/RV must be ignored before its malformed full spec can
        // influence the current exact pending occurrence.
        let stale_completion = reduce(
            completion_state.clone(),
            EventV0::AutomationCompleted {
                spec: historic_program_mismatch.clone(),
                occurrence_id: spec.occurrence_id.clone(),
                name: name.clone(),
                uid: "other-uid".into(),
                old_resource_version: "stale-rv".into(),
                new_resource_version: "9".into(),
                completion: AutomationRunCompletionState::Succeeded,
                attempts: 1,
                materialization_name: "event".into(),
                materialization_uid: "event-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(
            stale_completion.state.journal.state.automation.pending,
            completion_state.journal.state.automation.pending
        );
        assert!(matches!(
            stale_completion.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
        let held = reduce(
            completion_state.clone(),
            EventV0::AutomationCompleted {
                spec: historic_program_mismatch,
                occurrence_id: spec.occurrence_id.clone(),
                name: name.clone(),
                uid: "run-uid".into(),
                old_resource_version: "8".into(),
                new_resource_version: "9".into(),
                completion: AutomationRunCompletionState::Succeeded,
                attempts: 1,
                materialization_name: "event".into(),
                materialization_uid: "event-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(held.state.phase, PhaseV0::Hold);
        assert!(matches!(held.commands.as_slice(), [CommandV0::Hold { .. }]));

        let mut historic_hash_mismatch = spec.clone();
        // This is a valid exact run identity with every field duplicated in
        // AutomationPending intact. Only the program source authority changes,
        // so the canonical full-spec hash must be what rejects it.
        historic_hash_mismatch.program.config_map_name = "replacement-bundle".into();
        let held = reduce(
            completion_state.clone(),
            EventV0::AutomationCompleted {
                spec: historic_hash_mismatch,
                occurrence_id: spec.occurrence_id.clone(),
                name: name.clone(),
                uid: "run-uid".into(),
                old_resource_version: "8".into(),
                new_resource_version: "9".into(),
                completion: AutomationRunCompletionState::Succeeded,
                attempts: 1,
                materialization_name: "event".into(),
                materialization_uid: "event-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(held.state.phase, PhaseV0::Hold);
        assert!(matches!(held.commands.as_slice(), [CommandV0::Hold { .. }]));

        let completed = reduce(
            completion_state,
            EventV0::AutomationCompleted {
                spec,
                occurrence_id: stale_completion.state.journal.state.automation.pending[0]
                    .occurrence_id
                    .clone(),
                name,
                uid: "run-uid".into(),
                old_resource_version: "8".into(),
                new_resource_version: "9".into(),
                completion: AutomationRunCompletionState::Failed,
                attempts: 1,
                materialization_name: "event".into(),
                materialization_uid: "event-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(
            completed.state.journal.state.automation.completions[0].outcome,
            AutomationCompletionOutcome::Failed
        );
        assert_eq!(
            completed.state.journal.state.automation.completions[0].materialization_uid,
            "event-uid"
        );
        assert_eq!(token.len(), 64);
    }

    #[test]
    fn prepared_not_issued_restart_requires_revalidation_and_cannot_replay_effects() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let recovered =
            MachineStateV0::recover_json(&serde_json::to_string(&prepared.state).unwrap()).unwrap();
        let started = reduce(recovered, EventV0::Start).unwrap();
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
    }

    #[test]
    fn full_dependency_snapshot_including_empty_selector_is_durable_effect_input() {
        let machine = boot_to_render(state());
        let dependency = Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            selector: "".into(),
            collection_revision: "rv-1".into(),
            observation_token: "dep-1".into(),
        };
        let (frozen, input, output, observed) = rendered(&machine.root, vec![dependency]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        assert_eq!(
            prepared.state.journal.state.pending_dependencies[0]
                .selector
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn single_post_revalidates_to_completion_and_ready() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let ready = complete_single_apply_to_ready(
            machine,
            frozen,
            input,
            output,
            observed,
            "created-uid",
            "30",
        );
        assert_eq!(ready.phase, PhaseV0::Ready);
        assert_eq!(ready.journal.state.inventory.len(), 1);
        assert_eq!(ready.journal.state.inventory[0].uid, "created-uid");
    }

    #[test]
    fn ssa_revalidates_to_completion_and_ready() {
        let mut machine = boot_to_render(state());
        let (frozen, input, output, mut observed) = rendered(&machine.root, vec![]);
        let identity = observed[0].identity.clone();
        machine.journal.state.inventory.push(InventoryEntry {
            identity: identity.clone(),
            logical_id: "object".into(),
            uid: "existing-uid".into(),
            resource_version: "10".into(),
            bundle_digest: "a".repeat(64),
        });
        observed[0].observation = Observation::Existing {
            identity: MutationIdentity {
                uid: "existing-uid".into(),
                resource_version: "10".into(),
            },
        };
        let ready = complete_single_apply_to_ready(
            machine,
            frozen,
            input,
            output,
            observed,
            "existing-uid",
            "11",
        );
        assert_eq!(ready.phase, PhaseV0::Ready);
        assert_eq!(ready.journal.state.inventory[0].resource_version, "11");
    }

    #[test]
    fn unchanged_dependency_is_valid_in_committed_and_pending_second_prepare() {
        let machine = boot_to_render(state());
        let dependency = Dependency::Collection {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            selector: "app=demo".into(),
            collection_revision: "rv-1".into(),
            observation_token: "dep-1".into(),
        };
        let (frozen, input, output, observed) = rendered(&machine.root, vec![dependency]);
        let first = reduce(
            machine,
            EventV0::Rendered {
                frozen: frozen.clone(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let mut second_machine = first.state;
        // Model the completed first reconciliation's committed dependency
        // snapshot, then prepare the unchanged render again.
        second_machine.journal.state.committed_dependencies =
            second_machine.journal.state.pending_dependencies.clone();
        second_machine.journal.state.pending_persist = None;
        second_machine.phase = PhaseV0::RenderPending;
        let second = reduce(
            second_machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        assert_eq!(
            second.state.journal.state.committed_dependencies,
            second.state.journal.state.pending_dependencies
        );
        assert!(matches!(
            second.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
    }

    #[test]
    fn prune_requires_distinct_completed_final_observation_fences() {
        let mut machine = boot_to_render(state());
        let identity = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "old".into(),
        };
        let inventory = InventoryEntry {
            identity: identity.clone(),
            logical_id: "old".into(),
            uid: "old-uid".into(),
            resource_version: "10".into(),
            bundle_digest: "a".repeat(64),
        };
        machine.journal.state.inventory.push(inventory.clone());
        let (frozen, input, output, _) = rendered(&machine.root, vec![]);
        let mut output: RenderOutputV0 = serde_json::from_str(&output).unwrap();
        output.resources.clear();
        output.disposition = RenderDisposition::ExplicitEmpty;
        let output = serde_json::to_string(&output).unwrap();
        let observed = vec![DesiredObservationV0 {
            identity,
            observation: Observation::Existing {
                identity: MutationIdentity {
                    uid: "old-uid".into(),
                    resource_version: "10".into(),
                },
            },
            qualification: BackendQualification::BuiltInTested {
                kubernetes_minor: "1.36".into(),
                gvr: "configmaps".into(),
            },
            recovery_proof: None,
            deletion_recovery_proof: None,
        }];

        let first = plan_graph(&machine, &frozen, &input, &output, observed.clone()).unwrap();
        assert!(first.intents.is_empty());
        let first_candidate = first.candidate.unwrap();
        assert_eq!(
            first_candidate.first_observation.barrier_token,
            frozen.final_observation_fence_token
        );
        machine.journal.state.prune_candidate = Some(first_candidate.clone());

        // Repeating the same completed final fence, even with the same root,
        // epoch, and renderer token, cannot authorize a delete.
        let same_fence = plan_graph(&machine, &frozen, &input, &output, observed.clone()).unwrap();
        assert!(same_fence.intents.is_empty());
        assert_eq!(
            same_fence
                .candidate
                .as_ref()
                .unwrap()
                .first_observation
                .barrier_token,
            frozen.final_observation_fence_token
        );
        assert!(
            same_fence
                .candidate
                .as_ref()
                .unwrap()
                .second_observation
                .is_none()
        );

        let second_frozen = FrozenInputV0 {
            final_observation_fence_token: "e".repeat(64),
            ..frozen
        };
        let confirmed = plan_graph(&machine, &second_frozen, &input, &output, observed).unwrap();
        assert!(
            confirmed
                .intents
                .iter()
                .any(|intent| intent.operation == ApplyOperation::Delete)
        );
        let candidate = confirmed.candidate.unwrap();
        assert_eq!(
            candidate.second_observation.as_ref().unwrap().barrier_token,
            second_frozen.final_observation_fence_token
        );
        assert_ne!(
            candidate.first_observation.barrier_token,
            candidate.second_observation.unwrap().barrier_token
        );
    }

    #[test]
    fn stale_prune_generation_is_replaced_then_next_generation_match_prepares_delete() {
        let mut machine = boot_to_render(state());
        let old = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "old".into(),
        };
        let inventory = InventoryEntry {
            identity: old.clone(),
            logical_id: "old".into(),
            uid: "old-uid".into(),
            resource_version: "old-rv".into(),
            bundle_digest: "a".repeat(64),
        };
        machine.journal.state.inventory.push(inventory.clone());
        machine.journal.state.prune_candidate = Some(PruneCandidate {
            complete_key: prune_key(&machine, &inventory),
            inventory: inventory.clone(),
            first_observation: PruneObservation {
                observed_resource_version: "old-rv".into(),
                observed_generation: 6,
                barrier_token: "old-barrier".into(),
            },
            second_observation: None,
        });
        let (frozen, input, output, _) = rendered(&machine.root, vec![]);
        let mut output: RenderOutputV0 = serde_json::from_str(&output).unwrap();
        output.resources.clear();
        output.disposition = RenderDisposition::ExplicitEmpty;
        let output = serde_json::to_string(&output).unwrap();
        let observed = vec![DesiredObservationV0 {
            identity: old.clone(),
            observation: Observation::Existing {
                identity: MutationIdentity {
                    uid: "old-uid".into(),
                    resource_version: "old-rv".into(),
                },
            },
            qualification: BackendQualification::BuiltInTested {
                kubernetes_minor: "1.36".into(),
                gvr: "configmaps".into(),
            },
            recovery_proof: None,
            deletion_recovery_proof: None,
        }];
        let first = reduce(
            machine,
            EventV0::Rendered {
                frozen: frozen.clone(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        assert!(matches!(
            first.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
        let candidate = first.state.journal.state.prune_candidate.as_ref().unwrap();
        assert_eq!(candidate.first_observation.observed_generation, 7);
        assert!(candidate.second_observation.is_none());
        let prepare = match &first.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let revalidate = reduce(
            first.state,
            EventV0::Persisted {
                token: prepare.token,
                old_state_resource_version: prepare.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let completion = reduce(
            revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let completion_persist = match &completion.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let mut next_render = reduce(
            completion.state,
            EventV0::Persisted {
                token: completion_persist.token,
                old_state_resource_version: completion_persist.old_state_resource_version,
                new_state_resource_version: "13".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap()
        .state;
        assert_eq!(next_render.phase, PhaseV0::Ready);
        // A fresh external reconcile schedules the next render boundary.
        next_render.phase = PhaseV0::RenderPending;
        next_render.root.status.input_token = Some("observation-2".into());
        let mut second_input_document: RenderInputV0 = serde_json::from_str(&input).unwrap();
        second_input_document.observation_token = "observation-2".into();
        let second_input = serde_json::to_string(&second_input_document).unwrap();
        let second = reduce(
            next_render,
            EventV0::Rendered {
                frozen: FrozenInputV0 {
                    state_resource_version: "13".into(),
                    observation_token: "observation-2".into(),
                    final_observation_fence_token: "e".repeat(64),
                    render_input_hash: parse_validate_render_input(&second_input).unwrap().sha256,
                    ..frozen
                },
                render_input_json: second_input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        assert!(matches!(
            second.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
        assert!(
            second
                .state
                .journal
                .state
                .apply_transaction
                .as_ref()
                .unwrap()
                .prepared_intents
                .iter()
                .any(|intent| intent.operation == ApplyOperation::Delete)
        );
    }

    #[test]
    fn prune_delete_revalidates_to_completion_and_ready() {
        let mut machine = boot_to_render(state());
        let identity = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "old".into(),
        };
        let inventory = InventoryEntry {
            identity: identity.clone(),
            logical_id: "old".into(),
            uid: "old-uid".into(),
            resource_version: "10".into(),
            bundle_digest: "a".repeat(64),
        };
        machine.journal.state.inventory.push(inventory.clone());
        machine.journal.state.prune_candidate = Some(PruneCandidate {
            complete_key: prune_key(&machine, &inventory),
            inventory,
            first_observation: PruneObservation {
                observed_resource_version: "10".into(),
                observed_generation: 7,
                barrier_token: "first-barrier".into(),
            },
            second_observation: None,
        });
        let (frozen, input, output, _) = rendered(&machine.root, vec![]);
        let mut output: RenderOutputV0 = serde_json::from_str(&output).unwrap();
        output.resources.clear();
        output.disposition = RenderDisposition::ExplicitEmpty;
        let output = serde_json::to_string(&output).unwrap();
        let observed = vec![DesiredObservationV0 {
            identity: identity.clone(),
            observation: Observation::Existing {
                identity: MutationIdentity {
                    uid: "old-uid".into(),
                    resource_version: "10".into(),
                },
            },
            qualification: BackendQualification::BuiltInTested {
                kubernetes_minor: "1.36".into(),
                gvr: "configmaps".into(),
            },
            recovery_proof: None,
            deletion_recovery_proof: None,
        }];
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let prepare = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare.token,
                old_state_resource_version: prepare.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let recreated =
            MachineStateV0::recover_json(&serde_json::to_string(&revalidate.state).unwrap())
                .unwrap();
        let restarted = reduce(recreated, EventV0::Start).unwrap();
        let descriptor_token = match &restarted.commands[0] {
            CommandV0::RequestDescriptor { token, .. } => token.clone(),
            command => panic!("recovery skipped descriptor verification: {command:?}"),
        };
        let descriptor = reduce(
            restarted.state,
            EventV0::DescriptorReady {
                token: descriptor_token,
                bundle_digest: "a".repeat(64),
            },
        )
        .unwrap();
        let informed_state = match descriptor.commands.as_slice() {
            [] => descriptor.state,
            [
                CommandV0::PersistRootStatus {
                    token,
                    old_root_resource_version,
                    ..
                },
            ] => {
                reduce(
                    descriptor.state,
                    EventV0::RootStatusPersisted {
                        token: token.clone(),
                        old_root_resource_version: old_root_resource_version.clone(),
                        new_root_resource_version: "recovery-root-rv".into(),
                        root_uid: "root-uid".into(),
                    },
                )
                .unwrap()
                .state
            }
            commands => panic!("unexpected recovery descriptor commands: {commands:?}"),
        };
        let recovery_base = reduce(informed_state, EventV0::InformersReady)
            .unwrap()
            .state;
        assert_eq!(recovery_base.phase, PhaseV0::RevalidatePending);
        assert!(recovery_base.frozen.is_none());
        let retried = reduce(
            recovery_base.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        assert!(matches!(
            retried.commands.as_slice(),
            [CommandV0::Persist {
                operation: PersistOperation {
                    purpose: PersistPurpose::Invalidate,
                    ..
                },
                ..
            }]
        ));
        for (uid, resource_version) in [
            ("replacement-uid", "10"),
            ("old-uid", "changed-resource-version"),
        ] {
            let mut mismatched = observed.clone();
            mismatched[0].observation = Observation::Existing {
                identity: MutationIdentity {
                    uid: uid.into(),
                    resource_version: resource_version.into(),
                },
            };
            let held = reduce(
                recovery_base.clone(),
                EventV0::Revalidated {
                    frozen: revalidate.state.frozen.clone().unwrap(),
                    render_input_json: input.clone(),
                    render_output_json: output.clone(),
                    observed: mismatched,
                },
            )
            .unwrap();
            assert!(matches!(
                held.commands.as_slice(),
                [CommandV0::Persist {
                    operation: PersistOperation {
                        purpose: PersistPurpose::Invalidate,
                        ..
                    },
                    ..
                }]
            ));
        }
        let desired_present_output = serde_json::to_string(&RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::Apply,
            resources: vec![ResourceDeclaration {
                logical_id: "old".into(),
                object: serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"namespace": "demo", "name": "old"},
                    "data": {"desired": "again"}
                }),
                readiness: None,
            }],
            dependencies: vec![],
            automations: vec![],
            diagnostics: vec![],
        })
        .unwrap();
        let invalidated = reduce(
            recovery_base,
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: desired_present_output,
                observed: observed.clone(),
            },
        )
        .unwrap();
        assert!(matches!(
            invalidated.commands.as_slice(),
            [CommandV0::Persist {
                operation: PersistOperation {
                    purpose: PersistPurpose::Invalidate,
                    ..
                },
                ..
            }]
        ));
        assert!(
            invalidated
                .commands
                .iter()
                .all(|command| !matches!(command, CommandV0::Mutate { .. }))
        );
        let issued = reduce(
            revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let issued = acknowledge_dispatch_authorization(issued, "13");
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
            _ => panic!("prepared prune must issue an exact delete"),
        };
        let acknowledged = reduce(
            issued.state,
            EventV0::MutationAcknowledged {
                transaction_id,
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: command.command_id,
                    attempt_token,
                    identity: identity.clone(),
                    outcome: ApplyOutcome::Deleted,
                    uid: Some("old-uid".into()),
                    resource_version: Some("10".into()),
                }],
            },
        )
        .unwrap();
        let result = match &acknowledged.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let final_revalidate = reduce(
            acknowledged.state,
            EventV0::Persisted {
                token: result.token,
                old_state_resource_version: result.old_state_resource_version,
                new_state_resource_version: "14".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let completion = reduce(
            final_revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: final_revalidate.state.frozen.clone().unwrap(),
                render_input_json: input,
                render_output_json: output,
                observed: vec![DesiredObservationV0 {
                    identity,
                    observation: Observation::Absent,
                    qualification: BackendQualification::BuiltInTested {
                        kubernetes_minor: "1.36".into(),
                        gvr: "configmaps".into(),
                    },
                    recovery_proof: None,
                    deletion_recovery_proof: None,
                }],
            },
        )
        .unwrap();
        let completion_persist = match &completion.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let ready = reduce(
            completion.state,
            EventV0::Persisted {
                token: completion_persist.token,
                old_state_resource_version: completion_persist.old_state_resource_version,
                new_state_resource_version: "15".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(ready.state.phase, PhaseV0::Ready);
        assert!(ready.state.journal.state.inventory.is_empty());
    }

    #[test]
    fn bootstrap_requires_exact_linked_ack_and_descriptor_verification() {
        let mut root = state().root;
        root.state_name = "forged".into();
        root.state_uid = "forged-uid".into();
        root.state_resource_version = "forged-rv".into();
        root.root_finalizer_present = true;
        root.state_finalizer_present = true;
        root.status.verified_bundle_digest = Some("a".repeat(64));
        root.status.conditions = vec![
            RootCondition {
                type_: RootConditionType::Ready,
                status: ConditionStatus::True,
                reason: "spoofed".into(),
                message: "must not survive bootstrap".into(),
                observed_generation: Some(7),
            },
            RootCondition {
                type_: RootConditionType::Hold,
                status: ConditionStatus::True,
                reason: "spoofed".into(),
                message: "must not survive bootstrap".into(),
                observed_generation: Some(7),
            },
        ];
        let machine = MachineStateV0::bootstrap(root).unwrap();
        assert!(machine.root.status.verified_bundle_digest.is_none());
        assert_eq!(machine.root.status, WebApplicationStatus::default());
        let issued = reduce(machine, EventV0::Start).unwrap();
        let (token, expected_name) = match &issued.commands[0] {
            CommandV0::EnsureBootstrap {
                token, state_name, ..
            } => (token.clone(), state_name.clone()),
            _ => panic!("expected bootstrap"),
        };
        let stale = reduce(
            issued.state.clone(),
            EventV0::BootstrapAcknowledged {
                token: token.clone(),
                state_name: expected_name.clone(),
                state_uid: "state-uid".into(),
                state_resource_version: "11".into(),
                old_root_resource_version: "20".into(),
                new_root_resource_version: "21".into(),
                status_state_link: StateLink {
                    root_uid: "root-uid".into(),
                    name: expected_name.clone(),
                    uid: "state-uid".into(),
                },
                root_finalizer_present: true,
                state_finalizer_present: false,
            },
        )
        .unwrap();
        assert!(matches!(
            stale.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
        assert_eq!(stale.state.phase, PhaseV0::BootstrapPending);
        let acked = reduce(
            issued.state,
            EventV0::BootstrapAcknowledged {
                token,
                state_name: expected_name.clone(),
                state_uid: "state-uid".into(),
                state_resource_version: "11".into(),
                old_root_resource_version: "20".into(),
                new_root_resource_version: "21".into(),
                status_state_link: StateLink {
                    root_uid: "root-uid".into(),
                    name: expected_name,
                    uid: "state-uid".into(),
                },
                root_finalizer_present: true,
                state_finalizer_present: true,
            },
        )
        .unwrap();
        let descriptor_token = match &acked.commands[0] {
            CommandV0::RequestDescriptor { token, .. } => token.clone(),
            _ => panic!("descriptor must follow bootstrap acknowledgement"),
        };
        let wrong_descriptor = reduce(
            acked.state.clone(),
            EventV0::DescriptorReady {
                token: descriptor_token.clone(),
                bundle_digest: "b".repeat(64),
            },
        )
        .unwrap();
        assert!(
            wrong_descriptor
                .state
                .root
                .status
                .verified_bundle_digest
                .is_none()
        );
        let status = reduce(
            acked.state,
            EventV0::DescriptorReady {
                token: descriptor_token,
                bundle_digest: "a".repeat(64),
            },
        )
        .unwrap();
        let (status_token, old_rv) = match &status.commands[0] {
            CommandV0::PersistRootStatus {
                token,
                old_root_resource_version,
                status,
                ..
            } => {
                assert_eq!(status.verified_bundle_digest, Some("a".repeat(64)));
                (token.clone(), old_root_resource_version.clone())
            }
            _ => panic!("exact descriptor must be persisted before verified digest"),
        };
        let verified = reduce(
            status.state,
            EventV0::RootStatusPersisted {
                token: status_token,
                old_root_resource_version: old_rv,
                new_root_resource_version: "22".into(),
                root_uid: "root-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(
            verified.state.root.status.verified_bundle_digest,
            Some("a".repeat(64))
        );
    }

    #[test]
    fn serde_recovery_never_restores_a_forged_effect_phase() {
        let mut forged = state();
        forged.phase = PhaseV0::MutationPending;
        forged.frozen = Some(FrozenInputV0 {
            root_generation: 7,
            state_resource_version: "11".into(),
            observation_token: "forged".into(),
            cache_epoch: "forged".into(),
            final_observation_fence_token: "f".repeat(64),
            render_input_hash: "a".repeat(64),
        });
        let recovered: MachineStateV0 =
            serde_json::from_str(&serde_json::to_string(&forged).unwrap()).unwrap();
        assert_eq!(recovered.phase, PhaseV0::Recovery);
        assert!(recovered.frozen.is_none());
        assert!(matches!(
            reduce(
                recovered.clone(),
                EventV0::MutationAcknowledged {
                    attempt_token: "a".repeat(64),
                    transaction_id: "b".repeat(64),
                    results: vec![],
                },
            ),
            Err(ReconcileError::Recovery(_))
        ));
        assert!(matches!(
            reduce(recovered.clone(), EventV0::BeginDeletion),
            Err(ReconcileError::Recovery(_))
        ));
        let started = reduce(recovered, EventV0::Start).unwrap();
        assert!(matches!(
            started.commands.as_slice(),
            [CommandV0::RequestDescriptor { .. }]
        ));
    }

    #[test]
    fn recovery_accepts_immutable_deletion_start_generation_but_rejects_negative() {
        let mut advanced = state();
        advanced.journal.state.deletion = Some(DeletionTransaction {
            id: "d".repeat(64),
            deletion_start_generation: 6,
        });
        let recovered: MachineStateV0 =
            serde_json::from_str(&serde_json::to_string(&advanced).unwrap()).unwrap();
        assert!(reduce(recovered, EventV0::Start).is_ok());

        let mut forged = state();
        forged.journal.state.deletion = Some(DeletionTransaction {
            id: "d".repeat(64),
            deletion_start_generation: -1,
        });
        let recovered: MachineStateV0 =
            serde_json::from_str(&serde_json::to_string(&forged).unwrap()).unwrap();
        assert!(matches!(
            reduce(recovered, EventV0::Start),
            Err(ReconcileError::Link(_))
        ));
    }

    #[test]
    fn deletion_cleans_completed_runs_by_exact_uid_and_resource_version() {
        let mut machine = state();
        machine.journal.state.deletion = Some(DeletionTransaction {
            id: "d".repeat(64),
            deletion_start_generation: 7,
        });
        machine
            .journal
            .state
            .automation
            .completions
            .push(AutomationCompletion {
                occurrence_id: "occurrence".into(),
                outcome: AutomationCompletionOutcome::Succeeded,
                run_name: "run".into(),
                run_uid: "run-uid".into(),
                old_resource_version: "8".into(),
                completed_resource_version: "9".into(),
                materialization_name: "event".into(),
                materialization_uid: "event-uid".into(),
            });
        let mut commands = Vec::new();
        deletion_advance(&mut machine, &mut commands).unwrap();
        let token = match &commands[0] {
            CommandV0::DeleteAutomationRun {
                token,
                name,
                uid,
                resource_version,
                ..
            } => {
                assert_eq!(
                    (name.as_str(), uid.as_str(), resource_version.as_str()),
                    ("run", "run-uid", "9")
                );
                token.clone()
            }
            _ => panic!("completed root-owned run must be included in deletion"),
        };
        let mut persist = Vec::new();
        automation_deleted(
            &mut machine,
            &token,
            "occurrence",
            "run",
            "run-uid",
            "9",
            &mut persist,
        )
        .unwrap();
        assert!(machine.journal.state.automation.completions.is_empty());
        assert!(matches!(persist.as_slice(), [CommandV0::Persist { .. }]));
    }

    #[test]
    fn may_have_issued_post_with_copied_proof_holds_without_adoption() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen: frozen.clone(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let prepare_operation = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare_operation.token,
                old_state_resource_version: prepare_operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let fresh_frozen = revalidate.state.frozen.clone().unwrap();
        let issued = reduce(
            revalidate.state,
            EventV0::Revalidated {
                frozen: fresh_frozen.clone(),
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let issued = acknowledge_dispatch_authorization(issued, "13");
        let (transaction_id, command_id) = match &issued.commands[0] {
            CommandV0::Mutate {
                transaction_id,
                commands,
                ..
            } => {
                assert_eq!(commands.len(), 1);
                (transaction_id.clone(), commands[0].command_id.clone())
            }
            _ => panic!("one prepared mutation must be issued"),
        };
        let recovered =
            MachineStateV0::recover_json(&serde_json::to_string(&issued.state).unwrap()).unwrap();
        let descriptor = reduce(recovered, EventV0::Start).unwrap();
        let descriptor_token = match &descriptor.commands[0] {
            CommandV0::RequestDescriptor { token, .. } => token.clone(),
            _ => panic!(),
        };
        let informers = reduce(
            descriptor.state,
            EventV0::DescriptorReady {
                token: descriptor_token,
                bundle_digest: "a".repeat(64),
            },
        )
        .unwrap();
        let render = reduce(informers.state, EventV0::InformersReady).unwrap();
        assert_eq!(render.state.phase, PhaseV0::RevalidatePending);
        assert_eq!(render.state.root.state_resource_version, "13");
        assert_eq!(fresh_frozen.root_generation, render.state.root.generation);
        let mut proof_observed = observed;
        proof_observed[0].observation = Observation::Existing {
            identity: MutationIdentity {
                uid: "created-uid".into(),
                resource_version: "30".into(),
            },
        };
        proof_observed[0].recovery_proof = Some(RecoveryProofV0 {
            root_uid: "root-uid".into(),
            transaction_id,
            command_id,
            uid: "created-uid".into(),
            resource_version: "30".into(),
        });
        let recovery_state_resource_version = render.state.root.state_resource_version.clone();
        let recovered_result = reduce(
            render.state,
            EventV0::Revalidated {
                frozen: FrozenInputV0 {
                    state_resource_version: recovery_state_resource_version,
                    ..fresh_frozen
                },
                render_input_json: input,
                render_output_json: output,
                observed: proof_observed,
            },
        )
        .unwrap();
        assert!(
            matches!(
                recovered_result.commands.as_slice(),
                [CommandV0::Hold { .. }]
            ),
            "{:?}",
            recovered_result.commands
        );
        assert!(recovered_result.state.journal.state.inventory.is_empty());
    }

    #[test]
    fn mutation_requires_an_exact_dispatch_authorization_acknowledgement() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let prepare = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            command => panic!("expected prepare persist, got {command:?}"),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare.token,
                old_state_resource_version: prepare.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let authorized = reduce(
            revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let operation = match &authorized.commands[0] {
            CommandV0::Persist { operation, .. }
                if operation.purpose == PersistPurpose::DispatchAuthorization =>
            {
                operation.clone()
            }
            command => panic!("expected dispatch authorization persist, got {command:?}"),
        };
        assert_eq!(
            authorized.state.phase,
            PhaseV0::DispatchAuthorizationPersistPending
        );
        assert!(matches!(
            authorized.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));

        let stale = reduce(
            authorized.state.clone(),
            EventV0::Persisted {
                token: "0".repeat(64),
                old_state_resource_version: operation.old_state_resource_version.clone(),
                new_state_resource_version: "13".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        assert!(
            stale
                .commands
                .iter()
                .all(|command| !matches!(command, CommandV0::Mutate { .. }))
        );

        let issued = acknowledge_dispatch_authorization(authorized, "13");
        assert!(
            matches!(issued.commands.as_slice(), [CommandV0::Mutate { commands, .. }] if commands.len() == 1)
        );
    }

    #[test]
    fn missing_lifecycle_holds_without_issuing_a_recovered_mutation() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let mut wire = serde_json::to_value(&prepared.state).unwrap();
        wire.pointer_mut("/journal/state/applyTransaction/nextCommandLifecycle")
            .map(|value| *value = Value::Null)
            .expect("prepared state has a lifecycle");
        wire.pointer_mut("/journal/state/applyTransaction")
            .and_then(Value::as_object_mut)
            .unwrap()
            .remove("nextCommandLifecycle");
        let recovered =
            MachineStateV0::recover_json(&serde_json::to_string(&wire).unwrap()).unwrap();
        let held = reduce(recovered, EventV0::Start).unwrap();
        assert!(matches!(held.commands.as_slice(), [CommandV0::Hold { .. }]));
    }

    #[test]
    fn deletion_persists_and_preserves_a_may_have_issued_transaction() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let prepare = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            command => panic!("expected prepare persist, got {command:?}"),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare.token,
                old_state_resource_version: prepare.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let authorized = reduce(
            revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let command_id = authorized
            .state
            .journal
            .state
            .apply_transaction
            .as_ref()
            .unwrap()
            .next_command_lifecycle
            .command_id
            .clone();
        let deleting = reduce(authorized.state, EventV0::BeginDeletion).unwrap();
        assert!(matches!(
            deleting.commands.as_slice(),
            [CommandV0::Persist {
                operation: PersistOperation {
                    purpose: PersistPurpose::DeletionStart,
                    ..
                },
                ..
            }]
        ));
        assert_eq!(
            deleting
                .state
                .journal
                .state
                .apply_transaction
                .as_ref()
                .unwrap()
                .next_command_lifecycle
                .command_id,
            command_id
        );
        let operation = match &deleting.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => unreachable!(),
        };
        let held = reduce(
            deleting.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: "13".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        assert!(matches!(held.commands.as_slice(), [CommandV0::Hold { .. }]));
    }

    #[test]
    fn recovery_revalidates_all_fresh_inputs_before_any_response_lost_mutation() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let operation = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let pending = reduce(
            prepared.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap()
        .state;
        let fresh = pending.frozen.clone().unwrap();
        let before = pending.clone();
        assert!(matches!(
            reduce(
                pending.clone(),
                EventV0::Revalidated {
                    frozen: fresh.clone(),
                    render_input_json: input.clone(),
                    render_output_json: "{invalid".into(),
                    observed: observed.clone(),
                }
            ),
            Err(ReconcileError::Renderer(_))
        ));
        assert_eq!(pending, before);

        let mut unsupported = observed;
        unsupported[0].qualification = BackendQualification::Unsupported {
            reason: "hostile adapter proof".into(),
        };
        assert!(matches!(
            reduce(
                pending.clone(),
                EventV0::Revalidated {
                    frozen: fresh,
                    render_input_json: input,
                    render_output_json: output,
                    observed: unsupported,
                }
            ),
            Err(ReconcileError::Mutation(_))
        ));
        assert_eq!(pending, before);
    }

    #[test]
    fn delete_response_loss_requires_an_exact_durable_absence_proof() {
        let machine = state();
        let identity = ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "deleted".into(),
        };
        let command_id = "c".repeat(64);
        let transaction_id = "d".repeat(64);
        let tx = ApplyTransaction {
            id: transaction_id.clone(),
            generation: 7,
            frozen_render_input_hash: "a".repeat(64),
            desired_graph_hash: "b".repeat(64),
            recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
            attempt_token: "e".repeat(64),
            prepared_intents: vec![ApplyIntent {
                command_id: command_id.clone(),
                operation: ApplyOperation::Delete,
                identity: identity.clone(),
                logical_id: "deleted".into(),
                expected_uid: Some("deleted-uid".into()),
                expected_resource_version: Some("10".into()),
            }],
            next_command_lifecycle: NextCommandLifecycleV0::authorized(command_id.clone()),
            recovery_claims: vec![PreparedRecoveryClaim {
                root_uid: "root-uid".into(),
                transaction_id: transaction_id.clone(),
                command_id: command_id.clone(),
            }],
            results: vec![],
            unresolved_identities: vec![],
        };
        let mut observation = DesiredObservationV0 {
            identity,
            observation: Observation::Absent,
            qualification: BackendQualification::BuiltInTested {
                kubernetes_minor: "1.36".into(),
                gvr: "configmaps".into(),
            },
            recovery_proof: None,
            deletion_recovery_proof: None,
        };
        assert!(
            recover_prepared_result(&machine, &tx, &[observation.clone()])
                .unwrap()
                .is_empty()
        );
        observation.deletion_recovery_proof = Some(DeletionRecoveryProofV0 {
            root_uid: "root-uid".into(),
            transaction_id,
            command_id,
            expected_uid: "deleted-uid".into(),
            expected_resource_version: "10".into(),
        });
        assert_eq!(
            recover_prepared_result(&machine, &tx, &[observation.clone()]).unwrap()[0].outcome,
            ApplyOutcome::AlreadyAbsent
        );
        let mut no_prepared_intent = tx;
        no_prepared_intent.prepared_intents.clear();
        no_prepared_intent.recovery_claims.clear();
        assert!(
            recover_prepared_result(&machine, &no_prepared_intent, &[observation])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ssa_recovery_requires_same_uid_and_distinct_returned_resource_version() {
        let mut machine = boot_to_render(state());
        let (frozen, input, output, mut observed) = rendered(&machine.root, vec![]);
        let identity = observed[0].identity.clone();
        machine.journal.state.inventory.push(InventoryEntry {
            identity: identity.clone(),
            logical_id: "object".into(),
            uid: "original-uid".into(),
            resource_version: "10".into(),
            bundle_digest: "a".repeat(64),
        });
        observed[0].observation = Observation::Existing {
            identity: MutationIdentity {
                uid: "original-uid".into(),
                resource_version: "10".into(),
            },
        };
        let prepared = reduce(
            machine,
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
            _ => panic!(),
        };
        let pending = reduce(
            prepared.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap()
        .state;
        let issued = reduce(
            pending,
            EventV0::Revalidated {
                frozen: FrozenInputV0 {
                    state_resource_version: "12".into(),
                    ..frozen
                },
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let issued = acknowledge_dispatch_authorization(issued, "13");
        let (transaction_id, attempt_token, command_id) = match &issued.commands[0] {
            CommandV0::Mutate {
                transaction_id,
                attempt_token,
                commands,
            } => (
                transaction_id.clone(),
                attempt_token.clone(),
                commands[0].command_id.clone(),
            ),
            _ => panic!(),
        };
        let transaction = issued
            .state
            .journal
            .state
            .apply_transaction
            .clone()
            .unwrap();
        let mut response_lost = DesiredObservationV0 {
            identity: identity.clone(),
            observation: Observation::Existing {
                identity: MutationIdentity {
                    uid: "replacement-uid".into(),
                    resource_version: "11".into(),
                },
            },
            qualification: BackendQualification::BuiltInTested {
                kubernetes_minor: "1.36".into(),
                gvr: "configmaps".into(),
            },
            recovery_proof: Some(RecoveryProofV0 {
                root_uid: "root-uid".into(),
                transaction_id: transaction_id.clone(),
                command_id: command_id.clone(),
                uid: "replacement-uid".into(),
                resource_version: "11".into(),
            }),
            deletion_recovery_proof: None,
        };
        assert!(
            recover_prepared_result(&issued.state, &transaction, &[response_lost.clone()])
                .unwrap()
                .is_empty()
        );
        response_lost.observation = Observation::Existing {
            identity: MutationIdentity {
                uid: "original-uid".into(),
                resource_version: "11".into(),
            },
        };
        response_lost.recovery_proof.as_mut().unwrap().uid = "original-uid".into();
        assert_eq!(
            recover_prepared_result(&issued.state, &transaction, &[response_lost]).unwrap()[0]
                .outcome,
            ApplyOutcome::Applied
        );
        let wrong_uid = reduce(
            issued.state.clone(),
            EventV0::MutationAcknowledged {
                transaction_id: transaction_id.clone(),
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: command_id.clone(),
                    attempt_token: attempt_token.clone(),
                    identity: identity.clone(),
                    outcome: ApplyOutcome::Applied,
                    uid: Some("replacement-uid".into()),
                    resource_version: Some("11".into()),
                }],
            },
        )
        .unwrap();
        assert!(matches!(
            wrong_uid.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
        let equal_rv = reduce(
            issued.state.clone(),
            EventV0::MutationAcknowledged {
                transaction_id: transaction_id.clone(),
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: command_id.clone(),
                    attempt_token: attempt_token.clone(),
                    identity: identity.clone(),
                    outcome: ApplyOutcome::Applied,
                    uid: Some("original-uid".into()),
                    resource_version: Some("10".into()),
                }],
            },
        )
        .unwrap();
        assert!(matches!(
            equal_rv.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
        let opaque_distinct_rv = reduce(
            issued.state,
            EventV0::MutationAcknowledged {
                transaction_id,
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id,
                    attempt_token,
                    identity,
                    outcome: ApplyOutcome::Applied,
                    uid: Some("original-uid".into()),
                    // Kubernetes resource versions are opaque. A value that
                    // looks numerically older is still admissible if distinct.
                    resource_version: Some("1".into()),
                }],
            },
        )
        .unwrap();
        assert!(matches!(
            opaque_distinct_rv.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
    }

    #[test]
    fn multi_intent_progress_persists_one_result_before_the_next_effect() {
        let machine = boot_to_render(state());
        let (frozen, input, output, mut observed) = rendered(&machine.root, vec![]);
        let mut output: RenderOutputV0 = serde_json::from_str(&output).unwrap();
        output.resources.push(ResourceDeclaration {
            logical_id: "object-two".into(),
            object: serde_json::json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"namespace":"demo","name":"object-two"}}),
            readiness: None,
        });
        observed.push(DesiredObservationV0 {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "demo".into(),
                name: "object-two".into(),
            },
            observation: Observation::Absent,
            qualification: BackendQualification::BuiltInTested {
                kubernetes_minor: "1.36".into(),
                gvr: "configmaps".into(),
            },
            recovery_proof: None,
            deletion_recovery_proof: None,
        });
        let output_json = serde_json::to_string(&output).unwrap();
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input.clone(),
                render_output_json: output_json.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let prepare_operation = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare_operation.token,
                old_state_resource_version: prepare_operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let issued = reduce(
            revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: output_json.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let issued = acknowledge_dispatch_authorization(issued, "13");
        let (transaction_id, attempt_token, command) = match &issued.commands[0] {
            CommandV0::Mutate {
                transaction_id,
                attempt_token,
                commands,
            } => {
                assert_eq!(commands.len(), 1);
                (
                    transaction_id.clone(),
                    attempt_token.clone(),
                    commands[0].clone(),
                )
            }
            _ => panic!("exactly one effect"),
        };
        let acknowledged = reduce(
            issued.state,
            EventV0::MutationAcknowledged {
                transaction_id: transaction_id.clone(),
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: command.command_id.clone(),
                    attempt_token: attempt_token.clone(),
                    identity: command.identity.clone(),
                    outcome: ApplyOutcome::Applied,
                    uid: Some("uid-one".into()),
                    resource_version: Some("30".into()),
                }],
            },
        )
        .unwrap();
        assert!(matches!(
            acknowledged.commands.as_slice(),
            [CommandV0::Persist { .. }]
        ));
        assert_eq!(
            acknowledged
                .state
                .journal
                .state
                .apply_transaction
                .as_ref()
                .unwrap()
                .results
                .len(),
            1
        );
        let first_persist = match &acknowledged.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let after_first = reduce(
            acknowledged.state,
            EventV0::Persisted {
                token: first_persist.token,
                old_state_resource_version: first_persist.old_state_resource_version,
                new_state_resource_version: "14".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let mut after_first_observed = observed.clone();
        after_first_observed
            .iter_mut()
            .find(|item| item.identity == command.identity)
            .unwrap()
            .observation = Observation::Existing {
            identity: MutationIdentity {
                uid: "uid-one".into(),
                resource_version: "30".into(),
            },
        };
        let mut changed_output: RenderOutputV0 = serde_json::from_str(&output_json).unwrap();
        changed_output
            .resources
            .iter_mut()
            .find(|resource| resource.logical_id == "object-two")
            .unwrap()
            .logical_id = "object-two-changed".into();
        let invalidated = reduce(
            after_first.state.clone(),
            EventV0::Revalidated {
                frozen: after_first.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: serde_json::to_string(&changed_output).unwrap(),
                observed: after_first_observed.clone(),
            },
        )
        .unwrap();
        assert!(matches!(
            invalidated.commands.as_slice(),
            [CommandV0::Persist {
                operation: PersistOperation {
                    purpose: PersistPurpose::Invalidate,
                    ..
                },
                ..
            }]
        ));
        assert!(
            invalidated
                .commands
                .iter()
                .all(|command| !matches!(command, CommandV0::Mutate { .. }))
        );
        let second_issued = reduce(
            after_first.state.clone(),
            EventV0::Revalidated {
                frozen: after_first.state.frozen.clone().unwrap(),
                render_input_json: input.clone(),
                render_output_json: output_json.clone(),
                observed: after_first_observed.clone(),
            },
        )
        .unwrap();
        let second_issued = acknowledge_dispatch_authorization(second_issued, "15");
        let second_command = match &second_issued.commands[0] {
            CommandV0::Mutate { commands, .. } => {
                assert_eq!(commands.len(), 1);
                commands[0].clone()
            }
            _ => panic!("the next remaining command must issue after the first result persists"),
        };
        let second_ack = reduce(
            second_issued.state,
            EventV0::MutationAcknowledged {
                transaction_id,
                attempt_token: attempt_token.clone(),
                results: vec![ApplyResult {
                    command_id: second_command.command_id.clone(),
                    attempt_token,
                    identity: second_command.identity.clone(),
                    outcome: ApplyOutcome::Applied,
                    uid: Some("uid-two".into()),
                    resource_version: Some("31".into()),
                }],
            },
        )
        .unwrap();
        let second_persist = match &second_ack.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let after_second = reduce(
            second_ack.state,
            EventV0::Persisted {
                token: second_persist.token,
                old_state_resource_version: second_persist.old_state_resource_version,
                new_state_resource_version: "16".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        after_first_observed
            .iter_mut()
            .find(|item| item.identity == second_command.identity)
            .unwrap()
            .observation = Observation::Existing {
            identity: MutationIdentity {
                uid: "uid-two".into(),
                resource_version: "31".into(),
            },
        };
        let completion = reduce(
            after_second.state.clone(),
            EventV0::Revalidated {
                frozen: after_second.state.frozen.clone().unwrap(),
                render_input_json: input,
                render_output_json: output_json,
                observed: after_first_observed,
            },
        )
        .unwrap();
        let completion_persist = match &completion.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!("final result must lead to completion persistence"),
        };
        let ready = reduce(
            completion.state,
            EventV0::Persisted {
                token: completion_persist.token,
                old_state_resource_version: completion_persist.old_state_resource_version,
                new_state_resource_version: "17".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(ready.state.phase, PhaseV0::Ready);
        assert_eq!(ready.state.journal.state.inventory.len(), 2);
    }

    #[test]
    fn dependency_reconcile_reenters_ready_without_replacing_root_authority() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let ready = complete_single_apply_to_ready(
            machine,
            frozen,
            input,
            output,
            observed,
            "created-uid",
            "30",
        );
        let root = ready.root.clone();
        let requested = reduce(
            ready,
            EventV0::ReconcileRequested {
                cause: InternalReconcileCauseV0::DependencyWatch,
                observation_token: "barrier-2".into(),
                cache_epoch: "epoch-2".into(),
            },
        )
        .unwrap();
        assert_eq!(requested.state.root, root);
        assert_eq!(requested.state.phase, PhaseV0::RenderPending);
        match &requested.commands[0] {
            CommandV0::RequestRender { preconditions } => {
                assert_eq!(preconditions.observation_token, "barrier-2");
                assert_eq!(preconditions.cache_epoch, "epoch-2");
            }
            _ => panic!("dependency wakeup must request a fresh render"),
        }
    }

    #[test]
    fn unchanged_source_across_epoch_and_global_barrier_emits_no_automation_occurrence() {
        let mut machine = state();
        machine.phase = PhaseV0::Ready;
        let mut trigger = ActionTriggerV0 {
            automation_identity: "smoke".into(),
            automation_version: "v1".into(),
            action_module_identity: "actions/smoke.mjs".into(),
            action_module_digest: "b".repeat(64),
            source: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "demo".into(),
                name: "source".into(),
            },
            source_uid: "source-uid".into(),
            source_resource_version: "7".into(),
            observation_token: String::new(),
            event_metadata: serde_json::json!({"reason":"unchanged"}),
        };
        trigger.observation_token = automation_source_transition_token(&trigger);
        machine
            .journal
            .state
            .automation
            .baselines
            .push(AutomationBaseline {
                automation_id: trigger.automation_identity.clone(),
                source: trigger.source.clone(),
                token: trigger.observation_token.clone(),
            });

        let requested = reduce(
            machine,
            EventV0::ReconcileRequested {
                cause: InternalReconcileCauseV0::DependencyWatch,
                observation_token: "global-barrier-2".into(),
                cache_epoch: "epoch-2".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            requested.commands.as_slice(),
            [CommandV0::RequestRender { .. }]
        ));

        // Model the unchanged render's return to Ready. Neither the global
        // barrier nor cache epoch participates in the source-transition key.
        let mut ready_after_unchanged_render = requested.state;
        ready_after_unchanged_render.phase = PhaseV0::Ready;
        let no_occurrence = reduce(
            ready_after_unchanged_render,
            EventV0::TriggerAutomation { trigger },
        )
        .unwrap();
        assert!(no_occurrence.commands.is_empty());
        assert!(
            no_occurrence
                .state
                .journal
                .state
                .automation
                .pending
                .is_empty()
        );
    }

    #[test]
    fn exact_root_refresh_invalidates_same_bundle_and_holds_uid_digest_or_program_replacement() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let ready = complete_single_apply_to_ready(
            machine,
            frozen,
            input,
            output,
            observed,
            "created-uid",
            "30",
        );
        let mut advanced = ready.root.clone();
        advanced.generation += 1;
        advanced.root_resource_version = "31".into();
        advanced.status.observed_generation = Some(advanced.generation);
        advanced.spec.message = "refreshed root spec".into();
        let refreshed = reduce(
            ready.clone(),
            EventV0::RootRefreshed {
                root: advanced.clone(),
            },
        )
        .unwrap();
        assert_eq!(refreshed.state.root, advanced);
        assert!(matches!(
            refreshed.commands.as_slice(),
            [CommandV0::Persist {
                operation: PersistOperation {
                    purpose: PersistPurpose::Invalidate,
                    ..
                },
                ..
            }]
        ));

        let mut foreign_uid = ready.root.clone();
        foreign_uid.uid = "replacement-uid".into();
        assert!(matches!(
            reduce(ready.clone(), EventV0::RootRefreshed { root: foreign_uid })
                .unwrap()
                .commands
                .as_slice(),
            [CommandV0::Hold { .. }]
        ));
        let mut changed_program_config_map = ready.root.clone();
        changed_program_config_map.spec.program.config_map_name = "replacement-bundle".into();
        assert!(matches!(
            reduce(
                ready.clone(),
                EventV0::RootRefreshed {
                    root: changed_program_config_map
                }
            )
            .unwrap()
            .commands
            .as_slice(),
            [CommandV0::Hold { .. }]
        ));
        let mut changed_program_key = ready.root.clone();
        changed_program_key.spec.program.key = "replacement-program.mjs".into();
        assert!(matches!(
            reduce(
                ready.clone(),
                EventV0::RootRefreshed {
                    root: changed_program_key
                }
            )
            .unwrap()
            .commands
            .as_slice(),
            [CommandV0::Hold { .. }]
        ));
        let mut changed_digest = ready.root.clone();
        changed_digest.bundle_digest = "b".repeat(64);
        changed_digest.spec.program.digest = "b".repeat(64);
        assert!(matches!(
            reduce(
                ready,
                EventV0::RootRefreshed {
                    root: changed_digest
                }
            )
            .unwrap()
            .commands
            .as_slice(),
            [CommandV0::Hold { .. }]
        ));
    }

    #[test]
    fn root_refresh_preserves_may_have_issued_transaction_and_holds() {
        let machine = boot_to_render(state());
        let (frozen, input, output, observed) = rendered(&machine.root, vec![]);
        let prepared = reduce(
            machine,
            EventV0::Rendered {
                frozen,
                render_input_json: input.clone(),
                render_output_json: output.clone(),
                observed: observed.clone(),
            },
        )
        .unwrap();
        let operation = match &prepared.commands[0] {
            CommandV0::Persist { operation, .. } => operation.clone(),
            _ => panic!(),
        };
        let revalidate = reduce(
            prepared.state,
            EventV0::Persisted {
                token: operation.token,
                old_state_resource_version: operation.old_state_resource_version,
                new_state_resource_version: "12".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        let issued = reduce(
            revalidate.state.clone(),
            EventV0::Revalidated {
                frozen: revalidate.state.frozen.clone().unwrap(),
                render_input_json: input,
                render_output_json: output,
                observed,
            },
        )
        .unwrap();
        let issued = acknowledge_dispatch_authorization(issued, "13");
        let mut advanced = issued.state.root.clone();
        advanced.generation = 8;
        advanced.root_resource_version = "31".into();
        advanced.status.observed_generation = Some(8);
        let coalesced = reduce(issued.state, EventV0::RootRefreshed { root: advanced }).unwrap();
        assert_eq!(coalesced.state.phase, PhaseV0::Hold);
        assert!(matches!(
            coalesced.commands.as_slice(),
            [CommandV0::Hold { .. }]
        ));
    }

    #[test]
    fn deletion_absence_requires_prepared_claim_and_changed_identity_holds() {
        let mut machine = state();
        let inventory = InventoryEntry {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "demo".into(),
                name: "old".into(),
            },
            logical_id: "old".into(),
            uid: "old-uid".into(),
            resource_version: "old-rv".into(),
            bundle_digest: machine.journal.root.bundle_digest.clone(),
        };
        machine.journal.state.inventory.push(inventory.clone());
        machine.journal.state.deletion = Some(DeletionTransaction {
            id: "d".repeat(64),
            deletion_start_generation: 7,
        });
        machine.phase = PhaseV0::DeletionObservationPending;
        deletion_prepare(&mut machine, &mut Vec::new()).unwrap();
        machine.journal.state.pending_persist = None;
        machine.phase = PhaseV0::DeletionObservationPending;

        let absent = reduce(
            machine.clone(),
            EventV0::DeletionRevalidated {
                observed_inventory: vec![],
                observed_automation_runs: vec![],
            },
        )
        .unwrap();
        assert!(matches!(
            absent.commands.as_slice(),
            [CommandV0::Persist {
                operation: PersistOperation {
                    purpose: PersistPurpose::DeletionMutationResult,
                    ..
                },
                ..
            }]
        ));
        assert!(absent.state.journal.state.inventory.is_empty());

        let mut replacement = inventory;
        replacement.uid = "replacement-uid".into();
        let held = reduce(
            machine,
            EventV0::DeletionRevalidated {
                observed_inventory: vec![replacement],
                observed_automation_runs: vec![],
            },
        )
        .unwrap();
        assert!(matches!(held.commands.as_slice(), [CommandV0::Hold { .. }]));
    }

    fn root_only_deletion_snapshot(root_finalizer_present: bool) -> RootSnapshotV0 {
        let root_uid = "root-uid";
        let linked_state_name = state_name(root_uid).unwrap();
        RootSnapshotV0 {
            uid: root_uid.into(),
            namespace: "demo".into(),
            name: "demo".into(),
            generation: 9,
            spec: web_application_spec("c".repeat(64)),
            bundle_digest: "c".repeat(64),
            status: WebApplicationStatus {
                state: Some(StateLink {
                    root_uid: root_uid.into(),
                    name: linked_state_name.clone(),
                    uid: "deleted-state-uid".into(),
                }),
                deletion_cleanup_receipt: Some(DeletionCleanupReceipt {
                    root_uid: root_uid.into(),
                    deletion_id: "d".repeat(64),
                    deletion_start_generation: 7,
                    state_name: linked_state_name.clone(),
                    state_uid: "deleted-state-uid".into(),
                    cleanup_token: "e".repeat(64),
                }),
                ..WebApplicationStatus::default()
            },
            state_name: linked_state_name,
            state_uid: "deleted-state-uid".into(),
            state_resource_version: String::new(),
            root_resource_version: "root-rv-20".into(),
            root_finalizer_present,
            state_finalizer_present: false,
            deleting: true,
        }
    }

    #[test]
    fn root_only_deletion_recovery_is_receipt_bounded_and_process_recreatable() {
        let machine =
            MachineStateV0::recover_deleting_root_without_state(root_only_deletion_snapshot(true))
                .unwrap();

        // A restart before the fresh absence proof reissues only the proof;
        // it cannot bootstrap or persist a replacement state.
        let restarted =
            MachineStateV0::recover_json(&serde_json::to_string(&machine).unwrap()).unwrap();
        let verify = reduce(restarted, EventV0::Start).unwrap();
        let (token, preconditions) = match verify.commands.as_slice() {
            [
                CommandV0::VerifyOwnedChildrenAbsent {
                    token,
                    preconditions,
                },
            ] => (token.clone(), preconditions.clone()),
            commands => panic!("unexpected root-only recovery commands: {commands:?}"),
        };
        assert_eq!(preconditions.root_uid, "root-uid");
        assert_eq!(preconditions.root_resource_version, "root-rv-20");
        assert_eq!(preconditions.state_uid, "deleted-state-uid");
        assert!(preconditions.state_resource_version.is_empty());
        assert_ne!(token, "e".repeat(64));

        let remove = reduce(
            verify.state,
            EventV0::OwnedChildrenVerifiedAbsent {
                token: token.clone(),
                root_uid: "root-uid".into(),
                root_generation: 9,
                root_resource_version: "root-rv-20".into(),
                state_uid: "deleted-state-uid".into(),
                state_resource_version: String::new(),
                // This is proof of the cleanup that occurred before the
                // linked state disappeared, not this verification command's
                // freshly exported token.
                cleanup_token: "e".repeat(64),
            },
        )
        .unwrap();
        let remove_token = match remove.commands.as_slice() {
            [
                CommandV0::RemoveRootFinalizer {
                    token,
                    preconditions,
                },
            ] => {
                assert_eq!(preconditions.root_resource_version, "root-rv-20");
                assert_ne!(token, &"e".repeat(64));
                token.clone()
            }
            commands => panic!("unexpected root-only finalizer commands: {commands:?}"),
        };

        let prove_root_absent = reduce(
            remove.state,
            EventV0::RootFinalizerRemoved {
                token: remove_token,
                root_uid: "root-uid".into(),
                old_root_resource_version: "root-rv-20".into(),
                new_root_resource_version: "root-rv-21".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            prove_root_absent.commands.as_slice(),
            [CommandV0::VerifyRootUidAbsent { root_uid, .. }] if root_uid == "root-uid"
        ));

        // A process recreated after the finalizer write must never write it a
        // second time. It performs only the exact same-UID absence GET.
        let restarted =
            MachineStateV0::recover_json(&serde_json::to_string(&prove_root_absent.state).unwrap())
                .unwrap();
        let prove_again = reduce(restarted, EventV0::Start).unwrap();
        let absence_token = match prove_again.commands.as_slice() {
            [
                CommandV0::VerifyRootUidAbsent {
                    token, root_uid, ..
                },
            ] if root_uid == "root-uid" => token.clone(),
            commands => panic!("unexpected post-finalizer commands: {commands:?}"),
        };
        let terminal = reduce(
            prove_again.state,
            EventV0::RootUidVerifiedAbsent {
                token: absence_token,
                root_uid: "root-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(terminal.state.phase, PhaseV0::Terminal);
        assert!(terminal.commands.is_empty());
    }

    #[test]
    fn root_only_deletion_recovery_holds_on_children_and_rejects_invalid_authority() {
        let started = reduce(
            MachineStateV0::recover_deleting_root_without_state(root_only_deletion_snapshot(true))
                .unwrap(),
            EventV0::Start,
        )
        .unwrap();
        let token = match &started.commands[0] {
            CommandV0::VerifyOwnedChildrenAbsent { token, .. } => token.clone(),
            _ => panic!(),
        };
        let held = reduce(
            started.state,
            EventV0::OwnedChildrenVerificationFailed {
                token,
                reason: "owned child remains present".into(),
            },
        )
        .unwrap();
        assert!(matches!(held.commands.as_slice(), [CommandV0::Hold { .. }]));

        let mut nondeleting = root_only_deletion_snapshot(true);
        nondeleting.deleting = false;
        assert!(MachineStateV0::recover_deleting_root_without_state(nondeleting).is_err());

        let mut replacement = root_only_deletion_snapshot(true);
        replacement.uid = "replacement-uid".into();
        assert!(MachineStateV0::recover_deleting_root_without_state(replacement).is_err());

        let mut mismatched = root_only_deletion_snapshot(true);
        mismatched
            .status
            .deletion_cleanup_receipt
            .as_mut()
            .unwrap()
            .state_uid = "different-state-uid".into();
        assert!(MachineStateV0::recover_deleting_root_without_state(mismatched).is_err());
    }

    #[test]
    fn root_only_deletion_recovery_has_a_strict_event_allowlist() {
        let started = reduce(
            MachineStateV0::recover_deleting_root_without_state(root_only_deletion_snapshot(true))
                .unwrap(),
            EventV0::Start,
        )
        .unwrap();
        let disallowed = [
            EventV0::BeginDeletion,
            EventV0::InformersReady,
            EventV0::Persisted {
                token: "persist".into(),
                old_state_resource_version: String::new(),
                new_state_resource_version: "forged".into(),
                state_uid: "deleted-state-uid".into(),
            },
            EventV0::MutationFailed {
                attempt_token: "attempt".into(),
                transaction_id: "transaction".into(),
                reason: "forged".into(),
            },
            EventV0::StateFinalizerRemoved {
                token: "state-finalizer".into(),
                state_uid: "deleted-state-uid".into(),
                old_state_resource_version: String::new(),
                new_state_resource_version: "forged".into(),
            },
        ];
        for event in disallowed {
            let rejected = reduce(started.state.clone(), event).unwrap();
            assert!(matches!(
                rejected.commands.as_slice(),
                [CommandV0::Hold { .. }]
            ));
        }

        let duplicate_start = reduce(started.state, EventV0::Start).unwrap();
        assert!(matches!(
            duplicate_start.commands.as_slice(),
            [CommandV0::IgnoredStale { .. }]
        ));
    }

    #[test]
    fn root_only_refresh_reissues_only_under_unchanged_deletion_authority() {
        let started = reduce(
            MachineStateV0::recover_deleting_root_without_state(root_only_deletion_snapshot(true))
                .unwrap(),
            EventV0::Start,
        )
        .unwrap();
        let mut refreshed_root = started.state.root.clone();
        refreshed_root.generation += 1;
        refreshed_root.root_resource_version = "root-rv-21".into();
        refreshed_root.bundle_digest = "f".repeat(64);
        refreshed_root.spec.program.digest = "f".repeat(64);
        let refreshed = reduce(
            started.state.clone(),
            EventV0::RootRefreshed {
                root: refreshed_root,
            },
        )
        .unwrap();
        assert!(matches!(
            refreshed.commands.as_slice(),
            [CommandV0::VerifyOwnedChildrenAbsent { preconditions, .. }]
                if preconditions.root_generation == 10
                    && preconditions.root_resource_version == "root-rv-21"
        ));

        let mut missing_finalizer = started.state.root.clone();
        missing_finalizer.root_resource_version = "root-rv-22".into();
        missing_finalizer.root_finalizer_present = false;
        assert!(matches!(
            reduce(
                started.state.clone(),
                EventV0::RootRefreshed {
                    root: missing_finalizer
                }
            )
            .unwrap()
            .commands
            .as_slice(),
            [CommandV0::Hold { .. }]
        ));

        let mut changed_receipt = started.state.root.clone();
        changed_receipt.root_resource_version = "root-rv-23".into();
        changed_receipt
            .status
            .deletion_cleanup_receipt
            .as_mut()
            .unwrap()
            .cleanup_token = "a".repeat(64);
        assert!(matches!(
            reduce(
                started.state.clone(),
                EventV0::RootRefreshed {
                    root: changed_receipt
                }
            )
            .unwrap()
            .commands
            .as_slice(),
            [CommandV0::Hold { .. }]
        ));

        let mut nondeleting = started.state.root.clone();
        nondeleting.root_resource_version = "root-rv-24".into();
        nondeleting.deleting = false;
        assert!(matches!(
            reduce(started.state, EventV0::RootRefreshed { root: nondeleting })
                .unwrap()
                .commands
                .as_slice(),
            [CommandV0::Hold { .. }]
        ));
    }
}
