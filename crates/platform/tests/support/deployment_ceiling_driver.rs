use compiler_core::Capability;
use deploy::{DeploymentRequest, GenerationPlan, RegistrySnapshot};
use kube::{
    Client, Config,
    config::{KubeConfigOptions, Kubeconfig},
};
use platform_api::{CandidateQualificationTarget, IssuedDeployment, PlatformClient};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fmt::{Display, Formatter},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const RESULT_PREFIX: &str = "APS_CEILING_RESULT=";
const RESULT_SCHEMA: &str = "reakt.dev/deployment-ceiling-result/v1";
const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
const MAX_POLICY_BYTES: u64 = 1024 * 1024;
const MAX_KUBECONFIG_BYTES: u64 = 1024 * 1024;
const MAX_RESULT_BYTES: usize = 128 * 1024;
const EXECUTOR: &str = "deployment-ceiling-qualification";

#[derive(Debug)]
pub(super) struct DriverError(&'static str);

impl Display for DriverError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for DriverError {}

pub(super) type Result<T> = std::result::Result<T, DriverError>;

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

async fn coordination_store() -> Result<Arc<coordination::LibsqlCoordination>> {
    let coordination_path = database_path("APS_CEILING_COORDINATION_DB")?;
    Ok(Arc::new(
        coordination::LibsqlCoordination::open_local(&coordination_path)
            .await
            .map_err(|_| DriverError("local coordination database open failed"))?,
    ))
}

async fn inventory_store() -> Result<inventory::LibsqlInventory> {
    let path = database_path("APS_CEILING_INVENTORY_DB")?;
    inventory::LibsqlInventory::open_local(&path)
        .await
        .map_err(|_| DriverError("local inventory database open failed"))
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
    let path = environment_path("APS_CEILING_STEP")?;
    let bytes = bounded_regular_file(&path, MAX_SETTINGS_BYTES, "step settings are invalid")?;
    let text = std::str::from_utf8(&bytes).map_err(|_| DriverError("step settings are invalid"))?;
    let value =
        compiler_core::parse_strict(text).map_err(|_| DriverError("step settings are invalid"))?;
    serde_json::from_value(value).map_err(|_| DriverError("step settings are invalid"))
}

fn trust_policy(path: &Path) -> Result<release::TrustPolicy> {
    let bytes = bounded_regular_file(path, MAX_POLICY_BYTES, "trust policy is invalid")?;
    let text = std::str::from_utf8(&bytes).map_err(|_| DriverError("trust policy is invalid"))?;
    let value =
        compiler_core::parse_strict(text).map_err(|_| DriverError("trust policy is invalid"))?;
    serde_json::from_value(value).map_err(|_| DriverError("trust policy is invalid"))
}

fn artifact(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| DriverError("production candidate path is invalid"))?;
    if !path.is_absolute() || !metadata.file_type().is_dir() {
        return Err(DriverError("production candidate path is invalid"));
    }
    Ok(path.to_path_buf())
}

async fn explicit_config(identity: ConfigIdentity) -> Result<Config> {
    platform_api::install_rustls_ring_provider()
        .map_err(|_| DriverError("TLS provider initialization failed"))?;
    let path = environment_path("KUBECONFIG")?;
    let bytes = bounded_regular_file(
        &path,
        MAX_KUBECONFIG_BYTES,
        "dedicated kubeconfig is invalid",
    )?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| DriverError("dedicated kubeconfig is invalid"))?;
    let kubeconfig =
        Kubeconfig::from_yaml(text).map_err(|_| DriverError("dedicated kubeconfig is invalid"))?;
    let ([cluster], [user], [context]) = (
        kubeconfig.clusters.as_slice(),
        kubeconfig.auth_infos.as_slice(),
        kubeconfig.contexts.as_slice(),
    ) else {
        return Err(DriverError("dedicated kubeconfig is invalid"));
    };
    let current = kubeconfig
        .current_context
        .as_deref()
        .ok_or(DriverError("dedicated kubeconfig has no current context"))?;
    let selected = context
        .context
        .as_ref()
        .ok_or(DriverError("dedicated kubeconfig context is invalid"))?;
    let cluster_config = cluster
        .cluster
        .as_ref()
        .ok_or(DriverError("dedicated kubeconfig cluster is invalid"))?;
    if context.name != current
        || selected.cluster != cluster.name
        || selected.user.as_deref() != Some(user.name.as_str())
        || cluster_config.insecure_skip_tls_verify == Some(true)
        || (cluster_config.certificate_authority.is_none()
            && cluster_config.certificate_authority_data.is_none())
    {
        return Err(DriverError("dedicated kubeconfig selection is invalid"));
    }
    if matches!(identity, ConfigIdentity::Issuer) && !issuer_only(user) {
        return Err(DriverError(
            "measurement kubeconfig is not the dedicated issuer identity",
        ));
    }
    let options = KubeConfigOptions {
        context: Some(current.into()),
        cluster: None,
        user: None,
    };
    let mut config = Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .map_err(|_| DriverError("dedicated kubeconfig load failed"))?;
    if config.cluster_url.scheme_str() != Some("https")
        || config.accept_invalid_certs
        || (config.root_cert.is_none() && config.root_cert_file.is_none())
    {
        return Err(DriverError(
            "dedicated kubeconfig does not verify the API CA",
        ));
    }
    config.default_retry = false;
    config.connect_timeout = Some(Duration::from_secs(10));
    config.read_timeout = Some(Duration::from_secs(30));
    config.write_timeout = Some(Duration::from_secs(30));
    Ok(config)
}

fn issuer_only(user: &kube::config::NamedAuthInfo) -> bool {
    let Some(auth) = &user.auth_info else {
        return false;
    };
    user.name == "ceiling-issuer"
        && user.other.is_empty()
        && auth
            .token
            .as_ref()
            .is_some_and(|token| !token.expose_secret().is_empty())
        && auth.username.is_none()
        && auth.password.is_none()
        && auth.token_file.is_none()
        && auth.client_certificate.is_none()
        && auth.client_certificate_data.is_none()
        && auth.client_key.is_none()
        && auth.client_key_data.is_none()
        && auth.impersonate.is_none()
        && auth.impersonate_uid.is_none()
        && auth.impersonate_groups.is_none()
        && auth.impersonate_user_extra.is_none()
        && auth.extensions.is_none()
        && auth.auth_provider.is_none()
        && auth.exec.is_none()
        && auth.other.is_empty()
}

fn environment_path(name: &'static str) -> Result<PathBuf> {
    let value = env::var_os(name).ok_or(DriverError("required path environment is absent"))?;
    let paths = env::split_paths(&value).collect::<Vec<_>>();
    let [path] = paths.as_slice() else {
        return Err(DriverError("required path environment is invalid"));
    };
    if !path.is_absolute() {
        return Err(DriverError("required path environment is invalid"));
    }
    Ok(path.clone())
}

fn database_path(name: &'static str) -> Result<PathBuf> {
    let path = environment_path(name)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Ok(_) | Err(_) => Err(DriverError("local database path is invalid")),
    }
}

fn bounded_regular_file(path: &Path, maximum: u64, error: &'static str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|_| DriverError(error))?;
    if !path.is_absolute()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > maximum
    {
        return Err(DriverError(error));
    }
    let bytes = fs::read(path).map_err(|_| DriverError(error))?;
    let length = u64::try_from(bytes.len()).map_err(|_| DriverError(error))?;
    if bytes.is_empty() || length > maximum {
        return Err(DriverError(error));
    }
    Ok(bytes)
}

fn required<'a>(value: Option<&'a str>, error: &'static str) -> Result<&'a str> {
    let value = value.ok_or(DriverError(error))?;
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(DriverError(error));
    }
    Ok(value)
}

fn require_no_bootstrap_fields(settings: &Settings, error: &'static str) -> Result<()> {
    if settings.registry_uid.is_some() || settings.registry_resource_version.is_some() {
        return Err(DriverError(error));
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn bounded_metadata(uid: &str, resource_version: &str) -> Result<()> {
    if uid.is_empty()
        || uid.len() > 256
        || resource_version.is_empty()
        || resource_version.len() > 256
        || uid.chars().any(char::is_control)
        || resource_version.chars().any(char::is_control)
    {
        return Err(DriverError("result metadata is invalid"));
    }
    Ok(())
}

fn emit(value: &impl Serialize) -> Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| DriverError("qualification result serialization failed"))?;
    if encoded.len() > MAX_RESULT_BYTES || encoded.chars().any(|character| character == '\n') {
        return Err(DriverError("qualification result exceeds its bound"));
    }
    println!("{RESULT_PREFIX}{encoded}");
    Ok(())
}
