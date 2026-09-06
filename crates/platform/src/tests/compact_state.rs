use super::*;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn reservation_wire_excludes_private_values_and_source_payloads() -> TestResult {
    let ReservationFixture {
        registry,
        policy,
        mut request,
    } = reservation_fixture()?;
    let private_value = "private-value-canary-8c25485b";
    let private_source = "private-source-canary-435a1e68.tsx";
    let mut ir = serde_json::to_value(&request.next.compiled.ir)?;
    ir["resources"][0]["desired"]["data"]["value"] = json!(private_value);
    ir["resources"][0]["source"]["file"] = json!(private_source);
    ir["provenance"]["inputs"][0]["path"] = json!(private_source);
    request.next.compiled = parse_and_validate(&serde_json::to_string(&ir)?)?;
    let before = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, calls) = wire(vec![
        before.clone(),
        object("policy", &policy, "policy-uid", "40"),
        before,
        json!("echo-put"),
    ]);
    let client = platform_client(client, "control", "workload", "cluster-a")?;
    let written = reserve_test_transition(&client, &request, "ci_job_1").await?;
    let stored = written.value.transition(TRANSITION_A)?;
    let state = serde_json::to_string(
        &written
            .value
            .state(&written.value.claim_snapshot()?.reference()?)?,
    )?;
    let claims = serde_json::to_string(&written.value.claim_snapshot()?)?;
    let desired = request
        .next
        .compiled
        .ir
        .resources
        .first()
        .ok_or("resource missing")?;
    for forbidden in [
        private_value.to_owned(),
        private_source.to_owned(),
        digest(&private_value)?,
        digest(&desired.desired)?,
    ] {
        assert!(
            !state.contains(&forbidden),
            "private payload or direct hash in registry"
        );
        assert!(
            !claims.contains(&forbidden),
            "private payload or direct hash in claim metadata"
        );
    }
    let encoded = serde_json::to_value(stored)?;
    for key in [
        "plan",
        "compiled",
        "desired",
        "source",
        "claims",
        "inventory",
        "permittedApply",
    ] {
        assert!(!contains_key(&encoded, key), "unexpected retained {key}");
    }
    assert_eq!(
        stored.inputs.plan_digest,
        deploy::plan_transition(&request)?.plan_digest
    );
    let calls = calls.lock().map_err(|_| "request lock poisoned")?;
    let writes: Vec<_> = calls
        .iter()
        .filter(|(method, _, _)| method == "PUT")
        .collect();
    assert_eq!(writes.len(), 1);
    let wire_state = writes.first().ok_or("registry write missing")?.2["data"]["state.json"]
        .as_str()
        .ok_or("wire state missing")?;
    assert_eq!(
        serde_json::from_str::<Value>(wire_state)?,
        serde_json::from_str::<Value>(&state)?
    );
    Ok(())
}

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(fields) => {
            fields.contains_key(key) || fields.values().any(|v| contains_key(v, key))
        }
        Value::Array(values) => values.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

#[tokio::test]
async fn selected_session_rejects_substituted_frozen_planning_authority() -> TestResult {
    let ReservationFixture {
        registry,
        policy,
        request,
    } = reservation_fixture()?;
    let before = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, _) = wire(vec![
        before.clone(),
        object("policy", &policy, "policy-uid", "40"),
        before,
        json!("echo-put"),
    ]);
    let client = platform_client(client, "control", "workload", "cluster-a")?;
    let written = reserve_test_transition(&client, &request, "ci_job_1").await?;
    let plan = deploy::plan_transition(&request)?;
    let bound = client.bind_test_plan(&plan, None)?;
    bound.selected(&written, TRANSITION_A)?;
    for change in 0..3 {
        let mut substituted = written.clone();
        let inputs = &mut substituted
            .value
            .transitions
            .get_mut(TRANSITION_A)
            .ok_or("transition missing")?
            .inputs;
        match change {
            0 => inputs.registry.claim_snapshot.digest = format!("sha256:{}", "f".repeat(64)),
            1 => {
                inputs.registry.revision = "43".into();
                inputs.registry.generation_history.revision = "43".into();
            }
            _ => inputs.policy_digest = format!("sha256:{}", "f".repeat(64)),
        }
        assert!(bound.selected(&substituted, TRANSITION_A).is_err());
    }
    Ok(())
}
