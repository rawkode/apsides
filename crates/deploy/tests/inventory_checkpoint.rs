use compiler_core::{DeletionPolicy, parse_and_validate};
use deploy::{
    GenerationAllocation, GenerationPlan, generation_inventory, inventory_metadata_digest,
    validate_inventory_checkpoint,
};
use inventory::{
    INVENTORY_SCHEMA, InventoryScope, InventorySnapshot, ResourceKey, TrackedResource,
};
use serde_json::json;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn scope() -> InventoryScope {
    InventoryScope {
        cluster: "cluster-a".into(),
        namespace: "workload".into(),
        controller: "example".into(),
    }
}

fn resource(name: &str) -> TrackedResource {
    TrackedResource {
        identity: ResourceKey {
            api_group: String::new(),
            resource: "configmaps".into(),
            namespace: "workload".into(),
            name: name.into(),
        },
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        uid: None,
        deletion_policy: DeletionPolicy::Orphan,
    }
}

fn snapshot() -> InventorySnapshot {
    InventorySnapshot {
        schema_version: INVENTORY_SCHEMA.into(),
        generation: "gceirceircfardairceirceirce-1".into(),
        resources: vec![resource("alpha")],
    }
}

fn allocation(metadata_digest: String) -> GenerationAllocation {
    GenerationAllocation {
        registry_uid: REGISTRY_UID.into(),
        sequence: 1,
        generation_id: "gceirceircfardairceirceirce-1".into(),
        service_account_name: "rceirceircfardairceirceirce-1".into(),
        transition_id: "tceirceircfardairceirceirce-1".into(),
        artifact_digest: DIGEST.into(),
        ir_digest: DIGEST.into(),
        inventory_metadata_digest: metadata_digest,
    }
}

#[test]
fn metadata_only_checkpoint_validates_and_uid_completion_is_inert() -> TestResult {
    let scope = scope();
    let snapshot = snapshot();
    let digest = inventory_metadata_digest(&scope, &snapshot)?;
    let allocation = allocation(digest.clone());

    validate_inventory_checkpoint(&scope, &snapshot, &allocation)?;

    let mut completed = snapshot;
    completed
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .uid = Some("uid-after-apply".into());
    assert_eq!(inventory_metadata_digest(&scope, &completed)?, digest);
    validate_inventory_checkpoint(&scope, &completed, &allocation)?;
    Ok(())
}

#[test]
fn desired_and_private_values_do_not_enter_inventory_metadata() -> TestResult {
    let first = generation("private-value-one", "private-annotation-one")?;
    let second = generation("private-value-two", "private-annotation-two")?;
    let (first_scope, first_snapshot) = generation_inventory(&first, &[])?;
    let (second_scope, second_snapshot) = generation_inventory(&second, &[])?;

    assert_eq!(first_scope, second_scope);
    assert_eq!(first_snapshot, second_snapshot);
    assert_eq!(
        inventory_metadata_digest(&first_scope, &first_snapshot)?,
        inventory_metadata_digest(&second_scope, &second_snapshot)?
    );
    let encoded = serde_json::to_string(&first_snapshot)?;
    assert!(!encoded.contains("private-value"));
    assert!(!encoded.contains("private-annotation"));
    Ok(())
}

#[test]
fn every_approved_metadata_dimension_changes_the_checkpoint() -> TestResult {
    let scope = scope();
    let snapshot = snapshot();
    let baseline = inventory_metadata_digest(&scope, &snapshot)?;
    let allocation = allocation(baseline.clone());

    let mut changed_scope = scope.clone();
    changed_scope.cluster = "cluster-b".into();
    assert_changed(&changed_scope, &snapshot, &allocation, &baseline)?;

    changed_scope = scope.clone();
    changed_scope.controller = "another-controller".into();
    assert_changed(&changed_scope, &snapshot, &allocation, &baseline)?;

    changed_scope = scope.clone();
    changed_scope.namespace = "another-namespace".into();
    let mut moved = snapshot.clone();
    moved
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .identity
        .namespace = changed_scope.namespace.clone();
    assert_changed(&changed_scope, &moved, &allocation, &baseline)?;

    let mut changed = snapshot.clone();
    changed
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .identity
        .name = "bravo".into();
    assert_changed(&scope, &changed, &allocation, &baseline)?;

    changed = snapshot.clone();
    changed
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .api_version = "v2".into();
    assert_changed(&scope, &changed, &allocation, &baseline)?;

    changed = snapshot.clone();
    changed
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .kind = "Secret".into();
    assert_changed(&scope, &changed, &allocation, &baseline)?;

    changed = snapshot.clone();
    changed
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .deletion_policy = DeletionPolicy::Delete;
    assert_changed(&scope, &changed, &allocation, &baseline)?;

    changed = snapshot.clone();
    changed.resources.push(resource("bravo"));
    assert_changed(&scope, &changed, &allocation, &baseline)?;

    changed = snapshot;
    changed.generation = "generation-b".into();
    assert_changed(&scope, &changed, &allocation, &baseline)?;
    Ok(())
}

#[test]
fn invalid_inventory_cannot_be_committed() -> TestResult {
    let scope = scope();
    let mut duplicate = snapshot();
    duplicate.resources.push(resource("alpha"));
    assert!(inventory_metadata_digest(&scope, &duplicate).is_err());

    let mut mismatched_api_group = snapshot();
    mismatched_api_group
        .resources
        .first_mut()
        .ok_or("missing test resource")?
        .api_version = "apps/v1".into();
    assert!(inventory_metadata_digest(&scope, &mismatched_api_group).is_err());
    Ok(())
}

#[test]
fn allocation_requires_inventory_metadata_commitment() {
    let value = json!({
        "generationId": "gceirceircfardairceirceirce-1",
        "registryUid": REGISTRY_UID,
        "sequence": "1",
        "serviceAccountName": "rceirceircfardairceirceirce-1",
        "transitionId": "tceirceircfardairceirceirce-1",
        "artifactDigest": DIGEST,
        "irDigest": DIGEST
    });
    assert!(serde_json::from_value::<GenerationAllocation>(value).is_err());
}

fn assert_changed(
    scope: &InventoryScope,
    snapshot: &InventorySnapshot,
    allocation: &GenerationAllocation,
    baseline: &str,
) -> TestResult {
    assert_ne!(inventory_metadata_digest(scope, snapshot)?, baseline);
    assert!(validate_inventory_checkpoint(scope, snapshot, allocation).is_err());
    Ok(())
}

fn generation(
    private_value: &str,
    private_annotation: &str,
) -> Result<GenerationPlan, Box<dyn std::error::Error + Send + Sync>> {
    let value = json!({
        "schemaVersion": "apsides.dev/ir/v2",
        "domain": {
            "id": "example",
            "namespace": "workload",
            "controlNamespace": "control",
            "clusterBinding": "cluster-a",
            "policyRef": "platform/example",
            "enforcement": "api-bound",
            "minKubernetesVersion": "1.34",
            "budgets": {
                "maxResources": 8,
                "maxInputs": 8,
                "maxReactions": 8,
                "maxObjectBytes": 32768,
                "maxQueueDepth": 8,
                "maxConcurrentRequests": 2
            }
        },
        "resources": [{
            "id": "config",
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "name": "config",
            "namespace": "workload",
            "desired": {
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {
                    "name": "config",
                    "namespace": "workload",
                    "annotations": {"private": private_annotation}
                },
                "data": {"secret": private_value}
            },
            "lifecycle": {"deletionPolicy": "orphan"},
            "source": {"file": "program.ts", "line": 1, "column": 1}
        }],
        "inputs": [],
        "reactions": [],
        "handlers":[],"outputs":[],"provenance": {
            "compiler": "apsides/0.1",
            "inputs": [{
                "path": "program.ts",
                "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
            }]
        }
    });
    Ok(GenerationPlan {
        generation_id: "generation-a".into(),
        artifact_digest: DIGEST.into(),
        compiled: parse_and_validate(&value.to_string())?,
    })
}
