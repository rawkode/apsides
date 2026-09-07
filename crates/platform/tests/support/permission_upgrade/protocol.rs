use compiler_core::Capability;
use deploy::{DeploymentRequest, GenerationPlan, RegistrySnapshot};
use inventory::InventoryStore;
use kube::Client;
use platform_api::{
    CandidatePairLifecycleOperation, CandidatePairQualificationTarget, CandidatePairRole,
    IssuedDeployment, PlatformClient,
};
use release::IntegrityVerifiedCandidate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

mod qualification_driver {
    use super::platform_api;
    include!("../qualification_driver.rs");
}
pub(super) use qualification_driver::Result;
use qualification_driver::{ConfigIdentity, DriverError};

const RESULT_PREFIX: &str = "APS_PERMISSION_UPGRADE_RESULT=";
const RESULT_SCHEMA: &str = "reakt.dev/permission-upgrade-result/v1";
const ISSUER: &str = "ceiling-issuer";

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Action {
    Verify,
    Bootstrap,
    PrepareInitial,
    ReserveInitial,
    StageInitial,
    ActivateInitial,
    PublishInitial,
    PrepareRejection,
    RejectUpgrade,
    PrepareSuccessor,
    ReserveSuccessor,
    StageSuccessor,
    QuiesceUpgrade,
    ActivateUpgrade,
    CommitUpgrade,
    PublishSuccessor,
    ResolveInitial,
    ResolveSuccessor,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    action: Action,
    purpose: platform_api::CandidatePairPurpose,
    initial_artifact: PathBuf,
    successor_artifact: PathBuf,
    trust_policy: PathBuf,
    target: Option<CandidatePairQualificationTarget>,
    registry_uid: Option<String>,
    registry_resource_version: Option<String>,
    expected_request_digest: Option<String>,
    expected_missing_capability: Option<Capability>,
    transition: Option<String>,
}

struct CandidatePair {
    initial: IntegrityVerifiedCandidate,
    successor: IntegrityVerifiedCandidate,
}

struct Prepared {
    registry: platform_api::Versioned<platform_api::NamespaceRegistry>,
    request: DeploymentRequest,
    request_digest: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestPreparation {
    CapturePredecessor,
    ReadOnly,
}

#[derive(Serialize)]
struct CandidateDigests<'a> {
    #[serde(rename = "initialReleaseDigest")]
    initial_release: &'a str,
    #[serde(rename = "initialIrDigest")]
    initial_ir: &'a str,
    #[serde(rename = "initialProvenanceDigest")]
    initial_provenance: &'a str,
    #[serde(rename = "successorReleaseDigest")]
    successor_release: &'a str,
    #[serde(rename = "successorIrDigest")]
    successor_ir: &'a str,
    #[serde(rename = "successorProvenanceDigest")]
    successor_provenance: &'a str,
    #[serde(rename = "trustPolicyDigest")]
    trust_policy: &'a str,
}

/// Run one closed permission-upgrade driver action.
///
/// # Errors
/// Returns bounded static classifications without exposing credentials or underlying errors.
pub(super) async fn run() -> Result<()> {
    let settings: Settings = qualification_driver::load_settings("APS_PERMISSION_UPGRADE_STEP")?;
    let pair = candidate_pair(&settings).await?;
    match settings.action {
        Action::Verify => verify(&settings, &pair),
        Action::Bootstrap => bootstrap(&settings, &pair).await,
        Action::PrepareInitial | Action::PrepareRejection => {
            prepare(&settings, &pair, CandidatePairRole::Initial).await
        }
        Action::ReserveInitial => reserve_initial(&settings, &pair).await,
        Action::StageInitial => {
            lifecycle(
                &settings,
                &pair,
                CandidatePairLifecycleOperation::StageInitial,
            )
            .await
        }
        Action::ActivateInitial => {
            lifecycle(
                &settings,
                &pair,
                CandidatePairLifecycleOperation::ActivateInitial,
            )
            .await
        }
        Action::PublishInitial => publish(&settings, &pair, CandidatePairRole::Initial).await,
        Action::RejectUpgrade => reject_upgrade(&settings, &pair).await,
        Action::PrepareSuccessor => prepare(&settings, &pair, CandidatePairRole::Successor).await,
        Action::ReserveSuccessor => reserve_successor(&settings, &pair).await,
        Action::StageSuccessor => {
            lifecycle(
                &settings,
                &pair,
                CandidatePairLifecycleOperation::StageSuccessor,
            )
            .await
        }
        Action::QuiesceUpgrade => {
            lifecycle(
                &settings,
                &pair,
                CandidatePairLifecycleOperation::QuiesceUpgrade,
            )
            .await
        }
        Action::ActivateUpgrade => {
            lifecycle(
                &settings,
                &pair,
                CandidatePairLifecycleOperation::ActivateUpgrade,
            )
            .await
        }
        Action::CommitUpgrade => {
            lifecycle(
                &settings,
                &pair,
                CandidatePairLifecycleOperation::CommitUpgrade,
            )
            .await
        }
        Action::PublishSuccessor => publish(&settings, &pair, CandidatePairRole::Successor).await,
        Action::ResolveInitial => resolve(&settings, &pair, CandidatePairRole::Initial).await,
        Action::ResolveSuccessor => resolve(&settings, &pair, CandidatePairRole::Successor).await,
    }
}

async fn candidate_pair(settings: &Settings) -> Result<CandidatePair> {
    let trust = qualification_driver::trust_policy(&settings.trust_policy)?;
    let initial_path = qualification_driver::artifact(&settings.initial_artifact)?;
    let successor_path = qualification_driver::artifact(&settings.successor_artifact)?;
    let initial = release::verify_candidate(&initial_path, &trust)
        .await
        .map_err(|_| DriverError("initial signed production candidate verification failed"))?;
    let successor = release::verify_candidate(&successor_path, &trust)
        .await
        .map_err(|_| DriverError("successor signed production candidate verification failed"))?;
    let a = &initial.compiled().ir.domain;
    let b = &successor.compiled().ir.domain;
    if a != b
        || initial.release_digest() == successor.release_digest()
        || initial.compiled().ir_digest == successor.compiled().ir_digest
        || initial.trust_policy_digest() != successor.trust_policy_digest()
    {
        return Err(DriverError("candidate pair identities differ"));
    }
    let mut initial_policy = candidate_policy(initial.compiled());
    if let platform_api::CandidatePairPurpose::LastTypeCleanup { identity } = &settings.purpose {
        for verb in ["get", "delete"] {
            initial_policy
                .cleanup_ceiling
                .capabilities
                .push(Capability {
                    namespace: identity.namespace.clone(),
                    api_group: identity.api_group.clone(),
                    resource: identity.resource.clone(),
                    subresource: None,
                    verb: verb.into(),
                    names: vec![identity.name.clone()],
                });
        }
        initial_policy.cleanup_ceiling.capabilities.sort();
        initial_policy.cleanup_ceiling.capabilities.dedup();
    }
    let successor_policy = match settings.purpose {
        platform_api::CandidatePairPurpose::PermissionExpansion {} => {
            candidate_policy(successor.compiled())
        }
        platform_api::CandidatePairPurpose::LastTypeCleanup { .. } => initial_policy.clone(),
    };
    let initial_digest = compiler_core::digest(&initial_policy)
        .map_err(|_| DriverError("candidate policy digest failed"))?;
    let successor_digest = compiler_core::digest(&successor_policy)
        .map_err(|_| DriverError("candidate policy digest failed"))?;
    settings
        .purpose
        .validate_plans(
            initial.compiled(),
            successor.compiled(),
            &initial_digest,
            &successor_digest,
        )
        .map_err(|_| DriverError("candidate pair purpose validation failed"))?;
    if settings.target.as_ref().is_some_and(|target| {
        target.purpose != settings.purpose
            || target.initial_policy_digest != initial_digest
            || target.successor_policy_digest != successor_digest
    }) {
        return Err(DriverError(
            "candidate pair target differs from selected purpose",
        ));
    }
    Ok(CandidatePair { initial, successor })
}

fn verify(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Verify)?;
    let domain = &pair.initial.compiled().ir.domain;
    emit(
        "verified-pair",
        pair,
        json!({
            "domainId": domain.id,
            "clusterBinding": domain.cluster_binding,
            "controlNamespace": domain.control_namespace,
            "workloadNamespace": domain.namespace,
            "initialCapabilityCount": pair.initial.compiled().capabilities.len(),
            "successorCapabilityCount": pair.successor.compiled().capabilities.len(),
        }),
    )
}
