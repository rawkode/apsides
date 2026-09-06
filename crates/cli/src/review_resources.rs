//! Kubernetes identity correlates declarations; declaration IDs remain visible across replacements.
use coordination::{
    ReviewDeletionPolicy, ReviewIdentity, ReviewLifecycle, ReviewMetadataV1, ReviewOwnershipPlan,
    ReviewResource, ReviewSource,
};
use deploy::{OwnershipAction, OwnershipIntent, ResourceIdentity};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
pub(super) struct FieldChange<'a> {
    path: &'a str,
    change: &'static str,
}
#[derive(Serialize)]
pub(super) struct FieldPaths<'a> {
    added: Vec<&'a str>,
    removed: Vec<&'a str>,
    retained: Vec<&'a str>,
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
    namespace: &'a str,
    kind: &'a str,
    name: &'a str,
    before: Option<&'a ReviewResource>,
    after: Option<&'a ReviewResource>,
    before_api_version: Option<&'a str>,
    after_api_version: Option<&'a str>,
    before_kind: Option<&'a str>,
    after_kind: Option<&'a str>,
    before_id: Option<&'a str>,
    after_id: Option<&'a str>,
    before_uid: Option<&'a str>,
    change: &'static str,
    declaration_change: &'static str,
    field_changes: Vec<FieldChange<'a>>,
    field_paths: FieldPaths<'a>,
    before_source: Option<&'a ReviewSource>,
    after_source: Option<&'a ReviewSource>,
    before_lifecycle: Option<&'a ReviewLifecycle>,
    after_lifecycle: Option<&'a ReviewLifecycle>,
    before_force: Option<bool>,
    after_force: Option<bool>,
    ownership_change: OwnershipChange<'a>,
}

pub(super) fn resources<'a>(
    before: Option<&'a ReviewMetadataV1>,
    next: &'a ReviewMetadataV1,
    predecessor_inventory: Option<&'a inventory::InventorySnapshot>,
    intents: &'a [OwnershipIntent],
) -> crate::Result<Vec<ResourceChange<'a>>> {
    next.validate()?;
    if let Some(previous) = before {
        previous.validate()?;
    }
    let tracked = inventory_map(before, predecessor_inventory)?;
    resource_rows(
        before.map_or(&[], |metadata| &metadata.resources),
        &next.resources,
        &tracked,
        intents,
    )
}

fn inventory_map<'a>(
    before: Option<&ReviewMetadataV1>,
    inventory: Option<&'a inventory::InventorySnapshot>,
) -> crate::Result<BTreeMap<ReviewIdentity, &'a inventory::TrackedResource>> {
    let (previous, inventory) = match (before, inventory) {
        (None, None) => return Ok(BTreeMap::new()),
        (Some(previous), Some(inventory)) => (previous, inventory),
        _ => {
            return Err(
                "review predecessor metadata and inventory are missing or unexpected".into(),
            );
        }
    };
    let scope = inventory::InventoryScope {
        cluster: previous.scope.claim_scope.cluster_binding.clone(),
        namespace: previous.scope.claim_scope.workload_namespace.clone(),
        controller: previous.scope.domain_id.clone(),
    };
    inventory.encode(&scope)?;
    if inventory.generation
        != deploy::allocation_names(
            &previous.scope.claim_scope.registry_uid,
            previous.scope.sequence,
        )?
        .generation_id
    {
        return Err("review inventory generation differs from its declaration baseline".into());
    }
    let tracked: BTreeMap<_, _> = inventory
        .resources
        .iter()
        .map(|resource| {
            (
                ReviewIdentity {
                    api_group: resource.identity.api_group.clone(),
                    resource: resource.identity.resource.clone(),
                    namespace: resource.identity.namespace.clone(),
                    name: resource.identity.name.clone(),
                },
                resource,
            )
        })
        .collect();
    if tracked.len() != inventory.resources.len() || tracked.len() != previous.resources.len() {
        return Err("review inventory membership differs from its declaration baseline".into());
    }
    for resource in &previous.resources {
        let tracked = tracked
            .get(&resource.identity)
            .ok_or("review prior identity is absent from inventory")?;
        let policy_matches = matches!(
            (resource.lifecycle.deletion_policy, tracked.deletion_policy),
            (
                ReviewDeletionPolicy::Delete,
                compiler_core::DeletionPolicy::Delete
            ) | (
                ReviewDeletionPolicy::Orphan,
                compiler_core::DeletionPolicy::Orphan
            )
        );
        if resource.api_version != tracked.api_version
            || resource.kind != tracked.kind
            || !policy_matches
        {
            return Err("review prior lifecycle differs from authenticated inventory".into());
        }
    }
    Ok(tracked)
}

fn resource_rows<'a>(
    before: &'a [ReviewResource],
    next: &'a [ReviewResource],
    tracked: &BTreeMap<ReviewIdentity, &'a inventory::TrackedResource>,
    intents: &'a [OwnershipIntent],
) -> crate::Result<Vec<ResourceChange<'a>>> {
    let old: BTreeMap<_, _> = before
        .iter()
        .map(|resource| (&resource.identity, resource))
        .collect();
    let new: BTreeMap<_, _> = next
        .iter()
        .map(|resource| (&resource.identity, resource))
        .collect();
    if old.len() != before.len() || new.len() != next.len() {
        return Err("review encountered duplicate resource identities".into());
    }
    let approvals = approval_map(intents, &old, &new)?;
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).copied().collect();
    keys.into_iter()
        .map(|key| {
            resource_row(
                old.get(key).copied(),
                new.get(key).copied(),
                tracked
                    .get(key)
                    .and_then(|resource| resource.uid.as_deref()),
                approvals.get(key).copied(),
            )
        })
        .collect()
}

fn resource_row<'a>(
    before: Option<&'a ReviewResource>,
    after: Option<&'a ReviewResource>,
    before_uid: Option<&'a str>,
    approval: Option<&'a OwnershipIntent>,
) -> crate::Result<ResourceChange<'a>> {
    let resource = after
        .or(before)
        .ok_or("review resource identity disappeared")?;
    let change = match (before, after) {
        (None, Some(_)) => "added",
        (Some(_), None) => "removed",
        (Some(left), Some(right)) if same_declaration(left, right) => "unchanged",
        _ => "changed",
    };
    let (field_paths, field_changes) = fields(before, after);
    Ok(ResourceChange {
        api_version: &resource.api_version,
        namespace: &resource.identity.namespace,
        kind: &resource.kind,
        name: &resource.identity.name,
        before,
        after,
        before_api_version: before.map(|r| r.api_version.as_str()),
        after_api_version: after.map(|r| r.api_version.as_str()),
        before_kind: before.map(|r| r.kind.as_str()),
        after_kind: after.map(|r| r.kind.as_str()),
        before_id: before.map(|r| r.id.as_str()),
        after_id: after.map(|r| r.id.as_str()),
        before_uid,
        change,
        declaration_change: change,
        field_changes,
        field_paths,
        before_source: before.map(|r| &r.source),
        after_source: after.map(|r| &r.source),
        before_lifecycle: before.map(|r| &r.lifecycle),
        after_lifecycle: after.map(|r| &r.lifecycle),
        before_force: before.map(|r| r.force),
        after_force: after.map(|r| r.force),
        ownership_change: ownership(before, after, approval)?,
    })
}

fn same_declaration(before: &ReviewResource, after: &ReviewResource) -> bool {
    before.id == after.id
        && before.identity == after.identity
        && before.api_version == after.api_version
        && before.kind == after.kind
        && before.apply_fields == after.apply_fields
        && before.lifecycle == after.lifecycle
        && before.force == after.force
        && before.source == after.source
}

fn fields<'a>(
    before: Option<&'a ReviewResource>,
    after: Option<&'a ReviewResource>,
) -> (FieldPaths<'a>, Vec<FieldChange<'a>>) {
    let old: BTreeSet<_> = before
        .into_iter()
        .flat_map(|r| r.apply_fields.iter().map(String::as_str))
        .collect();
    let new: BTreeSet<_> = after
        .into_iter()
        .flat_map(|r| r.apply_fields.iter().map(String::as_str))
        .collect();
    let paths = FieldPaths {
        added: new.difference(&old).copied().collect(),
        removed: old.difference(&new).copied().collect(),
        retained: old.intersection(&new).copied().collect(),
    };
    let changes = old
        .union(&new)
        .map(|path| FieldChange {
            path,
            change: if !old.contains(path) {
                "added"
            } else if !new.contains(path) {
                "removed"
            } else {
                "retained"
            },
        })
        .collect();
    (paths, changes)
}

fn approval_map<'a>(
    intents: &'a [OwnershipIntent],
    old: &BTreeMap<&ReviewIdentity, &ReviewResource>,
    new: &BTreeMap<&ReviewIdentity, &ReviewResource>,
) -> crate::Result<BTreeMap<ReviewIdentity, &'a OwnershipIntent>> {
    let mut approvals = BTreeMap::new();
    for intent in intents {
        let key = identity(&intent.approval.identity);
        if !matches!(intent.approval.action, OwnershipAction::Adopt)
            || !new.contains_key(&key)
            || old.contains_key(&key)
            || approvals.insert(key, intent).is_some()
        {
            return Err("review encountered an ambiguous ownership approval".into());
        }
    }
    Ok(approvals)
}

fn ownership<'a>(
    before: Option<&ReviewResource>,
    after: Option<&ReviewResource>,
    approval: Option<&'a OwnershipIntent>,
) -> crate::Result<OwnershipChange<'a>> {
    let (change, status) = match (before, after, approval) {
        (None, Some(resource), Some(intent))
            if approval_matches(&resource.ownership_plan, intent) =>
        {
            ("adopted", "adoption-approved")
        }
        (None, Some(resource), None)
            if resource.ownership_plan == ReviewOwnershipPlan::CreateIfAbsent =>
        {
            ("new", "create-if-absent")
        }
        (Some(_), Some(resource), None)
            if resource.ownership_plan == ReviewOwnershipPlan::PreviouslyManaged =>
        {
            ("retained", "previously-managed")
        }
        (Some(_), None, None) => ("released", "release-planned"),
        _ => {
            return Err(
                "review ownership plan differs from authenticated membership or approval".into(),
            );
        }
    };
    Ok(OwnershipChange {
        change,
        status,
        execution: "not-evaluated",
        intent_object_name: approval.map(|intent| intent.object_name.as_str()),
    })
}

fn approval_matches(plan: &ReviewOwnershipPlan, intent: &OwnershipIntent) -> bool {
    matches!(plan, ReviewOwnershipPlan::AdoptionApproved {
        intent_object_name, object_uid, object_resource_version, observed_uid, observed_resource_version,
    } if intent_object_name == &intent.object_name && object_uid == &intent.object_uid
        && object_resource_version == &intent.object_resource_version
        && observed_uid == &intent.approval.observed_uid
        && observed_resource_version == &intent.approval.observed_resource_version)
}

fn identity(identity: &ResourceIdentity) -> ReviewIdentity {
    ReviewIdentity {
        api_group: identity.api_group.clone(),
        resource: identity.resource.clone(),
        namespace: identity.namespace.clone(),
        name: identity.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(name: &str, id: &str) -> ReviewResource {
        ReviewResource {
            id: id.into(),
            identity: ReviewIdentity {
                api_group: String::new(),
                resource: "configmaps".into(),
                namespace: "workload".into(),
                name: name.into(),
            },
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            apply_fields: vec!["/data/shared".into()],
            lifecycle: ReviewLifecycle {
                deletion_policy: ReviewDeletionPolicy::Orphan,
            },
            force: false,
            source: ReviewSource {
                file: "controller.tsx".into(),
                line: 1,
                column: 1,
            },
            ownership_plan: ReviewOwnershipPlan::CreateIfAbsent,
        }
    }

    #[test]
    fn retained_ownership_does_not_invent_a_declaration_change() -> crate::Result<()> {
        let before = [declaration("config", "config")];
        let mut retained = before[0].clone();
        retained.ownership_plan = ReviewOwnershipPlan::PreviouslyManaged;
        let next = [retained.clone()];
        let rows = resource_rows(&before, &next, &BTreeMap::new(), &[])?;
        let row = rows.first().ok_or("resource row missing")?;
        assert_eq!(row.declaration_change, "unchanged");
        assert_eq!(row.ownership_change.change, "retained");
        assert_eq!(row.field_paths.retained, ["/data/shared"]);
        for change in 0..5 {
            let mut changed = retained.clone();
            match change {
                0 => changed.id = "renamed-declaration".into(),
                1 => changed.source.line = 2,
                2 => changed.force = true,
                3 => changed.lifecycle.deletion_policy = ReviewDeletionPolicy::Delete,
                _ => changed.apply_fields = vec!["/data/other".into()],
            }
            let next = [changed];
            let rows = resource_rows(&before, &next, &BTreeMap::new(), &[])?;
            let row = rows.first().ok_or("resource row missing")?;
            assert_eq!(row.declaration_change, "changed");
            assert_eq!(row.ownership_change.change, "retained");
            assert_eq!(row.before_id, Some("config"));
            if change == 0 {
                assert_eq!(row.after_id, Some("renamed-declaration"));
            }
        }
        Ok(())
    }

    #[test]
    fn moving_an_id_replaces_identity_without_transferring_ownership() -> crate::Result<()> {
        let before = [declaration("old", "stable-id")];
        let next = [declaration("new", "stable-id")];
        let rows = resource_rows(&before, &next, &BTreeMap::new(), &[])?;
        assert_eq!(rows.len(), 2);
        let removed = rows
            .iter()
            .find(|row| row.name == "old")
            .ok_or("old row missing")?;
        let added = rows
            .iter()
            .find(|row| row.name == "new")
            .ok_or("new row missing")?;
        assert_eq!(removed.declaration_change, "removed");
        assert_eq!(removed.before_id, added.after_id);
        assert_eq!(removed.ownership_change.status, "release-planned");
        assert_eq!(added.declaration_change, "added");
        assert_eq!(added.ownership_change.status, "create-if-absent");
        assert_eq!(removed.field_paths.removed, ["/data/shared"]);
        assert_eq!(added.field_paths.added, ["/data/shared"]);
        Ok(())
    }

    #[test]
    fn field_paths_and_prior_cleanup_metadata_remain_explicit() -> crate::Result<()> {
        let mut old = declaration("config", "config");
        old.apply_fields.insert(0, "/data/old".into());
        old.lifecycle.deletion_policy = ReviewDeletionPolicy::Delete;
        let mut new = declaration("config", "config");
        new.apply_fields.insert(0, "/data/new".into());
        new.ownership_plan = ReviewOwnershipPlan::PreviouslyManaged;
        let before = [old];
        let next = [new];
        let tracked = inventory::TrackedResource {
            identity: inventory::ResourceKey {
                api_group: String::new(),
                resource: "configmaps".into(),
                namespace: "workload".into(),
                name: "config".into(),
            },
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            uid: Some("prior-uid".into()),
            deletion_policy: compiler_core::DeletionPolicy::Delete,
        };
        let tracking = BTreeMap::from([(before[0].identity.clone(), &tracked)]);
        let rows = resource_rows(&before, &next, &tracking, &[])?;
        let row = rows.first().ok_or("resource row missing")?;
        assert_eq!(row.before_uid, Some("prior-uid"));
        assert_eq!(
            row.before_lifecycle.map(|p| p.deletion_policy),
            Some(ReviewDeletionPolicy::Delete)
        );
        assert_eq!(
            row.after_lifecycle.map(|p| p.deletion_policy),
            Some(ReviewDeletionPolicy::Orphan)
        );
        assert_eq!(row.field_paths.added, ["/data/new"]);
        assert_eq!(row.field_paths.removed, ["/data/old"]);
        assert_eq!(row.field_paths.retained, ["/data/shared"]);
        let removed = resource_rows(&before, &[], &tracking, &[])?;
        let row = removed.first().ok_or("removed row missing")?;
        assert_eq!(row.before_uid, Some("prior-uid"));
        assert_eq!(row.ownership_change.status, "release-planned");
        assert!(row.after.is_none());
        Ok(())
    }
}
