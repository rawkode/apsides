//! Compatibility is a requirement projection, not a copy of compiler review documents.
use super::declarations::Changes;
use compiler_core::{Capability, PermissionCeiling};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApiRequirement<'a> {
    api_group: &'a str,
    resource: &'a str,
    subresource: Option<&'a str>,
    verb: &'a str,
    namespaced: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Compatibility<'a> {
    schema_library: &'static str,
    schema_kubernetes_minor: &'static str,
    required_runtime_apis: BTreeSet<ApiRequirement<'a>>,
    required_cleanup_apis: BTreeSet<ApiRequirement<'a>>,
    required_deployment_apis: BTreeSet<ApiRequirement<'a>>,
    deployment_api_scope: &'static str,
    qualified_create_precondition_servers: [&'static str; 1],
    field_validation: &'static str,
    transition_plan_schema: &'static str,
    runtime_replicas: u8,
}
impl<'a> Compatibility<'a> {
    pub(super) fn new(
        plan: &'a deploy::TransitionPlan,
        deployment: &'a platform::DeploymentPermissions,
    ) -> Self {
        Self {
            deployment_api_scope: "issued-deployer Kubernetes resource APIs; invoking-identity bootstrap, credential minting, and API discovery excluded",
            schema_library: "k8s-openapi/0.28.0",
            schema_kubernetes_minor: "1.36",
            required_runtime_apis: capability_apis(&plan.request.next.compiled.capabilities),
            required_cleanup_apis: capability_apis(&plan.cleanup_capabilities),
            required_deployment_apis: deployment
                .required_resources
                .iter()
                .map(|access| {
                    let (resource, subresource) = access.resource.split_once('/').map_or(
                        (access.resource.as_str(), None),
                        |(resource, subresource)| (resource, Some(subresource)),
                    );
                    ApiRequirement {
                        api_group: &access.api_group,
                        resource,
                        subresource,
                        verb: &access.verb,
                        namespaced: access.namespace.is_some(),
                    }
                })
                .collect(),
            qualified_create_precondition_servers: ["v1.34.3"],
            field_validation: "Strict",
            transition_plan_schema: "reakt.dev/transition-plan/v4",
            runtime_replicas: 1,
        }
    }
}

fn capability_apis(capabilities: &[Capability]) -> BTreeSet<ApiRequirement<'_>> {
    capabilities
        .iter()
        .map(|c| ApiRequirement {
            api_group: &c.api_group,
            resource: &c.resource,
            subresource: c.subresource.as_deref(),
            verb: &c.verb,
            namespaced: true,
        })
        .collect()
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CleanupPermissions<'a> {
    pub requested: &'a [Capability],
    pub ceiling: &'a PermissionCeiling,
    pub decision: &'static str,
    pub changes: Changes<'a, Capability>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeploymentPermissions<'a> {
    pub identity: &'static str,
    pub source_may_change_authority: bool,
    pub required: &'a platform::DeploymentPermissions,
    pub required_resources: Changes<'a, platform::ResourceAccess>,
    pub creation_targets: Changes<'a, platform::CreationTarget>,
    pub role_delegation: Changes<'a, platform::RoleDelegation>,
    pub effective_authority: &'static str,
}
#[derive(Serialize)]
pub(super) struct Permissions<'a> {
    pub runtime: Changes<'a, Capability>,
    pub cleanup: CleanupPermissions<'a>,
    pub deployment: DeploymentPermissions<'a>,
}
impl<'a> Permissions<'a> {
    pub(super) fn new(
        plan: &'a deploy::TransitionPlan,
        required: &'a platform::DeploymentPermissions,
        initial: bool,
    ) -> Self {
        Self {
            runtime: Changes::new(&plan.request.next.compiled.capabilities, initial),
            cleanup: CleanupPermissions {
                requested: &plan.cleanup_capabilities,
                ceiling: &plan.request.cleanup_ceiling,
                decision: "approved-by-transition-planner",
                changes: Changes::new(&plan.cleanup_capabilities, initial),
            },
            deployment: DeploymentPermissions {
                identity: "platform-provisioned-short-lived-deployer",
                source_may_change_authority: false,
                required,
                required_resources: Changes::new(&required.required_resources, initial),
                creation_targets: Changes::new(&required.creation_targets, initial),
                role_delegation: Changes::new(&required.role_delegation, initial),
                effective_authority: "not-evaluated; external-platform-rbac-and-admission-boundary",
            },
        }
    }
}
