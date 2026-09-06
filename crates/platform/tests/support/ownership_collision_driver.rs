use deploy::{DeploymentRequest, GenerationPlan, RegistrySnapshot, ResourceIdentity};
use kube::Client;
use platform_api::{CandidateQualificationTarget, IssuedDeployment, PlatformClient};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

mod qualification_driver {
    use super::platform_api;
    include!("qualification_driver.rs");
}
pub(super) use qualification_driver::Result;
use qualification_driver::{ConfigIdentity, DriverError};

const RESULT_PREFIX: &str = "APS_COLLISION_RESULT=";
const RESULT_SCHEMA: &str = "reakt.dev/ownership-collision-result/v1";
const EXECUTOR: &str = "ownership-collision-qualification";
const ISSUER: &str = "ceiling-issuer";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Action {
    Verify,
    Bootstrap,
    PrepareOwner,
    ReserveOwner,
    PrepareContender,
    Collide,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    action: Action,
    owner_artifact: PathBuf,
    owner_trust_policy: PathBuf,
    contender_artifact: PathBuf,
    contender_trust_policy: PathBuf,
    target: Option<CandidateQualificationTarget>,
    registry_uid: Option<String>,
    registry_resource_version: Option<String>,
    expected_request_digest: Option<String>,
}

struct CandidatePair {
    owner: release::IntegrityVerifiedCandidate,
    contender: release::IntegrityVerifiedCandidate,
    overlap_identity_digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairDigests<'a> {
    #[serde(rename = "ownerReleaseDigest")]
    owner_release: &'a str,
    #[serde(rename = "ownerIrDigest")]
    owner_ir: &'a str,
    #[serde(rename = "contenderReleaseDigest")]
    contender_release: &'a str,
    #[serde(rename = "contenderIrDigest")]
    contender_ir: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifiedResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    #[serde(flatten)]
    pair: PairDigests<'a>,
    owner_domain_id: &'a str,
    contender_domain_id: &'a str,
    cluster_binding: &'a str,
    control_namespace: &'a str,
    workload_namespace: &'a str,
    overlap_identity_digest: &'a str,
    overlap_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    #[serde(flatten)]
    pair: PairDigests<'a>,
    registry_uid: &'a str,
    registry_resource_version: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreparedResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    #[serde(flatten)]
    pair: PairDigests<'a>,
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
struct PlannedResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    #[serde(flatten)]
    pair: PairDigests<'a>,
    generation: &'a str,
    transition: &'a str,
    registry_uid: &'a str,
    registry_resource_version: &'a str,
    release_digest: &'a str,
    request_digest: &'a str,
    phase: &'static str,
    next_sequence: u64,
    claim_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollisionResult<'a> {
    schema_version: &'static str,
    outcome: &'static str,
    #[serde(flatten)]
    pair: PairDigests<'a>,
    generation: &'a str,
    transition: &'a str,
    registry_uid: &'a str,
    registry_resource_version: &'a str,
    release_digest: &'a str,
    request_digest: &'a str,
}

struct Prepared {
    registry: platform_api::Versioned<platform_api::NamespaceRegistry>,
    names: deploy::AllocationNames,
    request: DeploymentRequest,
    request_digest: String,
    creation_targets: Vec<platform_api::CreationTarget>,
}

/// Run one closed ownership-collision driver action.
///
/// # Errors
/// Returns only bounded static classifications; underlying errors are never exposed.
pub(super) async fn run() -> Result<()> {
    let settings: Settings = qualification_driver::load_settings("APS_COLLISION_STEP")?;
    let pair = candidate_pair(&settings).await?;
    match settings.action {
        Action::Verify => verify(&settings, &pair),
        Action::Bootstrap => bootstrap(&settings, &pair).await,
        Action::PrepareOwner => prepare(&settings, &pair, CandidateRole::Owner).await,
        Action::ReserveOwner => reserve_owner(&settings, &pair).await,
        Action::PrepareContender => prepare(&settings, &pair, CandidateRole::Contender).await,
        Action::Collide => collide(&settings, &pair).await,
    }
}

async fn candidate_pair(settings: &Settings) -> Result<CandidatePair> {
    let owner = verify_candidate(&settings.owner_artifact, &settings.owner_trust_policy).await?;
    let contender = verify_candidate(
        &settings.contender_artifact,
        &settings.contender_trust_policy,
    )
    .await?;
    let owner_domain = &owner.compiled().ir.domain;
    let contender_domain = &contender.compiled().ir.domain;
    let owner_identities = identities(owner.compiled())?;
    let contender_identities = identities(contender.compiled())?;
    let mut overlap = owner_identities.intersection(&contender_identities);
    let identity = overlap
        .next()
        .ok_or(DriverError("candidate pair has no canonical overlap"))?;
    if overlap.next().is_some()
        || owner_domain.id == contender_domain.id
        || owner.release_digest() == contender.release_digest()
        || owner_domain.cluster_binding != contender_domain.cluster_binding
        || owner_domain.control_namespace != contender_domain.control_namespace
        || owner_domain.namespace != contender_domain.namespace
    {
        return Err(DriverError("candidate pair scope or overlap is invalid"));
    }
    let overlap_identity_digest = compiler_core::digest(identity)
        .map_err(|_| DriverError("overlap identity digest failed"))?;
    Ok(CandidatePair {
        owner,
        contender,
        overlap_identity_digest,
    })
}

async fn verify_candidate(
    artifact_path: &std::path::Path,
    trust_path: &std::path::Path,
) -> Result<release::IntegrityVerifiedCandidate> {
    let artifact = qualification_driver::artifact(artifact_path)?;
    let policy = qualification_driver::trust_policy(trust_path)?;
    release::verify_candidate(&artifact, &policy)
        .await
        .map_err(|_| DriverError("signed production candidate verification failed"))
}

fn identities(compiled: &compiler_core::CompiledPlan) -> Result<BTreeSet<ResourceIdentity>> {
    compiled
        .ir
        .resources
        .iter()
        .map(deploy::resource_identity)
        .collect::<deploy::Result<_>>()
        .map_err(|_| DriverError("candidate resource identity projection failed"))
}

fn verify(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Verify)?;
    let owner = &pair.owner.compiled().ir.domain;
    let contender = &pair.contender.compiled().ir.domain;
    emit(&VerifiedResult {
        schema_version: RESULT_SCHEMA,
        outcome: "verified-pair",
        pair: pair_digests(pair),
        owner_domain_id: &owner.id,
        contender_domain_id: &contender.id,
        cluster_binding: &owner.cluster_binding,
        control_namespace: &owner.control_namespace,
        workload_namespace: &owner.namespace,
        overlap_identity_digest: &pair.overlap_identity_digest,
        overlap_count: 1,
    })
}

async fn bootstrap(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Bootstrap)?;
    let uid = qualification_driver::required(
        settings.registry_uid.as_deref(),
        "bootstrap registry UID is missing",
    )?;
    let revision = qualification_driver::required(
        settings.registry_resource_version.as_deref(),
        "bootstrap registry resourceVersion is missing",
    )?;
    let coordination = coordination_store().await?;
    let _ = inventory_store().await?;
    let platform = bootstrap_platform(&pair.owner, coordination).await?;
    let domains = vec![
        pair.owner.compiled().ir.domain.id.clone(),
        pair.contender.compiled().ir.domain.id.clone(),
    ];
    let written = platform
        .bootstrap_registry(uid, revision, &domains)
        .await
        .map_err(|_| DriverError("protected registry bootstrap failed"))?;
    qualification_driver::bounded_metadata(&written.uid, &written.resource_version)?;
    emit(&BootstrapResult {
        schema_version: RESULT_SCHEMA,
        outcome: "bootstrapped",
        pair: pair_digests(pair),
        registry_uid: &written.uid,
        registry_resource_version: &written.resource_version,
    })
}

#[derive(Clone, Copy)]
enum CandidateRole {
    Owner,
    Contender,
}

impl CandidateRole {
    fn candidate(self, pair: &CandidatePair) -> &release::IntegrityVerifiedCandidate {
        match self {
            Self::Owner => &pair.owner,
            Self::Contender => &pair.contender,
        }
    }

    fn outcome(self) -> &'static str {
        match self {
            Self::Owner => "owner-prepared",
            Self::Contender => "contender-prepared",
        }
    }

    fn creation_source(self) -> &'static str {
        match self {
            Self::Owner => "capability-complete-projection",
            Self::Contender => "claims-empty-capability-complete-projection",
        }
    }
}

async fn prepare(settings: &Settings, pair: &CandidatePair, role: CandidateRole) -> Result<()> {
    require_fields(settings, FieldSet::Prepare)?;
    let candidate = role.candidate(pair);
    let coordination = coordination_store().await?;
    let issued = issued_deployment(candidate, coordination).await?;
    let prepared = prepare_request(issued.platform(), candidate).await?;
    emit(&PreparedResult {
        schema_version: RESULT_SCHEMA,
        outcome: role.outcome(),
        pair: pair_digests(pair),
        generation: &prepared.names.generation_id,
        transition: &prepared.names.transition_id,
        registry_uid: &prepared.registry.uid,
        registry_resource_version: &prepared.registry.resource_version,
        release_digest: candidate.release_digest(),
        request_digest: &prepared.request_digest,
        creation_targets_source: role.creation_source(),
        creation_targets: &prepared.creation_targets,
    })
}

async fn reserve_owner(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Mutation)?;
    let target = settings
        .target
        .as_ref()
        .ok_or(DriverError("owner reservation target is missing"))?;
    let expected = expected_request(settings)?;
    let coordination = coordination_store().await?;
    let inventory = inventory_store().await?;
    let issued = issued_deployment(&pair.owner, coordination).await?;
    let platform = issued.platform();
    let prepared = prepare_request(platform, &pair.owner).await?;
    require_request_digest(&prepared, expected)?;
    let written = platform
        .reserve_qualification_candidate(
            &prepared.request,
            EXECUTOR,
            &pair.owner,
            target,
            &inventory,
        )
        .await
        .map_err(|_| DriverError("owner reservation failed"))?;
    verify_durable(platform, &written).await?;
    report_owner_planned(pair, &prepared, &written)
}

async fn collide(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Mutation)?;
    let target = settings
        .target
        .as_ref()
        .ok_or(DriverError("contender collision target is missing"))?;
    let expected = expected_request(settings)?;
    let coordination = coordination_store().await?;
    let inventory = inventory_store().await?;
    let issued = issued_deployment(&pair.contender, coordination).await?;
    let platform = issued.platform();
    let prepared = prepare_request(platform, &pair.contender).await?;
    require_request_digest(&prepared, expected)?;
    platform
        .qualify_candidate_collision(
            &prepared.request,
            EXECUTOR,
            &pair.owner,
            &pair.contender,
            target,
            &inventory,
        )
        .await
        .map_err(|_| DriverError("protected ownership collision qualification failed"))?;
    let durable = platform
        .load_registry()
        .await
        .map_err(|_| DriverError("collision durable readback failed"))?;
    if durable.uid != prepared.registry.uid
        || durable.resource_version != prepared.registry.resource_version
        || durable.value != prepared.registry.value
    {
        return Err(DriverError("collision changed protected registry state"));
    }
    emit(&CollisionResult {
        schema_version: RESULT_SCHEMA,
        outcome: "collision-rejected",
        pair: pair_digests(pair),
        generation: &prepared.names.generation_id,
        transition: &prepared.names.transition_id,
        registry_uid: &durable.uid,
        registry_resource_version: &durable.resource_version,
        release_digest: pair.contender.release_digest(),
        request_digest: &prepared.request_digest,
    })
}

async fn prepare_request(
    platform: &PlatformClient,
    candidate: &release::IntegrityVerifiedCandidate,
) -> Result<Prepared> {
    let domain = &candidate.compiled().ir.domain;
    let policy = platform
        .load_policy(&domain.id)
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

fn creation_targets(request: &DeploymentRequest) -> Result<Vec<platform_api::CreationTarget>> {
    let mut projection = request.clone();
    projection.registry.claims.clear();
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

async fn bootstrap_platform(
    candidate: &release::IntegrityVerifiedCandidate,
    coordination: Arc<coordination::LibsqlCoordination>,
) -> Result<PlatformClient> {
    let config = qualification_driver::explicit_config(ConfigIdentity::Admin).await?;
    let domain = &candidate.compiled().ir.domain;
    PlatformClient::new(
        Client::try_from(config)
            .map_err(|_| DriverError("Kubernetes client construction failed"))?,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )
    .map_err(|_| DriverError("platform client construction failed"))
    .map(|platform| {
        platform
            .with_claim_store(coordination.clone())
            .with_audit_store(coordination.clone())
            .with_review_store(coordination)
    })
}

async fn issued_deployment(
    candidate: &release::IntegrityVerifiedCandidate,
    coordination: Arc<coordination::LibsqlCoordination>,
) -> Result<IssuedDeployment> {
    let config = qualification_driver::explicit_config(ConfigIdentity::Issuer {
        name: ISSUER,
        error: "collision kubeconfig is not the dedicated issuer identity",
    })
    .await?;
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

async fn coordination_store() -> Result<Arc<coordination::LibsqlCoordination>> {
    qualification_driver::coordination_store("APS_COLLISION_COORDINATION_DB").await
}

async fn inventory_store() -> Result<inventory::LibsqlInventory> {
    qualification_driver::inventory_store("APS_COLLISION_INVENTORY_DB").await
}

async fn verify_durable(
    platform: &PlatformClient,
    written: &platform_api::Versioned<platform_api::NamespaceRegistry>,
) -> Result<()> {
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
    Ok(())
}

fn report_owner_planned(
    pair: &CandidatePair,
    prepared: &Prepared,
    written: &platform_api::Versioned<platform_api::NamespaceRegistry>,
) -> Result<()> {
    let stored = written
        .value
        .transition(&prepared.names.transition_id)
        .map_err(|_| DriverError("planned owner transition is absent"))?;
    let allocation = stored.allocation();
    let claim_count = identities(pair.owner.compiled())?.len();
    if written.uid != prepared.registry.uid
        || written.resource_version == prepared.registry.resource_version
        || written.value.next_sequence != 2
        || written.value.transitions.len() != 1
        || written.value.claims.len() != claim_count
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
        || allocation.artifact_digest != pair.owner.release_digest()
        || allocation.ir_digest != pair.owner.compiled().ir_digest
    {
        return Err(DriverError(
            "successful owner reservation metadata is not exact",
        ));
    }
    qualification_driver::bounded_metadata(&written.uid, &written.resource_version)?;
    emit(&PlannedResult {
        schema_version: RESULT_SCHEMA,
        outcome: "owner-planned",
        pair: pair_digests(pair),
        generation: &prepared.names.generation_id,
        transition: &prepared.names.transition_id,
        registry_uid: &written.uid,
        registry_resource_version: &written.resource_version,
        release_digest: pair.owner.release_digest(),
        request_digest: &prepared.request_digest,
        phase: "Planned",
        next_sequence: written.value.next_sequence,
        claim_count,
    })
}

#[derive(Clone, Copy)]
enum FieldSet {
    Verify,
    Bootstrap,
    Prepare,
    Mutation,
}

fn require_fields(settings: &Settings, fields: FieldSet) -> Result<()> {
    let target = settings.target.is_some();
    let uid = settings.registry_uid.is_some();
    let revision = settings.registry_resource_version.is_some();
    let expected = settings.expected_request_digest.is_some();
    let valid = match fields {
        FieldSet::Verify | FieldSet::Prepare => !target && !uid && !revision && !expected,
        FieldSet::Bootstrap => !target && uid && revision && !expected,
        FieldSet::Mutation => target && !uid && !revision && expected,
    };
    if !valid {
        return Err(DriverError("action settings fields are invalid"));
    }
    Ok(())
}

fn expected_request(settings: &Settings) -> Result<&str> {
    let digest = qualification_driver::required(
        settings.expected_request_digest.as_deref(),
        "expected request digest is missing",
    )?;
    if !qualification_driver::valid_digest(digest) {
        return Err(DriverError("expected request digest is invalid"));
    }
    Ok(digest)
}

fn require_request_digest(prepared: &Prepared, expected: &str) -> Result<()> {
    if prepared.request_digest != expected {
        return Err(DriverError(
            "protected request differs from prepared digest",
        ));
    }
    Ok(())
}

fn pair_digests(pair: &CandidatePair) -> PairDigests<'_> {
    PairDigests {
        owner_release: pair.owner.release_digest(),
        owner_ir: &pair.owner.compiled().ir_digest,
        contender_release: pair.contender.release_digest(),
        contender_ir: &pair.contender.compiled().ir_digest,
    }
}

fn emit(value: &impl Serialize) -> Result<()> {
    qualification_driver::emit(value, RESULT_PREFIX)
}
