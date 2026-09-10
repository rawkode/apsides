//! Connecting to the cluster and binding the release to protected trust policy.
//!
//! Authentication comes from the invoking environment's standard Kubernetes
//! configuration, never from files or executable hooks inside the release.

use crate::Result;
use compiler_core::CompiledPlan;
use kube::{Client, Config};
use platform::{IssuedDeployment, PlatformClient};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, time::Duration};

pub(super) struct DeploymentContext {
    pub(super) issued: IssuedDeployment,
    pub(super) verified: release::IntegrityVerifiedCandidate,
    pub(super) compatibility: Value,
}

pub(super) async fn connect(
    root: &Path,
    expected_digest: Option<&str>,
) -> Result<DeploymentContext> {
    let (_, plan, discovered_manifest) = release::read_release(root)?;
    let domain = &plan.ir.domain;
    // Authentication comes from the invoking environment's standard Kubernetes
    // configuration, never from files or executable hooks inside the release.
    let mut config = Config::infer().await?;
    if config.cluster_url.scheme_str() != Some("https") || config.accept_invalid_certs {
        return Err("deployment requires an authenticated TLS Kubernetes endpoint".into());
    }
    config.default_retry = false;
    config.connect_timeout = Some(Duration::from_secs(10));
    config.read_timeout = Some(Duration::from_secs(30));
    config.write_timeout = Some(Duration::from_secs(30));
    let bootstrap = Client::try_from(config.clone())?;
    let platform = PlatformClient::new(
        bootstrap.clone(),
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )?;
    let trust = platform.load_trust_policy(&domain.id).await?;
    let policy = serde_json::from_value(trust.value)?;
    let verified = release::verify_candidate(root, &policy).await?;
    verify_artifact_binding(
        &discovered_manifest,
        verified.release_digest(),
        expected_digest,
    )?;
    let domain = &verified.compiled().ir.domain;
    let compatibility = compatibility(&bootstrap, verified.compiled()).await?;
    let coordination = crate::connection::connect_coordination().await?;
    let issued = IssuedDeployment::connect(
        config,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
        &domain.id,
    )
    .await?
    .with_stores(coordination.clone(), coordination.clone(), coordination);
    Ok(DeploymentContext {
        issued,
        verified,
        compatibility,
    })
}

fn verify_artifact_binding(
    discovered: &[u8],
    verified_digest: &str,
    requested: Option<&str>,
) -> Result<()> {
    use sha2::{Digest as _, Sha256};
    let discovered_digest = format!("sha256:{:x}", Sha256::digest(discovered));
    if discovered_digest != verified_digest {
        return Err("release changed between scope discovery and policy verification".into());
    }
    if requested.is_some_and(|expected| expected != verified_digest) {
        return Err("verified release differs from the requested OCI release".into());
    }
    Ok(())
}

async fn compatibility(client: &Client, plan: &CompiledPlan) -> Result<Value> {
    let version = client.apiserver_version().await?;
    if version.git_version != "v1.34.3" {
        return Err(format!("Kubernetes {} has no qualified absent-only SSA creation contract; supported version is v1.34.3",version.git_version).into());
    }
    let groups: BTreeSet<_> = plan
        .capabilities
        .iter()
        .map(|c| c.api_group.as_str())
        .collect();
    for group in groups {
        let resources = if group.is_empty() {
            client.list_core_api_resources("v1").await?
        } else {
            client
                .list_api_group_resources(&format!("{group}/v1"))
                .await?
        };
        for capability in plan.capabilities.iter().filter(|c| c.api_group == group) {
            let name = capability.subresource.as_ref().map_or_else(
                || capability.resource.clone(),
                |sub| format!("{}/{sub}", capability.resource),
            );
            if !resources
                .resources
                .iter()
                .any(|r| r.name == name && r.namespaced && r.verbs.contains(&capability.verb))
            {
                return Err(format!(
                    "required namespaced API capability is unavailable: {group}/{name} {}",
                    capability.verb
                )
                .into());
            }
        }
    }
    Ok(
        json!({"serverVersion":version.git_version,"requiredApis":"available","fieldValidation":"Strict","admission":"each mutation requires successful server dry-run"}),
    )
}

#[cfg(test)]
mod tests {
    use super::verify_artifact_binding;
    use sha2::{Digest as _, Sha256};

    #[test]
    fn scope_discovery_and_approved_release_must_be_the_same_snapshot() -> crate::Result<()> {
        let discovered = br#"{"scope":"first"}"#;
        let same = format!("sha256:{:x}", Sha256::digest(discovered));
        let changed = format!("sha256:{:x}", Sha256::digest(br#"{"scope":"second"}"#));
        verify_artifact_binding(discovered, &same, None)?;
        verify_artifact_binding(discovered, &same, Some(&same))?;
        assert!(verify_artifact_binding(discovered, &changed, None).is_err());
        assert!(verify_artifact_binding(discovered, &changed, Some(&changed)).is_err());
        assert!(verify_artifact_binding(discovered, &same, Some(&changed)).is_err());
        Ok(())
    }
}
