use crate::{PlatformClient, ProcessTerminationObservation, Result, invalid, valid_name};
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ContainerStatus, Node, Pod},
};
use kube::{Api, api::ListParams};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const TERMINATION_FINALIZER: &str = "reakt.dev/termination-evidence";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedPod {
    pub name: String,
    pub uid: String,
    pub node_name: String,
    pub node_uid: String,
    pub containers: crate::CapturedContainers,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedWriters {
    pub deployment_name: String,
    pub deployment_uid: String,
    pub service_account_name: String,
    /// Complete platform-recorded Pod identities. A missing record is not
    /// inferred from an empty live list or runtime-written inventory.
    pub pods: Vec<ExpectedPod>,
}

impl PlatformClient {
    /// Inspect kubelet terminal states for retained known Pod UIDs after the
    /// generation Deployment has observed replicas zero. Missing Pods, unknown
    /// containers, changed nodes or unreachable nodes fail closed.
    /// # Errors
    /// Rejects incomplete process evidence. Success is not request-drain proof
    /// and cannot authorize activation by itself.
    pub async fn observe_writer_termination(
        &self,
        expected: &ExpectedWriters,
    ) -> Result<ProcessTerminationObservation> {
        if !valid_name(&expected.deployment_name)
            || !valid_name(&expected.service_account_name)
            || expected.deployment_uid.is_empty()
            || expected.pods.is_empty()
            || expected.pods.len() > 64
        {
            return Err(invalid(
                "termination requires bounded complete writer identities",
            ));
        }
        let deployments: Api<Deployment> =
            Api::namespaced(self.client.clone(), &self.control_namespace);
        let deployment = deployments.get(&expected.deployment_name).await?;
        verify_stopped_deployment(&deployment, expected)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let listed = pods
            .list(&ListParams::default().fields(&format!(
                "spec.serviceAccountName={}",
                expected.service_account_name
            )))
            .await?;
        if listed
            .metadata
            .continue_
            .as_ref()
            .is_some_and(|v| !v.is_empty())
            || listed.items.len() != expected.pods.len()
        {
            return Err(invalid(
                "live writer set differs from complete protected Pod history",
            ));
        }
        let mut versions = Vec::new();
        let mut container_ids = Vec::new();
        let mut seen = BTreeSet::new();
        for expected_pod in &expected.pods {
            if !seen.insert(&expected_pod.uid) {
                return Err(invalid("duplicate protected Pod UID"));
            }
            let pod = listed
                .items
                .iter()
                .find(|pod| pod.metadata.name.as_deref() == Some(&expected_pod.name))
                .ok_or_else(|| {
                    invalid("protected writer Pod disappeared; termination is unknown")
                })?;
            inspect_pod(
                &self.client,
                pod,
                expected_pod,
                &expected.service_account_name,
                &mut container_ids,
            )
            .await?;
            versions.push((
                expected_pod.uid.clone(),
                pod.metadata
                    .resource_version
                    .clone()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("writer Pod omitted resourceVersion"))?,
            ));
        }
        let after = deployments.get(&expected.deployment_name).await?;
        verify_stopped_deployment(&after, expected)?;
        if after.metadata.generation != deployment.metadata.generation {
            return Err(invalid(
                "writer Deployment changed during termination inspection",
            ));
        }
        Ok(ProcessTerminationObservation {
            deployment_uid: expected.deployment_uid.clone(),
            deployment_resource_version: after
                .metadata
                .resource_version
                .ok_or_else(|| invalid("writer Deployment omitted resourceVersion"))?,
            pod_resource_versions: versions,
            container_ids,
        })
    }
}

async fn inspect_pod(
    client: &kube::Client,
    pod: &Pod,
    expected_pod: &ExpectedPod,
    service_account_name: &str,
    container_ids: &mut Vec<String>,
) -> Result<()> {
    if pod.metadata.uid.as_deref() != Some(&expected_pod.uid)
        || !pod
            .metadata
            .finalizers
            .as_ref()
            .is_some_and(|f| f.iter().any(|f| f == TERMINATION_FINALIZER))
    {
        return Err(invalid("writer Pod UID/finalizer changed"));
    }
    let spec = pod
        .spec
        .as_ref()
        .ok_or_else(|| invalid("writer Pod lacks spec"))?;
    if spec.node_name.as_deref() != Some(&expected_pod.node_name)
        || spec.service_account_name.as_deref() != Some(service_account_name)
    {
        return Err(invalid("writer Pod node or identity changed"));
    }
    verify_node(client, expected_pod).await?;
    let status = pod
        .status
        .as_ref()
        .ok_or_else(|| invalid("writer Pod lacks terminal status"))?;
    if !matches!(status.phase.as_deref(), Some("Succeeded" | "Failed")) {
        return Err(crate::pending("writer Pod has not terminated yet"));
    }
    terminated_containers(
        spec.containers.iter().map(|c| c.name.as_str()),
        status.container_statuses.as_deref(),
        &expected_pod.containers.regular,
        container_ids,
    )?;
    terminated_containers(
        spec.init_containers
            .iter()
            .flatten()
            .map(|c| c.name.as_str()),
        status.init_container_statuses.as_deref(),
        &expected_pod.containers.init,
        container_ids,
    )?;
    terminated_containers(
        spec.ephemeral_containers
            .iter()
            .flatten()
            .map(|c| c.name.as_str()),
        status.ephemeral_container_statuses.as_deref(),
        &expected_pod.containers.ephemeral,
        container_ids,
    )?;
    Ok(())
}

async fn verify_node(client: &kube::Client, expected_pod: &ExpectedPod) -> Result<()> {
    let nodes: Api<Node> = Api::all(client.clone());
    let node = nodes.get(&expected_pod.node_name).await?;
    if node.metadata.uid.as_deref() != Some(&expected_pod.node_uid)
        || node.metadata.deletion_timestamp.is_some()
        || !node
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            })
        || node
            .spec
            .as_ref()
            .and_then(|s| s.taints.as_ref())
            .is_some_and(|taints| {
                taints.iter().any(|t| {
                    matches!(
                        t.key.as_str(),
                        "node.kubernetes.io/unreachable"
                            | "node.kubernetes.io/not-ready"
                            | "node.kubernetes.io/out-of-service"
                    )
                })
            })
    {
        return Err(invalid(
            "writer node is unavailable or changed; process termination is unqualified",
        ));
    }
    Ok(())
}

fn verify_stopped_deployment(deployment: &Deployment, expected: &ExpectedWriters) -> Result<()> {
    let spec = deployment
        .spec
        .as_ref()
        .ok_or_else(|| invalid("writer Deployment lacks spec"))?;
    let status = deployment
        .status
        .as_ref()
        .ok_or_else(|| invalid("writer Deployment lacks observed status"))?;
    if deployment.metadata.uid.as_deref() != Some(&expected.deployment_uid)
        || spec.replicas != Some(0)
        || deployment
            .metadata
            .generation
            .is_none_or(|generation| generation < 1)
        || !spec
            .template
            .metadata
            .as_ref()
            .and_then(|m| m.finalizers.as_ref())
            .is_some_and(|f| f.iter().any(|f| f == TERMINATION_FINALIZER))
    {
        return Err(invalid(
            "writer Deployment has not observed a protected zero-replica state",
        ));
    }
    // Kubernetes DeploymentStatus.replicas is int32 with json omitempty:
    // an omitted value represents zero, but observedGeneration is still required.
    if status.replicas.unwrap_or_default() != 0
        || status
            .observed_generation
            .is_none_or(|observed| observed < deployment.metadata.generation.unwrap_or(1))
    {
        return Err(crate::pending(
            "writer Deployment has not observed zero replicas yet",
        ));
    }
    Ok(())
}

fn terminated_containers<'a>(
    names: impl Iterator<Item = &'a str>,
    statuses: Option<&[ContainerStatus]>,
    captured: &[crate::CapturedContainer],
    ids: &mut Vec<String>,
) -> Result<()> {
    let expected: BTreeSet<_> = names.collect();
    let statuses = statuses.unwrap_or_default();
    if statuses.len() != expected.len()
        || captured.len() != expected.len()
        || captured
            .iter()
            .any(|container| !expected.contains(container.name.as_str()))
    {
        return Err(invalid(
            "writer container termination evidence is incomplete",
        ));
    }
    let mut seen = BTreeSet::new();
    for status in statuses {
        if !expected.contains(status.name.as_str()) || !seen.insert(status.name.as_str()) {
            return Err(invalid("unknown writer container status"));
        }
        let terminal = status
            .state
            .as_ref()
            .and_then(|s| s.terminated.as_ref())
            .ok_or_else(|| invalid("writer container is not terminated"))?;
        if terminal.finished_at.is_none()
            || terminal.reason.as_deref() == Some("ContainerStatusUnknown")
            || terminal.container_id.as_ref().is_none_or(String::is_empty)
            || !captured.iter().any(|container| {
                container.name == status.name
                    && terminal.container_id.as_ref() == Some(&container.container_id)
            })
        {
            return Err(invalid("writer container termination is unqualified"));
        }
        ids.push(
            terminal
                .container_id
                .clone()
                .ok_or_else(|| invalid("writer container lacks identity"))?,
        );
    }
    Ok(())
}
