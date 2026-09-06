use super::*;
use serde_json::json;

fn resource() -> Resource {
    Resource {
        id: "owned".into(),
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
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
    }
}
fn object() -> Result<DynamicObject> {
    Ok(serde_json::from_value(
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{
        "name":"owned","namespace":"workload","uid":"uid","resourceVersion":"2","managedFields":[{
            "manager":"reakt-example","operation":"Apply","apiVersion":"v1","fieldsType":"FieldsV1",
            "fieldsV1":{"f:data":{"f:private":{}}}}]},"data":{"private":"canary"}}),
    )?)
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
