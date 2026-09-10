use compiler_core::Capability;
use deploy::{DeploymentRequest, GenerationPlan, RegistrySnapshot};
use kube::{Client, Config};
use platform_api::{CandidateQualificationTarget, IssuedDeployment, PlatformClient};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

mod qualification_driver {
    use super::platform_api;
    include!("qualification_driver.rs");
}
use qualification_driver::DriverError;
pub(super) use qualification_driver::Result;

const RESULT_PREFIX: &str = "APS_CEILING_RESULT=";
const RESULT_SCHEMA: &str = "apsides.dev/deployment-ceiling-result/v1";
const EXECUTOR: &str = "deployment-ceiling-qualification";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Verify,
    Bootstrap,
    Prepare,
    Measure,
}

#[derive(Clone, Copy)]
enum ConfigIdentity {
    Admin,
    Issuer,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    action: Action,
    artifact: PathBuf,
    trust_policy: PathBuf,
    target: Option<CandidateQualificationTarget>,
    registry_uid: Option<String>,
    registry_resource_version: Option<String>,
    expected_missing_capability: Option<Capability>,
    expected_request_digest: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifiedResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    release_digest: &'a str,
    ir_digest: &'a str,
    domain_id: &'a str,
    cluster_binding: &'a str,
    control_namespace: &'a str,
    workload_namespace: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    registry_uid: &'a str,
    registry_resource_version: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    generation: &'a str,
    transition: &'a str,
    registry_uid: &'a str,
    registry_resource_version: &'a str,
    release_digest: &'a str,
    request_digest: &'a str,
    creation_targets_source: &'static str,
    creation_targets: &'a [platform_api::CreationTarget],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RejectionResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    generation: &'a str,
    transition: &'a str,
    registry_uid: &'a str,
    registry_resource_version: &'a str,
    release_digest: &'a str,
    request_digest: &'a str,
    missing_capability_digest: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    generation: &'a str,
    transition: &'a str,
    registry_uid: &'a str,
    registry_resource_version: &'a str,
    phase: &'static str,
    release_digest: &'a str,
    request_digest: &'a str,
    next_sequence: u64,
    claim_count: usize,
}

/// Run one closed qualification-driver action.
///
/// # Errors
/// Returns only bounded static classifications; underlying errors are never exposed.
pub(super) async fn run() -> Result<()> {
    let settings = settings()?;
    let policy = trust_policy(&settings.trust_policy)?;
    let artifact = artifact(&settings.artifact)?;
    let candidate = release::verify_candidate(&artifact, &policy)
        .await
        .map_err(|_| DriverError("signed production candidate verification failed"))?;

    match settings.action {
        Action::Verify => verify(&settings, &candidate),
        Action::Bootstrap => bootstrap(&settings, &candidate).await,
        Action::Prepare => prepare(&settings, &candidate).await,
        Action::Measure => measure(&settings, &candidate).await,
    }
}

fn verify(settings: &Settings, candidate: &release::IntegrityVerifiedCandidate) -> Result<()> {
    if settings.target.is_some()
        || settings.registry_uid.is_some()
        || settings.registry_resource_version.is_some()
        || settings.expected_missing_capability.is_some()
        || settings.expected_request_digest.is_some()
    {
        return Err(DriverError("verify settings contain cluster-only fields"));
    }
    let domain = &candidate.compiled().ir.domain;
    emit(&VerifiedResult {
        schema_version: RESULT_SCHEMA,
        outcome: "verified",
        release_digest: candidate.release_digest(),
        ir_digest: &candidate.compiled().ir_digest,
        domain_id: &domain.id,
        cluster_binding: &domain.cluster_binding,
        control_namespace: &domain.control_namespace,
        workload_namespace: &domain.namespace,
    })
}

async fn bootstrap(
    settings: &Settings,
    candidate: &release::IntegrityVerifiedCandidate,
) -> Result<()> {
    let _target = settings
        .target
        .as_ref()
        .ok_or(DriverError("bootstrap target is missing"))?;
    if settings.expected_missing_capability.is_some() || settings.expected_request_digest.is_some()
    {
        return Err(DriverError(
            "bootstrap settings contain a measurement field",
        ));
    }
    let uid = required(
        settings.registry_uid.as_deref(),
        "bootstrap registry UID is missing",
    )?;
    let revision = required(
        settings.registry_resource_version.as_deref(),
        "bootstrap registry resourceVersion is missing",
    )?;
    let coordination = coordination_store().await?;
    let _ = inventory_store().await?;
    let config = explicit_config(ConfigIdentity::Admin).await?;
    let domain = &candidate.compiled().ir.domain;
    let platform = PlatformClient::new(
        Client::try_from(config)
            .map_err(|_| DriverError("Kubernetes client construction failed"))?,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )
    .map_err(|_| DriverError("platform client construction failed"))?
    .with_claim_store(coordination.clone())
    .with_audit_store(coordination.clone())
    .with_review_store(coordination);
    let written = platform
        .bootstrap_registry(uid, revision, std::slice::from_ref(&domain.id))
        .await
        .map_err(|_| DriverError("protected registry bootstrap failed"))?;
    bounded_metadata(&written.uid, &written.resource_version)?;
    emit(&BootstrapResult {
        schema_version: RESULT_SCHEMA,
        outcome: "bootstrapped",
        registry_uid: &written.uid,
        registry_resource_version: &written.resource_version,
    })
}

async fn prepare(
    settings: &Settings,
    candidate: &release::IntegrityVerifiedCandidate,
) -> Result<()> {
    require_no_bootstrap_fields(settings, "prepare settings contain bootstrap fields")?;
    if settings.expected_missing_capability.is_some() || settings.expected_request_digest.is_some()
    {
        return Err(DriverError("prepare settings contain measurement fields"));
    }
    let _target = settings
        .target
        .as_ref()
        .ok_or(DriverError("prepare target is missing"))?;
    let coordination = coordination_store().await?;
    let issued = issued_deployment(candidate, coordination).await?;
    let prepared = prepare_request(issued.platform(), candidate).await?;
    emit(&PreparedResult {
        schema_version: RESULT_SCHEMA,
        outcome: "prepared",
        generation: &prepared.names.generation_id,
        transition: &prepared.names.transition_id,
        registry_uid: &prepared.registry.uid,
        registry_resource_version: &prepared.registry.resource_version,
        release_digest: candidate.release_digest(),
        request_digest: &prepared.request_digest,
        creation_targets_source: "capability-complete-projection",
        creation_targets: &prepared.creation_targets,
    })
}

async fn measure(
    settings: &Settings,
    candidate: &release::IntegrityVerifiedCandidate,
) -> Result<()> {
    require_no_bootstrap_fields(settings, "measurement settings contain bootstrap fields")?;
    let target = settings
        .target
        .as_ref()
        .ok_or(DriverError("measurement target is missing"))?;
    let expected_request = required(
        settings.expected_request_digest.as_deref(),
        "measurement request digest is missing",
    )?;
    if !valid_digest(expected_request) {
        return Err(DriverError("measurement request digest is invalid"));
    }
    let coordination = coordination_store().await?;
    let inventory = inventory_store().await?;
    let issued = issued_deployment(candidate, coordination).await?;
    reserve(
        settings,
        issued.platform(),
        candidate,
        target,
        &inventory,
        expected_request,
    )
    .await
}

async fn reserve(
    settings: &Settings,
    platform: &PlatformClient,
    candidate: &release::IntegrityVerifiedCandidate,
    target: &CandidateQualificationTarget,
    inventory: &inventory::LibsqlInventory,
    expected_request: &str,
) -> Result<()> {
    let prepared = prepare_request(platform, candidate).await?;
    if prepared.request_digest != expected_request {
        return Err(DriverError(
            "protected request differs from prepared digest",
        ));
    }
    let outcome = platform
        .reserve_qualification_candidate(&prepared.request, EXECUTOR, candidate, target, inventory)
        .await;
    if let Ok(written) = &outcome {
        let durable = platform
            .load_registry()
            .await
            .map_err(|_| DriverError("successful reservation durable readback failed"))?;
        if durable.uid != written.uid
            || durable.resource_version != written.resource_version
            || durable.value != written.value
        {
            return Err(DriverError(
                "successful reservation durable readback failed",
            ));
        }
    }
    report_reservation(
        outcome,
        settings.expected_missing_capability.as_ref(),
        candidate,
        &prepared,
    )
}

struct Prepared {
    registry: platform_api::Versioned<platform_api::NamespaceRegistry>,
    names: deploy::AllocationNames,
    request: DeploymentRequest,
    request_digest: String,
    creation_targets: Vec<platform_api::CreationTarget>,
}

async fn prepare_request(
    platform: &PlatformClient,
    candidate: &release::IntegrityVerifiedCandidate,
) -> Result<Prepared> {
    let policy = platform
        .load_policy(&candidate.compiled().ir.domain.id)
        .await
        .map_err(|_| DriverError("protected deployment policy load failed"))?;
    let registry = platform
        .load_registry()
        .await
        .map_err(|_| DriverError("protected registry load failed"))?;
    let names = deploy::allocation_names(&registry.uid, registry.value.next_sequence)
        .map_err(|_| DriverError("canonical allocation derivation failed"))?;
    let request = initial_request(candidate, &registry, policy.value)?;
    let request_digest = compiler_core::digest(&request)
        .map_err(|_| DriverError("deployment request digest failed"))?;
    let creation_targets = creation_targets(&request)?;
    Ok(Prepared {
        registry,
        names,
        request,
        request_digest,
        creation_targets,
    })
}

fn creation_targets(request: &DeploymentRequest) -> Result<Vec<platform_api::CreationTarget>> {
    let mut projection = request.clone();
    projection.runtime_ceiling.capabilities = projection.next.compiled.capabilities.clone();
    projection.runtime_ceiling.accepted_enforcement_gaps =
        projection.next.compiled.enforcement_gaps.clone();
    projection.runtime_ceiling.accepted_indirect_risks =
        projection.next.compiled.indirect_risks.clone();
    let plan = deploy::plan_transition(&projection)
        .map_err(|_| DriverError("capability-complete transition projection failed"))?;
    let permissions = platform_api::deployment_permissions(&plan)
        .map_err(|_| DriverError("creation-target projection failed"))?;
    Ok(permissions.creation_targets)
}

fn initial_request(
    candidate: &release::IntegrityVerifiedCandidate,
    registry: &platform_api::Versioned<platform_api::NamespaceRegistry>,
    policy: platform_api::PolicyState,
) -> Result<DeploymentRequest> {
    let domain = &candidate.compiled().ir.domain;
    let mut history = registry
        .value
        .histories
        .get(&domain.id)
        .cloned()
        .ok_or(DriverError(
            "candidate domain allocation authority is absent",
        ))?;
    history.revision.clone_from(&registry.resource_version);
    let names = deploy::allocation_names(&registry.uid, registry.value.next_sequence)
        .map_err(|_| DriverError("canonical allocation derivation failed"))?;
    Ok(DeploymentRequest {
        transition_id: names.transition_id,
        previous: None,
        next: GenerationPlan {
            generation_id: names.generation_id,
            artifact_digest: candidate.release_digest().into(),
            compiled: candidate.compiled().clone(),
        },
        runtime_ceiling: policy.runtime_ceiling,
        cleanup_ceiling: policy.cleanup_ceiling,
        registry: RegistrySnapshot {
            registry_uid: registry.uid.clone(),
            next_sequence: registry.value.next_sequence,
            revision: registry.resource_version.clone(),
            claims: registry.value.claims.clone(),
            generation_history: history,
        },
        observations: vec![],
        ownership_intents: vec![],
    })
}

fn report_reservation(
    outcome: platform_api::Result<platform_api::Versioned<platform_api::NamespaceRegistry>>,
    missing: Option<&Capability>,
    candidate: &release::IntegrityVerifiedCandidate,
    prepared: &Prepared,
) -> Result<()> {
    match (outcome, missing) {
        (Err(error), Some(capability)) => {
            report_expected_rejection(error, capability, candidate, prepared)
        }
        (Ok(written), None) => report_planned(&written, candidate, prepared),
        (Err(_), None) => Err(DriverError("positive reservation failed")),
        (Ok(_), Some(_)) => Err(DriverError("restricted reservation unexpectedly succeeded")),
    }
}

fn report_expected_rejection(
    error: platform_api::Error,
    capability: &Capability,
    candidate: &release::IntegrityVerifiedCandidate,
    prepared: &Prepared,
) -> Result<()> {
    if !candidate.compiled().capabilities.contains(capability) {
        return Err(DriverError("expected missing capability is not required"));
    }
    let platform_api::Error::Deploy(deploy::Error::Core(compiler_core::Error::Ceiling(actual))) =
        error
    else {
        return Err(DriverError(
            "reservation failed without the typed ceiling error",
        ));
    };
    let expected = format!(
        "{} {}/{} in {} is not explicitly allowed",
        capability.verb, capability.api_group, capability.resource, capability.namespace
    );
    if actual != expected {
        return Err(DriverError(
            "ceiling rejection did not identify the removed capability",
        ));
    }
    let digest = compiler_core::digest(capability)
        .map_err(|_| DriverError("missing capability digest failed"))?;
    bounded_metadata(&prepared.registry.uid, &prepared.registry.resource_version)?;
    emit(&RejectionResult {
        schema_version: RESULT_SCHEMA,
        outcome: "ceiling-rejected",
        generation: &prepared.names.generation_id,
        transition: &prepared.names.transition_id,
        registry_uid: &prepared.registry.uid,
        registry_resource_version: &prepared.registry.resource_version,
        release_digest: candidate.release_digest(),
        request_digest: &prepared.request_digest,
        missing_capability_digest: &digest,
    })
}

fn report_planned(
    written: &platform_api::Versioned<platform_api::NamespaceRegistry>,
    candidate: &release::IntegrityVerifiedCandidate,
    prepared: &Prepared,
) -> Result<()> {
    let stored = written
        .value
        .transition(&prepared.names.transition_id)
        .map_err(|_| DriverError("planned transition is absent"))?;
    let allocation = stored.allocation();
    if written.uid != prepared.registry.uid
        || written.resource_version == prepared.registry.resource_version
        || written.value.next_sequence != 2
        || written.value.transitions.len() != 1
        || stored.record.phase != deploy::Phase::Planned
        || stored.record.transition_id != prepared.names.transition_id
        || stored.record.executor_id != EXECUTOR
        || stored.record.revision != 0
        || stored.record.verification_epoch != 0
        || stored.record.blocked_from.is_some()
        || stored.record.blocked_reason.is_some()
        || !stored.record.receipts.is_empty()
        || allocation.registry_uid != written.uid
        || allocation.sequence != 1
        || allocation.generation_id != prepared.names.generation_id
        || allocation.transition_id != prepared.names.transition_id
        || allocation.service_account_name != prepared.names.service_account_name
        || allocation.artifact_digest != candidate.release_digest()
        || allocation.ir_digest != candidate.compiled().ir_digest.as_str()
    {
        return Err(DriverError("successful reservation metadata is not exact"));
    }
    bounded_metadata(&written.uid, &written.resource_version)?;
    emit(&PlannedResult {
        schema_version: RESULT_SCHEMA,
        outcome: "planned",
        generation: &prepared.names.generation_id,
        transition: &prepared.names.transition_id,
        registry_uid: &written.uid,
        registry_resource_version: &written.resource_version,
        phase: "Planned",
        release_digest: candidate.release_digest(),
        request_digest: &prepared.request_digest,
        next_sequence: written.value.next_sequence,
        claim_count: written.value.claims.len(),
    })
}

async fn issued_deployment(
    candidate: &release::IntegrityVerifiedCandidate,
    coordination: Arc<coordination::LibsqlCoordination>,
) -> Result<IssuedDeployment> {
    let config = explicit_config(ConfigIdentity::Issuer).await?;
    let domain = &candidate.compiled().ir.domain;
    Ok(IssuedDeployment::connect(
        config,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
        &domain.id,
    )
    .await
    .map_err(|_| DriverError("issued deployment connection failed"))?
    .with_stores(coordination.clone(), coordination.clone(), coordination))
}

fn settings() -> Result<Settings> {
    qualification_driver::load_settings("APS_CEILING_STEP")
}

fn trust_policy(path: &std::path::Path) -> Result<release::TrustPolicy> {
    qualification_driver::trust_policy(path)
}

fn artifact(path: &std::path::Path) -> Result<PathBuf> {
    qualification_driver::artifact(path)
}

async fn explicit_config(identity: ConfigIdentity) -> Result<Config> {
    let identity = match identity {
        ConfigIdentity::Admin => qualification_driver::ConfigIdentity::Admin,
        ConfigIdentity::Issuer => qualification_driver::ConfigIdentity::Issuer {
            name: "ceiling-issuer",
            error: "measurement kubeconfig is not the dedicated issuer identity",
        },
    };
    qualification_driver::explicit_config(identity).await
}

async fn coordination_store() -> Result<Arc<coordination::LibsqlCoordination>> {
    qualification_driver::coordination_store("APS_CEILING_COORDINATION_DB").await
}

async fn inventory_store() -> Result<inventory::LibsqlInventory> {
    qualification_driver::inventory_store("APS_CEILING_INVENTORY_DB").await
}

fn required<'a>(value: Option<&'a str>, error: &'static str) -> Result<&'a str> {
    qualification_driver::required(value, error)
}

fn require_no_bootstrap_fields(settings: &Settings, error: &'static str) -> Result<()> {
    if settings.registry_uid.is_some() || settings.registry_resource_version.is_some() {
        return Err(DriverError(error));
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    qualification_driver::valid_digest(value)
}

fn bounded_metadata(uid: &str, resource_version: &str) -> Result<()> {
    qualification_driver::bounded_metadata(uid, resource_version)
}

fn emit(value: &impl Serialize) -> Result<()> {
    qualification_driver::emit(value, RESULT_PREFIX)
}
