//! Supplemental Kubernetes access used only by authenticated live preview.
//!
//! This is not the complete deployment permission report. Registry, policy,
//! trust, ownership, immutable-store and credential-bootstrap access remains in
//! the existing deployment contract.
use crate::{CreationTarget, ResourceAccess, Result, invalid};
use coordination::ReviewIdentity;
use deploy::{TransitionPlan, plan_transition};
use serde::Serialize;
use std::collections::BTreeSet;

/// One exact managed target and the fixed requests issued for its live preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewTargetRequest {
    pub identity: ReviewIdentity,
    pub list_method: &'static str,
    pub list_field_selector: String,
    pub list_limit: u32,
    pub patch_method: &'static str,
    pub patch_dry_run: &'static str,
    pub patch_field_validation: &'static str,
}

/// Fixed qualifications on the supplemental preview permission projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewPermissionLimitation {
    SupplementalToDeploymentAuthority,
    LiveGetAndListRequests,
    PatchDryRunOnlyNoLiveWriteFallback,
    CreateAuthorizationMayBeNamespaceWide,
    IssuedDeployerAdmissionOnly,
    RuntimeAndCleanupAuthorityExcluded,
}

/// Additional API access used by the fixed live-preview mechanism.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewPermissions {
    pub schema_version: &'static str,
    pub scope: &'static str,
    pub non_resource_gets: Vec<&'static str>,
    pub required_resources: Vec<ResourceAccess>,
    pub target_requests: Vec<PreviewTargetRequest>,
    pub potential_creation_targets: Vec<CreationTarget>,
    pub authorization: &'static str,
    pub limitations: Vec<PreviewPermissionLimitation>,
}

/// Project only the extra requests made by authenticated live preview.
///
/// Named LIST authorization is restricted with `resourceNames` because the
/// request always supplies the matching `metadata.name` field selector. CREATE
/// authorization cannot be name-restricted, so exact intended names are
/// reported separately. This report grants no credential or execution authority.
/// # Errors
/// Rejects noncanonical plans or unsupported declaration metadata.
pub fn preview_permissions(plan: &TransitionPlan) -> Result<PreviewPermissions> {
    if plan_transition(&plan.request)? != *plan {
        return Err(invalid(
            "preview permissions require a canonical transition plan",
        ));
    }
    let metadata = crate::project_review_metadata(plan)?;
    let domain = &plan.request.next.compiled.ir.domain;
    let mut required = BTreeSet::from([ResourceAccess {
        namespace: Some(domain.control_namespace.clone()),
        api_group: String::new(),
        resource: "configmaps".into(),
        verb: "get".into(),
        resource_names: vec![format!("reakt-deploy-identity-{}", domain.id)],
    }]);
    let mut creates = BTreeSet::new();
    let mut targets = Vec::with_capacity(metadata.resources.len());
    for resource in metadata.resources {
        let identity = resource.identity;
        for verb in ["list", "patch"] {
            required.insert(ResourceAccess {
                namespace: Some(identity.namespace.clone()),
                api_group: identity.api_group.clone(),
                resource: identity.resource.clone(),
                verb: verb.into(),
                resource_names: vec![identity.name.clone()],
            });
        }
        if allows_create(&plan.request.next.compiled.capabilities, &identity) {
            required.insert(ResourceAccess {
                namespace: Some(identity.namespace.clone()),
                api_group: identity.api_group.clone(),
                resource: identity.resource.clone(),
                verb: "create".into(),
                resource_names: Vec::new(),
            });
            creates.insert(CreationTarget {
                namespace: identity.namespace.clone(),
                api_group: identity.api_group.clone(),
                resource: identity.resource.clone(),
                name: identity.name.clone(),
            });
        }
        targets.push(PreviewTargetRequest {
            list_field_selector: format!("metadata.name={}", identity.name),
            identity,
            list_method: "LIST",
            list_limit: 2,
            patch_method: "PATCH",
            patch_dry_run: "All",
            patch_field_validation: "Strict",
        });
    }
    targets.sort_by(|left, right| left.identity.cmp(&right.identity));
    Ok(PreviewPermissions {
        schema_version: "reakt.dev/preview-permissions/v1",
        scope: "supplemental-live-preview-only",
        non_resource_gets: vec!["/version"],
        required_resources: required.into_iter().collect(),
        target_requests: targets,
        potential_creation_targets: creates.into_iter().collect(),
        authorization: "additional-required-access-only; effective RBAC and admission not evaluated",
        limitations: vec![
            PreviewPermissionLimitation::SupplementalToDeploymentAuthority,
            PreviewPermissionLimitation::LiveGetAndListRequests,
            PreviewPermissionLimitation::PatchDryRunOnlyNoLiveWriteFallback,
            PreviewPermissionLimitation::CreateAuthorizationMayBeNamespaceWide,
            PreviewPermissionLimitation::IssuedDeployerAdmissionOnly,
            PreviewPermissionLimitation::RuntimeAndCleanupAuthorityExcluded,
        ],
    })
}

fn allows_create(capabilities: &[compiler_core::Capability], identity: &ReviewIdentity) -> bool {
    capabilities.iter().any(|capability| {
        capability.verb == "create"
            && capability.namespace == identity.namespace
            && capability.api_group == identity.api_group
            && capability.resource == identity.resource
            && capability.subresource.is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn plan(private: &str, observed_secret: bool, resources: bool) -> TestResult<TransitionPlan> {
        let (mut request, _, _, _) = crate::tests::preview_fresh_fixture()?;
        let mut ir = serde_json::to_value(&request.next.compiled.ir)?;
        if resources {
            *ir.pointer_mut("/resources/0/desired/data/value")
                .ok_or("fixture private value missing")? = json!(private);
        } else {
            *ir.get_mut("resources").ok_or("fixture resources missing")? = json!([]);
        }
        if observed_secret {
            *ir.get_mut("inputs").ok_or("fixture inputs missing")? = json!([{
                "id":"credentials", "apiVersion":"v1", "kind":"Secret",
                "name":"database", "namespace":"workload", "mode":"observed",
                "projection":["data"], "sensitive":true,
                "missing":"block-dependent-mutations",
                "source":{"file":"program.ts", "line":2, "column":1}
            }]);
        }
        request.next.compiled = compiler_core::parse_and_validate(&ir.to_string())?;
        request.runtime_ceiling.capabilities = request.next.compiled.capabilities.clone();
        request.runtime_ceiling.accepted_enforcement_gaps =
            request.next.compiled.enforcement_gaps.clone();
        request.runtime_ceiling.accepted_indirect_risks =
            request.next.compiled.indirect_risks.clone();
        request.cleanup_ceiling = request.runtime_ceiling.clone();
        Ok(plan_transition(&request)?)
    }

    #[test]
    fn canonical_projection_discloses_exact_supplemental_requests() -> TestResult {
        let permissions = preview_permissions(&plan("first-private-value", false, true)?)?;
        assert_eq!(permissions.scope, "supplemental-live-preview-only");
        assert_eq!(permissions.non_resource_gets, ["/version"]);
        assert_eq!(permissions.target_requests.len(), 1);
        let target = permissions
            .target_requests
            .first()
            .ok_or("preview target missing")?;
        assert_eq!(target.identity.resource, "configmaps");
        assert_eq!(target.identity.name, "config");
        assert_eq!(target.list_field_selector, "metadata.name=config");
        assert_eq!((target.list_method, target.list_limit), ("LIST", 2));
        assert_eq!(
            (
                target.patch_method,
                target.patch_dry_run,
                target.patch_field_validation
            ),
            ("PATCH", "All", "Strict")
        );
        for verb in ["list", "patch"] {
            assert!(permissions.required_resources.contains(&ResourceAccess {
                namespace: Some("workload".into()),
                api_group: String::new(),
                resource: "configmaps".into(),
                verb: verb.into(),
                resource_names: vec!["config".into()],
            }));
        }
        assert!(permissions.required_resources.contains(&ResourceAccess {
            namespace: Some("workload".into()),
            api_group: String::new(),
            resource: "configmaps".into(),
            verb: "create".into(),
            resource_names: vec![],
        }));
        assert!(permissions.required_resources.contains(&ResourceAccess {
            namespace: Some("control".into()),
            api_group: String::new(),
            resource: "configmaps".into(),
            verb: "get".into(),
            resource_names: vec!["reakt-deploy-identity-example".into()],
        }));
        assert_eq!(
            permissions.potential_creation_targets,
            [CreationTarget {
                namespace: "workload".into(),
                api_group: String::new(),
                resource: "configmaps".into(),
                name: "config".into(),
            }]
        );
        Ok(())
    }

    #[test]
    fn private_values_and_observed_secrets_do_not_change_preview_access() -> TestResult {
        let first = preview_permissions(&plan("private-value-one", false, true)?)?;
        let second = preview_permissions(&plan("private-value-two", true, true)?)?;
        assert_eq!(first, second);
        let encoded = serde_json::to_string(&second)?;
        for private in ["private-value-two", "database", "secrets"] {
            assert!(!encoded.contains(private));
        }
        Ok(())
    }

    #[test]
    fn zero_targets_add_no_wildcard_and_forged_plan_is_rejected() -> TestResult {
        let empty = preview_permissions(&plan("unused-private-value", false, false)?)?;
        assert!(empty.target_requests.is_empty());
        assert!(empty.potential_creation_targets.is_empty());
        assert_eq!(empty.required_resources.len(), 1);
        assert_eq!(
            empty
                .required_resources
                .first()
                .ok_or("identity access missing")?
                .verb,
            "get"
        );

        let mut forged = plan("private-value", false, true)?;
        forged.plan_digest = format!("sha256:{}", "0".repeat(64));
        assert!(preview_permissions(&forged).is_err());
        Ok(())
    }

    #[test]
    fn potential_create_requires_the_matching_canonical_capability() -> TestResult {
        let plan = plan("private-value", false, true)?;
        let identity = preview_permissions(&plan)?
            .target_requests
            .into_iter()
            .next()
            .ok_or("preview target missing")?
            .identity;
        assert!(allows_create(
            &plan.request.next.compiled.capabilities,
            &identity
        ));
        let without_create: Vec<_> = plan
            .request
            .next
            .compiled
            .capabilities
            .iter()
            .filter(|capability| capability.verb != "create")
            .cloned()
            .collect();
        assert!(!allows_create(&without_create, &identity));
        Ok(())
    }
}
