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
    AutomationRunCompletionState, AutomationRunSpec, DeletionTransaction, DependencySnapshot,
    DependencySnapshotType, DurablePhase, InventoryEntry, MAX_ACTION_INPUT_CANONICAL_BYTES,
    MAX_IDENTIFIER_BYTES, MAX_LIST_ENTRIES, ObjectIdentity, OxideLoopStateSpec, PersistOperation,
    PersistPurpose, PreparedRecoveryClaim, PruneCandidate, PruneObservation, StateBody,
    WebApplicationStatus, automation_run_name, bootstrap_id, state_name,
    validate_automation_run_name, validate_root_state_link,
};
use oxideloop_mutation_model::{
    BackendQualification, HttpMethod, MutationDecision, MutationIntent, MutationPlanInputV0,
    ObjectIdentity as MutationIdentity, Observation, PROBE_VERSION_V0, plan,
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
    pub generation: i64,
    pub bundle_digest: String,
    pub status: WebApplicationStatus,
    pub state_name: String,
    pub state_uid: String,
    pub state_resource_version: String,
    pub root_resource_version: String,
    pub root_finalizer_present: bool,
    pub state_finalizer_present: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenInputV0 {
    pub root_generation: i64,
    pub state_resource_version: String,
    pub observation_token: String,
    pub cache_epoch: String,
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
    DeletionObservationPending,
    OwnedChildrenVerifyPending,
    StateFinalizerPending,
    RootFinalizerPending,
    RootStatusPersistPending,
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
}

impl Default for RuntimeStateV0 {
    fn default() -> Self {
        Self {
            requires_revalidation: true,
            pending_automation: None,
            pending_root_status: None,
            pending_bootstrap: None,
            pending_descriptor_token: None,
        }
    }
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
}

/// Validated root-plus-durable-journal state for the pure reducer.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineStateV0 {
    root: RootSnapshotV0,
    journal: OxideLoopStateSpec,
    phase: PhaseV0,
    frozen: Option<FrozenInputV0>,
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
            runtime: RuntimeStateV0 {
                requires_revalidation: false,
                pending_automation: None,
                pending_root_status: None,
                pending_bootstrap: None,
                pending_descriptor_token: None,
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
            runtime: RuntimeStateV0 {
                requires_revalidation: false,
                pending_automation: None,
                pending_root_status: None,
                pending_bootstrap: None,
                pending_descriptor_token: None,
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

    /// Safe serde recovery entry point. It never restores runtime issuance.
    pub fn recover_json(serialized: &str) -> Result<Self, ReconcileError> {
        let state: Self = serde_json::from_str(serialized)
            .map_err(|e| ReconcileError::Recovery(e.to_string()))?;
        if validate_root_and_journal(&state.root, &state.journal).is_err() {
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum EventV0 {
    Start,
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
    if matches!(
        state.phase,
        PhaseV0::Bootstrap | PhaseV0::BootstrapPending | PhaseV0::Recovery
    ) {
        validate_bootstrap_machine(&state)?;
    } else {
        validate_root_and_journal(&state.root, &state.journal)?;
    }
    if matches!(event, EventV0::BeginDeletion) {
        return begin_deletion(state);
    }
    let mut commands = Vec::new();
    match event {
        EventV0::Start => start(&mut state, &mut commands),
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
                    preconditions: pins(&state.root),
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
        } => owned_children_verified(
            &mut state,
            &token,
            &root_uid,
            root_generation,
            &root_resource_version,
            &state_uid,
            &state_resource_version,
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
        EventV0::BeginDeletion => unreachable!(),
    }
    finish(state, commands)
}

fn start(state: &mut MachineStateV0, commands: &mut Vec<CommandV0>) {
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
        };
        if !bootstrap_confirmed(state) {
            issue_bootstrap(state, commands);
            return;
        }
        if state.journal.state.deletion.is_some() {
            state.phase = PhaseV0::DeletionObservationPending;
            commands.push(CommandV0::RequestDeletionObservation {
                preconditions: pins(&state.root),
            });
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
    if !queue_root_status(state, status, RootStatusContinuationV0::Informers, commands) {
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
    state.root.root_resource_version = new.into();
    state.root.status = pending.status;
    state.runtime.pending_root_status = None;
    match pending.continuation {
        RootStatusContinuationV0::Informers => state.phase = PhaseV0::InformersPending,
        RootStatusContinuationV0::Render => {
            state.phase = PhaseV0::RenderPending;
            commands.push(CommandV0::RequestRender {
                preconditions: pins(&state.root),
            });
        }
    }
}

fn begin_deletion(mut state: MachineStateV0) -> Result<TransitionV0, ReconcileError> {
    // A root observed during deletion can still be a fresh or partial root.
    // Never issue a state-journal write with empty/fabricated state pins; the
    // same exact bootstrap acknowledgement is required first.
    if !bootstrap_confirmed(&state) {
        let mut commands = Vec::new();
        issue_bootstrap(&mut state, &mut commands);
        return finish(state, commands);
    }
    if state.journal.state.deletion.is_some() {
        return finish(
            state,
            vec![CommandV0::IgnoredStale {
                reason: "deletion is already durable".into(),
            }],
        );
    }
    // Remove every normal issuance capability. The first deletion effect is
    // impossible until this replacement journal write has been acknowledged.
    state.runtime.pending_automation = None;
    state.frozen = None;
    state.journal.state.apply_transaction = None;
    state.journal.state.pending_dependencies.clear();
    state.journal.state.prune_candidate = None;
    state.journal.state.deletion = Some(DeletionTransaction {
        id: hash(&[
            "deletion/v0",
            &state.root.uid,
            &state.root.generation.to_string(),
            &state.root.root_resource_version,
            &state.root.state_resource_version,
        ]),
        root_generation: state.root.generation,
    });
    let mut commands = Vec::new();
    queue_persist(
        &mut state,
        PersistPurpose::DeletionStart,
        DurablePhase::DeletionStart,
        PhaseV0::DeletionStartPersistPending,
        &mut commands,
    )?;
    finish(state, commands)
}

fn queue_persist(
    state: &mut MachineStateV0,
    purpose: PersistPurpose,
    durable_phase: DurablePhase,
    phase: PhaseV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
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
        PersistPurpose::DeletionStart => deletion_prepare(state, commands)?,
        PersistPurpose::DeletionPrepare => deletion_issue(state, commands)?,
        PersistPurpose::DeletionMutationResult | PersistPurpose::DeletionAutomationCleanup => {
            deletion_advance(state, commands)?
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
        PersistPurpose::AutomationBaseline
        | PersistPurpose::AutomationPending
        | PersistPurpose::AutomationCompletion => phase == PhaseV0::AutomationPersistPending,
        PersistPurpose::DeletionStart => phase == PhaseV0::DeletionStartPersistPending,
        PersistPurpose::DeletionPrepare => phase == PhaseV0::DeletionPreparePersistPending,
        PersistPurpose::DeletionMutationResult => phase == PhaseV0::DeletionMutationPersistPending,
        PersistPurpose::DeletionAutomationCleanup => {
            phase == PhaseV0::DeletionAutomationPersistPending
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
    state.journal.state.apply_transaction = Some(ApplyTransaction {
        id: deletion_id,
        generation: state.root.generation,
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
        .map(|item| identity_key(&item.identity))
        .collect::<BTreeSet<_>>();
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
    let observed_inventory_keys = observed_inventory
        .iter()
        .map(|item| identity_key(&item.identity))
        .collect::<BTreeSet<_>>();
    let observed_runs = observed_automation_runs
        .iter()
        .map(|item| item.occurrence_id.as_str())
        .collect::<BTreeSet<_>>();
    if observed_inventory_keys.len() != observed_inventory.len()
        || observed_runs.len() != observed_automation_runs.len()
        || !observed_inventory_keys.is_subset(&known_inventory)
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
            .any(|item| item.bundle_digest != state.root.bundle_digest)
    {
        stale(
            commands,
            "deletion revalidation contains unowned or duplicate child",
        );
        return Ok(());
    }
    // Validate a clone before replacing anything. This makes malformed fresh
    // observations a stale no-op rather than a partially-mutated recovery.
    let mut candidate = state.journal.clone();
    candidate.state.inventory = observed_inventory;
    let observed_by_occurrence = observed_automation_runs
        .iter()
        .map(|item| (item.occurrence_id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    candidate.state.automation.pending.retain_mut(|item| {
        let Some(observed) = observed_by_occurrence.get(item.occurrence_id.as_str()) else {
            return false;
        };
        item.run_name.clone_from(&observed.name);
        item.run_uid.clone_from(&observed.uid);
        item.run_resource_version
            .clone_from(&observed.resource_version);
        true
    });
    candidate.state.automation.completions.retain_mut(|item| {
        let Some(observed) = observed_by_occurrence.get(item.occurrence_id.as_str()) else {
            return false;
        };
        item.run_name.clone_from(&observed.name);
        item.run_uid.clone_from(&observed.uid);
        item.completed_resource_version
            .clone_from(&observed.resource_version);
        true
    });
    candidate
        .state
        .inventory
        .sort_by_key(|item| identity_key(&item.identity));
    candidate
        .state
        .automation
        .pending
        .sort_by(|left, right| left.occurrence_id.cmp(&right.occurrence_id));
    candidate
        .state
        .automation
        .completions
        .sort_by(|left, right| left.occurrence_id.cmp(&right.occurrence_id));
    if candidate.validate().is_err() {
        stale(commands, "deletion revalidation violates durable bounds");
        return Ok(());
    }
    state.journal = candidate;
    deletion_prepare(state, commands)
}

fn deletion_issue(
    state: &mut MachineStateV0,
    commands: &mut Vec<CommandV0>,
) -> Result<(), ReconcileError> {
    let Some(tx) = state.journal.state.apply_transaction.as_ref() else {
        return deletion_advance(state, commands);
    };
    let effects = deletion_commands(state, tx);
    if effects.is_empty() {
        deletion_advance(state, commands)
    } else {
        state.phase = PhaseV0::DeletionMutationPending;
        commands.push(CommandV0::Mutate {
            transaction_id: tx.id.clone(),
            attempt_token: tx.attempt_token.clone(),
            commands: effects,
        });
        Ok(())
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
    if tx.frozen_render_input_hash != frozen.render_input_hash
        || state
            .frozen
            .as_ref()
            .is_some_and(|previous| previous != &frozen)
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
        state.phase = PhaseV0::MutationPending;
        commands.push(CommandV0::Mutate {
            transaction_id: tx.id.clone(),
            attempt_token: tx.attempt_token.clone(),
            commands: effects,
        });
        Ok(())
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
        state.phase = PhaseV0::Hold;
        commands.push(CommandV0::Hold {
            reason: "deletion inventory remains after acknowledged cleanup".into(),
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
        hash(&[
            "verify-owned-children-absent/v0",
            &deletion.id,
            &state.root.uid,
            &state.root.generation.to_string(),
            &state.root.root_resource_version,
            &state.root.state_uid,
            &state.root.state_resource_version,
        ])
    })
}

fn owned_children_verified(
    state: &mut MachineStateV0,
    token: &str,
    root_uid: &str,
    root_generation: i64,
    root_resource_version: &str,
    state_uid: &str,
    state_resource_version: &str,
    commands: &mut Vec<CommandV0>,
) {
    if state.phase != PhaseV0::OwnedChildrenVerifyPending
        || owned_children_token(state).as_deref() != Some(token)
        || root_uid != state.root.uid
        || root_generation != state.root.generation
        || root_resource_version != state.root.root_resource_version
        || state_uid != state.root.state_uid
        || state_resource_version != state.root.state_resource_version
    {
        stale(
            commands,
            "owned-child absence acknowledgement pins mismatch",
        );
        return;
    }
    state.phase = PhaseV0::StateFinalizerPending;
    commands.push(CommandV0::RemoveStateFinalizer {
        token: state_finalizer_token(state),
        preconditions: pins(&state.root),
    });
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
    hash(&[
        "root-finalizer/v0",
        &state.root.uid,
        &state.root.root_resource_version,
        &state.root.state_uid,
        &state.root.state_resource_version,
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
    state.phase = PhaseV0::RootFinalizerPending;
    commands.push(CommandV0::RemoveRootFinalizer {
        token: root_finalizer_token(state),
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
    state.phase = PhaseV0::Bootstrap;
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
    let bytes = canonical_bytes(&trigger.event_metadata)
        .map_err(|error| ReconcileError::Automation(error.to_string()))?;
    if bytes.len() > MAX_ACTION_INPUT_CANONICAL_BYTES {
        return Err(ReconcileError::Automation(
            "eventMetadata exceeds action protocol bound".into(),
        ));
    }
    Ok(())
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

#[allow(clippy::too_many_arguments)]
fn automation_completed(
    state: &mut MachineStateV0,
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
    if state.phase != PhaseV0::Ready
        || attempts == 0
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
                decision,
            });
        }
        effects.sort_by(|a, b| a.command_id.cmp(&b.command_id));
        Ok(effects.into_iter().take(1).collect())
    }
}

fn safe_mutation_decision(decision: &MutationDecision) -> bool {
    matches!(
        decision,
        MutationDecision::Post { .. }
            | MutationDecision::ConditionalApply { .. }
            | MutationDecision::PreconditionedDelete { .. }
    )
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
                                && old.second_observation.as_ref().is_none_or(|second| {
                                    second.observed_generation == frozen.root_generation
                                        && second.observed_resource_version
                                            == actual.resource_version
                                })
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
                                },
                                second_observation: Some(PruneObservation {
                                    observed_resource_version: actual.resource_version.clone(),
                                    observed_generation: frozen.root_generation,
                                }),
                            },
                            |mut old| {
                                old.second_observation = Some(PruneObservation {
                                    observed_resource_version: actual.resource_version.clone(),
                                    observed_generation: frozen.root_generation,
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
    state.journal.state.apply_transaction = Some(ApplyTransaction {
        id: transaction_id,
        generation: frozen.root_generation,
        frozen_render_input_hash: frozen.render_input_hash.clone(),
        desired_graph_hash: plan.hash,
        recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
        attempt_token: attempt,
        prepared_intents: plan.intents,
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

fn validate_bootstrap_root(root: &RootSnapshotV0) -> Result<(), ReconcileError> {
    for (field, value) in [
        ("root.uid", root.uid.as_str()),
        ("root.namespace", root.namespace.as_str()),
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
    Ok(())
}

fn validate_journal_generation(
    root: &RootSnapshotV0,
    journal: &OxideLoopStateSpec,
) -> Result<(), ReconcileError> {
    for (field, generation) in [
        (
            "apply transaction",
            journal
                .state
                .apply_transaction
                .as_ref()
                .map(|transaction| transaction.generation),
        ),
        (
            "pending persist",
            journal
                .state
                .pending_persist
                .as_ref()
                .map(|operation| operation.root_generation),
        ),
        (
            "deletion transaction",
            journal
                .state
                .deletion
                .as_ref()
                .map(|transaction| transaction.root_generation),
        ),
    ] {
        if generation.is_some_and(|actual| actual != root.generation) {
            return Err(ReconcileError::Link(format!(
                "{field} generation does not match root generation"
            )));
        }
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
        if matches!(
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
        ConditionStatus, RootBinding, RootCondition, RootConditionType, STATE_VERSION_V1,
        StateBody, StateLink, bootstrap_id, state_name,
    };
    use oxideloop_renderer_abi::RenderOutputV0;
    use oxideloop_renderer_abi::model::{ABI_VERSION_V0, ResourceDeclaration, RootProjection};

    fn state() -> MachineStateV0 {
        let root_uid = "root-uid";
        let state_name = state_name(root_uid).unwrap();
        MachineStateV0::new(
            RootSnapshotV0 {
                uid: root_uid.into(),
                namespace: "demo".into(),
                generation: 7,
                bundle_digest: "a".repeat(64),
                state_name: state_name.clone(),
                state_uid: "state-uid".into(),
                state_resource_version: "11".into(),
                root_resource_version: "20".into(),
                root_finalizer_present: true,
                state_finalizer_present: true,
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
                name: "demo".into(),
                uid: root.uid.clone(),
                generation: root.generation as u64,
                spec: serde_json::json!({}),
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
            _ => panic!("single apply must issue its one prepared effect"),
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
                new_state_resource_version: "13".into(),
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
                new_state_resource_version: "14".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap()
        .state
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
            [CommandV0::RequestDeletionObservation { .. }]
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
        let verify = reduce(
            prepared.state,
            EventV0::Persisted {
                token: prepare_operation.token,
                old_state_resource_version: prepare_operation.old_state_resource_version,
                new_state_resource_version: "13".into(),
                state_uid: "state-uid".into(),
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
            [CommandV0::RequestDeletionObservation { .. }]
        ));
        let stale = reduce(
            verify.state.clone(),
            EventV0::OwnedChildrenVerifiedAbsent {
                token: "0".repeat(64),
                root_uid: "root-uid".into(),
                root_generation: 7,
                root_resource_version: "20".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "13".into(),
            },
        )
        .unwrap();
        assert_eq!(stale.state.phase, PhaseV0::OwnedChildrenVerifyPending);
        let finalizer = reduce(
            verify.state,
            EventV0::OwnedChildrenVerifiedAbsent {
                token,
                root_uid: "root-uid".into(),
                root_generation: 7,
                root_resource_version: "20".into(),
                state_uid: "state-uid".into(),
                state_resource_version: "13".into(),
            },
        )
        .unwrap();
        assert!(matches!(
            finalizer.commands.as_slice(),
            [CommandV0::RemoveStateFinalizer { .. }]
        ));
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
        match &root_finalizer.commands[0] {
            CommandV0::RemoveRootFinalizer { preconditions, .. } => {
                assert_eq!(preconditions.state_resource_version, "14")
            }
            _ => panic!("state acknowledgement must pin its new RV before root finalization"),
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
            root_generation: 7,
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
        machine
            .journal
            .state
            .automation
            .baselines
            .push(AutomationBaseline {
                automation_id: "smoke".into(),
                source: source.clone(),
                token: "old".into(),
            });
        let trigger = ActionTriggerV0 {
            automation_identity: "smoke".into(),
            automation_version: "v1".into(),
            action_module_identity: "actions/smoke.mjs".into(),
            action_module_digest: "b".repeat(64),
            source: source.clone(),
            source_uid: "source-uid".into(),
            source_resource_version: "7".into(),
            observation_token: "new".into(),
            event_metadata: serde_json::json!({"reason":"changed"}),
        };
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
                name,
                spec,
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
                occurrence_id: "occurrence".into(),
                automation_id: "smoke".into(),
                source,
                run_name: "ol-run".into(),
                run_uid: "run-uid".into(),
                run_resource_version: "8".into(),
            });
        let stale_completion = reduce(
            completion_state.clone(),
            EventV0::AutomationCompleted {
                occurrence_id: "occurrence".into(),
                name: "ol-run".into(),
                uid: "other-uid".into(),
                old_resource_version: "8".into(),
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
        let completed = reduce(
            completion_state,
            EventV0::AutomationCompleted {
                occurrence_id: "occurrence".into(),
                name: "ol-run".into(),
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
    fn serialized_prepared_state_requires_new_observation_and_cannot_replay_effects() {
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
        let second = reduce(
            next_render,
            EventV0::Rendered {
                frozen: FrozenInputV0 {
                    state_resource_version: "13".into(),
                    ..frozen
                },
                render_input_json: input,
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
                new_state_resource_version: "13".into(),
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
                new_state_resource_version: "14".into(),
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
    fn recovery_rejects_foreign_or_negative_deletion_generations_before_effects() {
        for forged_generation in [6, -1] {
            let mut forged = state();
            forged.journal.state.deletion = Some(DeletionTransaction {
                id: "d".repeat(64),
                root_generation: forged_generation,
            });
            let recovered: MachineStateV0 =
                serde_json::from_str(&serde_json::to_string(&forged).unwrap()).unwrap();
            assert_eq!(recovered.phase, PhaseV0::Recovery);
            assert!(matches!(
                reduce(recovered, EventV0::Start),
                Err(ReconcileError::Link(_))
            ));
        }
    }

    #[test]
    fn deletion_cleans_completed_runs_by_exact_uid_and_resource_version() {
        let mut machine = state();
        machine.journal.state.deletion = Some(DeletionTransaction {
            id: "d".repeat(64),
            root_generation: 7,
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
    fn recovery_proof_recovers_one_lost_create_without_adopting_foreign_name() {
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
        assert_eq!(
            fresh_frozen.state_resource_version,
            render.state.root.state_resource_version
        );
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
        let recovered_result = reduce(
            render.state,
            EventV0::Revalidated {
                frozen: fresh_frozen,
                render_input_json: input,
                render_output_json: output,
                observed: proof_observed,
            },
        )
        .unwrap();
        assert!(
            matches!(
                recovered_result.commands.as_slice(),
                [CommandV0::Persist { .. }]
            ),
            "{:?}",
            recovered_result.commands
        );
        assert_eq!(
            recovered_result.state.journal.state.inventory[0].uid,
            "created-uid"
        );
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
            recover_prepared_result(&machine, &tx, &[observation]).unwrap()[0].outcome,
            ApplyOutcome::AlreadyAbsent
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
                new_state_resource_version: "13".into(),
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
                new_state_resource_version: "14".into(),
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
                new_state_resource_version: "15".into(),
                state_uid: "state-uid".into(),
            },
        )
        .unwrap();
        assert_eq!(ready.state.phase, PhaseV0::Ready);
        assert_eq!(ready.state.journal.state.inventory.len(), 2);
    }
}
