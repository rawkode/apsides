use oxideloop_renderer_abi::{
    ACTION_CONTEXT_KEY_V1, AbiError, action_context_source_matches, action_context_v1,
    canonical_bytes, parse_validate_action_input,
};
use serde_json::json;

fn envelope() -> serde_json::Value {
    json!({
        "$oxideloop": {
            "kind": "action-context",
            "version": "v1",
            "source": {
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "namespace": "demo",
                "name": "demo-input",
                "uid": "source-uid",
                "resourceVersion": "42"
            },
            "metadata": {"$oxideloop": {"user": true}, "message": "changed"}
        }
    })
}

#[test]
fn tagged_context_binds_all_six_source_fields_and_nests_user_metadata() {
    let context = action_context_v1(&envelope()).unwrap().unwrap();
    assert!(action_context_source_matches(
        &context,
        "v1",
        "ConfigMap",
        "demo",
        "demo-input",
        "source-uid",
        "42"
    ));
    for (api_version, kind, namespace, name, uid, resource_version) in [
        ("v2", "ConfigMap", "demo", "demo-input", "source-uid", "42"),
        ("v1", "Secret", "demo", "demo-input", "source-uid", "42"),
        ("v1", "ConfigMap", "other", "demo-input", "source-uid", "42"),
        ("v1", "ConfigMap", "demo", "other", "source-uid", "42"),
        ("v1", "ConfigMap", "demo", "demo-input", "other", "42"),
        ("v1", "ConfigMap", "demo", "demo-input", "source-uid", "43"),
    ] {
        assert!(!action_context_source_matches(
            &context,
            api_version,
            kind,
            namespace,
            name,
            uid,
            resource_version
        ));
    }
    assert_eq!(
        context.metadata.pointer("/$oxideloop/user"),
        Some(&json!(true))
    );
}

#[test]
fn legacy_is_untagged_but_malformed_tagged_metadata_fails_closed() {
    assert!(
        action_context_v1(&json!({"message": "legacy"}))
            .unwrap()
            .is_none()
    );
    for malformed in [
        json!({"$oxideloop": {}}),
        json!({"$oxideloop": {"kind":"action-context", "version":"v2", "source":{}, "metadata":{}}}),
        json!({"$oxideloop": {"kind":"action-context", "version":"v1", "source":{"apiVersion":"v1","kind":"ConfigMap","namespace":"demo","name":"demo-input","uid":"source-uid","resourceVersion":"42","extra":true}, "metadata":{}}}),
        json!({"$oxideloop": {"kind":"action-context", "version":"v1", "source":{"apiVersion":"v1","kind":"ConfigMap","namespace":"demo","name":"demo-input","uid":"source-uid","resourceVersion":"42"}, "metadata":{}}, "extra": true}),
    ] {
        assert!(action_context_v1(&malformed).is_err());
    }
}

#[test]
fn action_input_retains_legacy_bytes_and_rejects_invalid_tagged_context() {
    let legacy = json!({
        "abiVersion":"renderer.oxideloop.dev/v0",
        "bundleDigest":"a".repeat(64),
        "automationIdentity":"changed",
        "automationVersion":"v1",
        "actionModuleIdentity":"action",
        "actionModuleDigest":"b".repeat(64),
        "occurrenceId":"occurrence",
        "sourceTransitionId":"transition",
        "sourceUid":"source-uid",
        "sourceResourceVersion":"42",
        "eventMetadata":{"message":"legacy"}
    });
    let legacy_bytes = canonical_bytes(&legacy).unwrap();
    let parsed = parse_validate_action_input(std::str::from_utf8(&legacy_bytes).unwrap()).unwrap();
    assert_eq!(parsed.canonical_bytes, legacy_bytes);

    let mut tagged = legacy;
    tagged["eventMetadata"] = json!({ACTION_CONTEXT_KEY_V1: {"kind":"action-context"}});
    let tagged_bytes = canonical_bytes(&tagged).unwrap();
    assert!(parse_validate_action_input(std::str::from_utf8(&tagged_bytes).unwrap()).is_err());
}

#[test]
fn tagged_context_has_stable_retry_bytes_and_its_envelope_counts_toward_the_cap() {
    let mut tagged = json!({
        "abiVersion":"renderer.oxideloop.dev/v0",
        "bundleDigest":"a".repeat(64),
        "automationIdentity":"changed",
        "automationVersion":"v1",
        "actionModuleIdentity":"action",
        "actionModuleDigest":"b".repeat(64),
        "occurrenceId":"occurrence",
        "sourceTransitionId":"transition",
        "sourceUid":"source-uid",
        "sourceResourceVersion":"42",
        "eventMetadata": envelope()
    });
    let first = canonical_bytes(&tagged).unwrap();
    let first_validated =
        parse_validate_action_input(std::str::from_utf8(&first).unwrap()).expect("valid tag");
    let retried =
        parse_validate_action_input(std::str::from_utf8(&first_validated.canonical_bytes).unwrap())
            .expect("retry parses exactly the same input");
    assert_eq!(first_validated.canonical_bytes, retried.canonical_bytes);
    assert_eq!(first_validated.sha256, retried.sha256);

    tagged["eventMetadata"]["$oxideloop"]["metadata"] = json!({
        "first": "x".repeat(256 * 1024),
        "second": "x".repeat(256 * 1024)
    });
    let oversized = canonical_bytes(&tagged).unwrap();
    assert!(matches!(
        parse_validate_action_input(std::str::from_utf8(&oversized).unwrap()),
        Err(AbiError::CanonicalBytesLimit { .. })
    ));
}
