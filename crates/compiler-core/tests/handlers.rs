//! `validate()`'s handler and handler-output rules: one test per rule from
//! the Phase 4 plan, plus a happy-path acceptance test.
use compiler_core::*;
use serde_json::{Value, json};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn source() -> Value {
    json!({"file": "program.tsx", "line": 1, "column": 1})
}

/// A base IR with one Deployment, one non-sensitive `ConfigMap` input
/// (`settings`), one handler output (`flags`, backed by a `handlerOutput`
/// resource and an Output-mode input), and one handler triggering on
/// `settings`, reading it, and writing `flags`. Callers mutate this to
/// exercise one validation rule at a time.
fn fixture() -> Value {
    json!({
        "schemaVersion": IR_SCHEMA,
        "domain": {
            "id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
            "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
            "minKubernetesVersion": "1.34",
            "budgets": {"maxResources": 32, "maxInputs": 32, "maxReactions": 32, "maxObjectBytes": 262_144, "maxQueueDepth": 32, "maxConcurrentRequests": 4},
        },
        "resources": [
            {
                "id": "api", "apiVersion": "apps/v1", "kind": "Deployment", "name": "api", "namespace": "workload",
                "desired": {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api", "namespace": "workload"}, "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "api"}}, "template": {"metadata": {"labels": {"app": "api"}}, "spec": {"serviceAccountName": "api-workload", "containers": [{"name": "api", "image": format!("example.invalid/api@sha256:{}", "0".repeat(64))}]}}}},
                "lifecycle": {"deletionPolicy": "orphan"}, "source": source(),
            },
            {
                "id": "output-flags", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
                "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "flags", "namespace": "workload", "labels": {"apsides.dev/handler-output": "output-flags"}}},
                "lifecycle": {"deletionPolicy": "orphan"}, "source": source(), "handlerOutput": true,
            },
        ],
        "inputs": [
            {"id": "settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "namespace": "workload", "mode": "observed", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
            {"id": "output-flags-input", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload", "mode": "output", "managedResourceId": "output-flags", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
            {"id": "credentials", "apiVersion": "v1", "kind": "Secret", "name": "credentials", "namespace": "workload", "mode": "observed", "projection": ["data"], "sensitive": true, "missing": "block-dependent-mutations", "source": source()},
        ],
        "reactions": [],
        "handlers": [{
            "id": "sync-flags", "module": "handlers/sync-flags.ts", "export": "syncFlags",
            "trigger": {"kind": "input-changed", "input": "settings"},
            "inputs": ["settings"], "egress": [{"host": "api.example.com", "port": 443}],
            "outputs": ["output-flags"], "timeoutMillis": 30000, "maxHeapBytes": 67_108_864,
            "bundleSha256": "1".repeat(64), "source": source(),
        }],
        "outputs": [{"id": "output-flags", "name": "flags", "namespace": "workload"}],
        "provenance": {"compiler": "apsides/0.1", "inputs": [{"path": "program.tsx", "sha256": "0".repeat(64)}, {"path": "handlers/sync-flags.ts", "sha256": "1".repeat(64)}]},
    })
}

fn plan(value: &Value) -> Result<CompiledPlan> {
    parse_and_validate(&value.to_string())
}

fn expect_err(value: &Value, needle: &str) -> TestResult {
    let error = plan(value)
        .err()
        .ok_or_else(|| format!("expected rejection containing {needle:?}"))?;
    let message = error.to_string();
    assert!(
        message.contains(needle),
        "expected error containing {needle:?}, received: {message}"
    );
    Ok(())
}

#[test]
fn happy_path_handler_and_output_are_accepted() -> TestResult {
    let compiled = plan(&fixture())?;
    assert_eq!(compiled.ir.handlers.len(), 1);
    assert_eq!(compiled.ir.outputs.len(), 1);
    // Outputs add named get/list/watch plus create/patch capabilities.
    let verbs: Vec<_> = compiled
        .capabilities
        .iter()
        .filter(|capability| capability.resource == "configmaps" && capability.names == ["flags"])
        .map(|capability| capability.verb.as_str())
        .collect();
    for verb in ["get", "list", "watch", "patch"] {
        assert!(
            verbs.contains(&verb),
            "missing {verb} capability: {verbs:?}"
        );
    }
    Ok(())
}

#[test]
fn handler_input_must_be_non_sensitive_configmap() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["inputs"] = json!(["settings", "credentials"]);
    expect_err(&value, "non-sensitive ConfigMap input")
}

#[test]
fn handler_may_not_trigger_on_or_read_its_own_output() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["inputs"] = json!(["settings", "output-flags-input"]);
    expect_err(&value, "may not trigger on or read its own output")
}

#[test]
fn two_handler_cycle_is_rejected() -> TestResult {
    let mut value = fixture();
    value["resources"]
        .as_array_mut()
        .ok_or("not an array")?
        .push(json!({
            "id": "output-other", "apiVersion": "v1", "kind": "ConfigMap", "name": "other", "namespace": "workload",
            "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "other", "namespace": "workload", "labels": {"apsides.dev/handler-output": "output-other"}}},
            "lifecycle": {"deletionPolicy": "orphan"}, "source": source(), "handlerOutput": true,
        }));
    value["inputs"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "output-other-input", "apiVersion": "v1", "kind": "ConfigMap", "name": "other", "namespace": "workload",
        "mode": "output", "managedResourceId": "output-other", "projection": ["data"], "sensitive": false,
        "missing": "block-dependent-mutations", "source": source(),
    }));
    value["outputs"]
        .as_array_mut()
        .ok_or("not an array")?
        .push(json!({"id": "output-other", "name": "other", "namespace": "workload"}));
    // handler-b triggers on flags (handler-a's output) and writes "other".
    value["handlers"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "handler-b", "module": "handlers/sync-flags.ts", "export": "syncFlags",
        "trigger": {"kind": "input-changed", "input": "output-flags-input"},
        "inputs": ["output-flags-input"], "egress": [], "outputs": ["output-other"],
        "timeoutMillis": 30000, "maxHeapBytes": 67_108_864, "bundleSha256": "1".repeat(64), "source": source(),
    }));
    // handler-a (the original "sync-flags") now also reads handler-b's
    // output, closing the cycle: a -> flags -> b -> other -> a.
    value["handlers"][0]["inputs"] = json!(["settings", "output-other-input"]);
    expect_err(&value, "cycle")
}

#[test]
fn egress_host_rejects_ip_literal() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["egress"] = json!([{"host": "203.0.113.10", "port": 443}]);
    expect_err(&value, "IP literal")
}

#[test]
fn egress_host_rejects_wildcard() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["egress"] = json!([{"host": "*.example.com", "port": 443}]);
    expect_err(&value, "wildcard")
}

#[test]
fn egress_host_rejects_uppercase() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["egress"] = json!([{"host": "API.example.com", "port": 443}]);
    expect_err(&value, "lowercase DNS name")
}

#[test]
fn output_name_cannot_alias_another_resource() -> TestResult {
    let mut value = fixture();
    // "flags" (the output's name) collides with a second managed ConfigMap
    // declared under a different id.
    value["resources"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "collider", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
        "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "flags", "namespace": "workload"}, "data": {"k": "v"}},
        "lifecycle": {"deletionPolicy": "orphan"}, "source": source(),
    }));
    expect_err(&value, "overlapping managed object identities")
}

#[test]
fn handler_output_label_is_required_on_every_handler_output_resource() -> TestResult {
    let mut value = fixture();
    value["resources"][1]["desired"]["metadata"]["labels"] = json!({});
    expect_err(&value, "apsides.dev/handler-output is reserved")
}

#[test]
fn handler_output_label_is_rejected_on_any_other_resource() -> TestResult {
    let mut value = fixture();
    value["resources"][0]["desired"]["metadata"]["labels"] =
        json!({"apsides.dev/handler-output": "output-flags"});
    expect_err(&value, "apsides.dev/handler-output is reserved")
}

#[test]
fn handler_output_resource_must_carry_no_data() -> TestResult {
    let mut value = fixture();
    value["resources"][1]["desired"]["data"] = json!({"leaked": "value"});
    expect_err(&value, "metadata-only ConfigMap")
}

#[test]
fn timeout_millis_must_not_exceed_thirty_seconds() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["timeoutMillis"] = json!(30_001);
    expect_err(&value, "timeoutMillis")
}

#[test]
fn max_heap_bytes_must_not_exceed_sixty_four_mebibytes() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["maxHeapBytes"] = json!(67_108_865_u64);
    expect_err(&value, "maxHeapBytes")
}

#[test]
fn max_handlers_budget_is_sixteen() -> TestResult {
    let mut value = fixture();
    let handlers = value["handlers"].as_array().ok_or("not an array")?.clone();
    let base = handlers[0].clone();
    let mut many = Vec::new();
    for index in 0..17 {
        let mut handler = base.clone();
        handler["id"] = json!(format!("handler-{index}"));
        many.push(handler);
    }
    value["handlers"] = json!(many);
    expect_err(&value, "maxHandlers")
}

#[test]
fn max_egress_hosts_budget_is_sixteen() -> TestResult {
    let mut value = fixture();
    let hosts: Vec<Value> = (0..17)
        .map(|index| json!({"host": format!("host-{index}.example.com"), "port": 443}))
        .collect();
    value["handlers"][0]["egress"] = json!(hosts);
    expect_err(&value, "maxEgressHosts")
}

#[test]
fn handler_module_must_be_a_declared_build_input() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["module"] = json!("handlers/unlocked.ts");
    expect_err(&value, "not a declared build input")
}

#[test]
fn every_handler_output_resource_needs_a_matching_declared_output() -> TestResult {
    let mut value = fixture();
    value["outputs"] = json!([]);
    // Now no declared Output matches the "output-flags" handlerOutput
    // resource, and the handler still references it.
    value["handlers"][0]["outputs"] = json!([]);
    expect_err(&value, "matching declared output")
}
