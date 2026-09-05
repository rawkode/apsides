use crate::{Result, only};
use compiler_core::CompiledPlan;
use deploy::{DeploymentRequest, GenerationPlan, Phase, RegistrySnapshot};
use kube::{Client, Config, config::AuthInfo};
use platform::PlatformClient;
use serde_json::{Value, json};
use std::io::Write as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

pub fn command(options: &BTreeMap<String, String>) -> Result<()> {
    only(
        options,
        &[
            "--artifact",
            "--artifact-reference",
            "--transition",
            "--executor",
            "--ownership-intents",
        ],
    )?;
    let runtime = tokio::runtime::Runtime::new()?;
    let report = runtime.block_on(deploy(options))?;
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(&report)?
    )?;
    Ok(())
}

async fn deploy(options: &BTreeMap<String, String>) -> Result<Value> {
    platform::install_rustls_ring_provider()?;
    let inventory = crate::inventory_connection::connect().await?;
    let artifact = crate::artifact_input::open(options).await?;
    let root = artifact.path();
    let DeploymentContext {
        platform,
        verified,
        compatibility,
        expires_at,
    } = connect(root, artifact.expected_digest()).await?;
    let domain = &verified.compiled().ir.domain;
    let executor = executor_id(options)?;
    let registry = platform.load_registry().await?;
    let policy_digest = compiler_core::digest(&platform.load_policy(&domain.id).await?.value)?;
    let transition_id = options.get("--transition").cloned().unwrap_or_else(|| {
        default_transition(
            &registry.value,
            &domain.id,
            verified.release_digest(),
            &policy_digest,
        )
    });
    verify_resumed_intents(
        options.get("--ownership-intents"),
        registry.value.transitions.get(&transition_id),
    )?;
    match check_existing(
        &registry.value,
        &transition_id,
        &verified,
        &executor,
        &policy_digest,
    )? {
        ExistingTransition::Committed => {
            let tracking = crate::inventory_connection::publish(
                &platform,
                inventory.as_ref(),
                &registry.value,
                &transition_id,
            )
            .await?;
            let report = json!({"status":"already-committed","transition":transition_id,"releaseDigest":verified.release_digest(),"inventory":{"revision":tracking.revision,"snapshotId":tracking.snapshot_id}});
            return Ok(report);
        }
        ExistingTransition::Resume => {
            let stored = registry.value.transition(&transition_id)?;
            crate::inventory_connection::require_request(inventory.as_ref(), &stored.plan.request)
                .await?;
        }
        ExistingTransition::Absent => {
            reserve_new_transition(
                &platform,
                inventory.as_ref(),
                registry,
                &verified,
                &transition_id,
                &executor,
                options,
            )
            .await?;
        }
    }
    let prepared = platform.load_registry().await?;
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        serde_json::to_string_pretty(&crate::review::transition_review(
            &prepared.value,
            &transition_id,
        )?)?
    );
    let committed = drive_transition(&platform, &verified, &transition_id, &executor).await?;
    let tracking = crate::inventory_connection::publish(
        &platform,
        inventory.as_ref(),
        &committed.value,
        &transition_id,
    )
    .await?;
    deployment_report(
        &committed,
        &transition_id,
        &verified,
        &compatibility,
        &expires_at,
        &tracking,
    )
}

fn verify_resumed_intents(
    names: Option<&String>,
    stored: Option<&platform::StoredTransition>,
) -> Result<()> {
    if let (Some(names), Some(stored)) = (names, stored) {
        let supplied: BTreeSet<_> = names.split(',').collect();
        let recorded: BTreeSet<_> = stored
            .plan
            .request
            .ownership_intents
            .iter()
            .map(|i| i.object_name.as_str())
            .collect();
        if supplied != recorded {
            return Err("ownership intents differ from the recorded transition; approvals cannot be replaced during resume".into());
        }
    }
    Ok(())
}

async fn reserve_new_transition(
    platform: &PlatformClient,
    inventory: &dyn inventory::InventoryStore,
    mut registry: platform::Versioned<platform::NamespaceRegistry>,
    verified: &release::VerifiedRelease,
    transition_id: &str,
    executor: &str,
    options: &BTreeMap<String, String>,
) -> Result<()> {
    let domain = &verified.compiled().ir.domain;

    let active = registry
        .value
        .histories
        .get(&domain.id)
        .and_then(|history| history.active_generation_id.as_deref());
    crate::inventory_connection::require_head(inventory, domain, active).await?;
    if registry
        .value
        .histories
        .get(&domain.id)
        .is_some_and(|h| h.active_generation_id.is_some())
    {
        registry = crate::inventory_connection::capture_active(platform, inventory, domain).await?;
    }
    let request = prepare_request(
        platform,
        inventory,
        &registry,
        verified,
        transition_id,
        options.get("--ownership-intents").map(String::as_str),
    )
    .await?;
    crate::inventory_connection::require_request(inventory, &request).await?;
    platform.reserve_transition(&request, executor).await?;
    Ok(())
}

struct DeploymentContext {
    platform: PlatformClient,
    verified: release::VerifiedRelease,
    compatibility: Value,
    expires_at: String,
}

async fn connect(root: &Path, expected_digest: Option<&str>) -> Result<DeploymentContext> {
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
    let verified = release::verify_release(root, &policy).await?;
    verify_artifact_binding(
        &discovered_manifest,
        verified.release_digest(),
        expected_digest,
    )?;
    let domain = &verified.compiled().ir.domain;
    let compatibility = compatibility(&bootstrap, verified.compiled()).await?;
    let credential = platform.approved_deployment_credential(&domain.id).await?;
    let expires_at = credential.expires_at.clone();
    // Discard all inherited client keys, exec/auth providers, token paths and
    // impersonation settings. Only this short-lived, UID-bound bearer remains.
    config.auth_info = AuthInfo {
        token: Some(credential.token),
        ..AuthInfo::default()
    };
    config.headers.clear();
    let client = Client::try_from(config)?;
    let platform = PlatformClient::new(
        client,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )?;
    Ok(DeploymentContext {
        platform,
        verified,
        compatibility,
        expires_at,
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

fn deployment_report(
    committed: &platform::Versioned<platform::NamespaceRegistry>,
    transition_id: &str,
    verified: &release::VerifiedRelease,
    compatibility: &Value,
    expires_at: &str,
    tracking: &inventory::VersionedInventory,
) -> Result<Value> {
    let stored = committed.value.transition(transition_id)?;
    Ok(
        json!({"status":"committed","transition":transition_id,"generation":stored.plan.request.next.generation_id,
        "releaseDigest":verified.release_digest(),"irDigest":verified.compiled().ir_digest,"compatibility":compatibility,
        "inventory":{"revision":tracking.revision,"snapshotId":tracking.snapshot_id},
        "credentialExpiresAt":expires_at,"requestDrainQualification":stored.request_drain_qualification,
        "review":crate::review::transition_review(&committed.value, transition_id)?,
        "workloadReadiness":"not-established-by-deployment-commit"}),
    )
}

fn executor_id(options: &BTreeMap<String, String>) -> Result<String> {
    if let Some(value) = options.get("--executor") {
        return Ok(value.clone());
    }
    if let Ok(value) = std::env::var("REAKT_EXECUTOR_ID") {
        return Ok(value);
    }
    if let (Ok(run), Ok(attempt)) = (
        std::env::var("GITHUB_RUN_ID"),
        std::env::var("GITHUB_RUN_ATTEMPT"),
    ) {
        return Ok(format!("github-{run}-{attempt}"));
    }
    Err("set REAKT_EXECUTOR_ID for the trusted local deployment job (GitHub Actions supplies its run identity automatically)".into())
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

async fn prepare_request(
    platform: &PlatformClient,
    inventory: &dyn inventory::InventoryStore,
    registry: &platform::Versioned<platform::NamespaceRegistry>,
    verified: &release::VerifiedRelease,
    transition_id: &str,
    ownership_names: Option<&str>,
) -> Result<DeploymentRequest> {
    let domain = &verified.compiled().ir.domain;
    let policy = platform.load_policy(&domain.id).await?;
    let mut history = registry
        .value
        .histories
        .get(&domain.id)
        .cloned()
        .ok_or("platform has not provisioned this domain history")?;
    history.revision.clone_from(&registry.resource_version);
    let previous = prepare_predecessor(inventory, &registry.value, domain, &history).await?;
    let generation_id = format!(
        "g{}-{}",
        history.allocations.len() + 1,
        &verified.release_digest()[7..19]
    );
    let next = GenerationPlan {
        generation_id,
        artifact_digest: verified.release_digest().into(),
        compiled: verified.compiled().clone(),
    };
    let names = ownership_names.map_or_else(Vec::new, |value| {
        value.split(',').map(str::to_owned).collect()
    });
    if names.iter().any(String::is_empty)
        || names.iter().collect::<BTreeSet<_>>().len() != names.len()
    {
        return Err("ownership intent names must be nonempty and unique".into());
    }
    let ownership_intents = if names.is_empty() {
        vec![]
    } else {
        platform
            .load_ownership_intents(&names, &next, transition_id)
            .await?
    };
    let request = DeploymentRequest {
        transition_id: transition_id.into(),
        previous,
        next,
        runtime_ceiling: policy.value.runtime_ceiling,
        cleanup_ceiling: policy.value.cleanup_ceiling,
        registry: RegistrySnapshot {
            revision: registry.resource_version.clone(),
            claims: registry.value.claims.clone(),
            generation_history: history,
        },
        observations: vec![],
        ownership_intents,
    };
    Ok(request)
}

async fn prepare_predecessor(
    inventory: &dyn inventory::InventoryStore,
    registry: &platform::NamespaceRegistry,
    domain: &compiler_core::Domain,
    history: &deploy::GenerationHistory,
) -> Result<Option<deploy::PreviousGeneration>> {
    let Some(active) = &history.active_generation_id else {
        return Ok(None);
    };
    let mut allocations = history
        .allocations
        .iter()
        .filter(|a| &a.generation_id == active);
    let allocation = allocations.next().ok_or("active allocation missing")?;
    if allocations.next().is_some() {
        return Err("active allocation is ambiguous".into());
    }
    let prior = registry
        .transitions
        .get(&allocation.transition_id)
        .ok_or("active generation has no committed protected plan")?;
    deploy::verify_record(&prior.plan, &prior.record)?;
    if prior.record.phase != Phase::Committed
        || &prior.plan.generation_reservation != allocation
        || prior.plan.request.next.compiled.ir.domain.id != domain.id
    {
        return Err("active generation differs from its protected transition".into());
    }
    if !matches!(
        prior.request_drain_qualification,
        Some(platform::RequestDrainQualification::IsolatedEnvoy { .. })
    ) {
        return Err("previous generation has no qualified request-drain transport".into());
    }
    let head = crate::inventory_connection::load(inventory, domain)
        .await?
        .ok_or("active inventory missing; explicit recovery required")?;
    let predecessor = deploy::PredecessorGeneration {
        allocation: allocation.clone(),
        scope: inventory::InventoryScope {
            cluster: domain.cluster_binding.clone(),
            namespace: domain.namespace.clone(),
            controller: domain.id.clone(),
        },
        control_namespace: domain.control_namespace.clone(),
        inventory: head.snapshot,
        retirement_role_binding_namespaces: prior
            .plan
            .request
            .next
            .compiled
            .capabilities
            .iter()
            .map(|c| c.namespace.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    Ok(Some(deploy::PreviousGeneration::Metadata(Box::new(
        predecessor,
    ))))
}

fn default_transition(
    registry: &platform::NamespaceRegistry,
    domain: &str,
    release: &str,
    policy: &str,
) -> String {
    let matching = |s: &&platform::StoredTransition| {
        s.plan.request.next.compiled.ir.domain.id == domain
            && s.plan.request.next.artifact_digest == release
    };
    if let Some(pending) = registry
        .transitions
        .values()
        .filter(matching)
        .find(|s| s.record.phase != Phase::Committed)
    {
        return pending.plan.request.transition_id.clone();
    }
    if let Some(active) = registry.transitions.values().filter(matching).find(|s| {
        registry
            .histories
            .get(domain)
            .and_then(|h| h.active_generation_id.as_ref())
            == Some(&s.plan.request.next.generation_id)
            && s.runtime_binding
                .as_ref()
                .is_some_and(|b| b.policy_digest == policy)
    }) {
        return active.plan.request.transition_id.clone();
    }
    format!(
        "deploy-g{}-{}",
        registry
            .histories
            .get(domain)
            .map_or(1, |h| h.allocations.len() + 1),
        &release[7..31]
    )
}

async fn drive_transition(
    platform: &PlatformClient,
    verified: &release::VerifiedRelease,
    transition: &str,
    executor: &str,
) -> Result<platform::Versioned<platform::NamespaceRegistry>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let registry = platform.load_registry().await?;
        let stored = registry
            .value
            .transitions
            .get(transition)
            .ok_or("transition disappeared")?;
        let upgrade = stored.plan.request.previous.is_some();
        let result = match stored.record.phase {
            Phase::Committed => return Ok(registry),
            Phase::Blocked => {
                return Err(format!(
                    "transition blocked: {}",
                    stored
                        .record
                        .blocked_reason
                        .as_deref()
                        .unwrap_or("protected recovery required")
                )
                .into());
            }
            Phase::Planned => {
                platform
                    .stage_proxy_controller(transition, executor, verified)
                    .await
            }
            Phase::Staged if upgrade => platform.quiesce_previous(transition, executor).await,
            Phase::Staged | Phase::Quiesced | Phase::Activated if !upgrade => {
                platform.activate_initial(transition, executor).await
            }
            Phase::Quiesced | Phase::Activated => {
                platform.activate_upgrade(transition, executor).await
            }
            Phase::Cleaning if upgrade => platform.commit_upgrade(transition, executor).await,
            Phase::Cleaning => platform.activate_initial(transition, executor).await,
            Phase::Staged => return Err("unsupported protected transition phase".into()),
        };
        match result {
            Ok(_) => {}
            Err(platform::Error::Pending(reason)) if tokio::time::Instant::now() < deadline => {
                let _ = writeln!(std::io::stderr().lock(), "reakt: {reason}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

enum ExistingTransition {
    Absent,
    Resume,
    Committed,
}

fn check_existing(
    registry: &platform::NamespaceRegistry,
    transition: &str,
    verified: &release::VerifiedRelease,
    executor: &str,
    policy: &str,
) -> Result<ExistingTransition> {
    let domain = &verified.compiled().ir.domain.id;
    if registry.transitions.values().any(|s| {
        s.plan.request.next.compiled.ir.domain.id == *domain
            && s.record.phase != Phase::Committed
            && s.plan.request.next.artifact_digest != verified.release_digest()
    }) {
        return Err(
            "domain has an unfinished transition for another artifact; finish or recover it first"
                .into(),
        );
    }
    let Some(stored) = registry.transitions.get(transition) else {
        return Ok(ExistingTransition::Absent);
    };
    if stored.plan.request.next.artifact_digest != verified.release_digest()
        || stored.plan.request.next.compiled != *verified.compiled()
    {
        return Err("transition belongs to another artifact".into());
    }
    if stored.record.phase == Phase::Committed {
        if registry
            .histories
            .get(domain)
            .and_then(|h| h.active_generation_id.as_ref())
            != Some(&stored.plan.request.next.generation_id)
            || stored
                .runtime_binding
                .as_ref()
                .is_none_or(|b| b.policy_digest != policy)
        {
            return Err("completed transition is no longer active under current policy; deploy with a fresh transition identity".into());
        }
        return Ok(ExistingTransition::Committed);
    }
    if stored.record.executor_id != executor {
        return Err(
            "unfinished transition belongs to another executor; protected recovery is required"
                .into(),
        );
    }
    Ok(ExistingTransition::Resume)
}

#[cfg(test)]
#[path = "deploy_tests.rs"]
mod tests;

#[cfg(test)]
mod artifact_binding_tests {
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
