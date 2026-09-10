//! Synthetic typed maxima for sizing only. These values are never execution evidence.
use crate::{
    CapturedContainer, CapturedContainers, EvidenceProfile, ExpectedPod, ExpectedWriters,
    ProcessTerminationObservation, ProxyDrainObservation, ProxyIdentity, RequestDrainQualification,
    Result, RuntimeBinding, StoredTransition, UpgradeDrainState, WriterCheckpoint, invalid,
};
use std::collections::BTreeMap;

pub(crate) fn commitment() -> String {
    format!("sha256:{}", "f".repeat(64))
}

/// Complete JSON string width, including quotes; tiers are even and <=2050.
pub(crate) fn scalar(bytes: u16) -> Result<String> {
    let content = usize::from(bytes)
        .checked_sub(2)
        .ok_or_else(|| invalid("scalar sizing tier omitted JSON quotes"))?;
    let mut value = "\"".repeat(content / 2);
    if content % 2 != 0 {
        value.push('x');
    }
    Ok(value)
}

pub(crate) fn binding(stored: &StoredTransition) -> RuntimeBinding {
    let allocation = stored.allocation();
    RuntimeBinding {
        domain_id: stored.domain_id().into(),
        cluster_binding: stored
            .inputs
            .registry
            .claim_snapshot
            .scope
            .cluster_binding
            .clone(),
        generation_id: allocation.generation_id.clone(),
        build_digest: allocation.artifact_digest.clone(),
        ir_digest: allocation.ir_digest.clone(),
        policy_digest: stored.inputs.policy_digest.clone(),
        capability_digest: commitment(),
        service_account_name: allocation.service_account_name.clone(),
        // validate_binding permits 4096 raw bytes; escaping doubles the content.
        controller_image: "\"".repeat(4096),
    }
}

pub(crate) fn qualification() -> RequestDrainQualification {
    RequestDrainQualification::IsolatedEnvoy {
        qualification_digest: commitment(),
        node_image_digest: commitment(),
    }
}

pub(crate) use crate::capacity_checks::staged_keys;

pub(crate) fn checkpoint(
    stored: &StoredTransition,
    profile: &EvidenceProfile,
) -> Result<WriterCheckpoint> {
    let uid = scalar(profile.max_uid_json_bytes)?;
    let container = scalar(profile.max_container_id_json_bytes)?;
    let scope = &stored.inputs.registry.claim_snapshot.scope;
    let control = &scope.control_namespace;
    let allocation = stored.allocation();
    let proxy_name = format!("apsides-proxy-{}", allocation.generation_id);
    let identity = ProxyIdentity {
        policy_digest: commitment(),
        deployment_name: proxy_name.clone(),
        deployment_uid: uid.clone(),
        node_name: "n".repeat(253),
        node_uid: uid.clone(),
        pod_name: "p".repeat(253),
        pod_uid: uid.clone(),
        pod_spec_digest: commitment(),
        envoy_container_id: container.clone(),
        admin_container_id: container,
        config_map_name: format!("{proxy_name}-config"),
        config_map_uid: uid.clone(),
        config_digest: commitment(),
        qualification_digest: commitment(),
    };
    let retirement_identity_uids = staged_keys(stored)
        .into_iter()
        .filter(|key| {
            key == &format!(
                "v1/ServiceAccount/{control}/{}",
                allocation.service_account_name
            ) || key.starts_with("rbac.authorization.k8s.io/v1/RoleBinding/")
        })
        .map(|key| (key, uid.clone()))
        .collect();
    Ok(WriterCheckpoint {
        schema_version: "apsides.dev/writer-checkpoint/v1".into(),
        allocation: allocation.clone(),
        control_namespace: control.clone(),
        runtime_binding: binding(stored),
        request_drain_qualification: qualification(),
        proxy_identity: identity,
        controller_deployment_uid: uid.clone(),
        activation_uid: uid.clone(),
        inventory_uid: uid,
        retirement_identity_uids,
    })
}

fn containers(count: u16, profile: &EvidenceProfile) -> Result<Vec<CapturedContainer>> {
    let id = scalar(profile.max_container_id_json_bytes)?;
    Ok((0..count)
        .map(|_| CapturedContainer {
            name: "c".repeat(63),
            container_id: id.clone(),
        })
        .collect())
}

fn runtime_writers(
    previous: &WriterCheckpoint,
    profile: &EvidenceProfile,
) -> Result<ExpectedWriters> {
    let uid = scalar(profile.max_uid_json_bytes)?;
    let mut pods = Vec::new();
    for index in 0..profile.max_controller_pods {
        // With fixed total category counts, concentrating optional entries maximizes
        // separators. Every Pod retains its mandatory regular container.
        let regular = if index == 0 {
            profile.max_regular_containers - profile.max_controller_pods + 1
        } else {
            1
        };
        pods.push(ExpectedPod {
            name: "p".repeat(253),
            uid: uid.clone(),
            node_name: "n".repeat(253),
            node_uid: uid.clone(),
            containers: CapturedContainers {
                regular: containers(regular, profile)?,
                init: containers(
                    if index == 0 {
                        profile.max_init_containers
                    } else {
                        0
                    },
                    profile,
                )?,
                ephemeral: containers(
                    if index == 0 {
                        profile.max_ephemeral_containers
                    } else {
                        0
                    },
                    profile,
                )?,
            },
        });
    }
    Ok(ExpectedWriters {
        deployment_name: previous.runtime_binding.service_account_name.clone(),
        deployment_uid: previous.controller_deployment_uid.clone(),
        service_account_name: format!("{}-process", previous.runtime_binding.service_account_name),
        pods,
    })
}

fn termination(
    writers: &ExpectedWriters,
    profile: &EvidenceProfile,
) -> Result<ProcessTerminationObservation> {
    let version = scalar(profile.max_resource_version_json_bytes)?;
    let container_ids = writers
        .pods
        .iter()
        .flat_map(|pod| {
            pod.containers
                .regular
                .iter()
                .chain(&pod.containers.init)
                .chain(&pod.containers.ephemeral)
                .map(|container| container.container_id.clone())
        })
        .collect();
    Ok(ProcessTerminationObservation {
        deployment_uid: writers.deployment_uid.clone(),
        deployment_resource_version: version.clone(),
        pod_resource_versions: writers
            .pods
            .iter()
            .map(|pod| (pod.uid.clone(), version.clone()))
            .collect(),
        container_ids,
    })
}

pub(crate) fn drain(
    previous: &WriterCheckpoint,
    profile: &EvidenceProfile,
) -> Result<UpgradeDrainState> {
    let proxy = &previous.proxy_identity;
    let runtime_writers = runtime_writers(previous, profile)?;
    let proxy_writers = ExpectedWriters {
        deployment_name: proxy.deployment_name.clone(),
        deployment_uid: proxy.deployment_uid.clone(),
        service_account_name: previous.runtime_binding.service_account_name.clone(),
        pods: vec![ExpectedPod {
            name: proxy.pod_name.clone(),
            uid: proxy.pod_uid.clone(),
            node_name: proxy.node_name.clone(),
            node_uid: proxy.node_uid.clone(),
            containers: CapturedContainers {
                regular: vec![
                    CapturedContainer {
                        name: "envoy".into(),
                        container_id: proxy.envoy_container_id.clone(),
                    },
                    CapturedContainer {
                        name: "admin".into(),
                        container_id: proxy.admin_container_id.clone(),
                    },
                ],
                init: vec![],
                ephemeral: vec![],
            },
        }],
    };
    Ok(UpgradeDrainState {
        previous_transition_id: previous.allocation.transition_id.clone(),
        runtime_termination: Some(termination(&runtime_writers, profile)?),
        proxy_termination: Some(termination(&proxy_writers, profile)?),
        runtime_writers,
        proxy_writers,
        final_proxy_ledger: Some(ProxyDrainObservation {
            pod_uid: proxy.pod_uid.clone(),
            pod_resource_version: scalar(profile.max_resource_version_json_bytes)?,
            container_id: proxy.envoy_container_id.clone(),
            qualification_digest: commitment(),
            ledger_digest: commitment(),
            // The canonical JSON boundary rejects larger numeric values.
            completed_requests: 9_007_199_254_740_991,
            rejected_after_freeze: 9_007_199_254_740_991,
        }),
        retired_identity_uids: previous.retirement_identity_uids.clone(),
    })
}

pub(crate) fn populate(
    stored: &mut StoredTransition,
    profile: &EvidenceProfile,
) -> Result<WriterCheckpoint> {
    let checkpoint = checkpoint(stored, profile)?;
    let uid = scalar(profile.max_uid_json_bytes)?;
    stored.staged_identity_uids = staged_keys(stored)
        .into_iter()
        .map(|key| (key, uid.clone()))
        .collect();
    stored.staged_content_digests = staged_keys(stored)
        .into_iter()
        .filter(|key| !key.starts_with("v1/Secret/"))
        .map(|key| (key, commitment()))
        .collect::<BTreeMap<_, _>>();
    stored.runtime_binding = Some(checkpoint.runtime_binding.clone());
    stored.staged_trust_policy_digest = Some(commitment());
    stored.staged_provenance_digest = Some(commitment());
    stored.request_drain_qualification = Some(qualification());
    stored.proxy_identity = Some(checkpoint.proxy_identity.clone());
    stored.upgrade_drain = stored
        .previous_writer_checkpoint
        .as_ref()
        .map(|previous| drain(previous, profile))
        .transpose()?;
    Ok(checkpoint)
}
