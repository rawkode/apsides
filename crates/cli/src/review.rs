//! Deployment declaration review of a release-bound plan and authenticated predecessor metadata.
#[path = "review_budget.rs"]
mod budget;
#[path = "review_declarations.rs"]
mod declarations;
#[path = "review_requirements.rs"]
mod requirements;
#[path = "review_resources.rs"]
mod resources;

use declarations::{Changes, Declarations, Unavailable};
use serde::Serialize;
use serde_json::Value;

pub fn transition_review(resolved: &platform::ResolvedTransition) -> crate::Result<Value> {
    render_review(resolved.stored(), resolved.plan(), resolved.transition_id())
}

/// Refuse an incomplete report before the caller publishes claims or reserves a transition.
/// This checks the canonical projection, not the authority of a deployment record.
pub(crate) fn preflight(plan: &deploy::TransitionPlan) -> crate::Result<()> {
    validate_plan(plan)?;
    with_report(plan, |report| budget::check(report))
}

#[cfg(test)]
pub(crate) fn transition_review_fixture(
    current: &platform::StoredTransition,
    plan: &deploy::TransitionPlan,
    transition: &str,
) -> crate::Result<Value> {
    render_review(current, plan, transition)
}

fn render_review(
    current: &platform::StoredTransition,
    plan: &deploy::TransitionPlan,
    transition: &str,
) -> crate::Result<Value> {
    if plan.request.transition_id != transition
        || current.record.transition_id != transition
        || plan.generation_reservation != *current.allocation()
        || plan.plan_digest != current.plan_digest()
    {
        return Err("review requires the exact resolved transition plan".into());
    }
    validate_plan(plan)?;
    deploy::verify_record(plan, &current.record)?;
    validate_predecessor(current, plan)?;
    with_report(plan, |report| {
        budget::check(report)?;
        Ok(serde_json::to_value(report)?)
    })
}

fn validate_plan(plan: &deploy::TransitionPlan) -> crate::Result<()> {
    budget::count(plan)?;
    platform::project_review_metadata(plan)?;
    Ok(())
}

fn validate_predecessor(
    current: &platform::StoredTransition,
    plan: &deploy::TransitionPlan,
) -> crate::Result<()> {
    let Some(predecessor) = plan.request.predecessor()? else {
        if current.previous_writer_checkpoint.is_some() {
            return Err("initial review cannot contain predecessor authority".into());
        }
        return Ok(());
    };
    let checkpoint = current
        .previous_writer_checkpoint
        .as_ref()
        .ok_or("metadata-only review requires an authenticated predecessor writer checkpoint")?;
    let domain = &plan.request.next.compiled.ir.domain;
    checkpoint.validate(
        predecessor,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnershipIntent<'a> {
    object_name: &'a str,
    object_uid: &'a str,
    object_resource_version: &'a str,
    identity: &'a deploy::ResourceIdentity,
    action: &'a deploy::OwnershipAction,
    observed_uid: &'a str,
    observed_resource_version: &'a str,
    permitted_runtime_fields: &'a [String],
    deletion_policy: compiler_core::DeletionPolicy,
}
impl<'a> From<&'a deploy::OwnershipIntent> for OwnershipIntent<'a> {
    fn from(intent: &'a deploy::OwnershipIntent) -> Self {
        Self {
            object_name: &intent.object_name,
            object_uid: &intent.object_uid,
            object_resource_version: &intent.object_resource_version,
            identity: &intent.approval.identity,
            action: &intent.approval.action,
            observed_uid: &intent.approval.observed_uid,
            observed_resource_version: &intent.approval.observed_resource_version,
            permitted_runtime_fields: &intent.approval.permitted_runtime_fields,
            deletion_policy: intent.approval.deletion_policy,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Policy<'a> {
    runtime_ceiling: &'a compiler_core::PermissionCeiling,
    decision: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Limits {
    projection_source_element_limit: usize,
    report_byte_limit: usize,
    overflow: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a> {
    schema_version: &'static str,
    detail_availability: &'static str,
    detail_limitations: &'a [&'static str],
    plan_digest: &'a str,
    action_semantics: &'static str,
    resources: &'a [resources::ResourceChange<'a>],
    ownership_intents: Vec<OwnershipIntent<'a>>,
    ownership_mode: &'static str,
    reactions: Changes<'a, declarations::Reaction<'a>>,
    input_authority: Changes<'a, declarations::Input<'a>>,
    effects: Changes<'a, declarations::Effect<'a>>,
    dataflow: Changes<'a, declarations::Dataflow<'a>>,
    source_locations: Changes<'a, declarations::SourceEntry<'a>>,
    permissions: requirements::Permissions<'a>,
    policy: Policy<'a>,
    enforcement_gaps: &'a [String],
    indirect_privileges: &'a [String],
    cleanup: &'a [deploy::CleanupItem],
    warnings: &'a [String],
    irreversible_operations: Vec<&'a deploy::CleanupItem>,
    compatibility: Changes<'a, requirements::Compatibility<'a>>,
    expected_interruption: &'static str,
    live_impact: Unavailable,
    values_omitted: bool,
    limits: Limits,
}

fn with_report<T>(
    plan: &deploy::TransitionPlan,
    consume: impl FnOnce(&Report<'_>) -> crate::Result<T>,
) -> crate::Result<T> {
    let next = &plan.request.next.compiled;
    let predecessor = plan.request.predecessor()?;
    let initial = predecessor.is_none();
    let declarations = Declarations::new(next)?;
    let resources = resources::resources(
        predecessor.map(|p| &p.inventory),
        next,
        &plan.request.ownership_intents,
    )?;
    let permissions = platform::deployment_permissions(plan)?;
    let compatibility = [requirements::Compatibility::new(plan, &permissions)];
    let mut ownership_intents: Vec<_> = plan
        .request
        .ownership_intents
        .iter()
        .map(OwnershipIntent::from)
        .collect();
    ownership_intents.sort_by_key(|intent| intent.object_name);
    let limitations: &[&str] = if initial {
        &[]
    } else {
        &[
            "old-field-structure",
            "old-source-locations",
            "old-reactions",
            "old-input-authority",
            "old-effects",
            "old-dataflow",
            "old-runtime-permissions",
            "old-cleanup-permissions",
            "old-deployment-permissions",
            "old-compatibility",
        ]
    };
    consume(&Report {
        schema_version: "reakt.dev/deployment-review/v3",
        detail_availability: if initial { "initial" } else { "metadata-only" },
        detail_limitations: limitations,
        plan_digest: &plan.plan_digest,
        action_semantics: "planned",
        resources: &resources,
        ownership_intents,
        ownership_mode: "explicit-protected-approval-required-for-existing-objects",
        reactions: Changes::new(&declarations.reactions, initial),
        input_authority: Changes::new(&declarations.inputs, initial),
        effects: Changes::new(&declarations.effects, initial),
        dataflow: Changes::new(&declarations.dataflow, initial),
        source_locations: Changes::new(&declarations.sources, initial),
        permissions: requirements::Permissions::new(plan, &permissions, initial),
        policy: Policy {
            runtime_ceiling: &plan.request.runtime_ceiling,
            decision: "approved-by-transition-planner",
        },
        enforcement_gaps: &next.enforcement_gaps,
        indirect_privileges: &next.indirect_risks,
        cleanup: &plan.cleanup,
        warnings: &plan.warnings,
        irreversible_operations: plan
            .cleanup
            .iter()
            .filter(|i| i.policy == compiler_core::DeletionPolicy::Delete)
            .collect(),
        compatibility: Changes::new(&compatibility, initial),
        expected_interruption: if initial {
            "initial reconciliation starts after activation"
        } else {
            "reconciliation pauses from old proxy freeze until new generation activation; workloads remain running subject to declared changes"
        },
        live_impact: Unavailable::live(),
        values_omitted: true,
        limits: Limits {
            projection_source_element_limit: budget::MAX_SOURCE_ELEMENTS,
            report_byte_limit: budget::MAX_BYTES,
            overflow: "incomplete-review-error; never truncated",
        },
    })
}
