use super::*;

#[test]
fn custom_force_risk_retains_the_kind_and_identity_contract() -> Result<()> {
    for risk in [
        "force-apply:Secret/workload/database".to_string(),
        "force-apply:cluster/workload/database".into(),
        "force-apply:Cluster!/workload/database".into(),
        format!("force-apply:{}/workload/database", "C".repeat(64)),
        "force-apply:Cluster/workload/database/extra".into(),
        "force-apply:Cluster//database".into(),
    ] {
        let mut value = metadata()?;
        value.enforcement.indirect_risks = vec![risk.clone()];
        value.enforcement.runtime_ceiling.accepted_indirect_risks = vec![risk];
        assert!(value.validate().is_err());
    }
    Ok(())
}

#[test]
fn custom_resource_risk_requires_explicit_ceiling_acceptance() -> Result<()> {
    let mut value = metadata()?;
    let risk = "opaque-custom-resource-effects".to_string();
    value.enforcement.indirect_risks = vec![risk.clone()];
    assert!(value.validate().is_err());
    value.enforcement.runtime_ceiling.accepted_indirect_risks = vec![risk];
    value.validate()?;
    value.enforcement.indirect_risks = vec!["unknown-custom-resource-risk".into()];
    value.enforcement.runtime_ceiling.accepted_indirect_risks =
        value.enforcement.indirect_risks.clone();
    assert!(value.validate().is_err());
    Ok(())
}

fn metadata() -> Result<ReviewMetadata> {
    let claim_scope = ClaimScope {
        registry_uid: "11111111-1111-4111-8111-111111111111".into(),
        cluster_binding: "cluster".into(),
        control_namespace: "control".into(),
        workload_namespace: "workload".into(),
    };
    let names = deploy::allocation_names(&claim_scope.registry_uid, 1)
        .map_err(|_| Error::Invalid("test allocation"))?;
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
        kind: "ConfigMap".into(),
        api_resource: None,
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

fn custom_resource() -> ReviewResource {
    let mut value = resource("database");
    value.identity.api_group = "postgresql.cnpg.io".into();
    value.identity.resource = "clusters".into();
    value.api_version = "postgresql.cnpg.io/v1".into();
    value.kind = "Cluster".into();
    value.api_resource = Some(ReviewApiResourceMapping {
        plural: "clusters".into(),
        scope: ReviewApiResourceScope::Namespaced,
        schema_sha256: format!("sha256:{}", "a".repeat(64)),
    });
    value.apply_fields = vec!["/spec/instances".into()];
    value
}

#[test]
fn custom_resource_mapping_is_explicit_and_exact() -> Result<()> {
    let mut value = metadata()?;
    value.resources = vec![custom_resource()];
    value.validate()?;

    let mut invalid = value.clone();
    invalid.resources[0].kind = "Secret".into();
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.resources[0].api_resource = None;
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.resources[0]
        .api_resource
        .as_mut()
        .ok_or(Error::Invalid("test custom mapping"))?
        .plural = "databases".into();
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.resources[0]
        .api_resource
        .as_mut()
        .ok_or(Error::Invalid("test custom mapping"))?
        .plural = "Clusters".into();
    invalid.resources[0].identity.resource = "Clusters".into();
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.resources[0]
        .api_resource
        .as_mut()
        .ok_or(Error::Invalid("test custom mapping"))?
        .schema_sha256 = format!("sha256:{}", "A".repeat(64));
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.resources[0].api_version = "other.example/v1".into();
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.resources[0].identity.api_group = "other.example".into();
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.resources[0].api_version = "v1".into();
    invalid.resources[0].identity.api_group.clear();
    assert!(invalid.validate().is_err());

    invalid = value;
    invalid.resources[0].api_resource = Some(ReviewApiResourceMapping {
        plural: "configmaps".into(),
        scope: ReviewApiResourceScope::Namespaced,
        schema_sha256: format!("sha256:{}", "a".repeat(64)),
    });
    invalid.resources[0].api_version = "v1".into();
    invalid.resources[0].kind = "ConfigMap".into();
    invalid.resources[0].identity.api_group.clear();
    invalid.resources[0].identity.resource = "configmaps".into();
    assert!(invalid.validate().is_err());

    let mut encoded =
        serde_json::to_value(custom_resource()).map_err(|_| Error::Invalid("test encoding"))?;
    encoded["apiResource"]["scope"] = serde_json::json!("Cluster");
    assert!(serde_json::from_value::<ReviewResource>(encoded).is_err());
    let encoded =
        serde_json::to_value(resource("builtin")).map_err(|_| Error::Invalid("test encoding"))?;
    assert!(encoded.get("apiResource").is_none());
    Ok(())
}

#[test]
fn review_resource_catalog_rejects_duplicates_bad_paths_and_source() -> Result<()> {
    let mut value = metadata()?;
    value.resources = vec![resource("a"), resource("b")];
    value.validate()?;
    for path in [
        "/apiVersion",
        "/kind",
        "/metadata/name",
        "/metadata/namespace",
        "/data/~2",
        "data/key",
    ] {
        let mut invalid = value.clone();
        invalid.resources[0].apply_fields = vec![path.into()];
        assert!(invalid.validate().is_err());
    }
    let mut duplicate = value.clone();
    duplicate.resources[1].id.clone_from(&value.resources[0].id);
    assert!(duplicate.validate().is_err());
    duplicate = value.clone();
    duplicate.resources.reverse();
    assert!(duplicate.validate().is_err());
    duplicate = value;
    duplicate.resources[0].source.file = "../secret.tsx".into();
    assert!(duplicate.validate().is_err());
    Ok(())
}

#[test]
fn review_input_and_reaction_links_are_not_inferred_from_names() -> Result<()> {
    let mut value = metadata()?;
    value.resources.push(resource("owned"));
    value.inputs.push(ReviewInput {
        id: "input".into(),
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        name: "owned".into(),
        namespace: "workload".into(),
        mode: ReviewInputMode::Managed,
        managed_resource_id: Some("owned".into()),
        projection: vec!["data".into()],
        sensitive: false,
        missing: "block-dependent-mutations".into(),
        source: ReviewSource {
            file: "source.tsx".into(),
            line: 2,
            column: 1,
        },
    });
    value.validate()?;
    let mut invalid = value.clone();
    invalid.inputs[0].managed_resource_id = Some("missing".into());
    assert!(invalid.validate().is_err());
    invalid = value.clone();
    invalid.inputs[0].mode = ReviewInputMode::Observed;
    invalid.inputs[0].managed_resource_id = None;
    assert!(invalid.validate().is_err());
    value.reactions.push(ReviewReaction {
        id: "reaction".into(),
        op: ReviewPrimitive::Rollout,
        input_ids: vec!["input".into()],
        target_resource_id: "owned".into(),
        effect_fields: vec![REVIEW_ROLLOUT_FIELD.into()],
        source: ReviewSource {
            file: "source.tsx".into(),
            line: 3,
            column: 1,
        },
    });
    assert!(value.validate().is_err()); // A ConfigMap is not a rollout target.
    value.resources[0].api_version = "apps/v1".into();
    value.resources[0].kind = "Deployment".into();
    value.resources[0].identity.api_group = "apps".into();
    value.resources[0].identity.resource = "deployments".into();
    value.resources[0].apply_fields = vec!["/spec/replicas".into()];
    value.inputs[0].mode = ReviewInputMode::Observed;
    value.inputs[0].managed_resource_id = None;
    value.inputs[0].name = "external".into();
    value.validate()?;
    value.reactions[0].input_ids = vec!["missing".into()];
    assert!(value.validate().is_err());
    Ok(())
}

#[test]
fn review_capability_name_contract_and_derived_apis_are_checked() -> Result<()> {
    let mut value = metadata()?;
    let mut capability = ReviewCapability {
        namespace: "workload".into(),
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        verb: "get".into(),
        names: vec![],
    };
    value.runtime_permissions = vec![capability.clone()];
    value.enforcement.runtime_ceiling.capabilities = vec![capability.clone()];
    assert!(value.validate().is_err());
    capability.verb = "create".into();
    capability.names.push("owned".into());
    value.runtime_permissions = vec![capability.clone()];
    value.enforcement.runtime_ceiling.capabilities = vec![capability];
    assert!(value.validate().is_err());
    value = metadata()?;
    value
        .compatibility
        .required_runtime_apis
        .push(ReviewApiRequirement {
            api_group: String::new(),
            resource: "secrets".into(),
            subresource: None,
            verb: "get".into(),
            namespaced: true,
        });
    assert!(value.validate().is_err());
    Ok(())
}

#[test]
fn review_owned_schema_rejects_private_fields_and_bounded_overflow() -> Result<()> {
    let value = metadata()?;
    let mut encoded = serde_json::to_value(&value).map_err(|_| Error::Invalid("test encoding"))?;
    encoded["enforcement"]["runtimeCeiling"]["privateValueHash"] =
        serde_json::json!("private-canary");
    assert!(serde_json::from_value::<ReviewMetadata>(encoded).is_err());
    let mut invalid = value;
    invalid.resources.push(resource("owned"));
    invalid.resources[0].apply_fields = vec!["/data/key".into(); MAX_REVIEW_SOURCE_ELEMENTS + 1];
    assert!(matches!(
        invalid.validate(),
        Err(Error::Invalid("review source element limit"))
    ));
    invalid.resources[0].apply_fields =
        vec![format!("/data/{}", "a".repeat(MAX_REVIEW_METADATA_BYTES))];
    assert!(matches!(
        invalid.validate(),
        Err(Error::Invalid("review metadata size"))
    ));
    Ok(())
}

#[test]
fn review_resource_and_source_reject_private_payload_fields() -> Result<()> {
    let original =
        serde_json::to_value(resource("owned")).map_err(|_| Error::Invalid("test encoding"))?;
    for field in [
        "desired",
        "observed",
        "permittedApply",
        "irDigest",
        "priorManagedFieldsDigest",
    ] {
        let mut encoded = original.clone();
        encoded[field] = serde_json::json!("private-canary");
        assert!(serde_json::from_value::<ReviewResource>(encoded).is_err());
    }
    for field in ["contents", "sha256", "privateValue"] {
        let mut encoded = original.clone();
        encoded["source"][field] = serde_json::json!("private-canary");
        assert!(serde_json::from_value::<ReviewResource>(encoded).is_err());
    }
    Ok(())
}

#[test]
fn review_preserves_surplus_ceiling_names_without_granting_requested_access() -> Result<()> {
    let mut value = metadata()?;
    let mut capability = ReviewCapability {
        namespace: "workload".into(),
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        verb: "create".into(),
        names: vec!["owned".into()],
    };
    value
        .enforcement
        .runtime_ceiling
        .capabilities
        .push(capability.clone());
    capability.verb = "get".into();
    capability.names.clear();
    value
        .enforcement
        .runtime_ceiling
        .capabilities
        .push(capability.clone());
    value.validate()?;
    assert!(value.runtime_permissions.is_empty());
    capability.names.push("owned".into());
    value.runtime_permissions.push(capability);
    assert!(matches!(
        value.validate(),
        Err(Error::Invalid("review ceiling decision"))
    ));
    Ok(())
}

#[test]
fn review_source_paths_preserve_compiler_accepted_control_characters() -> Result<()> {
    let mut value = metadata()?;
    let mut resource = resource("owned");
    resource.source.file = "folder/line\nbreak.tsx".into();
    value.resources.push(resource);
    value.validate()?;
    let encoded =
        compiler_core::canonical_bytes(&value).map_err(|_| Error::Invalid("test encoding"))?;
    let decoded: ReviewMetadata =
        serde_json::from_slice(&encoded).map_err(|_| Error::Invalid("test decoding"))?;
    assert_eq!(decoded, value);
    Ok(())
}

fn handler_input(id: &str) -> ReviewInput {
    ReviewInput {
        id: id.into(),
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        name: id.into(),
        namespace: "workload".into(),
        mode: ReviewInputMode::Observed,
        managed_resource_id: None,
        projection: vec!["data".into()],
        sensitive: false,
        missing: "block-dependent-mutations".into(),
        source: ReviewSource {
            file: "source.tsx".into(),
            line: 1,
            column: 1,
        },
    }
}

#[test]
fn review_metadata_v3_round_trips_handlers_egress_and_outputs() -> Result<()> {
    let mut value = metadata()?;
    value.surface_digest = format!("sha256:{}", "1".repeat(64));
    value.inputs = vec![handler_input("settings")];
    value.outputs = vec![ReviewOutput {
        id: "flags".into(),
        name: "flags".into(),
        namespace: "workload".into(),
    }];
    value.handlers = vec![ReviewHandler {
        id: "sync-flags".into(),
        trigger: "settings".into(),
        inputs: vec!["settings".into()],
        outputs: vec!["flags".into()],
    }];
    value.egress = vec![ReviewEgress {
        host: "api.example.com".into(),
        port: 443,
        handler: "sync-flags".into(),
    }];
    value.validate()?;
    let encoded =
        compiler_core::canonical_bytes(&value).map_err(|_| Error::Invalid("test encoding"))?;
    let decoded: ReviewMetadata =
        serde_json::from_slice(&encoded).map_err(|_| Error::Invalid("test decoding"))?;
    assert_eq!(decoded, value);
    Ok(())
}

#[test]
fn review_handler_and_egress_links_are_not_inferred() -> Result<()> {
    let mut value = metadata()?;
    value.inputs = vec![handler_input("settings")];
    value.outputs = vec![ReviewOutput {
        id: "flags".into(),
        name: "flags".into(),
        namespace: "workload".into(),
    }];
    value.handlers = vec![ReviewHandler {
        id: "sync-flags".into(),
        trigger: "settings".into(),
        inputs: vec!["settings".into()],
        outputs: vec!["flags".into()],
    }];
    value.egress = vec![ReviewEgress {
        host: "api.example.com".into(),
        port: 443,
        handler: "sync-flags".into(),
    }];
    value.validate()?;

    let mut invalid = value.clone();
    invalid.handlers[0].trigger = "unknown".into();
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.handlers[0].outputs = vec!["unknown".into()];
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.egress[0].handler = "unknown".into();
    assert!(invalid.validate().is_err());

    invalid = value.clone();
    invalid.handlers[0].inputs = vec![];
    assert!(invalid.validate().is_err());

    invalid = value;
    invalid.outputs.push(ReviewOutput {
        id: "flags".into(),
        name: "duplicate".into(),
        namespace: "workload".into(),
    });
    assert!(invalid.validate().is_err());
    Ok(())
}

#[test]
fn review_metadata_v2_is_rejected() -> Result<()> {
    let value = metadata()?;
    let encoded = serde_json::to_value(&value).map_err(|_| Error::Invalid("test encoding"))?;

    // A genuine v2 row lacks surfaceDigest, handlers, egress and outputs
    // entirely, and strict `deny_unknown_fields` schema equality rejects it
    // outright, never silently accepting a legacy shape.
    let mut v2 = encoded;
    {
        let map = v2.as_object_mut().ok_or(Error::Invalid("test shape"))?;
        map.insert(
            "schemaVersion".into(),
            serde_json::json!("apsides.dev/review-metadata/v2"),
        );
        for field in ["surfaceDigest", "handlers", "egress", "outputs"] {
            map.remove(field);
        }
    }
    assert!(serde_json::from_value::<ReviewMetadata>(v2).is_err());

    // Even a row that carries every v3 field is rejected by the explicit
    // schema-version check if it still claims to be v2.
    let mut still_versioned_as_v2 = value;
    still_versioned_as_v2.schema_version = "apsides.dev/review-metadata/v2".into();
    assert!(still_versioned_as_v2.validate().is_err());
    Ok(())
}
