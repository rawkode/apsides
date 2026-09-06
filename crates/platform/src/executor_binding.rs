//! Original issued-credential metadata, not a lease or evidence of executor retirement.
use crate::{Result, invalid};
use coordination::ClaimScope;
use deploy::GenerationAllocation;
use serde::{Deserialize, Serialize};

pub const EXECUTOR_BINDING_SCHEMA: &str = "reakt.dev/executor-binding/v1";

/// Protected reservation provenance. The executor is a trusted caller's label,
/// not a cryptographic per-job identity. No token or credential fingerprint is retained.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutorBinding {
    pub schema_version: String,
    pub scope: ClaimScope,
    pub domain_id: String,
    pub transition_id: String,
    #[serde(with = "deploy::allocation_counter")]
    pub sequence: u64,
    pub executor_id: String,
    pub identity_approval_uid: String,
    pub identity_approval_resource_version: String,
    /// Commitment to the complete metadata-only identity approval, including
    /// schema, lifetime and preview settings. Retained only in protected state.
    pub identity_approval_digest: String,
    pub service_account_name: String,
    pub service_account_uid: String,
    pub binding_secret_name: String,
    pub binding_secret_uid: String,
    pub audience: String,
    /// Expiration of the credential used for reservation, not a bound on renewed
    /// credentials and never sufficient evidence that an executor has retired.
    pub reservation_credential_expires_at: String,
}

impl ExecutorBinding {
    pub(crate) fn validate(
        &self,
        scope: &ClaimScope,
        allocation: &GenerationAllocation,
        domain: &str,
        executor: &str,
    ) -> Result<()> {
        scope.validate()?;
        deploy::validate_allocation(allocation)?;
        if self.schema_version != EXECUTOR_BINDING_SCHEMA
            || &self.scope != scope
            || self.scope.registry_uid != allocation.registry_uid
            || self.domain_id != domain
            || !crate::valid_name(domain)
            || self.transition_id != allocation.transition_id
            || self.sequence != allocation.sequence
            || self.executor_id != executor
            || !valid_executor(executor)
            || !crate::valid_name(&self.service_account_name)
            || !crate::valid_name(&self.binding_secret_name)
            || !scalar(&self.identity_approval_uid, 1024)
            || !scalar(&self.identity_approval_resource_version, 1024)
            || self.identity_approval_resource_version == "0"
            || !crate::valid_digest(&self.identity_approval_digest)
            || !scalar(&self.service_account_uid, 1024)
            || !scalar(&self.binding_secret_uid, 1024)
            || !scalar(&self.audience, 256)
        {
            return Err(invalid(
                "executor credential binding is invalid or differently scoped",
            ));
        }
        expiration(&self.reservation_credential_expires_at)?;
        Ok(())
    }
}

fn valid_executor(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn scalar(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

pub(crate) fn expiration(value: &str) -> Result<k8s_openapi::jiff::Timestamp> {
    if !scalar(value, 64) {
        return Err(invalid("executor credential expiration is invalid"));
    }
    let parsed: k8s_openapi::jiff::Timestamp = value
        .parse()
        .map_err(|_| invalid("executor credential expiration is invalid"))?;
    if parsed.to_string() != value {
        return Err(invalid("executor credential expiration is not canonical"));
    }
    Ok(parsed)
}

#[cfg(feature = "deployer")]
#[path = "issued_executor.rs"]
mod issued;
#[cfg(feature = "deployer")]
pub(crate) use issued::IssuedExecutor;
