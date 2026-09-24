use super::*;
use serde_json::json;

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
/// where a later phase attributes handler writes) separately owns `data` without
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
