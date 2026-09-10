use compiler_core::{Capability, DeletionPolicy, PermissionCeiling};
use deploy::{ClaimState, ProtectedClaim, ResourceIdentity, plan_inventory_cleanup};
use inventory::{
    INVENTORY_SCHEMA, InventoryScope, InventorySnapshot, ResourceKey, TrackedResource,
};
use std::collections::BTreeSet;

#[test]
fn cleanup_uses_only_metadata_and_prior_policy()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let scope = InventoryScope {
        cluster: "cluster".into(),
        namespace: "workload".into(),
        controller: "example".into(),
    };
    let mut snapshot = InventorySnapshot {
        schema_version: INVENTORY_SCHEMA.into(),
        generation: "previous".into(),
        resources: vec![],
    };
    let mut claims = vec![];
    let mut next = BTreeSet::new();
    for (name, policy) in [
        ("delete", DeletionPolicy::Delete),
        ("keep", DeletionPolicy::Delete),
        ("orphan", DeletionPolicy::Orphan),
    ] {
        let identity = ResourceIdentity {
            api_group: String::new(),
            resource: "configmaps".into(),
            namespace: "workload".into(),
            name: name.into(),
        };
        let uid = Some(format!("uid-{name}"));
        snapshot.resources.push(TrackedResource {
            identity: ResourceKey {
                api_group: String::new(),
                resource: "configmaps".into(),
                namespace: "workload".into(),
                name: name.into(),
            },
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            uid: uid.clone(),
            deletion_policy: policy,
        });
        claims.push(ProtectedClaim {
            identity: identity.clone(),
            domain_id: "example".into(),
            generation_id: "previous".into(),
            transition_id: "old-transition".into(),
            state: ClaimState::Active,
            uid,
        });
        if name == "keep" {
            next.insert(identity);
        }
    }
    let ceiling = ceiling();
    let (cleanup, capabilities) =
        plan_inventory_cleanup(&scope, &snapshot, &next, &claims, &ceiling)?;
    assert_eq!(cleanup.len(), 2);
    assert!(cleanup.iter().any(|item| item.identity.name == "delete"
        && item.policy == DeletionPolicy::Delete
        && item.expected_uid.as_deref() == Some("uid-delete")));
    assert!(
        cleanup
            .iter()
            .any(|item| item.identity.name == "orphan" && item.policy == DeletionPolicy::Orphan)
    );
    assert_eq!(capabilities.len(), 2);
    assert!(
        capabilities
            .iter()
            .all(|capability| capability.names == vec!["delete"])
    );
    let mut missing = snapshot.clone();
    missing.resources.pop();
    assert!(plan_inventory_cleanup(&scope, &missing, &next, &claims, &ceiling).is_err());
    let mut replaced = snapshot.clone();
    replaced
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .uid = Some("replacement".into());
    assert!(plan_inventory_cleanup(&scope, &replaced, &next, &claims, &ceiling).is_err());
    let mut denied = ceiling.clone();
    denied
        .capabilities
        .retain(|capability| capability.verb != "delete");
    assert!(plan_inventory_cleanup(&scope, &snapshot, &next, &claims, &denied).is_err());
    let mut foreign = scope;
    foreign.controller = "another-controller".into();
    assert!(plan_inventory_cleanup(&foreign, &snapshot, &next, &claims, &ceiling).is_err());
    Ok(())
}

fn ceiling() -> PermissionCeiling {
    PermissionCeiling {
        policy_ref: "platform/example".into(),
        domain_id: "example".into(),
        cluster_binding: "cluster".into(),
        workload_namespace: "workload".into(),
        control_namespace: "control".into(),
        capabilities: ["get", "delete"]
            .into_iter()
            .map(|verb| Capability {
                namespace: "workload".into(),
                api_group: String::new(),
                resource: "configmaps".into(),
                subresource: None,
                verb: verb.into(),
                names: vec!["delete".into()],
            })
            .collect(),
        accepted_enforcement_gaps: vec![],
        accepted_indirect_risks: vec![],
    }
}
