//! Ordered two-candidate lifecycle qualification on one runner-owned kind target.
use super::{
    CandidateBinding, CandidateQualificationTarget, qualification_cluster_name,
    validate_fresh_qualification_registry, validate_fresh_qualification_request,
};
use crate::{NamespaceRegistry, PlatformClient, Result, Versioned, invalid};
use deploy::Phase;
use release::IntegrityVerifiedCandidate;
use serde::{Deserialize, Serialize};

mod purpose;
mod rejection;
#[cfg(test)]
mod tests;

pub const CANDIDATE_PAIR_QUALIFICATION_SCHEMA: &str = "reakt.dev/candidate-pair-qualification/v2";
pub const CANDIDATE_PAIR_QUALIFICATION_MARKER_NAME: &str = "reakt-candidate-pair-qualification";
const CANDIDATE_PAIR_QUALIFICATION_EXECUTOR: &str = "candidate-pair-qualification";

/// Return the fixed pair-qualification executor identity.
#[must_use]
pub const fn candidate_pair_qualification_executor() -> &'static str {
    CANDIDATE_PAIR_QUALIFICATION_EXECUTOR
}

/// The single lifecycle relation authorized by an immutable qualification marker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CandidatePairPurpose {
    PermissionExpansion {},
    LastTypeCleanup { identity: deploy::ResourceIdentity },
}

/// One immutable pair marker and the two platform-policy identities it binds.
/// The nested marker UID and resourceVersion identify the pair marker, never the
/// single-candidate marker used by reservation and collision qualification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidatePairQualificationTarget {
    pub purpose: CandidatePairPurpose,
    pub cluster: CandidateQualificationTarget,
    pub initial_policy_digest: String,
    pub successor_policy_digest: String,
}

/// Exact ordered candidates, policy decisions and owned-cluster topology.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidatePairQualificationMarker {
    pub purpose: CandidatePairPurpose,
    pub schema_version: String,
    pub run_id: String,
    pub cluster_name: String,
    pub sentinel_namespace: String,
    pub domain_id: String,
    pub cluster_binding: String,
    pub control_namespace: String,
    pub workload_namespace: String,
    pub kube_system_namespace_uid: String,
    pub control_namespace_uid: String,
    pub workload_namespace_uid: String,
    pub sentinel_namespace_uid: String,
    pub node_name: String,
    pub node_uid: String,
    pub node_provider_id: String,
    pub node_image_digest: String,
    pub initial_release_digest: String,
    pub initial_ir_digest: String,
    pub initial_provenance_digest: String,
    pub successor_release_digest: String,
    pub successor_ir_digest: String,
    pub successor_provenance_digest: String,
    pub trust_policy_digest: String,
    pub initial_policy_digest: String,
    pub successor_policy_digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidatePairRole {
    Initial,
    Successor,
}

/// One bounded production lifecycle operation for an exact pair member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidatePairLifecycleOperation {
    StageInitial,
    ActivateInitial,
    StageSuccessor,
    QuiesceUpgrade,
    ActivateUpgrade,
    CommitUpgrade,
}

impl CandidatePairLifecycleOperation {
    const fn role(self) -> CandidatePairRole {
        match self {
            Self::StageInitial | Self::ActivateInitial => CandidatePairRole::Initial,
            Self::StageSuccessor
            | Self::QuiesceUpgrade
            | Self::ActivateUpgrade
            | Self::CommitUpgrade => CandidatePairRole::Successor,
        }
    }
}

/// Unforgeable, ordered candidate references whose exact pair marker was read
/// from the runner-owned cluster. Operations re-read that marker before and
/// after delegating to the ordinary lifecycle implementation.
pub struct CandidateQualificationPair<'a> {
    initial: &'a IntegrityVerifiedCandidate,
    successor: &'a IntegrityVerifiedCandidate,
    target: &'a CandidatePairQualificationTarget,
}

impl CandidateQualificationPair<'_> {
    fn candidate(&self, role: CandidatePairRole) -> &IntegrityVerifiedCandidate {
        match role {
            CandidatePairRole::Initial => self.initial,
            CandidatePairRole::Successor => self.successor,
        }
    }

    fn policy_digest(&self, role: CandidatePairRole) -> &str {
        match role {
            CandidatePairRole::Initial => &self.target.initial_policy_digest,
            CandidatePairRole::Successor => &self.target.successor_policy_digest,
        }
    }
}

impl PlatformClient {
    /// Authenticate one ordered candidate pair against its immutable owned-kind marker.
    /// # Errors
    /// Rejects candidate, scope, trust, permission, policy or target mismatch.
    pub async fn authorize_candidate_qualification_pair<'a>(
        &self,
        role: CandidatePairRole,
        initial: &'a IntegrityVerifiedCandidate,
        successor: &'a IntegrityVerifiedCandidate,
        target: &'a CandidatePairQualificationTarget,
    ) -> Result<CandidateQualificationPair<'a>> {
        let pair = CandidateQualificationPair {
            initial,
            successor,
            target,
        };
        self.verify_candidate_pair(&pair).await?;
        self.verify_pair_policy(role, &pair).await?;
        Ok(pair)
    }

    /// Reserve the pair's exact initial candidate in fresh protected state.
    /// # Errors
    /// Rejects any noninitial request, target drift or protected-state mismatch.
    pub async fn reserve_pair_initial_candidate(
        &self,
        request: &deploy::DeploymentRequest,
        pair: &CandidateQualificationPair<'_>,
        inventory: &dyn inventory::InventoryStore,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.verify_pair_operation(CandidatePairRole::Initial, pair)
            .await?;
        let candidate = pair.initial;
        let binding = self.candidate_binding(request, candidate)?;
        validate_fresh_qualification_request(request)?;
        let current = self.load_registry().await?;
        validate_fresh_qualification_registry(&current, candidate)?;
        self.verify_pair_policy(CandidatePairRole::Initial, pair)
            .await?;
        let reserved = Box::pin(self.reserve_authenticated(
            request,
            CANDIDATE_PAIR_QUALIFICATION_EXECUTOR,
            inventory,
            Some(candidate.trust_policy_digest()),
        ))
        .await;
        self.verify_pair_operation(CandidatePairRole::Initial, pair)
            .await?;
        let reserved = reserved?;
        verify_reserved(
            &reserved,
            &binding,
            request,
            CANDIDATE_PAIR_QUALIFICATION_EXECUTOR,
            None,
        )?;
        Ok(reserved)
    }

    /// Reserve the exact successor from a fully committed exact initial generation.
    /// The predecessor is authenticated from protected allocation, writer,
    /// inventory and review continuity; it is not rebound after policy replacement.
    /// # Errors
    /// Rejects a non-sequence-two upgrade or any predecessor, policy or target drift.
    pub async fn reserve_pair_upgrade_candidate(
        &self,
        request: &deploy::DeploymentRequest,
        pair: &CandidateQualificationPair<'_>,
        inventory: &dyn inventory::InventoryStore,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.verify_pair_operation(CandidatePairRole::Successor, pair)
            .await?;
        let successor_binding = self.candidate_binding(request, pair.successor)?;
        let current = self.validate_pair_predecessor(request, pair).await?;
        purpose::validate_cleanup_plan(&pair.target.purpose, &deploy::plan_transition(request)?)?;
        let initial_review = current
            .value
            .transition(
                &request
                    .previous
                    .as_deref()
                    .ok_or_else(|| {
                        invalid("candidate pair successor reservation requires a predecessor")
                    })?
                    .allocation
                    .transition_id,
            )?
            .next_review_metadata
            .clone();
        self.verify_pair_policy(CandidatePairRole::Successor, pair)
            .await?;
        let reserved = Box::pin(self.reserve_authenticated(
            request,
            CANDIDATE_PAIR_QUALIFICATION_EXECUTOR,
            inventory,
            Some(pair.successor.trust_policy_digest()),
        ))
        .await;
        self.verify_pair_operation(CandidatePairRole::Successor, pair)
            .await?;
        let reserved = reserved?;
        verify_reserved(
            &reserved,
            &successor_binding,
            request,
            CANDIDATE_PAIR_QUALIFICATION_EXECUTOR,
            Some(&initial_review),
        )?;
        if matches!(
            pair.target.purpose,
            CandidatePairPurpose::LastTypeCleanup { .. }
        ) {
            self.pair_transition_session(
                CandidatePairRole::Successor,
                pair,
                inventory,
                &request.transition_id,
            )
            .await?;
            self.verify_pair_operation(CandidatePairRole::Successor, pair)
                .await?;
        }
        Ok(reserved)
    }

    async fn validate_pair_predecessor(
        &self,
        request: &deploy::DeploymentRequest,
        pair: &CandidateQualificationPair<'_>,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let successor_binding = self.candidate_binding(request, pair.successor)?;
        let previous = request.previous.as_deref().ok_or_else(|| {
            invalid("candidate pair successor reservation requires a predecessor")
        })?;
        if request.registry.next_sequence != 2
            || !request.observations.is_empty()
            || !request.ownership_intents.is_empty()
            || previous.allocation.sequence != 1
            || previous.allocation.artifact_digest != pair.initial.release_digest()
            || previous.allocation.ir_digest != pair.initial.compiled().ir_digest
        {
            return Err(invalid(
                "candidate pair successor request differs from the exact initial generation",
            ));
        }
        let current = self.load_registry().await?;
        let initial = current
            .value
            .transition(&previous.allocation.transition_id)?;
        let history = current
            .value
            .histories
            .get(successor_binding.domain_id)
            .ok_or_else(|| invalid("candidate pair active generation history is absent"))?;
        if current.value.next_sequence != 2
            || current.value.transitions.len() != 1
            || initial.record.phase != Phase::Committed
            || initial.record.executor_id != CANDIDATE_PAIR_QUALIFICATION_EXECUTOR
            || initial.allocation() != &previous.allocation
            || history.active_allocation.as_ref() != Some(&previous.allocation)
            || initial.inputs.policy_digest != pair.target.initial_policy_digest
            || initial.staged_trust_policy_digest.as_deref()
                != Some(pair.initial.trust_policy_digest())
            || initial.staged_provenance_digest.as_deref() != Some(pair.initial.provenance_digest())
            || initial.runtime_binding.is_none()
            || initial.previous_review_metadata.is_some()
        {
            return Err(invalid(
                "candidate pair protected predecessor is not the exact committed initial",
            ));
        }
        self.validate_reservation_snapshot(&current, request)?
            .ok_or_else(|| invalid("candidate pair active writer checkpoint is absent"))?;
        let initial_review = initial.next_review_metadata.clone();
        self.read_review_metadata(&initial_review).await?;
        Ok(current)
    }

    /// Execute one bounded lifecycle step through an internally selected exact candidate.
    /// # Errors
    /// Rejects pair target, policy, trust, review, selection or lifecycle drift.
    pub async fn run_pair_lifecycle_operation(
        &self,
        operation: CandidatePairLifecycleOperation,
        pair: &CandidateQualificationPair<'_>,
        inventory: &dyn inventory::InventoryStore,
        transition: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let role = operation.role();
        self.verify_pair_operation(role, pair).await?;
        let session = self
            .pair_transition_session(role, pair, inventory, transition)
            .await;
        self.verify_pair_operation(role, pair).await?;
        let session = session?;
        let candidate = pair.candidate(role);
        let artifact = crate::controller::StagingArtifact {
            compiled: candidate.compiled(),
            image: &candidate.manifest().image.reference,
            release_digest: candidate.release_digest(),
            provenance_digest: candidate.provenance_digest(),
            trust_policy_digest: candidate.trust_policy_digest(),
        };
        let result = match operation {
            CandidatePairLifecycleOperation::StageInitial
            | CandidatePairLifecycleOperation::StageSuccessor => {
                session
                    .stage_proxy_artifact(
                        transition,
                        CANDIDATE_PAIR_QUALIFICATION_EXECUTOR,
                        &artifact,
                    )
                    .await
            }
            CandidatePairLifecycleOperation::ActivateInitial => {
                Box::pin(
                    session.activate_initial(transition, CANDIDATE_PAIR_QUALIFICATION_EXECUTOR),
                )
                .await
            }
            CandidatePairLifecycleOperation::QuiesceUpgrade => {
                session
                    .quiesce_previous(transition, CANDIDATE_PAIR_QUALIFICATION_EXECUTOR)
                    .await
            }
            CandidatePairLifecycleOperation::ActivateUpgrade => {
                session
                    .activate_upgrade(transition, CANDIDATE_PAIR_QUALIFICATION_EXECUTOR)
                    .await
            }
            CandidatePairLifecycleOperation::CommitUpgrade => {
                session
                    .commit_upgrade(transition, CANDIDATE_PAIR_QUALIFICATION_EXECUTOR)
                    .await
            }
        };
        self.verify_pair_operation(role, pair).await?;
        result
    }

    /// Publish committed inventory through an internally selected exact pair member.
    /// # Errors
    /// Rejects pair target, policy, selection, committed-head or inventory drift.
    pub async fn publish_pair_inventory(
        &self,
        role: CandidatePairRole,
        pair: &CandidateQualificationPair<'_>,
        inventory: &dyn inventory::InventoryStore,
        transition: &str,
    ) -> Result<inventory::VersionedInventory> {
        self.verify_pair_operation(role, pair).await?;
        let session = self
            .pair_transition_session(role, pair, inventory, transition)
            .await;
        self.verify_pair_operation(role, pair).await?;
        let session = session?;
        let result = session
            .publish_committed_inventory(
                inventory,
                transition,
                CANDIDATE_PAIR_QUALIFICATION_EXECUTOR,
            )
            .await;
        self.verify_pair_operation(role, pair).await?;
        result
    }

    /// Resolve one exact pair transition for bounded read-only driver observations.
    /// # Errors
    /// Rejects pair target, policy, trust, review, selection or registry drift.
    pub async fn resolve_pair_transition(
        &self,
        role: CandidatePairRole,
        pair: &CandidateQualificationPair<'_>,
        inventory: &dyn inventory::InventoryStore,
        transition: &str,
    ) -> Result<crate::ResolvedTransition> {
        self.verify_pair_operation(role, pair).await?;
        let session = self
            .pair_transition_session(role, pair, inventory, transition)
            .await;
        self.verify_pair_operation(role, pair).await?;
        let session = session?;
        let resolved = session.resolve().await;
        self.verify_pair_operation(role, pair).await?;
        resolved
    }

    async fn pair_transition_session(
        &self,
        role: CandidatePairRole,
        pair: &CandidateQualificationPair<'_>,
        inventory: &dyn inventory::InventoryStore,
        transition: &str,
    ) -> Result<crate::TransitionSession> {
        let candidate = pair.candidate(role);
        let session = self
            .bind_release(
                candidate.compiled(),
                candidate.release_digest(),
                candidate.trust_policy_digest(),
                inventory,
                transition,
            )
            .await?;
        if role == CandidatePairRole::Successor
            && matches!(
                pair.target.purpose,
                CandidatePairPurpose::LastTypeCleanup { .. }
            )
        {
            let resolved = session.resolve().await?;
            purpose::validate_cleanup_plan(&pair.target.purpose, resolved.plan())?;
        }
        Ok(session)
    }

    async fn verify_pair_operation(
        &self,
        role: CandidatePairRole,
        pair: &CandidateQualificationPair<'_>,
    ) -> Result<()> {
        self.verify_candidate_pair(pair).await?;
        self.verify_pair_policy(role, pair).await
    }

    async fn verify_pair_policy(
        &self,
        role: CandidatePairRole,
        pair: &CandidateQualificationPair<'_>,
    ) -> Result<()> {
        self.verify_pair_policy_digest(
            &pair.candidate(role).compiled().ir.domain.id,
            pair.policy_digest(role),
        )
        .await
    }

    async fn verify_pair_policy_digest(&self, domain: &str, expected: &str) -> Result<()> {
        let policy = self.load_policy(domain).await?;
        if compiler_core::digest(&policy.value)? != expected {
            return Err(invalid(
                "candidate pair current platform policy differs from its ordered marker",
            ));
        }
        Ok(())
    }

    async fn verify_candidate_pair(&self, pair: &CandidateQualificationPair<'_>) -> Result<()> {
        let initial = self.candidate_binding_for(pair.initial)?;
        let successor = self.candidate_binding_for(pair.successor)?;
        if initial.domain_id != successor.domain_id
            || initial.cluster_binding != successor.cluster_binding
            || initial.control_namespace != successor.control_namespace
            || initial.workload_namespace != successor.workload_namespace
            || initial.release_digest == successor.release_digest
            || pair.initial.compiled().ir_digest == pair.successor.compiled().ir_digest
            || pair.initial.trust_policy_digest() != pair.successor.trust_policy_digest()
            || !crate::valid_digest(&pair.target.initial_policy_digest)
            || !crate::valid_digest(&pair.target.successor_policy_digest)
        {
            return Err(invalid(
                "candidate pair lacks an exact ordered candidate identity",
            ));
        }
        purpose::validate_relation(
            &pair.target.purpose,
            pair.initial.compiled(),
            pair.successor.compiled(),
            &pair.target.initial_policy_digest,
            &pair.target.successor_policy_digest,
        )?;
        self.verify_release_trust(initial.domain_id, pair.initial.trust_policy_digest())
            .await?;
        let expected = pair_marker(pair, &successor)?;
        self.verify_pair_target(&successor, pair.target, &expected)
            .await
    }

    async fn verify_pair_target(
        &self,
        binding: &CandidateBinding<'_>,
        target: &CandidatePairQualificationTarget,
        expected: &CandidatePairQualificationMarker,
    ) -> Result<()> {
        self.verify_qualification_topology(binding, &target.cluster)
            .await?;
        let marker: Versioned<CandidatePairQualificationMarker> = self
            .read_qualification_marker(&target.cluster, CANDIDATE_PAIR_QUALIFICATION_MARKER_NAME)
            .await?;
        if marker.uid != target.cluster.marker_uid
            || marker.resource_version != target.cluster.marker_resource_version
            || marker.value != *expected
        {
            return Err(invalid(
                "candidate pair marker differs from the exact ordered candidates",
            ));
        }
        Ok(())
    }
}

fn strict_capability_expansion(
    initial: &[compiler_core::Capability],
    successor: &[compiler_core::Capability],
) -> bool {
    initial.len() < successor.len()
        && initial
            .iter()
            .all(|capability| successor.contains(capability))
}

fn pair_marker(
    pair: &CandidateQualificationPair<'_>,
    binding: &CandidateBinding<'_>,
) -> Result<CandidatePairQualificationMarker> {
    let target = &pair.target.cluster;
    let cluster = qualification_cluster_name(&target.run_id)?;
    Ok(CandidatePairQualificationMarker {
        purpose: pair.target.purpose.clone(),
        schema_version: CANDIDATE_PAIR_QUALIFICATION_SCHEMA.into(),
        run_id: target.run_id.clone(),
        cluster_name: cluster.clone(),
        sentinel_namespace: cluster,
        domain_id: binding.domain_id.into(),
        cluster_binding: binding.cluster_binding.into(),
        control_namespace: binding.control_namespace.into(),
        workload_namespace: binding.workload_namespace.into(),
        kube_system_namespace_uid: target.kube_system_namespace_uid.clone(),
        control_namespace_uid: target.control_namespace_uid.clone(),
        workload_namespace_uid: target.workload_namespace_uid.clone(),
        sentinel_namespace_uid: target.sentinel_namespace_uid.clone(),
        node_name: target.node_name.clone(),
        node_uid: target.node_uid.clone(),
        node_provider_id: target.node_provider_id.clone(),
        node_image_digest: target.node_image_digest.clone(),
        initial_release_digest: pair.initial.release_digest().into(),
        initial_ir_digest: pair.initial.compiled().ir_digest.clone(),
        initial_provenance_digest: pair.initial.provenance_digest().into(),
        successor_release_digest: pair.successor.release_digest().into(),
        successor_ir_digest: pair.successor.compiled().ir_digest.clone(),
        successor_provenance_digest: pair.successor.provenance_digest().into(),
        trust_policy_digest: pair.initial.trust_policy_digest().into(),
        initial_policy_digest: pair.target.initial_policy_digest.clone(),
        successor_policy_digest: pair.target.successor_policy_digest.clone(),
    })
}

fn verify_reserved(
    registry: &Versioned<NamespaceRegistry>,
    binding: &CandidateBinding<'_>,
    request: &deploy::DeploymentRequest,
    executor: &str,
    previous_review: Option<&coordination::ReviewRef>,
) -> Result<()> {
    let stored = registry.value.transition(&request.transition_id)?;
    if stored.record.phase != Phase::Planned
        || stored.record.executor_id != executor
        || stored.domain_id() != binding.domain_id
        || stored.allocation().artifact_digest != binding.release_digest
        || stored.allocation().ir_digest != request.next.compiled.ir_digest
        || stored.previous_review_metadata.as_ref() != previous_review
    {
        return Err(invalid(
            "candidate pair reservation result differs from the exact request",
        ));
    }
    Ok(())
}
