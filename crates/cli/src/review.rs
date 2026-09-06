//! Deployment review of a release-bound plan and authenticated predecessor metadata.
use compiler_core::{Capability, Resource};
use deploy::{PredecessorGeneration, ResourceIdentity};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub fn transition_review(resolved: &platform::ResolvedTransition) -> crate::Result<Value> {
    render_review(resolved.stored(), resolved.plan(), resolved.transition_id())
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
        || deploy::plan_transition(&plan.request)? != *plan
    {
        return Err("review requires the exact resolved transition plan".into());
    }
    deploy::verify_record(plan, &current.record)?;
    let next = &plan.request.next.compiled;
    let prior = prior_review(current, plan)?;
    let resources = match prior {
        PriorReview::Initial => detailed_resources(&[], next),
        PriorReview::Metadata(metadata) => metadata_resources(&metadata.inventory, next)?,
    };
    let unavailable = || unavailable_detail("historical-compiled-plan-not-retained");
    let (detail_availability, limitations) = match prior {
        PriorReview::Initial => ("initial", Vec::new()),
        PriorReview::Metadata(_) => (
            "metadata-only",
            vec![
                "old-field-structure".to_owned(),
                "old-source-locations".to_owned(),
                "old-reactions".to_owned(),
                "old-input-authority".to_owned(),
                "old-runtime-permissions".to_owned(),
                "old-compatibility".to_owned(),
            ],
        ),
    };
    let reactions_before = match prior {
        PriorReview::Initial => Value::Null,
        PriorReview::Metadata(_) => unavailable(),
    };
    let inputs_before = match prior {
        PriorReview::Initial => Value::Null,
        PriorReview::Metadata(_) => unavailable(),
    };
    let runtime_permissions = match prior {
        PriorReview::Initial => capability_changes(&[], &next.capabilities),
        PriorReview::Metadata(_) => unavailable_runtime_permissions(&next.capabilities),
    };
    let compatibility_before = match prior {
        PriorReview::Initial => Value::Null,
        PriorReview::Metadata(_) => unavailable(),
    };
    let has_predecessor = !matches!(prior, PriorReview::Initial);
    Ok(json!({
        "schemaVersion":"reakt.dev/deployment-review/v2",
        "detailAvailability":detail_availability,
        "detailLimitations":limitations,
        "planDigest":plan.plan_digest,"resources":resources,
        "ownershipIntents":plan.request.ownership_intents.iter().map(|intent| json!({
            "objectName":intent.object_name,"objectUid":intent.object_uid,"objectResourceVersion":intent.object_resource_version,
            "identity":intent.approval.identity,"action":intent.approval.action,"observedUid":intent.approval.observed_uid,
            "priorManagedFieldsDigest":intent.approval.prior_managed_fields_digest,"priorManagerConflicts":intent.approval.prior_manager_conflicts,
            "permittedRuntimeFields":intent.approval.permitted_runtime_fields,"deletionPolicy":intent.approval.deletion_policy,
        })).collect::<Vec<_>>(),"ownershipMode":"explicit-protected-approval-required-for-existing-objects",
        "reactions":{"before":reactions_before,"after":next.ir.reactions},
        "inputAuthority":{"before":inputs_before,"after":next.ir.inputs},
        "permissions":{
            "runtime":runtime_permissions,
            "cleanup":{"requested":plan.cleanup_capabilities,"ceiling":plan.request.cleanup_ceiling,"decision":"approved-by-transition-planner"},
            "deployment":{"identity":"platform-provisioned-short-lived-deployer","sourceMayChangeAuthority":false,
                "required":platform::deployment_permissions(plan)?,"effectiveAuthority":"not-evaluated; external-platform-rbac-and-admission-boundary"}
        },
        "policy":{"runtimeCeiling":plan.request.runtime_ceiling,"decision":"approved-by-transition-planner"},
        "enforcementGaps":next.enforcement_gaps,"indirectPrivileges":next.indirect_risks,
        "cleanup":plan.cleanup,"warnings":plan.warnings,
        "irreversibleOperations":plan.cleanup.iter().filter(|i| i.policy == compiler_core::DeletionPolicy::Delete).collect::<Vec<_>>(),
        "compatibility":{"before":compatibility_before,"after":compatibility(next)?},
        "expectedInterruption":if has_predecessor {"reconciliation pauses from old proxy freeze until new generation activation; workloads remain running subject to declared changes"} else {"initial reconciliation starts after activation"},
        "valuesOmitted":true
    }))
}

fn detailed_resources(before: &[Resource], next: &compiler_core::CompiledPlan) -> Vec<Value> {
    let resource_key = |r: &Resource| (r.namespace.clone(), r.kind.clone(), r.name.clone());
    let old: BTreeMap<_, _> = before.iter().map(|r| (resource_key(r), r)).collect();
    let new: BTreeMap<_, _> = next
        .ir
        .resources
        .iter()
        .map(|r| (resource_key(r), r))
        .collect();
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    keys.into_iter().map(|key| {
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
    }).collect()
}

fn metadata_resources(
    before: &inventory::InventorySnapshot,
    next: &compiler_core::CompiledPlan,
) -> crate::Result<Vec<Value>> {
    let old: BTreeMap<_, _> = before
        .resources
        .iter()
        .map(|resource| {
            (
                ResourceIdentity {
                    api_group: resource.identity.api_group.clone(),
                    resource: resource.identity.resource.clone(),
                    namespace: resource.identity.namespace.clone(),
                    name: resource.identity.name.clone(),
                },
                resource,
            )
        })
        .collect();
    let new: BTreeMap<_, _> = next
        .ir
        .resources
        .iter()
        .map(|resource| Ok((resource_identity(resource)?, resource)))
        .collect::<crate::Result<_>>()?;
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    let unavailable = || unavailable_detail("historical-compiled-plan-not-retained");

    keys
        .into_iter()
        .map(|key| {
            let left = old.get(&key).copied();
            let right = new.get(&key).copied();
            let (api_version, kind) = match (left, right) {
                (_, Some(resource)) => (&resource.api_version, &resource.kind),
                (Some(resource), None) => (&resource.api_version, &resource.kind),
                (None, None) => return Err("review resource identity disappeared".into()),
            };
            let change = match (left, right) {
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                (Some(_), Some(_)) => "unknown",
                (None, None) => return Err("review resource identity disappeared".into()),
            };
            let field_changes = match (left, right) {
                (None, Some(resource)) => {
                    let mut fields = Vec::new();
                    changed_paths(&json!({}), &resource.desired, "", &mut fields);
                    json!(fields)
                }
                _ => unavailable(),
            };
            let before_source = if left.is_some() {
                unavailable()
            } else {
                Value::Null
            };
            let before_force = if left.is_some() {
                unavailable()
            } else {
                Value::Null
            };
            Ok(json!({
                "apiVersion":api_version,
                "namespace":key.namespace,
                "kind":kind,
                "name":key.name,
                "beforeApiVersion":left.map(|resource| &resource.api_version),
                "afterApiVersion":right.map(|resource| &resource.api_version),
                "beforeKind":left.map(|resource| &resource.kind),
                "afterKind":right.map(|resource| &resource.kind),
                "change":change,
                "fieldChanges":field_changes,
                "beforeSource":before_source,
                "afterSource":right.map(|resource| &resource.source),
                "beforeLifecycle":left.map(|resource| json!({"deletionPolicy":resource.deletion_policy})),
                "afterLifecycle":right.map(|resource| &resource.lifecycle),
                "beforeForce":before_force,
                "afterForce":right.map(|resource| resource.force),
            }))
        })
        .collect()
}

fn resource_identity(resource: &Resource) -> crate::Result<ResourceIdentity> {
    let (api_group, plural) = match (resource.api_version.as_str(), resource.kind.as_str()) {
        ("v1", "ConfigMap") => ("", "configmaps"),
        ("v1", "Service") => ("", "services"),
        ("apps/v1", "Deployment") => ("apps", "deployments"),
        _ => return Err("review encountered an unsupported lifecycle resource".into()),
    };
    Ok(ResourceIdentity {
        api_group: api_group.to_owned(),
        resource: plural.to_owned(),
        namespace: resource.namespace.clone(),
        name: resource.name.clone(),
    })
}

fn unavailable_detail(reason: &str) -> Value {
    json!({"status":"unavailable","reason":reason})
}

fn unavailable_runtime_permissions(after: &[Capability]) -> Value {
    json!({
        "status":"unavailable",
        "reason":"historical-compiled-plan-not-retained",
        "after":after,
    })
}

#[derive(Clone, Copy)]
enum PriorReview<'a> {
    Initial,
    Metadata(&'a PredecessorGeneration),
}

fn prior_review<'a>(
    current: &platform::StoredTransition,
    plan: &'a deploy::TransitionPlan,
) -> crate::Result<PriorReview<'a>> {
    let Some(predecessor) = plan.request.predecessor()? else {
        if current.previous_writer_checkpoint.is_some() {
            return Err("initial review cannot contain predecessor authority".into());
        }
        return Ok(PriorReview::Initial);
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
    Ok(PriorReview::Metadata(predecessor))
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

    fn resource(name: &str, force: bool) -> Resource {
        Resource {
            force,
            id: name.to_owned(),
            api_version: "v1".to_owned(),
            kind: "ConfigMap".to_owned(),
            name: name.to_owned(),
            namespace: "workload".to_owned(),
            desired: json!({"data":{"private":"must-not-appear"}}),
            lifecycle: compiler_core::Lifecycle {
                deletion_policy: compiler_core::DeletionPolicy::Orphan,
            },
            source: compiler_core::SourceLocation {
                file: "next.tsx".to_owned(),
                line: 2,
                column: 3,
            },
        }
    }

    fn compiled(resources: Vec<Resource>) -> compiler_core::CompiledPlan {
        compiler_core::CompiledPlan {
            ir_digest: "digest".to_owned(),
            ir: compiler_core::Ir {
                test_only: false,
                schema_version: compiler_core::IR_SCHEMA.to_owned(),
                domain: compiler_core::Domain {
                    id: "controller".to_owned(),
                    namespace: "workload".to_owned(),
                    control_namespace: "control".to_owned(),
                    cluster_binding: "cluster".to_owned(),
                    policy_ref: "policy".to_owned(),
                    enforcement: "enforced".to_owned(),
                    budgets: compiler_core::Budgets {
                        max_resources: 3,
                        max_inputs: 0,
                        max_reactions: 0,
                        max_object_bytes: 1_000,
                        max_queue_depth: 1,
                        max_concurrent_requests: 1,
                    },
                },
                resources,
                inputs: Vec::new(),
                reactions: Vec::new(),
                provenance: compiler_core::Provenance {
                    compiler: "test".to_owned(),
                    inputs: Vec::new(),
                },
            },
            effects: Vec::new(),
            request_plan: Vec::new(),
            capabilities: Vec::new(),
            enforcement_gaps: Vec::new(),
            indirect_risks: Vec::new(),
            test_plan: Vec::new(),
        }
    }

    fn tracked(name: &str, policy: compiler_core::DeletionPolicy) -> inventory::TrackedResource {
        inventory::TrackedResource {
            identity: inventory::ResourceKey {
                api_group: String::new(),
                resource: "configmaps".to_owned(),
                namespace: "workload".to_owned(),
                name: name.to_owned(),
            },
            api_version: "v1".to_owned(),
            kind: "ConfigMap".to_owned(),
            uid: Some(format!("uid-{name}")),
            deletion_policy: policy,
        }
    }

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

    #[test]
    fn metadata_review_preserves_membership_without_inventing_old_structure() -> crate::Result<()> {
        let before = inventory::InventorySnapshot {
            schema_version: inventory::INVENTORY_SCHEMA.to_owned(),
            generation: "generation".to_owned(),
            resources: vec![
                tracked("retained", compiler_core::DeletionPolicy::Delete),
                tracked("removed", compiler_core::DeletionPolicy::Orphan),
            ],
        };
        let after = compiled(vec![resource("added", false), resource("retained", true)]);

        let review = metadata_resources(&before, &after)?;
        assert_eq!(
            review,
            vec![
                json!({
                    "apiVersion":"v1","namespace":"workload","kind":"ConfigMap","name":"added",
                    "beforeApiVersion":null,"afterApiVersion":"v1","beforeKind":null,"afterKind":"ConfigMap",
                    "change":"added","fieldChanges":[{"path":"/data","change":"added"}],
                    "beforeSource":null,"afterSource":{"file":"next.tsx","line":2,"column":3},
                    "beforeLifecycle":null,"afterLifecycle":{"deletionPolicy":"orphan"},
                    "beforeForce":null,"afterForce":false,
                }),
                json!({
                    "apiVersion":"v1","namespace":"workload","kind":"ConfigMap","name":"removed",
                    "beforeApiVersion":"v1","afterApiVersion":null,"beforeKind":"ConfigMap","afterKind":null,
                    "change":"removed","fieldChanges":{"status":"unavailable","reason":"historical-compiled-plan-not-retained"},
                    "beforeSource":{"status":"unavailable","reason":"historical-compiled-plan-not-retained"},"afterSource":null,
                    "beforeLifecycle":{"deletionPolicy":"orphan"},"afterLifecycle":null,
                    "beforeForce":{"status":"unavailable","reason":"historical-compiled-plan-not-retained"},"afterForce":null,
                }),
                json!({
                    "apiVersion":"v1","namespace":"workload","kind":"ConfigMap","name":"retained",
                    "beforeApiVersion":"v1","afterApiVersion":"v1","beforeKind":"ConfigMap","afterKind":"ConfigMap",
                    "change":"unknown","fieldChanges":{"status":"unavailable","reason":"historical-compiled-plan-not-retained"},
                    "beforeSource":{"status":"unavailable","reason":"historical-compiled-plan-not-retained"},"afterSource":{"file":"next.tsx","line":2,"column":3},
                    "beforeLifecycle":{"deletionPolicy":"delete"},"afterLifecycle":{"deletionPolicy":"orphan"},
                    "beforeForce":{"status":"unavailable","reason":"historical-compiled-plan-not-retained"},"afterForce":true,
                }),
            ]
        );
        assert!(!serde_json::to_string(&review)?.contains("must-not-appear"));
        Ok(())
    }

    #[test]
    fn unavailable_runtime_permissions_never_emit_false_empty_diffs() -> crate::Result<()> {
        let capability = Capability {
            namespace: "workload".to_owned(),
            api_group: String::new(),
            resource: "configmaps".to_owned(),
            subresource: None,
            verb: "get".to_owned(),
            names: vec!["owned".to_owned()],
        };
        let review = unavailable_runtime_permissions(&[capability]);
        assert_eq!(
            review,
            json!({
                "status":"unavailable",
                "reason":"historical-compiled-plan-not-retained",
                "after":[{
                    "namespace":"workload","apiGroup":"","resource":"configmaps",
                    "subresource":null,"verb":"get","names":["owned"]
                }]
            })
        );
        serde_json::to_string(&review)?;
        Ok(())
    }
}
