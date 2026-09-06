#![cfg(feature = "test-support")]
//! Explicitly invoked by the disposable lifecycle runner, never a release bypass.
use deploy::{DeploymentRequest, GenerationPlan, Phase, RegistrySnapshot};
use kube::{Client, Config, config::AuthInfo};
use platform::{PlatformClient, TestClusterTarget};
use release::TestStagingScope;
use serde_json::json;
use std::{env, fs, path::Path, sync::Arc};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
#[ignore = "requires the explicitly created disposable lifecycle cluster and signed candidate"]
async fn candidate_transition_step() -> Result<()> {
    let outcome = Box::pin(transition_step()).await;
    if let Err(error) = &outcome {
        let pending = matches!(
            error.downcast_ref::<platform::Error>(),
            Some(platform::Error::Pending(_))
        );
        println!(
            "REAKT_TEST_ERROR={}",
            json!({"pending":pending,"message":error.to_string()})
        );
    }
    outcome
}

async fn transition_step() -> Result<()> {
    platform::install_rustls_ring_provider()?;
    let settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(env::var("REAKT_TEST_STEP")?)?)?;
    let artifact = settings["artifact"].as_str().ok_or("artifact missing")?;
    let transition = settings["transition"]
        .as_str()
        .ok_or("transition missing")?;
    let action = settings["action"].as_str().ok_or("action missing")?;
    let executor = "disposable-lifecycle-test";
    let mut config = Config::infer().await?;
    if config.cluster_url.scheme_str() != Some("https") || config.accept_invalid_certs {
        return Err("test API requires verified TLS".into());
    }
    config.default_retry = false;
    let (_, plan, _) = release::read_release(Path::new(artifact))?;
    let domain = &plan.ir.domain;
    let claim_store = claim_store().await?;
    let bootstrap = PlatformClient::new(
        Client::try_from(config.clone())?,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )?
    .with_claim_store(claim_store.clone());
    let trust = serde_json::from_value(bootstrap.load_trust_policy(&domain.id).await?.value)?;
    let candidate = release::verify_candidate(Path::new(artifact), &trust).await?;
    let authorization = candidate.authorize_test_staging(TestStagingScope {
        cluster_binding: domain.cluster_binding.clone(),
        control_namespace: domain.control_namespace.clone(),
        workload_namespace: domain.namespace.clone(),
    })?;
    let target: TestClusterTarget = serde_json::from_value(settings["target"].clone())?;
    bootstrap
        .verify_test_cluster_target(&authorization, &target)
        .await?;
    if action == "bootstrap" {
        let registry_uid = settings["registryUid"]
            .as_str()
            .ok_or("registry UID missing")?;
        let registry_resource_version = settings["registryResourceVersion"]
            .as_str()
            .ok_or("registry resourceVersion missing")?;
        bootstrap
            .bootstrap_registry(
                registry_uid,
                registry_resource_version,
                std::slice::from_ref(&domain.id),
            )
            .await?;
        return Ok(());
    }
    let credential = bootstrap.approved_deployment_credential(&domain.id).await?;
    config.auth_info = AuthInfo {
        token: Some(credential.token),
        ..AuthInfo::default()
    };
    config.headers.clear();
    let platform = PlatformClient::new(
        Client::try_from(config)?,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )?
    .with_claim_store(claim_store);
    run_action(
        action,
        &settings,
        &platform,
        &authorization,
        &target,
        transition,
        executor,
    )
    .await
}

async fn run_action(
    action: &str,
    settings: &serde_json::Value,
    platform: &PlatformClient,
    authorization: &release::TestStagingAuthorization,
    target: &TestClusterTarget,
    transition: &str,
    executor: &str,
) -> Result<()> {
    if action == "allocation" {
        let registry = platform.load_registry().await?;
        let names = deploy::allocation_names(&registry.uid, registry.value.next_sequence)?;
        println!(
            "REAKT_TEST_ALLOCATION={}",
            json!({"generation":names.generation_id,"transition":names.transition_id})
        );
        return Ok(());
    }
    if action == "claims" {
        let registry = platform.load_registry().await?;
        println!("REAKT_TEST_CLAIMS={}", json!(registry.value.claims));
        return Ok(());
    }
    match action {
        "reserve" => {
            let names: Vec<String> = serde_json::from_value(
                settings
                    .get("ownershipIntents")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )?;
            reserve(platform, authorization, transition, executor, &names).await?;
        }
        "stage" => {
            platform
                .stage_test_candidate(transition, executor, authorization, target)
                .await?;
        }
        "quiesce" => {
            platform.quiesce_previous(transition, executor).await?;
        }
        "activate" => {
            let registry = platform.load_registry().await?;
            if registry.value.transitions[transition]
                .plan
                .request
                .previous
                .is_some()
            {
                platform.activate_upgrade(transition, executor).await?;
            } else {
                platform.activate_initial(transition, executor).await?;
            }
        }
        "commit" => {
            platform.commit_upgrade(transition, executor).await?;
        }
        _ => return Err("unknown disposable test action".into()),
    }
    let registry = platform.load_registry().await?;
    let state = &registry.value.transitions[transition];
    println!(
        "REAKT_TEST_RESULT={}",
        json!({"candidateIntegrityVerified":true,"releaseVerified":false,"transition":transition,"phase":state.record.phase,"generation":state.plan.request.next.generation_id,"registryResourceVersion":registry.resource_version})
    );
    Ok(())
}

async fn claim_store() -> Result<Arc<dyn coordination::ClaimStore>> {
    let database = env::var("REAKT_INVENTORY_DATABASE")?;
    let path = Path::new(&database);
    if !path.is_absolute() || env::var_os("REAKT_INVENTORY_TOKEN").is_some() {
        return Err(
            "disposable lifecycle claim store requires an absolute local database path".into(),
        );
    }
    Ok(Arc::new(
        coordination::LibsqlCoordination::open_local(path).await?,
    ))
}

async fn reserve(
    platform: &PlatformClient,
    authorization: &release::TestStagingAuthorization,
    transition: &str,
    executor: &str,
    ownership_names: &[String],
) -> Result<()> {
    let candidate = authorization.candidate();
    let domain = &candidate.compiled().ir.domain;
    let mut registry = platform.load_registry().await?;
    if registry.value.transitions.contains_key(transition) {
        return Err("test transition already exists".into());
    }
    if registry.value.histories[&domain.id]
        .active_allocation
        .is_some()
    {
        let active = registry
            .value
            .histories
            .get(&domain.id)
            .and_then(|history| history.active_allocation.as_ref())
            .ok_or("active generation missing")?;
        let prior = registry
            .value
            .transitions
            .values()
            .find(|stored| {
                stored.plan.generation_reservation == *active
                    && stored.plan.request.next.compiled.ir.domain.id == domain.id
            })
            .ok_or("active test generation missing")?;
        let (_, inventory) =
            deploy::generation_inventory(&prior.plan.request.next, &registry.value.claims)?;
        registry = platform
            .capture_active_claim_uids(&domain.id, &inventory)
            .await?;
    }
    let mut history = registry.value.histories[&domain.id].clone();
    history.revision.clone_from(&registry.resource_version);
    let previous = history
        .active_allocation
        .as_ref()
        .map(|generation| -> Result<deploy::PreviousGeneration> {
            let stored = registry
                .value
                .transitions
                .values()
                .find(|s| {
                    s.record.phase == Phase::Committed
                        && s.plan.generation_reservation == *generation
                })
                .ok_or("active plan absent")?;
            Ok(deploy::PreviousGeneration::Metadata(Box::new(
                deploy::predecessor_metadata(
                    &stored.plan.request.next,
                    &stored.plan.generation_reservation,
                    &registry.value.claims,
                )?,
            )))
        })
        .transpose()?;
    let policy = platform.load_policy(&domain.id).await?.value;
    let names = deploy::allocation_names(&registry.uid, registry.value.next_sequence)?;
    if transition != names.transition_id {
        return Err("test transition must match the protected allocation identity".into());
    }
    let next = GenerationPlan {
        generation_id: names.generation_id,
        artifact_digest: candidate.release_digest().into(),
        compiled: candidate.compiled().clone(),
    };
    let ownership_intents = platform
        .load_ownership_intents(ownership_names, &next, transition)
        .await?;
    let request = DeploymentRequest {
        transition_id: transition.into(),
        previous,
        next,
        runtime_ceiling: policy.runtime_ceiling,
        cleanup_ceiling: policy.cleanup_ceiling,
        registry: RegistrySnapshot {
            registry_uid: registry.uid,
            next_sequence: registry.value.next_sequence,
            revision: registry.resource_version,
            claims: registry.value.claims,
            generation_history: history,
        },
        observations: vec![],
        ownership_intents,
    };
    platform.reserve_transition(&request, executor).await?;
    Ok(())
}

#[test]
#[ignore = "metadata for explicit disposable-cluster bootstrap"]
fn proxy_profile() -> Result<()> {
    println!(
        "REAKT_TEST_PROFILE={}",
        json!({"envoyImage":platform::ENVOY_IMAGE,"adminImage":platform::ADMIN_IMAGE,"configurationTemplateDigest":compiler_core::digest(&platform::envoy_configuration("qualified-template")?)?,"maxLedgerBytes":platform::MAX_PROXY_LEDGER_BYTES})
    );
    Ok(())
}
