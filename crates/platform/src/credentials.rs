use crate::{
    DeploymentCredential, PlatformClient, Result, SCHEMA_VERSION, Versioned, invalid,
    store::read_state, valid_name,
};
use coordination::ReviewIdentity;
use serde::{Deserialize, Serialize};

pub const DEPLOYMENT_IDENTITY_SCHEMA_V2: &str = "apsides.dev/deployment-identity/v2";
const MAX_PREVIEW_TARGETS: usize = 256;

/// A fixed preview mechanism approved by the deployment identity policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeploymentPreviewProfile {
    /// Approves exact named LIST/PATCH dry-run SSA and acknowledges that the required Kubernetes
    /// CREATE permission is broader across the target namespace and GVR. RBAC is provisioned
    /// independently; this value grants no credential, session, client binding or live-write
    /// authority.
    #[serde(rename = "static-ssa-issued-deployer/v1")]
    StaticSsaIssuedDeployerV1,
}

/// Explicit preview scope for a v2 deployment identity policy.
///
/// An empty target list approves preview processing for a declaration with no managed objects; it
/// does not mean all names and must not be translated to an empty Kubernetes `resourceNames` list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentPreviewApproval {
    pub profile: DeploymentPreviewProfile,
    pub targets: Vec<ReviewIdentity>,
}

/// Platform-owned identity adapter settings. Releases cannot select a more
/// privileged service account or replace the credential's binding object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentIdentity {
    pub schema_version: String,
    pub domain_id: String,
    pub cluster_binding: String,
    pub service_account_name: String,
    pub service_account_uid: String,
    pub binding_secret_name: String,
    pub binding_secret_uid: String,
    pub audience: String,
    pub lifetime_seconds: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<DeploymentPreviewApproval>,
}

impl DeploymentIdentity {
    pub(crate) fn validate(
        &self,
        expected_domain: &str,
        expected_cluster: &str,
        expected_workload_namespace: &str,
    ) -> Result<()> {
        let version_valid = match self.schema_version.as_str() {
            SCHEMA_VERSION => self.preview.is_none(),
            DEPLOYMENT_IDENTITY_SCHEMA_V2 => self
                .preview
                .as_ref()
                .is_none_or(|approval| approval.validate(expected_workload_namespace)),
            _ => false,
        };
        if !version_valid
            || self.domain_id != expected_domain
            || self.cluster_binding != expected_cluster
            || !valid_name(&self.service_account_name)
            || !valid_name(&self.binding_secret_name)
            || self.service_account_uid.is_empty()
            || self.binding_secret_uid.is_empty()
            || self.audience.is_empty()
            || self.audience.len() > 256
            || self.audience.chars().any(char::is_control)
            || !(600..=900).contains(&self.lifetime_seconds)
        {
            return Err(invalid(
                "invalid or differently scoped deployment identity policy",
            ));
        }
        Ok(())
    }
}

impl DeploymentPreviewApproval {
    fn validate(&self, expected_workload_namespace: &str) -> bool {
        self.targets.len() <= MAX_PREVIEW_TARGETS
            && !self
                .targets
                .windows(2)
                .any(|pair| matches!(pair, [previous, current] if previous >= current))
            && self.targets.iter().all(|target| {
                target.namespace == expected_workload_namespace
                    && valid_name(&target.name)
                    && matches!(
                        (target.api_group.as_str(), target.resource.as_str()),
                        ("", "configmaps" | "services") | ("apps", "deployments")
                    )
            })
    }
}

impl PlatformClient {
    /// Load approved names, UIDs and audience from protected platform state.
    /// # Errors
    /// Rejects missing or invalid identity settings and scope mismatches.
    pub async fn load_deployment_identity(
        &self,
        domain: &str,
    ) -> Result<Versioned<DeploymentIdentity>> {
        if !valid_name(domain) {
            return Err(invalid("invalid deployment identity domain"));
        }
        let state: Versioned<DeploymentIdentity> = read_state(
            &self.client,
            &self.control_namespace,
            &format!("apsides-deploy-identity-{domain}"),
        )
        .await?;
        let identity = &state.value;
        identity.validate(domain, &self.cluster_binding, &self.workload_namespace)?;
        Ok(state)
    }

    /// Mint credentials solely from the current protected identity policy.
    /// # Errors
    /// Rejects changed bindings, denied `TokenRequest`, and changed policy.
    pub async fn approved_deployment_credential(
        &self,
        domain: &str,
    ) -> Result<DeploymentCredential> {
        let before = self.load_deployment_identity(domain).await?;
        let identity = &before.value;
        let credential = self
            .request_deployment_credential(
                &identity.service_account_name,
                &identity.service_account_uid,
                &identity.binding_secret_name,
                &identity.binding_secret_uid,
                &identity.audience,
                identity.lifetime_seconds,
            )
            .await?;
        let after = self.load_deployment_identity(domain).await?;
        if before.uid != after.uid || before.resource_version != after.resource_version {
            return Err(invalid(
                "deployment identity policy changed while minting a credential",
            ));
        }
        Ok(credential)
    }
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
