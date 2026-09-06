//! Declaration membership and field contracts, independent of live apply impact.
use super::declarations::{Detail, Unavailable};
use compiler_core::{CompiledPlan, Lifecycle, Resource, SourceLocation};
use deploy::{OwnershipAction, OwnershipIntent, ResourceIdentity};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
pub(super) struct FieldChange<'a> {
    path: &'a str,
    change: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OwnershipChange<'a> {
    change: &'static str,
    status: &'static str,
    execution: &'static str,
    intent_object_name: Option<&'a str>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResourceChange<'a> {
    api_version: &'a str,
    namespace: String,
    kind: &'a str,
    name: String,
    before_api_version: Option<&'a str>,
    after_api_version: Option<&'a str>,
    before_kind: Option<&'a str>,
    after_kind: Option<&'a str>,
    after_id: Option<&'a str>,
    change: &'static str,
    declaration_change: &'static str,
    field_changes: Detail<Vec<FieldChange<'a>>>,
    before_source: Detail<Option<&'a SourceLocation>>,
    after_source: Option<&'a SourceLocation>,
    before_lifecycle: Option<Lifecycle>,
    after_lifecycle: Option<&'a Lifecycle>,
    before_force: Detail<Option<bool>>,
    after_force: Option<bool>,
    ownership_change: OwnershipChange<'a>,
}

pub(super) fn resources<'a>(
    before: Option<&'a inventory::InventorySnapshot>,
    next: &'a CompiledPlan,
    intents: &'a [OwnershipIntent],
) -> crate::Result<Vec<ResourceChange<'a>>> {
    let old: BTreeMap<_, _> = before
        .into_iter()
        .flat_map(|i| &i.resources)
        .map(|r| {
            (
                ResourceIdentity {
                    api_group: r.identity.api_group.clone(),
                    resource: r.identity.resource.clone(),
                    namespace: r.identity.namespace.clone(),
                    name: r.identity.name.clone(),
                },
                r,
            )
        })
        .collect();
    let new: BTreeMap<_, _> = next
        .ir
        .resources
        .iter()
        .map(|r| Ok((identity(r)?, r)))
        .collect::<crate::Result<_>>()?;
    let approvals = approval_map(intents, &new)?;
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    keys.into_iter()
        .map(|key| {
            let left = old.get(&key).copied();
            let right = new.get(&key).copied();
            let (api_version, kind) = match (left, right) {
                (_, Some(r)) => (r.api_version.as_str(), r.kind.as_str()),
                (Some(r), None) => (r.api_version.as_str(), r.kind.as_str()),
                _ => return Err("review resource identity disappeared".into()),
            };
            let change = match (left, right) {
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                _ => "unknown",
            };
            let field_changes = declared_fields(left.is_some(), right, next)?;
            let intent = approvals.get(&key).copied();
            let (ownership, status) = match (left, right, intent) {
                (_, _, Some(_)) => ("adopted", "adoption-approved"),
                (None, Some(_), None) => ("new", "create-if-absent"),
                (Some(_), Some(_), None) => ("retained", "previously-managed"),
                _ => ("released", "release-planned"),
            };
            Ok(ResourceChange {
                api_version,
                namespace: key.namespace,
                kind,
                name: key.name,
                before_api_version: left.map(|r| r.api_version.as_str()),
                after_api_version: right.map(|r| r.api_version.as_str()),
                before_kind: left.map(|r| r.kind.as_str()),
                after_kind: right.map(|r| r.kind.as_str()),
                after_id: right.map(|r| r.id.as_str()),
                change,
                declaration_change: change,
                field_changes,
                before_source: if left.is_some() {
                    Detail::Unavailable(Unavailable::historical())
                } else {
                    Detail::Available(None)
                },
                after_source: right.map(|r| &r.source),
                before_lifecycle: left.map(|r| Lifecycle {
                    deletion_policy: r.deletion_policy,
                }),
                after_lifecycle: right.map(|r| &r.lifecycle),
                before_force: if left.is_some() {
                    Detail::Unavailable(Unavailable::historical())
                } else {
                    Detail::Available(None)
                },
                after_force: right.map(|r| r.force),
                ownership_change: OwnershipChange {
                    change: ownership,
                    status,
                    execution: "not-evaluated",
                    intent_object_name: intent.map(|i| i.object_name.as_str()),
                },
            })
        })
        .collect()
}

fn declared_fields<'a>(
    previously_managed: bool,
    resource: Option<&Resource>,
    next: &'a CompiledPlan,
) -> crate::Result<Detail<Vec<FieldChange<'a>>>> {
    if previously_managed {
        return Ok(Detail::Unavailable(Unavailable::historical()));
    }
    let target = resource.ok_or("review resource identity disappeared")?;
    let effect = next
        .effects
        .iter()
        .find(|effect| effect.target == target.id && effect.operation == "server-side-apply")
        .ok_or("review omitted canonical field contract")?;
    Ok(Detail::Available(
        effect
            .field_paths
            .iter()
            .map(|path| FieldChange {
                path,
                change: "added",
            })
            .collect(),
    ))
}

fn approval_map<'a>(
    intents: &'a [OwnershipIntent],
    resources: &BTreeMap<ResourceIdentity, &Resource>,
) -> crate::Result<BTreeMap<ResourceIdentity, &'a OwnershipIntent>> {
    let mut approvals = BTreeMap::new();
    for intent in intents {
        if !matches!(intent.approval.action, OwnershipAction::Adopt)
            || !resources.contains_key(&intent.approval.identity)
            || approvals
                .insert(intent.approval.identity.clone(), intent)
                .is_some()
        {
            return Err("review encountered an ambiguous ownership approval".into());
        }
    }
    Ok(approvals)
}

fn identity(resource: &Resource) -> crate::Result<ResourceIdentity> {
    let (api_group, plural) = match (resource.api_version.as_str(), resource.kind.as_str()) {
        ("v1", "ConfigMap") => ("", "configmaps"),
        ("v1", "Service") => ("", "services"),
        ("apps/v1", "Deployment") => ("apps", "deployments"),
        _ => return Err("review encountered an unsupported lifecycle resource".into()),
    };
    Ok(ResourceIdentity {
        api_group: api_group.into(),
        resource: plural.into(),
        namespace: resource.namespace.clone(),
        name: resource.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proposed() -> crate::Result<CompiledPlan> {
        let declaration = |name: &str, force| {
            json!({
                "id":format!("declaration-{name}"), "apiVersion":"v1", "kind":"ConfigMap",
                "name":name,"namespace":"workload", "force":force,
                "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"workload"},
                    "data":{"private":"must-not-appear"}},
                "lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"next.tsx","line":2,"column":3},
            })
        };
        let ir = json!({
            "schemaVersion":compiler_core::IR_SCHEMA,
            "domain":{"id":"example","namespace":"workload","controlNamespace":"control",
                "clusterBinding":"cluster","policyRef":"policy","enforcement":"api-bound",
                "budgets":{"maxResources":3,"maxInputs":1,"maxReactions":1,"maxObjectBytes":4096,
                    "maxQueueDepth":8,"maxConcurrentRequests":1}},
            "resources":[declaration("added",false),declaration("retained",true)],
            "inputs":[],"reactions":[],"provenance":{"compiler":"test","inputs":[{"path":"next.tsx","sha256":"a".repeat(64)}]},
        });
        Ok(compiler_core::parse_and_validate(&ir.to_string())?)
    }

    fn tracked(
        name: &str,
        deletion_policy: compiler_core::DeletionPolicy,
    ) -> inventory::TrackedResource {
        inventory::TrackedResource {
            identity: inventory::ResourceKey {
                api_group: String::new(),
                resource: "configmaps".into(),
                namespace: "workload".into(),
                name: name.into(),
            },
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            uid: Some(format!("uid-{name}")),
            deletion_policy,
        }
    }

    #[test]
    fn metadata_review_preserves_membership_without_inventing_old_structure() -> crate::Result<()> {
        let before = inventory::InventorySnapshot {
            schema_version: inventory::INVENTORY_SCHEMA.into(),
            generation: "previous".into(),
            resources: vec![
                tracked("retained", compiler_core::DeletionPolicy::Delete),
                tracked("removed", compiler_core::DeletionPolicy::Orphan),
            ],
        };
        let next = proposed()?;
        let rows = resources(Some(&before), &next, &[])?;
        let review = serde_json::to_value(&rows)?;
        let added = &review[0];
        let removed = &review[1];
        let retained = &review[2];
        assert_eq!(added["name"], "added");
        assert_eq!(added["afterId"], "declaration-added");
        assert_eq!(added["declarationChange"], "added");
        assert_eq!(
            added["fieldChanges"],
            json!([{"path":"/data/private","change":"added"}])
        );
        assert_eq!(removed["name"], "removed");
        assert_eq!(removed["declarationChange"], "removed");
        assert_eq!(removed["ownershipChange"]["change"], "released");
        assert_eq!(removed["ownershipChange"]["status"], "release-planned");
        assert_eq!(removed["ownershipChange"]["execution"], "not-evaluated");
        assert_eq!(removed["beforeLifecycle"]["deletionPolicy"], "orphan");
        assert_eq!(removed["afterLifecycle"], json!(null));
        assert_eq!(retained["name"], "retained");
        assert_eq!(retained["declarationChange"], "unknown");
        assert_eq!(retained["ownershipChange"]["change"], "retained");
        assert_eq!(retained["ownershipChange"]["status"], "previously-managed");
        assert_eq!(retained["beforeLifecycle"]["deletionPolicy"], "delete");
        assert_eq!(retained["afterLifecycle"]["deletionPolicy"], "orphan");
        assert_eq!(retained["afterForce"], true);
        assert_eq!(
            retained["afterSource"],
            json!({"file":"next.tsx","line":2,"column":3})
        );
        for row in [removed, retained] {
            for field in ["fieldChanges", "beforeForce", "beforeSource"] {
                assert_eq!(row[field]["status"], "unavailable");
            }
        }
        assert!(!serde_json::to_string(&rows)?.contains("must-not-appear"));
        Ok(())
    }
}
