use crate::{IssuedDeployment, PolicyState, Result, Versioned, invalid};
use coordination::{ReviewMetadataV1, ReviewRef};
use deploy::{DeploymentRequest, TransitionPlan};
use inventory::InventoryStore;
use serde_json::Value;

pub(super) struct Context {
    pub plan: TransitionPlan,
    pub next: ReviewMetadataV1,
    pub previous: Option<ReviewMetadataV1>,
    previous_reference: Option<ReviewRef>,
    policy: Versioned<PolicyState>,
    trust: Versioned<Value>,
    registry_uid: String,
    registry_revision: String,
}

impl Context {
    pub async fn authenticate(
        issued: &IssuedDeployment,
        request: &DeploymentRequest,
        trust_digest: &str,
        inventory: &dyn InventoryStore,
    ) -> Result<Self> {
        if !request.observations.is_empty() {
            return Err(invalid("preview requires empty observations"));
        }
        let plan = deploy::plan_transition(request)?;
        let platform = issued.platform();
        let domain = &request.next.compiled.ir.domain;
        require_qualified_server(&platform.client.apiserver_version().await?.git_version)?;
        let approval = issued.require_preview_approval().await?;
        if issued.identity().domain_id != domain.id
            || issued.identity().cluster_binding != domain.cluster_binding
        {
            return Err(invalid("preview identity does not bind the request domain"));
        }
        let registry = platform.load_registry().await?;
        platform.validate_reservation_snapshot(&registry, request)?;
        registry
            .value
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("registry allocation counter exhausted"))?;
        if registry.value.transitions.values().any(|stored| {
            stored.domain_id() == domain.id && stored.record.phase != deploy::Phase::Committed
        }) {
            return Err(invalid("fresh preview requires no pending transition"));
        }
        let policy = platform.load_policy(&domain.id).await?;
        if policy.value.runtime_ceiling != request.runtime_ceiling
            || policy.value.cleanup_ceiling != request.cleanup_ceiling
        {
            return Err(invalid("preview policy differs from the request"));
        }
        let trust = platform.load_trust_policy(&domain.id).await?;
        if compiler_core::digest(&trust.value)? != trust_digest {
            return Err(invalid("preview release trust changed"));
        }
        check_inventory(request, inventory).await?;
        check_approvals(issued, request).await?;
        let (next, previous_reference) = platform
            .prepare_review_metadata(&registry.value, &plan)
            .await?;
        require_targets(approval, &next.resources)?;
        let previous = match &previous_reference {
            Some(reference) => Some(platform.read_review_metadata(reference).await?),
            None => None,
        };
        let result = Self {
            plan,
            next,
            previous,
            previous_reference,
            policy,
            trust,
            registry_uid: registry.uid,
            registry_revision: registry.resource_version,
        };
        result.recheck(issued, request, inventory).await?;
        Ok(result)
    }

    pub async fn recheck(
        &self,
        issued: &IssuedDeployment,
        request: &DeploymentRequest,
        inventory: &dyn InventoryStore,
    ) -> Result<()> {
        let approval = issued.require_preview_approval().await?;
        require_targets(approval, &self.next.resources)?;
        let platform = issued.platform();
        let registry = platform.load_registry().await?;
        if registry.uid != self.registry_uid || registry.resource_version != self.registry_revision
        {
            return Err(invalid("preview registry changed; replan"));
        }
        platform.validate_reservation_snapshot(&registry, request)?;
        let domain = &request.next.compiled.ir.domain.id;
        let policy = platform.load_policy(domain).await?;
        let trust = platform.load_trust_policy(domain).await?;
        if !same(&self.policy, &policy) || !same(&self.trust, &trust) {
            return Err(invalid("preview policy or trust changed"));
        }
        check_inventory(request, inventory).await?;
        check_approvals(issued, request).await?;
        if let Some(reference) = &self.previous_reference {
            let current = platform.read_review_metadata(reference).await?;
            if self.previous.as_ref() != Some(&current) {
                return Err(invalid("preview predecessor metadata changed"));
            }
        }
        Ok(())
    }
}

pub(super) fn require_qualified_server(version: &str) -> Result<()> {
    if version != "v1.34.3" {
        return Err(invalid(
            "live preview requires the qualified v1.34.3 conditional SSA server",
        ));
    }
    Ok(())
}

pub(super) fn same<T: PartialEq>(left: &Versioned<T>, right: &Versioned<T>) -> bool {
    left.uid == right.uid
        && left.resource_version == right.resource_version
        && left.value == right.value
}

async fn check_inventory(request: &DeploymentRequest, store: &dyn InventoryStore) -> Result<()> {
    if let Some(previous) = request.predecessor()? {
        let id = inventory::snapshot_id(&previous.scope, &previous.inventory)?;
        let actual = store
            .read_snapshot(&previous.scope, &id)
            .await?
            .ok_or_else(|| invalid("preview predecessor inventory is missing"))?;
        if actual != previous.inventory || inventory::snapshot_id(&previous.scope, &actual)? != id {
            return Err(invalid("preview predecessor inventory changed"));
        }
    }
    Ok(())
}

pub(super) async fn check_approvals(
    issued: &IssuedDeployment,
    request: &DeploymentRequest,
) -> Result<()> {
    let names = request
        .ownership_intents
        .iter()
        .map(|intent| intent.object_name.clone())
        .collect::<Vec<_>>();
    let actual = issued
        .platform()
        .load_ownership_intents(&names, &request.next, &request.transition_id)
        .await?;
    if actual != request.ownership_intents {
        return Err(invalid("preview ownership approvals changed"));
    }
    Ok(())
}

pub(super) fn require_targets(
    approval: &crate::DeploymentPreviewApproval,
    resources: &[coordination::ReviewResource],
) -> Result<()> {
    if resources
        .iter()
        .any(|resource| !approval.targets.contains(&resource.identity))
    {
        return Err(invalid(
            "preview target is outside the approved identity scope",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "preview_authority_tests.rs"]
mod tests;
