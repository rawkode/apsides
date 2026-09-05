use crate::{
    DeploymentCredential, PlatformClient, Result, SCHEMA_VERSION, Versioned, invalid,
    store::read_state, valid_name,
};
use serde::{Deserialize, Serialize};

/// Platform-owned identity adapter settings. Releases cannot select a more
/// privileged service account or replace the credential's binding object.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
            &format!("reakt-deploy-identity-{domain}"),
        )
        .await?;
        let identity = &state.value;
        if identity.schema_version != SCHEMA_VERSION
            || identity.domain_id != domain
            || identity.cluster_binding != self.cluster_binding
            || !valid_name(&identity.service_account_name)
            || !valid_name(&identity.binding_secret_name)
            || identity.service_account_uid.is_empty()
            || identity.binding_secret_uid.is_empty()
            || identity.audience.is_empty()
            || identity.audience.len() > 256
            || identity.audience.chars().any(char::is_control)
            || !(600..=900).contains(&identity.lifetime_seconds)
        {
            return Err(invalid(
                "invalid or differently scoped deployment identity policy",
            ));
        }
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
