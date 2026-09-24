//! The review policy `aps diff --policy` evaluates against a [`SurfaceDiff`].
//!
//! A policy names the diff conditions that require human approval before a
//! head surface can be deployed. The rule vocabulary is fixed: `egress.added`
//! and `handlers.added` are accepted today so a policy file written against
//! this schema version keeps parsing once Phase 4 adds an `egress` and a
//! `handlers` section to [`SurfaceDiff`] — until then those two rules simply
//! never trigger, since there is nothing yet for them to match.

use crate::json::parse_strict;
use crate::surface_diff::SurfaceDiff;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// The `apsides.surface-policy.json` schema version this module accepts.
pub const SURFACE_POLICY_SCHEMA: &str = "apsides.dev/surface-policy/v1";

/// One diff condition that requires human approval. `EgressAdded` and
/// `HandlersAdded` never trigger yet: Phase 2's [`SurfaceDiff`] has no
/// `egress` or `handlers` section for them to match against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PolicyRule {
    #[serde(rename = "egress.added")]
    EgressAdded,
    #[serde(rename = "secretsRead.added")]
    SecretsReadAdded,
    #[serde(rename = "permissions.added")]
    PermissionsAdded,
    #[serde(rename = "permissions.changed")]
    PermissionsChanged,
    #[serde(rename = "images.unpinned")]
    ImagesUnpinned,
    #[serde(rename = "kubernetes.changed")]
    KubernetesChanged,
    #[serde(rename = "handlers.added")]
    HandlersAdded,
    #[serde(rename = "resources.removed")]
    ResourcesRemoved,
    #[serde(rename = "risks.added")]
    RisksAdded,
}

impl PolicyRule {
    /// The wire name this rule serializes to, for reporting which rules
    /// triggered without re-serializing a whole policy document.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EgressAdded => "egress.added",
            Self::SecretsReadAdded => "secretsRead.added",
            Self::PermissionsAdded => "permissions.added",
            Self::PermissionsChanged => "permissions.changed",
            Self::ImagesUnpinned => "images.unpinned",
            Self::KubernetesChanged => "kubernetes.changed",
            Self::HandlersAdded => "handlers.added",
            Self::ResourcesRemoved => "resources.removed",
            Self::RisksAdded => "risks.added",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfacePolicy {
    pub schema_version: String,
    pub require_approval: Vec<PolicyRule>,
}

/// Parse a strict-JSON surface review policy.
///
/// # Errors
/// Rejects malformed JSON, unknown fields, an unrecognized rule name, and a
/// mismatched `schemaVersion`.
pub fn parse_policy(input: &str) -> Result<SurfacePolicy> {
    let policy: SurfacePolicy = serde_json::from_value(parse_strict(input)?)
        .map_err(|error| Error::Invalid(format!("surface policy schema mismatch: {error}")))?;
    if policy.schema_version != SURFACE_POLICY_SCHEMA {
        return Err(Error::Invalid(format!(
            "surface policy schemaVersion must be {SURFACE_POLICY_SCHEMA}"
        )));
    }
    Ok(policy)
}

/// Whether `rule` matches a change present in `diff`.
fn matches(rule: PolicyRule, diff: &SurfaceDiff) -> bool {
    match rule {
        PolicyRule::EgressAdded | PolicyRule::HandlersAdded => false,
        PolicyRule::SecretsReadAdded => !diff.secrets_read.added.is_empty(),
        PolicyRule::PermissionsAdded => !diff.permissions.added.is_empty(),
        PolicyRule::PermissionsChanged => !diff.permissions.changed.is_empty(),
        PolicyRule::ImagesUnpinned => {
            diff.images.added.iter().any(|image| !image.pinned)
                || diff.images.changed.iter().any(|change| change.unpinned)
        }
        PolicyRule::KubernetesChanged => diff.kubernetes.is_some(),
        PolicyRule::ResourcesRemoved => !diff.resources.removed.is_empty(),
        PolicyRule::RisksAdded => !diff.risks.added.is_empty(),
    }
}

/// Every rule in `policy.require_approval` that matches a change in `diff`,
/// in the policy's own declared order with duplicates removed.
#[must_use]
pub fn evaluate_policy(policy: &SurfacePolicy, diff: &SurfaceDiff) -> Vec<PolicyRule> {
    let mut triggered = Vec::new();
    for rule in &policy.require_approval {
        if matches(*rule, diff) && !triggered.contains(rule) {
            triggered.push(*rule);
        }
    }
    triggered
}
