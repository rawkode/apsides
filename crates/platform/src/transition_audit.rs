//! Bounded protected retention; immutable archive data grants no lifecycle authority.
#[cfg(feature = "deployer")]
use crate::Versioned;
use crate::{NamespaceRegistry, PlatformClient, Result, StoredTransition, invalid};
#[cfg(feature = "deployer")]
use coordination::AuditScope;
use coordination::{AuditRef, AuditStore, TransitionAuditV1};
use deploy::Phase;

impl PlatformClient {
    pub(crate) fn audit_backend(&self) -> Result<&dyn AuditStore> {
        self.audit_store
            .as_deref()
            .ok_or_else(|| invalid("registry operations require a configured audit store"))
    }

    pub(crate) fn validate_registry_retention(&self, registry: &NamespaceRegistry) -> Result<()> {
        for (domain, history) in &registry.histories {
            let mut committed = None;
            let mut pending = None;
            for stored in registry
                .transitions
                .values()
                .filter(|stored| stored.domain_id() == domain)
            {
                let slot = if stored.record.phase == Phase::Committed {
                    &mut committed
                } else {
                    &mut pending
                };
                if slot.replace(stored).is_some() {
                    return Err(invalid(
                        "registry retention allows only active committed and one pending transition per domain",
                    ));
                }
            }
            match (&history.active_allocation, committed) {
                (None, None)
                    if !registry.active_writer_checkpoints.contains_key(domain)
                        && !registry.audit_heads.contains_key(domain) => {}
                (Some(active), Some(stored)) if stored.allocation() == active => {
                    let checkpoint = crate::writer_checkpoint::active(
                        registry,
                        domain,
                        &self.control_namespace,
                    )?;
                    validate_committed_writer(stored, checkpoint)?;
                    validate_head(registry, stored)?;
                }
                _ => {
                    return Err(invalid(
                        "registry active allocation lacks its unique committed transition and checkpoint",
                    ));
                }
            }
            if let Some(stored) = pending
                && (stored
                    .inputs
                    .previous
                    .as_ref()
                    .map(|previous| &previous.allocation)
                    != history.active_allocation.as_ref()
                    || stored.previous_writer_checkpoint.as_ref()
                        != registry.active_writer_checkpoints.get(domain))
            {
                return Err(invalid(
                    "pending transition predecessor differs from protected active authority",
                ));
            }
        }
        if registry
            .transitions
            .values()
            .any(|stored| !registry.histories.contains_key(stored.domain_id()))
            || registry
                .audit_heads
                .keys()
                .any(|domain| !registry.histories.contains_key(domain))
        {
            return Err(invalid(
                "registry contains orphan transition or audit domain",
            ));
        }
        Ok(())
    }

    async fn read_audit_exact(&self, reference: &AuditRef) -> Result<TransitionAuditV1> {
        reference.validate()?;
        let audit = self
            .audit_backend()?
            .read_audit(reference)
            .await?
            .ok_or_else(|| {
                invalid("protected transition audit missing; explicit recovery required")
            })?;
        audit.validate()?;
        if audit.reference()? != *reference {
            return Err(invalid(
                "transition audit differs from its protected reference",
            ));
        }
        Ok(audit)
    }

    /// Read historical metadata only through the authenticated protected audit chain.
    /// No release is needed and the returned record grants no resume or mutation authority.
    /// # Errors
    /// Rejects unknown domains, unavailable/corrupt records, invalid links or excessive traversal.
    pub async fn load_transition_audit(
        &self,
        domain: &str,
        transition: &str,
    ) -> Result<Option<TransitionAuditV1>> {
        let registry = self.load_registry().await?;
        if !registry.value.histories.contains_key(domain) {
            return Err(invalid("unknown audit domain"));
        }
        let mut cursor = registry.value.audit_heads.get(domain).cloned();
        for _ in 0..100_000 {
            let Some(reference) = cursor else {
                return Ok(None);
            };
            validate_reference_scope(&reference, &registry.value, domain)?;
            let audit = self.read_audit_exact(&reference).await?;
            if reference.scope.transition_id == transition {
                return Ok(Some(audit));
            }
            cursor = audit.previous;
        }
        if cursor.is_none() {
            Ok(None)
        } else {
            Err(invalid("transition audit traversal budget exhausted"))
        }
    }

    #[cfg(feature = "deployer")]
    pub(crate) async fn archive_predecessor(
        &self,
        previous: &Versioned<NamespaceRegistry>,
        next: &mut NamespaceRegistry,
        successor: &str,
    ) -> Result<()> {
        let selected = self.selected(previous, successor)?;
        let domain = selected.domain_id();
        let Some(metadata) = selected.plan.request.predecessor() else {
            if previous.value.audit_heads.contains_key(domain) {
                return Err(invalid("initial transition cannot advance an audit chain"));
            }
            return Ok(());
        };
        self.validate_registry_retention(&previous.value)?;
        let active =
            crate::writer_checkpoint::active(&previous.value, domain, &self.control_namespace)?;
        let old = previous
            .value
            .transition(&metadata.allocation.transition_id)?;
        if old.record.phase != Phase::Committed
            || old.allocation() != &metadata.allocation
            || active.allocation != metadata.allocation
            || selected.previous_writer_checkpoint.as_ref() != Some(active)
            || selected.previous_review_metadata.as_ref() != Some(&old.next_review_metadata)
        {
            return Err(invalid(
                "archive predecessor differs from committed active writer authority",
            ));
        }
        active.validate(
            metadata,
            &self.control_namespace,
            &self.workload_namespace,
            &self.cluster_binding,
        )?;
        let prior_head = previous.value.audit_heads.get(domain).cloned();
        if let Some(reference) = &prior_head {
            self.read_audit_exact(reference).await?;
        }
        let audit = project_audit(&previous.value, old, prior_head)?;
        let expected = audit.reference()?;
        let written = self.audit_backend()?.put_audit(&audit).await?;
        if written != expected || self.read_audit_exact(&written).await? != audit {
            return Err(invalid("transition audit publication verification failed"));
        }
        if next.transitions.get(successor).is_none_or(|stored| {
            stored.record.phase != Phase::Committed
                || stored.next_review_metadata != selected.next_review_metadata
                || stored.previous_review_metadata != selected.previous_review_metadata
        }) || next.transitions.get(&metadata.allocation.transition_id) != Some(old)
            || next.audit_heads.get(domain) != previous.value.audit_heads.get(domain)
        {
            return Err(invalid(
                "archive candidate changed outside the successor commit",
            ));
        }
        next.transitions
            .remove(&metadata.allocation.transition_id)
            .ok_or_else(|| invalid("archive predecessor disappeared before pruning"))?;
        next.audit_heads.insert(domain.into(), expected);
        Ok(())
    }
}

fn validate_reference_scope(
    reference: &AuditRef,
    registry: &NamespaceRegistry,
    domain: &str,
) -> Result<()> {
    reference.validate()?;
    if reference.scope.claim_scope != registry.captured_claim_scope
        || reference.scope.domain_id != domain
    {
        return Err(invalid(
            "audit reference differs from protected registry scope",
        ));
    }
    Ok(())
}

fn validate_head(registry: &NamespaceRegistry, active: &StoredTransition) -> Result<()> {
    match (
        &active.inputs.previous,
        registry.audit_heads.get(active.domain_id()),
    ) {
        (None, None) => Ok(()),
        (Some(previous), Some(head)) => {
            validate_reference_scope(head, registry, active.domain_id())?;
            if head.scope.transition_id != previous.allocation.transition_id
                || head.scope.sequence != previous.allocation.sequence
            {
                return Err(invalid(
                    "audit head differs from active transition predecessor",
                ));
            }
            Ok(())
        }
        _ => Err(invalid(
            "active transition requires its exact predecessor audit head",
        )),
    }
}

#[cfg(feature = "deployer")]
fn project_audit(
    registry: &NamespaceRegistry,
    stored: &StoredTransition,
    previous: Option<AuditRef>,
) -> Result<TransitionAuditV1> {
    let allocation = stored.allocation();
    let mut audit = TransitionAuditV1 {
        schema_version: coordination::TRANSITION_AUDIT_SCHEMA.into(),
        scope: AuditScope {
            claim_scope: registry.captured_claim_scope.clone(),
            domain_id: stored.domain_id().into(),
            transition_id: allocation.transition_id.clone(),
            sequence: allocation.sequence,
        },
        allocation: coordination::AuditAllocation {
            sequence: allocation.sequence,
            generation_id: allocation.generation_id.clone(),
            transition_id: allocation.transition_id.clone(),
            service_account_name: allocation.service_account_name.clone(),
        },
        executor_id: stored.record.executor_id.clone(),
        final_revision: stored.record.revision,
        final_verification_epoch: stored.record.verification_epoch,
        events: project_events(stored)?,
        cleanup: project_cleanup(stored)?,
        ownership: stored
            .inputs
            .ownership_intents
            .iter()
            .map(|intent| coordination::AuditOwnership {
                identity: intent.identity.clone(),
                observed_uid: intent.observed_uid.clone(),
                action: intent.action.clone(),
            })
            .collect(),
        retirement: project_retirement(stored)?,
        previous,
    };
    audit.ownership.sort_by(|a, b| a.identity.cmp(&b.identity));
    audit.validate()?;
    Ok(audit)
}

#[cfg(feature = "deployer")]
fn project_events(stored: &StoredTransition) -> Result<Vec<coordination::AuditEvent>> {
    let mut epoch = 0_u64;
    stored
        .record
        .receipts
        .iter()
        .map(|receipt| {
            use coordination::AuditEventKind as Kind;
            let kind = match &receipt.event {
                deploy::Event::Staged { .. } => Kind::Staged,
                deploy::Event::Quiesced { .. } => Kind::Quiesced,
                deploy::Event::Activated { .. } => Kind::Activated,
                deploy::Event::Cleaning { .. } => Kind::Cleaning,
                deploy::Event::Committed { .. } => Kind::Committed,
                deploy::Event::Blocked { .. } => Kind::Blocked,
                deploy::Event::Resume { .. } => {
                    epoch = epoch
                        .checked_add(1)
                        .ok_or_else(|| invalid("audit event epoch exhausted"))?;
                    Kind::Resume
                }
            };
            Ok(coordination::AuditEvent {
                kind,
                event_id: receipt.event_id.clone(),
                verification_epoch: epoch,
            })
        })
        .collect()
}

#[cfg(feature = "deployer")]
fn project_cleanup(stored: &StoredTransition) -> Result<Vec<coordination::AuditCleanup>> {
    let mut cleanup = match stored.record.receipts.last().map(|receipt| &receipt.event) {
        Some(deploy::Event::Committed {
            cleanup_receipts, ..
        }) => cleanup_receipts
            .iter()
            .map(|receipt| coordination::AuditCleanup {
                identity: receipt.identity.clone(),
                expected_uid: receipt.expected_uid.clone(),
                outcome: receipt.outcome.clone(),
            })
            .collect::<Vec<_>>(),
        _ => return Err(invalid("committed audit has no final commit event")),
    };
    cleanup.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(cleanup)
}

#[cfg(feature = "deployer")]
fn project_retirement(stored: &StoredTransition) -> Result<Vec<coordination::AuditRetirement>> {
    let (Some(writer), Some(drain)) = (&stored.previous_writer_checkpoint, &stored.upgrade_drain)
    else {
        return if stored.inputs.previous.is_none() && stored.upgrade_drain.is_none() {
            Ok(vec![])
        } else {
            Err(invalid(
                "committed upgrade audit lacks retirement identities",
            ))
        };
    };
    if drain.retired_identity_uids != writer.retirement_identity_uids {
        return Err(invalid(
            "committed upgrade retirement differs from its protected writer",
        ));
    }
    let mut identities = Vec::new();
    for (key, uid) in &drain.retired_identity_uids {
        let parts: Vec<_> = key.split('/').collect();
        let identity = match parts.as_slice() {
            ["v1", "ServiceAccount", namespace, name] => deploy::ResourceIdentity {
                api_group: String::new(),
                resource: "serviceaccounts".into(),
                namespace: (*namespace).into(),
                name: (*name).into(),
            },
            [
                "rbac.authorization.k8s.io",
                "v1",
                "RoleBinding",
                namespace,
                name,
            ] => deploy::ResourceIdentity {
                api_group: "rbac.authorization.k8s.io".into(),
                resource: "rolebindings".into(),
                namespace: (*namespace).into(),
                name: (*name).into(),
            },
            _ => {
                return Err(invalid(
                    "unsupported identity in committed retirement audit",
                ));
            }
        };
        identities.push(coordination::AuditRetirement {
            identity,
            uid: uid.clone(),
        });
    }
    identities.sort_by(|a, b| a.identity.cmp(&b.identity));
    Ok(identities)
}

fn validate_committed_writer(
    stored: &StoredTransition,
    checkpoint: &crate::WriterCheckpoint,
) -> Result<()> {
    if stored.runtime_binding.as_ref() != Some(&checkpoint.runtime_binding)
        || stored.request_drain_qualification.as_ref()
            != Some(&checkpoint.request_drain_qualification)
        || stored.proxy_identity.as_ref() != Some(&checkpoint.proxy_identity)
        || stored.inputs.policy_digest != checkpoint.runtime_binding.policy_digest
    {
        return Err(invalid(
            "committed transition differs from its active writer checkpoint",
        ));
    }
    let control = &checkpoint.control_namespace;
    let domain = stored.domain_id();
    let controller_key = format!(
        "apps/v1/Deployment/{control}/{}",
        checkpoint.allocation.service_account_name
    );
    let activation_key = format!("v1/ConfigMap/{control}/reakt-activation-{domain}");
    let inventory_key = format!("v1/ConfigMap/{control}/reakt-inventory-{domain}");
    for (key, uid) in [
        (&controller_key, &checkpoint.controller_deployment_uid),
        (&activation_key, &checkpoint.activation_uid),
        (&inventory_key, &checkpoint.inventory_uid),
    ] {
        if stored.staged_identity_uids.get(key) != Some(uid) {
            return Err(invalid(
                "committed writer object UID differs from active checkpoint",
            ));
        }
    }
    for (key, uid) in &checkpoint.retirement_identity_uids {
        if stored.staged_identity_uids.get(key) != Some(uid) {
            return Err(invalid(
                "committed writer retirement UID differs from active checkpoint",
            ));
        }
    }
    Ok(())
}
