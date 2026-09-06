//! Required API access for the fixed deployment executor. This report neither
//! grants RBAC nor claims that authorization or admission will allow a request.
use crate::{Result, invalid};
use compiler_core::Capability;
use deploy::{TransitionPlan, plan_transition};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceAccess {
    pub namespace: Option<String>,
    pub api_group: String,
    /// Kubernetes RBAC spelling, including a slash for subresources.
    pub resource: String,
    pub verb: String,
    /// Empty means unrestricted names in the specified scope, never no access.
    pub resource_names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreationTarget {
    pub namespace: String,
    pub api_group: String,
    pub resource: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleDelegation {
    pub namespace: String,
    pub role_name: String,
    pub granted_capabilities: Vec<Capability>,
    pub create_requirement: &'static str,
    pub bind_requirement: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentPermissions {
    pub schema_version: &'static str,
    pub required_resources: Vec<ResourceAccess>,
    pub creation_targets: Vec<CreationTarget>,
    pub role_delegation: Vec<RoleDelegation>,
    pub authorization: &'static str,
    pub limitations: Vec<&'static str>,
}

#[derive(Default)]
struct Requests {
    resources: BTreeSet<ResourceAccess>,
    creates: BTreeSet<CreationTarget>,
}

impl Requests {
    fn add(
        &mut self,
        namespace: Option<&str>,
        group: &str,
        resource: &str,
        verb: &str,
        name: Option<&str>,
    ) {
        self.resources.insert(ResourceAccess {
            namespace: namespace.map(str::to_owned),
            api_group: group.into(),
            resource: resource.into(),
            verb: verb.into(),
            resource_names: name.map(str::to_owned).into_iter().collect(),
        });
    }

    fn stage(&mut self, namespace: &str, group: &str, resource: &str, name: &str) {
        self.add(Some(namespace), group, resource, "get", Some(name));
        // Top-level POST authorization cannot restrict names. Admission must
        // constrain the exact objects whose intended names are reported below.
        self.add(Some(namespace), group, resource, "create", None);
        self.creates.insert(CreationTarget {
            namespace: namespace.into(),
            api_group: group.into(),
            resource: resource.into(),
            name: name.into(),
        });
    }

    fn capability(&mut self, capability: &Capability) {
        let resource = capability.subresource.as_ref().map_or_else(
            || capability.resource.clone(),
            |sub| format!("{}/{sub}", capability.resource),
        );
        if capability.names.is_empty() {
            self.add(
                Some(&capability.namespace),
                &capability.api_group,
                &resource,
                &capability.verb,
                None,
            );
        } else {
            for name in &capability.names {
                self.add(
                    Some(&capability.namespace),
                    &capability.api_group,
                    &resource,
                    &capability.verb,
                    Some(name),
                );
            }
        }
    }
}

/// Enumerate the isolated-proxy executor's required access, conditional Role
/// delegation, and the broader RBAC scopes needed for generated names.
/// This report grants no authority. An upgrade's retirement namespaces must be
/// authenticated against protected history by reservation before the report can
/// be used to provision access; canonicality alone cannot establish that history.
/// # Errors
/// Rejects noncanonical plans and invalid generation identities.
pub fn deployment_permissions(plan: &TransitionPlan) -> Result<DeploymentPermissions> {
    if plan_transition(&plan.request)? != *plan {
        return Err(invalid(
            "deployment permissions require a canonical transition plan",
        ));
    }
    let mut requests = Requests::default();
    control_state(plan, &mut requests)?;
    let roles = stage_generation(plan, &mut requests);
    previous_generation(plan, &mut requests);
    for capability in &plan.cleanup_capabilities {
        requests.capability(capability);
    }
    for intent in &plan.request.ownership_intents {
        requests.add(
            Some(crate::OWNERSHIP_INTENT_NAMESPACE),
            "",
            "configmaps",
            "get",
            Some(&intent.object_name),
        );
        let id = &intent.approval.identity;
        for verb in ["get", "patch"] {
            requests.add(
                Some(&id.namespace),
                &id.api_group,
                &id.resource,
                verb,
                Some(&id.name),
            );
        }
    }
    Ok(DeploymentPermissions {
        schema_version: "reakt.dev/deployment-permissions/v1",
        required_resources: requests.resources.into_iter().collect(),
        creation_targets: requests.creates.into_iter().collect(),
        role_delegation: roles,
        authorization: "required-access-only; effective RBAC and admission not evaluated",
        limitations: vec![
            "top-level create cannot be restricted by resourceNames; creationTargets require admission enforcement",
            "Pod list and generated Pod/ReplicaSet/Node names require broader access; executor UID and ownership checks do not narrow RBAC",
            "bootstrap identity discovery, credential minting, and API discovery use the invoking identity and are outside this issued-deployer report",
            "inventory backend read and conditional publication require separate database credentials",
        ],
    })
}

fn control_state(plan: &TransitionPlan, requests: &mut Requests) -> Result<()> {
    let domain = &plan.request.next.compiled.ir.domain;
    requests.add(
        Some(crate::REGISTRY_AUTHORITY_NAMESPACE),
        "",
        "configmaps",
        "get",
        Some(&crate::registry_authority_name(
            &domain.control_namespace,
            &domain.namespace,
        )?),
    );
    let namespace = &domain.control_namespace;
    // The issued executor rechecks its protected approval during reservation
    // and selected lifecycle operations; bootstrap discovery is separate.
    for name in [
        format!("reakt-deploy-identity-{}", domain.id),
        format!("reakt-policy-{}", domain.id),
        format!("reakt-trust-{}", domain.id),
        format!("reakt-proxy-policy-{}", domain.id),
    ] {
        requests.add(Some(namespace), "", "configmaps", "get", Some(&name));
    }
    for name in [
        format!("reakt-registry-{}", domain.namespace),
        format!("reakt-activation-{}", domain.id),
        format!("reakt-inventory-{}", domain.id),
    ] {
        for verb in ["get", "update"] {
            requests.add(Some(namespace), "", "configmaps", verb, Some(&name));
        }
    }
    if plan.request.previous.is_none() {
        for name in [
            format!("reakt-activation-{}", domain.id),
            format!("reakt-inventory-{}", domain.id),
        ] {
            requests.stage(namespace, "", "configmaps", &name);
        }
    }
    if plan
        .request
        .next
        .compiled
        .ir
        .inputs
        .iter()
        .any(|input| input.sensitive)
    {
        requests.add(
            Some(namespace),
            "",
            "secrets",
            "get",
            Some(&format!("reakt-rollout-key-{}", domain.id)),
        );
    }
    requests.add(Some(namespace), "", "pods", "list", None);
    requests.add(Some(namespace), "apps", "replicasets", "get", None);
    requests.add(None, "", "nodes", "get", None);
    Ok(())
}

fn stage_generation(plan: &TransitionPlan, requests: &mut Requests) -> Vec<RoleDelegation> {
    let control = &plan.request.next.compiled.ir.domain.control_namespace;
    let name = &plan.generation_reservation.service_account_name;
    let proxy = format!("reakt-proxy-{}", plan.request.next.generation_id);
    for account in [name.clone(), format!("{name}-process")] {
        requests.stage(control, "", "serviceaccounts", &account);
    }
    requests.stage(control, "", "secrets", &format!("{name}-tls"));
    for configuration in [format!("{name}-config"), format!("{proxy}-config")] {
        requests.stage(control, "", "configmaps", &configuration);
    }
    for deployment in [name.as_str(), proxy.as_str()] {
        requests.stage(control, "apps", "deployments", deployment);
    }
    requests.add(Some(control), "apps", "deployments", "update", Some(name));
    requests.stage(control, "", "services", &proxy);
    let mut namespaces: BTreeMap<String, Vec<Capability>> = BTreeMap::new();
    for capability in &plan.request.next.compiled.capabilities {
        namespaces
            .entry(capability.namespace.clone())
            .or_default()
            .push(capability.clone());
    }
    namespaces.into_iter().map(|(namespace, capabilities)| {
        for resource in ["roles", "rolebindings"] {
            requests.stage(&namespace, "rbac.authorization.k8s.io", resource, name);
        }
        RoleDelegation { namespace, role_name: name.clone(), granted_capabilities: capabilities,
            create_requirement: "hold all granted capabilities in this scope OR be authorized to escalate the Role",
            bind_requirement: "hold all granted capabilities in this scope OR be authorized to bind the Role" }
    }).collect()
}

fn previous_generation(plan: &TransitionPlan, requests: &mut Requests) {
    let Some(previous) = plan.request.predecessor() else {
        return;
    };
    let control = &previous.control_namespace;
    let name = &previous.allocation.service_account_name;
    let proxy = format!("reakt-proxy-{}", previous.allocation.generation_id);
    requests.add(Some(control), "", "serviceaccounts", "get", Some(name));
    requests.add(Some(control), "", "serviceaccounts", "delete", Some(name));
    requests.add(
        Some(control),
        "",
        "configmaps",
        "get",
        Some(&format!("{proxy}-config")),
    );
    for deployment in [name.as_str(), proxy.as_str()] {
        for verb in ["get", "update"] {
            requests.add(Some(control), "apps", "deployments", verb, Some(deployment));
        }
    }
    requests.add(Some(control), "", "pods", "get", None);
    // kube-core 4.2's WebSocket exec request uses HTTP GET, not POST.
    requests.add(Some(control), "", "pods/exec", "get", None);
    for namespace in &previous.retirement_role_binding_namespaces {
        for verb in ["get", "delete"] {
            requests.add(
                Some(namespace),
                "rbac.authorization.k8s.io",
                "rolebindings",
                verb,
                Some(name),
            );
        }
    }
    for resource in &previous.inventory.resources {
        requests.add(
            Some(&resource.identity.namespace),
            &resource.identity.api_group,
            &resource.identity.resource,
            "get",
            Some(&resource.identity.name),
        );
    }
}
