use crate::{Error, Result};
use deploy::{ClaimState, ProtectedClaim};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

pub const CLAIM_SNAPSHOT_SCHEMA: &str = "apsides.dev/claim-snapshot/v1";
pub const MAX_CLAIM_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CLAIMS: usize = 100_000;
const DIGEST_DOMAIN: &str = "apsides.dev/claim-snapshot-digest/v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimScope {
    pub registry_uid: String,
    pub cluster_binding: String,
    pub control_namespace: String,
    pub workload_namespace: String,
}

/// Only protected tracking fields; no desired objects, observations, private
/// values, content hashes, ownership payloads or source material are accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimSnapshotV1 {
    pub schema_version: String,
    pub scope: ClaimScope,
    pub claims: Vec<ProtectedClaim>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimRef {
    pub scope: ClaimScope,
    pub digest: String,
}

impl ClaimScope {
    /// Validate a canonical registry incarnation and its fixed namespace scope.
    /// # Errors
    /// Rejects malformed registry UUIDs, invalid namespaces or unbounded cluster bindings.
    pub fn validate(&self) -> Result<()> {
        deploy::allocation_names(&self.registry_uid, 1)
            .map_err(|_| Error::Invalid("registry UID"))?;
        if self.cluster_binding.is_empty()
            || self.cluster_binding.len() > 256
            || self.cluster_binding.chars().any(char::is_control)
            || !namespace(&self.control_namespace)
            || !namespace(&self.workload_namespace)
            || self.control_namespace == self.workload_namespace
        {
            return Err(Error::Invalid("scope"));
        }
        Ok(())
    }
}

impl ClaimRef {
    /// Validate reference syntax; this does not establish external authority.
    /// # Errors
    /// Rejects invalid scope or noncanonical digest syntax.
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        if !valid_digest(&self.digest) {
            return Err(Error::Invalid("reference digest"));
        }
        Ok(())
    }
}

impl ClaimSnapshotV1 {
    /// Validate sorted unique claims and the bounded serialized metadata size.
    /// # Errors
    /// Rejects invalid scope, foreign identities, allocation mismatches and oversized snapshots.
    pub fn validate(&self) -> Result<()> {
        self.scope.validate()?;
        if self.schema_version != CLAIM_SNAPSHOT_SCHEMA || self.claims.len() > MAX_CLAIMS {
            return Err(Error::Invalid("schema or claim count"));
        }
        let mut previous = None;
        for claim in &self.claims {
            validate_claim(&self.scope, claim)?;
            if previous.is_some_and(|identity| identity >= &claim.identity) {
                return Err(Error::Invalid("claim ordering or duplicate identity"));
            }
            previous = Some(&claim.identity);
        }
        // Count serialization before constructing a canonical Value or byte buffer.
        // Field order changes no byte lengths; this bounds the canonical encoding too.
        serde_json::to_writer(SizeBudget(0), self).map_err(|_| Error::Invalid("snapshot size"))?;
        Ok(())
    }

    /// Derive a typed reference that commits to the schema, scope and every claim.
    /// # Errors
    /// Rejects invalid snapshots or canonical serialization failures.
    pub fn reference(&self) -> Result<ClaimRef> {
        self.validate()?;
        Ok(ClaimRef {
            scope: self.scope.clone(),
            digest: compiler_core::digest(&(DIGEST_DOMAIN, self))
                .map_err(|_| Error::Invalid("snapshot digest"))?,
        })
    }

    #[cfg(feature = "sqlite-backend")]
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        compiler_core::canonical_bytes(self).map_err(|_| Error::Invalid("snapshot encoding"))
    }

    #[cfg(feature = "sqlite-backend")]
    pub(crate) fn decode(bytes: &[u8], reference: &ClaimRef) -> Result<Self> {
        reference.validate()?;
        if bytes.len() > MAX_CLAIM_SNAPSHOT_BYTES {
            return Err(Error::Invalid("snapshot size"));
        }
        // Typed derives reject duplicate/unknown fields at every level. Exact
        // canonical re-encoding rejects alternate JSON spellings and whitespace,
        // without imposing compiler-core's unrelated 4 MiB source-IR parser limit.
        let snapshot: Self =
            serde_json::from_slice(bytes).map_err(|_| Error::Invalid("snapshot shape"))?;
        if snapshot.scope != reference.scope
            || snapshot.encode()? != bytes
            || snapshot.reference()? != *reference
        {
            return Err(Error::Invalid("snapshot integrity"));
        }
        Ok(snapshot)
    }
}

fn validate_claim(scope: &ClaimScope, claim: &ProtectedClaim) -> Result<()> {
    let identity = &claim.identity;
    if (!identity.api_group.is_empty() && !subdomain(&identity.api_group))
        || !namespace(&identity.resource)
        || !subdomain(&identity.name)
        || identity.namespace != scope.workload_namespace
        || !namespace(&claim.domain_id)
        || claim
            .uid
            .as_ref()
            .is_some_and(|uid| !opaque_identifier(uid))
    {
        return Err(Error::Invalid("claim identity or UID"));
    }
    let (generation_sequence, generation_names) =
        allocation(&scope.registry_uid, &claim.generation_id)?;
    let (transition_sequence, transition_names) =
        allocation(&scope.registry_uid, &claim.transition_id)?;
    if claim.generation_id != generation_names.generation_id
        || claim.transition_id != transition_names.transition_id
        || match claim.state {
            ClaimState::Active | ClaimState::Reserved => generation_sequence != transition_sequence,
            ClaimState::Releasing => transition_sequence <= generation_sequence,
        }
    {
        return Err(Error::Invalid("claim allocation or lifecycle binding"));
    }
    Ok(())
}

fn allocation(uid: &str, identity: &str) -> Result<(u64, deploy::AllocationNames)> {
    let sequence = identity
        .rsplit_once('-')
        .ok_or(Error::Invalid("claim allocation identity"))?
        .1;
    let sequence = u64::from_str_radix(sequence, 36)
        .map_err(|_| Error::Invalid("claim allocation sequence"))?;
    let names = deploy::allocation_names(uid, sequence)
        .map_err(|_| Error::Invalid("claim allocation identity"))?;
    Ok((sequence, names))
}

fn namespace(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

struct SizeBudget(usize);
impl Write for SizeBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_CLAIM_SNAPSHOT_BYTES)
            .ok_or_else(|| io::Error::other("snapshot size"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_CLAIM_SNAPSHOT_BYTES, SizeBudget};
    use std::io::Write;

    #[test]
    fn serialization_budget_rejects_the_first_byte_past_limit() -> std::io::Result<()> {
        let mut budget = SizeBudget(MAX_CLAIM_SNAPSHOT_BYTES - 1);
        assert_eq!(budget.write(&[0])?, 1);
        assert!(budget.write(&[0]).is_err());
        Ok(())
    }
}
