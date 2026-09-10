use crate::{PlatformClient, Result, invalid, store::read_state, valid_name};
#[cfg(feature = "deployer")]
use compiler_core::digest;
#[cfg(feature = "deployer")]
use deploy::{DeploymentRequest, OwnershipAction};
use deploy::{GenerationPlan, OwnershipApproval, OwnershipIntent, validate_ownership_intent};
#[cfg(feature = "deployer")]
use kube::{
    Api,
    api::{ApiResource, DynamicObject, Patch, PatchParams},
    core::GroupVersionKind,
};
#[cfg(feature = "deployer")]
use serde_json::Value;

/// Separately administered from writable per-domain control state. Deployment
/// identities receive GET only; source artifacts never select this namespace.
pub const OWNERSHIP_INTENT_NAMESPACE: &str = "apsides-platform-intents";

impl PlatformClient {
    /// Read operator approvals and check their signed-target binding. The caller
    /// supplies object names, never approval contents or approval credentials.
    /// # Errors
    /// Rejects missing, replaced, malformed, duplicate or differently bound approvals.
    pub async fn load_ownership_intents(
        &self,
        names: &[String],
        target: &GenerationPlan,
        transition: &str,
    ) -> Result<Vec<OwnershipIntent>> {
        let domain = &target.compiled.ir.domain;
        if domain.namespace != self.workload_namespace
            || domain.control_namespace != self.control_namespace
            || domain.cluster_binding != self.cluster_binding
            || names.len() > target.compiled.ir.resources.len()
        {
            return Err(invalid(
                "ownership approval target differs from platform scope",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut intents = Vec::new();
        for name in names {
            if !valid_name(name) || !seen.insert(name) {
                return Err(invalid("invalid or duplicate ownership approval name"));
            }
            let approval =
                read_state::<OwnershipApproval>(&self.client, OWNERSHIP_INTENT_NAMESPACE, name)
                    .await?;
            let intent = OwnershipIntent {
                object_name: name.clone(),
                object_uid: approval.uid,
                object_resource_version: approval.resource_version,
                approval: approval.value,
            };
            validate_ownership_intent(&intent, target, transition)?;
            intents.push(intent);
        }
        Ok(intents)
    }

    #[cfg(feature = "deployer")]
    pub(crate) async fn verify_ownership_intents(&self, request: &DeploymentRequest) -> Result<()> {
        if request.ownership_intents.is_empty() {
            return Ok(());
        }
        let names = request
            .ownership_intents
            .iter()
            .map(|intent| intent.object_name.clone())
            .collect::<Vec<_>>();
        let actual = self
            .load_ownership_intents(&names, &request.next, &request.transition_id)
            .await?;
        if actual != request.ownership_intents {
            return Err(invalid(
                "ownership approval UID, resourceVersion or contents changed",
            ));
        }
        for intent in &actual {
            self.preflight_adoption(&request.next, intent).await?;
        }
        Ok(())
    }

    #[cfg(feature = "deployer")]
    pub(crate) async fn preflight_adoption(
        &self,
        target: &GenerationPlan,
        intent: &OwnershipIntent,
    ) -> Result<()> {
        let approval = &intent.approval;
        if approval.action != OwnershipAction::Adopt {
            return Err(invalid(
                "transfer requires qualified source suspension and held-claim execution",
            ));
        }
        let resource = target
            .compiled
            .ir
            .resources
            .iter()
            .find(|resource| {
                crate::runtime::resource_identity(resource)
                    .is_ok_and(|identity| identity == approval.identity)
            })
            .ok_or_else(|| invalid("adoption resource missing"))?;
        let resolved = compiler_core::resolve_resource_api(resource)
            .map_err(|_| invalid("adoption resource API is unsupported"))?;
        let api_resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk(&resolved.api_group, &resolved.version, &resolved.kind),
            &resolved.plural,
        );
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &resource.namespace, &api_resource);
        let current = api.get(&resource.name).await?;
        if current.metadata.uid.as_ref() != Some(&approval.observed_uid)
            || current.metadata.resource_version.as_ref()
                != Some(&approval.observed_resource_version)
            || current.metadata.deletion_timestamp.is_some()
            || digest(&current.metadata.managed_fields)? != approval.prior_managed_fields_digest
        {
            return Err(invalid(
                "adoption object UID, version, deletion state or prior ownership changed",
            ));
        }
        let mut payload = approval.permitted_apply.clone();
        let metadata = payload
            .get_mut("metadata")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| invalid("adoption metadata missing"))?;
        metadata.insert("uid".into(), Value::String(approval.observed_uid.clone()));
        metadata.insert(
            "resourceVersion".into(),
            Value::String(approval.observed_resource_version.clone()),
        );
        let params = PatchParams::apply(&format!("apsides-{}", target.compiled.ir.domain.id))
            .dry_run()
            .validation_strict();
        let result = api.patch(&resource.name, &params, &Patch::Apply(payload)).await
            .map_err(|_| invalid("adoption non-force dry-run conflicted or failed; new operator approval may be required"))?;
        if result.metadata.uid.as_ref() != Some(&approval.observed_uid) {
            return Err(invalid("adoption dry-run returned another UID"));
        }
        Ok(())
    }
}
