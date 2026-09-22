use crate::{ClaimScope, Error, Result};
use deploy::{CleanupOutcome, OwnershipAction, ResourceIdentity};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{self, Write},
};

pub const TRANSITION_AUDIT_SCHEMA: &str = "apsides.dev/transition-audit/v1";
pub const MAX_TRANSITION_AUDIT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_AUDIT_EVENTS: usize = 256;
pub const MAX_AUDIT_CLEANUP: usize = 100_000;
pub const MAX_AUDIT_OWNERSHIP: usize = 4096;
pub const MAX_AUDIT_RETIREMENT: usize = 4097;
const DIGEST_DOMAIN: &str = "apsides.dev/transition-audit-digest/v1";

/// Scope and allocation identity committed by one transition audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditScope {
    pub claim_scope: ClaimScope,
    pub domain_id: String,
    pub transition_id: String,
    #[serde(with = "audit_counter")]
    pub sequence: u64,
}

/// A metadata-only reference to one immutable transition audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditRef {
    pub scope: AuditScope,
    pub digest: String,
}

/// Canonical allocation names without artifact, IR, inventory or content digests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditAllocation {
    #[serde(with = "audit_counter")]
    pub sequence: u64,
    pub generation_id: String,
    pub transition_id: String,
    pub service_account_name: String,
}

/// Sanitized event kind. Evidence values and blocked reasons are excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuditEventKind {
    Staged,
    Quiesced,
    Activated,
    Cleaning,
    Committed,
    Blocked,
    Resume,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditEvent {
    pub kind: AuditEventKind,
    pub event_id: String,
    #[serde(with = "audit_counter::zero_allowed")]
    pub verification_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditCleanup {
    pub identity: ResourceIdentity,
    pub expected_uid: Option<String>,
    pub outcome: CleanupOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditOwnership {
    pub identity: ResourceIdentity,
    pub observed_uid: String,
    pub action: OwnershipAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditRetirement {
    pub identity: ResourceIdentity,
    pub uid: String,
}

/// Immutable, metadata-only record of one completed transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransitionAuditV1 {
    pub schema_version: String,
    pub scope: AuditScope,
    pub allocation: AuditAllocation,
    pub executor_id: String,
    #[serde(with = "audit_counter::zero_allowed")]
    pub final_revision: u64,
    #[serde(with = "audit_counter::zero_allowed")]
    pub final_verification_epoch: u64,
    pub events: Vec<AuditEvent>,
    pub cleanup: Vec<AuditCleanup>,
    pub ownership: Vec<AuditOwnership>,
    pub retirement: Vec<AuditRetirement>,
    pub previous: Option<AuditRef>,
}

impl AuditScope {
    /// Validate the registry, domain and canonical transition allocation identity.
    /// # Errors
    /// Rejects malformed scope, domain, sequence or transition identity.
    pub fn validate(&self) -> Result<()> {
        self.claim_scope.validate()?;
        let names = deploy::allocation_names(&self.claim_scope.registry_uid, self.sequence)
            .map_err(|_| Error::Invalid("audit allocation scope"))?;
        if !opaque_identifier(&self.domain_id) || self.transition_id != names.transition_id {
            return Err(Error::Invalid("audit allocation scope"));
        }
        Ok(())
    }
}

impl AuditRef {
    /// Validate reference syntax; this does not establish protected-head authority.
    /// # Errors
    /// Rejects malformed scope or digest metadata.
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        if !valid_digest(&self.digest) {
            return Err(Error::Invalid("audit reference digest"));
        }
        Ok(())
    }
}

impl TransitionAuditV1 {
    /// Validate bounded, canonical transition metadata and its immutable chain link.
    /// # Errors
    /// Rejects malformed allocation, lifecycle, ownership, cleanup, retirement or chain metadata.
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        if self.schema_version != TRANSITION_AUDIT_SCHEMA
            || !opaque_identifier(&self.executor_id)
            || self.events.is_empty()
            || self.events.len() > MAX_AUDIT_EVENTS
            || self.cleanup.len() > MAX_AUDIT_CLEANUP
            || self.ownership.len() > MAX_AUDIT_OWNERSHIP
            || self.retirement.len() > MAX_AUDIT_RETIREMENT
            || self.final_revision != self.events.len() as u64
        {
            return Err(Error::Invalid("audit schema or bounds"));
        }
        self.validate_allocation()?;
        self.validate_events()?;
        validate_cleanup(&self.cleanup)?;
        validate_ownership(&self.ownership)?;
        validate_chain_and_retirement(&self.scope, self.previous.as_ref(), &self.retirement)?;
        serde_json::to_writer(AuditSizeBudget(0), self)
            .map_err(|_| Error::Invalid("audit size"))?;
        Ok(())
    }

    /// Derive a scope-bound reference over the complete canonical audit metadata.
    /// # Errors
    /// Rejects invalid audits or canonical serialization failures.
    pub fn reference(&self) -> Result<AuditRef> {
        self.validate()?;
        Ok(AuditRef {
            scope: self.scope.clone(),
            digest: compiler_core::digest(&(DIGEST_DOMAIN, self))
                .map_err(|_| Error::Invalid("audit digest"))?,
        })
    }

    fn validate_allocation(&self) -> Result<()> {
        let names = deploy::allocation_names(
            &self.scope.claim_scope.registry_uid,
            self.allocation.sequence,
        )
        .map_err(|_| Error::Invalid("audit allocation"))?;
        if self.allocation.sequence != self.scope.sequence
            || self.allocation.transition_id != self.scope.transition_id
            || self.allocation.generation_id != names.generation_id
            || self.allocation.transition_id != names.transition_id
            || self.allocation.service_account_name != names.service_account_name
        {
            return Err(Error::Invalid("audit allocation"));
        }
        Ok(())
    }

    fn validate_events(&self) -> Result<()> {
        let mut phase = AuditProgress::Planned;
        let mut epoch = 0_u64;
        let mut event_ids = BTreeSet::new();
        for event in &self.events {
            if !opaque_identifier(&event.event_id) || !event_ids.insert(&event.event_id) {
                return Err(Error::Invalid("audit event identity"));
            }
            match (phase, event.kind) {
                (AuditProgress::Planned, AuditEventKind::Staged) => phase = AuditProgress::Staged,
                (AuditProgress::Staged, AuditEventKind::Quiesced) => {
                    phase = AuditProgress::Quiesced;
                }
                (AuditProgress::Quiesced, AuditEventKind::Activated) => {
                    phase = AuditProgress::Activated;
                }
                (AuditProgress::Activated, AuditEventKind::Cleaning) => {
                    phase = AuditProgress::Cleaning;
                }
                (AuditProgress::Cleaning, AuditEventKind::Committed) => {
                    phase = AuditProgress::Committed;
                }
                (
                    AuditProgress::Planned | AuditProgress::Staged | AuditProgress::Quiesced,
                    AuditEventKind::Blocked,
                ) => phase = AuditProgress::Blocked,
                (AuditProgress::Blocked, AuditEventKind::Resume) => {
                    epoch = epoch
                        .checked_add(1)
                        .ok_or(Error::Invalid("audit event epoch"))?;
                    phase = AuditProgress::Planned;
                }
                _ => return Err(Error::Invalid("audit event order")),
            }
            if event.verification_epoch != epoch {
                return Err(Error::Invalid("audit event epoch"));
            }
        }
        if phase != AuditProgress::Committed || self.final_verification_epoch != epoch {
            return Err(Error::Invalid("audit final phase or epoch"));
        }
        Ok(())
    }

    #[cfg(feature = "sqlite-backend")]
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        compiler_core::canonical_bytes(self).map_err(|_| Error::Invalid("audit encoding"))
    }

    #[cfg(feature = "sqlite-backend")]
    pub(crate) fn decode(bytes: &[u8], reference: &AuditRef) -> Result<Self> {
        reference.validate()?;
        if bytes.len() > MAX_TRANSITION_AUDIT_BYTES {
            return Err(Error::Invalid("audit size"));
        }
        let audit: Self =
            serde_json::from_slice(bytes).map_err(|_| Error::Invalid("audit shape"))?;
        if audit.scope != reference.scope
            || audit.encode()? != bytes
            || audit.reference()? != *reference
        {
            return Err(Error::Invalid("audit integrity"));
        }
        Ok(audit)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuditProgress {
    Planned,
    Staged,
    Quiesced,
    Activated,
    Cleaning,
    Committed,
    Blocked,
}

fn validate_cleanup(cleanup: &[AuditCleanup]) -> Result<()> {
    let mut previous = None;
    for item in cleanup {
        validate_identity(&item.identity)?;
        if item
            .expected_uid
            .as_ref()
            .is_some_and(|uid| !opaque_identifier(uid))
            || matches!(item.outcome, CleanupOutcome::Deleted) && item.expected_uid.is_none()
            || previous.is_some_and(|identity| identity >= &item.identity)
        {
            return Err(Error::Invalid("audit cleanup ordering or UID"));
        }
        previous = Some(&item.identity);
    }
    Ok(())
}

fn validate_ownership(ownership: &[AuditOwnership]) -> Result<()> {
    let mut previous = None;
    for item in ownership {
        validate_identity(&item.identity)?;
        if !opaque_identifier(&item.observed_uid)
            || previous.is_some_and(|identity| identity >= &item.identity)
        {
            return Err(Error::Invalid("audit ownership ordering or UID"));
        }
        previous = Some(&item.identity);
    }
    Ok(())
}

fn validate_chain_and_retirement(
    scope: &AuditScope,
    previous: Option<&AuditRef>,
    retirement: &[AuditRetirement],
) -> Result<()> {
    let Some(previous) = previous else {
        return if retirement.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid("initial audit has retirement metadata"))
        };
    };
    previous.validate()?;
    if previous.scope.claim_scope != scope.claim_scope
        || previous.scope.domain_id != scope.domain_id
        || previous.scope.sequence >= scope.sequence
    {
        return Err(Error::Invalid("audit predecessor scope"));
    }
    let prior_names = deploy::allocation_names(
        &previous.scope.claim_scope.registry_uid,
        previous.scope.sequence,
    )
    .map_err(|_| Error::Invalid("audit predecessor allocation"))?;
    let mut previous_identity = None;
    let mut service_account_seen = false;
    for item in retirement {
        validate_identity(&item.identity)?;
        if !opaque_identifier(&item.uid)
            || item.identity.name != prior_names.service_account_name
            || previous_identity.is_some_and(|identity| identity >= &item.identity)
        {
            return Err(Error::Invalid("audit retirement ordering or identity"));
        }
        match (
            item.identity.api_group.as_str(),
            item.identity.resource.as_str(),
        ) {
            ("", "serviceaccounts")
                if item.identity.namespace == scope.claim_scope.control_namespace
                    && !service_account_seen =>
            {
                service_account_seen = true;
            }
            ("rbac.authorization.k8s.io", "rolebindings") => {}
            _ => return Err(Error::Invalid("audit retirement resource")),
        }
        previous_identity = Some(&item.identity);
    }
    if !service_account_seen {
        return Err(Error::Invalid("audit retirement ServiceAccount"));
    }
    Ok(())
}

fn validate_identity(identity: &ResourceIdentity) -> Result<()> {
    if (!identity.api_group.is_empty() && !subdomain(&identity.api_group))
        || !namespace(&identity.resource)
        || !namespace(&identity.namespace)
        || !subdomain(&identity.name)
    {
        return Err(Error::Invalid("audit resource identity"));
    }
    Ok(())
}

fn namespace(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn subdomain(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(namespace)
}

fn opaque_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

struct AuditSizeBudget(usize);

impl Write for AuditSizeBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_TRANSITION_AUDIT_BYTES)
            .ok_or_else(|| io::Error::other("audit size"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

mod audit_counter {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    #[allow(clippy::trivially_copy_pass_by_ref)] // Required by serde's `with` module contract.
    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        let value = text.parse::<u64>().map_err(D::Error::custom)?;
        if value == 0 || value.to_string() != text {
            return Err(D::Error::custom("noncanonical audit counter"));
        }
        Ok(value)
    }

    pub mod zero_allowed {
        use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

        #[allow(clippy::trivially_copy_pass_by_ref)] // Required by serde's `with` module contract.
        pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_str(&value.to_string())
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
        where
            D: Deserializer<'de>,
        {
            let text = String::deserialize(deserializer)?;
            let value = text.parse::<u64>().map_err(D::Error::custom)?;
            if value.to_string() != text {
                return Err(D::Error::custom("noncanonical audit counter"));
            }
            Ok(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuditSizeBudget, MAX_TRANSITION_AUDIT_BYTES};
    use std::io::Write;

    #[test]
    fn audit_serialization_budget_rejects_the_first_byte_past_limit() -> std::io::Result<()> {
        let mut budget = AuditSizeBudget(MAX_TRANSITION_AUDIT_BYTES - 1);
        assert_eq!(budget.write(&[0])?, 1);
        assert!(budget.write(&[0]).is_err());
        Ok(())
    }
}
