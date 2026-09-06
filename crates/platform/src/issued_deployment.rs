//! Opaque provenance for the stock client carrying the issued deployment token.
use crate::{DeploymentIdentity, PlatformClient, Result, Versioned, invalid};
use kube::{Client, Config, config::AuthInfo};
use std::{sync::Arc, time::Duration};

/// An issued client and its exact platform-owned identity approval. There is no
/// constructor accepting a client or bearer: both clients originate from one
/// trusted TLS configuration, and only the issued client survives construction.
pub struct IssuedDeployment {
    platform: PlatformClient,
    identity: Versioned<DeploymentIdentity>,
    expires_at: String,
    expires: k8s_openapi::jiff::Timestamp,
}

impl IssuedDeployment {
    /// Mint a bounded credential and construct the stock client using that bearer
    /// alone. The configuration must come from the trusted invoking environment.
    /// # Errors
    /// Rejects insecure TLS, invalid scope, changed approval or credential failure.
    pub async fn connect(
        mut config: Config,
        control_namespace: &str,
        workload_namespace: &str,
        cluster_binding: &str,
        domain: &str,
    ) -> Result<Self> {
        configure_transport(&mut config)?;
        let bootstrap = PlatformClient::new(
            Client::try_from(config.clone())
                .map_err(|_| invalid("deployment bootstrap client construction failed"))?,
            control_namespace,
            workload_namespace,
            cluster_binding,
        )?;
        let identity = bootstrap.load_deployment_identity(domain).await?;
        let credential = bootstrap.approved_deployment_credential(domain).await?;
        let after = bootstrap.load_deployment_identity(domain).await?;
        if !same_identity(&identity, &after)
            || credential.service_account_uid != identity.value.service_account_uid
        {
            return Err(invalid(
                "deployment identity changed during client construction",
            ));
        }
        let expires = credential
            .expires_at
            .parse::<k8s_openapi::jiff::Timestamp>()
            .map_err(|_| invalid("deployment credential expiration is invalid"))?;
        let config = issued_config(config, credential.token);
        let platform = PlatformClient::new(
            Client::try_from(config)
                .map_err(|_| invalid("issued deployment client construction failed"))?,
            control_namespace,
            workload_namespace,
            cluster_binding,
        )?;
        let issued = Self {
            platform,
            identity,
            expires_at: credential.expires_at,
            expires,
        };
        issued.require_unexpired()?;
        Ok(issued)
    }

    /// Attach metadata stores without replacing the privately constructed client.
    #[must_use]
    pub fn with_stores(
        mut self,
        claims: Arc<dyn coordination::ClaimStore>,
        audit: Arc<dyn coordination::AuditStore>,
        review: Arc<dyn coordination::ReviewStore>,
    ) -> Self {
        self.platform = self
            .platform
            .with_claim_store(claims)
            .with_audit_store(audit)
            .with_review_store(review);
        self
    }

    #[must_use]
    pub fn platform(&self) -> &PlatformClient {
        &self.platform
    }

    #[must_use]
    pub fn expires_at(&self) -> &str {
        &self.expires_at
    }

    #[must_use]
    pub fn identity(&self) -> &DeploymentIdentity {
        &self.identity.value
    }

    #[must_use]
    pub fn identity_approval(&self) -> &Versioned<DeploymentIdentity> {
        &self.identity
    }

    /// Recheck the pinned preview opt-in through the issued identity. This check
    /// alone does not authenticate a release, target set, or observation interval.
    /// # Errors
    /// Rejects missing opt-in, expired credentials or changed protected identity.
    pub async fn require_preview_approval(&self) -> Result<&crate::DeploymentPreviewApproval> {
        let approval = self
            .identity
            .value
            .preview
            .as_ref()
            .ok_or_else(|| invalid("deployment identity has no preview approval"))?;
        self.require_unexpired()?;
        let current = self
            .platform
            .load_deployment_identity(&self.identity.value.domain_id)
            .await?;
        if !same_identity(&self.identity, &current) {
            return Err(invalid("deployment preview identity approval changed"));
        }
        self.require_unexpired()?;
        Ok(approval)
    }

    fn require_unexpired(&self) -> Result<()> {
        if self.expires <= k8s_openapi::jiff::Timestamp::now() {
            return Err(invalid("deployment credential expired"));
        }
        Ok(())
    }
}

fn issued_config(mut config: Config, token: secrecy::SecretString) -> Config {
    config.auth_info = AuthInfo {
        token: Some(token),
        ..AuthInfo::default()
    };
    config.headers.clear();
    config
}

fn same_identity(
    left: &Versioned<DeploymentIdentity>,
    right: &Versioned<DeploymentIdentity>,
) -> bool {
    left.uid == right.uid
        && left.resource_version == right.resource_version
        && left.value == right.value
}

fn configure_transport(config: &mut Config) -> Result<()> {
    if config.cluster_url.scheme_str() != Some("https") || config.accept_invalid_certs {
        return Err(invalid("deployment requires verified TLS"));
    }
    config.default_retry = false;
    config.connect_timeout = Some(Duration::from_secs(10));
    config.read_timeout = Some(Duration::from_secs(30));
    config.write_timeout = Some(Duration::from_secs(30));
    Ok(())
}

#[cfg(test)]
#[path = "issued_deployment_tests.rs"]
mod tests;

#[cfg(all(test, feature = "deployer"))]
impl IssuedDeployment {
    pub(crate) fn for_preview_test(
        platform: PlatformClient,
        identity: Versioned<DeploymentIdentity>,
    ) -> Result<Self> {
        identity.value.validate(
            &identity.value.domain_id,
            &platform.cluster_binding,
            &platform.workload_namespace,
        )?;
        let expires = k8s_openapi::jiff::Timestamp::from_second(
            k8s_openapi::jiff::Timestamp::now()
                .as_second()
                .checked_add(600)
                .ok_or_else(|| invalid("test expiration overflow"))?,
        )
        .map_err(|_| invalid("test expiration invalid"))?;
        Ok(Self {
            platform,
            identity,
            expires_at: expires.to_string(),
            expires,
        })
    }
}
