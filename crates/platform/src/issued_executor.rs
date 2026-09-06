use super::{EXECUTOR_BINDING_SCHEMA, ExecutorBinding, expiration, scalar};
use crate::{DeploymentIdentity, PlatformClient, Result, Versioned, invalid};
use coordination::ClaimScope;
use deploy::GenerationAllocation;
use std::sync::Arc;

/// Only the issued client factory installs this witness. Deserializing durable
/// metadata cannot create client provenance.
#[derive(Clone)]
pub(crate) struct IssuedExecutor {
    identity: Versioned<DeploymentIdentity>,
    expires_at: String,
}

impl PlatformClient {
    pub(crate) fn install_issued_identity(
        mut self,
        identity: Versioned<DeploymentIdentity>,
        expires_at: &str,
    ) -> Result<Self> {
        identity.value.validate(
            &identity.value.domain_id,
            &self.cluster_binding,
            &self.workload_namespace,
        )?;
        if !scalar(&identity.uid, 1024)
            || !scalar(&identity.resource_version, 1024)
            || identity.resource_version == "0"
        {
            return Err(invalid("issued deployment approval metadata is invalid"));
        }
        expiration(expires_at)?;
        self.issued_executor = Some(Arc::new(IssuedExecutor {
            identity,
            expires_at: expires_at.into(),
        }));
        self.require_issued_identity(&self.issued_context()?.identity.value.domain_id)?;
        Ok(self)
    }

    fn issued_context(&self) -> Result<&IssuedExecutor> {
        self.issued_executor
            .as_deref()
            .ok_or_else(|| invalid("deployment requires a platform-issued credential"))
    }

    pub(crate) fn require_issued_identity(&self, domain: &str) -> Result<()> {
        let issued = self.issued_context()?;
        issued
            .identity
            .value
            .validate(domain, &self.cluster_binding, &self.workload_namespace)?;
        if expiration(&issued.expires_at)? <= k8s_openapi::jiff::Timestamp::now() {
            return Err(invalid("deployment credential expired"));
        }
        Ok(())
    }

    pub(crate) async fn verify_issued_identity(&self, domain: &str) -> Result<()> {
        self.require_issued_identity(domain)?;
        let issued = self.issued_context()?;
        let current = self.load_deployment_identity(domain).await?;
        if current.uid != issued.identity.uid
            || current.resource_version != issued.identity.resource_version
            || current.value != issued.identity.value
        {
            return Err(invalid("issued deployment identity approval changed"));
        }
        self.require_issued_identity(domain)
    }

    pub(crate) fn reservation_executor_binding(
        &self,
        registry_uid: &str,
        allocation: &GenerationAllocation,
        domain: &str,
        executor: &str,
    ) -> Result<ExecutorBinding> {
        self.require_issued_identity(domain)?;
        let issued = self.issued_context()?;
        let identity = &issued.identity.value;
        let binding = ExecutorBinding {
            schema_version: EXECUTOR_BINDING_SCHEMA.into(),
            scope: ClaimScope {
                registry_uid: registry_uid.into(),
                cluster_binding: self.cluster_binding.clone(),
                control_namespace: self.control_namespace.clone(),
                workload_namespace: self.workload_namespace.clone(),
            },
            domain_id: domain.into(),
            transition_id: allocation.transition_id.clone(),
            sequence: allocation.sequence,
            executor_id: executor.into(),
            identity_approval_uid: issued.identity.uid.clone(),
            identity_approval_resource_version: issued.identity.resource_version.clone(),
            identity_approval_digest: compiler_core::digest(&issued.identity.value)?,
            service_account_name: identity.service_account_name.clone(),
            service_account_uid: identity.service_account_uid.clone(),
            binding_secret_name: identity.binding_secret_name.clone(),
            binding_secret_uid: identity.binding_secret_uid.clone(),
            audience: identity.audience.clone(),
            reservation_credential_expires_at: issued.expires_at.clone(),
        };
        binding.validate(&binding.scope, allocation, domain, executor)?;
        Ok(binding)
    }

    pub(crate) fn require_executor_binding(
        &self,
        binding: &ExecutorBinding,
        allocation: &GenerationAllocation,
        domain: &str,
        executor: &str,
    ) -> Result<()> {
        let mut expected = self.reservation_executor_binding(
            &allocation.registry_uid,
            allocation,
            domain,
            executor,
        )?;
        // Renewing a credential under the exact same approval is allowed. The
        // original timestamp remains immutable evidence, not a retirement horizon.
        expected
            .reservation_credential_expires_at
            .clone_from(&binding.reservation_credential_expires_at);
        binding.validate(&expected.scope, allocation, domain, executor)?;
        if binding != &expected {
            return Err(invalid(
                "transition belongs to another issued executor binding",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_test_issued_identity(
        self,
        identity: Versioned<DeploymentIdentity>,
        expires_at: &str,
    ) -> Result<Self> {
        self.install_issued_identity(identity, expires_at)
    }

    #[cfg(test)]
    pub(crate) fn executor_binding_for_test(
        &self,
        registry_uid: &str,
        allocation: &GenerationAllocation,
        domain: &str,
        executor: &str,
    ) -> Result<ExecutorBinding> {
        self.reservation_executor_binding(registry_uid, allocation, domain, executor)
    }
}
