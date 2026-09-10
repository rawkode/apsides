//! Deliberate disposable-cluster acceptance capability. Normal deployments never
//! compile this module and cannot turn a candidate into a verified release.
use crate::{NamespaceRegistry, PlatformClient, Result, Versioned, invalid};
use k8s_openapi::api::core::v1::Namespace;
use kube::Api;
use release::TestStagingAuthorization;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestClusterTarget {
    pub kube_system_namespace_uid: String,
    pub marker_uid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisposableTestCluster {
    pub schema_version: String,
    pub cluster_binding: String,
    pub control_namespace: String,
    pub workload_namespace: String,
    pub kube_system_namespace_uid: String,
}

impl PlatformClient {
    /// Reserve only a signed candidate bound to a disposable cluster marker.
    /// # Errors
    /// Rejects mismatched candidate bytes, target identity or immutable inventory.
    pub async fn reserve_test_candidate(
        &self,
        request: &deploy::DeploymentRequest,
        executor: &str,
        authorization: &TestStagingAuthorization,
        target: &TestClusterTarget,
        inventory: &dyn inventory::InventoryStore,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.verify_test_cluster_target(authorization, target)
            .await?;
        let candidate = authorization.candidate();
        if request.next.compiled != *candidate.compiled()
            || request.next.artifact_digest != candidate.release_digest()
            || candidate.manifest().image.reference.len() > 4096
        {
            return Err(invalid("test reservation differs from signed candidate"));
        }
        self.verify_release_trust(
            &request.next.compiled.ir.domain.id,
            candidate.trust_policy_digest(),
        )
        .await?;
        Box::pin(self.reserve_authenticated(
            request,
            executor,
            inventory,
            Some(candidate.trust_policy_digest()),
        ))
        .await
    }

    /// Resolve one signed candidate for an explicitly marked disposable target.
    /// # Errors
    /// Rejects target drift, missing metadata or nonexact transition replay.
    pub async fn transition_test_session(
        &self,
        authorization: &TestStagingAuthorization,
        target: &TestClusterTarget,
        inventory: &dyn inventory::InventoryStore,
        transition: &str,
    ) -> Result<crate::TransitionSession> {
        self.verify_test_cluster_target(authorization, target)
            .await?;
        let candidate = authorization.candidate();
        self.bind_release(
            candidate.compiled(),
            candidate.release_digest(),
            candidate.trust_policy_digest(),
            inventory,
            transition,
        )
        .await
    }

    /// Stage an integrity/provenance verified candidate on an explicitly marked
    /// disposable API target. This is not available in the production CLI build.
    /// # Errors
    /// Rejects unmarked, replaced, production-named or scope-mismatched targets.
    pub async fn stage_test_candidate(
        &self,
        transition: &str,
        executor: &str,
        authorization: &TestStagingAuthorization,
        target: &TestClusterTarget,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.verify_test_cluster_target(authorization, target)
            .await?;
        let candidate = authorization.candidate();
        let artifact = crate::controller::StagingArtifact {
            compiled: candidate.compiled(),
            image: &candidate.manifest().image.reference,
            release_digest: candidate.release_digest(),
            provenance_digest: candidate.provenance_digest(),
            trust_policy_digest: candidate.trust_policy_digest(),
        };
        self.stage_proxy_artifact(transition, executor, &artifact)
            .await
    }

    /// Verify the explicit disposable API binding before any test lifecycle mutation.
    /// # Errors
    /// Rejects replaced cluster/marker identities or mismatched signed scope.
    pub async fn verify_test_cluster_target(
        &self,
        authorization: &TestStagingAuthorization,
        target: &TestClusterTarget,
    ) -> Result<()> {
        let scope = authorization.scope();
        if scope.cluster_binding != self.cluster_binding
            || scope.control_namespace != self.control_namespace
            || scope.workload_namespace != self.workload_namespace
            || !scope.cluster_binding.starts_with("apsides-disposable-")
            || !scope.control_namespace.starts_with("apsides-test-")
            || !scope.workload_namespace.starts_with("apsides-test-")
            || target.marker_uid.is_empty()
            || target.kube_system_namespace_uid.is_empty()
        {
            return Err(invalid(
                "test staging requires an explicit matching disposable scope",
            ));
        }
        let namespaces: Api<Namespace> = Api::all(self.client.clone());
        let namespace = namespaces.get("kube-system").await?;
        if namespace.metadata.uid.as_ref() != Some(&target.kube_system_namespace_uid)
            || namespace.metadata.deletion_timestamp.is_some()
        {
            return Err(invalid(
                "test API target cluster UID differs from bootstrap",
            ));
        }
        let marker: Versioned<DisposableTestCluster> = crate::store::read_state(
            &self.client,
            &self.control_namespace,
            "apsides-test-support",
        )
        .await?;
        if marker.uid != target.marker_uid
            || marker.value.schema_version != "apsides.dev/disposable-test-cluster/v1"
            || marker.value.cluster_binding != scope.cluster_binding
            || marker.value.control_namespace != scope.control_namespace
            || marker.value.workload_namespace != scope.workload_namespace
            || marker.value.kube_system_namespace_uid != target.kube_system_namespace_uid
        {
            return Err(invalid(
                "disposable-cluster marker identity or scope changed",
            ));
        }
        Ok(())
    }
}
