use compiler_core::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn source() -> Value {
    json!({"file":"controller.ts","line":1,"column":1})
}

fn fixture() -> Value {
    json!({
        "schemaVersion":IR_SCHEMA,
        "domain":{"id":"example","namespace":"workload","controlNamespace":"reakt-system","clusterBinding":"test-cluster","policyRef":"platform/example","enforcement":"api-bound","budgets":{"maxResources":64,"maxInputs":64,"maxReactions":64,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":[{"id":"api","apiVersion":"apps/v1","kind":"Deployment","name":"api","namespace":"workload","desired":{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"api","namespace":"workload"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"api"}},"template":{"metadata":{"labels":{"app":"api"}},"spec":{"serviceAccountName":"api-workload","containers":[{"name":"api","image":"example.invalid/api@sha256:0000000000000000000000000000000000000000000000000000000000000000"}]}}}},"lifecycle":{"deletionPolicy":"orphan"},"source":source()}],
        "inputs":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"external","namespace":"workload","mode":"observed","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":source()}],
        "reactions":[{"id":"restart","op":"rollout","inputs":["config"],"target":"api","source":source()}],
        "provenance":{"compiler":"reakt/0.1","inputs":[{"path":"controller.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    })
}

fn plan(value: &Value) -> CompiledPlan {
    parse_and_validate(&value.to_string()).expect("valid fixture")
}

fn snapshots(id: &str, data: Value) -> BTreeMap<String, InputSnapshot> {
    BTreeMap::from([(
        id.into(),
        InputSnapshot::Available {
            data,
            authoritative_applied: true,
        },
    )])
}

fn ceiling(plan: &CompiledPlan) -> PermissionCeiling {
    PermissionCeiling {
        policy_ref: plan.ir.domain.policy_ref.clone(),
        domain_id: plan.ir.domain.id.clone(),
        cluster_binding: plan.ir.domain.cluster_binding.clone(),
        workload_namespace: plan.ir.domain.namespace.clone(),
        control_namespace: plan.ir.domain.control_namespace.clone(),
        capabilities: plan.capabilities.clone(),
        accepted_enforcement_gaps: plan.enforcement_gaps.clone(),
        accepted_indirect_risks: plan.indirect_risks.clone(),
    }
}

#[test]
fn canonical_digest_is_independently_known_and_key_order_independent() {
    assert_eq!(
        digest(&json!({"b":2,"a":1})).unwrap(),
        "sha256:43258cff783fe7036d8a43033f830adfc60ec037382473548ac742b888292777"
    );
    let original = fixture();
    let pretty = serde_json::to_string_pretty(&original).unwrap();
    assert_eq!(plan(&original), parse_and_validate(&pretty).unwrap());
}

#[test]
fn strict_json_rejects_duplicate_keys_unknown_fields_and_ambiguous_numbers() {
    for value in [
        "{\"a\":1,\"a\":2}",
        "{\"nested\":{\"x\":1,\"x\":2}}",
        "{\"n\":1.0}",
        "{\"n\":1e2}",
        "{\"n\":9007199254740992}",
    ] {
        assert!(parse_strict(value).is_err(), "accepted {value}");
    }
    let mut value = fixture();
    value["runtimePlan"] = json!({"allow":"*"});
    assert!(parse_and_validate(&value.to_string()).is_err());
    let mut value = fixture();
    value["domain"]["enforcement"] = json!("effect-bound");
    assert!(parse_and_validate(&value.to_string()).is_err());
}

#[test]
fn unsupported_effects_and_scope_cannot_cross_ir_boundary() {
    let mut cases = Vec::new();
    let mut value = fixture();
    value["resources"][0]["desired"]["spec"]["template"]["spec"]["hostNetwork"] = json!(true);
    cases.push(value);
    let mut value = fixture();
    value["resources"][0]["namespace"] = json!("other");
    cases.push(value);
    let mut value = fixture();
    value["resources"][0]["desired"]["metadata"]["name"] = json!("other");
    cases.push(value);
    let mut value = fixture();
    value["resources"][0]["desired"]["status"] = json!({});
    cases.push(value);
    let mut value = fixture();
    value["resources"][0]["desired"]["spec"]["template"]["metadata"]["annotations"] =
        json!({REVISION_ANNOTATION:"caller-controlled"});
    cases.push(value);
    let mut value = fixture();
    value["reactions"][0]["op"] = json!("raw-patch");
    cases.push(value);
    let mut value = fixture();
    value["reactions"][0]["inputs"] = json!(["unknown"]);
    cases.push(value);
    let mut value = fixture();
    value["inputs"][0]["projection"] = json!(["metadata"]);
    cases.push(value);
    let mut value = fixture();
    value["inputs"][0]["missing"] = json!("delete-target");
    cases.push(value);
    let mut value = fixture();
    value["domain"]["controlNamespace"] = json!("workload");
    cases.push(value);
    let mut value = fixture();
    value["resources"][0]["source"]["file"] = json!("undeclared.ts");
    cases.push(value);
    let mut value = fixture();
    let duplicate = value["resources"][0].clone();
    value["resources"].as_array_mut().unwrap().push(duplicate);
    cases.push(value);
    for value in cases {
        assert!(parse_and_validate(&value.to_string()).is_err());
    }
}

#[test]
fn capabilities_follow_requests_and_do_not_sneak_in_cleanup() {
    let plan = plan(&fixture());
    assert!(
        plan.capabilities.iter().all(|c| c.verb != "delete"
            && c.resource != "roles"
            && c.resource != "serviceaccounts")
    );
    assert!(
        plan.capabilities
            .iter()
            .any(|c| c.verb == "create" && c.names.is_empty() && c.resource == "deployments")
    );
    assert!(
        plan.capabilities
            .iter()
            .filter(|c| c.verb != "create")
            .all(|c| !c.names.is_empty())
    );
    for request in &plan.request_plan {
        assert!(plan.capabilities.contains(&request.capability));
        if matches!(request.capability.verb.as_str(), "list" | "watch") {
            assert_eq!(
                request.field_selector,
                Some(format!("metadata.name={}", request.capability.names[0]))
            );
        }
    }
    let dry_run: Vec<_> = plan
        .request_plan
        .iter()
        .filter(|request| request.dry_run)
        .collect();
    assert_eq!(dry_run.len(), plan.ir.resources.len());
    assert!(dry_run.iter().all(|request| {
        request.purpose == "managed-target-dry-run-apply"
            && request.capability.verb == "patch"
            && request.field_selector.is_none()
    }));
    assert!(
        plan.enforcement_gaps
            .contains(&"rbac-create-not-name-restricted".into())
    );
    assert!(
        plan.indirect_risks
            .contains(&"workload-service-account-authority".into())
    );
}

#[test]
fn deployment_secret_key_reference_is_validated_and_reported_as_risk() -> Result<()> {
    let mut value = fixture();
    value["resources"][0]["desired"]["spec"]["template"]["spec"]["containers"][0]["env"] = json!([{"name":"PGRST_DB_URI","valueFrom":{"secretKeyRef":{
        "name":"database-app","key":"uri"
    }}}]);
    let plan = parse_and_validate(&value.to_string())?;
    assert!(
        plan.indirect_risks
            .contains(&"workload-secret-reference".into())
    );

    let mut ambiguous = value.clone();
    ambiguous["resources"][0]["desired"]["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"] =
        json!("private");
    assert!(parse_and_validate(&ambiguous.to_string()).is_err());

    value["resources"][0]["desired"]["spec"]["template"]["spec"]["containers"][0]["env"][0]["valueFrom"]
        ["secretKeyRef"]["optional"] = json!(true);
    assert!(parse_and_validate(&value.to_string()).is_err());
    Ok(())
}

#[test]
fn custom_resource_mapping_drives_exact_capabilities_and_base_validation() -> Result<()> {
    let mut value = fixture();
    value["resources"] = json!([{
        "id":"database","apiVersion":"postgresql.cnpg.io/v1","kind":"Cluster",
        "apiResource":{"plural":"clusters","scope":"Namespaced",
            "schemaSha256":format!("sha256:{}", "a".repeat(64))},
        "name":"database","namespace":"workload",
        "desired":{"apiVersion":"postgresql.cnpg.io/v1","kind":"Cluster",
            "metadata":{"name":"database","namespace":"workload"},
            "spec":{"instances":1,"storage":{"size":"1Gi"}}},
        "lifecycle":{"deletionPolicy":"orphan"},"source":source()
    }]);
    value["reactions"] = json!([]);
    let plan = parse_and_validate(&value.to_string())?;
    assert!(plan.capabilities.iter().any(|capability| {
        capability.api_group == "postgresql.cnpg.io"
            && capability.resource == "clusters"
            && capability.verb == "patch"
            && matches!(capability.names.as_slice(), [name] if name == "database")
    }));

    let mut remapped_builtin = fixture();
    remapped_builtin["resources"][0]["apiResource"] = value["resources"][0]["apiResource"].clone();
    assert!(parse_and_validate(&remapped_builtin.to_string()).is_err());

    value["resources"][0]["desired"]["status"] = json!({});
    assert!(parse_and_validate(&value.to_string()).is_err());
    Ok(())
}

#[test]
fn independent_ceiling_requires_scope_gaps_risks_and_unmodified_derivation() {
    let plan = plan(&fixture());
    let policy = ceiling(&plan);
    validate_ceiling(&plan, &policy).unwrap();
    let mut missing = policy.clone();
    missing.capabilities.pop();
    assert!(validate_ceiling(&plan, &missing).is_err());
    let mut missing = policy.clone();
    missing.accepted_enforcement_gaps.clear();
    assert!(validate_ceiling(&plan, &missing).is_err());
    let mut missing = policy.clone();
    missing.accepted_indirect_risks.clear();
    assert!(validate_ceiling(&plan, &missing).is_err());
    let mut wrong_cluster = policy.clone();
    wrong_cluster.cluster_binding = "other".into();
    assert!(validate_ceiling(&plan, &wrong_cluster).is_err());
    let mut forged = plan.clone();
    forged.capabilities.clear();
    assert!(validate_ceiling(&forged, &policy).is_err());
    let mut wildcard = policy.clone();
    wildcard.capabilities[0].resource = "*".into();
    assert!(validate_ceiling(&plan, &wildcard).is_err());
    let mut named_create = policy;
    let create = named_create
        .capabilities
        .iter_mut()
        .find(|c| c.verb == "create")
        .unwrap();
    create.names = vec!["api".into()];
    assert!(validate_ceiling(&plan, &named_create).is_err());
}

#[test]
fn rollout_matches_independent_python_hash_vector_and_relevant_changes_only() {
    let plan = plan(&fixture());
    let input = snapshots("config", json!({"fixture":"synthetic-only"}));
    let expected = RolloutOutcome::Ready {
        revision: "sha256:0a81201fb87962c941642913beeddd7f69bfcbb65e9c16b326849f3084bd5909".into(),
    };
    assert_eq!(
        evaluate_rollout(&plan, "api", &input, None).unwrap(),
        expected
    );
    assert_eq!(
        evaluate_rollout(&plan, "api", &input, None).unwrap(),
        expected
    );
    assert_ne!(
        evaluate_rollout(
            &plan,
            "api",
            &snapshots("config", json!({"fixture":"changed"})),
            None
        )
        .unwrap(),
        expected
    );
}

#[test]
fn every_input_failure_blocks_without_encoding_projection_contents() {
    let plan = plan(&fixture());
    for (snapshot, reason) in [
        (InputSnapshot::Missing, BlockReason::Missing),
        (InputSnapshot::Unauthorized, BlockReason::Unauthorized),
        (InputSnapshot::Unavailable, BlockReason::Unavailable),
        (InputSnapshot::Invalid, BlockReason::Invalid),
        (
            InputSnapshot::Available {
                data: json!({"password":{"malformed":"TOP-SECRET"}}),
                authoritative_applied: true,
            },
            BlockReason::Invalid,
        ),
    ] {
        let result = evaluate_rollout(
            &plan,
            "api",
            &BTreeMap::from([("config".into(), snapshot)]),
            None,
        )
        .unwrap();
        assert_eq!(
            result,
            RolloutOutcome::Blocked {
                input_id: "config".into(),
                reason
            }
        );
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("TOP-SECRET")
        );
    }
}

fn secret_fixture() -> Value {
    let mut value = fixture();
    value["inputs"][0]["id"] = json!("secret");
    value["inputs"][0]["kind"] = json!("Secret");
    value["inputs"][0]["sensitive"] = json!(true);
    value["reactions"][0]["inputs"] = json!(["secret"]);
    value
}

#[test]
fn secret_rollout_matches_independent_hmac_vector_and_requires_key() {
    let plan = plan(&secret_fixture());
    let input = snapshots("secret", json!({"fixture":"c3ludGhldGljLW9ubHk="}));
    let key = RolloutKey {
        id: "key-v1".into(),
        bytes: vec![0x5a; 32],
    };
    let expected = RolloutOutcome::Ready {
        revision:
            "hmac-sha256:key-v1:a6e2f3f526db99efb8e6d75fa79eba835de3198d4688f9b99609982584d6d6a3"
                .into(),
    };
    assert_eq!(
        evaluate_rollout(&plan, "api", &input, Some(&key)).unwrap(),
        expected
    );
    assert!(matches!(
        evaluate_rollout(&plan, "api", &input, None).unwrap(),
        RolloutOutcome::Blocked {
            reason: BlockReason::KeyUnavailable,
            ..
        }
    ));
    let other_key = RolloutKey {
        id: "key-v1".into(),
        bytes: vec![0x42; 32],
    };
    assert_ne!(
        evaluate_rollout(&plan, "api", &input, Some(&other_key)).unwrap(),
        expected
    );
    let bad = snapshots("secret", json!({"fixture":"this is not base64!"}));
    assert!(matches!(
        evaluate_rollout(&plan, "api", &bad, Some(&key)).unwrap(),
        RolloutOutcome::Blocked {
            reason: BlockReason::Invalid,
            ..
        }
    ));
    assert!(plan.capabilities.iter().any(|c| c.resource == "secrets"
        && c.namespace == "reakt-system"
        && c.names == ["reakt-rollout-key-example"]));
    let mut declassified = secret_fixture();
    declassified["inputs"][0]["sensitive"] = json!(false);
    assert!(parse_and_validate(&declassified.to_string()).is_err());
}

#[test]
fn managed_config_uses_authoritative_projection_only_after_successful_apply() {
    let mut value = fixture();
    value["resources"].as_array_mut().unwrap().push(json!({"id":"owned-config","apiVersion":"v1","kind":"ConfigMap","name":"external","namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"external","namespace":"workload"},"data":{"fixture":"synthetic-only"}},"lifecycle":{"deletionPolicy":"orphan"},"source":source()}));
    assert!(
        parse_and_validate(&value.to_string()).is_err(),
        "observed alias must be rejected"
    );
    value["inputs"][0]["mode"] = json!("managed");
    value["inputs"][0]["managedResourceId"] = json!("owned-config");
    let plan = plan(&value);
    let expected = evaluate_rollout(
        &plan,
        "api",
        &snapshots("config", json!({"fixture":"synthetic-only"})),
        None,
    )
    .unwrap();
    let drift = evaluate_rollout(
        &plan,
        "api",
        &snapshots("config", json!({"fixture":"UNAPPROVED-DRIFT"})),
        None,
    )
    .unwrap();
    assert_eq!(expected, drift);
    let not_applied = BTreeMap::from([(
        "config".into(),
        InputSnapshot::Available {
            data: json!({}),
            authoritative_applied: false,
        },
    )]);
    assert!(matches!(
        evaluate_rollout(&plan, "api", &not_applied, None).unwrap(),
        RolloutOutcome::Blocked {
            reason: BlockReason::ManagedSourceNotApplied,
            ..
        }
    ));
}

#[test]
fn generated_coverage_does_not_claim_unexecuted_integration_evidence() {
    for fixture in [fixture(), secret_fixture()] {
        let plan = plan(&fixture);
        let report = run_semantic_tests(&plan).unwrap();
        assert_eq!(report.ir_digest, plan.ir_digest);
        assert!(!report.production_release_ready);
        assert!(report.test_only);
        assert_eq!(report.results.len(), 16);
        assert_eq!(
            report
                .results
                .iter()
                .filter(|r| r.status == CaseStatus::Passed)
                .count(),
            10
        );
        assert_eq!(
            report
                .results
                .iter()
                .filter(|r| r.status == CaseStatus::Unsupported)
                .count(),
            6
        );
        assert!(
            report
                .results
                .iter()
                .all(|r| r.status != CaseStatus::Failed)
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("c3ludGhldGljLW9ubHk=")
        );
    }
}

fn mixed_modes_fixture() -> Value {
    let mut value = fixture();
    value["resources"].as_array_mut().unwrap().push(json!({
        "id":"managed-config","apiVersion":"v1","kind":"ConfigMap","name":"managed-config","namespace":"workload",
        "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"managed-config","namespace":"workload"},"data":{"fixture":"managed-baseline"}},
        "lifecycle":{"deletionPolicy":"orphan"},"source":source()
    }));
    value["inputs"].as_array_mut().unwrap().extend([
        json!({"id":"secret","apiVersion":"v1","kind":"Secret","name":"external-secret","namespace":"workload","mode":"observed","projection":["data"],"sensitive":true,"missing":"block-dependent-mutations","source":source()}),
        json!({"id":"managed","apiVersion":"v1","kind":"ConfigMap","name":"managed-config","namespace":"workload","mode":"managed","managedResourceId":"managed-config","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":source()}),
    ]);
    value["reactions"][0]["inputs"] = json!(["config", "secret", "managed"]);
    value
}

#[test]
fn generated_semantics_exercise_every_reaction_input_and_separate_real_auth() {
    let value = mixed_modes_fixture();
    let report = run_semantic_tests(&plan(&value)).unwrap();
    for scenario in [
        "projected-change",
        "coalesced-inputs",
        "missing-input",
        "input-returned",
        "unauthorized-input",
        "unavailable-input",
        "invalid-input",
    ] {
        let result = report
            .results
            .iter()
            .find(|result| result.case.scenario == scenario)
            .unwrap();
        assert_eq!(result.status, CaseStatus::Passed);
        assert!(result.detail.contains("[config,secret]"));
        assert_eq!(result.case.input_ids, ["config", "secret"]);
    }
    let credential_case = report
        .results
        .iter()
        .find(|result| result.case.scenario == "credential-read-denial")
        .unwrap();
    assert_eq!(credential_case.case.layer, "api");
    assert_eq!(credential_case.status, CaseStatus::Unsupported);
    assert!(!report.production_release_ready);
}

#[test]
fn generated_cases_preserve_managed_and_observed_applicability_without_fabricated_repairs() {
    let value = mixed_modes_fixture();
    let compiled = plan(&value);
    for (scenario, layer) in [
        ("managed-drift-repair", "workload"),
        ("managed-missing", "api"),
        ("managed-read-denial", "api"),
        ("managed-unavailable", "api"),
    ] {
        let matching: Vec<_> = compiled
            .test_plan
            .iter()
            .filter(|case| case.scenario == scenario)
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].input_ids, ["managed"]);
        assert_eq!(matching[0].layer, layer);
        let report = run_semantic_tests(&compiled).unwrap();
        assert_eq!(
            report
                .results
                .iter()
                .find(|r| r.case.scenario == scenario)
                .unwrap()
                .status,
            CaseStatus::Unsupported
        );
    }
    let initial = compiled
        .test_plan
        .iter()
        .find(|case| case.scenario == "initial-evaluation")
        .unwrap();
    assert_eq!(initial.input_ids, ["config", "managed", "secret"]);
    let mut managed_only = value;
    managed_only["reactions"][0]["inputs"] = json!(["managed"]);
    let only = plan(&managed_only);
    assert_eq!(only.test_plan.len(), 11);
    assert!(
        only.test_plan
            .iter()
            .all(|case| case.input_ids == ["managed"])
    );
    assert!(!only.test_plan.iter().any(|case| matches!(
        case.scenario.as_str(),
        "projected-change"
            | "coalesced-inputs"
            | "missing-input"
            | "credential-read-denial"
            | "restart-and-relist"
    )));
    assert!(
        run_semantic_tests(&only)
            .unwrap()
            .results
            .iter()
            .all(|result| result.status != CaseStatus::Failed)
    );
}

#[test]
fn generated_case_identities_and_applicability_ignore_declaration_order() {
    let mut value = mixed_modes_fixture();
    let mut additional = value["reactions"][0].clone();
    additional["id"] = json!("additional");
    value["reactions"].as_array_mut().unwrap().push(additional);
    let baseline = plan(&value).test_plan;
    value["inputs"].as_array_mut().unwrap().reverse();
    value["resources"].as_array_mut().unwrap().reverse();
    value["reactions"].as_array_mut().unwrap().reverse();
    for reaction in value["reactions"].as_array_mut().unwrap() {
        reaction["inputs"].as_array_mut().unwrap().reverse();
    }
    assert_eq!(baseline, plan(&value).test_plan);
    let unique: std::collections::BTreeSet<_> = baseline
        .iter()
        .map(|case| (&case.reaction_id, &case.scenario))
        .collect();
    assert_eq!(unique.len(), baseline.len());
}

#[test]
fn managed_drift_repair_requires_a_declared_owned_data_key() {
    let mut value = mixed_modes_fixture();
    value["reactions"][0]["inputs"] = json!(["managed"]);
    let resource = value["resources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["id"] == "managed-config")
        .unwrap();
    resource["desired"]["data"] = json!({});
    let compiled = plan(&value);
    assert!(
        !compiled
            .test_plan
            .iter()
            .any(|case| case.scenario == "managed-drift-repair")
    );
    for scenario in [
        "managed-missing",
        "managed-read-denial",
        "managed-unavailable",
    ] {
        assert!(
            compiled
                .test_plan
                .iter()
                .any(|case| case.scenario == scenario && case.input_ids == ["managed"])
        );
    }
}

#[test]
fn forced_resource_requires_exact_policy_acceptance_and_force_aware_evidence() {
    let baseline = plan(&fixture());
    let mut value = fixture();
    value["resources"][0]["force"] = json!(true);
    let forced = plan(&value);
    assert_ne!(baseline.ir_digest, forced.ir_digest);
    assert!(
        forced
            .indirect_risks
            .contains(&"force-apply:Deployment/workload/api".into())
    );
    assert!(validate_ceiling(&forced, &ceiling(&baseline)).is_err());
    validate_ceiling(&forced, &ceiling(&forced)).unwrap();
    assert!(
        forced
            .test_plan
            .iter()
            .any(|case| case.scenario == "forced-field-conflict")
    );
    assert!(
        !forced
            .test_plan
            .iter()
            .any(|case| case.scenario == "field-conflict")
    );
    value["resources"][0]["force"] = json!(false);
    assert_eq!(plan(&value), baseline);
}
