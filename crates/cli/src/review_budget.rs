//! Bound authenticated source elements before allocating deltas, then bound serialized bytes.
//! The source guard counts both declaration snapshots plus transition-only report rows.
//! Repeated change arrays and derived permission, compatibility, and location rows
//! are covered by the byte limit; the source guard is not an output-entry limit.
use serde::Serialize;
use std::io::{self, Write};

pub(super) const MAX_SOURCE_ELEMENTS: usize = 100_000;
pub(super) const MAX_BYTES: usize = 8 * 1024 * 1024;
const LIMIT: &str = "deployment review incomplete: metadata projection limit exceeded";

pub(super) fn count(
    plan: &deploy::TransitionPlan,
    previous: Option<&coordination::ReviewMetadataV1>,
    next: &coordination::ReviewMetadataV1,
) -> crate::Result<()> {
    let mut count = 0usize;
    let lengths = [
        plan.cleanup.len(),
        plan.request.ownership_intents.len(),
        plan.request
            .predecessor()?
            .map_or(0, |p| p.inventory.resources.len()),
        plan.warnings.len(),
    ];
    for length in lengths.into_iter().chain(
        plan.request
            .ownership_intents
            .iter()
            .map(|i| i.approval.permitted_runtime_fields.len()),
    ) {
        add_source_elements(&mut count, length)?;
    }
    if let Some(previous) = previous {
        count_metadata(&mut count, previous)?;
    }
    count_metadata(&mut count, next)
}

fn count_metadata(
    count: &mut usize,
    metadata: &coordination::ReviewMetadataV1,
) -> crate::Result<()> {
    let deployment = &metadata.deployment_permissions;
    let compatibility = &metadata.compatibility;
    let enforcement = &metadata.enforcement;
    let lengths = [
        metadata.resources.len(),
        metadata.inputs.len(),
        metadata.reactions.len(),
        metadata.runtime_permissions.len(),
        metadata.cleanup_permissions.len(),
        deployment.required_resources.len(),
        deployment.creation_targets.len(),
        deployment.role_delegation.len(),
        deployment.limitations.len(),
        compatibility.required_runtime_apis.len(),
        compatibility.required_cleanup_apis.len(),
        compatibility.required_deployment_apis.len(),
        compatibility.qualified_create_precondition_servers.len(),
        enforcement.gaps.len(),
        enforcement.indirect_risks.len(),
        enforcement.runtime_ceiling.capabilities.len(),
        enforcement.cleanup_ceiling.capabilities.len(),
        enforcement.runtime_ceiling.accepted_enforcement_gaps.len(),
        enforcement.runtime_ceiling.accepted_indirect_risks.len(),
        enforcement.cleanup_ceiling.accepted_enforcement_gaps.len(),
        enforcement.cleanup_ceiling.accepted_indirect_risks.len(),
    ];
    for length in lengths
        .into_iter()
        .chain(metadata.resources.iter().map(|r| r.apply_fields.len()))
        .chain(metadata.inputs.iter().map(|i| i.projection.len()))
        .chain(metadata.reactions.iter().map(|r| r.input_ids.len()))
        .chain(metadata.reactions.iter().map(|r| r.effect_fields.len()))
        .chain(metadata.runtime_permissions.iter().map(|c| c.names.len()))
        .chain(metadata.cleanup_permissions.iter().map(|c| c.names.len()))
        .chain(
            deployment
                .required_resources
                .iter()
                .map(|r| r.resource_names.len()),
        )
        .chain(
            deployment
                .role_delegation
                .iter()
                .map(|r| r.granted_capabilities.len()),
        )
        .chain(
            deployment
                .role_delegation
                .iter()
                .flat_map(|r| &r.granted_capabilities)
                .map(|c| c.names.len()),
        )
        .chain(
            enforcement
                .runtime_ceiling
                .capabilities
                .iter()
                .map(|c| c.names.len()),
        )
        .chain(
            enforcement
                .cleanup_ceiling
                .capabilities
                .iter()
                .map(|c| c.names.len()),
        )
    {
        add_source_elements(count, length)?;
    }
    Ok(())
}

fn add_source_elements(count: &mut usize, length: usize) -> crate::Result<()> {
    *count = count.checked_add(length).ok_or(LIMIT)?;
    if *count > MAX_SOURCE_ELEMENTS {
        return Err(LIMIT.into());
    }
    Ok(())
}

pub(super) fn check(value: &impl Serialize) -> crate::Result<()> {
    let mut budget = Budget {
        remaining: MAX_BYTES,
    };
    serde_json::to_writer(&mut budget, value).map_err(|_| LIMIT)?;
    Ok(())
}

struct Budget {
    remaining: usize,
}
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other(LIMIT))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_guard_rejects_limit_and_arithmetic_overflow() -> crate::Result<()> {
        let mut count = 0;
        add_source_elements(&mut count, MAX_SOURCE_ELEMENTS)?;
        assert!(add_source_elements(&mut count, 1).is_err());
        assert!(add_source_elements(&mut count, usize::MAX).is_err());
        Ok(())
    }

    #[test]
    fn byte_budget_accounts_for_json_escaping_and_exact_boundary() -> crate::Result<()> {
        let exact = "a".repeat(MAX_BYTES - 2);
        check(&exact)?;
        assert!(check(&"a".repeat(MAX_BYTES - 1)).is_err());
        assert!(check(&"\0".repeat(MAX_BYTES / 6 + 1)).is_err());
        Ok(())
    }
}
