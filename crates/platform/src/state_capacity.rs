//! Canonical accounting for protected state. Capacity never grants lifecycle authority.
use crate::{EvidenceProfile, MAX_STATE_BYTES, NamespaceRegistryState, Result, invalid};
use compiler_core::canonical_bytes;
use deploy::Phase;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const CAPACITY_SCHEMA: &str = "reakt.dev/capacity-admission/v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapacityAdmission {
    pub schema_version: String,
    #[serde(with = "deploy::allocation_counter")]
    pub revision: u64,
    pub profile: EvidenceProfile,
    pub domain_bytes: u64,
    pub activation_bytes: u64,
    pub inventory_bytes: u64,
}

impl CapacityAdmission {
    /// In-memory maximum-width scaffold for envelope calculation. This is not
    /// an admission result and must never be published as a pending capacity.
    #[cfg(feature = "deployer")]
    pub(crate) fn projection_placeholder(profile: EvidenceProfile) -> Self {
        Self {
            schema_version: CAPACITY_SCHEMA.into(),
            revision: u64::MAX,
            profile,
            domain_bytes: MAX_STATE_BYTES as u64,
            activation_bytes: MAX_STATE_BYTES as u64,
            inventory_bytes: MAX_STATE_BYTES as u64,
        }
    }
    /// Validate only the admission shape. The selected plan must independently
    /// establish that these ceilings cover its complete lifecycle.
    /// # Errors
    /// Rejects unknown schemas, invalid profiles and out-of-range ceilings.
    pub fn validate(&self) -> Result<()> {
        self.profile.validate()?;
        if self.schema_version != CAPACITY_SCHEMA
            || [
                self.domain_bytes,
                self.activation_bytes,
                self.inventory_bytes,
            ]
            .into_iter()
            .any(|bytes| bytes == 0 || bytes > MAX_STATE_BYTES as u64)
        {
            return Err(invalid("capacity admission schema or byte ceiling invalid"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct RegistryFootprint {
    pub fixed_bytes: usize,
    pub domain_bytes: BTreeMap<String, usize>,
}

fn add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| invalid("registry capacity arithmetic overflow"))
}

fn entry_bytes<T: Serialize>(key: &str, value: &T) -> Result<usize> {
    // A singleton includes two braces; the skeleton owns those braces. Keep
    // one byte to cover a separator regardless of this entry's map position.
    canonical_bytes(&BTreeMap::from([(key, value)]))?
        .len()
        .checked_sub(1)
        .ok_or_else(|| invalid("registry capacity map encoding invalid"))
}

pub(crate) fn measure(state: &NamespaceRegistryState) -> Result<RegistryFootprint> {
    let mut skeleton = state.clone();
    skeleton.histories.clear();
    skeleton.active_writer_checkpoints.clear();
    skeleton.audit_heads.clear();
    skeleton.transitions.clear();
    // The namespace counter may grow while another domain owns reserved space.
    skeleton.next_sequence = u64::MAX;
    let mut footprint = RegistryFootprint {
        fixed_bytes: canonical_bytes(&skeleton)?.len(),
        domain_bytes: BTreeMap::new(),
    };
    for (domain, history) in &state.histories {
        if history.domain_id != *domain {
            return Err(invalid("capacity history domain mismatch"));
        }
        footprint
            .domain_bytes
            .insert(domain.clone(), entry_bytes(domain, history)?);
    }
    let mut charge = |domain: &str, bytes: usize| -> Result<()> {
        let current = footprint
            .domain_bytes
            .get_mut(domain)
            .ok_or_else(|| invalid("capacity projection contains an orphan domain"))?;
        *current = add(*current, bytes)?;
        Ok(())
    };
    for (domain, checkpoint) in &state.active_writer_checkpoints {
        charge(domain, entry_bytes(domain, checkpoint)?)?;
    }
    for (domain, head) in &state.audit_heads {
        charge(domain, entry_bytes(domain, head)?)?;
    }
    for (transition, stored) in &state.transitions {
        charge(stored.domain_id(), entry_bytes(transition, stored)?)?;
    }
    Ok(footprint)
}

#[cfg(any(feature = "deployer", test))]
pub(crate) fn domain_bytes(state: &NamespaceRegistryState, domain: &str) -> Result<usize> {
    measure(state)?
        .domain_bytes
        .get(domain)
        .copied()
        .ok_or_else(|| invalid("capacity domain is absent"))
}

fn admitted_total(
    state: &NamespaceRegistryState,
    replacement: Option<(&str, usize)>,
) -> Result<usize> {
    let footprint = measure(state)?;
    let mut pending = BTreeMap::new();
    for stored in state.transitions.values() {
        stored.capacity.validate()?;
        if stored.record.phase != Phase::Committed {
            let ceiling = usize::try_from(stored.capacity.domain_bytes)
                .map_err(|_| invalid("capacity ceiling cannot be represented"))?;
            let actual = footprint
                .domain_bytes
                .get(stored.domain_id())
                .ok_or_else(|| invalid("pending capacity domain is absent"))?;
            // Candidate construction replaces the ceiling below; all other
            // pending domains must remain within their own admitted space.
            if replacement.is_none_or(|(domain, _)| domain != stored.domain_id())
                && ceiling < *actual
            {
                return Err(invalid(
                    "pending domain exceeds its admitted completion capacity",
                ));
            }
            if pending.insert(stored.domain_id(), ceiling).is_some() {
                return Err(invalid(
                    "capacity domain has more than one pending transition",
                ));
            }
        }
    }
    if let Some((domain, ceiling)) = replacement {
        if ceiling == 0 || ceiling > MAX_STATE_BYTES || !footprint.domain_bytes.contains_key(domain)
        {
            return Err(invalid("candidate capacity domain or ceiling invalid"));
        }
        if footprint
            .domain_bytes
            .get(domain)
            .is_some_and(|actual| ceiling < *actual)
        {
            return Err(invalid(
                "candidate domain exceeds its proposed completion capacity",
            ));
        }
        pending.insert(domain, ceiling);
    }
    footprint
        .domain_bytes
        .iter()
        .try_fold(footprint.fixed_bytes, |total, (domain, actual)| {
            add(
                total,
                (*actual).max(pending.get(domain.as_str()).copied().unwrap_or(0)),
            )
        })
}

pub(crate) fn validate_reserved_capacity(state: &NamespaceRegistryState) -> Result<()> {
    if admitted_total(state, None)? > MAX_STATE_BYTES {
        return Err(invalid(
            "registry completion capacity exceeds 512 KiB; split or reduce domains",
        ));
    }
    Ok(())
}

#[cfg(any(feature = "deployer", test))]
pub(crate) fn validate_domain_candidate(
    state: &NamespaceRegistryState,
    domain: &str,
    ceiling: usize,
) -> Result<()> {
    if admitted_total(state, Some((domain, ceiling)))? > MAX_STATE_BYTES {
        return Err(invalid(
            "candidate completion capacity exceeds 512 KiB; split or reduce domains",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use coordination::{ClaimScope, ClaimSnapshotV1};
    use deploy::GenerationHistory;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn state(domains: &[&str]) -> TestResult<NamespaceRegistryState> {
        let scope = ClaimScope {
            registry_uid: "11111111-1111-4111-8111-111111111111".into(),
            cluster_binding: "cluster-a".into(),
            control_namespace: "control".into(),
            workload_namespace: "workload".into(),
        };
        let reference = ClaimSnapshotV1 {
            schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
            scope: scope.clone(),
            claims: vec![],
        }
        .reference()?;
        Ok(NamespaceRegistryState {
            registry_uid: scope.registry_uid,
            schema_version: crate::REGISTRY_SCHEMA_VERSION.into(),
            cluster_binding: scope.cluster_binding.clone(),
            workload_namespace: scope.workload_namespace,
            next_sequence: 1,
            claim_snapshot: reference,
            histories: domains
                .iter()
                .map(|domain| {
                    (
                        (*domain).to_owned(),
                        GenerationHistory {
                            domain_id: (*domain).into(),
                            cluster_binding: scope.cluster_binding.clone(),
                            revision: "escaped-\"-\\-\n".into(),
                            active_allocation: None,
                        },
                    )
                })
                .collect(),
            active_writer_checkpoints: BTreeMap::new(),
            audit_heads: BTreeMap::new(),
            transitions: BTreeMap::new(),
        })
    }

    #[test]
    fn canonical_partition_counts_escaped_entries_and_counter_growth() -> TestResult {
        for domains in [
            &[][..],
            &["a"][..],
            &["a", "b"][..],
            &["quoted-\"", "slash-\\"][..],
        ] {
            let mut state = state(domains)?;
            for sequence in [1, 999, u64::MAX] {
                state.next_sequence = sequence;
                let measured = measure(&state)?;
                let accounted = measured
                    .domain_bytes
                    .values()
                    .try_fold(measured.fixed_bytes, |sum, bytes| add(sum, *bytes))?;
                let separators = usize::from(!state.histories.is_empty());
                let counter_growth = u64::MAX.to_string().len() - sequence.to_string().len();
                assert_eq!(
                    accounted,
                    canonical_bytes(&state)?.len() + separators + counter_growth
                );
            }
        }
        Ok(())
    }

    #[test]
    fn candidate_uses_whole_domain_ceiling_without_double_counting() -> TestResult {
        let state = state(&["a", "b"])?;
        let footprint = measure(&state)?;
        let other = footprint
            .domain_bytes
            .get("b")
            .copied()
            .ok_or("domain b missing")?;
        let ceiling = MAX_STATE_BYTES
            .checked_sub(footprint.fixed_bytes + other)
            .ok_or("fixture exceeds capacity")?;
        validate_domain_candidate(&state, "a", ceiling)?;
        assert!(validate_domain_candidate(&state, "a", ceiling + 1).is_err());
        let actual = domain_bytes(&state, "a")?;
        assert!(validate_domain_candidate(&state, "a", actual - 1).is_err());
        assert!(validate_domain_candidate(&state, "missing", ceiling).is_err());
        Ok(())
    }

    #[test]
    fn accounting_rejects_orphans_and_arithmetic_overflow() -> TestResult {
        let mut state = state(&["a"])?;
        state
            .histories
            .get_mut("a")
            .ok_or("history missing")?
            .domain_id = "foreign".into();
        assert!(measure(&state).is_err());
        assert!(add(usize::MAX, 1).is_err());
        Ok(())
    }
}
