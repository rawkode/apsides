use super::*;
use deploy::{OwnershipAction, OwnershipApproval, OwnershipIntent, ResourceIdentity};

fn adoption_compiled() -> Result<CompiledPlan> {
    Ok(compiler_core::parse_and_validate(
        &json!({
            "schemaVersion":"apsides.dev/ir/v2",
            "testOnly":false,
            "domain":{
                "id":"example","namespace":"workload","controlNamespace":"control",
                "clusterBinding":"cluster-a","policyRef":"platform/example",
                "enforcement":"api-bound",
                "minKubernetesVersion":"1.34",
                "budgets":{
                    "maxResources":16,"maxInputs":16,"maxReactions":16,
                    "maxObjectBytes":262_144,"maxQueueDepth":128,
                    "maxConcurrentRequests":4
                }
            },
            "resources":[
                {
                    "id":"adopted","apiVersion":"v1","kind":"ConfigMap",
                    "name":"adopted","namespace":"workload",
                    "desired":{
                        "apiVersion":"v1","kind":"ConfigMap",
                        "metadata":{"name":"adopted","namespace":"workload"},
                        "data":{"value":"approved-private-value"}
                    },
                    "lifecycle":{"deletionPolicy":"orphan"},
                    "source":{"file":"program.tsx","line":4,"column":5}
                },
                {
                    "id":"created","apiVersion":"v1","kind":"ConfigMap",
                    "name":"created","namespace":"workload",
                    "desired":{
                        "apiVersion":"v1","kind":"ConfigMap",
                        "metadata":{"name":"created","namespace":"workload"},
                        "data":{"value":"new-private-value"}
                    },
                    "lifecycle":{"deletionPolicy":"orphan"},
                    "source":{"file":"program.tsx","line":8,"column":5}
                }
            ],
            "inputs":[],"reactions":[],
            "handlers":[],"outputs":[],"provenance":{
                "compiler":"apsides/0.1",
                "inputs":[{
                    "path":"program.tsx",
                    "sha256":"0000000000000000000000000000000000000000000000000000000000000000"
                }]
            }
        })
        .to_string(),
    )?)
}

pub(super) fn adoption_fixture() -> Result<(platform::StoredTransition, deploy::TransitionPlan)> {
    let (registry, base) = registry()?;
    let compiled = adoption_compiled()?;
    let domain = compiled.ir.domain.clone();
    let ceiling = compiler_core::PermissionCeiling {
        policy_ref: domain.policy_ref.clone(),
        domain_id: domain.id.clone(),
        cluster_binding: domain.cluster_binding.clone(),
        workload_namespace: domain.namespace.clone(),
        control_namespace: domain.control_namespace.clone(),
        capabilities: compiled.capabilities.clone(),
        accepted_enforcement_gaps: compiled.enforcement_gaps.clone(),
        accepted_indirect_risks: compiled.indirect_risks.clone(),
    };
    let mut request = base.request;
    request.next.compiled = compiled;
    request.runtime_ceiling = ceiling.clone();
    request.cleanup_ceiling = ceiling;
    let resource = request
        .next
        .compiled
        .ir
        .resources
        .iter()
        .find(|resource| resource.name == "adopted")
        .ok_or("adoption target missing")?;
    let permitted_apply = resource.desired.clone();
    let deletion_policy = resource.lifecycle.deletion_policy;
    let identity = ResourceIdentity {
        api_group: String::new(),
        resource: "configmaps".into(),
        namespace: "workload".into(),
        name: "adopted".into(),
    };
    request.ownership_intents = vec![OwnershipIntent {
        object_name: "approve-adopted".into(),
        object_uid: "approval-object-uid".into(),
        object_resource_version: "17".into(),
        approval: OwnershipApproval {
            schema_version: deploy::OWNERSHIP_INTENT_SCHEMA.into(),
            transition_id: request.transition_id.clone(),
            domain_id: domain.id.clone(),
            generation_id: request.next.generation_id.clone(),
            release_digest: request.next.artifact_digest.clone(),
            cluster_binding: domain.cluster_binding.clone(),
            control_namespace: domain.control_namespace,
            identity,
            observed_uid: "existing-object-uid".into(),
            observed_resource_version: "23".into(),
            prior_managed_fields_digest: compiler_core::digest(&Value::Null)?,
            permitted_apply,
            permitted_runtime_fields: vec![],
            deletion_policy,
            prior_manager_conflicts: vec![],
            action: OwnershipAction::Adopt,
        },
    }];
    let plan = plan_transition(&request)?;
    let mut stored = registry.transition(TRANSITION_1)?.clone();
    stored.next_review_metadata = platform::project_review_metadata(&plan)?.reference()?;
    stored.inputs = frozen_inputs(&plan)?;
    stored.capacity = routing_capacity();
    stored.record = begin(&plan, "job-1")?;
    stored.staged_identity_uids.clear();
    stored.staged_content_digests.clear();
    stored.runtime_binding = None;
    stored.staged_trust_policy_digest = None;
    stored.staged_provenance_digest = None;
    stored.request_drain_qualification = None;
    stored.proxy_identity = None;
    stored.upgrade_drain = None;
    stored.previous_writer_checkpoint = None;
    Ok((stored, plan))
}

fn resource_row<'a>(review: &'a Value, name: &str) -> Result<&'a Value> {
    review
        .get("resources")
        .and_then(Value::as_array)
        .and_then(|resources| {
            resources
                .iter()
                .find(|resource| resource.get("name").and_then(Value::as_str) == Some(name))
        })
        .ok_or_else(|| format!("review resource {name} missing").into())
}

#[test]
fn authenticated_review_links_added_declaration_to_planned_adoption() -> Result<()> {
    let (stored, plan) = adoption_fixture()?;
    let review = crate::review::transition_review_fixture(&stored, &plan, TRANSITION_1, None)?;
    assert_eq!(
        review.get("schemaVersion").and_then(Value::as_str),
        Some("apsides.dev/deployment-review/v5")
    );
    assert_eq!(
        review.get("actionSemantics").and_then(Value::as_str),
        Some("planned")
    );
    let adopted = resource_row(&review, "adopted")?;
    assert_eq!(
        adopted.get("declarationChange").and_then(Value::as_str),
        Some("added")
    );
    assert_eq!(
        adopted
            .pointer("/ownershipChange/change")
            .and_then(Value::as_str),
        Some("adopted")
    );
    assert_eq!(
        adopted
            .pointer("/ownershipChange/status")
            .and_then(Value::as_str),
        Some("adoption-approved")
    );
    assert_eq!(
        adopted
            .pointer("/ownershipChange/execution")
            .and_then(Value::as_str),
        Some("not-evaluated")
    );
    assert_eq!(
        adopted
            .pointer("/ownershipChange/intentObjectName")
            .and_then(Value::as_str),
        Some("approve-adopted")
    );
    let created = resource_row(&review, "created")?;
    assert_eq!(
        created.get("declarationChange").and_then(Value::as_str),
        Some("added")
    );
    assert_eq!(
        created
            .pointer("/ownershipChange/change")
            .and_then(Value::as_str),
        Some("new")
    );
    assert_eq!(
        created
            .pointer("/ownershipChange/status")
            .and_then(Value::as_str),
        Some("create-if-absent")
    );
    assert_eq!(
        created
            .pointer("/ownershipChange/execution")
            .and_then(Value::as_str),
        Some("not-evaluated")
    );

    let intent = review
        .get("ownershipIntents")
        .and_then(Value::as_array)
        .and_then(|intents| intents.first())
        .ok_or("review ownership intent missing")?;
    assert_eq!(
        intent.get("objectName").and_then(Value::as_str),
        Some("approve-adopted")
    );
    assert_eq!(
        intent.get("objectUid").and_then(Value::as_str),
        Some("approval-object-uid")
    );
    assert_eq!(
        intent.get("objectResourceVersion").and_then(Value::as_str),
        Some("17")
    );
    assert_eq!(
        intent.get("observedUid").and_then(Value::as_str),
        Some("existing-object-uid")
    );
    assert_eq!(
        intent
            .get("observedResourceVersion")
            .and_then(Value::as_str),
        Some("23")
    );
    let encoded = serde_json::to_string(&review)?;
    assert!(!encoded.contains("approved-private-value"));
    assert!(!encoded.contains("new-private-value"));
    assert!(!encoded.contains("priorManagedFieldsDigest"));
    assert!(!encoded.contains("adoption-completed"));
    Ok(())
}

#[test]
fn forged_adoption_plan_never_renders_as_approved() -> Result<()> {
    let (stored, plan) = adoption_fixture()?;
    let mut forged = plan.clone();
    forged
        .request
        .ownership_intents
        .first_mut()
        .ok_or("ownership intent missing")?
        .object_uid = "forged-approval-uid".into();
    assert!(
        crate::review::transition_review_fixture(&stored, &forged, TRANSITION_1, None).is_err()
    );

    let mut wrong_commitment = stored;
    wrong_commitment.inputs.plan_digest = OTHER.into();
    assert!(
        crate::review::transition_review_fixture(&wrong_commitment, &plan, TRANSITION_1, None)
            .is_err()
    );
    Ok(())
}
