//! Bound source elements before allocating the derived projection, then bound serialized bytes.
//! The source guard counts catalog rows, field paths, input projections/references,
//! capability names, ownership paths, predecessor inventory, and policy/warning lists.
//! Repeated change arrays and derived permission, compatibility, and location rows
//! are covered by the byte limit; the source guard is not an output-entry limit.
use serde::Serialize;
use std::io::{self, Write};

pub(super) const MAX_SOURCE_ELEMENTS: usize = 100_000;
pub(super) const MAX_BYTES: usize = 8 * 1024 * 1024;
const LIMIT: &str = "deployment review incomplete: metadata projection limit exceeded";

pub(super) fn count(plan: &deploy::TransitionPlan) -> crate::Result<()> {
    let next = &plan.request.next.compiled;
    let mut count = 0usize;
    let lengths = [
        next.ir.resources.len(),
        next.ir.inputs.len(),
        next.ir.reactions.len(),
        next.effects.len(),
        next.capabilities.len(),
        plan.cleanup.len(),
        plan.cleanup_capabilities.len(),
        plan.request.ownership_intents.len(),
        plan.request
            .predecessor()?
            .map_or(0, |p| p.inventory.resources.len()),
        next.enforcement_gaps.len(),
        next.indirect_risks.len(),
        plan.warnings.len(),
        plan.request.runtime_ceiling.accepted_enforcement_gaps.len(),
        plan.request.runtime_ceiling.accepted_indirect_risks.len(),
        plan.request.cleanup_ceiling.accepted_enforcement_gaps.len(),
        plan.request.cleanup_ceiling.accepted_indirect_risks.len(),
    ];
    for length in lengths
        .into_iter()
        .chain(next.effects.iter().map(|e| e.field_paths.len()))
        .chain(next.ir.inputs.iter().map(|i| i.projection.len()))
        .chain(next.ir.reactions.iter().map(|r| r.inputs.len()))
        .chain(next.capabilities.iter().map(|c| c.names.len()))
        .chain(plan.cleanup_capabilities.iter().map(|c| c.names.len()))
        .chain(
            plan.request
                .ownership_intents
                .iter()
                .map(|i| i.approval.permitted_runtime_fields.len()),
        )
        .chain(
            plan.request
                .runtime_ceiling
                .capabilities
                .iter()
                .map(|c| c.names.len() + 1),
        )
        .chain(
            plan.request
                .cleanup_ceiling
                .capabilities
                .iter()
                .map(|c| c.names.len() + 1),
        )
    {
        add_source_elements(&mut count, length)?;
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
