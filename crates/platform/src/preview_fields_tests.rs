use super::*;
use crate::Result;
use coordination::*;
use serde_json::json;

fn metadata() -> Result<ReviewMetadataV1> {
    let claim_scope = ClaimScope {
        registry_uid: "11111111-1111-4111-8111-111111111111".into(),
        cluster_binding: "cluster".into(),
        control_namespace: "control".into(),
        workload_namespace: "workload".into(),
    };
    let names = deploy::allocation_names(&claim_scope.registry_uid, 1)
        .map_err(|_| invalid("test allocation"))?;
    let ceiling = ReviewCeiling {
        policy_ref: "policy".into(),
        domain_id: "example".into(),
        cluster_binding: claim_scope.cluster_binding.clone(),
        workload_namespace: claim_scope.workload_namespace.clone(),
        control_namespace: claim_scope.control_namespace.clone(),
        capabilities: vec![],
        accepted_enforcement_gaps: vec![],
        accepted_indirect_risks: vec![],
    };
    Ok(ReviewMetadataV1 {
        schema_version:REVIEW_METADATA_SCHEMA.into(),scope:ReviewScope {claim_scope,domain_id:"example".into(),transition_id:names.transition_id,sequence:1},
        resources:vec![],inputs:vec![],reactions:vec![],runtime_permissions:vec![],cleanup_permissions:vec![],
        deployment_permissions:ReviewDeploymentPermissions {required_resources:vec![],creation_targets:vec![],role_delegation:vec![],
            scope:ReviewDeploymentScope::IssuedDeployerOnly,authorization:ReviewAuthorization::RequiredAccessOnly,
            limitations:vec![ReviewDeploymentLimitation::CreateNamesRequireAdmission,ReviewDeploymentLimitation::GeneratedNamesRequireBroadAccess,
                ReviewDeploymentLimitation::InvokingIdentityBootstrapCredentialsAndDiscoveryExcluded,ReviewDeploymentLimitation::DatabaseCredentialsSeparate]},
        compatibility:compatibility(),
        enforcement:ReviewEnforcement {profile:ReviewEnforcementProfile::ApiBound,gaps:vec![],indirect_risks:vec![],
            runtime_ceiling:ceiling.clone(),cleanup_ceiling:ceiling,decision:ReviewCeilingDecision::ApprovedByPlanner},
    })
}
fn compatibility() -> ReviewCompatibility {
    ReviewCompatibility {
        runtime_budgets: ReviewBudgets {
            max_resources: 3,
            max_inputs: 3,
            max_reactions: 3,
            max_object_bytes: 262_144,
            max_queue_depth: 8,
            max_concurrent_requests: 1,
        },
        schema_library: ReviewSchemaLibrary::K8sOpenapi0280,
        schema_kubernetes_minor: ReviewSchemaMinor::V136,
        required_runtime_apis: vec![],
        required_cleanup_apis: vec![],
        required_deployment_apis: vec![],
        deployment_api_scope: ReviewDeploymentScope::IssuedDeployerOnly,
        kubernetes_version: ReviewKubernetesVersion {
            min_version: "1.34".into(),
            qualified_on: ReviewQualifiedServer::V1343,
        },
        field_validation: ReviewFieldValidation::Strict,
        transition_plan_schema: ReviewTransitionSchema::V4,
        runtime_replicas: 1,
    }
}
fn resource(name: &str) -> ReviewResource {
    ReviewResource {
        id: name.into(),
        identity: ReviewIdentity {
            api_group: String::new(),
            resource: "configmaps".into(),
            namespace: "workload".into(),
            name: name.into(),
        },
        api_version: "v1".into(),
        api_resource: None,
        kind: "ConfigMap".into(),
        apply_fields: vec!["/data/key".into()],
        lifecycle: ReviewLifecycle {
            deletion_policy: ReviewDeletionPolicy::Orphan,
        },
        force: false,
        source: ReviewSource {
            file: "source.tsx".into(),
            line: 1,
            column: 1,
        },
        ownership_plan: ReviewOwnershipPlan::CreateIfAbsent,
    }
}

fn object(data: Value) -> Value {
    let mut value = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"a","namespace":"workload"}});
    value["data"] = data;
    value
}
fn next_generation(old: &ReviewMetadataV1) -> Result<ReviewMetadataV1> {
    let mut next = old.clone();
    next.scope.sequence = 3;
    next.scope.transition_id =
        deploy::allocation_names(&next.scope.claim_scope.registry_uid, 3)?.transition_id;
    Ok(next)
}

#[test]
fn union_reports_prior_only_removal_and_aggregate_arrays() -> Result<()> {
    let mut old = metadata()?;
    old.resources.push(resource("a"));
    old.resources[0].apply_fields = vec!["/data/list".into(), "/data/old".into()];
    let mut next = next_generation(&old)?;
    next.resources[0].apply_fields = vec!["/data/list".into(), "/data/new".into()];
    let before = object(json!({"old":"private-before","list":[{"key":"private-a"}]}));
    let projected = object(json!({"new":"private-after","list":[{"key":"private-b"}]}));
    let report = classify_preview_fields(
        Some(&old),
        &next,
        &next.resources[0].identity,
        PreviewObjects {
            before: Some(&before),
            projected: &projected,
        },
    )?;
    let changes: Vec<_> = report
        .fields
        .iter()
        .map(|row| (row.path.as_str(), row.change))
        .collect();
    assert_eq!(
        changes,
        vec![
            ("/data/list", ProjectionChange::Changed),
            ("/data/new", ProjectionChange::Added),
            ("/data/old", ProjectionChange::Removed)
        ]
    );
    assert_eq!(
        report.undeclared_server_difference,
        CoarseDifference::Unchanged
    );
    assert!(!serde_json::to_string(&report)?.contains("private"));
    Ok(())
}

#[test]
fn overlapping_paths_mask_subtrees_without_positional_interpretation() -> Result<()> {
    let mut old = metadata()?;
    old.resources.push(resource("a"));
    old.resources[0].apply_fields = vec!["/data".into()];
    let mut next = next_generation(&old)?;
    next.resources[0].apply_fields = vec!["/data/0".into()];
    let before = object(json!({}));
    let projected = object(json!({"0":"private","unapproved-map-key":"secret"}));
    let report = classify_preview_fields(
        Some(&old),
        &next,
        &next.resources[0].identity,
        PreviewObjects {
            before: Some(&before),
            projected: &projected,
        },
    )?;
    assert_eq!(report.fields.len(), 2);
    assert_eq!(
        report.undeclared_server_difference,
        CoarseDifference::Unchanged
    );
    assert!(!serde_json::to_string(&report)?.contains("unapproved-map-key"));
    let array = object(json!(["secret"]));
    assert!(
        classify_preview_fields(
            Some(&old),
            &next,
            &next.resources[0].identity,
            PreviewObjects {
                before: Some(&array),
                projected: &projected
            }
        )
        .is_err()
    );
    let mut ignored = Ignore::default();
    ignored.insert(&tokens("/data/key")?);
    assert_eq!(
        equal_except(Some(&json!({})), Some(&json!({"data":{"key":1}})), &ignored),
        CoarseDifference::Unknown
    );
    Ok(())
}

#[test]
fn private_fields_v1_suffixes_and_managers_never_escape() -> Result<()> {
    let mut metadata = metadata()?;
    metadata.resources.push(resource("a"));
    let mut before = object(json!({"key":"a"}));
    let entry = json!({"manager":"private-manager","operation":"Apply","apiVersion":"v1",
        "time":"yesterday","fieldsType":"FieldsV1","fieldsV1":{
            "f:private-map-key":{"k:{\"private-selector\":\"secret\"}":{
                "v:\"private-value\"":{"i:17":{}}}}}});
    before["metadata"]["managedFields"] = json!([entry]);
    let mut after = before.clone();
    after["metadata"]["managedFields"][0]["time"] = json!("today");
    let unchanged = classify_preview_fields(
        None,
        &metadata,
        &metadata.resources[0].identity,
        PreviewObjects {
            before: Some(&before),
            projected: &after,
        },
    )?;
    assert_eq!(
        unchanged.ownership_field_set_evidence,
        FieldSetEvidence::Unchanged
    );
    after["metadata"]["managedFields"][0]["fieldsV1"] = json!({"f:different-private":{}});
    let changed = classify_preview_fields(
        None,
        &metadata,
        &metadata.resources[0].identity,
        PreviewObjects {
            before: Some(&before),
            projected: &after,
        },
    )?;
    assert_eq!(
        changed.ownership_field_set_evidence,
        FieldSetEvidence::SerializedFieldSetChanged
    );
    let serialized = serde_json::to_string(&changed)?;
    for canary in ["private", "secret", "k:", "v:", "i:", "f:"] {
        assert!(!serialized.contains(canary));
    }
    after["metadata"]["managedFields"][0]["fieldsV1"] = json!({"unsupported":{}});
    assert_eq!(
        field_sets(Some(&before), &after, "v1"),
        FieldSetEvidence::Unknown
    );
    Ok(())
}

fn rollout_metadata() -> Result<ReviewMetadataV1> {
    let mut metadata = metadata()?;
    let mut target = resource("a");
    target.api_version = "apps/v1".into();
    target.kind = "Deployment".into();
    target.identity.api_group = "apps".into();
    target.identity.resource = "deployments".into();
    target.apply_fields = vec!["/spec/replicas".into()];
    metadata.inputs.push(ReviewInput {
        id: "input".into(),
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        name: "external".into(),
        namespace: "workload".into(),
        mode: ReviewInputMode::Observed,
        managed_resource_id: None,
        projection: vec!["data".into()],
        sensitive: false,
        missing: "block-dependent-mutations".into(),
        source: target.source.clone(),
    });
    metadata.reactions.push(ReviewReaction {
        id: "rollout".into(),
        op: ReviewPrimitive::Rollout,
        input_ids: vec!["input".into()],
        target_resource_id: target.id.clone(),
        effect_fields: vec![REVIEW_ROLLOUT_FIELD.into()],
        source: target.source.clone(),
    });
    metadata.resources.push(target);
    metadata.validate()?;
    Ok(metadata)
}

#[test]
fn rollout_path_is_fixed_and_conditional_without_value_prediction() -> Result<()> {
    let old = rollout_metadata()?;
    let mut next = next_generation(&old)?;
    let before = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"a","namespace":"workload"},
        "spec":{"replicas":1,"template":{"metadata":{"annotations":{"apsides.dev/revision":"private-old"}}}}});
    let mut projected = before.clone();
    projected["spec"]["template"]["metadata"]["annotations"]["apsides.dev/revision"] =
        json!("private-new");
    let report = classify_preview_fields(
        Some(&old),
        &next,
        &next.resources[0].identity,
        PreviewObjects {
            before: Some(&before),
            projected: &projected,
        },
    )?;
    assert!(report.runtime_conditional);
    assert!(
        report
            .fields
            .iter()
            .any(|field| field.path == REVIEW_ROLLOUT_FIELD && field.runtime_contribution)
    );
    assert!(!serde_json::to_string(&report)?.contains("private"));
    next.reactions.clear();
    projected["spec"]["template"]["metadata"]["annotations"] = json!({});
    let removed = classify_preview_fields(
        Some(&old),
        &next,
        &next.resources[0].identity,
        PreviewObjects {
            before: Some(&before),
            projected: &projected,
        },
    )?;
    assert!(!removed.runtime_conditional);
    assert!(removed.fields.iter().any(
        |field| field.path == REVIEW_ROLLOUT_FIELD && field.change == ProjectionChange::Removed
    ));
    Ok(())
}

#[test]
fn scope_identity_and_object_limits_fail_closed() -> Result<()> {
    let mut old = metadata()?;
    old.resources.push(resource("a"));
    let next = next_generation(&old)?;
    let before = object(json!({"key":"a"}));
    let mut mismatched = before.clone();
    mismatched["metadata"]["name"] = json!("other");
    assert!(
        classify_preview_fields(
            None,
            &next,
            &next.resources[0].identity,
            PreviewObjects {
                before: None,
                projected: &mismatched
            }
        )
        .is_err()
    );
    assert!(
        classify_preview_fields(
            Some(&next),
            &old,
            &old.resources[0].identity,
            PreviewObjects {
                before: None,
                projected: &before
            }
        )
        .is_err()
    );
    let oversized = object(json!({"key":"x".repeat(MAX_PREVIEW_OBJECT_BYTES)}));
    assert!(
        classify_preview_fields(
            None,
            &next,
            &next.resources[0].identity,
            PreviewObjects {
                before: None,
                projected: &oversized
            }
        )
        .is_err()
    );
    let mut deeply_nested = Value::Null;
    for _ in 0..65 {
        deeply_nested = json!({"a":deeply_nested});
    }
    let deep = object(deeply_nested);
    assert!(
        classify_preview_fields(
            None,
            &next,
            &next.resources[0].identity,
            PreviewObjects {
                before: None,
                projected: &deep
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn malformed_field_encodings_and_projection_budget_are_refused() {
    for tree in [
        json!({"f:":{}}),
        json!({"k:not-json":{}}),
        json!({"v:not-json":{}}),
        json!({"i:01":{}}),
        json!({".":{"f:child":{}}}),
    ] {
        assert!(!field_tree(&tree));
    }
    assert!(field_tree(&json!({"k:{\"name\":\"private\"}":{".":{}}})));
    let long = format!("/{}", "x".repeat(MAX_PREVIEW_PATH_BYTES));
    let object = json!({});
    assert!(
        classify_paths(
            &resource("a").identity,
            "v1",
            BTreeSet::from([long.as_str()]),
            false,
            PreviewObjects {
                before: None,
                projected: &object
            }
        )
        .is_err()
    );
    assert!(tokens("/bad~2escape").is_err());
    assert!(tokens(&"/a".repeat(65)).is_err());
}
