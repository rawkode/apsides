//! Exact permission and compatibility deltas from authenticated review metadata.
use super::declarations::Changes;
use coordination::{
    ReviewCapability, ReviewCreationTarget, ReviewMetadata, ReviewResourceAccess,
    ReviewRoleDelegation,
};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CleanupPermissions<'a> {
    pub requested: &'a [ReviewCapability],
    pub ceiling: &'a coordination::ReviewCeiling,
    pub decision: &'static str,
    pub changes: Changes<'a, ReviewCapability>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeploymentPermissions<'a> {
    pub identity: &'static str,
    pub source_may_change_authority: bool,
    pub required: &'a coordination::ReviewDeploymentPermissions,
    pub changes: Changes<'a, coordination::ReviewDeploymentPermissions>,
    pub required_resources: Changes<'a, ReviewResourceAccess>,
    pub creation_targets: Changes<'a, ReviewCreationTarget>,
    pub role_delegation: Changes<'a, ReviewRoleDelegation>,
    pub effective_authority: &'static str,
}
#[derive(Serialize)]
pub(super) struct Permissions<'a> {
    pub runtime: Changes<'a, ReviewCapability>,
    pub cleanup: CleanupPermissions<'a>,
    pub deployment: DeploymentPermissions<'a>,
}
impl<'a> Permissions<'a> {
    pub(super) fn new(
        before: Option<&'a ReviewMetadata>,
        next: &'a ReviewMetadata,
    ) -> crate::Result<Self> {
        let prior_runtime = before.map(|value| value.runtime_permissions.as_slice());
        let prior_cleanup = before.map(|value| value.cleanup_permissions.as_slice());
        let prior_deployment = before.map(|value| &value.deployment_permissions);
        Ok(Self {
            runtime: Changes::keyed(prior_runtime, &next.runtime_permissions, Clone::clone)?,
            cleanup: CleanupPermissions {
                requested: &next.cleanup_permissions,
                ceiling: &next.enforcement.cleanup_ceiling,
                decision: "approved-by-transition-planner",
                changes: Changes::keyed(prior_cleanup, &next.cleanup_permissions, Clone::clone)?,
            },
            deployment: DeploymentPermissions {
                identity: "platform-provisioned-short-lived-deployer",
                source_may_change_authority: false,
                required: &next.deployment_permissions,
                changes: Changes::keyed(
                    prior_deployment.map(std::slice::from_ref),
                    std::slice::from_ref(&next.deployment_permissions),
                    |_| "deployment-permissions",
                )?,
                required_resources: Changes::keyed(
                    prior_deployment.map(|value| value.required_resources.as_slice()),
                    &next.deployment_permissions.required_resources,
                    Clone::clone,
                )?,
                creation_targets: Changes::keyed(
                    prior_deployment.map(|value| value.creation_targets.as_slice()),
                    &next.deployment_permissions.creation_targets,
                    Clone::clone,
                )?,
                role_delegation: Changes::keyed(
                    prior_deployment.map(|value| value.role_delegation.as_slice()),
                    &next.deployment_permissions.role_delegation,
                    |value| (value.namespace.clone(), value.role_name.clone()),
                )?,
                effective_authority: "not-evaluated; external-platform-rbac-and-admission-boundary",
            },
        })
    }
}
