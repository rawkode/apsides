//! A small fixture program shared by the offline unit tests: one managed
//! `ConfigMap`, one observed `ConfigMap` and one observed `Secret`, each driving a
//! rollout of the same `Deployment`.

use crate::{FIXTURE_IMAGE, Result, contract::MUTATION_SCENARIO};
use compiler_core::{CompiledPlan, InputMode, digest};
use serde_json::{Value, json};

pub fn ir() -> Value {
    let source = json!({"file": "program.tsx", "line": 1, "column": 1});
    json!({
        "testOnly": true,
        "schemaVersion": "apsides.dev/ir/v2",
        "domain": {"id": "unit", "namespace": "work", "controlNamespace": "control",
            "clusterBinding": "unit-fixture", "policyRef": "platform/unit", "enforcement": "api-bound",
            "minKubernetesVersion": "1.34",
            "budgets": {"maxResources": 8, "maxInputs": 8, "maxReactions": 8,
                "maxObjectBytes": 262_144, "maxQueueDepth": 8, "maxConcurrentRequests": 2}},
        "resources": [
            {"id": "configmap-settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings",
             "namespace": "work",
             "desired": {"apiVersion": "v1", "kind": "ConfigMap",
                 "metadata": {"name": "settings", "namespace": "work"}, "data": {"LEVEL": "info"}},
             "lifecycle": {"deletionPolicy": "orphan"}, "source": source},
            {"id": "deployment-api", "apiVersion": "apps/v1", "kind": "Deployment", "name": "api",
             "namespace": "work",
             "desired": {"apiVersion": "apps/v1", "kind": "Deployment",
                 "metadata": {"name": "api", "namespace": "work"},
                 "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "api"}},
                     "template": {"metadata": {"labels": {"app": "api"}},
                         "spec": {"serviceAccountName": "api", "automountServiceAccountToken": false,
                             "containers": [{"name": "api", "image": FIXTURE_IMAGE,
                                 "imagePullPolicy": "Never"}]}}}},
             "lifecycle": {"deletionPolicy": "orphan"}, "source": source},
        ],
        "inputs": [
            {"id": "managed-settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings",
             "namespace": "work", "mode": "managed", "managedResourceId": "configmap-settings",
             "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations",
             "source": source},
            {"id": "observed-external", "apiVersion": "v1", "kind": "ConfigMap", "name": "external",
             "namespace": "work", "mode": "observed", "projection": ["data"], "sensitive": false,
             "missing": "block-dependent-mutations", "source": source},
            {"id": "observed-credentials", "apiVersion": "v1", "kind": "Secret", "name": "credentials",
             "namespace": "work", "mode": "observed", "projection": ["data"], "sensitive": true,
             "missing": "block-dependent-mutations", "source": source},
        ],
        "reactions": [
            {"id": "rollout-managed", "op": "rollout", "inputs": ["managed-settings"],
             "target": "deployment-api", "source": source},
            {"id": "rollout-external", "op": "rollout", "inputs": ["observed-external"],
             "target": "deployment-api", "source": source},
            {"id": "rollout-credentials", "op": "rollout", "inputs": ["observed-credentials"],
             "target": "deployment-api", "source": source},
        ],
        "handlers":[],"outputs":[],"provenance": {"compiler": "apsides/0.1", "inputs": [{"path": "program.tsx",
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000"}]},
    })
}

pub fn plan() -> Result<CompiledPlan> {
    Ok(compiler_core::parse_and_validate(&ir().to_string())?)
}

/// The fixtures, overlay and coverage documents the compiler would stage for
/// [`plan`].
pub fn fixture_documents(plan: &CompiledPlan) -> Result<(Value, Value, Value)> {
    // `Output`-mode inputs get no synthetic fixture: unlike an `Observed`
    // input, there is no "present"/"missing" presence state to synthesize —
    // the reconciler itself creates the handler-output `ConfigMap`
    // absent-only every cycle, before anything observes it, so
    // `missing-input`/`input-returned`-style presence scenarios never apply
    // to an output (see `test_plan.rs`'s `OUTPUT` scenario set).
    let fixtures = json!({
        "schemaVersion": "apsides.dev/fixtures/v1", "testOnly": true, "productionReady": false,
        "inputs": plan.ir.inputs.iter()
            .filter(|input| input.mode == InputMode::Observed)
            .map(|input| json!({"inputId": input.id, "state": "present", "synthetic": true,
                "data": {"value": format!("synthetic-{}", input.id)}}))
            .collect::<Vec<_>>(),
    });
    let fixture_hex = hex(&digest(&fixtures)?);
    let ir_hex = hex(&plan.ir_digest);
    let production_hex = "1".repeat(64);
    let reactions = plan
        .ir
        .reactions
        .iter()
        .map(|reaction| {
            let mut cases = plan
                .test_plan
                .iter()
                .filter(|case| case.reaction_id == reaction.id)
                .map(|case| {
                    let inputs = plan
                        .ir
                        .inputs
                        .iter()
                        .filter(|input| case.input_ids.contains(&input.id))
                        .collect::<Vec<_>>();
                    let modes = inputs
                        .iter()
                        .map(|input| match input.mode {
                            InputMode::Observed | InputMode::Output => "observed",
                            InputMode::Managed => "managed",
                        })
                        .collect::<std::collections::BTreeSet<_>>();
                    let managed = inputs
                        .iter()
                        .filter_map(|input| input.managed_resource_id.clone())
                        .collect::<Vec<_>>();
                    json!({"reactionId": case.reaction_id, "scenario": case.scenario,
                        "status": "required", "fixtureSha256": fixture_hex, "overlayIdentity": ir_hex,
                        "applicability": {"inputIds": case.input_ids, "inputModes": modes,
                            "managedResourceIds": managed}})
                })
                .collect::<Vec<_>>();
            cases.push(json!({"reactionId": reaction.id, "scenario": MUTATION_SCENARIO,
                "status": "required", "fixtureSha256": fixture_hex, "overlayIdentity": ir_hex}));
            json!({"id": reaction.id, "cases": cases})
        })
        .collect::<Vec<_>>();
    let coverage = json!({
        "schemaVersion": "apsides.dev/fixture-coverage/v1", "testOnly": true, "productionReady": false,
        "fixtureSha256": fixture_hex, "fixtureIrSha256": ir_hex, "productionIrSha256": production_hex,
        "reactions": reactions,
    });
    let overlay = json!({
        "schemaVersion": "apsides.dev/fixture-overlay/v1", "testOnly": true, "productionReady": false,
        "fixtureIrSha256": ir_hex, "productionIrSha256": production_hex,
    });
    Ok((fixtures, overlay, coverage))
}

fn hex(digest: &str) -> String {
    digest.trim_start_matches("sha256:").to_owned()
}
