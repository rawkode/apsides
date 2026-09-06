use super::*;
use compiler_core::canonical_bytes;

#[path = "review_projection_tests.rs"]
mod projection;
#[path = "review_v4_tests.rs"]
mod v4;

fn source(line: u32) -> Value {
    json!({"file":"program.tsx","line":line,"column":3})
}

fn declaration(private: &str) -> Result<Value> {
    let (_, plan) = registry()?;
    let mut ir = serde_json::to_value(&plan.request.next.compiled.ir)?;
    ir["provenance"]["inputs"][0]["path"] = json!("program.tsx");
    ir["resources"] = json!([
        {"id":"owned","apiVersion":"v1","kind":"ConfigMap","name":"owned","namespace":"workload",
            "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"owned","namespace":"workload"},"data":{"a/b~c":private,"private":private}},
            "lifecycle":{"deletionPolicy":"orphan"},"source":source(4)},
        {"id":"api","apiVersion":"apps/v1","kind":"Deployment","name":"api","namespace":"workload",
            "desired":{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"api","namespace":"workload"},
                "spec":{"replicas":1,"selector":{"matchLabels":{"app":"api"}},"template":{"metadata":{"labels":{"app":"api"},"annotations":{}},
                    "spec":{"serviceAccountName":"api-workload","containers":[{"name":"api","image":format!("registry.example/api@{BUILD}"),"args":[private]}]}}}},
            "lifecycle":{"deletionPolicy":"orphan"},"source":source(12)}
    ]);
    ir["inputs"] = json!([
        {"id":"managed","apiVersion":"v1","kind":"ConfigMap","name":"owned","namespace":"workload","mode":"managed","managedResourceId":"owned","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":source(6)},
        {"id":"external","apiVersion":"v1","kind":"ConfigMap","name":"external","namespace":"workload","mode":"observed","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":source(7)},
        {"id":"credentials","apiVersion":"v1","kind":"Secret","name":"database","namespace":"workload","mode":"observed","projection":["data"],"sensitive":true,"missing":"block-dependent-mutations","source":source(8)}
    ]);
    ir["reactions"] = json!([{ "id":"restart","op":"rollout","inputs":["managed","external","credentials"],"target":"api","source":source(10)}]);
    Ok(ir)
}

fn transition(ir: &Value) -> Result<(StoredTransition, deploy::TransitionPlan)> {
    let (registry, base) = registry()?;
    let mut stored = registry.transition(TRANSITION_1)?.clone();
    let mut request = base.request;
    request.next.compiled = compiler_core::parse_and_validate(&ir.to_string())?;
    let compiled = &request.next.compiled;
    request.runtime_ceiling.capabilities = compiled.capabilities.clone();
    request.runtime_ceiling.accepted_enforcement_gaps = compiled.enforcement_gaps.clone();
    request.runtime_ceiling.accepted_indirect_risks = compiled.indirect_risks.clone();
    request.cleanup_ceiling = request.runtime_ceiling.clone();
    let plan = plan_transition(&request)?;
    stored.next_review_metadata = platform::project_review_metadata(&plan)?.reference()?;
    stored.inputs = frozen_inputs(&plan)?;
    stored.record = begin(&plan, "job-1")?;
    Ok((stored, plan))
}

fn report(ir: &Value) -> Result<Value> {
    let (stored, plan) = transition(ir)?;
    crate::review::transition_review_fixture(&stored, &plan, TRANSITION_1, None)
}

#[test]
fn initial_review_uses_complete_compiler_fields_and_explicit_empty_baselines() -> Result<()> {
    let report = report(&declaration("private-value-canary")?)?;
    assert_eq!(report["schemaVersion"], "reakt.dev/deployment-review/v4");
    for category in [
        "reactions",
        "inputAuthority",
        "effects",
        "dataflow",
        "sourceLocations",
    ] {
        assert_eq!(
            report[category]["baseline"], "empty-initial-declaration",
            "{category}"
        );
        assert_eq!(report[category]["before"], json!([]), "{category}");
        assert_eq!(report[category]["removed"], json!([]), "{category}");
        assert_eq!(report[category]["changed"], json!([]), "{category}");
        assert_eq!(
            report[category]["added"], report[category]["after"],
            "{category}"
        );
    }
    let resources = report["resources"].as_array().ok_or("resources missing")?;
    let owned = resources
        .iter()
        .find(|row| row["name"] == "owned")
        .ok_or("owned missing")?;
    assert_eq!(
        owned["fieldChanges"],
        json!([
            {"path":"/data/a~1b~0c","change":"added"},
            {"path":"/data/private","change":"added"}
        ])
    );
    let api = resources
        .iter()
        .find(|row| row["name"] == "api")
        .ok_or("api missing")?;
    let fields = api["fieldChanges"].as_array().ok_or("fields missing")?;
    assert!(
        fields
            .iter()
            .any(|row| row["path"] == "/spec/template/spec/containers")
    );
    assert!(
        fields
            .iter()
            .any(|row| row["path"] == "/spec/template/metadata/annotations")
    );
    assert!(!fields.iter().any(|row| {
        row["path"]
            .as_str()
            .is_some_and(|path| path.contains("containers/0"))
    }));
    assert_eq!(api["declarationChange"], "added");
    assert_eq!(api["ownershipChange"]["status"], "create-if-absent");
    assert_eq!(report["liveImpact"]["status"], "unavailable");
    assert_eq!(
        report["liveImpact"]["reason"],
        "live-server-side-apply-preview-not-executed"
    );
    Ok(())
}

#[test]
fn initial_review_preserves_input_sensitivity_missing_policy_and_managed_association() -> Result<()>
{
    let report = report(&declaration("private-value-canary")?)?;
    let inputs = report["inputAuthority"]["after"]
        .as_array()
        .ok_or("inputs missing")?;
    assert_eq!(inputs.len(), 3);
    for input in inputs {
        assert_eq!(input["missing"], "block-dependent-mutations");
        assert_eq!(input["projection"], json!(["data"]));
        assert_eq!(input["source"]["file"], "program.tsx");
    }
    let managed = inputs
        .iter()
        .find(|row| row["id"] == "managed")
        .ok_or("managed missing")?;
    assert_eq!(managed["managedResourceId"], "owned");
    assert_eq!(managed["mode"], "managed");
    let credentials = inputs
        .iter()
        .find(|row| row["id"] == "credentials")
        .ok_or("credentials missing")?;
    assert_eq!(credentials["sensitive"], true);
    assert_eq!(credentials["mode"], "observed");
    Ok(())
}

#[test]
fn private_value_changes_do_not_change_declaration_report() -> Result<()> {
    let mut first = report(&declaration("private-value-canary-a")?)?;
    let mut second = report(&declaration("private-value-canary-b")?)?;
    assert_ne!(first["planDigest"], second["planDigest"]);
    let encoded = serde_json::to_string(&first)?;
    assert!(!encoded.contains("private-value-canary"));
    assert!(!encoded.contains("irDigest"));
    assert!(!encoded.contains("permittedApply"));
    first
        .as_object_mut()
        .ok_or("report object missing")?
        .remove("planDigest");
    second
        .as_object_mut()
        .ok_or("report object missing")?
        .remove("planDigest");
    assert_eq!(canonical_bytes(&first)?, canonical_bytes(&second)?);
    Ok(())
}

#[test]
fn review_rejects_substituted_derived_fields_and_plan_receipts() -> Result<()> {
    let (stored, mut plan) = transition(&declaration("private-value-canary")?)?;
    plan.request
        .next
        .compiled
        .effects
        .first_mut()
        .ok_or("effect missing")?
        .field_paths
        .push("/unapproved".into());
    assert!(crate::review::transition_review_fixture(&stored, &plan, TRANSITION_1, None).is_err());
    let (mut stored, plan) = transition(&declaration("private-value-canary")?)?;
    stored.record.phase = Phase::Committed;
    assert!(crate::review::transition_review_fixture(&stored, &plan, TRANSITION_1, None).is_err());
    Ok(())
}

#[test]
fn review_preserves_canonical_graph_and_separates_permission_requirements() -> Result<()> {
    let (stored, plan) = transition(&declaration("private-value-canary")?)?;
    crate::review::preflight(&plan, None)?;
    let report = crate::review::transition_review_fixture(&stored, &plan, TRANSITION_1, None)?;
    assert_eq!(
        report["dataflow"]["after"],
        json!([{
            "reaction":"restart", "inputs":["credentials","external","managed"], "target":"api"
        }])
    );
    let effects = report["effects"]["after"]
        .as_array()
        .ok_or("effects missing")?;
    assert_eq!(effects.len(), plan.request.next.compiled.effects.len());
    for effect in &plan.request.next.compiled.effects {
        assert!(effects.iter().any(|row| row
            == &json!({
                "id": if effect.operation == "server-side-apply" { format!("resource:{}", effect.target) } else { "reaction:restart".to_owned() },
                "target":effect.target, "operation":effect.operation,
                "fieldPaths":effect.field_paths, "source":effect.source
            })));
    }
    assert_eq!(
        report["sourceLocations"]["after"]
            .as_array()
            .ok_or("sources missing")?
            .len(),
        6
    );
    assert_eq!(
        report["permissions"]["runtime"]["after"],
        serde_json::to_value(&plan.request.next.compiled.capabilities)?
    );
    assert_eq!(
        report["permissions"]["cleanup"]["changes"]["after"],
        serde_json::to_value(&plan.cleanup_capabilities)?
    );
    let required = platform::deployment_permissions(&plan)?;
    assert_eq!(
        report["permissions"]["deployment"]["requiredResources"]["after"],
        serde_json::to_value(&required.required_resources)?
    );
    assert_eq!(
        report["permissions"]["deployment"]["sourceMayChangeAuthority"],
        false
    );
    assert_eq!(report["compatibility"]["before"], json!([]));
    assert_eq!(
        report["compatibility"]["added"],
        report["compatibility"]["after"]
    );
    assert_eq!(
        report["compatibility"]["after"][0]["fieldValidation"],
        "Strict"
    );
    let compatibility = &report["compatibility"]["after"][0];
    assert_eq!(compatibility["requiredCleanupApis"], json!([]));
    let runtime = compatibility["requiredRuntimeApis"]
        .as_array()
        .ok_or("runtime APIs missing")?;
    assert!(
        runtime
            .iter()
            .any(|api| api["resource"] == "secrets" && api["verb"] == "get")
    );
    assert!(
        runtime
            .iter()
            .all(|api| api["apiGroup"] != "rbac.authorization.k8s.io")
    );
    let deployment = compatibility["requiredDeploymentApis"]
        .as_array()
        .ok_or("deployment APIs missing")?;
    assert!(
        deployment
            .iter()
            .any(|api| api["apiGroup"] == "rbac.authorization.k8s.io")
    );
    assert!(
        !deployment
            .iter()
            .any(|api| api["resource"] == "serviceaccounts"
                && api["subresource"] == "token"
                && api["verb"] == "create")
    );
    Ok(())
}
