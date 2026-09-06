use crate::{NamespaceRegistry, PlatformClient, Result, Versioned, invalid};
use compiler_core::Capability;
use deploy::{Phase, plan_transition};
use kube::{
    Api,
    api::{ApiResource, DynamicObject, PostParams},
    core::GroupVersionKind,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

impl PlatformClient {
    #[cfg(feature = "deployer")]
    pub(crate) async fn verify_runtime_identity(
        &self,
        stored: &crate::transition_session::ResolvedStoredTransition,
    ) -> Result<()> {
        let name = &stored.plan.generation_reservation.service_account_name;
        for (api_version, kind, namespace, desired) in
            identity_resources(stored, &self.control_namespace, &self.workload_namespace)?
        {
            let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
            let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
            let api: Api<DynamicObject> =
                Api::namespaced_with(self.client.clone(), &namespace, &ar);
            let actual = api.get(name).await?;
            let expected: DynamicObject = serde_json::from_value(desired)?;
            let key = format!("{api_version}/{kind}/{namespace}/{name}");
            if stored.staged_identity_uids.get(&key) != actual.metadata.uid.as_ref()
                || actual.metadata.uid.is_none()
                || actual.metadata.deletion_timestamp.is_some()
                || actual.data != expected.data
                || actual.metadata.annotations != expected.metadata.annotations
            {
                return Err(invalid(
                    "staged credential identity or exact capability content changed",
                ));
            }
        }
        Ok(())
    }
    /// Provision a generation's distinct `ServiceAccount` and exact derived Role/
    /// `RoleBindings`. This starts no controller or proxy process. Each created UID
    /// is persisted before the next object; retries recognize only recorded UIDs.
    /// # Errors
    /// Rejects stale executor authority, policy drift and unrecorded existing
    /// control objects. A crash between create and UID persistence needs explicit
    /// platform recovery of that object; there is no blind adoption.
    pub async fn stage_runtime_identity(
        &self,
        transition_id: &str,
        executor_id: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let mut registry = self.load_registry().await?;
        let stored = self.selected(&registry, transition_id)?;
        if stored.record.executor_id != executor_id
            || stored.record.phase != Phase::Planned
            || plan_transition(&stored.plan.request)? != *stored.plan
        {
            return Err(invalid(
                "identity staging requires the owning Planned transition",
            ));
        }
        deploy::require_event_capacity(&stored.plan, &stored.record)?;
        let domain = &stored.plan.request.next.compiled.ir.domain;
        let policy = self.load_policy(&domain.id).await?;
        if policy.value.runtime_ceiling != stored.plan.request.runtime_ceiling
            || policy.value.cleanup_ceiling != stored.plan.request.cleanup_ceiling
        {
            return Err(invalid(
                "platform ceiling changed since transition reservation",
            ));
        }
        let name = &stored.plan.generation_reservation.service_account_name;
        let resources =
            identity_resources(&stored, &self.control_namespace, &self.workload_namespace)?;
        for (api_version, kind, namespace, desired) in resources {
            let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
            let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
            let api: Api<DynamicObject> =
                Api::namespaced_with(self.client.clone(), &namespace, &ar);
            let key = format!("{api_version}/{kind}/{namespace}/{name}");
            let expected: DynamicObject = serde_json::from_value(desired)?;
            let known_uid = registry
                .value
                .transition(transition_id)?
                .staged_identity_uids
                .get(&key);
            let existing = api.get_opt(name).await?;
            let actual = match (existing, known_uid) {
                (Some(object), Some(uid))
                    if object.metadata.uid.as_ref() == Some(uid)
                        && object.metadata.deletion_timestamp.is_none()
                        && object.data == expected.data
                        && object.metadata.annotations == expected.metadata.annotations =>
                {
                    object
                }
                (None, None) => api.create(&PostParams::default(), &expected).await?,
                _ => {
                    return Err(invalid(
                        "staged identity exists without matching recorded UID/content; platform recovery required",
                    ));
                }
            };
            let uid = actual
                .metadata
                .uid
                .filter(|uid| !uid.is_empty())
                .ok_or_else(|| invalid("created identity omitted UID"))?;
            if known_uid.is_some() {
                continue;
            }
            let mut next = registry.value.clone();
            next.transitions
                .get_mut(transition_id)
                .ok_or_else(|| invalid("transition disappeared"))?
                .staged_identity_uids
                .insert(key, uid);
            registry = self.persist_registry(&registry, &next).await?;
        }
        Ok(registry)
    }
}

fn role_rule(capability: &Capability) -> Value {
    let resource = capability.subresource.as_ref().map_or_else(
        || capability.resource.clone(),
        |sub| format!("{}/{sub}", capability.resource),
    );
    let mut rule = json!({"apiGroups":[capability.api_group],"resources":[resource],"verbs":[capability.verb]});
    if !capability.names.is_empty() {
        rule["resourceNames"] = json!(capability.names);
    }
    rule
}

fn identity_resources(
    stored: &crate::transition_session::ResolvedStoredTransition,
    control_namespace: &str,
    workload_namespace: &str,
) -> Result<Vec<(&'static str, &'static str, String, Value)>> {
    let name = &stored.plan.generation_reservation.service_account_name;
    let transition_id = &stored.plan.request.transition_id;
    let annotations = json!({"reakt.dev/transition": transition_id, "reakt.dev/generation": stored.plan.request.next.generation_id,
        "reakt.dev/plan-digest":stored.plan.plan_digest});
    let mut resources = vec![(
        "v1",
        "ServiceAccount",
        control_namespace.to_owned(),
        json!({
            "apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":name,"namespace":control_namespace,"annotations":annotations},
            "automountServiceAccountToken":false
        }),
    )];
    let mut rules: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for capability in &stored.plan.request.next.compiled.capabilities {
        if capability.namespace != workload_namespace && capability.namespace != control_namespace {
            return Err(invalid(
                "runtime capability lies outside platform staging scopes",
            ));
        }
        rules
            .entry(&capability.namespace)
            .or_default()
            .push(role_rule(capability));
    }
    for (namespace, rules) in rules {
        resources.push(("rbac.authorization.k8s.io/v1", "Role", namespace.into(), json!({
            "apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":name,"namespace":namespace,"annotations":annotations},"rules":rules
        })));
        resources.push(("rbac.authorization.k8s.io/v1", "RoleBinding", namespace.into(), json!({
            "apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":name,"namespace":namespace,"annotations":annotations},
            "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":name},
            "subjects":[{"kind":"ServiceAccount","name":name,"namespace":control_namespace}]
        })));
    }
    Ok(resources)
}
