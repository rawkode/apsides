//! Live pre-quiescence identity capture. Capacity profiles never substitute for this evidence.
use crate::{
    CapturedContainer, CapturedContainers, EvidenceProfile, ExpectedPod, ExpectedWriters,
    PlatformClient, Result, WriterCheckpoint, invalid,
};
use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet},
    core::v1::{ContainerStatus, Node, Pod},
};
use kube::{Api, api::ListParams};
use std::collections::BTreeSet;

impl PlatformClient {
    pub(crate) async fn capture_runtime_topology(
        &self,
        previous: &WriterCheckpoint,
    ) -> Result<ExpectedWriters> {
        let first = self.capture_runtime_snapshot(previous, false).await?;
        let second = self.capture_runtime_snapshot(previous, false).await?;
        if first != second {
            return Err(invalid(
                "writer topology changed during authenticated capture; retry before quiescence",
            ));
        }
        Ok(first)
    }

    pub(crate) async fn verify_runtime_topology(
        &self,
        previous: &WriterCheckpoint,
        expected: &ExpectedWriters,
    ) -> Result<()> {
        for _ in 0..2 {
            if self.capture_runtime_snapshot(previous, true).await? != *expected {
                return Err(invalid(
                    "captured writer topology changed; explicit recovery required",
                ));
            }
        }
        Ok(())
    }

    async fn capture_runtime_snapshot(
        &self,
        previous: &WriterCheckpoint,
        allow_stopped: bool,
    ) -> Result<ExpectedWriters> {
        let name = &previous.runtime_binding.service_account_name;
        let account = format!("{name}-process");
        let deployments: Api<Deployment> =
            Api::namespaced(self.client.clone(), &self.control_namespace);
        let deployment = deployments.get(name).await?;
        verify_controller(&deployment, previous, &account, allow_stopped)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let listed = pods
            .list(&ListParams::default().fields(&format!("spec.serviceAccountName={account}")))
            .await?;
        if listed.items.is_empty()
            || listed.items.len() > 64
            || listed
                .metadata
                .continue_
                .is_some_and(|cursor| !cursor.is_empty())
        {
            return Err(invalid(
                "complete retained writer topology is absent or exceeds64 Pods",
            ));
        }
        let replicas: Api<ReplicaSet> =
            Api::namespaced(self.client.clone(), &self.control_namespace);
        let nodes: Api<Node> = Api::all(self.client.clone());
        let mut captured = Vec::new();
        for pod in listed.items {
            let replica_owner = owner(&pod.metadata, "ReplicaSet")?;
            let replica = replicas.get(&replica_owner.name).await?;
            let deployment_owner = owner(&replica.metadata, "Deployment")?;
            if replica.metadata.uid.as_ref() != Some(&replica_owner.uid)
                || replica.metadata.deletion_timestamp.is_some()
                || deployment_owner.name != *name
                || deployment_owner.uid != previous.controller_deployment_uid
                || !pod.metadata.finalizers.as_ref().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item == crate::TERMINATION_FINALIZER)
                })
            {
                return Err(invalid(
                    "writer Pod ownership or termination retention differs from protected Deployment",
                ));
            }
            let spec = pod
                .spec
                .as_ref()
                .ok_or_else(|| invalid("writer Pod spec missing"))?;
            if spec.service_account_name.as_ref() != Some(&account) {
                return Err(invalid("writer Pod service account changed"));
            }
            let node_name = spec
                .node_name
                .as_ref()
                .ok_or_else(|| invalid("writer node absent"))?;
            let node = nodes.get(node_name).await?;
            let status = pod
                .status
                .as_ref()
                .ok_or_else(|| invalid("writer container identity not observable"))?;
            captured.push(ExpectedPod {
                name: required(pod.metadata.name.as_deref())?,
                uid: required(pod.metadata.uid.as_deref())?,
                node_name: node_name.clone(),
                node_uid: required(node.metadata.uid.as_deref())?,
                containers: CapturedContainers {
                    regular: capture_containers(
                        spec.containers
                            .iter()
                            .map(|container| container.name.as_str()),
                        status.container_statuses.as_deref(),
                    )?,
                    init: capture_containers(
                        spec.init_containers
                            .iter()
                            .flatten()
                            .map(|container| container.name.as_str()),
                        status.init_container_statuses.as_deref(),
                    )?,
                    ephemeral: capture_containers(
                        spec.ephemeral_containers
                            .iter()
                            .flatten()
                            .map(|container| container.name.as_str()),
                        status.ephemeral_container_statuses.as_deref(),
                    )?,
                },
            });
        }
        captured.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(ExpectedWriters {
            deployment_name: name.clone(),
            deployment_uid: previous.controller_deployment_uid.clone(),
            service_account_name: account,
            pods: captured,
        })
    }

    pub(crate) async fn select_evidence_profile(
        &self,
        previous: Option<&WriterCheckpoint>,
    ) -> Result<EvidenceProfile> {
        let Some(previous) = previous else {
            return Ok(crate::completion_envelope::initial_profile());
        };
        self.require_active_predecessor(previous).await?;
        self.verify_previous_writer_proxy(previous).await?;
        let writers = self.capture_runtime_topology(previous).await?;
        let mut profile = crate::completion_envelope::initial_profile();
        profile.max_controller_pods =
            u16::try_from(writers.pods.len()).map_err(|_| invalid("writer count overflow"))?;
        profile.max_regular_containers =
            category_count(&writers, |containers| &containers.regular)?;
        profile.max_init_containers = category_count(&writers, |containers| &containers.init)?;
        profile.max_ephemeral_containers =
            category_count(&writers, |containers| &containers.ephemeral)?;
        for uid_tier in [128, 512, 2050] {
            for container_tier in [256, 1024, 2050] {
                profile.max_uid_json_bytes = uid_tier;
                profile.max_container_id_json_bytes = container_tier;
                if profile.validate_writers(&writers).is_ok()
                    && [
                        &previous.controller_deployment_uid,
                        &previous.activation_uid,
                        &previous.inventory_uid,
                        &previous.proxy_identity.deployment_uid,
                        &previous.proxy_identity.node_uid,
                        &previous.proxy_identity.pod_uid,
                        &previous.proxy_identity.config_map_uid,
                    ]
                    .iter()
                    .all(|uid| profile.validate_uid(uid).is_ok())
                    && previous
                        .retirement_identity_uids
                        .values()
                        .all(|uid| profile.validate_uid(uid).is_ok())
                    && [
                        &previous.proxy_identity.envoy_container_id,
                        &previous.proxy_identity.admin_container_id,
                    ]
                    .iter()
                    .all(|id| profile.validate_container_id(id).is_ok())
                {
                    return Ok(profile);
                }
            }
        }
        Err(invalid(
            "live writer topology has no supported bounded evidence profile",
        ))
    }
}

fn required(value: Option<&str>) -> Result<String> {
    value
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid("captured writer metadata missing"))
}

fn owner<'a>(
    metadata: &'a kube::core::ObjectMeta,
    kind: &str,
) -> Result<&'a k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference> {
    let mut owners = metadata
        .owner_references
        .iter()
        .flatten()
        .filter(|owner| owner.controller == Some(true));
    let found = owners
        .next()
        .ok_or_else(|| invalid("writer owner chain missing"))?;
    if owners.next().is_some()
        || found.kind != kind
        || found.api_version != "apps/v1"
        || found.uid.is_empty()
        || !crate::valid_name(&found.name)
    {
        return Err(invalid("writer owner chain is ambiguous or malformed"));
    }
    Ok(found)
}

fn capture_containers<'a>(
    names: impl Iterator<Item = &'a str>,
    statuses: Option<&[ContainerStatus]>,
) -> Result<Vec<CapturedContainer>> {
    let expected = names.collect::<BTreeSet<_>>();
    let statuses = statuses.unwrap_or_default();
    if expected.len() != statuses.len() {
        return Err(invalid("complete current container identities unavailable"));
    }
    let mut captured = Vec::new();
    let mut seen = BTreeSet::new();
    for status in statuses {
        if !expected.contains(status.name.as_str()) || !seen.insert(&status.name) {
            return Err(invalid("container topology differs from Pod spec"));
        }
        captured.push(CapturedContainer {
            name: status.name.clone(),
            container_id: required(status.container_id.as_deref())?,
        });
    }
    captured.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(captured)
}

fn category_count(
    writers: &ExpectedWriters,
    category: impl Fn(&CapturedContainers) -> &[CapturedContainer],
) -> Result<u16> {
    let count = writers.pods.iter().try_fold(0_usize, |total, pod| {
        total
            .checked_add(category(&pod.containers).len())
            .ok_or_else(|| invalid("container count overflow"))
    })?;
    u16::try_from(count).map_err(|_| invalid("container count exceeds supported profile"))
}

fn verify_controller(
    deployment: &Deployment,
    previous: &WriterCheckpoint,
    account: &str,
    allow_stopped: bool,
) -> Result<()> {
    if deployment.metadata.uid.as_ref() != Some(&previous.controller_deployment_uid)
        || deployment.metadata.deletion_timestamp.is_some()
        || deployment.spec.as_ref().is_none_or(|spec| {
            !(spec.replicas == Some(1) || (allow_stopped && spec.replicas == Some(0)))
                || spec
                    .template
                    .spec
                    .as_ref()
                    .is_none_or(|pod| pod.service_account_name.as_deref() != Some(account))
        })
    {
        return Err(invalid(
            "pre-quiescence controller deployment identity changed",
        ));
    }
    Ok(())
}
