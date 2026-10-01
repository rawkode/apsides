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
fn handler_may_not_trigger_on_its_own_output() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["trigger"]["input"] = json!("output-flags-input");
    value["handlers"][0]["inputs"] = json!(["output-flags-input"]);
    expect_err(&value, "may not trigger on or read its own output")
}

/// `fixture()` plus a second output, "other" (resource `output-other`,
/// Output-mode input `output-other-input`), and `handler-b`, which
/// triggers on and reads `sync-flags`' output `flags` and writes "other".
fn chained_fixture() -> TestResult<Value> {
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
    Ok(value)
}

#[test]
fn a_handler_may_trigger_on_and_read_another_handlers_output() -> TestResult {
    let compiled = plan(&chained_fixture()?)?;
    let downstream = compiled
        .ir
        .handlers
        .iter()
        .find(|handler| handler.id == "handler-b")
        .ok_or("handler-b must be declared")?;
    assert_eq!(downstream.trigger.input, "output-flags-input");
    assert_eq!(downstream.inputs, ["output-flags-input"]);
    Ok(())
}

#[test]
fn two_handler_cycle_is_rejected() -> TestResult {
    let mut value = chained_fixture()?;
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

/// Numeric IPv4 shorthand (decimal, dotted-decimal, hex or octal) and any
/// host that otherwise "ends in a number" (the WHATWG URL host check) must
/// never pass as a domain name — a browser or Node's `net` module would
/// parse every one of these as an IP address, not a hostname.
#[test]
fn egress_host_rejects_every_numeric_ip_shorthand() -> TestResult {
    for host in [
        "127.1",
        "2130706433",
        "0x7f.1",
        "0177.0.0.1",
        "1.2.3.04",
        "example.123",
    ] {
        let mut value = fixture();
        value["handlers"][0]["egress"] = json!([{"host": host, "port": 443}]);
        expect_err(&value, "lowercase DNS name")?;
    }
    Ok(())
}

#[test]
fn egress_host_rejects_localhost_and_other_single_label_hosts() -> TestResult {
    for host in ["localhost", "internal"] {
        let mut value = fixture();
        value["handlers"][0]["egress"] = json!([{"host": host, "port": 443}]);
        expect_err(&value, "lowercase DNS name")?;
    }
    Ok(())
}

/// RFC 6761 reserves the `.localhost` TLD for loopback; any host under it
/// must be rejected exactly like bare `localhost`.
#[test]
fn egress_host_rejects_the_dot_localhost_suffix() -> TestResult {
    for host in ["foo.localhost", "bar.foo.localhost"] {
        let mut value = fixture();
        value["handlers"][0]["egress"] = json!([{"host": host, "port": 443}]);
        expect_err(&value, "lowercase DNS name")?;
    }
    Ok(())
}

/// A host whose last label is a real (non-numeric) TLD is still accepted
/// even when an earlier label happens to be all digits — only the *last*
/// label decides "ends in a number".
#[test]
fn egress_host_accepts_a_numeric_non_last_label() -> TestResult {
    let mut value = fixture();
    value["handlers"][0]["egress"] = json!([{"host": "203.example.com", "port": 443}]);
    plan(&value)?;
    Ok(())
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
fn max_egress_rows_budget_is_two_hundred_fifty_six() -> TestResult {
    let mut value = fixture();
    // 16 distinct hosts (at, not over, `maxEgressHosts`) times 17 distinct
    // ports each: 272 rows, over `maxEgressRows` (256) despite never
    // tripping the distinct-host budget.
    let mut rows = Vec::new();
    for host_index in 0..16 {
        for port_offset in 0..17_u16 {
            rows.push(json!({
                "host": format!("host-{host_index}.example.com"),
                "port": 443 + port_offset,
            }));
        }
    }
    assert_eq!(rows.len(), 272);
    value["handlers"][0]["egress"] = json!(rows);
    expect_err(&value, "maxEgressRows")
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

// --- B1: a handler output is reachable only through its own Output-mode
// input; a Managed input aliasing the same resource must be rejected before
// it can bypass the self-loop/cycle checks below (which only look at
// Output-mode inputs). ---

/// A Managed input pointing at a `handlerOutput` resource is rejected
/// outright: it would read the reconciler's own static desired value
/// (always `{}`, since a handler-output resource never carries `data`), not
/// the handler's live write.
fn add_managed_alias(value: &mut Value) -> TestResult {
    value["inputs"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "flags-alias", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
        "mode": "managed", "managedResourceId": "output-flags", "projection": ["data"], "sensitive": false,
        "missing": "block-dependent-mutations", "source": source(),
    }));
    Ok(())
}

#[test]
fn managed_input_cannot_alias_a_handler_output() -> TestResult {
    let mut value = fixture();
    add_managed_alias(&mut value)?;
    expect_err(&value, "managed input cannot reference a handler output")
}

#[test]
fn handler_self_read_through_a_managed_alias_is_rejected() -> TestResult {
    let mut value = fixture();
    add_managed_alias(&mut value)?;
    value["handlers"][0]["inputs"] = json!(["settings", "flags-alias"]);
    // The alias itself is rejected before the handler graph is even built.
    expect_err(&value, "managed input cannot reference a handler output")
}

#[test]
fn handler_self_trigger_through_a_managed_alias_is_rejected() -> TestResult {
    let mut value = fixture();
    add_managed_alias(&mut value)?;
    value["handlers"][0]["trigger"]["input"] = json!("flags-alias");
    value["handlers"][0]["inputs"] = json!(["flags-alias"]);
    expect_err(&value, "managed input cannot reference a handler output")
}

#[test]
fn two_handler_cycle_through_managed_aliases_is_rejected() -> TestResult {
    let mut value = fixture();
    // A second output/handler pair, then close the cycle through Managed
    // aliases of both outputs instead of their real Output-mode inputs.
    value["resources"].as_array_mut().ok_or("not an array")?.push(json!({
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
    value["inputs"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "other-alias", "apiVersion": "v1", "kind": "ConfigMap", "name": "other", "namespace": "workload",
        "mode": "managed", "managedResourceId": "output-other", "projection": ["data"], "sensitive": false,
        "missing": "block-dependent-mutations", "source": source(),
    }));
    value["handlers"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "handler-b", "module": "handlers/sync-flags.ts", "export": "syncFlags",
        "trigger": {"kind": "input-changed", "input": "flags-alias-b"},
        "inputs": ["flags-alias-b"], "egress": [], "outputs": ["output-other"],
        "timeoutMillis": 30000, "maxHeapBytes": 67_108_864, "bundleSha256": "1".repeat(64), "source": source(),
    }));
    value["inputs"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "flags-alias-b", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
        "mode": "managed", "managedResourceId": "output-flags", "projection": ["data"], "sensitive": false,
        "missing": "block-dependent-mutations", "source": source(),
    }));
    value["handlers"][0]["inputs"] = json!(["settings", "other-alias"]);
    // Rejected at the Managed-alias check, never reaching cycle detection.
    expect_err(&value, "managed input cannot reference a handler output")
}

// --- N2: exactly one Output-mode input per output. ---

#[test]
fn output_with_no_watching_input_is_rejected() -> TestResult {
    let mut value = fixture();
    value["inputs"] = Value::Array(
        value["inputs"]
            .as_array()
            .ok_or("not an array")?
            .iter()
            .filter(|input| input["id"] != "output-flags-input")
            .cloned()
            .collect(),
    );
    value["handlers"][0]["inputs"] = json!(["settings"]);
    value["handlers"][0]["outputs"] = json!([]);
    expect_err(&value, "exactly one Output-mode input")
}

#[test]
fn output_with_two_watching_inputs_is_rejected() -> TestResult {
    let mut value = fixture();
    value["inputs"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "output-flags-input-2", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
        "mode": "output", "managedResourceId": "output-flags", "projection": ["data"], "sensitive": false,
        "missing": "block-dependent-mutations", "source": source(),
    }));
    expect_err(&value, "exactly one Output-mode input")
}

// --- One writer per output (binding decision): two handlers cannot declare
// the same output. ---

#[test]
fn two_handlers_cannot_write_the_same_output() -> TestResult {
    let mut value = fixture();
    value["handlers"].as_array_mut().ok_or("not an array")?.push(json!({
        "id": "handler-b", "module": "handlers/sync-flags.ts", "export": "syncFlags",
        "trigger": {"kind": "input-changed", "input": "settings"},
        "inputs": ["settings"], "egress": [], "outputs": ["output-flags"],
        "timeoutMillis": 30000, "maxHeapBytes": 67_108_864, "bundleSha256": "1".repeat(64), "source": source(),
    }));
    expect_err(&value, "already has a writer handler")
}

// --- B3: an output's rolloutOn test plan carries only the scenarios that
// apply to a handler output, never the full Observed set. ---

#[test]
fn output_rollout_test_plan_excludes_inapplicable_observed_scenarios() -> TestResult {
    let mut value = fixture();
    value["reactions"] = json!([{
        "id": "rollout-flags", "op": "rollout", "inputs": ["output-flags-input"], "target": "api", "source": source(),
    }]);
    let compiled = plan(&value)?;
    let scenarios: std::collections::BTreeSet<&str> = compiled
        .test_plan
        .iter()
        .filter(|case| case.reaction_id == "rollout-flags")
        .map(|case| case.scenario.as_str())
        .collect();
    for expected in [
        "projected-change",
        "restart-and-relist",
        "initial-evaluation",
    ] {
        assert!(
            scenarios.contains(expected),
            "expected {expected} in {scenarios:?}"
        );
    }
    for excluded in [
        "missing-input",
        "input-returned",
        "unauthorized-input",
        "credential-read-denial",
        "unavailable-input",
        "invalid-input",
        "coalesced-inputs",
    ] {
        assert!(
            !scenarios.contains(excluded),
            "did not expect {excluded} in {scenarios:?}"
        );
    }
    Ok(())
}

// --- B3 residual: a reaction reading both an Observed and an Output-mode
// input must not produce two rows for the same scenario name. ---

#[test]
fn mixed_observed_and_output_reaction_has_one_row_per_scenario_covering_every_live_input()
-> TestResult {
    let mut value = fixture();
    value["reactions"] = json!([{
        "id": "rollout-mixed", "op": "rollout", "inputs": ["settings", "output-flags-input"], "target": "api", "source": source(),
    }]);
    let compiled = plan(&value)?;
    let cases: Vec<_> = compiled
        .test_plan
        .iter()
        .filter(|case| case.reaction_id == "rollout-mixed")
        .collect();

    // (reactionId, scenario) must be unique: no two cases share a scenario name.
    let scenarios: Vec<&str> = cases.iter().map(|case| case.scenario.as_str()).collect();
    let unique: std::collections::BTreeSet<&str> = scenarios.iter().copied().collect();
    assert_eq!(
        scenarios.len(),
        unique.len(),
        "duplicate scenario rows for one reaction: {scenarios:?}"
    );

    // The two scenarios shared between Observed and Output ("projected-change",
    // "restart-and-relist") each get exactly one row, covering both live inputs.
    for shared in ["projected-change", "restart-and-relist"] {
        let case = cases
            .iter()
            .find(|case| case.scenario == shared)
            .ok_or_else(|| format!("expected exactly one {shared} row"))?;
        assert_eq!(
            case.input_ids,
            ["output-flags-input", "settings"],
            "{shared} must cover every live input"
        );
    }

    // An Observed-only scenario still applies, but only to the Observed input.
    let coalesced = cases
        .iter()
        .find(|case| case.scenario == "coalesced-inputs")
        .ok_or("expected a coalesced-inputs row")?;
    assert_eq!(coalesced.input_ids, ["settings"]);

    // 7 COMMON + 2 shared (projected-change, restart-and-relist) + 7 Observed-only.
    assert_eq!(cases.len(), 16, "unexpected case count: {cases:?}");
    Ok(())
}

// --- N3: `derive_surface` reports handler/egress/output rows, sorted, with
// egress attribution kept per handler rather than collapsed across
// handlers. ---

/// Two handlers, each writing its own output, sharing one egress host, with
/// resources and handlers declared out of alphabetical order.
fn two_handler_two_output_fixture() -> Value {
    let deployment = |id: &str, name: &str| {
        json!({
            "id": id, "apiVersion": "apps/v1", "kind": "Deployment", "name": name, "namespace": "workload",
            "desired": {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "namespace": "workload"}, "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}}, "template": {"metadata": {"labels": {"app": name}}, "spec": {"serviceAccountName": "api-workload", "containers": [{"name": name, "image": format!("example.invalid/api@sha256:{}", "0".repeat(64))}]}}}},
            "lifecycle": {"deletionPolicy": "orphan"}, "source": source(),
        })
    };
    let handler_output = |id: &str, name: &str| {
        json!({
            "id": id, "apiVersion": "v1", "kind": "ConfigMap", "name": name, "namespace": "workload",
            "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": "workload", "labels": {"apsides.dev/handler-output": id}}},
            "lifecycle": {"deletionPolicy": "orphan"}, "source": source(), "handlerOutput": true,
        })
    };
    let output_input = |id: &str, resource_id: &str, name: &str| json!({"id": id, "apiVersion": "v1", "kind": "ConfigMap", "name": name, "namespace": "workload", "mode": "output", "managedResourceId": resource_id, "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()});
    json!({
        "schemaVersion": IR_SCHEMA,
        "domain": {
            "id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
            "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
            "minKubernetesVersion": "1.34",
            "budgets": {"maxResources": 32, "maxInputs": 32, "maxReactions": 32, "maxObjectBytes": 262_144, "maxQueueDepth": 32, "maxConcurrentRequests": 4},
        },
        // Declared out of alphabetical order, to exercise `derive_surface`'s sorting.
        "resources": [
            deployment("api", "api"),
            handler_output("output-metrics", "metrics"),
            handler_output("output-flags", "flags"),
        ],
        "inputs": [
            {"id": "settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "namespace": "workload", "mode": "observed", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
            output_input("output-flags-input", "output-flags", "flags"),
            output_input("output-metrics-input", "output-metrics", "metrics"),
        ],
        "reactions": [],
        // "zeta-handler" is declared before "alpha-handler" to exercise sorting by id.
        "handlers": [
            {
                "id": "zeta-handler", "module": "handlers/zeta.ts", "export": "zeta",
                "trigger": {"kind": "input-changed", "input": "settings"},
                "inputs": ["settings"],
                // Same host:port as "alpha-handler" below: two handlers reaching the
                // same host is allowed (only a *single* handler's own list must be
                // distinct), so both rows must survive attributed to their own handler.
                "egress": [{"host": "api.example.com", "port": 443}],
                "outputs": ["output-flags"], "timeoutMillis": 30000, "maxHeapBytes": 67_108_864,
                "bundleSha256": "1".repeat(64), "source": source(),
            },
            {
                "id": "alpha-handler", "module": "handlers/alpha.ts", "export": "alpha",
                "trigger": {"kind": "input-changed", "input": "settings"},
                "inputs": ["settings"],
                "egress": [
                    {"host": "api.example.com", "port": 443},
                    {"host": "metrics.example.com", "port": 8443},
                ],
                "outputs": ["output-metrics"], "timeoutMillis": 30000, "maxHeapBytes": 67_108_864,
                "bundleSha256": "2".repeat(64), "source": source(),
            },
        ],
        "outputs": [
            {"id": "output-metrics", "name": "metrics", "namespace": "workload"},
            {"id": "output-flags", "name": "flags", "namespace": "workload"},
        ],
        "provenance": {"compiler": "apsides/0.1", "inputs": [
            {"path": "program.tsx", "sha256": "0".repeat(64)},
            {"path": "handlers/zeta.ts", "sha256": "1".repeat(64)},
            {"path": "handlers/alpha.ts", "sha256": "2".repeat(64)},
        ]},
    })
}

#[test]
fn derive_surface_reports_handler_egress_and_output_rows_sorted_and_per_handler_attributed()
-> TestResult {
    let compiled = plan(&two_handler_two_output_fixture())?;
    let surface = derive_surface(&compiled);

    // Handler rows: sorted by id, each carrying its own inputs/outputs.
    assert_eq!(
        surface
            .handlers
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha-handler", "zeta-handler"]
    );
    assert_eq!(surface.handlers[0].trigger, "settings");
    assert_eq!(surface.handlers[0].inputs, ["settings"]);
    assert_eq!(surface.handlers[0].outputs, ["output-metrics"]);
    assert_eq!(surface.handlers[1].outputs, ["output-flags"]);

    // Egress rows: sorted by (host, port, handler); the shared host is
    // attributed separately to each handler, never collapsed into one row.
    let egress: Vec<(&str, u16, &str)> = surface
        .egress
        .iter()
        .map(|row| (row.host.as_str(), row.port, row.handler.as_str()))
        .collect();
    assert_eq!(
        egress,
        [
            ("api.example.com", 443, "alpha-handler"),
            ("api.example.com", 443, "zeta-handler"),
            ("metrics.example.com", 8443, "alpha-handler"),
        ]
    );

    // Output rows: sorted by id.
    assert_eq!(
        surface
            .outputs
            .iter()
            .map(|o| o.id.as_str())
            .collect::<Vec<_>>(),
        ["output-flags", "output-metrics"]
    );
    assert_eq!(surface.outputs[0].name, "flags");
    assert_eq!(surface.outputs[1].name, "metrics");
    Ok(())
}
