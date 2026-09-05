//! Protected shutdown identities, independent of the historical desired program.
use crate::{
    NamespaceRegistry, ProxyIdentity, RequestDrainQualification, Result, RuntimeBinding, invalid,
    valid_digest, valid_name,
};
use deploy::{GenerationAllocation, PredecessorGeneration};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const SCHEMA: &str = "reakt.dev/writer-checkpoint/v1";
const MAX_CHECKPOINT_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriterCheckpoint {
    pub schema_version: String,
    pub allocation: GenerationAllocation,
    pub control_namespace: String,
    pub runtime_binding: RuntimeBinding,
    pub request_drain_qualification: RequestDrainQualification,
    pub proxy_identity: ProxyIdentity,
    pub controller_deployment_uid: String,
    pub activation_uid: String,
    pub inventory_uid: String,
    pub retirement_identity_uids: BTreeMap<String, String>,
}

impl WriterCheckpoint {
    /// Validate protected writer identities against the current approved predecessor.
    /// # Errors
    /// Rejects changed scope/allocation, incomplete qualification, malformed identity
    /// evidence, or retirement targets beyond the authenticated predecessor metadata.
    pub fn validate(
        &self,
        metadata: &PredecessorGeneration,
        control: &str,
        workload: &str,
        cluster: &str,
    ) -> Result<()> {
        if compiler_core::canonical_bytes(self)?.len() > MAX_CHECKPOINT_BYTES {
            return Err(invalid(
                "writer checkpoint exceeds its bounded identity budget",
            ));
        }
        let binding = &self.runtime_binding;
        let allocation = &self.allocation;
        if self.schema_version != SCHEMA
            || allocation != &metadata.allocation
            || self.control_namespace != control
            || self.control_namespace != metadata.control_namespace
            || metadata.scope.namespace != workload
            || metadata.scope.cluster != cluster
            || binding.domain_id != metadata.scope.controller
            || binding.cluster_binding != cluster
            || binding.generation_id != allocation.generation_id
            || binding.service_account_name != allocation.service_account_name
            || binding.build_digest != allocation.artifact_digest
            || binding.ir_digest != allocation.ir_digest
            || !valid_name(control)
            || !valid_name(workload)
            || !valid_name(&binding.domain_id)
            || !valid_name(&allocation.generation_id)
            || !valid_name(&allocation.transition_id)
            || !valid_name(&binding.service_account_name)
        {
            return Err(invalid(
                "writer checkpoint allocation or scope differs from predecessor",
            ));
        }
        self.validate_proxy()?;
        self.validate_retirement(metadata, control)
    }

    fn validate_proxy(&self) -> Result<()> {
        let allocation = &self.allocation;
        let RequestDrainQualification::IsolatedEnvoy {
            qualification_digest,
            node_image_digest,
        } = &self.request_drain_qualification
        else {
            return Err(invalid(
                "writer checkpoint requires a qualified isolated proxy",
            ));
        };
        let proxy = &self.proxy_identity;
        if proxy.deployment_name != format!("reakt-proxy-{}", allocation.generation_id)
            || proxy.config_map_name != format!("reakt-proxy-{}-config", allocation.generation_id)
            || proxy.qualification_digest != *qualification_digest
            || !valid_name(&proxy.deployment_name)
            || !valid_name(&proxy.config_map_name)
            || !valid_name(&proxy.pod_name)
            || !valid_name(&proxy.node_name)
        {
            return Err(invalid(
                "writer checkpoint proxy identity or qualification changed",
            ));
        }
        self.validate_evidence(qualification_digest, node_image_digest)
    }

    fn validate_evidence(&self, qualification_digest: &str, node_image_digest: &str) -> Result<()> {
        let binding = &self.runtime_binding;
        let allocation = &self.allocation;
        let proxy = &self.proxy_identity;
        if ![
            &binding.build_digest,
            &binding.ir_digest,
            &binding.policy_digest,
            &binding.capability_digest,
            &proxy.policy_digest,
            &proxy.pod_spec_digest,
            &proxy.config_digest,
            &proxy.qualification_digest,
            qualification_digest,
            node_image_digest,
        ]
        .iter()
        .all(|value| valid_digest(value))
            || allocation
                .inventory_metadata_digest
                .as_ref()
                .is_none_or(|value| !valid_digest(value))
            || binding.controller_image.len() > 4096
            || !binding
                .controller_image
                .rsplit_once('@')
                .is_some_and(|(name, digest)| {
                    !name.is_empty()
                        && !name.chars().any(char::is_whitespace)
                        && valid_digest(digest)
                })
        {
            return Err(invalid("writer checkpoint digest evidence is malformed"));
        }
        if ![
            &self.controller_deployment_uid,
            &self.activation_uid,
            &self.inventory_uid,
            &proxy.deployment_uid,
            &proxy.node_uid,
            &proxy.pod_uid,
            &proxy.config_map_uid,
            &proxy.envoy_container_id,
            &proxy.admin_container_id,
        ]
        .iter()
        .all(|value| valid_uid(value))
        {
            return Err(invalid(
                "writer checkpoint lacks bounded UID or container evidence",
            ));
        }
        Ok(())
    }

    fn validate_retirement(&self, metadata: &PredecessorGeneration, control: &str) -> Result<()> {
        let allocation = &self.allocation;
        let namespaces = &metadata.retirement_role_binding_namespaces;
        if namespaces.len() > 4096
            || namespaces.windows(2).any(|pair| pair[0] >= pair[1])
            || namespaces.iter().any(|namespace| !valid_name(namespace))
        {
            return Err(invalid(
                "writer checkpoint retirement namespaces are malformed",
            ));
        }
        let mut expected: BTreeSet<_> = namespaces
            .iter()
            .map(|namespace| {
                format!(
                    "rbac.authorization.k8s.io/v1/RoleBinding/{namespace}/{}",
                    allocation.service_account_name
                )
            })
            .collect();
        expected.insert(format!(
            "v1/ServiceAccount/{control}/{}",
            allocation.service_account_name
        ));
        if self
            .retirement_identity_uids
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected
            || self
                .retirement_identity_uids
                .values()
                .any(|uid| !valid_uid(uid))
        {
            return Err(invalid(
                "writer checkpoint retirement identities differ from predecessor",
            ));
        }
        Ok(())
    }
}

fn valid_uid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

pub(crate) fn project(
    registry: &NamespaceRegistry,
    metadata: &PredecessorGeneration,
) -> Result<WriterCheckpoint> {
    let stored = crate::predecessor::authenticate(registry, metadata)?;
    let binding = stored
        .runtime_binding
        .clone()
        .ok_or_else(|| invalid("previous writer binding missing"))?;
    if binding.capability_digest
        != compiler_core::digest(&stored.plan.request.next.compiled.capabilities)?
    {
        return Err(invalid(
            "previous writer capability commitment differs from its approved plan",
        ));
    }
    let proxy = stored
        .proxy_identity
        .clone()
        .ok_or_else(|| invalid("previous generation lacks qualified proxy epoch"))?;
    let qualification = stored
        .request_drain_qualification
        .clone()
        .ok_or_else(|| invalid("previous writer qualification missing"))?;
    let control = &metadata.control_namespace;
    let name = &metadata.allocation.service_account_name;
    let required_uid = |key: &str| {
        stored
            .staged_identity_uids
            .get(key)
            .cloned()
            .ok_or_else(|| invalid("previous writer protected object UID missing"))
    };
    let retirement_identity_uids = select_retirement_identities(
        name,
        control,
        metadata
            .retirement_role_binding_namespaces
            .iter()
            .map(String::as_str),
        &stored.staged_identity_uids,
    )?;
    if required_uid(&format!(
        "apps/v1/Deployment/{control}/{}",
        proxy.deployment_name
    ))? != proxy.deployment_uid
        || required_uid(&format!("v1/ConfigMap/{control}/{}", proxy.config_map_name))?
            != proxy.config_map_uid
    {
        return Err(invalid(
            "previous proxy UIDs differ from protected staged identities",
        ));
    }
    let checkpoint = WriterCheckpoint {
        schema_version: SCHEMA.into(),
        allocation: metadata.allocation.clone(),
        control_namespace: control.clone(),
        controller_deployment_uid: required_uid(&format!("apps/v1/Deployment/{control}/{name}"))?,
        activation_uid: required_uid(&format!(
            "v1/ConfigMap/{control}/reakt-activation-{}",
            binding.domain_id
        ))?,
        inventory_uid: required_uid(&format!(
            "v1/ConfigMap/{control}/reakt-inventory-{}",
            binding.domain_id
        ))?,
        runtime_binding: binding,
        request_drain_qualification: qualification,
        proxy_identity: proxy,
        retirement_identity_uids,
    };
    checkpoint.validate(
        metadata,
        control,
        &metadata.scope.namespace,
        &metadata.scope.cluster,
    )?;
    Ok(checkpoint)
}

pub(crate) fn select_retirement_identities<'a>(
    name: &str,
    control: &str,
    namespaces: impl Iterator<Item = &'a str>,
    staged: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut expected: BTreeSet<String> = namespaces
        .map(|namespace| format!("rbac.authorization.k8s.io/v1/RoleBinding/{namespace}/{name}"))
        .collect();
    expected.insert(format!("v1/ServiceAccount/{control}/{name}"));
    if staged.keys().any(|key| {
        let parts: Vec<_> = key.split('/').collect();
        let is_binding = matches!(
            parts.as_slice(),
            [_, _, "RoleBinding", _, _] | [_, "RoleBinding", _, _]
        );
        is_binding && !expected.contains(key)
    }) {
        return Err(invalid(
            "staged RoleBinding lies outside approved generation",
        ));
    }
    expected
        .into_iter()
        .map(|key| {
            let uid = staged
                .get(&key)
                .filter(|uid| !uid.is_empty())
                .ok_or_else(|| invalid("approved retirement identity UID missing"))?;
            Ok((key, uid.clone()))
        })
        .collect()
}
