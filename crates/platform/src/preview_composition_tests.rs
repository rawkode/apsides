use super::*;
use crate::Result;
use coordination::*;
use serde_json::json;

fn metadata() -> Result<ReviewMetadata> {
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
    Ok(ReviewMetadata {
        schema_version:REVIEW_METADATA_SCHEMA.into(),scope:ReviewScope {claim_scope,domain_id:"example".into(),transition_id:names.transition_id,sequence:1},
        surface_digest:format!("sha256:{}", "0".repeat(64)),
        resources:vec![],inputs:vec![],reactions:vec![],handlers:vec![],egress:vec![],outputs:vec![],runtime_permissions:vec![],cleanup_permissions:vec![],
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

fn rollout_metadata() -> Result<ReviewMetadata> {
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
fn desired() -> Value {
    json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"a","namespace":"workload"},
        "spec":{"replicas":1,"template":{"metadata":{"annotations":{"declared":"keep"}},
        "spec":{"containers":[{"name":"app","image":"example"}]}}}})
}
fn live() -> Value {
    let mut value = desired();
    value["metadata"]["uid"] = json!("uid-a");
    value["metadata"]["managedFields"] = json!([{
        "manager":"apsides-example","operation":"Apply","apiVersion":"apps/v1","fieldsType":"FieldsV1",
        "fieldsV1":{"f:spec":{"f:template":{"f:metadata":{"f:annotations":{"f:apsides.dev/revision":{}}}}}}
    }]);
    value["spec"]["template"]["metadata"]["annotations"][REVISION_ANNOTATION] =
        json!(format!("hmac-sha256:private-key-id:{}", "a".repeat(64)));
    value
}
fn compose(metadata: &ReviewMetadata, before: Option<&Value>) -> Result<PreviewComposition> {
    compose_preview(
        metadata,
        &metadata.resources[0].identity,
        &desired(),
        before.map(|object| PreviewLiveObject {
            object,
            expected_uid: "uid-a",
        }),
    )
}

#[test]
fn owned_contribution_is_private_preserved_and_admission_checked() -> Result<()> {
    let metadata = rollout_metadata()?;
    let before = live();
    let composition = compose(&metadata, Some(&before))?;
    assert!(composition.runtime_conditional);
    assert_eq!(annotation(composition.payload())?, annotation(&before)?);
    assert_eq!(
        composition.payload()["spec"]["template"]["metadata"]["annotations"]["declared"],
        json!("keep")
    );
    assert_eq!(
        composition.payload()["spec"]["template"]["spec"],
        desired()["spec"]["template"]["spec"]
    );
    assert!(
        composition
            .payload()
            .get("metadata")
            .and_then(|v| v.get("managedFields"))
            .is_none()
    );
    composition.verify_projected(&before)?;
    let mut changed = before;
    changed["spec"]["template"]["metadata"]["annotations"][REVISION_ANNOTATION] =
        json!("private-admission-canary");
    let error = composition
        .verify_projected(&changed)
        .err()
        .ok_or_else(|| invalid("test expected refusal"))?;
    assert!(!error.to_string().contains("private"));
    assert!(!error.to_string().contains("canary"));
    Ok(())
}

#[test]
fn absent_contribution_stays_absent_and_removal_has_no_prediction() -> Result<()> {
    let mut metadata = rollout_metadata()?;
    let absent = compose(&metadata, None)?;
    assert!(absent.runtime_conditional);
    assert!(annotation(absent.payload())?.is_none());
    absent.verify_projected(&desired())?;
    assert!(absent.verify_projected(&live()).is_err());
    let mut before = live();
    before["spec"]["template"]["metadata"]["annotations"] = json!({"declared":"keep"});
    let retained_absence = compose(&metadata, Some(&before))?;
    assert!(annotation(retained_absence.payload())?.is_none());
    metadata.reactions.clear();
    let removed = compose(&metadata, Some(&live()))?;
    assert!(!removed.runtime_conditional);
    assert!(annotation(removed.payload())?.is_none());
    // The dry-run API decides whether omission removes a field; composition does
    // not assert that removal or adoption already happened.
    removed.verify_projected(&live())?;
    Ok(())
}

#[test]
fn foreign_missing_ambiguous_and_update_ownership_reject_without_values() -> Result<()> {
    let metadata = rollout_metadata()?;
    for mutation in [
        "foreign",
        "missing",
        "duplicate",
        "update",
        "wrong-api",
        "status",
        "wrong-path",
    ] {
        let mut before = live();
        match mutation {
            "foreign" => {
                before["metadata"]["managedFields"][0]["manager"] =
                    json!("foreign-private-manager");
            }
            "missing" => before["metadata"]["managedFields"] = json!([]),
            "duplicate" => {
                let entry = before["metadata"]["managedFields"][0].clone();
                before["metadata"]["managedFields"] = json!([entry.clone(), entry]);
            }
            "update" => before["metadata"]["managedFields"][0]["operation"] = json!("Update"),
            "wrong-api" => before["metadata"]["managedFields"][0]["apiVersion"] = json!("apps/v2"),
            "status" => before["metadata"]["managedFields"][0]["subresource"] = json!("status"),
            _ => {
                before["metadata"]["managedFields"][0]["fieldsV1"] =
                    json!({"f:spec":{"f:replicas":{}}});
            }
        }
        let error = compose(&metadata, Some(&before))
            .err()
            .ok_or_else(|| invalid("test expected ownership refusal"))?;
        assert!(!error.to_string().contains("private"));
    }
    let mut before = live();
    let foreign = json!({"manager":"foreign","operation":"Apply","apiVersion":"apps/v1","fieldsType":"FieldsV1",
        "fieldsV1":{"f:spec":{}}});
    let own = before["metadata"]["managedFields"][0].clone();
    before["metadata"]["managedFields"] = json!([own, foreign]);
    assert!(compose(&metadata, Some(&before)).is_err());
    Ok(())
}

#[test]
fn malformed_revision_identity_and_author_contribution_are_rejected() -> Result<()> {
    let metadata = rollout_metadata()?;
    for value in [
        json!(true),
        json!("private-invalid"),
        json!(format!("sha256:{}", "A".repeat(64))),
        json!(format!("hmac-sha256:bad/key:{}", "a".repeat(64))),
    ] {
        let mut before = live();
        before["spec"]["template"]["metadata"]["annotations"][REVISION_ANNOTATION] = value;
        assert!(compose(&metadata, Some(&before)).is_err());
    }
    for field in ["name", "namespace", "uid"] {
        let mut before = live();
        before["metadata"][field] = json!("wrong");
        assert!(compose(&metadata, Some(&before)).is_err());
    }
    let mut wrong = desired();
    wrong["kind"] = json!("StatefulSet");
    assert!(compose_preview(&metadata, &metadata.resources[0].identity, &wrong, None).is_err());
    let mut author = desired();
    author["spec"]["template"]["metadata"]["annotations"][REVISION_ANNOTATION] = json!("forbidden");
    assert!(compose_preview(&metadata, &metadata.resources[0].identity, &author, None).is_err());
    let mut huge = desired();
    huge["spec"]["private"] =
        json!("x".repeat(metadata.compatibility.runtime_budgets.max_object_bytes));
    assert!(compose_preview(&metadata, &metadata.resources[0].identity, &huge, None).is_err());
    Ok(())
}
