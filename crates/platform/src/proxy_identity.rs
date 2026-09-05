use crate::{
    NamespaceRegistry, PlatformClient, ProxyIdentity, Result, RuntimeBinding, StoredTransition,
    Versioned, invalid,
};
use compiler_core::digest;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, ReplicaSet},
        core::v1::{Node, Pod},
    },
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
};
use kube::{Api, api::DynamicObject};

impl PlatformClient {
    pub(crate) async fn verify_proxy_stop_epoch(
        &self,
        identity: &ProxyIdentity,
        terminal: bool,
    ) -> Result<()> {
        self.verify_proxy_node(identity).await?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let pod = pods.get(&identity.pod_name).await?;
        verify_stop_epoch(&pod, identity, terminal)
    }
    pub(crate) async fn verify_proxy_origin(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
        binding: &RuntimeBinding,
        pod: &Pod,
    ) -> Result<(String, String, String)> {
        let name = format!("reakt-proxy-{}", binding.generation_id);
        let api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let deployment = api.get(&name).await?;
        let key = format!("apps/v1/Deployment/{}/{name}", self.control_namespace);
        let entry = registry.value.transition(transition)?;
        let uid = deployment
            .metadata
            .uid
            .as_ref()
            .ok_or_else(|| invalid("proxy Deployment UID missing"))?;
        let dynamic: DynamicObject = serde_json::from_value(serde_json::to_value(&deployment)?)?;
        if entry.staged_identity_uids.get(&key) != Some(uid)
            || deployment.metadata.deletion_timestamp.is_some()
            || entry.staged_content_digests.get(&key)
                != Some(&digest(&crate::controller::stable_data(&dynamic))?)
        {
            return Err(invalid("proxy Deployment differs from recorded creation"));
        }
        let owner = controller_owner(&pod.metadata, "ReplicaSet")?;
        let replicas: Api<ReplicaSet> =
            Api::namespaced(self.client.clone(), &self.control_namespace);
        let replica = replicas.get(&owner.name).await?;
        let deployment_owner = controller_owner(&replica.metadata, "Deployment")?;
        if replica.metadata.uid.as_ref() != Some(&owner.uid)
            || replica.metadata.deletion_timestamp.is_some()
            || deployment_owner.name != name
            || deployment_owner.uid != *uid
        {
            return Err(invalid(
                "proxy Pod is not controlled by the staged Deployment",
            ));
        }
        let spec = pod
            .spec
            .as_ref()
            .ok_or_else(|| invalid("proxy Pod spec missing"))?;
        let expected = deployment
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .ok_or_else(|| invalid("proxy Deployment template missing"))?;
        if spec.containers != expected.containers
            || spec.volumes != expected.volumes
            || spec.service_account_name != expected.service_account_name
            || spec.automount_service_account_token != Some(false)
            || spec.security_context != expected.security_context
            || spec.host_network == Some(true)
            || spec.host_pid == Some(true)
            || spec.host_ipc == Some(true)
            || spec.init_containers.as_ref().is_some_and(|v| !v.is_empty())
            || spec
                .ephemeral_containers
                .as_ref()
                .is_some_and(|v| !v.is_empty())
        {
            return Err(invalid(
                "proxy Pod differs from its fixed isolated template",
            ));
        }
        let node_name = spec
            .node_name
            .clone()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| invalid("proxy node missing"))?;
        let nodes: Api<Node> = Api::all(self.client.clone());
        let node = nodes.get(&node_name).await?;
        ready_node(&node)?;
        let node_uid = node
            .metadata
            .uid
            .clone()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| invalid("proxy node UID missing"))?;
        Ok((uid.clone(), node_name, node_uid))
    }

    pub(crate) async fn verify_proxy_node(&self, identity: &ProxyIdentity) -> Result<()> {
        let nodes: Api<Node> = Api::all(self.client.clone());
        let node = nodes.get(&identity.node_name).await?;
        if node.metadata.uid.as_ref() != Some(&identity.node_uid) {
            return Err(invalid("qualified proxy node UID changed"));
        }
        ready_node(&node)
    }

    pub(crate) async fn verify_staged_proxy(&self, stored: &StoredTransition) -> Result<()> {
        self.verify_runtime_identity(stored).await?;
        self.verify_controller_support(stored).await?;
        match (&stored.request_drain_qualification, &stored.proxy_identity) {
            (Some(crate::RequestDrainQualification::UnqualifiedDirectApi), None) => Ok(()),
            (
                Some(crate::RequestDrainQualification::IsolatedEnvoy {
                    qualification_digest,
                    node_image_digest,
                }),
                Some(identity),
            ) => {
                let binding = stored
                    .runtime_binding
                    .as_ref()
                    .ok_or_else(|| invalid("staged runtime binding missing"))?;
                let policy = self.load_proxy_policy(&binding.domain_id).await?;
                if digest(&policy.value)? != identity.policy_digest
                    || &policy.value.environment_qualification_digest != qualification_digest
                    || &policy.value.node_image_digest != node_image_digest
                {
                    return Err(invalid("proxy qualification changed after staging"));
                }
                self.inspect_proxy(identity).await?;
                Ok(())
            }
            _ => Err(invalid("staged transport qualification is incomplete")),
        }
    }
}

fn verify_stop_epoch(pod: &Pod, identity: &ProxyIdentity, terminal: bool) -> Result<()> {
    if pod.metadata.uid.as_ref() != Some(&identity.pod_uid)
        || digest(&pod.spec)? != identity.pod_spec_digest
    {
        return Err(invalid("stopping proxy differs from frozen Pod/spec epoch"));
    }
    let statuses = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .ok_or_else(|| invalid("proxy container states missing"))?;
    if statuses.len() != 2 {
        return Err(invalid("proxy container states changed"));
    }
    for (name, id) in [
        ("envoy", &identity.envoy_container_id),
        ("admin", &identity.admin_container_id),
    ] {
        let status = statuses
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| invalid("frozen proxy container missing"))?;
        let state = status
            .state
            .as_ref()
            .ok_or_else(|| invalid("proxy container state unknown"))?;
        if status.container_id.as_ref() != Some(id)
            || status.restart_count != 0
            || (terminal
                && state
                    .terminated
                    .as_ref()
                    .is_none_or(|s| s.finished_at.is_none() || s.container_id.as_ref() != Some(id)))
            || (!terminal && state.running.is_none() && state.terminated.is_none())
        {
            return Err(invalid(
                "proxy restarted or terminated outside frozen container epoch",
            ));
        }
    }
    Ok(())
}

fn controller_owner<'a>(
    metadata: &'a ObjectMeta,
    kind: &str,
) -> Result<&'a k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference> {
    let owners: Vec<_> = metadata
        .owner_references
        .iter()
        .flatten()
        .filter(|o| o.controller == Some(true))
        .collect();
    match owners.as_slice() {
        [owner]
            if owner.api_version == "apps/v1" && owner.kind == kind && !owner.uid.is_empty() =>
        {
            Ok(owner)
        }
        _ => Err(invalid(
            "proxy controller ownership is missing or ambiguous",
        )),
    }
}

fn ready_node(node: &Node) -> Result<()> {
    if node.metadata.deletion_timestamp.is_some()
        || !node
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
        || node
            .spec
            .as_ref()
            .and_then(|s| s.taints.as_ref())
            .is_some_and(|ts| {
                ts.iter().any(|t| {
                    matches!(
                        t.key.as_str(),
                        "node.kubernetes.io/unreachable" | "node.kubernetes.io/not-ready"
                    )
                })
            })
    {
        return Err(invalid("proxy node readiness is uncertain"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_does_not_accept_labels_or_ambiguous_controller_owners() {
        let base = json!({"labels":{"reakt.dev/proxy-generation":"g1"},"ownerReferences":[{"apiVersion":"apps/v1","kind":"ReplicaSet","name":"proxy-rs","uid":"rs-uid","controller":true}]});
        let metadata: ObjectMeta = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(
            controller_owner(&metadata, "ReplicaSet").unwrap().uid,
            "rs-uid"
        );
        for value in [
            json!({"labels":{"reakt.dev/proxy-generation":"g1"}}),
            json!({"ownerReferences":[base["ownerReferences"][0],base["ownerReferences"][0]]}),
            json!({"ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"proxy","uid":"forged","controller":true}]}),
        ] {
            assert!(
                controller_owner(&serde_json::from_value(value).unwrap(), "ReplicaSet").is_err()
            );
        }
    }

    #[test]
    fn readiness_never_treats_partition_or_missing_status_as_termination_certainty() {
        let ready = json!({"metadata":{"name":"node","uid":"node-uid"},"status":{"conditions":[{"type":"Ready","status":"True"}]}});
        assert!(ready_node(&serde_json::from_value(ready.clone()).unwrap()).is_ok());
        for value in [
            json!({"metadata":{"name":"node"}}),
            json!({"metadata":{"name":"node"},"status":{"conditions":[{"type":"Ready","status":"Unknown"}]}}),
            json!({"metadata":{"name":"node"},"status":ready["status"],"spec":{"taints":[{"key":"node.kubernetes.io/unreachable","effect":"NoExecute"}]}}),
        ] {
            assert!(ready_node(&serde_json::from_value(value).unwrap()).is_err());
        }
    }

    #[test]
    fn terminal_restarted_proxy_cannot_reuse_a_pre_restart_drain_ledger() {
        let value = json!({"metadata":{"name":"proxy","uid":"pod-uid"},"spec":{"containers":[{"name":"envoy"},{"name":"admin"}]},"status":{"containerStatuses":[{"name":"envoy","image":"envoy","imageID":"envoy-image","containerID":"containerd://envoy-first","ready":false,"restartCount":0,"state":{"terminated":{"exitCode":0,"containerID":"containerd://envoy-first","finishedAt":"2026-09-05T10:00:00Z"}}},{"name":"admin","image":"admin","imageID":"admin-image","containerID":"containerd://admin-first","ready":false,"restartCount":0,"state":{"terminated":{"exitCode":0,"containerID":"containerd://admin-first","finishedAt":"2026-09-05T10:00:00Z"}}}]}});
        let pod: Pod = serde_json::from_value(value.clone()).unwrap();
        let identity = ProxyIdentity {
            policy_digest: digest(&"policy").unwrap(),
            deployment_name: "proxy".into(),
            deployment_uid: "deployment-uid".into(),
            node_name: "node".into(),
            node_uid: "node-uid".into(),
            pod_name: "proxy".into(),
            pod_uid: "pod-uid".into(),
            pod_spec_digest: digest(&pod.spec).unwrap(),
            envoy_container_id: "containerd://envoy-first".into(),
            admin_container_id: "containerd://admin-first".into(),
            config_map_name: "config".into(),
            config_map_uid: "config-uid".into(),
            config_digest: digest(&"config").unwrap(),
            qualification_digest: digest(&"qualification").unwrap(),
        };
        assert!(verify_stop_epoch(&pod, &identity, true).is_ok());
        for (field, replacement) in [
            ("restartCount", json!(1)),
            ("containerID", json!("containerd://envoy-second")),
        ] {
            let mut changed = value.clone();
            changed["status"]["containerStatuses"][0][field] = replacement;
            assert!(
                verify_stop_epoch(&serde_json::from_value(changed).unwrap(), &identity, true)
                    .is_err()
            );
        }
        let mut changed = value;
        changed["status"]["containerStatuses"][0]["state"]["terminated"]["containerID"] =
            json!("containerd://envoy-second");
        assert!(
            verify_stop_epoch(&serde_json::from_value(changed).unwrap(), &identity, true).is_err()
        );
    }
}
