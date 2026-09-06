use compiler_core::{DeletionPolicy, PermissionCeiling};
use deploy::{GenerationHistory, ProtectedClaim, RuntimeObservation, TransitionRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeBinding {
    pub domain_id: String,
    pub cluster_binding: String,
    pub generation_id: String,
    pub build_digest: String,
    pub ir_digest: String,
    pub policy_digest: String,
    pub capability_digest: String,
    pub service_account_name: String,
    pub controller_image: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivationStatus {
    Inactive,
    Active,
    Draining,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovedClaim {
    pub claim: ProtectedClaim,
    pub deletion_policy: DeletionPolicy,
    pub allow_create: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivationState {
    pub schema_version: String,
    pub binding: RuntimeBinding,
    pub status: ActivationStatus,
    pub approved_claims: Vec<ApprovedClaim>,
    pub rollout_key_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyState {
    pub schema_version: String,
    pub runtime_ceiling: PermissionCeiling,
    pub cleanup_ceiling: PermissionCeiling,
}

/// UID and resourceVersion come only from the enclosing Kubernetes object.
#[derive(Clone, Debug)]
pub struct Versioned<T> {
    pub value: T,
    pub uid: String,
    pub resource_version: String,
}

/// Constructed only after protected activation and policy validation.
#[derive(Clone, Debug)]
pub struct VerifiedActivation {
    pub(crate) activation: Versioned<ActivationState>,
}
impl VerifiedActivation {
    #[must_use]
    pub fn state(&self) -> &ActivationState {
        &self.activation.value
    }
    #[must_use]
    pub fn uid(&self) -> &str {
        &self.activation.uid
    }
    #[must_use]
    pub fn resource_version(&self) -> &str {
        &self.activation.resource_version
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeCondition {
    /// Bounded machine-readable code, never an exception or input value.
    pub reason_code: String,
    pub resource_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeInventory {
    pub schema_version: String,
    pub binding: RuntimeBinding,
    pub activation_uid: String,
    pub activation_resource_version: String,
    pub observations: Vec<RuntimeObservation>,
    pub conditions: Vec<RuntimeCondition>,
    /// Cooperative diagnostic only; never proof that API requests have drained.
    pub drained: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredTransition {
    pub inputs: crate::TransitionInputs,
    pub capacity: crate::CapacityAdmission,
    /// Complete declaration metadata published before this reservation's registry CAS.
    pub next_review_metadata: coordination::ReviewRef,
    /// Immutable baseline copied from the protected active committed transition.
    pub previous_review_metadata: Option<coordination::ReviewRef>,
    pub record: TransitionRecord,
    pub staged_identity_uids: BTreeMap<String, String>,
    pub staged_content_digests: BTreeMap<String, String>,
    pub runtime_binding: Option<RuntimeBinding>,
    pub staged_trust_policy_digest: Option<String>,
    pub staged_provenance_digest: Option<String>,
    pub request_drain_qualification: Option<RequestDrainQualification>,
    pub proxy_identity: Option<ProxyIdentity>,
    pub upgrade_drain: Option<UpgradeDrainState>,
    /// Immutable predecessor authority captured with the upgrade reservation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_writer_checkpoint: Option<crate::WriterCheckpoint>,
}

impl StoredTransition {
    #[must_use]
    pub fn domain_id(&self) -> &str {
        &self.inputs.registry.generation_history.domain_id
    }
    #[must_use]
    pub fn allocation(&self) -> &deploy::GenerationAllocation {
        &self.inputs.allocation
    }
    #[must_use]
    pub fn plan_digest(&self) -> &str {
        &self.inputs.plan_digest
    }

    #[cfg(feature = "deployer")]
    pub(crate) fn reserved(
        inputs: crate::TransitionInputs,
        record: TransitionRecord,
        capacity: crate::CapacityAdmission,
        next_review_metadata: coordination::ReviewRef,
        previous_review_metadata: Option<coordination::ReviewRef>,
    ) -> Self {
        Self {
            inputs,
            capacity,
            next_review_metadata,
            previous_review_metadata,
            record,
            staged_identity_uids: BTreeMap::new(),
            staged_content_digests: BTreeMap::new(),
            runtime_binding: None,
            staged_trust_policy_digest: None,
            staged_provenance_digest: None,
            request_drain_qualification: None,
            proxy_identity: None,
            upgrade_drain: None,
            previous_writer_checkpoint: None,
        }
    }
}

/// Hydrated registry state. Its claims cannot be serialized into a control object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceRegistry {
    pub registry_uid: String,
    pub next_sequence: u64,
    pub schema_version: String,
    pub cluster_binding: String,
    pub workload_namespace: String,
    pub claims: Vec<ProtectedClaim>,
    pub histories: BTreeMap<String, GenerationHistory>,
    pub active_writer_checkpoints: BTreeMap<String, crate::WriterCheckpoint>,
    pub audit_heads: BTreeMap<String, coordination::AuditRef>,
    pub transitions: BTreeMap<String, StoredTransition>,
    pub(crate) captured_claim_scope: coordination::ClaimScope,
}

/// Protected wire format. Only this DTO may be serialized into registry state.json.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespaceRegistryState {
    pub registry_uid: String,
    #[serde(with = "deploy::allocation_counter")]
    pub next_sequence: u64,
    pub schema_version: String,
    pub cluster_binding: String,
    pub workload_namespace: String,
    pub claim_snapshot: coordination::ClaimRef,
    pub histories: BTreeMap<String, GenerationHistory>,
    #[serde(default)]
    pub active_writer_checkpoints: BTreeMap<String, crate::WriterCheckpoint>,
    pub audit_heads: BTreeMap<String, coordination::AuditRef>,
    pub transitions: BTreeMap<String, StoredTransition>,
}

/// Separately provisioned registry incarnation authority; deployers can only read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistryAuthority {
    pub control_namespace: String,
    pub schema_version: String,
    pub registry_uid: String,
    pub cluster_binding: String,
    pub workload_namespace: String,
}

/// Domain key bytes deliberately have no Debug or Serialize implementation.
pub struct DomainKey {
    pub id: String,
    pub bytes: secrecy::SecretBox<[u8]>,
}

/// Short-lived credential returned by the Kubernetes `TokenRequest` API.
/// No Debug or Serialize implementation can expose its bearer value.
pub struct DeploymentCredential {
    pub token: secrecy::SecretString,
    pub expires_at: String,
    pub service_account_uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestDrainQualification {
    UnqualifiedDirectApi,
    IsolatedEnvoy {
        qualification_digest: String,
        node_image_digest: String,
    },
}

/// Values must come from the protected stage record, including the API-defaulted
/// Pod spec and approved immutable Envoy configuration. Runtime inventory is not
/// an authority for these identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyIdentity {
    pub policy_digest: String,
    pub deployment_name: String,
    pub deployment_uid: String,
    pub node_name: String,
    pub node_uid: String,
    pub pod_name: String,
    pub pod_uid: String,
    pub pod_spec_digest: String,
    pub envoy_container_id: String,
    pub admin_container_id: String,
    pub config_map_name: String,
    pub config_map_uid: String,
    pub config_digest: String,
    pub qualification_digest: String,
}

/// Protected checkpoint populated only by live platform operations. Runtime
/// inventory and caller-provided receipts cannot create these observations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpgradeDrainState {
    pub previous_transition_id: String,
    /// Current tokenless controller Pods stopped for operational hygiene. This
    /// is not a claim to enumerate all historical tokenless runtime processes.
    pub runtime_writers: crate::ExpectedWriters,
    pub proxy_writers: crate::ExpectedWriters,
    pub runtime_termination: Option<crate::ProcessTerminationObservation>,
    pub final_proxy_ledger: Option<crate::ProxyDrainObservation>,
    pub proxy_termination: Option<crate::ProcessTerminationObservation>,
    pub retired_identity_uids: BTreeMap<String, String>,
}

impl NamespaceRegistry {
    /// Look up a recorded transition without assuming an earlier read is current.
    /// # Errors
    /// Returns a controlled rejection when the transition is absent.
    pub fn transition(&self, id: &str) -> crate::Result<&StoredTransition> {
        self.transitions
            .get(id)
            .ok_or_else(|| crate::invalid("transition is absent from registry"))
    }
}
