//! Choosing a transition identity and deciding what already exists under it.

use crate::Result;
use deploy::Phase;
use std::collections::BTreeSet;

pub(super) fn verify_resumed_intents(
    names: &[String],
    stored: Option<&platform::StoredTransition>,
) -> Result<()> {
    if let (false, Some(stored)) = (names.is_empty(), stored) {
        let supplied: BTreeSet<_> = names.iter().map(String::as_str).collect();
        let recorded: BTreeSet<_> = stored
            .inputs
            .ownership_intents
            .iter()
            .map(|i| i.object_name.as_str())
            .collect();
        if supplied != recorded {
            return Err("ownership intents differ from the recorded transition; approvals cannot be replaced during resume".into());
        }
    }
    Ok(())
}

pub(super) fn default_transition(
    registry: &platform::NamespaceRegistry,
    domain: &str,
    release: &str,
    policy: &str,
) -> Result<String> {
    let matching = |s: &&platform::StoredTransition| {
        s.domain_id() == domain && s.allocation().artifact_digest == release
    };
    if let Some(pending) = registry
        .transitions
        .values()
        .filter(matching)
        .find(|s| s.record.phase != Phase::Committed)
    {
        return Ok(pending.allocation().transition_id.clone());
    }
    if let Some(active) = registry.transitions.values().filter(matching).find(|s| {
        registry
            .histories
            .get(domain)
            .and_then(|h| h.active_allocation.as_ref().map(|a| &a.generation_id))
            == Some(&s.allocation().generation_id)
            && s.runtime_binding
                .as_ref()
                .is_some_and(|b| b.policy_digest == policy)
    }) {
        return Ok(active.allocation().transition_id.clone());
    }
    Ok(deploy::allocation_names(&registry.registry_uid, registry.next_sequence)?.transition_id)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExistingTransition {
    Absent,
    Resume,
    Committed,
}

/// The verified release a deployment routes by.
pub(super) struct Artifact<'a> {
    pub(super) domain: &'a str,
    pub(super) release_digest: &'a str,
    pub(super) ir_digest: &'a str,
}

impl<'a> From<&'a release::IntegrityVerifiedCandidate> for Artifact<'a> {
    fn from(verified: &'a release::IntegrityVerifiedCandidate) -> Self {
        Self {
            domain: &verified.compiled().ir.domain.id,
            release_digest: verified.release_digest(),
            ir_digest: &verified.compiled().ir_digest,
        }
    }
}

/// Whether `transition` is new, an unfinished run of `artifact` this
/// executor resumes from whatever phase its registry write last stored, or
/// already committed and active, in which case the rerun only republishes
/// its inventory. A process stopped after any registry write therefore
/// resumes, including between the Committed write and the inventory
/// publication.
pub(super) fn check_existing(
    registry: &platform::NamespaceRegistry,
    transition: &str,
    artifact: &Artifact<'_>,
    executor: &str,
    policy: &str,
) -> Result<ExistingTransition> {
    let domain = artifact.domain;
    if registry.transitions.values().any(|s| {
        s.domain_id() == domain
            && s.record.phase != Phase::Committed
            && s.allocation().artifact_digest != artifact.release_digest
    }) {
        return Err(
            "domain has an unfinished transition for another artifact; finish or recover it first"
                .into(),
        );
    }
    let Some(stored) = registry.transitions.get(transition) else {
        return Ok(ExistingTransition::Absent);
    };
    if stored.allocation().artifact_digest != artifact.release_digest
        || stored.allocation().ir_digest != artifact.ir_digest
    {
        return Err("transition belongs to another artifact".into());
    }
    if stored.record.phase == Phase::Committed {
        if registry
            .histories
            .get(domain)
            .and_then(|h| h.active_allocation.as_ref().map(|a| &a.generation_id))
            != Some(&stored.allocation().generation_id)
            || stored
                .runtime_binding
                .as_ref()
                .is_none_or(|b| b.policy_digest != policy)
        {
            return Err("completed transition is no longer active under current policy; deploy with a fresh transition identity".into());
        }
        return Ok(ExistingTransition::Committed);
    }
    if stored.record.executor_id != executor {
        return Err(
            "unfinished transition belongs to another executor; protected recovery is required"
                .into(),
        );
    }
    Ok(ExistingTransition::Resume)
}
