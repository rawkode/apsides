use std::{collections::BTreeMap, fmt};

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{IdError, automation_run_name, bootstrap_id, state_name};

pub const API_DOMAIN: &str = "oxideloop.dev";
pub const ROOT_FINALIZER: &str = "demo.oxideloop.dev/finalizer";
pub const STATE_FINALIZER: &str = "runtime.oxideloop.dev/state-finalizer";
pub const ANNOTATION_BOOTSTRAP_ID: &str = "runtime.oxideloop.dev/bootstrap-id";
pub const ANNOTATION_ROOT_UID: &str = "runtime.oxideloop.dev/root-uid";
pub const ANNOTATION_STATE_NAME: &str = "runtime.oxideloop.dev/state-name";
pub const ANNOTATION_STATE_UID: &str = "runtime.oxideloop.dev/state-uid";
pub const ANNOTATION_TRANSACTION_ID: &str = "runtime.oxideloop.dev/transaction-id";
pub const ANNOTATION_BUNDLE_DIGEST: &str = "runtime.oxideloop.dev/bundle-digest";
pub const ANNOTATION_LOGICAL_ID: &str = "runtime.oxideloop.dev/logical-id";
pub const STATE_VERSION_V1: u16 = 1;

pub const MAX_MESSAGE_BYTES: usize = 1_024;
pub const MAX_IDENTIFIER_BYTES: usize = 253;
pub const MAX_DIGEST_BYTES: usize = 128;
pub const MAX_LIST_ENTRIES: usize = 1_024;
/// A root refresh may rebase one acknowledgement closure only this many
/// times.  The bound keeps an unstable root from retaining an old effect
/// acknowledgement indefinitely.
pub const MAX_CLOSURE_REBASES: u8 = 4;
/// The exact Action request frame payload cap in `renderer-protocol`.
pub const MAX_ACTION_INPUT_CANONICAL_BYTES: usize = 512 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpecValidationError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} exceeds its {maximum}-byte limit")]
    TooLong { field: &'static str, maximum: usize },
    #[error("unsupported OxideLoopState state version {actual}; supported version is {supported}")]
    UnsupportedStateVersion { actual: u16, supported: u16 },
    #[error("state root UID and bootstrap ID do not agree")]
    RootBootstrapMismatch,
    #[error("root and state links do not identify the same root UID")]
    RootUidMismatch,
    #[error("the WebApplication status state link does not match the state identity")]
    StateIdentityMismatch,
    #[error("the OxideLoopState metadata name is not the deterministic name for its root UID")]
    StateNameMismatch,
    #[error(
        "the AutomationRun metadata name is not the deterministic name for its root UID and occurrence ID"
    )]
    AutomationRunNameMismatch,
    #[error("AutomationRun specs are immutable after creation")]
    ImmutableAutomationRunSpec,
    #[error("AutomationRun program digest must match its bundle digest")]
    AutomationRunProgramDigestMismatch,
    #[error("the persisted ActionInputV0 is not canonical JSON")]
    NonCanonicalActionInput,
    #[error("the persisted ActionInputV0 does not match its indexed immutable pins")]
    ActionInputPinMismatch,
    #[error("the persisted ActionInputV0 hash does not match its canonical bytes")]
    ActionInputHashMismatch,
    #[error("the persisted ActionInputV0 event metadata hash does not match its canonical bytes")]
    EventMetadataHashMismatch,
    #[error("{field} must be a 64-character lowercase hexadecimal SHA-256 digest")]
    InvalidSha256 { field: &'static str },
    #[error("the persisted ActionInputV0 is invalid: {0}")]
    InvalidActionInput(String),
    #[error("{field} exceeds the durable list-entry limit")]
    ListBound { field: &'static str },
    #[error("{field} contains a duplicate or uncorrelated entry")]
    Correlation { field: &'static str },
    #[error("pendingPersist purpose and durable phase do not form an exact pair")]
    PersistPurposePhaseMismatch,
    #[error("pendingPersist root generation must be non-negative")]
    InvalidRootGeneration,
    #[error("AutomationRun status materialization name and UID must be both absent or both valid")]
    InvalidMaterializationPair,
    #[error(transparent)]
    Identifier(#[from] IdError),
}

fn bounded_len(len: usize, field: &'static str) -> Result<(), SpecValidationError> {
    if len > MAX_LIST_ENTRIES {
        Err(SpecValidationError::ListBound { field })
    } else {
        Ok(())
    }
}

fn non_empty_within(
    value: &str,
    field: &'static str,
    maximum: usize,
) -> Result<(), SpecValidationError> {
    if value.is_empty() {
        return Err(SpecValidationError::Empty { field });
    }
    if value.len() > maximum {
        return Err(SpecValidationError::TooLong { field, maximum });
    }
    Ok(())
}

fn raw_sha256(value: &str, field: &'static str) -> Result<(), SpecValidationError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(SpecValidationError::InvalidSha256 { field })
    }
}

/// Immutable reference to an already verified program bundle in a `ConfigMap`.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct ProgramBundleReference {
    #[schemars(length(min = 1, max = 253))]
    pub config_map_name: String,
    #[schemars(length(min = 1, max = 253))]
    pub key: String,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub digest: String,
}

impl ProgramBundleReference {
    /// Validates the bounded bundle reference fields.
    ///
    /// # Errors
    ///
    /// Returns an error when a required field is empty or exceeds its bound.
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        non_empty_within(
            &self.config_map_name,
            "program.configMapName",
            MAX_IDENTIFIER_BYTES,
        )?;
        non_empty_within(&self.key, "program.key", MAX_IDENTIFIER_BYTES)?;
        raw_sha256(&self.digest, "program.digest")
    }
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "demo.oxideloop.dev",
    version = "v1alpha1",
    kind = "WebApplication",
    plural = "webapplications",
    namespaced,
    status = "WebApplicationStatus",
    shortname = "olwa"
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct WebApplicationSpec {
    #[schemars(length(min = 1, max = 1024))]
    pub message: String,
    pub expose: bool,
    pub program: ProgramBundleReference,
}

impl WebApplicationSpec {
    /// Validates the bounded authored root specification.
    ///
    /// # Errors
    ///
    /// Returns an error when a required field is empty or exceeds its bound.
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        non_empty_within(&self.message, "message", MAX_MESSAGE_BYTES)?;
        self.program.validate()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebApplicationStatus {
    /// Controller-issued token proving the exact status persist operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub last_status_persist_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub verified_bundle_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub input_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateLink>,
    /// Durable proof that the controller observed all owned children absent
    /// before removing the internal state finalizer during root deletion.
    ///
    /// This intentionally binds the deletion's start generation, rather than
    /// the current root generation: metadata-only root updates are permitted
    /// while deletion converges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_cleanup_receipt: Option<DeletionCleanupReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 6))]
    pub conditions: Vec<RootCondition>,
}

/// Status-only deletion cleanup receipt.  It is deliberately small and
/// bounded because it is retained by the authored root after the internal
/// journal may already have disappeared.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeletionCleanupReceipt {
    #[schemars(length(min = 1, max = 253))]
    pub root_uid: String,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub deletion_id: String,
    pub deletion_start_generation: i64,
    #[schemars(length(min = 1, max = 63))]
    pub state_name: String,
    #[schemars(length(min = 1, max = 253))]
    pub state_uid: String,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub cleanup_token: String,
}

impl DeletionCleanupReceipt {
    /// Validates the durable, status-only deletion receipt.
    ///
    /// # Errors
    ///
    /// Returns an error when a receipt is malformed or cannot be correlated
    /// with its immutable deletion identity.
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        non_empty_within(
            &self.root_uid,
            "deletionCleanupReceipt.rootUid",
            MAX_IDENTIFIER_BYTES,
        )?;
        raw_sha256(&self.deletion_id, "deletionCleanupReceipt.deletionId")?;
        if self.deletion_start_generation < 0 {
            return Err(SpecValidationError::InvalidRootGeneration);
        }
        non_empty_within(&self.state_name, "deletionCleanupReceipt.stateName", 63)?;
        non_empty_within(
            &self.state_uid,
            "deletionCleanupReceipt.stateUid",
            MAX_IDENTIFIER_BYTES,
        )?;
        raw_sha256(&self.cleanup_token, "deletionCleanupReceipt.cleanupToken")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateLink {
    #[schemars(length(min = 1, max = 253))]
    pub root_uid: String,
    #[schemars(length(min = 1, max = 63))]
    pub name: String,
    #[schemars(length(min = 1, max = 253))]
    pub uid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootCondition {
    #[serde(rename = "type")]
    pub type_: RootConditionType,
    pub status: ConditionStatus,
    #[schemars(length(max = 253))]
    pub reason: String,
    #[schemars(length(max = 1_024))]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum RootConditionType {
    BootstrapReady,
    RendererAvailable,
    DependenciesReady,
    Ready,
    Hold,
    PrunePending,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

/// Controller-owned durable journal. Only `KeyedRootCoordinator` may write it.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "runtime.oxideloop.dev",
    version = "v1alpha1",
    kind = "OxideLoopState",
    plural = "oxideloopstates",
    namespaced,
    status = "OxideLoopStateStatus",
    shortname = "ols",
    validation = "self.spec.stateVersion == 1",
    validation = "self.spec.root == oldSelf.spec.root"
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OxideLoopStateSpec {
    /// Version gate for this durable state; unknown versions are rejected.
    pub state_version: u16,
    pub root: RootBinding,
    pub state: StateBody,
}

impl OxideLoopStateSpec {
    /// Validates a state record before it is read or persisted by the controller.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown state version or invalid immutable root binding.
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        validate_state_version(self.state_version)?;
        self.root.validate()?;
        self.state
            .validate(&self.root.uid, &self.root.bundle_digest)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootBinding {
    #[schemars(length(min = 1, max = 253))]
    pub uid: String,
    #[schemars(length(min = 1, max = 64))]
    pub bootstrap_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub bundle_digest: String,
}

impl RootBinding {
    /// Validates the immutable root identity and bootstrap hash relationship.
    ///
    /// # Errors
    ///
    /// Returns an error when a marker is invalid or the bootstrap hash mismatches.
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        non_empty_within(&self.uid, "root.uid", MAX_IDENTIFIER_BYTES)?;
        non_empty_within(&self.bootstrap_id, "root.bootstrapId", 64)?;
        non_empty_within(&self.bundle_digest, "root.bundleDigest", MAX_DIGEST_BYTES)?;
        if bootstrap_id(&self.uid)? != self.bootstrap_id {
            return Err(SpecValidationError::RootBootstrapMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OxideLoopStateStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub root_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 128))]
    pub committed_bundle_digest: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateBody {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub committed_dependencies: Vec<DependencySnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub pending_dependencies: Vec<DependencySnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub inventory: Vec<InventoryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_transaction: Option<ApplyTransaction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune_candidate: Option<PruneCandidate>,
    /// The exact optimistic-concurrency write currently awaiting its API
    /// acknowledgement.  It is durable so an acknowledgement cannot be
    /// confused with a later write after a process restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_persist: Option<PersistOperation>,
    /// Deletion is a durable transaction, rather than an in-memory mode bit.
    /// This records that normal reconciliation has been preempted before any
    /// cleanup effect may be issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion: Option<DeletionTransaction>,
    /// Persistence-only handoff for an external-effect acknowledgement whose
    /// original root pins advanced before the acknowledgement facts became
    /// durable. It deliberately contains no effect payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_advance_closure: Option<GenerationAdvanceClosure>,
    #[serde(default)]
    pub automation: AutomationState,
}

impl StateBody {
    fn validate(
        &self,
        root_uid: &str,
        root_bundle_digest: &str,
    ) -> Result<(), SpecValidationError> {
        bounded_len(self.committed_dependencies.len(), "committedDependencies")?;
        bounded_len(self.pending_dependencies.len(), "pendingDependencies")?;
        bounded_len(self.inventory.len(), "inventory")?;
        // A prepare record legitimately holds the same snapshot in both
        // sets: committed is the last completed render, pending is the exact
        // candidate being persisted. Each set is unique independently.
        validate_dependency_set(&self.committed_dependencies)?;
        validate_dependency_set(&self.pending_dependencies)?;
        let mut inventory_keys = std::collections::BTreeSet::new();
        for inventory in &self.inventory {
            inventory.validate()?;
            if !inventory_keys.insert(inventory.identity.to_string()) {
                return Err(SpecValidationError::Correlation {
                    field: "inventory identity uniqueness",
                });
            }
        }
        if let Some(transaction) = &self.apply_transaction {
            transaction.validate(root_uid)?;
        }
        if let Some(candidate) = &self.prune_candidate {
            candidate.validate(root_bundle_digest)?;
        }
        if let Some(persist) = &self.pending_persist {
            persist.validate()?;
        }
        if let Some(deletion) = &self.deletion {
            deletion.validate()?;
        }
        if let Some(closure) = &self.generation_advance_closure {
            closure.validate(root_uid)?;
            if !self
                .pending_persist
                .as_ref()
                .is_some_and(|persist| persist.purpose == PersistPurpose::GenerationAdvanceClosure)
            {
                return Err(SpecValidationError::Correlation {
                    field: "generationAdvanceClosure pending persist purpose",
                });
            }
        }
        self.automation.validate()?;
        Ok(())
    }
}

fn validate_dependency_set(dependencies: &[DependencySnapshot]) -> Result<(), SpecValidationError> {
    let mut keys = std::collections::BTreeSet::new();
    for dependency in dependencies {
        dependency.validate()?;
        let key = format!(
            "{:?}\0{}\0{}\0{}\0{}",
            dependency.type_,
            dependency.identity,
            dependency.selector.as_deref().unwrap_or_default(),
            dependency
                .collection_revision
                .as_deref()
                .unwrap_or_default(),
            dependency.observation_token,
        );
        if !keys.insert(key) {
            return Err(SpecValidationError::Correlation {
                field: "dependency uniqueness",
            });
        }
    }
    Ok(())
}

/// A write operation which is safe to acknowledge exactly once.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersistOperation {
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub token: String,
    pub purpose: PersistPurpose,
    pub phase: DurablePhase,
    #[schemars(length(min = 1, max = 253))]
    pub old_state_resource_version: String,
    pub root_generation: i64,
}

impl PersistOperation {
    fn validate(&self) -> Result<(), SpecValidationError> {
        raw_sha256(&self.token, "pendingPersist.token")?;
        non_empty_within(
            &self.old_state_resource_version,
            "pendingPersist.oldStateResourceVersion",
            MAX_IDENTIFIER_BYTES,
        )?;
        if self.root_generation < 0 {
            return Err(SpecValidationError::InvalidRootGeneration);
        }
        if self.phase != self.purpose.durable_phase() {
            return Err(SpecValidationError::PersistPurposePhaseMismatch);
        }
        Ok(())
    }
}

/// Durable protocol phase attached to a [`PersistOperation`].
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum DurablePhase {
    Prepare,
    Invalidate,
    MutationResult,
    Completion,
    AutomationBaseline,
    AutomationPending,
    AutomationCompletion,
    DeletionStart,
    DeletionPrepare,
    DeletionMutationResult,
    DeletionAutomationCleanup,
    GenerationAdvanceClosure,
}

/// Why a durable state write is being made.  This is deliberately separate
/// from the in-memory reducer phase so adapters can reject stale acks without
/// interpreting implementation details.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum PersistPurpose {
    Prepare,
    Invalidate,
    MutationResult,
    Completion,
    AutomationBaseline,
    AutomationPending,
    AutomationCompletion,
    DeletionStart,
    DeletionPrepare,
    DeletionMutationResult,
    DeletionAutomationCleanup,
    GenerationAdvanceClosure,
}

impl PersistPurpose {
    /// The operation purpose is an authenticated part of the durable protocol,
    /// not an adapter hint. A purpose may acknowledge only this exact phase.
    #[must_use]
    pub const fn durable_phase(self) -> DurablePhase {
        match self {
            Self::Prepare => DurablePhase::Prepare,
            Self::Invalidate => DurablePhase::Invalidate,
            Self::MutationResult => DurablePhase::MutationResult,
            Self::Completion => DurablePhase::Completion,
            Self::AutomationBaseline => DurablePhase::AutomationBaseline,
            Self::AutomationPending => DurablePhase::AutomationPending,
            Self::AutomationCompletion => DurablePhase::AutomationCompletion,
            Self::DeletionStart => DurablePhase::DeletionStart,
            Self::DeletionPrepare => DurablePhase::DeletionPrepare,
            Self::DeletionMutationResult => DurablePhase::DeletionMutationResult,
            Self::DeletionAutomationCleanup => DurablePhase::DeletionAutomationCleanup,
            Self::GenerationAdvanceClosure => DurablePhase::GenerationAdvanceClosure,
        }
    }
}

/// Durable deletion transaction.  Conflict or failed cleanup never advances
/// finalization; the controller remains in this record for inspection/retry.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeletionTransaction {
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub id: String,
    /// Immutable generation observed when deletion began.  Subsequent
    /// same-UID root updates must not retarget deletion effects.
    pub deletion_start_generation: i64,
}

impl DeletionTransaction {
    fn validate(&self) -> Result<(), SpecValidationError> {
        raw_sha256(&self.id, "deletion.id")?;
        if self.deletion_start_generation < 0 {
            return Err(SpecValidationError::InvalidRootGeneration);
        }
        Ok(())
    }
}

/// The accepted-persist class that is being durably rebased across a root
/// generation advance.  These are exactly the classes whose acknowledged
/// facts may be retained without reissuing an external effect.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum GenerationAdvanceClosurePurpose {
    MutationResult,
    AutomationPending,
    AutomationCompletion,
    DeletionMutationResult,
    DeletionAutomationCleanup,
}

impl GenerationAdvanceClosurePurpose {
    /// Returns the one durable persistence purpose this closure may preserve.
    #[must_use]
    pub const fn persist_purpose(self) -> PersistPurpose {
        match self {
            Self::MutationResult => PersistPurpose::MutationResult,
            Self::AutomationPending => PersistPurpose::AutomationPending,
            Self::AutomationCompletion => PersistPurpose::AutomationCompletion,
            Self::DeletionMutationResult => PersistPurpose::DeletionMutationResult,
            Self::DeletionAutomationCleanup => PersistPurpose::DeletionAutomationCleanup,
        }
    }
}

/// Exact durable proof that acknowledgement facts were accepted under a
/// previous root snapshot and must be persisted under the current pins.
///
/// The closure has no command payload.  It is a persistence-only handoff, so
/// recovery can never replay the old mutation or automation effect.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerationAdvanceClosure {
    #[schemars(length(min = 1, max = 253))]
    pub root_uid: String,
    pub source_generation: i64,
    #[schemars(length(min = 1, max = 253))]
    pub source_root_resource_version: String,
    #[schemars(length(min = 1, max = 253))]
    pub source_state_uid: String,
    #[schemars(length(min = 1, max = 253))]
    pub source_state_resource_version: String,
    #[schemars(range(min = 0, max = 4))]
    pub rebase_count: u8,
    pub purpose: GenerationAdvanceClosurePurpose,
}

impl GenerationAdvanceClosure {
    fn validate(&self, root_uid: &str) -> Result<(), SpecValidationError> {
        non_empty_within(
            &self.root_uid,
            "generationAdvanceClosure.rootUid",
            MAX_IDENTIFIER_BYTES,
        )?;
        if self.root_uid != root_uid {
            return Err(SpecValidationError::Correlation {
                field: "generationAdvanceClosure root UID",
            });
        }
        if self.source_generation < 0 {
            return Err(SpecValidationError::InvalidRootGeneration);
        }
        for (field, value) in [
            (
                "generationAdvanceClosure.sourceRootResourceVersion",
                self.source_root_resource_version.as_str(),
            ),
            (
                "generationAdvanceClosure.sourceStateUid",
                self.source_state_uid.as_str(),
            ),
            (
                "generationAdvanceClosure.sourceStateResourceVersion",
                self.source_state_resource_version.as_str(),
            ),
        ] {
            non_empty_within(value, field, MAX_IDENTIFIER_BYTES)?;
        }
        if self.rebase_count > MAX_CLOSURE_REBASES {
            return Err(SpecValidationError::Correlation {
                field: "generationAdvanceClosure rebase bound",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DependencySnapshot {
    #[serde(rename = "type")]
    pub type_: DependencySnapshotType,
    #[schemars(length(min = 1, max = 253))]
    pub observation_token: String,
    pub identity: ObjectIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub resource_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1_024))]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub collection_revision: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum DependencySnapshotType {
    ObjectAbsent,
    ObjectPresent,
    Collection,
}

impl DependencySnapshot {
    fn validate(&self) -> Result<(), SpecValidationError> {
        self.identity.validate()?;
        non_empty_within(
            &self.observation_token,
            "dependency.observationToken",
            MAX_IDENTIFIER_BYTES,
        )?;
        match self.type_ {
            DependencySnapshotType::ObjectAbsent
                if self.uid.is_none()
                    && self.resource_version.is_none()
                    && self.selector.is_none()
                    && self.collection_revision.is_none() =>
            {
                Ok(())
            }
            DependencySnapshotType::ObjectPresent
                if self.selector.is_none() && self.collection_revision.is_none() =>
            {
                non_empty_within(
                    self.uid.as_deref().unwrap_or_default(),
                    "dependency.uid",
                    MAX_IDENTIFIER_BYTES,
                )?;
                non_empty_within(
                    self.resource_version.as_deref().unwrap_or_default(),
                    "dependency.resourceVersion",
                    MAX_IDENTIFIER_BYTES,
                )
            }
            DependencySnapshotType::Collection
                if self.uid.is_none() && self.resource_version.is_none() =>
            {
                // An empty selector is a meaningful renderer declaration: it
                // selects the full collection.  The ABI accepts it, so the
                // durable journal must preserve it rather than rejecting a
                // replayable render result.
                let selector = self.selector.as_deref().ok_or(SpecValidationError::Empty {
                    field: "dependency.selector",
                })?;
                if selector.len() > MAX_MESSAGE_BYTES {
                    return Err(SpecValidationError::TooLong {
                        field: "dependency.selector",
                        maximum: MAX_MESSAGE_BYTES,
                    });
                }
                non_empty_within(
                    self.collection_revision.as_deref().unwrap_or_default(),
                    "dependency.collectionRevision",
                    MAX_IDENTIFIER_BYTES,
                )
            }
            _ => Err(SpecValidationError::Empty {
                field: "dependency snapshot combination",
            }),
        }
    }
}

/// Exact Kubernetes object identity; no abbreviated or inferred GVK is stored.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectIdentity {
    #[schemars(length(min = 1, max = 253))]
    pub api_version: String,
    #[schemars(length(min = 1, max = 253))]
    pub kind: String,
    #[schemars(length(min = 1, max = 63))]
    pub namespace: String,
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
}

impl ObjectIdentity {
    fn validate(&self) -> Result<(), SpecValidationError> {
        non_empty_within(
            &self.api_version,
            "identity.apiVersion",
            MAX_IDENTIFIER_BYTES,
        )?;
        non_empty_within(&self.kind, "identity.kind", MAX_IDENTIFIER_BYTES)?;
        non_empty_within(&self.namespace, "identity.namespace", 63)?;
        non_empty_within(&self.name, "identity.name", MAX_IDENTIFIER_BYTES)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InventoryEntry {
    pub identity: ObjectIdentity,
    #[schemars(length(min = 1, max = 253))]
    pub logical_id: String,
    #[schemars(length(min = 1, max = 253))]
    pub uid: String,
    #[schemars(length(min = 1, max = 253))]
    pub resource_version: String,
    #[schemars(length(min = 1, max = 128))]
    pub bundle_digest: String,
}

impl InventoryEntry {
    fn validate(&self) -> Result<(), SpecValidationError> {
        self.identity.validate()?;
        non_empty_within(
            &self.logical_id,
            "inventory.logicalId",
            MAX_IDENTIFIER_BYTES,
        )?;
        non_empty_within(&self.uid, "inventory.uid", MAX_IDENTIFIER_BYTES)?;
        non_empty_within(
            &self.resource_version,
            "inventory.resourceVersion",
            MAX_IDENTIFIER_BYTES,
        )?;
        non_empty_within(
            &self.bundle_digest,
            "inventory.bundleDigest",
            MAX_DIGEST_BYTES,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplyTransaction {
    #[schemars(length(min = 1, max = 128))]
    pub id: String,
    pub generation: i64,
    /// Hash of the canonical renderer input used to prepare this transaction.
    /// The desired object bytes are deliberately not persisted in this journal.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub frozen_render_input_hash: String,
    /// Hash of the desired graph prepared from `frozenRenderInputHash`.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub desired_graph_hash: String,
    /// Declares the only safe restart behavior when desired bytes are absent.
    pub recovery_policy: ApplyRecoveryPolicy,
    /// Exact token attached to the one issued effect batch.  It is persisted
    /// with the prepared intents and changes whenever the transaction is
    /// replanned against a different state resource version.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub attempt_token: String,
    #[schemars(length(max = 1024))]
    pub prepared_intents: Vec<ApplyIntent>,
    /// Durable, root-bound recovery claims. A controller may recover an
    /// unpersisted API success only when a fresh observation returns this exact
    /// `(root UID, transaction ID, command ID)` proof.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub recovery_claims: Vec<PreparedRecoveryClaim>,
    #[schemars(length(max = 1024))]
    pub results: Vec<ApplyResult>,
    #[schemars(length(max = 1024))]
    pub unresolved_identities: Vec<ObjectIdentity>,
}

impl ApplyTransaction {
    #[allow(clippy::too_many_lines)]
    fn validate(&self, root_uid: &str) -> Result<(), SpecValidationError> {
        non_empty_within(&self.id, "applyTransaction.id", MAX_DIGEST_BYTES)?;
        if self.generation < 0 {
            return Err(SpecValidationError::InvalidRootGeneration);
        }
        raw_sha256(
            &self.frozen_render_input_hash,
            "applyTransaction.frozenRenderInputHash",
        )?;
        raw_sha256(
            &self.desired_graph_hash,
            "applyTransaction.desiredGraphHash",
        )?;
        raw_sha256(&self.attempt_token, "applyTransaction.attemptToken")?;
        bounded_len(
            self.prepared_intents.len(),
            "applyTransaction.preparedIntents",
        )?;
        bounded_len(
            self.recovery_claims.len(),
            "applyTransaction.recoveryClaims",
        )?;
        bounded_len(self.results.len(), "applyTransaction.results")?;
        bounded_len(
            self.unresolved_identities.len(),
            "applyTransaction.unresolvedIdentities",
        )?;
        let mut intent_ids = std::collections::BTreeSet::new();
        let mut intent_identity_keys = std::collections::BTreeSet::new();
        let mut previous_command_id: Option<&str> = None;
        for intent in &self.prepared_intents {
            intent.identity.validate()?;
            non_empty_within(
                &intent.logical_id,
                "applyTransaction.preparedIntents.logicalId",
                MAX_IDENTIFIER_BYTES,
            )?;
            if let Some(uid) = &intent.expected_uid {
                non_empty_within(
                    uid,
                    "applyTransaction.preparedIntents.expectedUid",
                    MAX_IDENTIFIER_BYTES,
                )?;
            }
            if let Some(resource_version) = &intent.expected_resource_version {
                non_empty_within(
                    resource_version,
                    "applyTransaction.preparedIntents.expectedResourceVersion",
                    MAX_IDENTIFIER_BYTES,
                )?;
            }
            let expected_pair_is_valid = matches!(
                (&intent.expected_uid, &intent.expected_resource_version),
                (Some(_), Some(_)) | (None, None)
            );
            if !expected_pair_is_valid {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.preparedIntents expected UID/resourceVersion pair",
                });
            }
            if intent.operation == ApplyOperation::Delete
                && !matches!(
                    (&intent.expected_uid, &intent.expected_resource_version),
                    (Some(_), Some(_))
                )
            {
                return Err(SpecValidationError::Empty {
                    field: "applyTransaction.delete expected UID/resourceVersion",
                });
            }
            raw_sha256(
                &intent.command_id,
                "applyTransaction.preparedIntents.commandId",
            )?;
            if previous_command_id.is_some_and(|previous| previous >= intent.command_id.as_str()) {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.preparedIntents command order",
                });
            }
            previous_command_id = Some(&intent.command_id);
            if !intent_ids.insert(&intent.command_id) {
                return Err(SpecValidationError::Empty {
                    field: "applyTransaction.preparedIntents.commandId uniqueness",
                });
            }
            if !intent_identity_keys.insert(intent.identity.to_string()) {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.preparedIntents identity uniqueness",
                });
            }
        }
        for (index, result) in self.results.iter().enumerate() {
            result.identity.validate()?;
            raw_sha256(&result.command_id, "applyTransaction.results.commandId")?;
            raw_sha256(
                &result.attempt_token,
                "applyTransaction.results.attemptToken",
            )?;
            let Some(intent) = self.prepared_intents.get(index) else {
                return Err(SpecValidationError::Empty {
                    field: "applyTransaction.results command correlation",
                });
            };
            if result.attempt_token != self.attempt_token
                || result.command_id != intent.command_id
                || result.identity != intent.identity
            {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.results issued-prefix correlation",
                });
            }
            validate_apply_result(result, intent)?;
        }
        let mut claims = std::collections::BTreeSet::new();
        for claim in &self.recovery_claims {
            non_empty_within(
                &claim.root_uid,
                "applyTransaction.recoveryClaims.rootUid",
                MAX_IDENTIFIER_BYTES,
            )?;
            raw_sha256(
                &claim.command_id,
                "applyTransaction.recoveryClaims.commandId",
            )?;
            if claim.root_uid != root_uid
                || claim.transaction_id != self.id
                || !intent_ids.contains(&claim.command_id)
                || !claims.insert(&claim.command_id)
            {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.recoveryClaims correlation",
                });
            }
        }
        if claims.len() != intent_ids.len() {
            return Err(SpecValidationError::Correlation {
                field: "applyTransaction.recoveryClaims completeness",
            });
        }
        let mut unresolved = std::collections::BTreeSet::new();
        for identity in &self.unresolved_identities {
            identity.validate()?;
            if !unresolved.insert(identity.to_string()) {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.unresolvedIdentities uniqueness",
                });
            }
        }
        Ok(())
    }
}

fn validate_apply_result(
    result: &ApplyResult,
    intent: &ApplyIntent,
) -> Result<(), SpecValidationError> {
    match intent.operation {
        ApplyOperation::Apply => {
            if result.outcome != ApplyOutcome::Applied {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.results apply outcome",
                });
            }
            let uid = result.uid.as_deref().unwrap_or_default();
            let resource_version = result.resource_version.as_deref().unwrap_or_default();
            non_empty_within(uid, "applyTransaction.results.uid", MAX_IDENTIFIER_BYTES)?;
            non_empty_within(
                resource_version,
                "applyTransaction.results.resourceVersion",
                MAX_IDENTIFIER_BYTES,
            )?;
            if let (Some(expected_uid), Some(expected_rv)) = (
                intent.expected_uid.as_deref(),
                intent.expected_resource_version.as_deref(),
            ) && (uid != expected_uid || resource_version == expected_rv)
            {
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.results SSA UID/resourceVersion",
                });
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
                return Err(SpecValidationError::Correlation {
                    field: "applyTransaction.results delete outcome",
                });
            }
        }
    }
    Ok(())
}

/// A durable, explicit recovery ownership claim for a prepared effect.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedRecoveryClaim {
    #[schemars(length(min = 1, max = 253))]
    pub root_uid: String,
    #[schemars(length(min = 1, max = 128))]
    pub transaction_id: String,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub command_id: String,
}

/// Restart contract for a prepared apply transaction.
///
/// A transaction never resumes an effect from `preparedIntents` alone: after a
/// restart the coordinator must reconstruct the renderer input, compare both
/// frozen hashes, and only then prepare a fresh effectful transaction.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ApplyRecoveryPolicy {
    ReplanAndRevalidateBeforeEffect,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplyIntent {
    /// Stable ID for this intent within a particular transaction.  Results
    /// must echo it; identity alone is not sufficient when a physical name is
    /// deleted and recreated.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub command_id: String,
    pub operation: ApplyOperation,
    pub identity: ObjectIdentity,
    #[schemars(length(min = 1, max = 253))]
    pub logical_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub expected_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub expected_resource_version: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ApplyOperation {
    Apply,
    Delete,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplyResult {
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub command_id: String,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub attempt_token: String,
    pub identity: ObjectIdentity,
    pub outcome: ApplyOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub resource_version: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Deleted,
    AlreadyAbsent,
    Conflict,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PruneCandidate {
    #[schemars(length(min = 1, max = 253))]
    pub complete_key: String,
    pub inventory: InventoryEntry,
    pub first_observation: PruneObservation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_observation: Option<PruneObservation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PruneObservation {
    #[schemars(length(min = 1, max = 253))]
    pub observed_resource_version: String,
    pub observed_generation: i64,
    /// Exact completed final-observation fence which produced this
    /// observation. This is not the global renderer observation token.
    #[schemars(length(min = 1, max = 253))]
    pub barrier_token: String,
}

impl PruneCandidate {
    fn validate(&self, root_bundle_digest: &str) -> Result<(), SpecValidationError> {
        raw_sha256(&self.complete_key, "pruneCandidate.completeKey")?;
        self.inventory.validate()?;
        if self.inventory.bundle_digest != root_bundle_digest {
            return Err(SpecValidationError::Correlation {
                field: "pruneCandidate.inventory bundle digest",
            });
        }
        self.first_observation
            .validate("pruneCandidate.firstObservation")?;
        if let Some(second) = &self.second_observation {
            second.validate("pruneCandidate.secondObservation")?;
            if second.observed_generation != self.first_observation.observed_generation
                || second.observed_resource_version
                    != self.first_observation.observed_resource_version
                || second.barrier_token == self.first_observation.barrier_token
            {
                return Err(SpecValidationError::Correlation {
                    field: "pruneCandidate observation correlation",
                });
            }
        }
        Ok(())
    }
}

impl PruneObservation {
    fn validate(&self, field: &'static str) -> Result<(), SpecValidationError> {
        non_empty_within(&self.observed_resource_version, field, MAX_IDENTIFIER_BYTES)?;
        non_empty_within(&self.barrier_token, field, MAX_IDENTIFIER_BYTES)?;
        if self.observed_generation < 0 {
            return Err(SpecValidationError::InvalidRootGeneration);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub baselines: Vec<AutomationBaseline>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub pending: Vec<AutomationPending>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub completions: Vec<AutomationCompletion>,
}

impl AutomationState {
    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), SpecValidationError> {
        bounded_len(self.baselines.len(), "automation.baselines")?;
        bounded_len(self.pending.len(), "automation.pending")?;
        bounded_len(self.completions.len(), "automation.completions")?;
        let mut baselines = std::collections::BTreeSet::new();
        let mut pending = std::collections::BTreeSet::new();
        for baseline in &self.baselines {
            non_empty_within(
                &baseline.automation_id,
                "automation.baselines.automationId",
                MAX_IDENTIFIER_BYTES,
            )?;
            baseline.source.validate()?;
            non_empty_within(
                &baseline.token,
                "automation.baselines.token",
                MAX_DIGEST_BYTES,
            )?;
            if !baselines.insert(format!("{}\0{}", baseline.automation_id, baseline.source)) {
                return Err(SpecValidationError::Correlation {
                    field: "automation.baselines uniqueness",
                });
            }
        }
        for item in &self.pending {
            if !pending.insert(&item.occurrence_id) {
                return Err(SpecValidationError::Empty {
                    field: "automation.pending occurrence uniqueness",
                });
            }
            for (field, value, maximum) in [
                (
                    "automation.pending.occurrenceId",
                    item.occurrence_id.as_str(),
                    MAX_IDENTIFIER_BYTES,
                ),
                (
                    "automation.pending.automationId",
                    item.automation_id.as_str(),
                    MAX_IDENTIFIER_BYTES,
                ),
                ("automation.pending.runName", item.run_name.as_str(), 63),
                (
                    "automation.pending.runUid",
                    item.run_uid.as_str(),
                    MAX_IDENTIFIER_BYTES,
                ),
                (
                    "automation.pending.runResourceVersion",
                    item.run_resource_version.as_str(),
                    MAX_IDENTIFIER_BYTES,
                ),
                ("automation.pending.specHash", item.spec_hash.as_str(), 64),
            ] {
                non_empty_within(value, field, maximum)?;
            }
            item.source.validate()?;
            raw_sha256(&item.spec_hash, "automation.pending.specHash")?;
        }
        let mut completions = std::collections::BTreeSet::new();
        for completion in &self.completions {
            non_empty_within(
                &completion.occurrence_id,
                "automation.completions.occurrenceId",
                MAX_IDENTIFIER_BYTES,
            )?;
            non_empty_within(
                &completion.completed_resource_version,
                "automation.completions.completedResourceVersion",
                MAX_IDENTIFIER_BYTES,
            )?;
            for (field, value, maximum) in [
                (
                    "automation.completions.runName",
                    completion.run_name.as_str(),
                    63,
                ),
                (
                    "automation.completions.runUid",
                    completion.run_uid.as_str(),
                    MAX_IDENTIFIER_BYTES,
                ),
                (
                    "automation.completions.oldResourceVersion",
                    completion.old_resource_version.as_str(),
                    MAX_IDENTIFIER_BYTES,
                ),
                (
                    "automation.completions.materializationName",
                    completion.materialization_name.as_str(),
                    63,
                ),
                (
                    "automation.completions.materializationUid",
                    completion.materialization_uid.as_str(),
                    MAX_IDENTIFIER_BYTES,
                ),
            ] {
                non_empty_within(value, field, maximum)?;
            }
            if completion.old_resource_version == completion.completed_resource_version
                || completion.run_name == completion.materialization_name
                || completion.run_uid == completion.materialization_uid
                || pending.contains(&completion.occurrence_id)
                || !completions.insert(&completion.occurrence_id)
            {
                return Err(SpecValidationError::Correlation {
                    field: "automation.completions correlation",
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationBaseline {
    #[schemars(length(min = 1, max = 253))]
    pub automation_id: String,
    pub source: ObjectIdentity,
    #[schemars(length(min = 1, max = 128))]
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationPending {
    #[schemars(length(min = 1, max = 253))]
    pub occurrence_id: String,
    #[schemars(length(min = 1, max = 253))]
    pub automation_id: String,
    pub source: ObjectIdentity,
    #[schemars(length(min = 1, max = 63))]
    pub run_name: String,
    #[schemars(length(min = 1, max = 253))]
    pub run_uid: String,
    #[schemars(length(min = 1, max = 253))]
    pub run_resource_version: String,
    /// SHA-256 of the canonical complete immutable `AutomationRunSpec`.
    ///
    /// This field is intentionally required without a migration path: the
    /// v1alpha1 durable layout is supported for fresh clusters only.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub spec_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationCompletion {
    #[schemars(length(min = 1, max = 253))]
    pub occurrence_id: String,
    pub outcome: AutomationCompletionOutcome,
    #[schemars(length(min = 1, max = 63))]
    pub run_name: String,
    #[schemars(length(min = 1, max = 253))]
    pub run_uid: String,
    #[schemars(length(min = 1, max = 253))]
    pub old_resource_version: String,
    #[schemars(length(min = 1, max = 253))]
    pub completed_resource_version: String,
    #[schemars(length(min = 1, max = 63))]
    pub materialization_name: String,
    #[schemars(length(min = 1, max = 253))]
    pub materialization_uid: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum AutomationCompletionOutcome {
    Succeeded,
    Failed,
    DeadLettered,
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "runtime.oxideloop.dev",
    version = "v1alpha1",
    kind = "AutomationRun",
    plural = "automationruns",
    namespaced,
    status = "AutomationRunStatus",
    shortname = "olar",
    validation = "self.spec == oldSelf.spec",
    validation = "self.spec.program.digest == self.spec.bundleDigest"
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationRunSpec {
    #[schemars(length(min = 1, max = 253))]
    pub occurrence_id: String,
    #[schemars(length(min = 1, max = 253))]
    pub root_uid: String,
    /// The exact object identity in addition to its immutable UID and resource version.
    pub source: ObjectIdentity,
    #[schemars(length(min = 1, max = 253))]
    pub source_uid: String,
    #[schemars(length(min = 1, max = 253))]
    pub source_resource_version: String,
    #[schemars(length(min = 1, max = 253))]
    pub source_transition_id: String,
    #[schemars(length(min = 1, max = 253))]
    pub automation_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub automation_version: String,
    #[schemars(length(min = 64, max = 64))]
    pub bundle_digest: String,
    /// Exact ConfigMap/key/digest authority for fetching the immutable action
    /// program. The digest is required to equal `bundleDigest`.
    pub program: ProgramBundleReference,
    #[schemars(length(min = 1, max = 128))]
    pub action_module_identity: String,
    #[schemars(length(min = 64, max = 64))]
    pub action_module_digest: String,
    /// ABI version pinned independently of the replay payload for indexing and admission.
    #[schemars(length(min = 1, max = 128))]
    pub renderer_abi_version: String,
    /// Exact canonical UTF-8 JSON bytes sent to the action renderer.
    ///
    /// Kubernetes applies this 512 KiB limit by Unicode code points, while the
    /// controller applies the same limit to UTF-8 bytes before use. The schema
    /// can admit a multibyte string above the protocol byte cap; runtime
    /// validation rejects it before it can reach the action renderer.
    #[schemars(length(min = 2, max = 524_288))]
    pub action_input_v0: String,
    /// Lowercase SHA-256 of `actionInputV0` canonical bytes.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub action_input_hash: String,
    /// Lowercase SHA-256 of canonical `ActionInputV0.eventMetadata` bytes.
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub event_metadata_hash: String,
}

impl AutomationRunSpec {
    /// Validates the immutable replay record and proves its indexed pins match
    /// the exact canonical [`ActionInputV0`](oxideloop_renderer_abi::ActionInputV0).
    ///
    /// # Errors
    ///
    /// Returns an error when the payload exceeds the Action protocol cap, is
    /// not canonical strict ABI JSON, or has been retargeted relative to its
    /// immutable indexed fields.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        for (field, value, maximum) in [
            (
                "occurrenceId",
                self.occurrence_id.as_str(),
                MAX_IDENTIFIER_BYTES,
            ),
            ("rootUid", self.root_uid.as_str(), MAX_IDENTIFIER_BYTES),
            ("sourceUid", self.source_uid.as_str(), MAX_IDENTIFIER_BYTES),
            (
                "sourceResourceVersion",
                self.source_resource_version.as_str(),
                MAX_IDENTIFIER_BYTES,
            ),
            (
                "sourceTransitionId",
                self.source_transition_id.as_str(),
                MAX_IDENTIFIER_BYTES,
            ),
            (
                "automationId",
                self.automation_id.as_str(),
                MAX_IDENTIFIER_BYTES,
            ),
            (
                "automationVersion",
                self.automation_version.as_str(),
                MAX_DIGEST_BYTES,
            ),
            (
                "bundleDigest",
                self.bundle_digest.as_str(),
                MAX_DIGEST_BYTES,
            ),
            (
                "actionModuleIdentity",
                self.action_module_identity.as_str(),
                MAX_DIGEST_BYTES,
            ),
            (
                "actionModuleDigest",
                self.action_module_digest.as_str(),
                MAX_DIGEST_BYTES,
            ),
            (
                "rendererAbiVersion",
                self.renderer_abi_version.as_str(),
                MAX_DIGEST_BYTES,
            ),
            (
                "actionInputHash",
                self.action_input_hash.as_str(),
                MAX_DIGEST_BYTES,
            ),
            (
                "eventMetadataHash",
                self.event_metadata_hash.as_str(),
                MAX_DIGEST_BYTES,
            ),
        ] {
            non_empty_within(value, field, maximum)?;
        }
        raw_sha256(&self.action_input_hash, "actionInputHash")?;
        raw_sha256(&self.event_metadata_hash, "eventMetadataHash")?;
        self.source.validate()?;
        self.program.validate()?;
        if self.program.digest != self.bundle_digest {
            return Err(SpecValidationError::AutomationRunProgramDigestMismatch);
        }
        if self.action_input_v0.len() > MAX_ACTION_INPUT_CANONICAL_BYTES {
            return Err(SpecValidationError::TooLong {
                field: "actionInputV0",
                maximum: MAX_ACTION_INPUT_CANONICAL_BYTES,
            });
        }

        let validated = oxideloop_renderer_abi::parse_validate_action_input(&self.action_input_v0)
            .map_err(|error| SpecValidationError::InvalidActionInput(error.to_string()))?;
        if validated.canonical_bytes != self.action_input_v0.as_bytes() {
            return Err(SpecValidationError::NonCanonicalActionInput);
        }
        if validated.sha256 != self.action_input_hash {
            return Err(SpecValidationError::ActionInputHashMismatch);
        }
        let input = validated.document;
        let event_metadata_hash = oxideloop_renderer_abi::canonical_bytes(&input.event_metadata)
            .map_err(|error| SpecValidationError::InvalidActionInput(error.to_string()))
            .map(|bytes| oxideloop_renderer_abi::sha256_hex(&bytes))?;
        if event_metadata_hash != self.event_metadata_hash {
            return Err(SpecValidationError::EventMetadataHashMismatch);
        }
        if input.abi_version != self.renderer_abi_version
            || input.bundle_digest != self.bundle_digest
            || input.automation_identity != self.automation_id
            || input.automation_version != self.automation_version
            || input.action_module_identity != self.action_module_identity
            || input.action_module_digest != self.action_module_digest
            || input.occurrence_id != self.occurrence_id
            || input.source_transition_id != self.source_transition_id
            || input.source_uid != self.source_uid
            || input.source_resource_version != self.source_resource_version
        {
            return Err(SpecValidationError::ActionInputPinMismatch);
        }
        Ok(())
    }

    /// Admission code must call this when an existing run is updated.
    ///
    /// # Errors
    ///
    /// Returns an error when any immutable authored field has changed.
    pub fn validate_immutable_against(&self, previous: &Self) -> Result<(), SpecValidationError> {
        if self != previous {
            return Err(SpecValidationError::ImmutableAutomationRunSpec);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationRunStatus {
    #[serde(default)]
    pub attempts: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 63))]
    pub materialization_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 253))]
    pub materialization_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<AutomationRunCompletion>,
}

impl AutomationRunStatus {
    /// Validates the status-only acknowledgement boundary used by the
    /// controller before it removes a durable pending occurrence.
    ///
    /// # Errors
    ///
    /// Returns an error when the materialized object cannot be proven as a
    /// complete `(name, UID)` pair.
    pub fn validate(&self) -> Result<(), SpecValidationError> {
        match (&self.materialization_name, &self.materialization_uid) {
            (None, None) => Ok(()),
            (Some(name), Some(uid)) => {
                non_empty_within(name, "automationRun.status.materializationName", 63)?;
                non_empty_within(
                    uid,
                    "automationRun.status.materializationUid",
                    MAX_IDENTIFIER_BYTES,
                )
            }
            _ => Err(SpecValidationError::InvalidMaterializationPair),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutomationRunCompletion {
    pub state: AutomationRunCompletionState,
    #[schemars(length(max = 1_024))]
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum AutomationRunCompletionState {
    Succeeded,
    Failed,
    DeadLettered,
}

/// Rejects unknown durable state layouts before any controller mutation occurs.
///
/// # Errors
///
/// Returns an error for a state version this binary does not understand.
pub fn validate_state_version(version: u16) -> Result<(), SpecValidationError> {
    if version == STATE_VERSION_V1 {
        Ok(())
    } else {
        Err(SpecValidationError::UnsupportedStateVersion {
            actual: version,
            supported: STATE_VERSION_V1,
        })
    }
}

/// Validates the two persisted sides of the root/state UID relationship.
///
/// # Errors
///
/// Returns an error when the root UID, state name, state UID, or bootstrap link
/// disagrees between the two resources.
pub fn validate_root_state_link(
    root_uid: &str,
    status: &WebApplicationStatus,
    state_metadata_name: &str,
    state_uid: &str,
    state: &OxideLoopStateSpec,
) -> Result<(), SpecValidationError> {
    state.root.validate()?;
    if state_name(root_uid)? != state_metadata_name {
        return Err(SpecValidationError::StateNameMismatch);
    }
    if state.root.uid != root_uid {
        return Err(SpecValidationError::RootUidMismatch);
    }
    let Some(link) = &status.state else {
        return Err(SpecValidationError::StateIdentityMismatch);
    };
    if link.root_uid != root_uid {
        return Err(SpecValidationError::RootUidMismatch);
    }
    if link.name != state_metadata_name || link.uid != state_uid {
        return Err(SpecValidationError::StateIdentityMismatch);
    }
    Ok(())
}

/// Validates the deterministic metadata name assigned to an automation occurrence.
///
/// # Errors
///
/// Returns an error when the run's metadata name is not derived from its
/// immutable root UID and occurrence ID.
pub fn validate_automation_run_name(
    metadata_name: &str,
    run: &AutomationRunSpec,
) -> Result<(), SpecValidationError> {
    non_empty_within(metadata_name, "metadata.name", 63)?;
    if automation_run_name(&run.root_uid, &run.occurrence_id)? != metadata_name {
        return Err(SpecValidationError::AutomationRunNameMismatch);
    }
    Ok(())
}

/// Builds the controller-owned annotations for a root/state relationship.
///
/// Callers must not copy these values from renderer output: the root UID and
/// bootstrap marker are derived by the controller, and the state link comes
/// from Kubernetes metadata after the state object is created.
///
/// # Errors
///
/// Returns an error when a required identity marker is empty or out of bounds.
pub fn controller_link_annotations(
    root_uid: &str,
    state: &StateLink,
    bundle_digest: &str,
    transaction_id: Option<&str>,
) -> Result<BTreeMap<String, String>, SpecValidationError> {
    non_empty_within(root_uid, "root.uid", MAX_IDENTIFIER_BYTES)?;
    non_empty_within(&state.root_uid, "state.rootUid", MAX_IDENTIFIER_BYTES)?;
    non_empty_within(&state.name, "state.name", 63)?;
    non_empty_within(&state.uid, "state.uid", MAX_IDENTIFIER_BYTES)?;
    non_empty_within(bundle_digest, "bundle.digest", MAX_DIGEST_BYTES)?;
    if state.root_uid != root_uid {
        return Err(SpecValidationError::RootUidMismatch);
    }

    let mut annotations = BTreeMap::from([
        (ANNOTATION_ROOT_UID.into(), root_uid.into()),
        (ANNOTATION_BOOTSTRAP_ID.into(), bootstrap_id(root_uid)?),
        (ANNOTATION_STATE_NAME.into(), state.name.clone()),
        (ANNOTATION_STATE_UID.into(), state.uid.clone()),
        (ANNOTATION_BUNDLE_DIGEST.into(), bundle_digest.into()),
    ]);
    if let Some(transaction_id) = transaction_id {
        non_empty_within(transaction_id, "transaction.id", 128)?;
        annotations.insert(ANNOTATION_TRANSACTION_ID.into(), transaction_id.into());
    }
    Ok(annotations)
}

impl fmt::Display for ObjectIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}/{}",
            self.api_version, self.kind, self.namespace, self.name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ObjectIdentity {
        ObjectIdentity {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "input".into(),
        }
    }

    fn run_spec() -> AutomationRunSpec {
        let input = oxideloop_renderer_abi::ActionInputV0 {
            abi_version: oxideloop_renderer_abi::model::ABI_VERSION_V0.into(),
            bundle_digest: "a".repeat(64),
            automation_identity: "smoke".into(),
            automation_version: "v1".into(),
            action_module_identity: "actions/smoke.mjs".into(),
            action_module_digest: "b".repeat(64),
            occurrence_id: "occurrence-1".into(),
            source_transition_id: "transition-1".into(),
            source_uid: "source-uid".into(),
            source_resource_version: "7".into(),
            event_metadata: serde_json::json!({"reason": "changed"}),
        };
        let value = serde_json::to_value(input).unwrap();
        let action_input_v0 =
            String::from_utf8(oxideloop_renderer_abi::canonical_bytes(&value).unwrap()).unwrap();
        let action_input_hash = oxideloop_renderer_abi::sha256_hex(action_input_v0.as_bytes());
        let event_metadata_hash = oxideloop_renderer_abi::sha256_hex(
            &oxideloop_renderer_abi::canonical_bytes(&serde_json::json!({"reason": "changed"}))
                .unwrap(),
        );
        AutomationRunSpec {
            occurrence_id: "occurrence-1".into(),
            root_uid: "root-uid".into(),
            source: identity(),
            source_uid: "source-uid".into(),
            source_resource_version: "7".into(),
            source_transition_id: "transition-1".into(),
            automation_id: "smoke".into(),
            automation_version: "v1".into(),
            bundle_digest: "a".repeat(64),
            program: ProgramBundleReference {
                config_map_name: "bundle".into(),
                key: "program.mjs".into(),
                digest: "a".repeat(64),
            },
            action_module_identity: "actions/smoke.mjs".into(),
            action_module_digest: "b".repeat(64),
            renderer_abi_version: oxideloop_renderer_abi::model::ABI_VERSION_V0.into(),
            action_input_v0,
            action_input_hash,
            event_metadata_hash,
        }
    }

    #[test]
    fn validation_rejects_unknown_state_versions() {
        assert_eq!(
            validate_state_version(2),
            Err(SpecValidationError::UnsupportedStateVersion {
                actual: 2,
                supported: STATE_VERSION_V1
            })
        );
    }

    #[test]
    fn web_application_bounds_are_enforced() {
        let spec = WebApplicationSpec {
            message: "x".repeat(MAX_MESSAGE_BYTES + 1),
            expose: false,
            program: ProgramBundleReference {
                config_map_name: "bundle".into(),
                key: "program.mjs".into(),
                digest: "sha256:x".into(),
            },
        };
        assert!(matches!(
            spec.validate(),
            Err(SpecValidationError::TooLong {
                field: "message",
                ..
            })
        ));
    }

    #[test]
    fn program_bundle_digest_is_an_exact_raw_sha256_pin() {
        let mut program = ProgramBundleReference {
            config_map_name: "bundle".into(),
            key: "program.mjs".into(),
            digest: "a".repeat(64),
        };
        assert!(program.validate().is_ok());
        program.digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            program.validate(),
            Err(SpecValidationError::InvalidSha256 {
                field: "program.digest"
            })
        );
    }

    #[test]
    fn root_and_state_uid_pair_must_match() {
        let root_uid = "root-uid";
        let expected_state_name = state_name(root_uid).unwrap();
        let state = OxideLoopStateSpec {
            state_version: STATE_VERSION_V1,
            root: RootBinding {
                uid: root_uid.into(),
                bootstrap_id: bootstrap_id(root_uid).unwrap(),
                bundle_digest: "sha256:bundle".into(),
            },
            state: StateBody::default(),
        };
        let status = WebApplicationStatus {
            state: Some(StateLink {
                root_uid: root_uid.into(),
                name: expected_state_name.clone(),
                uid: "state-uid".into(),
            }),
            ..WebApplicationStatus::default()
        };
        assert!(
            validate_root_state_link(root_uid, &status, &expected_state_name, "state-uid", &state)
                .is_ok()
        );
        assert_eq!(
            validate_root_state_link(root_uid, &status, &expected_state_name, "other", &state),
            Err(SpecValidationError::StateIdentityMismatch)
        );
        assert_eq!(
            validate_root_state_link(root_uid, &status, "forged-state-name", "state-uid", &state),
            Err(SpecValidationError::StateNameMismatch)
        );
    }

    #[test]
    fn controller_annotations_are_derived_from_the_uid_pair() {
        let link = StateLink {
            root_uid: "root-uid".into(),
            name: "ol-state-example".into(),
            uid: "state-uid".into(),
        };
        let annotations =
            controller_link_annotations("root-uid", &link, "sha256:bundle", Some("tx-1")).unwrap();
        assert_eq!(annotations[ANNOTATION_STATE_NAME], "ol-state-example");
        assert_eq!(annotations[ANNOTATION_TRANSACTION_ID], "tx-1");
        assert_eq!(
            annotations[ANNOTATION_BOOTSTRAP_ID],
            bootstrap_id("root-uid").unwrap()
        );
    }

    #[test]
    fn automation_specs_are_immutable() {
        let original = run_spec();
        assert!(original.validate_immutable_against(&original).is_ok());
        let mut changed = original.clone();
        changed.action_module_digest = "c".repeat(64);
        assert_eq!(
            changed.validate_immutable_against(&original),
            Err(SpecValidationError::ImmutableAutomationRunSpec)
        );
    }

    #[test]
    fn automation_run_program_is_an_exact_bundle_digest_pin() {
        let mut run = run_spec();
        assert!(run.validate().is_ok());
        run.program.digest = "c".repeat(64);
        assert_eq!(
            run.validate(),
            Err(SpecValidationError::AutomationRunProgramDigestMismatch)
        );
    }

    #[test]
    fn automation_run_persists_and_verifies_exact_canonical_action_input() {
        let run = run_spec();
        assert!(run.validate().is_ok());
        let expected_name = automation_run_name(&run.root_uid, &run.occurrence_id).unwrap();
        assert!(validate_automation_run_name(&expected_name, &run).is_ok());

        let mut retargeted = run.clone();
        retargeted.source_resource_version = "8".into();
        assert_eq!(
            retargeted.validate(),
            Err(SpecValidationError::ActionInputPinMismatch)
        );

        let mut metadata_changed = run.clone();
        let mut action_input: serde_json::Value =
            serde_json::from_str(&metadata_changed.action_input_v0).unwrap();
        action_input["eventMetadata"] = serde_json::json!({"reason": "restarted"});
        metadata_changed.action_input_v0 =
            String::from_utf8(oxideloop_renderer_abi::canonical_bytes(&action_input).unwrap())
                .unwrap();
        metadata_changed.action_input_hash =
            oxideloop_renderer_abi::sha256_hex(metadata_changed.action_input_v0.as_bytes());
        assert_eq!(
            metadata_changed.validate(),
            Err(SpecValidationError::EventMetadataHashMismatch)
        );

        let mut non_canonical = run.clone();
        non_canonical.action_input_v0 = non_canonical
            .action_input_v0
            .replace("\"abiVersion\"", "\"zz\":null,\"abiVersion\"");
        assert!(matches!(
            non_canonical.validate(),
            Err(SpecValidationError::InvalidActionInput(_)
                | SpecValidationError::NonCanonicalActionInput)
        ));
        assert_eq!(
            validate_automation_run_name("forged-run-name", &run),
            Err(SpecValidationError::AutomationRunNameMismatch)
        );
    }

    #[test]
    fn action_input_bound_matches_the_action_protocol_cap() {
        assert_eq!(
            MAX_ACTION_INPUT_CANONICAL_BYTES,
            oxideloop_renderer_protocol::phase_cap(oxideloop_renderer_protocol::PhaseV0::Action)
        );
        let mut run = run_spec();
        run.action_input_v0 = "€".repeat((MAX_ACTION_INPUT_CANONICAL_BYTES / 3) + 1);
        assert_eq!(
            run.validate(),
            Err(SpecValidationError::TooLong {
                field: "actionInputV0",
                maximum: MAX_ACTION_INPUT_CANONICAL_BYTES,
            })
        );
    }

    #[test]
    fn defaults_and_serialization_round_trip() {
        let status = AutomationRunStatus::default();
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(
            serde_json::from_str::<AutomationRunStatus>(&json).unwrap(),
            status
        );

        assert_eq!(
            serde_json::from_str::<WebApplicationStatus>("{}").unwrap(),
            WebApplicationStatus::default()
        );

        let materialized = AutomationRunStatus {
            attempts: 1,
            materialization_name: Some("materialized-run".into()),
            materialization_uid: Some("materialized-uid".into()),
            completion: None,
        };
        assert_eq!(
            serde_json::from_str::<AutomationRunStatus>(
                &serde_json::to_string(&materialized).unwrap()
            )
            .unwrap(),
            materialized
        );
    }

    #[test]
    fn prepared_apply_transactions_are_replan_only_after_restart() {
        let transaction = ApplyTransaction {
            id: "transaction-1".into(),
            generation: 7,
            frozen_render_input_hash: "a".repeat(64),
            desired_graph_hash: "b".repeat(64),
            recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
            attempt_token: "c".repeat(64),
            prepared_intents: vec![],
            recovery_claims: vec![],
            results: vec![],
            unresolved_identities: vec![],
        };
        assert!(transaction.validate("root-uid").is_ok());
        assert_eq!(
            serde_json::to_value(&transaction)
                .unwrap()
                .get("recoveryPolicy"),
            Some(&serde_json::json!("ReplanAndRevalidateBeforeEffect"))
        );
        for invalid in [
            "x".repeat(64),
            "A".repeat(64),
            format!("sha256:{}", "a".repeat(64)),
        ] {
            let mut invalid_hash = transaction.clone();
            invalid_hash.frozen_render_input_hash = invalid;
            assert_eq!(
                invalid_hash.validate("root-uid"),
                Err(SpecValidationError::InvalidSha256 {
                    field: "applyTransaction.frozenRenderInputHash"
                })
            );
        }
    }

    #[test]
    fn durable_persist_pairs_lists_and_materialization_pairs_are_rejected() {
        let operation = PersistOperation {
            token: "a".repeat(64),
            purpose: PersistPurpose::Prepare,
            phase: DurablePhase::Completion,
            old_state_resource_version: "7".into(),
            root_generation: 1,
        };
        assert_eq!(
            operation.validate(),
            Err(SpecValidationError::PersistPurposePhaseMismatch)
        );
        assert_eq!(
            AutomationRunStatus {
                attempts: 1,
                materialization_name: Some("event".into()),
                materialization_uid: None,
                completion: None,
            }
            .validate(),
            Err(SpecValidationError::InvalidMaterializationPair)
        );
        let state = StateBody {
            inventory: vec![
                InventoryEntry {
                    identity: identity(),
                    logical_id: "item".into(),
                    uid: "uid".into(),
                    resource_version: "7".into(),
                    bundle_digest: "a".repeat(64),
                };
                MAX_LIST_ENTRIES + 1
            ],
            ..StateBody::default()
        };
        assert_eq!(
            state.validate("root-uid", &"a".repeat(64)),
            Err(SpecValidationError::ListBound { field: "inventory" })
        );
    }

    #[test]
    fn dependency_snapshot_may_be_pending_and_committed_during_prepare() {
        let dependency = DependencySnapshot {
            type_: DependencySnapshotType::ObjectAbsent,
            observation_token: "observation".into(),
            identity: identity(),
            uid: None,
            resource_version: None,
            selector: None,
            collection_revision: None,
        };
        let state = StateBody {
            committed_dependencies: vec![dependency.clone()],
            pending_dependencies: vec![dependency],
            ..StateBody::default()
        };
        assert!(state.validate("root-uid", &"a".repeat(64)).is_ok());
    }

    #[test]
    fn manual_serialized_durable_protocol_attacks_are_rejected() {
        let bundle = "a".repeat(64);
        let mut spec = OxideLoopStateSpec {
            state_version: STATE_VERSION_V1,
            root: RootBinding {
                uid: "root-uid".into(),
                bootstrap_id: bootstrap_id("root-uid").unwrap(),
                bundle_digest: bundle.clone(),
            },
            state: StateBody::default(),
        };
        spec.state.prune_candidate = Some(PruneCandidate {
            complete_key: "b".repeat(64),
            inventory: InventoryEntry {
                identity: identity(),
                logical_id: "object".into(),
                uid: "uid".into(),
                resource_version: "7".into(),
                bundle_digest: bundle.clone(),
            },
            first_observation: PruneObservation {
                observed_resource_version: "7".into(),
                observed_generation: 1,
                barrier_token: "barrier-1".into(),
            },
            second_observation: Some(PruneObservation {
                observed_resource_version: "7".into(),
                observed_generation: 1,
                barrier_token: "barrier-2".into(),
            }),
        });
        assert!(spec.validate().is_ok());

        let mut hostile = serde_json::to_value(&spec).unwrap();
        hostile["state"]["pruneCandidate"]["inventory"]["bundleDigest"] =
            serde_json::json!("foreign-bundle");
        let hostile: OxideLoopStateSpec = serde_json::from_value(hostile).unwrap();
        assert!(matches!(
            hostile.validate(),
            Err(SpecValidationError::Correlation { .. })
        ));

        let command_id = "c".repeat(64);
        let mut result_spec = spec;
        result_spec.state.prune_candidate = None;
        result_spec.state.apply_transaction = Some(ApplyTransaction {
            id: "transaction".into(),
            generation: 1,
            frozen_render_input_hash: "d".repeat(64),
            desired_graph_hash: "e".repeat(64),
            recovery_policy: ApplyRecoveryPolicy::ReplanAndRevalidateBeforeEffect,
            attempt_token: "f".repeat(64),
            prepared_intents: vec![ApplyIntent {
                command_id: command_id.clone(),
                operation: ApplyOperation::Apply,
                identity: identity(),
                logical_id: "object".into(),
                expected_uid: Some("uid".into()),
                expected_resource_version: Some("7".into()),
            }],
            recovery_claims: vec![PreparedRecoveryClaim {
                root_uid: "root-uid".into(),
                transaction_id: "transaction".into(),
                command_id,
            }],
            results: vec![],
            unresolved_identities: vec![],
        });
        let mut hostile = serde_json::to_value(&result_spec).unwrap();
        hostile["state"]["applyTransaction"]["results"] = serde_json::json!([{
            "commandId": "c".repeat(64),
            "attemptToken": "f".repeat(64),
            "identity": {
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "namespace": "default",
                "name": "different"
            },
            "outcome": "Conflict"
        }]);
        let hostile: OxideLoopStateSpec = serde_json::from_value(hostile).unwrap();
        assert!(matches!(
            hostile.validate(),
            Err(SpecValidationError::Correlation { .. })
        ));

        let mut hostile = serde_json::to_value(&result_spec).unwrap();
        hostile["state"]["deletion"] = serde_json::json!({
            "id": "d".repeat(64),
            "deletionStartGeneration": -1
        });
        let hostile: OxideLoopStateSpec = serde_json::from_value(hostile).unwrap();
        assert_eq!(
            hostile.validate(),
            Err(SpecValidationError::InvalidRootGeneration)
        );
    }
}
