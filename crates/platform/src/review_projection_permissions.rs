use crate::{Result, invalid};
use coordination::{
    ReviewApiRequirement, ReviewAuthorization, ReviewBudgets, ReviewCapability, ReviewCeiling,
    ReviewCeilingDecision, ReviewCompatibility, ReviewCreationTarget, ReviewDeploymentLimitation,
    ReviewDeploymentPermissions, ReviewDeploymentScope, ReviewEnforcement,
    ReviewEnforcementProfile, ReviewFieldValidation, ReviewKubernetesVersion,
    ReviewQualifiedServer, ReviewResourceAccess, ReviewRoleBinding, ReviewRoleCreation,
    ReviewRoleDelegation, ReviewSchemaLibrary, ReviewSchemaMinor, ReviewTransitionSchema,
};
use deploy::TransitionPlan;

fn ordered<T: Ord>(mut values: Vec<T>) -> Vec<T> {
    values.sort();
    values.dedup();
    values
}

pub(super) fn capabilities(values: &[compiler_core::Capability]) -> Vec<ReviewCapability> {
    ordered(
        values
            .iter()
            .map(|c| ReviewCapability {
                namespace: c.namespace.clone(),
                api_group: c.api_group.clone(),
                resource: c.resource.clone(),
                subresource: c.subresource.clone(),
                verb: c.verb.clone(),
                names: ordered(c.names.clone()),
            })
            .collect(),
    )
}
fn ceiling(value: &compiler_core::PermissionCeiling) -> ReviewCeiling {
    ReviewCeiling {
        policy_ref: value.policy_ref.clone(),
        domain_id: value.domain_id.clone(),
        cluster_binding: value.cluster_binding.clone(),
        workload_namespace: value.workload_namespace.clone(),
        control_namespace: value.control_namespace.clone(),
        capabilities: capabilities(&value.capabilities),
        accepted_enforcement_gaps: ordered(value.accepted_enforcement_gaps.clone()),
        accepted_indirect_risks: ordered(value.accepted_indirect_risks.clone()),
    }
}
pub(super) fn enforcement(plan: &TransitionPlan) -> Result<ReviewEnforcement> {
    let compiled = &plan.request.next.compiled;
    if compiled.ir.domain.enforcement != "api-bound" {
        return Err(invalid("review enforcement profile is unsupported"));
    }
    Ok(ReviewEnforcement {
        profile: ReviewEnforcementProfile::ApiBound,
        gaps: ordered(compiled.enforcement_gaps.clone()),
        indirect_risks: ordered(compiled.indirect_risks.clone()),
        runtime_ceiling: ceiling(&plan.request.runtime_ceiling),
        cleanup_ceiling: ceiling(&plan.request.cleanup_ceiling),
        decision: ReviewCeilingDecision::ApprovedByPlanner,
    })
}

pub(super) fn deployment(
    value: &crate::DeploymentPermissions,
) -> Result<ReviewDeploymentPermissions> {
    if value.schema_version != "apsides.dev/deployment-permissions/v1"
        || value.authorization != "required-access-only; effective RBAC and admission not evaluated"
    {
        return Err(invalid(
            "review deployment permission semantics are unsupported",
        ));
    }
    let required_resources = ordered(
        value
            .required_resources
            .iter()
            .map(|r| ReviewResourceAccess {
                namespace: r.namespace.clone(),
                api_group: r.api_group.clone(),
                resource: r.resource.clone(),
                verb: r.verb.clone(),
                resource_names: ordered(r.resource_names.clone()),
            })
            .collect(),
    );
    let creation_targets = ordered(
        value
            .creation_targets
            .iter()
            .map(|r| ReviewCreationTarget {
                namespace: r.namespace.clone(),
                api_group: r.api_group.clone(),
                resource: r.resource.clone(),
                name: r.name.clone(),
            })
            .collect(),
    );
    let mut role_delegation = Vec::with_capacity(value.role_delegation.len());
    for role in &value.role_delegation {
        if role.create_requirement
            != "hold all granted capabilities in this scope OR be authorized to escalate the Role"
            || role.bind_requirement
                != "hold all granted capabilities in this scope OR be authorized to bind the Role"
        {
            return Err(invalid("review role delegation semantics are unsupported"));
        }
        role_delegation.push(ReviewRoleDelegation {
            namespace: role.namespace.clone(),
            role_name: role.role_name.clone(),
            granted_capabilities: capabilities(&role.granted_capabilities),
            create_requirement: ReviewRoleCreation::HeldCapabilitiesOrEscalate,
            bind_requirement: ReviewRoleBinding::HeldCapabilitiesOrBind,
        });
    }
    role_delegation.sort_by(|a, b| (&a.namespace, &a.role_name).cmp(&(&b.namespace, &b.role_name)));
    let limitations = value.limitations.iter().map(|text| match *text {
        "top-level create cannot be restricted by resourceNames; creationTargets require admission enforcement" => Ok(ReviewDeploymentLimitation::CreateNamesRequireAdmission),
        "Pod list and generated Pod/ReplicaSet/Node names require broader access; executor UID and ownership checks do not narrow RBAC" => Ok(ReviewDeploymentLimitation::GeneratedNamesRequireBroadAccess),
        "bootstrap identity discovery, credential minting, and API discovery use the invoking identity and are outside this issued-deployer report" => Ok(ReviewDeploymentLimitation::InvokingIdentityBootstrapCredentialsAndDiscoveryExcluded),
        "inventory backend read and conditional publication require separate database credentials" => Ok(ReviewDeploymentLimitation::DatabaseCredentialsSeparate),
        _ => Err(invalid("review deployment limitation is unsupported")),
    }).collect::<Result<Vec<_>>>()?;
    Ok(ReviewDeploymentPermissions {
        required_resources,
        creation_targets,
        role_delegation,
        scope: ReviewDeploymentScope::IssuedDeployerOnly,
        authorization: ReviewAuthorization::RequiredAccessOnly,
        limitations: ordered(limitations),
    })
}

pub(super) fn compatibility(
    budgets: &compiler_core::Budgets,
    min_kubernetes_version: &str,
    runtime: &[ReviewCapability],
    cleanup: &[ReviewCapability],
    deployment: &ReviewDeploymentPermissions,
) -> ReviewCompatibility {
    ReviewCompatibility {
        runtime_budgets: ReviewBudgets {
            max_resources: budgets.max_resources,
            max_inputs: budgets.max_inputs,
            max_reactions: budgets.max_reactions,
            max_object_bytes: budgets.max_object_bytes,
            max_queue_depth: budgets.max_queue_depth,
            max_concurrent_requests: budgets.max_concurrent_requests,
        },
        schema_library: ReviewSchemaLibrary::K8sOpenapi0280,
        schema_kubernetes_minor: ReviewSchemaMinor::V136,
        required_runtime_apis: api_requirements(runtime),
        required_cleanup_apis: api_requirements(cleanup),
        required_deployment_apis: ordered(
            deployment
                .required_resources
                .iter()
                .map(|r| {
                    let (resource, subresource) = r
                        .resource
                        .split_once('/')
                        .map_or((r.resource.as_str(), None), |(r, s)| {
                            (r, Some(s.to_owned()))
                        });
                    ReviewApiRequirement {
                        api_group: r.api_group.clone(),
                        resource: resource.into(),
                        subresource,
                        verb: r.verb.clone(),
                        namespaced: r.namespace.is_some(),
                    }
                })
                .collect(),
        ),
        deployment_api_scope: ReviewDeploymentScope::IssuedDeployerOnly,
        kubernetes_version: ReviewKubernetesVersion {
            min_version: min_kubernetes_version.into(),
            qualified_on: ReviewQualifiedServer::V1343,
        },
        field_validation: ReviewFieldValidation::Strict,
        transition_plan_schema: ReviewTransitionSchema::V4,
        runtime_replicas: 1,
    }
}
fn api_requirements(capabilities: &[ReviewCapability]) -> Vec<ReviewApiRequirement> {
    ordered(
        capabilities
            .iter()
            .map(|c| ReviewApiRequirement {
                api_group: c.api_group.clone(),
                resource: c.resource.clone(),
                subresource: c.subresource.clone(),
                verb: c.verb.clone(),
                namespaced: true,
            })
            .collect(),
    )
}
