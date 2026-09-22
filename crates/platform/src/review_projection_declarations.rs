use crate::{Result, invalid};
use compiler_core::{CompiledPlan, Effect, SourceLocation};
use coordination::{
    REVIEW_ROLLOUT_FIELD, ReviewDeletionPolicy, ReviewIdentity, ReviewInput, ReviewInputMode,
    ReviewLifecycle, ReviewOwnershipPlan, ReviewPrimitive, ReviewReaction, ReviewResource,
    ReviewSource,
};
use deploy::{OwnershipIntent, TransitionPlan};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Declarations {
    pub resources: Vec<ReviewResource>,
    pub inputs: Vec<ReviewInput>,
    pub reactions: Vec<ReviewReaction>,
}
struct Effects<'a> {
    apply: BTreeMap<&'a str, &'a [String]>,
    rollout: BTreeMap<&'a str, &'a [String]>,
}

pub(super) fn project(plan: &TransitionPlan) -> Result<Declarations> {
    let compiled = &plan.request.next.compiled;
    let effects = effects(compiled)?;
    let old: BTreeSet<_> = plan
        .request
        .predecessor()
        .into_iter()
        .flat_map(|p| &p.inventory.resources)
        .map(|r| ReviewIdentity {
            api_group: r.identity.api_group.clone(),
            resource: r.identity.resource.clone(),
            namespace: r.identity.namespace.clone(),
            name: r.identity.name.clone(),
        })
        .collect();
    let mut approvals = BTreeMap::new();
    for intent in &plan.request.ownership_intents {
        if approvals
            .insert(approval_identity(intent), intent)
            .is_some()
        {
            return Err(invalid("review ownership approvals are ambiguous"));
        }
    }
    let mut resources = Vec::with_capacity(compiled.ir.resources.len());
    for resource in &compiled.ir.resources {
        let identity = resource_identity(resource)?;
        let prior = old.contains(&identity);
        let ownership_plan = match approvals.remove(&identity) {
            Some(_) if prior => {
                return Err(invalid(
                    "review adoption cannot replace predecessor ownership",
                ));
            }
            Some(intent) => ReviewOwnershipPlan::AdoptionApproved {
                intent_object_name: intent.object_name.clone(),
                object_uid: intent.object_uid.clone(),
                object_resource_version: intent.object_resource_version.clone(),
                observed_uid: intent.approval.observed_uid.clone(),
                observed_resource_version: intent.approval.observed_resource_version.clone(),
            },
            None if prior => ReviewOwnershipPlan::PreviouslyManaged,
            None => ReviewOwnershipPlan::CreateIfAbsent,
        };
        resources.push(ReviewResource {
            id: resource.id.clone(),
            identity,
            api_version: resource.api_version.clone(),
            kind: resource.kind.clone(),
            api_resource: resource.api_resource.as_ref().map(|mapping| {
                coordination::ReviewApiResourceMapping {
                    plural: mapping.plural.clone(),
                    scope: match mapping.scope {
                        compiler_core::ApiResourceScope::Namespaced => {
                            coordination::ReviewApiResourceScope::Namespaced
                        }
                    },
                    schema_sha256: mapping.schema_sha256.clone(),
                }
            }),
            apply_fields: effects
                .apply
                .get(resource.id.as_str())
                .ok_or_else(|| invalid("review apply catalog is incomplete"))?
                .to_vec(),
            lifecycle: ReviewLifecycle {
                deletion_policy: match resource.lifecycle.deletion_policy {
                    compiler_core::DeletionPolicy::Orphan => ReviewDeletionPolicy::Orphan,
                    compiler_core::DeletionPolicy::Delete => ReviewDeletionPolicy::Delete,
                },
            },
            force: resource.force,
            source: source(&resource.source),
            ownership_plan,
        });
    }
    if !approvals.is_empty() {
        return Err(invalid("review approval has no current resource"));
    }
    resources.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(Declarations {
        resources,
        inputs: inputs(compiled),
        reactions: reactions(compiled, &effects)?,
    })
}
fn inputs(compiled: &CompiledPlan) -> Vec<ReviewInput> {
    let mut inputs: Vec<_> = compiled
        .ir
        .inputs
        .iter()
        .map(|i| ReviewInput {
            id: i.id.clone(),
            api_version: i.api_version.clone(),
            kind: i.kind.clone(),
            name: i.name.clone(),
            namespace: i.namespace.clone(),
            mode: match i.mode {
                compiler_core::InputMode::Managed => ReviewInputMode::Managed,
                compiler_core::InputMode::Observed => ReviewInputMode::Observed,
            },
            managed_resource_id: i.managed_resource_id.clone(),
            projection: i.projection.clone(),
            sensitive: i.sensitive,
            missing: i.missing.clone(),
            source: source(&i.source),
        })
        .collect();
    inputs.sort_by(|a, b| a.id.cmp(&b.id));
    inputs
}
fn reactions(compiled: &CompiledPlan, effects: &Effects<'_>) -> Result<Vec<ReviewReaction>> {
    let mut reactions = Vec::with_capacity(compiled.ir.reactions.len());
    for reaction in &compiled.ir.reactions {
        if reaction.op != "rollout" {
            return Err(invalid("review reaction primitive is unsupported"));
        }
        let mut input_ids = reaction.inputs.clone();
        input_ids.sort();
        reactions.push(ReviewReaction {
            id: reaction.id.clone(),
            op: ReviewPrimitive::Rollout,
            input_ids,
            target_resource_id: reaction.target.clone(),
            effect_fields: effects
                .rollout
                .get(reaction.id.as_str())
                .ok_or_else(|| invalid("review rollout catalog is incomplete"))?
                .to_vec(),
            source: source(&reaction.source),
        });
    }
    reactions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(reactions)
}

fn source(location: &SourceLocation) -> ReviewSource {
    ReviewSource {
        file: location.file.clone(),
        line: location.line,
        column: location.column,
    }
}
fn approval_identity(intent: &OwnershipIntent) -> ReviewIdentity {
    let id = &intent.approval.identity;
    ReviewIdentity {
        api_group: id.api_group.clone(),
        resource: id.resource.clone(),
        namespace: id.namespace.clone(),
        name: id.name.clone(),
    }
}
fn resource_identity(resource: &compiler_core::Resource) -> Result<ReviewIdentity> {
    let identity = deploy::resource_identity(resource)
        .map_err(|_| invalid("review managed API is unsupported"))?;
    Ok(ReviewIdentity {
        api_group: identity.api_group,
        resource: identity.resource,
        namespace: identity.namespace,
        name: identity.name,
    })
}
fn effects(compiled: &CompiledPlan) -> Result<Effects<'_>> {
    let mut remaining: Vec<_> = compiled.effects.iter().collect();
    let mut apply = BTreeMap::new();
    let mut rollout = BTreeMap::new();
    for resource in &compiled.ir.resources {
        let effect = take_effect(
            &mut remaining,
            "server-side-apply",
            &resource.id,
            &resource.source,
        )?;
        apply.insert(resource.id.as_str(), effect.field_paths.as_slice());
    }
    for reaction in &compiled.ir.reactions {
        let effect = take_effect(
            &mut remaining,
            "rollout-contribution",
            &reaction.target,
            &reaction.source,
        )?;
        if effect.field_paths != [REVIEW_ROLLOUT_FIELD] {
            return Err(invalid("review rollout field contract is unsupported"));
        }
        rollout.insert(reaction.id.as_str(), effect.field_paths.as_slice());
    }
    if !remaining.is_empty() {
        return Err(invalid("review effect catalog is ambiguous"));
    }
    Ok(Effects { apply, rollout })
}
fn take_effect<'a>(
    remaining: &mut Vec<&'a Effect>,
    operation: &str,
    target: &str,
    source: &SourceLocation,
) -> Result<&'a Effect> {
    let index = remaining
        .iter()
        .position(|e| e.operation == operation && e.target == target && e.source == *source)
        .ok_or_else(|| invalid("review effect catalog is incomplete"))?;
    Ok(remaining.remove(index))
}
