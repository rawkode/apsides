//! Ownership intents can never target a handler output: it is either absent
//! at first deploy (created absent-only by the reconciler) or already owned
//! by this domain.
use compiler_core::{DeletionPolicy, parse_and_validate};
use deploy::{
    Error, GenerationPlan, OWNERSHIP_INTENT_SCHEMA, OwnershipAction, OwnershipApproval,
    OwnershipIntent, ResourceIdentity, validate_ownership_intent,
};
use serde_json::json;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn plan_with_handler_output() -> TestResult<GenerationPlan> {
    let source = json!({"file": "program.tsx", "line": 1, "column": 1});
    let ir = json!({
        "schemaVersion": "apsides.dev/ir/v2",
        "domain": {
            "id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
            "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
            "minKubernetesVersion": "1.34",
            "budgets": {"maxResources": 8, "maxInputs": 8, "maxReactions": 8, "maxObjectBytes": 32768, "maxQueueDepth": 8, "maxConcurrentRequests": 2},
        },
        "resources": [{
            "id": "output-flags", "apiVersion": "v1", "kind": "ConfigMap",
            "name": "flags", "namespace": "workload",
            "desired": {
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "flags", "namespace": "workload", "labels": {"apsides.dev/handler-output": "output-flags"}},
            },
            "lifecycle": {"deletionPolicy": "orphan"},
            "source": source,
            "handlerOutput": true,
        }],
        "inputs": [{
            "id": "output-flags", "apiVersion": "v1", "kind": "ConfigMap",
            "name": "flags", "namespace": "workload", "mode": "output",
            "managedResourceId": "output-flags", "projection": ["data"], "sensitive": false,
            "missing": "block-dependent-mutations", "source": source,
        }],
        "reactions": [],
        "handlers": [],
        "outputs": [{"id": "output-flags", "name": "flags", "namespace": "workload"}],
        "provenance": {
            "compiler": "apsides/0.1",
            "inputs": [{"path": "program.tsx", "sha256": "0".repeat(64)}],
        },
    });
    let compiled = parse_and_validate(&ir.to_string())?;
    Ok(GenerationPlan {
        generation_id: "generation-a".into(),
        artifact_digest: format!("sha256:{}", "a".repeat(64)),
        compiled,
    })
}

fn intent_for_handler_output() -> OwnershipIntent {
    OwnershipIntent {
        object_name: "adopt-flags".into(),
        object_uid: "uid-1".into(),
        object_resource_version: "1".into(),
        approval: OwnershipApproval {
            schema_version: OWNERSHIP_INTENT_SCHEMA.into(),
            transition_id: "transition-a".into(),
            domain_id: "example".into(),
            generation_id: "generation-a".into(),
            release_digest: format!("sha256:{}", "a".repeat(64)),
            cluster_binding: "test-cluster".into(),
            control_namespace: "apsides-system".into(),
            identity: ResourceIdentity {
                api_group: String::new(),
                resource: "configmaps".into(),
                namespace: "workload".into(),
                name: "flags".into(),
            },
            observed_uid: "uid-1".into(),
            observed_resource_version: "1".into(),
            prior_managed_fields_digest: format!("sha256:{}", "b".repeat(64)),
            permitted_apply: json!({}),
            permitted_runtime_fields: vec![],
            deletion_policy: DeletionPolicy::Orphan,
            prior_manager_conflicts: vec![],
            action: OwnershipAction::Adopt,
        },
    }
}

#[test]
fn ownership_intent_targeting_a_handler_output_is_rejected() -> TestResult {
    let plan = plan_with_handler_output()?;
    let intent = intent_for_handler_output();
    let error = validate_ownership_intent(&intent, &plan, "transition-a")
        .err()
        .ok_or("ownership intent targeting a handler output was accepted")?;
    let Error::Plan(message) = error else {
        return Err("expected a plan error".into());
    };
    assert!(
        message.contains("handler output"),
        "expected a handler-output-specific rejection, received: {message}"
    );
    Ok(())
}
