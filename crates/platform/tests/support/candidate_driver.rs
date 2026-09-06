// Shared only by test executables; does not construct a VerifiedRelease.
use deploy::{DeploymentRequest, GenerationPlan, RegistrySnapshot};
use inventory::InventoryStore;
use kube::{Client, Config};
use platform_api::{IssuedDeployment, PlatformClient, TestClusterTarget};
use release::TestStagingScope;
use serde_json::json;
use std::{env, fs, path::Path, sync::Arc};

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) async fn transition_step(
    configure: impl FnOnce(PlatformClient, &serde_json::Value) -> Result<PlatformClient>,
) -> Result<()> {
    platform_api::install_rustls_ring_provider()?;
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
    .with_claim_store(claim_store.clone())
    .with_audit_store(claim_store.clone())
    .with_review_store(claim_store.clone());
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
    let issued = IssuedDeployment::connect(
        config,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
        &domain.id,
    )
    .await?
    .with_stores(claim_store.clone(), claim_store.clone(), claim_store);
    let platform = configure(issued.platform().clone(), &settings)?;
    Box::pin(run_action(
        action,
        &settings,
        &platform,
        &authorization,
        &target,
        transition,
        executor,
    ))
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
    let inventory = inventory_store().await?;
    if action == "reserve" {
        let names: Vec<String> = serde_json::from_value(
            settings
                .get("ownershipIntents")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )?;
        reserve(
            platform,
            authorization,
            target,
            &inventory,
            transition,
            executor,
            &names,
        )
        .await?;
    } else {
        let session = platform
            .transition_test_session(authorization, target, &inventory, transition)
            .await?;
        if action == "inventory" {
            return report_committed_inventory(&session, &inventory, executor).await;
        }
        match action {
            "stage" => {
                session
                    .stage_test_candidate(transition, executor, authorization, target)
                    .await?;
            }
            "quiesce" => {
                session.quiesce_previous(transition, executor).await?;
            }
            "activate" => {
                let registry = session.load_registry().await?;
                if registry
                    .value
                    .transition(transition)?
                    .inputs
                    .previous
                    .is_some()
                {
                    session.activate_upgrade(transition, executor).await?;
                } else {
                    Box::pin(session.activate_initial(transition, executor)).await?;
                    session
                        .publish_committed_inventory(&inventory, transition, executor)
                        .await?;
                }
            }
            "commit" => {
                session.commit_upgrade(transition, executor).await?;
                session
                    .publish_committed_inventory(&inventory, transition, executor)
                    .await?;
            }
            _ => return Err("unknown disposable test action".into()),
        }
    }
    let registry = platform.load_registry().await?;
    let state = registry.value.transition(transition)?;
    println!(
        "REAKT_TEST_RESULT={}",
        json!({"candidateIntegrityVerified":true,"releaseVerified":false,"transition":transition,"phase":state.record.phase,"generation":state.allocation().generation_id,"registryResourceVersion":registry.resource_version})
    );
    Ok(())
}

async fn report_committed_inventory(
    session: &platform_api::TransitionSession,
    inventory: &dyn InventoryStore,
    executor: &str,
) -> Result<()> {
    let selected = session.resolve().await?;
    let stored = selected.stored();
    let domain = stored.domain_id();
    let registry = selected.registry();
    if stored.record.phase != deploy::Phase::Committed
        || stored.record.executor_id != executor
        || registry
            .value
            .histories
            .get(domain)
            .and_then(|history| history.active_allocation.as_ref())
            != Some(stored.allocation())
        || registry.value.transitions.values().any(|entry| {
            entry.domain_id() == domain && entry.record.phase != deploy::Phase::Committed
        })
    {
        return Err("inventory check requires the active committed transition".into());
    }
    let (scope, expected) =
        deploy::generation_inventory(&selected.plan().request.next, &registry.value.claims)?;
    let head = inventory
        .load(&scope)
        .await?
        .ok_or("committed inventory head missing")?;
    deploy::validate_inventory_checkpoint(&scope, &head.snapshot, stored.allocation())?;
    if head.snapshot != expected
        || inventory::snapshot_id(&scope, &head.snapshot)? != head.snapshot_id
    {
        return Err("committed inventory differs from protected generation metadata".into());
    }
    let after = session.resolve().await?;
    if after.registry().uid != registry.uid
        || after.registry().resource_version != registry.resource_version
    {
        return Err("registry changed during committed inventory observation".into());
    }
    println!(
        "REAKT_TEST_INVENTORY={}",
        json!({
            "generation": head.snapshot.generation, "snapshotId": head.snapshot_id,
        })
    );
    Ok(())
}

async fn claim_store() -> Result<Arc<coordination::LibsqlCoordination>> {
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

async fn inventory_store() -> Result<inventory::LibsqlInventory> {
    let database = env::var("REAKT_INVENTORY_DATABASE")?;
    let path = Path::new(&database);
    if !path.is_absolute() || env::var_os("REAKT_INVENTORY_TOKEN").is_some() {
        return Err(
            "disposable lifecycle inventory requires an absolute local database path".into(),
        );
    }
    Ok(inventory::LibsqlInventory::open_local(path).await?)
}

async fn reserve(
    platform: &PlatformClient,
    authorization: &release::TestStagingAuthorization,
    target: &TestClusterTarget,
    inventory: &dyn InventoryStore,
    transition: &str,
    executor: &str,
    ownership_names: &[String],
) -> Result<()> {
    let candidate = authorization.candidate();
    let domain = &candidate.compiled().ir.domain;
    let policy = platform.load_policy(&domain.id).await?.value;
    compiler_core::validate_ceiling(candidate.compiled(), &policy.runtime_ceiling)?;
    let mut registry = platform.load_registry().await?;
    if registry.value.transitions.contains_key(transition) {
        return Err("test transition already exists".into());
    }
    let scope = inventory::InventoryScope {
        cluster: domain.cluster_binding.clone(),
        namespace: domain.namespace.clone(),
        controller: domain.id.clone(),
    };
    let head = inventory.load(&scope).await?;
    let active = registry
        .value
        .histories
        .get(&domain.id)
        .and_then(|history| history.active_allocation.as_ref());
    if head.as_ref().map(|head| head.snapshot.generation.as_str())
        != active.map(|allocation| allocation.generation_id.as_str())
    {
        return Err("inventory head differs from active test generation".into());
    }
    let previous = if let Some(head) = head {
        platform.ensure_active_writer_checkpoint(&domain.id).await?;
        registry = platform
            .capture_active_claim_uids(&domain.id, &head.snapshot)
            .await?;
        let captured = platform
            .publish_captured_inventory(inventory, &domain.id, &registry, &head)
            .await?;
        let checkpoint = registry
            .value
            .active_writer_checkpoints
            .get(&domain.id)
            .ok_or("active writer checkpoint missing")?;
        Some(deploy::PreviousGeneration::Metadata(Box::new(
            checkpoint.predecessor_metadata(&scope, &captured.snapshot)?,
        )))
    } else {
        None
    };
    let mut history = registry
        .value
        .histories
        .get(&domain.id)
        .ok_or("test domain history missing")?
        .clone();
    history.revision.clone_from(&registry.resource_version);
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
    platform
        .reserve_test_candidate(&request, executor, authorization, target, inventory)
        .await?;
    Ok(())
}
