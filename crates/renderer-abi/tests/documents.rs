use oxideloop_renderer_abi::{
    AbiError,
    limits::{MAX_RESOURCES, validate_json_limits},
    parse_validate_action_output, parse_validate_render_output,
};
use serde_json::{Value, json};

fn resource(index: usize) -> Value {
    json!({
        "logicalId": format!("resource-{index}"),
        "object": {
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": format!("resource-{index}") }
        },
        "readiness": null
    })
}

fn render_output(resources: &[Value], disposition: &str) -> String {
    json!({
        "abiVersion": "renderer.oxideloop.dev/v0",
        "disposition": disposition,
        "resources": resources,
        "dependencies": [],
        "automations": [],
        "diagnostics": []
    })
    .to_string()
}

#[test]
fn explicit_empty_is_required_for_an_empty_graph() {
    let error = parse_validate_render_output(&render_output(&[], "apply")).unwrap_err();
    assert_eq!(
        error,
        AbiError::Contract("an empty desired graph must use disposition `explicitEmpty`".into())
    );
    parse_validate_render_output(&render_output(&[], "explicitEmpty")).unwrap();
}

#[test]
fn resource_limit_is_inclusive() {
    let resources: Vec<_> = (0..MAX_RESOURCES).map(resource).collect();
    parse_validate_render_output(&render_output(&resources, "apply")).unwrap();

    let mut over = resources;
    over.push(resource(MAX_RESOURCES));
    assert_eq!(
        parse_validate_render_output(&render_output(&over, "apply")).unwrap_err(),
        AbiError::CardinalityLimit {
            field: "resources",
            actual: MAX_RESOURCES + 1,
            maximum: MAX_RESOURCES,
        }
    );
}

#[test]
fn unknown_fields_and_wrong_phase_documents_fail() {
    let unknown = r#"{
        "abiVersion":"renderer.oxideloop.dev/v0",
        "actions":[],
        "diagnostics":[],
        "renderOnly":true
    }"#;
    assert!(matches!(
        parse_validate_action_output(unknown),
        Err(AbiError::InvalidDocument(_))
    ));

    let render = render_output(&[], "explicitEmpty");
    assert!(matches!(
        parse_validate_action_output(&render),
        Err(AbiError::InvalidDocument(_))
    ));
}

#[test]
fn direct_limit_api_applies_before_typed_deserialization() {
    let value = Value::String("safe".into());
    validate_json_limits(&value).unwrap();
}

#[test]
fn object_dependencies_distinguish_present_and_absent_observations() {
    let mut absent: Value = serde_json::from_str(&render_output(&[resource(0)], "apply")).unwrap();
    absent["dependencies"] = json!([{
        "type": "object",
        "apiVersion": "v1",
        "kind": "Service",
        "namespace": "default",
        "name": "api-service",
        "uid": null,
        "resourceVersion": null
    }]);
    parse_validate_render_output(&absent.to_string()).unwrap();

    absent["dependencies"][0]["uid"] = json!("service-uid");
    assert_eq!(
        parse_validate_render_output(&absent.to_string()).unwrap_err(),
        AbiError::Contract(
            "an object dependency must declare both uid and resourceVersion when present, or neither when absent"
                .into()
        )
    );
}
