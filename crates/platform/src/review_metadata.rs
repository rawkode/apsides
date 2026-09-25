//! Review blobs describe declarations; only protected registry references select them.
use crate::{NamespaceRegistry, PlatformClient, Result, invalid};
use coordination::{ClaimScope, ReviewMetadata, ReviewRef, ReviewStore};
use deploy::{GenerationAllocation, Phase};

impl PlatformClient {
    pub(crate) fn review_backend(&self) -> Result<&dyn ReviewStore> {
        self.review_store
            .as_deref()
            .ok_or_else(|| invalid("deployment review requires a configured review store"))
    }

    pub(crate) async fn read_review_metadata(
        &self,
        reference: &ReviewRef,
    ) -> Result<ReviewMetadata> {
        reference.validate()?;
        let metadata = self
            .review_backend()?
            .read_review(reference)
            .await?
            .ok_or_else(|| {
                invalid("protected review metadata missing; explicit recovery required")
            })?;
        metadata.validate()?;
        if metadata.reference()? != *reference {
            return Err(invalid(
                "review metadata differs from its protected reference",
            ));
        }
        Ok(metadata)
    }

    #[cfg(feature = "deployer")]
    pub(crate) async fn publish_review_metadata(
        &self,
        metadata: &ReviewMetadata,
    ) -> Result<ReviewRef> {
        metadata.validate()?;
        let expected = metadata.reference()?;
        let written = self.review_backend()?.put_review(metadata).await?;
        if written != expected || self.read_review_metadata(&written).await? != *metadata {
            return Err(invalid("review metadata publication verification failed"));
        }
        Ok(written)
    }

    /// Read the active declaration baseline at the exact authenticated planning revision.
    /// This grants no lifecycle authority and does not retrieve historical releases.
    /// # Errors
    /// Rejects unknown domains, unavailable metadata and mismatched protected references.
    pub async fn load_active_review_metadata(
        &self,
        domain: &str,
        expected_registry_uid: &str,
        expected_registry_revision: &str,
    ) -> Result<Option<ReviewMetadata>> {
        let registry = self.load_registry().await?;
        if registry.uid != expected_registry_uid
            || registry.resource_version != expected_registry_revision
        {
            return Err(invalid(
                "review baseline registry revision changed; replan deployment",
            ));
        }
        match active_review_reference(&registry.value, domain)? {
            Some(reference) => self.read_review_metadata(reference).await.map(Some),
            None => Ok(None),
        }
    }

    #[cfg(feature = "deployer")]
    pub(crate) async fn prepare_review_metadata(
        &self,
        registry: &NamespaceRegistry,
        plan: &deploy::TransitionPlan,
    ) -> Result<(ReviewMetadata, Option<ReviewRef>)> {
        self.review_backend()?;
        let next = crate::project_review_metadata(plan)?;
        validate_reference(
            &next.reference()?,
            &registry.captured_claim_scope,
            &next.scope.domain_id,
            &plan.generation_reservation,
        )?;
        let previous = active_review_reference(registry, &next.scope.domain_id)?.cloned();
        match (plan.request.predecessor(), previous.as_ref()) {
            (None, None) => (),
            (Some(predecessor), Some(reference)) => {
                validate_reference(
                    reference,
                    &registry.captured_claim_scope,
                    &next.scope.domain_id,
                    &predecessor.allocation,
                )?;
                self.read_review_metadata(reference).await?;
            }
            _ => {
                return Err(invalid(
                    "review baseline differs from the planned predecessor",
                ));
            }
        }
        Ok((next, previous))
    }
}

pub(crate) fn validate_review_references(registry: &NamespaceRegistry) -> Result<()> {
    for stored in registry.transitions.values() {
        validate_reference(
            &stored.next_review_metadata,
            &registry.captured_claim_scope,
            stored.domain_id(),
            stored.allocation(),
        )?;
        match (&stored.inputs.previous, &stored.previous_review_metadata) {
            (None, None) => (),
            (Some(previous), Some(reference)) => validate_reference(
                reference,
                &registry.captured_claim_scope,
                stored.domain_id(),
                &previous.allocation,
            )?,
            _ => {
                return Err(invalid(
                    "transition review baseline is missing or unexpected",
                ));
            }
        }
        if stored.record.phase != Phase::Committed
            && stored.previous_review_metadata.as_ref()
                != active_review_reference(registry, stored.domain_id())?
        {
            return Err(invalid(
                "pending review baseline differs from the active declaration",
            ));
        }
    }
    Ok(())
}

pub(crate) fn active_review_reference<'a>(
    registry: &'a NamespaceRegistry,
    domain: &str,
) -> Result<Option<&'a ReviewRef>> {
    let history = registry
        .histories
        .get(domain)
        .ok_or_else(|| invalid("unknown review domain"))?;
    let mut committed = registry
        .transitions
        .values()
        .filter(|stored| stored.domain_id() == domain && stored.record.phase == Phase::Committed);
    match (
        &history.active_allocation,
        committed.next(),
        committed.next(),
    ) {
        (None, None, None) => Ok(None),
        (Some(active), Some(stored), None) if stored.allocation() == active => {
            validate_reference(
                &stored.next_review_metadata,
                &registry.captured_claim_scope,
                domain,
                active,
            )?;
            Ok(Some(&stored.next_review_metadata))
        }
        _ => Err(invalid(
            "active review lacks its unique committed allocation",
        )),
    }
}

fn validate_reference(
    reference: &ReviewRef,
    scope: &ClaimScope,
    domain: &str,
    allocation: &GenerationAllocation,
) -> Result<()> {
    reference.validate()?;
    if reference.scope.claim_scope != *scope
        || reference.scope.domain_id != domain
        || reference.scope.claim_scope.registry_uid != allocation.registry_uid
        || reference.scope.sequence != allocation.sequence
        || reference.scope.transition_id != allocation.transition_id
    {
        return Err(invalid(
            "review reference differs from protected allocation scope",
        ));
    }
    Ok(())
}
