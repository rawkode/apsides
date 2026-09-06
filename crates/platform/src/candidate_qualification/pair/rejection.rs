use super::{CANDIDATE_PAIR_QUALIFICATION_EXECUTOR, CandidatePairRole, CandidateQualificationPair};
use crate::{PlatformClient, Result, invalid};
use compiler_core::Capability;

impl PlatformClient {
    /// Attempt the exact successor under the initial policy and verify rejection
    /// before any reservation write. Success is never an accepted outcome.
    /// # Errors
    /// Rejects changed authority, predecessor drift, or any other reservation outcome.
    pub async fn reject_pair_upgrade_candidate(
        &self,
        request: &deploy::DeploymentRequest,
        pair: &CandidateQualificationPair<'_>,
        inventory: &dyn inventory::InventoryStore,
        expected_missing: &Capability,
    ) -> Result<()> {
        self.verify_pair_operation(CandidatePairRole::Initial, pair)
            .await?;
        let binding = self.candidate_binding(request, pair.successor)?;
        if pair
            .initial
            .compiled()
            .capabilities
            .contains(expected_missing)
            || !pair
                .successor
                .compiled()
                .capabilities
                .contains(expected_missing)
        {
            return Err(invalid("pair rejection requires a newly added capability"));
        }
        let current = self.validate_pair_predecessor(request, pair).await?;
        let previous = request
            .previous
            .as_deref()
            .ok_or_else(|| invalid("pair rejection predecessor is absent"))?;
        let review_ref = &current
            .value
            .transition(&previous.allocation.transition_id)?
            .next_review_metadata;
        let review = self.read_review_metadata(review_ref).await?;
        let policy = self.load_policy(binding.domain_id).await?;
        let trust = self.load_trust_policy(binding.domain_id).await?;
        if compiler_core::digest(&policy.value)? != pair.target.initial_policy_digest
            || compiler_core::digest(&trust.value)? != pair.initial.trust_policy_digest()
            || request.runtime_ceiling != policy.value.runtime_ceiling
            || request.cleanup_ceiling != policy.value.cleanup_ceiling
        {
            return Err(invalid(
                "pair rejection request differs from initial authority",
            ));
        }
        // The same pure planner runs inside reserve_authenticated before review
        // publication or registry persistence. Refuse a potentially writable call.
        if !matches!(deploy::plan_transition(request), Err(error) if expected_ceiling(&error, expected_missing))
        {
            return Err(invalid(
                "pair rejection plan lacks the expected ceiling failure",
            ));
        }
        let outcome = Box::pin(self.reserve_authenticated(
            request,
            CANDIDATE_PAIR_QUALIFICATION_EXECUTOR,
            inventory,
            Some(pair.successor.trust_policy_digest()),
        ))
        .await;
        self.verify_pair_operation(CandidatePairRole::Initial, pair)
            .await?;
        if !super::super::same_version(&self.load_registry().await?, &current)
            || !super::super::same_version(&self.load_policy(binding.domain_id).await?, &policy)
            || !super::super::same_version(
                &self.load_trust_policy(binding.domain_id).await?,
                &trust,
            )
            || self.read_review_metadata(review_ref).await? != review
        {
            return Err(invalid(
                "pair rejection authority changed during reservation",
            ));
        }
        match outcome {
            Err(crate::Error::Deploy(error)) if expected_ceiling(&error, expected_missing) => {
                Ok(())
            }
            _ => Err(invalid(
                "pair reservation did not return the expected ceiling rejection",
            )),
        }
    }
}

fn expected_ceiling(error: &deploy::Error, capability: &Capability) -> bool {
    matches!(error, deploy::Error::Core(compiler_core::Error::Ceiling(actual))
        if *actual == format!("{} {}/{} in {} is not explicitly allowed",
            capability.verb, capability.api_group, capability.resource, capability.namespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_requires_the_exact_typed_missing_capability() {
        let capability = Capability {
            namespace: "production".into(),
            api_group: String::new(),
            resource: "secrets".into(),
            subresource: None,
            verb: "get".into(),
            names: vec!["credentials".into()],
        };
        let message = "get /secrets in production is not explicitly allowed";
        assert!(expected_ceiling(
            &deploy::Error::Core(compiler_core::Error::Ceiling(message.into())),
            &capability
        ));
        assert!(!expected_ceiling(
            &deploy::Error::Core(compiler_core::Error::Invalid(message.into())),
            &capability
        ));
        assert!(!expected_ceiling(
            &deploy::Error::Plan(message.into()),
            &capability
        ));
        assert!(!expected_ceiling(
            &deploy::Error::Core(compiler_core::Error::Ceiling("another capability".into())),
            &capability
        ));
    }
}
