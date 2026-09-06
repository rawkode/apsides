use crate::{
    ActivationState, ActivationStatus, ExpectedPod, ExpectedWriters, NamespaceRegistry,
    PlatformClient, Result, StoredTransition, UpgradeDrainState, Versioned, WriterCheckpoint,
    invalid,
};
use compiler_core::digest;
use deploy::{Event, Phase, Quiescence, TerminationEvidence};
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Node, Pod},
};
use kube::{
    Api,
    api::{ApiResource, DeleteParams, DynamicObject, ListParams, PostParams, Preconditions},
    core::GroupVersionKind,
};

impl PlatformClient {
    /// Freeze and retire the sole credential-bearing generation API writer.
    /// Current tokenless controller Pods are stopped as operational hygiene; a
    /// surviving copied client cannot bypass the frozen generation proxy.
    /// Each successful step is persisted for the same executor. Pending kubelet
    /// termination returns an error without treating elapsed time as evidence.
    /// # Errors
    /// Rejects direct-API predecessors, unknown/restarted proxies, missing Pods,
    /// uncertain nodes, incomplete request ledgers and replaced credentials.
    pub async fn quiesce_previous(
        &self,
        transition: &str,
        executor: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.verify_new_writer_qualification(transition, executor)
            .await?;
        let result = self.quiesce_previous_inner(transition, executor).await;
        if let Err(crate::Error::Invalid(reason)) = &result {
            let registry = self.load_registry().await?;
            if let Some(stored) = registry.value.transitions.get(transition)
                && stored.record.phase == Phase::Staged
                && stored.record.executor_id == executor
            {
                self.transition_event(
                    &registry,
                    transition,
                    executor,
                    Event::Blocked {
                        event_id: format!("blocked-quiesce-{}", stored.record.verification_epoch),
                        reason: reason.clone(),
                    },
                )
                .await?;
            }
        }
        result
    }

    async fn quiesce_previous_inner(
        &self,
        transition: &str,
        executor: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let mut registry = self
            .ensure_previous_writer_checkpoint(transition, executor)
            .await?;
        let current = self
            .owning_upgrade(&registry, transition, executor, &[Phase::Staged])?
            .clone();
        let previous = self.previous_writer(&current)?.clone();
        self.verify_staged_proxy(&current).await?;
        if current.upgrade_drain.is_none() {
            registry = self
                .initialize_drain(&registry, transition, &previous)
                .await?;
        }
        let mut drain = registry
            .value
            .transition(transition)?
            .upgrade_drain
            .clone()
            .ok_or_else(|| invalid("drain checkpoint missing"))?;
        let proxy = &previous.proxy_identity;
        if drain.final_proxy_ledger.is_none() {
            self.freeze_proxy_admission(proxy).await?;
            self.mark_previous_draining(&previous).await?;
            self.wait_for_frozen_writes(proxy).await?;
            self.scale_writer_zero(&drain.runtime_writers).await?;
            let terminated = self
                .observe_writer_termination(&drain.runtime_writers)
                .await?;
            drain.runtime_termination = Some(serde_json::to_value(terminated)?);
            drain.final_proxy_ledger = Some(serde_json::to_value(
                self.freeze_and_inspect_proxy(proxy).await?,
            )?);
            registry = self
                .persist_drain(&registry, transition, drain.clone())
                .await?;
        }
        self.verify_proxy_stop_epoch(proxy, false).await?;
        if drain.proxy_termination.is_none() {
            self.scale_writer_zero(&drain.proxy_writers).await?;
            let terminated = self
                .observe_writer_termination(&drain.proxy_writers)
                .await?;
            self.verify_proxy_stop_epoch(proxy, true).await?;
            drain.proxy_termination = Some(serde_json::to_value(terminated)?);
            registry = self
                .persist_drain(&registry, transition, drain.clone())
                .await?;
        }
        self.verify_proxy_stop_epoch(proxy, true).await?;
        self.observe_writer_termination(&drain.runtime_writers)
            .await?;
        self.observe_writer_termination(&drain.proxy_writers)
            .await?;
        registry = self
            .retire_previous_identity(&registry, transition, &previous)
            .await?;
        let drain = registry
            .value
            .transition(transition)?
            .upgrade_drain
            .as_ref()
            .ok_or_else(|| invalid("drain state missing"))?;
        let evidence = TerminationEvidence {
            generation_id: previous.allocation.generation_id.clone(),
            executor_id: executor.into(),
            verification_epoch: current.record.verification_epoch,
            environment_qualification_digest: proxy.qualification_digest.clone(),
            process_termination_receipt_digest: digest(&(
                drain.runtime_termination.as_ref(),
                drain.final_proxy_ledger.as_ref(),
                drain.proxy_termination.as_ref(),
                &registry.resource_version,
            ))?,
            access_revocation_receipt_digest: digest(&(
                &drain.retired_identity_uids,
                &registry.resource_version,
            ))?,
        };
        self.transition_event(
            &registry,
            transition,
            executor,
            Event::Quiesced {
                event_id: format!("quiesce-{}", current.record.verification_epoch),
                evidence: Quiescence::OldGeneration(evidence),
            },
        )
        .await
    }

    async fn initialize_drain(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
        previous: &WriterCheckpoint,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.verify_previous_writer_proxy(previous).await?;
        let proxy = &previous.proxy_identity;
        let binding = &previous.runtime_binding;
        let runtime = self.capture_runtime_writers(previous).await?;
        let drain = UpgradeDrainState {
            previous_transition_id: previous.allocation.transition_id.clone(),
            runtime_writers: runtime,
            proxy_writers: ExpectedWriters {
                deployment_name: proxy.deployment_name.clone(),
                deployment_uid: proxy.deployment_uid.clone(),
                service_account_name: binding.service_account_name.clone(),
                pods: vec![ExpectedPod {
                    name: proxy.pod_name.clone(),
                    uid: proxy.pod_uid.clone(),
                    node_name: proxy.node_name.clone(),
                    node_uid: proxy.node_uid.clone(),
                }],
            },
            runtime_termination: None,
            final_proxy_ledger: None,
            proxy_termination: None,
            retired_identity_uids: std::collections::BTreeMap::default(),
        };
        return self.persist_drain(registry, transition, drain).await;
    }

    pub(crate) fn owning_upgrade<'a>(
        &self,
        registry: &'a Versioned<NamespaceRegistry>,
        transition: &str,
        executor: &str,
        phases: &[Phase],
    ) -> Result<&'a StoredTransition> {
        let stored = registry
            .value
            .transitions
            .get(transition)
            .ok_or_else(|| invalid("unknown transition"))?;
        if stored
            .plan
            .request
            .next
            .compiled
            .ir
            .domain
            .control_namespace
            != self.control_namespace
            || stored.plan.request.next.compiled.ir.domain.namespace != self.workload_namespace
            || stored.record.executor_id != executor
            || !phases.contains(&stored.record.phase)
            || stored.plan.request.previous.is_none()
            || deploy::plan_transition(&stored.plan.request)? != stored.plan
        {
            return Err(invalid("operation requires owning validated upgrade phase"));
        }
        verify_record(stored)?;
        Ok(stored)
    }

    async fn persist_drain(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
        drain: UpgradeDrainState,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let mut next = registry.value.clone();
        next.transitions
            .get_mut(transition)
            .ok_or_else(|| invalid("transition missing"))?
            .upgrade_drain = Some(drain);
        self.persist_registry(registry, &next).await
    }

    async fn mark_previous_draining(&self, previous: &WriterCheckpoint) -> Result<()> {
        let binding = &previous.runtime_binding;
        let name = format!("reakt-activation-{}", binding.domain_id);
        let state: Versioned<ActivationState> =
            crate::store::read_state(&self.client, &self.control_namespace, &name).await?;
        if previous.activation_uid != state.uid
            || state.value.binding != *binding
            || !matches!(
                state.value.status,
                ActivationStatus::Active | ActivationStatus::Draining
            )
        {
            return Err(invalid("old activation differs from protected generation"));
        }
        if state.value.status == ActivationStatus::Active {
            let mut next = state.value.clone();
            next.status = ActivationStatus::Draining;
            crate::store::replace_state(
                &self.client,
                &self.control_namespace,
                &name,
                &state,
                &next,
            )
            .await?;
        }
        Ok(())
    }

    pub(crate) async fn scale_writer_zero(&self, expected: &ExpectedWriters) -> Result<()> {
        let api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let mut deployment = api.get(&expected.deployment_name).await?;
        if deployment.metadata.uid.as_ref() != Some(&expected.deployment_uid)
            || deployment.metadata.deletion_timestamp.is_some()
        {
            return Err(invalid("writer Deployment UID changed"));
        }
        let spec = deployment
            .spec
            .as_mut()
            .ok_or_else(|| invalid("writer Deployment spec missing"))?;
        if !matches!(spec.replicas, Some(0 | 1)) {
            return Err(invalid("writer replica count changed"));
        }
        if spec.replicas == Some(1) {
            spec.replicas = Some(0);
            api.replace(
                &expected.deployment_name,
                &PostParams::default(),
                &deployment,
            )
            .await?;
        }
        Ok(())
    }

    async fn capture_runtime_writers(
        &self,
        previous: &WriterCheckpoint,
    ) -> Result<ExpectedWriters> {
        let binding = &previous.runtime_binding;
        let deployment_uid = previous.controller_deployment_uid.clone();
        let service_account = format!("{}-process", binding.service_account_name);
        let api: Api<Pod> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let listed = api
            .list(
                &ListParams::default()
                    .fields(&format!("spec.serviceAccountName={service_account}")),
            )
            .await?;
        if listed.items.is_empty()
            || listed.items.len() > 64
            || listed.metadata.continue_.is_some_and(|s| !s.is_empty())
        {
            return Err(invalid(
                "current tokenless controller stop set is absent or incomplete",
            ));
        }
        let nodes: Api<Node> = Api::all(self.client.clone());
        let mut pods = Vec::new();
        for pod in listed.items {
            if !pod
                .metadata
                .finalizers
                .as_ref()
                .is_some_and(|fs| fs.iter().any(|f| f == crate::TERMINATION_FINALIZER))
            {
                return Err(invalid("old controller lacks termination retention"));
            }
            let node_name = pod
                .spec
                .as_ref()
                .and_then(|s| s.node_name.clone())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("old controller node unknown"))?;
            let node = nodes.get(&node_name).await?;
            pods.push(ExpectedPod {
                name: pod
                    .metadata
                    .name
                    .ok_or_else(|| invalid("Pod name missing"))?,
                uid: pod.metadata.uid.ok_or_else(|| invalid("Pod UID missing"))?,
                node_name,
                node_uid: node
                    .metadata
                    .uid
                    .ok_or_else(|| invalid("Node UID missing"))?,
            });
        }
        Ok(ExpectedWriters {
            deployment_name: binding.service_account_name.clone(),
            deployment_uid,
            service_account_name: service_account,
            pods,
        })
    }

    async fn retire_previous_identity(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
        previous: &WriterCheckpoint,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let mut registry = registry.clone();
        let approved = crate::retirement::identities(previous);
        let mut identities: Vec<_> = approved.iter().collect();
        identities.sort_by_key(|(key, _)| key.starts_with("v1/ServiceAccount/"));
        for (key, uid) in identities {
            let parts: Vec<_> = key.split('/').collect();
            let (group, version, kind, namespace, name) = match parts.as_slice() {
                ["v1", kind, ns, name] => ("", "v1", *kind, *ns, *name),
                [group, version, kind, ns, name] => (*group, *version, *kind, *ns, *name),
                _ => return Err(invalid("recorded identity key invalid")),
            };
            let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
            let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), namespace, &ar);
            if let Some(object) = api.get_opt(name).await? {
                if object.metadata.uid.as_ref() != Some(uid) {
                    return Err(invalid("obsolete credential identity was replaced"));
                }
                api.delete(
                    name,
                    &DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: Some(uid.clone()),
                            resource_version: object.metadata.resource_version,
                        }),
                        ..Default::default()
                    },
                )
                .await?;
                if api.get_opt(name).await?.is_some() {
                    return Err(crate::pending("obsolete credential retirement pending"));
                }
            }
            let mut next = registry.value.clone();
            next.transitions
                .get_mut(transition)
                .and_then(|s| s.upgrade_drain.as_mut())
                .ok_or_else(|| invalid("drain state missing"))?
                .retired_identity_uids
                .insert(key.clone(), uid.clone());
            registry = self.persist_registry(&registry, &next).await?;
        }
        Ok(registry)
    }
}

impl PlatformClient {
    pub(crate) fn previous_writer<'a>(
        &self,
        current: &'a StoredTransition,
    ) -> Result<&'a WriterCheckpoint> {
        let metadata = current
            .plan
            .request
            .predecessor()?
            .ok_or_else(|| invalid("initial generation has no previous writer"))?;
        let checkpoint = current.previous_writer_checkpoint.as_ref().ok_or_else(|| {
            invalid("previous writer checkpoint requires explicit protected backfill")
        })?;
        checkpoint.validate(
            metadata,
            &self.control_namespace,
            &self.workload_namespace,
            &self.cluster_binding,
        )?;
        Ok(checkpoint)
    }
}

fn verify_record(stored: &StoredTransition) -> Result<()> {
    let replayed = if let Some(receipt) = stored.record.receipts.last() {
        deploy::advance(
            &stored.plan,
            &stored.record,
            stored.record.revision,
            &stored.record.executor_id,
            receipt.event.clone(),
        )?
    } else {
        deploy::begin(&stored.plan, &stored.record.executor_id)?
    };
    if replayed != stored.record {
        return Err(invalid(
            "protected transition phase differs from event history",
        ));
    }
    Ok(())
}
