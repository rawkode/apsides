//! Exact-candidate qualification on a runner-owned disposable kind cluster.
use crate::{NamespaceRegistry, PlatformClient, Result, Versioned, invalid};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Node};
use kube::Api;
use release::IntegrityVerifiedCandidate;
use serde::{Deserialize, Serialize};
use std::future::Future;

mod pair;
pub use pair::{
    CANDIDATE_PAIR_QUALIFICATION_MARKER_NAME, CANDIDATE_PAIR_QUALIFICATION_SCHEMA,
    CandidatePairLifecycleOperation, CandidatePairPurpose, CandidatePairQualificationMarker,
    CandidatePairQualificationTarget, CandidatePairRole, CandidateQualificationPair,
    candidate_pair_qualification_executor,
};

pub const CANDIDATE_QUALIFICATION_SCHEMA: &str = "apsides.dev/candidate-qualification/v1";
pub const CANDIDATE_QUALIFICATION_MARKER_NAME: &str = "apsides-candidate-qualification";

const KUBE_SYSTEM_NAMESPACE: &str = "kube-system";
const MAX_IDENTITY_BYTES: usize = 256;

/// API identities observed after a host runner created one disposable cluster.
///
/// The node-image digest is a runner attestation bound into the immutable
/// marker; Kubernetes Node data does not establish that image digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateQualificationTarget {
    pub run_id: String,
    pub kube_system_namespace_uid: String,
    pub control_namespace_uid: String,
    pub workload_namespace_uid: String,
    pub sentinel_namespace_uid: String,
    pub marker_uid: String,
    pub marker_resource_version: String,
    pub node_name: String,
    pub node_uid: String,
    pub node_provider_id: String,
    pub node_image_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateQualificationMarker {
    pub schema_version: String,
    pub run_id: String,
    pub cluster_name: String,
    pub sentinel_namespace: String,
    pub domain_id: String,
    pub release_digest: String,
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
}

struct CandidateBinding<'a> {
    domain_id: &'a str,
    release_digest: &'a str,
    cluster_binding: &'a str,
    control_namespace: &'a str,
    workload_namespace: &'a str,
}

fn same_version<T: PartialEq>(observed: &Versioned<T>, expected: &Versioned<T>) -> bool {
    observed.uid == expected.uid
        && observed.resource_version == expected.resource_version
        && observed.value == expected.value
}

fn validate_fresh_qualification_request(request: &deploy::DeploymentRequest) -> Result<()> {
    if request.previous.is_some()
        || !request.observations.is_empty()
        || !request.ownership_intents.is_empty()
        || request.registry.next_sequence != 1
        || !request.registry.claims.is_empty()
        || request
            .registry
            .generation_history
            .active_allocation
            .is_some()
    {
        return Err(invalid(
            "candidate qualification requires an unobserved initial installation",
        ));
    }
    Ok(())
}

fn validate_fresh_qualification_registry(
    current: &Versioned<NamespaceRegistry>,
    candidate: &IntegrityVerifiedCandidate,
) -> Result<()> {
    let domain = &candidate.compiled().ir.domain;
    let history =
        current.value.histories.get(&domain.id).ok_or_else(|| {
            invalid("candidate qualification domain allocation authority is absent")
        })?;
    if current.value.next_sequence != 1
        || !current.value.claims.is_empty()
        || !current.value.transitions.is_empty()
        || !current.value.audit_heads.is_empty()
        || !current.value.active_writer_checkpoints.is_empty()
        || current
            .value
            .histories
            .values()
            .any(|entry| entry.active_allocation.is_some())
        || history.domain_id != domain.id
        || history.cluster_binding != domain.cluster_binding
    {
        return Err(invalid(
            "candidate qualification requires fresh protected allocation state",
        ));
    }
    Ok(())
}

impl PlatformClient {
    /// Reserve an initial transition for one exact signed production candidate
    /// after reobserving its runner-owned disposable kind target.
    ///
    /// This capability neither stages nor starts a session. A separate host runner must establish kind/Docker
    /// ownership and the node-image identity before creating the marker.
    ///
    /// # Errors
    /// Rejects noninitial requests, candidate or protected-state drift, and any
    /// changed or malformed disposable-cluster identity.
    pub async fn reserve_qualification_candidate(
        &self,
        request: &deploy::DeploymentRequest,
        executor: &str,
        candidate: &IntegrityVerifiedCandidate,
        target: &CandidateQualificationTarget,
        inventory: &dyn inventory::InventoryStore,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let binding = self.candidate_binding(request, candidate)?;
        let domain = &candidate.compiled().ir.domain;
        validate_fresh_qualification_request(request)?;
        let current = self.load_registry().await?;
        validate_fresh_qualification_registry(&current, candidate)?;
        self.verify_release_trust(&domain.id, candidate.trust_policy_digest())
            .await?;
        self.after_candidate_qualification(&binding, target, || {
            Box::pin(self.reserve_authenticated(
                request,
                executor,
                inventory,
                Some(candidate.trust_policy_digest()),
            ))
        })
        .await
    }

    fn candidate_binding<'a>(
        &self,
        request: &deploy::DeploymentRequest,
        candidate: &'a IntegrityVerifiedCandidate,
    ) -> Result<CandidateBinding<'a>> {
        let binding = self.candidate_binding_for(candidate)?;
        if request.next.compiled != *candidate.compiled()
            || request.next.artifact_digest != candidate.release_digest()
        {
            return Err(invalid(
                "qualification reservation differs from the exact signed candidate scope",
            ));
        }
        Ok(binding)
    }

    fn candidate_binding_for<'a>(
        &self,
        candidate: &'a IntegrityVerifiedCandidate,
    ) -> Result<CandidateBinding<'a>> {
        let domain = &candidate.compiled().ir.domain;
        if candidate.manifest().image.reference.len() > 4096
            || domain.cluster_binding != self.cluster_binding
            || domain.control_namespace != self.control_namespace
            || domain.namespace != self.workload_namespace
        {
            return Err(invalid(
                "qualification reservation differs from the exact signed candidate scope",
            ));
        }
        Ok(CandidateBinding {
            domain_id: &domain.id,
            release_digest: candidate.release_digest(),
            cluster_binding: &domain.cluster_binding,
            control_namespace: &domain.control_namespace,
            workload_namespace: &domain.namespace,
        })
    }

    async fn after_candidate_qualification<T, F, Fut>(
        &self,
        binding: &CandidateBinding<'_>,
        target: &CandidateQualificationTarget,
        continue_with: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.verify_candidate_qualification_target(binding, target)
            .await?;
        continue_with().await
    }

    async fn verify_qualification_topology(
        &self,
        binding: &CandidateBinding<'_>,
        target: &CandidateQualificationTarget,
    ) -> Result<()> {
        let cluster = qualification_cluster_name(&target.run_id)?;
        let node_name = format!("{cluster}-control-plane");
        let provider_id = format!("kind://docker/{cluster}/{node_name}");
        if binding.control_namespace == KUBE_SYSTEM_NAMESPACE
            || binding.workload_namespace == KUBE_SYSTEM_NAMESPACE
            || binding.control_namespace == cluster
            || binding.workload_namespace == cluster
            || !bounded_identity(&target.kube_system_namespace_uid)
            || !bounded_identity(&target.control_namespace_uid)
            || !bounded_identity(&target.workload_namespace_uid)
            || !bounded_identity(&target.sentinel_namespace_uid)
            || target.sentinel_namespace_uid == target.kube_system_namespace_uid
            || !bounded_identity(&target.marker_uid)
            || !bounded_revision(&target.marker_resource_version)
            || target.node_name != node_name
            || !bounded_identity(&target.node_uid)
            || target.node_provider_id != provider_id
            || !crate::valid_digest(&target.node_image_digest)
        {
            return Err(invalid("invalid candidate qualification target identity"));
        }

        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        verify_namespace(
            &namespaces,
            KUBE_SYSTEM_NAMESPACE,
            &target.kube_system_namespace_uid,
        )
        .await?;
        verify_namespace(
            &namespaces,
            binding.control_namespace,
            &target.control_namespace_uid,
        )
        .await?;
        verify_namespace(
            &namespaces,
            binding.workload_namespace,
            &target.workload_namespace_uid,
        )
        .await?;
        verify_namespace(&namespaces, &cluster, &target.sentinel_namespace_uid).await?;

        verify_node(&Api::all(self.client.clone()), target).await?;

        Ok(())
    }

    async fn read_qualification_marker<T: serde::de::DeserializeOwned>(
        &self,
        target: &CandidateQualificationTarget,
        name: &str,
    ) -> Result<Versioned<T>> {
        let cluster = qualification_cluster_name(&target.run_id)?;
        let markers: Api<ConfigMap> = Api::namespaced(self.client.clone(), &cluster);
        let object = markers.get(name).await?;
        if object.metadata.name.as_deref() != Some(name)
            || object.metadata.namespace.as_deref() != Some(cluster.as_str())
            || object.metadata.uid.as_deref() != Some(target.marker_uid.as_str())
            || object.metadata.resource_version.as_deref()
                != Some(target.marker_resource_version.as_str())
            || object.metadata.deletion_timestamp.is_some()
            || object.immutable != Some(true)
        {
            return Err(invalid(
                "candidate qualification marker identity or immutability changed",
            ));
        }
        crate::store::decode_state(&object)
    }

    async fn verify_candidate_qualification_target(
        &self,
        binding: &CandidateBinding<'_>,
        target: &CandidateQualificationTarget,
    ) -> Result<()> {
        self.verify_qualification_topology(binding, target).await?;
        let cluster = qualification_cluster_name(&target.run_id)?;
        let marker: Versioned<CandidateQualificationMarker> = self
            .read_qualification_marker(target, CANDIDATE_QUALIFICATION_MARKER_NAME)
            .await?;
        let expected = CandidateQualificationMarker {
            schema_version: CANDIDATE_QUALIFICATION_SCHEMA.into(),
            run_id: target.run_id.clone(),
            cluster_name: cluster.clone(),
            sentinel_namespace: cluster,
            domain_id: binding.domain_id.into(),
            release_digest: binding.release_digest.into(),
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
        };
        if marker.uid != target.marker_uid
            || marker.resource_version != target.marker_resource_version
            || marker.value != expected
        {
            return Err(invalid(
                "candidate qualification marker content differs from the exact candidate target",
            ));
        }
        Ok(())
    }
}

async fn verify_node(nodes: &Api<Node>, target: &CandidateQualificationTarget) -> Result<()> {
    let node = nodes.get(&target.node_name).await?;
    let mut ready = node
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .into_iter()
        .flatten()
        .filter(|condition| condition.type_ == "Ready");
    let ready_condition = ready.next();
    if node.metadata.name.as_deref() != Some(target.node_name.as_str())
        || node.metadata.uid.as_deref() != Some(target.node_uid.as_str())
        || node.metadata.deletion_timestamp.is_some()
        || node
            .spec
            .as_ref()
            .and_then(|spec| spec.provider_id.as_deref())
            != Some(target.node_provider_id.as_str())
        || ready_condition.is_none_or(|condition| condition.status != "True")
        || ready.next().is_some()
    {
        return Err(invalid(
            "candidate qualification kind node identity or readiness changed",
        ));
    }
    Ok(())
}

async fn verify_namespace(api: &Api<Namespace>, name: &str, uid: &str) -> Result<()> {
    let namespace = api.get(name).await?;
    if namespace.metadata.name.as_deref() != Some(name)
        || namespace.metadata.uid.as_deref() != Some(uid)
        || namespace.metadata.deletion_timestamp.is_some()
    {
        return Err(invalid(
            "candidate qualification namespace identity changed",
        ));
    }
    Ok(())
}

fn qualification_cluster_name(run_id: &str) -> Result<String> {
    if run_id.len() != 32
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "candidate qualification run ID must be 32 lowercase hexadecimal characters",
        ));
    }
    Ok(format!("apsides-qual-{run_id}"))
}

fn bounded_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}

fn bounded_revision(value: &str) -> bool {
    bounded_identity(value) && value != "0"
}

#[cfg(test)]
mod tests;
