//! Structural admission checks shared by generic loads and selected lifecycle work.
use crate::{Result, StoredTransition, invalid};
use std::collections::BTreeSet;

pub(crate) fn validate(stored: &StoredTransition) -> Result<()> {
    stored.capacity.validate()?;
    if stored.record.receipts.len() > 40
        || stored
            .record
            .receipts
            .iter()
            .filter(|receipt| matches!(receipt.event, deploy::Event::Resume { .. }))
            .count()
            > 8
    {
        return Err(invalid(
            "transition exceeds its admitted recovery allowance; explicit recovery required",
        ));
    }
    let profile = &stored.capacity.profile;
    let keys = staged_keys(stored).into_iter().collect::<BTreeSet<_>>();
    for (key, uid) in &stored.staged_identity_uids {
        if !keys.contains(key) {
            return Err(invalid(
                "staged identity is outside the bounded support set",
            ));
        }
        profile.validate_uid(uid)?;
    }
    for (key, digest) in &stored.staged_content_digests {
        if !keys.contains(key) || key.starts_with("v1/Secret/") || !crate::valid_digest(digest) {
            return Err(invalid(
                "staged content commitment is outside the bounded support set",
            ));
        }
    }
    if let Some(binding) = &stored.runtime_binding {
        crate::runtime::validate_binding(binding)?;
    }
    if let Some(proxy) = &stored.proxy_identity {
        let name = format!("apsides-proxy-{}", stored.allocation().generation_id);
        if proxy.deployment_name != name
            || proxy.config_map_name != format!("{name}-config")
            || !crate::valid_name(&proxy.node_name)
            || !crate::valid_name(&proxy.pod_name)
            || [
                &proxy.policy_digest,
                &proxy.pod_spec_digest,
                &proxy.config_digest,
                &proxy.qualification_digest,
            ]
            .iter()
            .any(|value| !crate::valid_digest(value))
        {
            return Err(invalid("proxy metadata exceeds its derived capacity shape"));
        }
        for uid in [
            &proxy.deployment_uid,
            &proxy.node_uid,
            &proxy.pod_uid,
            &proxy.config_map_uid,
        ] {
            profile.validate_uid(uid)?;
        }
        profile.validate_container_id(&proxy.envoy_container_id)?;
        profile.validate_container_id(&proxy.admin_container_id)?;
    }
    for value in [
        &stored.staged_trust_policy_digest,
        &stored.staged_provenance_digest,
    ]
    .into_iter()
    .flatten()
    {
        if !crate::valid_digest(value) {
            return Err(invalid("staged commitment encoding invalid"));
        }
    }
    if let Some(crate::RequestDrainQualification::IsolatedEnvoy {
        qualification_digest,
        node_image_digest,
    }) = &stored.request_drain_qualification
        && (!crate::valid_digest(qualification_digest) || !crate::valid_digest(node_image_digest))
    {
        return Err(invalid("qualification metadata encoding invalid"));
    }
    if let Some(drain) = &stored.upgrade_drain {
        profile.validate_drain(drain)?;
    }
    Ok(())
}

pub(crate) fn staged_keys(stored: &StoredTransition) -> Vec<String> {
    let scope = &stored.inputs.registry.claim_snapshot.scope;
    let control = &scope.control_namespace;
    let name = &stored.allocation().service_account_name;
    let proxy = format!("apsides-proxy-{}", stored.allocation().generation_id);
    let mut keys = vec![
        format!("v1/ServiceAccount/{control}/{name}"),
        format!("v1/ServiceAccount/{control}/{name}-process"),
        format!("v1/Secret/{control}/{name}-tls"),
        format!("v1/ConfigMap/{control}/{name}-config"),
        format!("apps/v1/Deployment/{control}/{name}"),
        format!("v1/ConfigMap/{control}/{proxy}-config"),
        format!("apps/v1/Deployment/{control}/{proxy}"),
        format!("v1/Service/{control}/{proxy}"),
        format!(
            "v1/ConfigMap/{control}/apsides-activation-{}",
            stored.domain_id()
        ),
        format!(
            "v1/ConfigMap/{control}/apsides-inventory-{}",
            stored.domain_id()
        ),
    ];
    for namespace in [control, &scope.workload_namespace] {
        for kind in ["Role", "RoleBinding"] {
            keys.push(format!(
                "rbac.authorization.k8s.io/v1/{kind}/{namespace}/{name}"
            ));
        }
    }
    keys
}
