use super::super::authority::{check_approvals, require_qualified_server, require_targets, same};
use super::PendingPreviewBinding;
use crate::{
    IssuedDeployment, PolicyState, ResolvedTransition, Result, TransitionSession, Versioned,
    invalid,
};

pub(super) struct PendingContext {
    pub resolved: ResolvedTransition,
    pub binding: PendingPreviewBinding,
    policy: Versioned<PolicyState>,
    trust: Versioned<serde_json::Value>,
}
impl PendingContext {
    pub async fn authenticate(
        issued: &IssuedDeployment,
        session: &TransitionSession,
    ) -> Result<Self> {
        let resolved = session.resolve().await?;
        if !matches!(
            resolved.stored().record.phase,
            deploy::Phase::Planned
                | deploy::Phase::Staged
                | deploy::Phase::Quiesced
                | deploy::Phase::Activated
                | deploy::Phase::Cleaning
        ) {
            return Err(invalid(
                "pending preview requires a resumable nonterminal transition",
            ));
        }
        let next = resolved.next_review_metadata();
        let approval = issued.require_preview_approval().await?;
        require_targets(approval, &next.resources)?;
        if issued.identity().domain_id != next.scope.domain_id
            || issued.identity().cluster_binding != next.scope.claim_scope.cluster_binding
        {
            return Err(invalid(
                "pending preview identity differs from selected scope",
            ));
        }
        require_qualified_server(
            &issued
                .platform()
                .client
                .apiserver_version()
                .await?
                .git_version,
        )?;
        let policy = issued.platform().load_policy(&next.scope.domain_id).await?;
        let trust = issued
            .platform()
            .load_trust_policy(&next.scope.domain_id)
            .await?;
        check_approvals(issued, &resolved.plan().request).await?;
        let context = Self {
            binding: PendingPreviewBinding::capture(&resolved),
            resolved,
            policy,
            trust,
        };
        context.recheck(issued, session).await?;
        Ok(context)
    }
    pub async fn recheck(
        &self,
        issued: &IssuedDeployment,
        session: &TransitionSession,
    ) -> Result<()> {
        let approval = issued.require_preview_approval().await?;
        require_targets(approval, &self.resolved.next_review_metadata().resources)?;
        let resolved = session.resolve().await?;
        self.binding.validate(&resolved)?;
        let domain = &self.resolved.next_review_metadata().scope.domain_id;
        let policy = issued.platform().load_policy(domain).await?;
        let trust = issued.platform().load_trust_policy(domain).await?;
        if !same(&self.policy, &policy) || !same(&self.trust, &trust) {
            return Err(invalid("pending preview policy or trust changed"));
        }
        check_approvals(issued, &self.resolved.plan().request).await
    }
}
