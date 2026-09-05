//! Deployment review derived from protected prior and validated next plans.
use compiler_core::{Capability, Resource};
use deploy::{Phase, TransitionPlan};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub fn transition_review(
    registry: &platform::NamespaceRegistry,
    transition: &str,
) -> crate::Result<Value> {
    let plan = &registry.transition(transition)?.plan;
    let next = &plan.request.next.compiled;
    let prior = prior_compiled(registry, plan)?;
    let before = prior.map_or(&[][..], |p| p.ir.resources.as_slice());
    let resource_key = |r: &Resource| (r.namespace.clone(), r.kind.clone(), r.name.clone());
    let old: BTreeMap<_, _> = before.iter().map(|r| (resource_key(r), r)).collect();
    let new: BTreeMap<_, _> = next
        .ir
        .resources
        .iter()
        .map(|r| (resource_key(r), r))
        .collect();
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let resources: Vec<_> = keys.into_iter().map(|key| {
        let left = old.get(key).copied();
        let right = new.get(key).copied();
        let change = match (left, right) {
            (None, _) => "added", (_, None) => "removed",
            (Some(a), Some(b)) if a.desired == b.desired && a.lifecycle == b.lifecycle && a.force == b.force => "retained",
            _ => "changed",
        };
        let empty = json!({});
        let mut fields = Vec::new();
        changed_paths(left.map_or(&empty, |r| &r.desired), right.map_or(&empty, |r| &r.desired), "", &mut fields);
        json!({"namespace":key.0,"kind":key.1,"name":key.2,"change":change,
            "fieldChanges":fields,"beforeSource":left.map(|r| &r.source),"afterSource":right.map(|r| &r.source),
            "beforeLifecycle":left.map(|r| &r.lifecycle),"afterLifecycle":right.map(|r| &r.lifecycle),
            "beforeForce":left.map(|r| r.force),"afterForce":right.map(|r| r.force)})
    }).collect();
    let prior_capabilities = prior.map_or(&[][..], |p| p.capabilities.as_slice());
    Ok(json!({
        "schemaVersion":"reakt.dev/deployment-review/v1",
        "planDigest":plan.plan_digest,"resources":resources,
        "ownershipIntents":plan.request.ownership_intents.iter().map(|intent| json!({
            "objectName":intent.object_name,"objectUid":intent.object_uid,"objectResourceVersion":intent.object_resource_version,
            "identity":intent.approval.identity,"action":intent.approval.action,"observedUid":intent.approval.observed_uid,
            "priorManagedFieldsDigest":intent.approval.prior_managed_fields_digest,"priorManagerConflicts":intent.approval.prior_manager_conflicts,
            "permittedRuntimeFields":intent.approval.permitted_runtime_fields,"deletionPolicy":intent.approval.deletion_policy,
        })).collect::<Vec<_>>(),"ownershipMode":"explicit-protected-approval-required-for-existing-objects",
        "reactions":{"before":prior.map(|p| &p.ir.reactions),"after":next.ir.reactions},
        "inputAuthority":{"before":prior.map(|p| &p.ir.inputs),"after":next.ir.inputs},
        "permissions":{
            "runtime":capability_changes(prior_capabilities, &next.capabilities),
            "cleanup":{"requested":plan.cleanup_capabilities,"ceiling":plan.request.cleanup_ceiling,"decision":"approved-by-transition-planner"},
            "deployment":{"identity":"platform-provisioned-short-lived-deployer","sourceMayChangeAuthority":false,
                "required":platform::deployment_permissions(plan)?,"effectiveAuthority":"not-evaluated; external-platform-rbac-and-admission-boundary"}
        },
        "policy":{"runtimeCeiling":plan.request.runtime_ceiling,"decision":"approved-by-transition-planner"},
        "enforcementGaps":next.enforcement_gaps,"indirectPrivileges":next.indirect_risks,
        "cleanup":plan.cleanup,"warnings":plan.warnings,
        "irreversibleOperations":plan.cleanup.iter().filter(|i| i.policy == compiler_core::DeletionPolicy::Delete).collect::<Vec<_>>(),
        "compatibility":{"before":prior.map(compatibility).transpose()?,
            "after":compatibility(next)?},
        "expectedInterruption":if prior.is_some() {"reconciliation pauses from old proxy freeze until new generation activation; workloads remain running subject to declared changes"} else {"initial reconciliation starts after activation"},
        "valuesOmitted":true
    }))
}

// The predecessor carries only metadata. Until historical registry compaction,
// detailed review can separately verify the old release retained by the platform.
fn prior_compiled<'a>(
    registry: &'a platform::NamespaceRegistry,
    plan: &TransitionPlan,
) -> crate::Result<Option<&'a compiler_core::CompiledPlan>> {
    let Some(predecessor) = plan.request.predecessor()? else {
        return Ok(None);
    };
    let transition = &predecessor.allocation.transition_id;
    let stored = registry.transition(transition)?;
    if stored.record.phase != Phase::Committed
        || stored.plan.request.transition_id != *transition
        || stored.record.transition_id != *transition
        || stored.plan.generation_reservation != predecessor.allocation
    {
        return Err("review predecessor differs from its protected transition".into());
    }
    deploy::verify_record(&stored.plan, &stored.record)?;
    let expected = deploy::predecessor_metadata(
        &stored.plan.request.next,
        &stored.plan.generation_reservation,
        &plan.request.registry.claims,
    )?;
    if &expected != predecessor {
        return Err("review predecessor metadata differs from its protected release".into());
    }
    Ok(Some(&stored.plan.request.next.compiled))
}

fn capability_changes(before: &[Capability], after: &[Capability]) -> Value {
    let old: BTreeSet<_> = before.iter().collect();
    let new: BTreeSet<_> = after.iter().collect();
    json!({"added":new.difference(&old).collect::<Vec<_>>(),
        "removed":old.difference(&new).collect::<Vec<_>>(),"retained":old.intersection(&new).collect::<Vec<_>>()})
}

fn changed_paths(before: &Value, after: &Value, path: &str, output: &mut Vec<Value>) {
    if before == after {
        return;
    }
    if let (Some(old), Some(new)) = (before.as_object(), after.as_object()) {
        let keys: BTreeSet<_> = old.keys().chain(new.keys()).collect();
        for key in keys {
            let child = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
            match (old.get(key), new.get(key)) {
                (Some(a), Some(b)) => changed_paths(a, b, &child, output),
                (None, Some(_)) => output.push(json!({"path":child,"change":"added"})),
                (Some(_), None) => output.push(json!({"path":child,"change":"removed"})),
                (None, None) => {}
            }
        }
    } else {
        // Arrays are one field: positional diffs would imply Kubernetes list ownership semantics.
        output.push(json!({"path":path,"change":"changed"}));
    }
}

fn compatibility(plan: &compiler_core::CompiledPlan) -> crate::Result<Value> {
    compiler_core::review_documents(plan)
        .remove("compatibility.json")
        .ok_or_else(|| "compiler omitted compatibility review".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn field_diffs_escape_pointers_and_never_emit_values() -> crate::Result<()> {
        let mut changes = Vec::new();
        changed_paths(
            &json!({"metadata":{"a/b~c":"old"},"gone":1}),
            &json!({"metadata":{"a/b~c":"private-value"},"added":2}),
            "",
            &mut changes,
        );
        assert_eq!(
            changes,
            vec![
                json!({"path":"/added","change":"added"}),
                json!({"path":"/gone","change":"removed"}),
                json!({"path":"/metadata/a~1b~0c","change":"changed"})
            ]
        );
        assert!(!serde_json::to_string(&changes)?.contains("private-value"));
        Ok(())
    }
}
