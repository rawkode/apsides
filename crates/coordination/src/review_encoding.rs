use super::{MAX_REVIEW_METADATA_BYTES, MAX_REVIEW_SOURCE_ELEMENTS, ReviewMetadataV1, ReviewRef};
use crate::{Error, Result};
use std::io::{self, Write};

pub(super) fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

pub(super) fn validate_size(value: &ReviewMetadataV1) -> Result<()> {
    let mut count = 0usize;
    let d = &value.deployment_permissions;
    let c = &value.compatibility;
    let e = &value.enforcement;
    let lengths = [
        value.resources.len(),
        value.inputs.len(),
        value.reactions.len(),
        value.runtime_permissions.len(),
        value.cleanup_permissions.len(),
        d.required_resources.len(),
        d.creation_targets.len(),
        d.role_delegation.len(),
        d.limitations.len(),
        c.required_runtime_apis.len(),
        c.required_cleanup_apis.len(),
        c.required_deployment_apis.len(),
        c.qualified_create_precondition_servers.len(),
        e.gaps.len(),
        e.indirect_risks.len(),
        e.runtime_ceiling.capabilities.len(),
        e.cleanup_ceiling.capabilities.len(),
        e.runtime_ceiling.accepted_enforcement_gaps.len(),
        e.runtime_ceiling.accepted_indirect_risks.len(),
        e.cleanup_ceiling.accepted_enforcement_gaps.len(),
        e.cleanup_ceiling.accepted_indirect_risks.len(),
    ];
    for length in lengths
        .into_iter()
        .chain(value.resources.iter().map(|r| r.apply_fields.len()))
        .chain(value.inputs.iter().map(|i| i.projection.len()))
        .chain(
            value
                .reactions
                .iter()
                .flat_map(|r| [r.input_ids.len(), r.effect_fields.len()]),
        )
        .chain(value.runtime_permissions.iter().map(|c| c.names.len()))
        .chain(value.cleanup_permissions.iter().map(|c| c.names.len()))
        .chain(e.runtime_ceiling.capabilities.iter().map(|c| c.names.len()))
        .chain(e.cleanup_ceiling.capabilities.iter().map(|c| c.names.len()))
        .chain(d.required_resources.iter().map(|r| r.resource_names.len()))
        .chain(
            d.role_delegation
                .iter()
                .map(|r| r.granted_capabilities.len()),
        )
        .chain(
            d.role_delegation
                .iter()
                .flat_map(|r| r.granted_capabilities.iter().map(|c| c.names.len())),
        )
    {
        count = count
            .checked_add(length)
            .filter(|n| *n <= MAX_REVIEW_SOURCE_ELEMENTS)
            .ok_or(Error::Invalid("review source element limit"))?;
    }
    serde_json::to_writer(SizeBudget(MAX_REVIEW_METADATA_BYTES), value)
        .map_err(|_| Error::Invalid("review metadata size"))?;
    Ok(())
}
impl ReviewMetadataV1 {
    /// Derive the domain-separated reference to exactly this validated metadata.
    /// # Errors
    /// Rejects invalid or oversized metadata and encoding failures.
    pub fn reference(&self) -> Result<ReviewRef> {
        self.validate()?;
        Ok(ReviewRef {
            scope: self.scope.clone(),
            digest: compiler_core::digest(&("apsides.dev/review-metadata-digest/v1", self))
                .map_err(|_| Error::Invalid("review metadata digest"))?,
        })
    }
    #[cfg(feature = "libsql-backend")]
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        compiler_core::canonical_bytes(self).map_err(|_| Error::Invalid("review metadata encoding"))
    }
    #[cfg(feature = "libsql-backend")]
    pub(crate) fn decode(bytes: &[u8], reference: &ReviewRef) -> Result<Self> {
        reference.validate()?;
        if bytes.len() > MAX_REVIEW_METADATA_BYTES {
            return Err(Error::Invalid("review metadata size"));
        }
        let metadata: Self =
            serde_json::from_slice(bytes).map_err(|_| Error::Invalid("review metadata shape"))?;
        if metadata.scope != reference.scope
            || metadata.encode()? != bytes
            || metadata.reference()? != *reference
        {
            return Err(Error::Invalid("review metadata integrity"));
        }
        Ok(metadata)
    }
}
struct SizeBudget(usize);
impl Write for SizeBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("review metadata size"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SizeBudget;

    #[test]
    fn serialized_budget_accepts_exact_boundary_and_counts_json_escapes()
    -> Result<(), serde_json::Error> {
        serde_json::to_writer(SizeBudget(8), &"aaaaaa")?;
        assert!(serde_json::to_writer(SizeBudget(7), &"aaaaaa").is_err());
        serde_json::to_writer(SizeBudget(8), &"\0")?;
        assert!(serde_json::to_writer(SizeBudget(7), &"\0").is_err());
        Ok(())
    }
}
