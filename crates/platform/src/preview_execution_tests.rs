use super::*;
use bytes::Bytes;
use compiler_core::{CompiledPlan, parse_and_validate};
use http::Response;
use http_body_util::Full;
use kube::{Client, client::Body};
use serde_json::json;
use tower::service_fn;

fn resource() -> Resource {
    Resource {
        id: "owned".into(),
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        api_resource: None,
        name: "owned".into(),
        namespace: "workload".into(),
        force: false,
        desired: json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"owned","namespace":"workload"},"data":{"private":"value"}}),
        lifecycle: compiler_core::Lifecycle {
            deletion_policy: compiler_core::DeletionPolicy::Orphan,
        },
        source: compiler_core::SourceLocation {
            file: "source.tsx".into(),
            line: 1,
            column: 1,
        },
        handler_output: false,
    }
}
fn object() -> Result<DynamicObject> {
    Ok(serde_json::from_value(
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{
        "name":"owned","namespace":"workload","uid":"uid","resourceVersion":"2","managedFields":[{
            "manager":"apsides-example","operation":"Apply","apiVersion":"v1","fieldsType":"FieldsV1",
            "fieldsV1":{"f:data":{"f:private":{}}}}]},"data":{"private":"canary"}}),
    )?)
}

fn handler_output_resource() -> Resource {
    Resource {
        id: "output-flags".into(),
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        api_resource: None,
        name: "flags".into(),
        namespace: "workload".into(),
        force: false,
        desired: json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"flags","namespace":"workload","labels":{"apsides.dev/handler-output":"output-flags"}}}),
        lifecycle: compiler_core::Lifecycle {
            deletion_policy: compiler_core::DeletionPolicy::Orphan,
        },
        source: compiler_core::SourceLocation {
            file: "source.tsx".into(),
            line: 1,
            column: 1,
        },
        handler_output: true,
    }
}

/// A handler output's desired value is metadata-only (see [`handler_output_resource`]):
/// the reconciler's own SSA of it, under field manager `apsides-<domain>`, never
/// claims `data`, so it still passes `require_apply_owner` on the strength of the
/// reserved label alone, while a distinct field manager (`apsides-handler-<domain>`,
/// under which the runtime commits handler writes) separately owns `data` without
/// conflict.
#[test]
fn handler_output_apply_evidence_never_claims_data_which_a_distinct_manager_owns() -> Result<()> {
    let object: DynamicObject = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {
            "name": "flags", "namespace": "workload", "uid": "uid", "resourceVersion": "2",
            "labels": {"apsides.dev/handler-output": "output-flags"},
            "managedFields": [
                {
                    "manager": "apsides-example", "operation": "Apply", "apiVersion": "v1",
                    "fieldsType": "FieldsV1", "fieldsV1": {"f:metadata": {"f:labels": {"f:apsides.dev/handler-output": {}}}},
                },
                {
                    "manager": "apsides-handler-example", "operation": "Apply", "apiVersion": "v1",
                    "fieldsType": "FieldsV1", "fieldsV1": {"f:data": {}},
                },
            ],
        },
        "data": {"value": "written-by-handler"},
    }))?;
    let resource = handler_output_resource();
    assert!(
        resource.desired.get("data").is_none(),
        "handler output desired value must never carry data"
    );
    require_apply_owner(&object, &resource, "example")?;
    Ok(())
}

/// A minimal compiled plan whose IR content is irrelevant to `require_target_authority`:
/// the function takes its `resource`/`identity` as direct arguments and only reaches into
/// `request.next.compiled` for capabilities on the absent-target path, which these tests
/// never exercise (the listed object always exists).
fn minimal_plan() -> Result<CompiledPlan> {
    parse_and_validate(&json!({"schemaVersion":"apsides.dev/ir/v2","testOnly":false,
        "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":64,"maxInputs":64,"maxReactions":64,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"config","namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload"},"data":{"value":"approved"}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}}],
        "inputs":[],"reactions":[],"handlers":[],"outputs":[],"provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    }).to_string())
    .map_err(|_| invalid("test IR failed to validate"))
}

fn handler_output_identity() -> ResourceIdentity {
    ResourceIdentity {
        api_group: String::new(),
        resource: "configmaps".into(),
        namespace: "workload".into(),
        name: "flags".into(),
    }
}

/// A `DeploymentRequest` carrying no ownership intent, no predecessor and no protected
/// claim for the target under test, i.e. it can supply no authority to adopt an
/// existing-but-unrecognized live object.
fn deployment_request_without_ownership(compiled: CompiledPlan) -> DeploymentRequest {
    DeploymentRequest {
        transition_id: "transition-a".into(),
        previous: None,
        next: deploy::GenerationPlan {
            generation_id: "generation-a".into(),
            artifact_digest: format!("sha256:{}", "a".repeat(64)),
            compiled,
        },
        runtime_ceiling: compiler_core::PermissionCeiling {
            policy_ref: "platform/example".into(),
            domain_id: "example".into(),
            cluster_binding: "cluster-a".into(),
            workload_namespace: "workload".into(),
            control_namespace: "control".into(),
            capabilities: vec![],
            accepted_enforcement_gaps: vec![],
            accepted_indirect_risks: vec![],
        },
        cleanup_ceiling: compiler_core::PermissionCeiling {
            policy_ref: "platform/example".into(),
            domain_id: "example".into(),
            cluster_binding: "cluster-a".into(),
            workload_namespace: "workload".into(),
            control_namespace: "control".into(),
            capabilities: vec![],
            accepted_enforcement_gaps: vec![],
            accepted_indirect_risks: vec![],
        },
        registry: deploy::RegistrySnapshot {
            registry_uid: "registry-a".into(),
            next_sequence: 1,
            revision: "1".into(),
            claims: vec![],
            generation_history: deploy::GenerationHistory {
                domain_id: "example".into(),
                cluster_binding: "cluster-a".into(),
                revision: "1".into(),
                active_allocation: None,
            },
        },
        observations: vec![],
        ownership_intents: vec![],
    }
}

/// List a single already-existing handler-output `ConfigMap` (uid `existing-uid`, never
/// touched by this domain's field manager) through a real `PreviewTransport` backed by a
/// mocked HTTP service, mirroring the shape `preview_transport.rs` itself requires.
async fn named_preview_of_an_unowned_handler_output() -> Result<NamedPreview> {
    let response = json!({"apiVersion":"v1","kind":"ConfigMapList",
        "metadata":{"resourceVersion":"9","continue":""},
        "items":[{"apiVersion":"v1","kind":"ConfigMap","metadata":{
            "name":"flags","namespace":"workload","uid":"existing-uid","resourceVersion":"9",
            "labels":{"apsides.dev/handler-output":"output-flags"}}}]});
    let bytes =
        serde_json::to_vec(&response).map_err(|_| invalid("test response encode failed"))?;
    let service = service_fn(move |_: http::Request<Body>| {
        let bytes = bytes.clone();
        async move {
            Response::builder()
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(bytes)))
                .map_err(std::io::Error::other)
        }
    });
    let transport = PreviewTransport::new(Client::new(service, "workload"));
    let target = PreviewTarget::new("v1", "ConfigMap", "", "configmaps", "workload", "flags")?;
    transport.list_named(&target).await
}

/// Deploying against a handler output that already exists on the cluster but was never
/// created (and is therefore never owned) by this domain must be rejected, and the error
/// must name the handler output clearly rather than the generic adoption-refusal message
/// used for ordinary managed resources — a handler output is created absent-only and can
/// never be adopted, since adoption is meaningless for a resource only this domain's own
/// handlers ever write to (see IR-CONTRACT.md's "Ownership intents" paragraph).
#[tokio::test]
async fn existing_unowned_handler_output_is_rejected_and_named_in_the_error() -> Result<()> {
    let compiled = minimal_plan()?;
    let request = deployment_request_without_ownership(compiled);
    let resource = handler_output_resource();
    let identity = handler_output_identity();
    let listed = named_preview_of_an_unowned_handler_output().await?;
    let error = require_target_authority(&request, &resource, &identity, &listed)
        .err()
        .ok_or_else(|| invalid("test expected refusal of an unowned handler output"))?;
    let message = error.to_string();
    assert!(
        message.contains("output-flags"),
        "error must name the handler output clearly: {message}"
    );
    assert!(
        !message.contains("cannot adopt an unapproved existing target"),
        "handler outputs must not use the generic adoption-refusal message: {message}"
    );
    Ok(())
}

/// The same live, unowned object rejected for a handler output is accepted-as-an-error but
/// with the *generic* message for an ordinary managed resource, confirming the two error
/// paths are distinct and the handler-output message is not simply always emitted.
#[tokio::test]
async fn existing_unowned_ordinary_resource_uses_the_generic_message() -> Result<()> {
    let compiled = minimal_plan()?;
    let request = deployment_request_without_ownership(compiled);
    let mut resource = handler_output_resource();
    resource.handler_output = false;
    let identity = handler_output_identity();
    let listed = named_preview_of_an_unowned_handler_output().await?;
    let error = require_target_authority(&request, &resource, &identity, &listed)
        .err()
        .ok_or_else(|| invalid("test expected refusal of an unowned existing target"))?;
    let message = error.to_string();
    assert!(message.contains("cannot adopt an unapproved existing target"));
    Ok(())
}

#[test]
fn exact_stable_apply_evidence_rejects_update_missing_duplicate_and_wrong_api() -> Result<()> {
    let valid = object()?;
    require_apply_owner(&valid, &resource(), "example")?;
    for mutation in [
        "update",
        "missing",
        "duplicate",
        "api",
        "foreign",
        "subresource",
    ] {
        let mut value = serde_json::to_value(&valid)?;
        match mutation {
            "update" => value["metadata"]["managedFields"][0]["operation"] = json!("Update"),
            "missing" => value["metadata"]["managedFields"] = json!([]),
            "duplicate" => {
                let entry = value["metadata"]["managedFields"][0].clone();
                value["metadata"]["managedFields"] = json!([entry.clone(), entry]);
            }
            "api" => value["metadata"]["managedFields"][0]["apiVersion"] = json!("v2"),
            "foreign" => {
                value["metadata"]["managedFields"][0]["manager"] = json!("private-foreign");
            }
            _ => value["metadata"]["managedFields"][0]["subresource"] = json!("status"),
        }
        let object = serde_json::from_value(value)?;
        let error = require_apply_owner(&object, &resource(), "example")
            .err()
            .ok_or_else(|| invalid("test expected refusal"))?;
        assert!(!error.to_string().contains("private"));
        assert!(!error.to_string().contains("canary"));
    }
    Ok(())
}
