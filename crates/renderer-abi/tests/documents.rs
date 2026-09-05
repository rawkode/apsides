use renderer_abi::{
    AbiError,
    limits::{MAX_EVENT_METADATA_CANONICAL_BYTES, MAX_RESOURCES, validate_json_limits},
    parse_validate_action_input, parse_validate_action_output, parse_validate_render_output,
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
        ,"observationToken": "snapshot-1"
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

#[test]
fn automation_and_action_inputs_are_pinned_and_strict() {
    let mut output: Value = serde_json::from_str(&render_output(&[resource(0)], "apply")).unwrap();
    output["automations"] = json!([{
        "identity": "demo.input-changed",
        "version": "1",
        "sourceDependency": {
            "type": "object",
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "namespace": "default",
            "name": "input",
            "uid": "input-uid",
            "resourceVersion": "7"
            ,"observationToken": "snapshot-1"
        },
        "eventMetadata": {"source":{"namespace":"default","name":"input"},"note":"changed"},
        "bundleDigest": "a5e0d6e99a2c8512a63f602b31100493013ab284f4dd0bb21d7894e13aa7b9c3",
        "actionModuleIdentity": "demo.action",
        "actionModuleDigest": "b5e0d6e99a2c8512a63f602b31100493013ab284f4dd0bb21d7894e13aa7b9c3",
        "rendererAbiVersion": "renderer.oxideloop.dev/v0"
    }]);
    parse_validate_render_output(&output.to_string()).unwrap();
    output["automations"][0]["rendererAbiVersion"] = json!("renderer.oxideloop.dev/v1");
    assert!(matches!(
        parse_validate_render_output(&output.to_string()),
        Err(AbiError::Contract(_))
    ));
    let mut action_input = json!({
        "abiVersion": "renderer.oxideloop.dev/v0",
        "bundleDigest": "a5e0d6e99a2c8512a63f602b31100493013ab284f4dd0bb21d7894e13aa7b9c3",
        "automationIdentity": "demo.input-changed",
        "automationVersion": "1",
        "actionModuleIdentity": "demo.action",
        "actionModuleDigest": "b5e0d6e99a2c8512a63f602b31100493013ab284f4dd0bb21d7894e13aa7b9c3",
        "occurrenceId": "occ-1",
        "sourceTransitionId": "transition-1",
        "sourceUid": "uid-1",
        "sourceResourceVersion": "3",
        "eventMetadata": {"reason": "changed"}
    });
    parse_validate_action_input(&action_input.to_string()).unwrap();
    let duplicate = output["automations"][0].clone();
    output["automations"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(matches!(
        parse_validate_render_output(&output.to_string()),
        Err(AbiError::Contract(_))
    ));
    action_input["bundleDigest"] = json!("sha256:bundle");
    assert!(matches!(
        parse_validate_action_input(&action_input.to_string()),
        Err(AbiError::Contract(_))
    ));
}

#[test]
fn event_metadata_is_required_and_has_a_canonical_byte_bound() {
    let mut output: Value = serde_json::from_str(&render_output(&[resource(0)], "apply")).unwrap();
    output["automations"] = json!([{
        "identity": "demo.input-changed",
        "version": "1",
        "sourceDependency": {
            "type": "object", "apiVersion": "v1", "kind": "ConfigMap",
            "namespace": "default", "name": "input", "uid": "uid-1",
            "resourceVersion": "7", "observationToken": "snapshot-1"
        },
        "eventMetadata": "x",
        "bundleDigest": "a5e0d6e99a2c8512a63f602b31100493013ab284f4dd0bb21d7894e13aa7b9c3",
        "actionModuleIdentity": "demo.action",
        "actionModuleDigest": "b5e0d6e99a2c8512a63f602b31100493013ab284f4dd0bb21d7894e13aa7b9c3",
        "rendererAbiVersion": "renderer.oxideloop.dev/v0"
    }]);
    parse_validate_render_output(&output.to_string()).unwrap();

    output["automations"][0]
        .as_object_mut()
        .unwrap()
        .remove("eventMetadata");
    assert!(matches!(
        parse_validate_render_output(&output.to_string()),
        Err(AbiError::InvalidDocument(_))
    ));

    let metadata_string_length = (MAX_EVENT_METADATA_CANONICAL_BYTES - 7) / 2;
    output["automations"][0]["eventMetadata"] = json!([
        "x".repeat(metadata_string_length),
        "x".repeat(metadata_string_length + 1)
    ]);
    parse_validate_render_output(&output.to_string()).unwrap();
    output["automations"][0]["eventMetadata"] = json!([
        "x".repeat(metadata_string_length),
        "x".repeat(metadata_string_length + 2)
    ]);
    assert_eq!(
        parse_validate_render_output(&output.to_string()).unwrap_err(),
        AbiError::CanonicalBytesLimit {
            actual: MAX_EVENT_METADATA_CANONICAL_BYTES + 1,
            maximum: MAX_EVENT_METADATA_CANONICAL_BYTES,
        }
    );

    let mut action_input = json!({
        "abiVersion": "renderer.oxideloop.dev/v0",
        "bundleDigest": "a".repeat(64),
        "automationIdentity": "demo.input-changed",
        "automationVersion": "1",
        "actionModuleIdentity": "demo.action",
        "actionModuleDigest": "b".repeat(64),
        "occurrenceId": "occ-1",
        "sourceTransitionId": "transition-1",
        "sourceUid": "uid-1",
        "sourceResourceVersion": "7",
        "eventMetadata": [
            "x".repeat(metadata_string_length),
            "x".repeat(metadata_string_length + 1)
        ]
    });
    parse_validate_action_input(&action_input.to_string()).unwrap();
    action_input["eventMetadata"] = json!([
        "x".repeat(metadata_string_length),
        "x".repeat(metadata_string_length + 2)
    ]);
    assert_eq!(
        parse_validate_action_input(&action_input.to_string()).unwrap_err(),
        AbiError::CanonicalBytesLimit {
            actual: MAX_EVENT_METADATA_CANONICAL_BYTES + 1,
            maximum: MAX_EVENT_METADATA_CANONICAL_BYTES,
        }
    );
}
