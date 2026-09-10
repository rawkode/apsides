//! Deployment declaration review of a release-bound plan and authenticated metadata.
#[path = "review_budget.rs"]
mod budget;
#[path = "review_declarations.rs"]
mod declarations;
#[path = "review_requirements.rs"]
mod requirements;
#[path = "review_resources.rs"]
mod resources;

#[path = "review_live.rs"]
mod live;
pub(crate) use live::LiveReview;

use coordination::{ReviewMetadataV1, ReviewRef};
use declarations::{Changes, Declarations, Unavailable};
use serde::Serialize;
use serde_json::Value;

pub(crate) fn transition_review_with_live(
    resolved: &platform::ResolvedTransition,
    live: &LiveReview,
) -> crate::Result<Value> {
    live.validate_resolved(resolved)?;
    render_review(
        resolved.stored(),
        resolved.plan(),
        resolved.transition_id(),
        resolved.next_review_metadata(),
        resolved.previous_review_metadata(),
        Some(live),
    )
}

pub(crate) fn preflight_with_live(
    plan: &deploy::TransitionPlan,
    previous: Option<&ReviewMetadataV1>,
    live: &LiveReview,
) -> crate::Result<()> {
    let next = platform::project_review_metadata(plan)?;
    validate_metadata(plan, previous, &next)?;
    live.validate(plan, previous, &next)?;
    budget::count(plan, previous, &next)?;
    with_report(plan, previous, &next, Some(live), |report| {
        budget::check(report)
    })
}

/// Refuse an incomplete report before the caller publishes claims or reserves a transition.
/// The predecessor body must have been loaded through its protected active reference.
pub(crate) fn preflight(
    plan: &deploy::TransitionPlan,
    previous: Option<&ReviewMetadataV1>,
) -> crate::Result<()> {
    let next = platform::project_review_metadata(plan)?;
    validate_metadata(plan, previous, &next)?;
    budget::count(plan, previous, &next)?;
    with_report(plan, previous, &next, None, |report| budget::check(report))
}

#[cfg(test)]
pub(crate) fn transition_review_fixture(
    current: &platform::StoredTransition,
    plan: &deploy::TransitionPlan,
    transition: &str,
    previous: Option<&ReviewMetadataV1>,
) -> crate::Result<Value> {
    let next = platform::project_review_metadata(plan)?;
    render_review(current, plan, transition, &next, previous, None)
}

#[cfg(test)]
pub(crate) fn transition_review_fixture_with_live(
    current: &platform::StoredTransition,
    plan: &deploy::TransitionPlan,
    transition: &str,
    previous: Option<&ReviewMetadataV1>,
    live: &LiveReview,
) -> crate::Result<Value> {
    let next = platform::project_review_metadata(plan)?;
    render_review(current, plan, transition, &next, previous, Some(live))
}

fn render_review(
    current: &platform::StoredTransition,
    plan: &deploy::TransitionPlan,
    transition: &str,
    next: &ReviewMetadataV1,
    previous: Option<&ReviewMetadataV1>,
    live: Option<&LiveReview>,
) -> crate::Result<Value> {
    if plan.request.transition_id != transition
        || current.record.transition_id != transition
        || plan.generation_reservation != *current.allocation()
        || plan.plan_digest != current.plan_digest()
    {
        return Err("review requires the exact resolved transition plan".into());
    }
    validate_metadata(plan, previous, next)?;
    if current.next_review_metadata != next.reference()?
        || current.previous_review_metadata != reference(previous)?
    {
        return Err("review metadata differs from its protected transition references".into());
    }
    deploy::verify_record(plan, &current.record)?;
    validate_predecessor_checkpoint(current, plan)?;
    budget::count(plan, previous, next)?;
    if let Some(live) = live {
        live.validate(plan, previous, next)?;
    }
    with_report(plan, previous, next, live, |report| {
        budget::check(report)?;
        Ok(serde_json::to_value(report)?)
    })
}

fn reference(value: Option<&ReviewMetadataV1>) -> crate::Result<Option<ReviewRef>> {
    value
        .map(ReviewMetadataV1::reference)
        .transpose()
        .map_err(Into::into)
}

fn validate_metadata(
    plan: &deploy::TransitionPlan,
    previous: Option<&ReviewMetadataV1>,
    next: &ReviewMetadataV1,
) -> crate::Result<()> {
    next.validate()?;
    if platform::project_review_metadata(plan)? != *next {
        return Err("review requires the canonical next declaration metadata".into());
    }
    match (plan.request.predecessor(), previous) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err("initial review cannot contain predecessor metadata".into()),
        (Some(_), None) => Err("upgrade review requires predecessor declaration metadata".into()),
        (Some(predecessor), Some(metadata)) => {
            metadata.validate()?;
            let allocation = &predecessor.allocation;
            let scope = &metadata.scope;
            if scope.claim_scope.registry_uid != allocation.registry_uid
                || scope.sequence != allocation.sequence
                || scope.transition_id != allocation.transition_id
                || scope.domain_id != plan.request.next.compiled.ir.domain.id
                || scope.claim_scope.cluster_binding != predecessor.scope.cluster
                || scope.claim_scope.control_namespace != predecessor.control_namespace
                || scope.claim_scope.workload_namespace != predecessor.scope.namespace
            {
                return Err(
                    "review predecessor metadata differs from its allocation or scope".into(),
                );
            }
            Ok(())
        }
    }
}

fn validate_predecessor_checkpoint(
    current: &platform::StoredTransition,
    plan: &deploy::TransitionPlan,
) -> crate::Result<()> {
    let Some(predecessor) = plan.request.predecessor() else {
        if current.previous_writer_checkpoint.is_some() {
            return Err("initial review cannot contain predecessor authority".into());
        }
        return Ok(());
    };
    let checkpoint = current
        .previous_writer_checkpoint
        .as_ref()
        .ok_or("upgrade review requires an authenticated predecessor writer checkpoint")?;
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
fn ownership_intents(plan: &deploy::TransitionPlan) -> Vec<OwnershipIntent<'_>> {
    let mut intents: Vec<_> = plan
        .request
        .ownership_intents
        .iter()
        .map(OwnershipIntent::from)
        .collect();
    intents.sort_by_key(|intent| intent.object_name);
    intents
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
    runtime_ceiling: &'a coordination::ReviewCeiling,
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
    reactions: Changes<'a, coordination::ReviewReaction>,
    input_authority: Changes<'a, coordination::ReviewInput>,
    effects: Changes<'a, declarations::Effect<'a>>,
    dataflow: Changes<'a, declarations::Dataflow<'a>>,
    source_locations: Changes<'a, declarations::SourceEntry<'a>>,
    permissions: requirements::Permissions<'a>,
    policy: Policy<'a>,
    enforcement: Changes<'a, coordination::ReviewEnforcement>,
    enforcement_gaps: &'a [String],
    indirect_privileges: &'a [String],
    cleanup: &'a [deploy::CleanupItem],
    warnings: &'a [String],
    irreversible_operations: Vec<&'a deploy::CleanupItem>,
    compatibility: Changes<'a, coordination::ReviewCompatibility>,
    expected_interruption: &'static str,
    live_impact: live::Impact<'a>,
    values_omitted: bool,
    limits: Limits,
}

fn singleton_changes<'a, T: PartialEq + Serialize>(
    before: Option<&'a T>,
    after: &'a T,
    key: &'static str,
) -> crate::Result<Changes<'a, T>> {
    Changes::keyed(
        before.map(std::slice::from_ref),
        std::slice::from_ref(after),
        |_| key,
    )
}

fn review_resources<'a>(
    plan: &'a deploy::TransitionPlan,
    previous: Option<&'a ReviewMetadataV1>,
    next: &'a ReviewMetadataV1,
) -> crate::Result<(Vec<resources::ResourceChange<'a>>, Vec<OwnershipIntent<'a>>)> {
    let resources = resources::resources(
        previous,
        next,
        plan.request.predecessor().map(|value| &value.inventory),
        &plan.request.ownership_intents,
    )?;
    Ok((resources, ownership_intents(plan)))
}

const fn expected_interruption(initial: bool) -> &'static str {
    if initial {
        "initial reconciliation starts after activation"
    } else {
        "reconciliation pauses from old proxy freeze until new generation activation; workloads remain running subject to declared changes"
    }
}

fn with_report<T>(
    plan: &deploy::TransitionPlan,
    previous: Option<&ReviewMetadataV1>,
    next: &ReviewMetadataV1,
    live: Option<&LiveReview>,
    consume: impl FnOnce(&Report<'_>) -> crate::Result<T>,
) -> crate::Result<T> {
    let initial = previous.is_none();
    let before_declarations = previous.map(Declarations::new);
    let after_declarations = Declarations::new(next);
    let (resources, ownership_intents) = review_resources(plan, previous, next)?;
    let empty: &[&str] = &[];
    let reactions = Changes::keyed(
        previous.map(|value| value.reactions.as_slice()),
        &next.reactions,
        |value| value.id.clone(),
    )?;
    let input_authority = Changes::keyed(
        previous.map(|value| value.inputs.as_slice()),
        &next.inputs,
        |value| value.id.clone(),
    )?;
    let effects = Changes::keyed(
        before_declarations
            .as_ref()
            .map(|value| value.effects.as_slice()),
        &after_declarations.effects,
        |value| value.id.clone(),
    )?;
    let dataflow = Changes::keyed(
        before_declarations
            .as_ref()
            .map(|value| value.dataflow.as_slice()),
        &after_declarations.dataflow,
        |value| value.reaction.to_owned(),
    )?;
    let source_locations = Changes::keyed(
        before_declarations
            .as_ref()
            .map(|value| value.sources.as_slice()),
        &after_declarations.sources,
        |value| (value.category.to_owned(), value.id.to_owned()),
    )?;
    let compatibility = singleton_changes(
        previous.map(|value| &value.compatibility),
        &next.compatibility,
        "compatibility",
    )?;
    let permissions = requirements::Permissions::new(previous, next)?;
    let enforcement = singleton_changes(
        previous.map(|value| &value.enforcement),
        &next.enforcement,
        "enforcement",
    )?;
    consume(&Report {
        schema_version: if live.is_some() {
            "apsides.dev/deployment-review/v5"
        } else {
            "apsides.dev/deployment-review/v4"
        },
        detail_availability: if initial {
            "initial"
        } else {
            "authenticated-declaration-metadata"
        },
        detail_limitations: empty,
        plan_digest: &plan.plan_digest,
        action_semantics: "planned",
        resources: &resources,
        ownership_intents,
        ownership_mode: "explicit-protected-approval-required-for-existing-objects",
        reactions,
        input_authority,
        effects,
        dataflow,
        source_locations,
        permissions,
        policy: Policy {
            runtime_ceiling: &next.enforcement.runtime_ceiling,
            decision: "approved-by-transition-planner",
        },
        enforcement,
        enforcement_gaps: &next.enforcement.gaps,
        indirect_privileges: &next.enforcement.indirect_risks,
        cleanup: &plan.cleanup,
        warnings: &plan.warnings,
        irreversible_operations: plan
            .cleanup
            .iter()
            .filter(|item| item.policy == compiler_core::DeletionPolicy::Delete)
            .collect(),
        compatibility,
        expected_interruption: expected_interruption(initial),
        live_impact: live.map_or_else(
            || live::Impact::Unavailable(Unavailable::live()),
            live::Impact::Current,
        ),
        values_omitted: true,
        limits: Limits {
            projection_source_element_limit: budget::MAX_SOURCE_ELEMENTS,
            report_byte_limit: budget::MAX_BYTES,
            overflow: "incomplete-review-error; never truncated",
        },
    })
}
