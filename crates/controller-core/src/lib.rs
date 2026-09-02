//! Pure, deterministic reconcile planning for one keyed `OxideLoop` root.
//!
//! This crate has no Kubernetes client, renderer process, clock, or random
//! source. Adapters submit immutable snapshots through [`reduce`] and execute
//! the returned commands only after checking their attached preconditions.

#![forbid(unsafe_code)]

mod reducer;

pub use reducer::{
    ActionTriggerV0, AutomationRunCleanupV0, CommandV0, DeletionAuthorityModeV0,
    DeletionRecoveryProofV0, DesiredObservationV0, DesiredProjectionV0, EventV0, FrozenInputV0,
    InternalReconcileCauseV0, MachineStateV0, MutationCommandV0, OrdinaryProofV0,
    PendingAutomationV0, PendingRootStatusV0, PhaseV0, PreconditionsV0, ReconcileError,
    RecoveryProofV0, RootSnapshotV0, RootStatusContinuationPayloadV0, RootStatusContinuationV0,
    RuntimeStateV0, TransitionV0, automation_source_transition_token,
    linked_root_finalizer_command_token, linked_state_finalizer_command_token, reduce,
    retired_state_delete_command_token, root_finalizer_command_token,
    root_only_finalizer_command_token, root_only_owned_children_verify_token,
    validate_deleting_root_receipt,
};
