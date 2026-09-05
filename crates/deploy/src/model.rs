use compiler_core::{Capability, CompiledPlan, DeletionPolicy, PermissionCeiling};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerationPlan {
    pub generation_id: String,
    pub artifact_digest: String,
    pub compiled: CompiledPlan,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceIdentity {
    pub api_group: String,
    pub resource: String,
    pub namespace: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaimState {
    Active,
    Reserved,
    Releasing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtectedClaim {
    pub identity: ResourceIdentity,
    pub domain_id: String,
    pub generation_id: String,
    pub transition_id: String,
    pub state: ClaimState,
    pub uid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistrySnapshot {
    pub revision: String,
    pub claims: Vec<ProtectedClaim>,
    pub generation_history: GenerationHistory,
}

/// Platform-owned, complete allocation history. Failed and retired allocations
/// remain present permanently; current claims cannot replace this authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerationHistory {
    pub domain_id: String,
    pub cluster_binding: String,
    pub revision: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub active_generation_id: Option<String>,
    pub allocations: Vec<GenerationAllocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerationAllocation {
    pub generation_id: String,
    pub service_account_name: String,
    pub transition_id: String,
    pub artifact_digest: String,
    pub ir_digest: String,
    /// Protected commitment to approved identity/lifecycle metadata, never values.
    /// Missing on legacy allocations; it cannot be inferred as upgrade authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory_metadata_digest: Option<String>,
}

/// Runtime-written information is diagnostic input only. It does not grant
/// lifecycle authority, add claims or substitute for trusted UID evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeObservation {
    pub identity: ResourceIdentity,
    pub uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentRequest {
    pub transition_id: String,
    pub previous: Option<GenerationPlan>,
    pub next: GenerationPlan,
    pub runtime_ceiling: PermissionCeiling,
    pub cleanup_ceiling: PermissionCeiling,
    pub registry: RegistrySnapshot,
    pub observations: Vec<RuntimeObservation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ownership_intents: Vec<crate::OwnershipIntent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CleanupItem {
    pub identity: ResourceIdentity,
    pub api_version: String,
    pub kind: String,
    pub policy: DeletionPolicy,
    pub expected_uid: Option<String>,
    pub prior_generation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimReservation {
    pub identity: ResourceIdentity,
    pub prior_generation_id: Option<String>,
    pub next_generation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransitionPlan {
    pub schema_version: String,
    pub plan_digest: String,
    pub request: DeploymentRequest,
    pub reservations: Vec<ClaimReservation>,
    pub generation_reservation: GenerationAllocation,
    pub cleanup: Vec<CleanupItem>,
    pub cleanup_capabilities: Vec<Capability>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Planned,
    Staged,
    Quiesced,
    Activated,
    Cleaning,
    Committed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventReceipt {
    pub event_id: String,
    pub evidence_digest: String,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransitionRecord {
    pub schema_version: String,
    pub transition_id: String,
    pub plan_digest: String,
    pub executor_id: String,
    pub revision: u64,
    pub verification_epoch: u64,
    pub phase: Phase,
    pub blocked_from: Option<Phase>,
    pub blocked_reason: Option<String>,
    pub receipts: Vec<EventReceipt>,
}

/// These are executor evidence records, not self-proving attestations. The
/// trusted adapter must verify and persist their source artifacts independently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminationEvidence {
    pub generation_id: String,
    pub executor_id: String,
    pub verification_epoch: u64,
    pub environment_qualification_digest: String,
    pub process_termination_receipt_digest: String,
    pub access_revocation_receipt_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Quiescence {
    InitialInstallation { verification_epoch: u64 },
    OldGeneration(TerminationEvidence),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CleanupOutcome {
    OrphanReleased,
    Deleted,
    AlreadyAbsent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CleanupReceipt {
    pub identity: ResourceIdentity,
    pub expected_uid: Option<String>,
    pub outcome: CleanupOutcome,
    pub evidence_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Event {
    Staged {
        event_id: String,
        generation_id: String,
        service_account_name: String,
        registry_cas_receipt_digest: String,
        verification_epoch: u64,
    },
    Quiesced {
        event_id: String,
        evidence: Quiescence,
    },
    Activated {
        event_id: String,
        generation_id: String,
        activation_receipt_digest: String,
        verification_epoch: u64,
    },
    Cleaning {
        event_id: String,
    },
    Committed {
        event_id: String,
        cleanup_receipts: Vec<CleanupReceipt>,
        obsolete_identity_retirement_digest: String,
    },
    Blocked {
        event_id: String,
        reason: String,
    },
    Resume {
        event_id: String,
        recovery_receipt_digest: String,
    },
}

impl Event {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Staged { event_id, .. }
            | Self::Quiesced { event_id, .. }
            | Self::Activated { event_id, .. }
            | Self::Cleaning { event_id }
            | Self::Committed { event_id, .. }
            | Self::Blocked { event_id, .. }
            | Self::Resume { event_id, .. } => event_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveCleanupObject {
    Absent,
    Present {
        uid: String,
        finalizers: Vec<String>,
        deletion_started: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupDecision {
    ReleaseOrphanClaim,
    AlreadyAbsent,
    DeleteWithUidPrecondition { uid: String },
    WaitForFinalizers,
}
