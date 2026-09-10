use super::*;
use coordination::{ClaimScope, ReviewMetadataV1, ReviewRef, ReviewStore};
use http::Method;
use release::{IntegrityVerifiedCandidate, TrustPolicy};
use std::{
    env,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

const CANDIDATE_ENV: &str = "APS_QUALIFICATION_CANDIDATE";
const TRUST_ENV: &str = "APS_QUALIFICATION_TRUST_POLICY";
const RUN_ID: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct ObservedReviewStore {
    rows: Mutex<BTreeMap<String, ReviewMetadataV1>>,
    writes: AtomicUsize,
}

#[async_trait::async_trait]
impl ReviewStore for ObservedReviewStore {
    async fn put_review(&self, metadata: &ReviewMetadataV1) -> coordination::Result<ReviewRef> {
        let reference = metadata.reference()?;
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("qualification review lock"))?;
        if rows
            .get(&reference.digest)
            .is_some_and(|existing| existing != metadata)
        {
            return Err(coordination::Error::Invalid(
                "qualification review digest collision",
            ));
        }
        rows.entry(reference.digest.clone())
            .or_insert_with(|| metadata.clone());
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(reference)
    }

    async fn read_review(
        &self,
        reference: &ReviewRef,
    ) -> coordination::Result<Option<ReviewMetadataV1>> {
        reference.validate()?;
        Ok(self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("qualification review lock"))?
            .get(&reference.digest)
            .cloned())
    }
}

struct QualificationFixture {
    request: deploy::DeploymentRequest,
    identity: Versioned<DeploymentIdentity>,
    target: CandidateQualificationTarget,
    objects: BTreeMap<String, Value>,
    claim_store: Arc<MemoryClaimStore>,
}

fn explicit_path(
    variable: &str,
) -> std::result::Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let path = PathBuf::from(env::var_os(variable).ok_or("qualification input variable missing")?);
    if !path.is_absolute() {
        return Err(format!("{variable} must be an absolute path").into());
    }
    Ok(path)
}

fn read_trust(
    path: &Path,
) -> std::result::Result<TrustPolicy, Box<dyn std::error::Error + Send + Sync>> {
    const LIMIT: u64 = 1024 * 1024;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > LIMIT
    {
        return Err("qualification trust policy must be a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err("qualification trust policy exceeded its size limit".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn state_object(
    name: &str,
    namespace: &str,
    value: &impl serde::Serialize,
    uid: &str,
    resource_version: &str,
) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    Ok(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": uid,
            "resourceVersion": resource_version
        },
        "data": {"state.json": serde_json::to_string(value)?}
    }))
}

fn namespace(name: &str, uid: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {"name": name, "uid": uid, "resourceVersion": "1"}
    })
}

impl QualificationFixture {
    fn new(
        candidate: &IntegrityVerifiedCandidate,
        trust: &TrustPolicy,
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let domain = &candidate.compiled().ir.domain;
        let names = deploy::allocation_names(REGISTRY_UID, 1)?;
        let policy = policy(candidate.compiled());
        let history = deploy::GenerationHistory {
            domain_id: domain.id.clone(),
            cluster_binding: domain.cluster_binding.clone(),
            revision: "42".into(),
            active_allocation: None,
        };
        let request = deploy::DeploymentRequest {
            transition_id: names.transition_id,
            previous: None,
            next: deploy::GenerationPlan {
                generation_id: names.generation_id,
                artifact_digest: candidate.release_digest().into(),
                compiled: candidate.compiled().clone(),
            },
            runtime_ceiling: policy.runtime_ceiling.clone(),
            cleanup_ceiling: policy.cleanup_ceiling,
            registry: deploy::RegistrySnapshot {
                registry_uid: REGISTRY_UID.into(),
                next_sequence: 1,
                revision: "42".into(),
                claims: vec![],
                generation_history: history.clone(),
            },
            observations: vec![],
            ownership_intents: vec![],
        };
        let claim_store = Arc::new(MemoryClaimStore::default());
        let scope = ClaimScope {
            registry_uid: REGISTRY_UID.into(),
            cluster_binding: domain.cluster_binding.clone(),
            control_namespace: domain.control_namespace.clone(),
            workload_namespace: domain.namespace.clone(),
        };
        let claims = ClaimSnapshotV1 {
            schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
            scope,
            claims: vec![],
        };
        let claim_reference = claim_store.seed(claims)?;
        let registry = NamespaceRegistryState {
            registry_uid: REGISTRY_UID.into(),
            next_sequence: 1,
            schema_version: REGISTRY_SCHEMA_VERSION.into(),
            cluster_binding: domain.cluster_binding.clone(),
            workload_namespace: domain.namespace.clone(),
            claim_snapshot: claim_reference,
            histories: BTreeMap::from([(domain.id.clone(), history)]),
            active_writer_checkpoints: BTreeMap::new(),
            audit_heads: BTreeMap::new(),
            transitions: BTreeMap::new(),
        };
        let identity = Versioned {
            value: DeploymentIdentity {
                schema_version: SCHEMA_VERSION.into(),
                domain_id: domain.id.clone(),
                cluster_binding: domain.cluster_binding.clone(),
                service_account_name: "deployer".into(),
                service_account_uid: "deployer-uid".into(),
                binding_secret_name: "deployer-binding".into(),
                binding_secret_uid: "deployer-binding-uid".into(),
                audience: "https://kubernetes.default.svc".into(),
                lifetime_seconds: 600,
                preview: None,
            },
            uid: "deployment-identity-uid".into(),
            resource_version: "31".into(),
        };
        let target = qualification_target();
        let objects = qualification_objects(candidate, trust, &registry, &identity, &target)?;
        Ok(Self {
            request,
            identity,
            target,
            objects,
            claim_store,
        })
    }
}

fn qualification_target() -> CandidateQualificationTarget {
    let cluster = format!("apsides-qual-{RUN_ID}");
    let node_name = format!("{cluster}-control-plane");
    let provider_id = format!("kind://docker/{cluster}/{node_name}");
    CandidateQualificationTarget {
        run_id: RUN_ID.into(),
        kube_system_namespace_uid: "kube-system-uid".into(),
        control_namespace_uid: "control-uid".into(),
        workload_namespace_uid: "workload-uid".into(),
        sentinel_namespace_uid: "sentinel-uid".into(),
        marker_uid: "marker-uid".into(),
        marker_resource_version: "7".into(),
        node_name,
        node_uid: "node-uid".into(),
        node_provider_id: provider_id,
        node_image_digest: format!("sha256:{}", "b".repeat(64)),
    }
}

fn qualification_marker(
    candidate: &IntegrityVerifiedCandidate,
    target: &CandidateQualificationTarget,
) -> CandidateQualificationMarker {
    let domain = &candidate.compiled().ir.domain;
    let cluster = format!("apsides-qual-{RUN_ID}");
    let node_name = &target.node_name;
    CandidateQualificationMarker {
        schema_version: CANDIDATE_QUALIFICATION_SCHEMA.into(),
        run_id: RUN_ID.into(),
        cluster_name: cluster.clone(),
        sentinel_namespace: cluster,
        domain_id: domain.id.clone(),
        release_digest: candidate.release_digest().into(),
        cluster_binding: domain.cluster_binding.clone(),
        control_namespace: domain.control_namespace.clone(),
        workload_namespace: domain.namespace.clone(),
        kube_system_namespace_uid: target.kube_system_namespace_uid.clone(),
        control_namespace_uid: target.control_namespace_uid.clone(),
        workload_namespace_uid: target.workload_namespace_uid.clone(),
        sentinel_namespace_uid: target.sentinel_namespace_uid.clone(),
        node_name: node_name.clone(),
        node_uid: target.node_uid.clone(),
        node_provider_id: target.node_provider_id.clone(),
        node_image_digest: target.node_image_digest.clone(),
    }
}

fn qualification_objects(
    candidate: &IntegrityVerifiedCandidate,
    trust: &TrustPolicy,
    registry: &NamespaceRegistryState,
    identity: &Versioned<DeploymentIdentity>,
    target: &CandidateQualificationTarget,
) -> std::result::Result<BTreeMap<String, Value>, Box<dyn std::error::Error + Send + Sync>> {
    let domain = &candidate.compiled().ir.domain;
    let cluster = format!("apsides-qual-{RUN_ID}");
    let node_name = &target.node_name;
    let marker = qualification_marker(candidate, target);
    let authority_name = registry_authority_name(&domain.control_namespace, &domain.namespace)?;
    let authority = RegistryAuthority {
        control_namespace: domain.control_namespace.clone(),
        schema_version: REGISTRY_AUTHORITY_SCHEMA_VERSION.into(),
        registry_uid: REGISTRY_UID.into(),
        cluster_binding: domain.cluster_binding.clone(),
        workload_namespace: domain.namespace.clone(),
    };
    let mut marker_object = state_object(
        CANDIDATE_QUALIFICATION_MARKER_NAME,
        &cluster,
        &marker,
        &target.marker_uid,
        &target.marker_resource_version,
    )?;
    marker_object["immutable"] = json!(true);

    let mut objects = BTreeMap::new();
    for (name, value, uid, revision) in [
        (
            format!("apsides-registry-{}", domain.namespace),
            serde_json::to_value(registry)?,
            REGISTRY_UID,
            "42",
        ),
        (
            format!("apsides-trust-{}", domain.id),
            serde_json::to_value(trust)?,
            "trust-uid",
            "9",
        ),
        (
            format!("apsides-policy-{}", domain.id),
            serde_json::to_value(policy(candidate.compiled()))?,
            "policy-uid",
            "10",
        ),
        (
            format!("apsides-deploy-identity-{}", domain.id),
            serde_json::to_value(&identity.value)?,
            identity.uid.as_str(),
            identity.resource_version.as_str(),
        ),
    ] {
        objects.insert(
            format!(
                "/api/v1/namespaces/{}/configmaps/{name}",
                domain.control_namespace
            ),
            state_object(&name, &domain.control_namespace, &value, uid, revision)?,
        );
    }
    objects.insert(
        format!("/api/v1/namespaces/{REGISTRY_AUTHORITY_NAMESPACE}/configmaps/{authority_name}"),
        state_object(
            &authority_name,
            REGISTRY_AUTHORITY_NAMESPACE,
            &authority,
            "authority-uid",
            "11",
        )?,
    );
    for (name, uid) in [
        ("kube-system", &target.kube_system_namespace_uid),
        (
            domain.control_namespace.as_str(),
            &target.control_namespace_uid,
        ),
        (domain.namespace.as_str(), &target.workload_namespace_uid),
        (&cluster, &target.sentinel_namespace_uid),
    ] {
        objects.insert(format!("/api/v1/namespaces/{name}"), namespace(name, uid));
    }
    objects.insert(
        format!("/api/v1/namespaces/{cluster}/configmaps/{CANDIDATE_QUALIFICATION_MARKER_NAME}"),
        marker_object,
    );
    objects.insert(format!("/api/v1/nodes/{node_name}"), json!({
        "apiVersion":"v1", "kind":"Node", "metadata":{"name":node_name,"uid":target.node_uid,"resourceVersion":"4"},
        "spec":{"providerID":target.node_provider_id}, "status":{"conditions":[{"type":"Ready","status":"True"}]}
    }));
    Ok(objects)
}

fn wire_objects(objects: BTreeMap<String, Value>) -> (Client, Calls) {
    let objects = Arc::new(objects);
    let calls: Calls = Arc::default();
    let service = service_fn({
        let calls = calls.clone();
        move |request: Request<Body>| {
            let objects = objects.clone();
            let calls = calls.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = body
                    .collect()
                    .await
                    .map_err(std::io::Error::other)?
                    .to_bytes();
                let value = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).map_err(std::io::Error::other)?
                };
                calls
                    .lock()
                    .map_err(|_| std::io::Error::other("qualification calls lock"))?
                    .push((
                        parts.method.to_string(),
                        parts.uri.to_string(),
                        value.clone(),
                    ));
                let mut response = if parts.method == Method::PUT {
                    value
                } else {
                    objects.get(parts.uri.path()).cloned().ok_or_else(|| {
                        std::io::Error::other(format!(
                            "unexpected Kubernetes request: {} {}",
                            parts.method,
                            parts.uri.path()
                        ))
                    })?
                };
                if parts.method == Method::PUT {
                    response["metadata"]["resourceVersion"] = json!("43");
                }
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&response).map_err(std::io::Error::other)?,
                    ))
                    .map_err(std::io::Error::other)
            }
        }
    });
    (Client::new(service, "qualification"), calls)
}

fn platform(
    client: Client,
    candidate: &IntegrityVerifiedCandidate,
    fixture: &QualificationFixture,
    reviews: Arc<ObservedReviewStore>,
) -> platform::Result<PlatformClient> {
    let domain = &candidate.compiled().ir.domain;
    PlatformClient::new(
        client,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )?
    .with_claim_store(fixture.claim_store.clone())
    .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()))
    .with_review_store(reviews)
    .with_test_issued_identity(fixture.identity.clone(), FIXTURE_CREDENTIAL_EXPIRY)
}

fn make_noninitial(
    request: &mut deploy::DeploymentRequest,
    kind: &str,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let domain = &request.next.compiled.ir.domain;
    let names = deploy::allocation_names(REGISTRY_UID, 1)?;
    let allocation = deploy::GenerationAllocation {
        registry_uid: REGISTRY_UID.into(),
        sequence: 1,
        generation_id: names.generation_id,
        service_account_name: names.service_account_name,
        transition_id: names.transition_id,
        artifact_digest: request.next.artifact_digest.clone(),
        ir_digest: request.next.compiled.ir_digest.clone(),
        inventory_metadata_digest: format!("sha256:{}", "c".repeat(64)),
    };
    let identity = deploy::ResourceIdentity {
        api_group: String::new(),
        resource: "configmaps".into(),
        namespace: domain.namespace.clone(),
        name: "existing".into(),
    };
    match kind {
        "previous" => {
            request.previous = Some(Box::new(deploy::PredecessorGeneration {
                allocation,
                scope: inventory::InventoryScope {
                    cluster: domain.cluster_binding.clone(),
                    namespace: domain.namespace.clone(),
                    controller: domain.id.clone(),
                },
                control_namespace: domain.control_namespace.clone(),
                inventory: inventory::InventorySnapshot {
                    schema_version: inventory::INVENTORY_SCHEMA.into(),
                    generation: "previous".into(),
                    resources: vec![],
                },
                retirement_role_binding_namespaces: vec![],
            }));
        }
        "observation" => request.observations.push(deploy::RuntimeObservation {
            identity,
            uid: "existing-uid".into(),
        }),
        "intent" => {
            let approval: deploy::OwnershipApproval = serde_json::from_value(json!({
                "schemaVersion": deploy::OWNERSHIP_INTENT_SCHEMA,
                "transitionId": request.transition_id,
                "domainId": domain.id,
                "generationId": request.next.generation_id,
                "releaseDigest": request.next.artifact_digest,
                "clusterBinding": domain.cluster_binding,
                "controlNamespace": domain.control_namespace,
                "identity": identity,
                "observedUid": "existing-uid",
                "observedResourceVersion": "1",
                "priorManagedFieldsDigest": format!("sha256:{}", "d".repeat(64)),
                "permittedApply": {},
                "permittedRuntimeFields": [],
                "deletionPolicy": "orphan",
                "priorManagerConflicts": [],
                "action": {"kind": "adopt"}
            }))?;
            request.ownership_intents.push(deploy::OwnershipIntent {
                object_name: "approval".into(),
                object_uid: "approval-uid".into(),
                object_resource_version: "1".into(),
                approval,
            });
        }
        "sequence" => request.registry.next_sequence = 2,
        "claim" => request.registry.claims.push(deploy::ProtectedClaim {
            identity,
            domain_id: domain.id.clone(),
            generation_id: request.next.generation_id.clone(),
            transition_id: request.transition_id.clone(),
            state: deploy::ClaimState::Reserved,
            uid: None,
        }),
        "active" => request.registry.generation_history.active_allocation = Some(allocation),
        _ => return Err("unknown noninitial request mutation".into()),
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires APS_QUALIFICATION_CANDIDATE and APS_QUALIFICATION_TRUST_POLICY absolute paths"]
async fn public_candidate_qualification_rejects_drift_and_reserves_exact_candidate()
-> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidate_path = explicit_path(CANDIDATE_ENV)?;
    let trust_path = explicit_path(TRUST_ENV)?;
    let trust = read_trust(&trust_path)?;
    let candidate = release::verify_candidate(&candidate_path, &trust).await?;

    reject_mismatched_candidate(&candidate, &trust).await?;
    reject_noninitial_requests(&candidate, &trust).await?;
    reject_deficient_ceiling(&candidate, &trust).await?;
    reserve_and_reject_dirty_state(&candidate, &trust).await
}

async fn reject_mismatched_candidate(
    candidate: &IntegrityVerifiedCandidate,
    trust: &TrustPolicy,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for mismatch in ["compiled", "release", "scope"] {
        let fixture = QualificationFixture::new(candidate, trust)?;
        let (client, calls) = wire_objects(fixture.objects.clone());
        let reviews = Arc::new(ObservedReviewStore::default());
        let mut request = fixture.request.clone();
        let client = if mismatch == "scope" {
            PlatformClient::new(
                client,
                &candidate.compiled().ir.domain.control_namespace,
                &candidate.compiled().ir.domain.namespace,
                "different-cluster",
            )?
            .with_claim_store(fixture.claim_store.clone())
            .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()))
            .with_review_store(reviews)
        } else {
            platform(client, candidate, &fixture, reviews)?
        };
        if mismatch == "compiled" {
            request.next.compiled = plan()?;
        } else if mismatch == "release" {
            request.next.artifact_digest = format!("sha256:{}", "0".repeat(64));
        }
        let error = client
            .reserve_qualification_candidate(
                &request,
                "executor-a",
                candidate,
                &fixture.target,
                &MemoryInventoryStore::default(),
            )
            .await
            .err()
            .ok_or("mismatched qualification unexpectedly reserved")?;
        if !error.to_string().contains("exact signed candidate scope") {
            return Err(format!("unexpected qualification mismatch error: {error}").into());
        }
        if !calls
            .lock()
            .map_err(|_| "qualification calls lock")?
            .is_empty()
        {
            return Err(format!("{mismatch} mismatch reached the Kubernetes API").into());
        }
    }

    Ok(())
}

async fn reject_noninitial_requests(
    candidate: &IntegrityVerifiedCandidate,
    trust: &TrustPolicy,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for kind in [
        "previous",
        "observation",
        "intent",
        "sequence",
        "claim",
        "active",
    ] {
        let fixture = QualificationFixture::new(candidate, trust)?;
        let (client, calls) = wire_objects(fixture.objects.clone());
        let mut request = fixture.request.clone();
        make_noninitial(&mut request, kind)?;
        let error = platform(
            client,
            candidate,
            &fixture,
            Arc::new(ObservedReviewStore::default()),
        )?
        .reserve_qualification_candidate(
            &request,
            "executor-a",
            candidate,
            &fixture.target,
            &MemoryInventoryStore::default(),
        )
        .await
        .err()
        .ok_or("noninitial qualification unexpectedly reserved")?;
        if !error
            .to_string()
            .contains("unobserved initial installation")
            || !calls
                .lock()
                .map_err(|_| "qualification calls lock")?
                .is_empty()
        {
            return Err(format!("{kind} request did not fail closed before API access").into());
        }
    }

    Ok(())
}

async fn reject_deficient_ceiling(
    candidate: &IntegrityVerifiedCandidate,
    trust: &TrustPolicy,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut fixture = QualificationFixture::new(candidate, trust)?;
    let mut deficient_policy = policy(candidate.compiled());
    if deficient_policy
        .runtime_ceiling
        .capabilities
        .pop()
        .is_none()
    {
        return Err(
            "signed candidate has no runtime capability to remove for the ceiling test".into(),
        );
    }
    fixture.request.runtime_ceiling = deficient_policy.runtime_ceiling.clone();
    fixture.request.cleanup_ceiling = deficient_policy.cleanup_ceiling.clone();
    let domain = &candidate.compiled().ir.domain;
    fixture.objects.insert(
        format!(
            "/api/v1/namespaces/{}/configmaps/apsides-policy-{}",
            domain.control_namespace, domain.id
        ),
        state_object(
            &format!("apsides-policy-{}", domain.id),
            &domain.control_namespace,
            &deficient_policy,
            "policy-uid",
            "10",
        )?,
    );
    let initial_claim_rows = fixture
        .claim_store
        .rows
        .lock()
        .map_err(|_| "qualification claims lock")?
        .len();
    let (client, calls) = wire_objects(fixture.objects.clone());
    let reviews = Arc::new(ObservedReviewStore::default());
    let error = platform(client, candidate, &fixture, reviews.clone())?
        .reserve_qualification_candidate(
            &fixture.request,
            "executor-a",
            candidate,
            &fixture.target,
            &MemoryInventoryStore::default(),
        )
        .await
        .err()
        .ok_or("deficient platform ceiling unexpectedly reserved")?;
    if !matches!(
        &error,
        platform::Error::Deploy(deploy::Error::Core(compiler_core::Error::Ceiling(_)))
    ) {
        return Err(format!("unexpected deficient-ceiling error: {error}").into());
    }
    let claim_rows = fixture
        .claim_store
        .rows
        .lock()
        .map_err(|_| "qualification claims lock")?
        .len();
    if reviews.writes.load(Ordering::SeqCst) != 0
        || claim_rows != initial_claim_rows
        || calls
            .lock()
            .map_err(|_| "qualification calls lock")?
            .iter()
            .any(|(method, _, _)| method != "GET")
    {
        return Err("ceiling rejection mutated claim, review, or Kubernetes state".into());
    }

    Ok(())
}

async fn reserve_and_reject_dirty_state(
    candidate: &IntegrityVerifiedCandidate,
    trust: &TrustPolicy,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let fixture = QualificationFixture::new(candidate, trust)?;
    let (client, calls) = wire_objects(fixture.objects.clone());
    let reviews = Arc::new(ObservedReviewStore::default());
    let reserved = platform(client, candidate, &fixture, reviews.clone())?
        .reserve_qualification_candidate(
            &fixture.request,
            "executor-a",
            candidate,
            &fixture.target,
            &MemoryInventoryStore::default(),
        )
        .await?;
    if reserved.value.next_sequence != 2
        || !reserved
            .value
            .transitions
            .contains_key(&fixture.request.transition_id)
    {
        return Err("exact candidate did not produce its expected reservation".into());
    }
    let stored = reserved
        .value
        .transitions
        .get(&fixture.request.transition_id)
        .ok_or("exact candidate transition missing")?;
    if stored.record.phase != deploy::Phase::Planned
        || stored.allocation().generation_id != fixture.request.next.generation_id
        || stored.allocation().artifact_digest != candidate.release_digest()
    {
        return Err("exact candidate reservation has the wrong phase or allocation".into());
    }
    let writes = calls
        .lock()
        .map_err(|_| "qualification calls lock")?
        .iter()
        .filter(|(method, _, _)| method == "PUT")
        .count();
    if writes != 1 || reviews.writes.load(Ordering::SeqCst) != 1 {
        return Err(
            "exact candidate reservation did not publish one review and one registry update".into(),
        );
    }

    reject_dirty_state(candidate, &fixture, &reserved.value, &reviews).await
}

async fn reject_dirty_state(
    candidate: &IntegrityVerifiedCandidate,
    fixture: &QualificationFixture,
    reserved: &NamespaceRegistry,
    reviews: &Arc<ObservedReviewStore>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for kind in ["transition", "claim"] {
        let mut dirty = reserved.clone();
        if kind == "transition" {
            dirty.claims.clear();
        } else {
            dirty.transitions.clear();
        }
        let claim_reference = fixture.claim_store.seed(dirty.claim_snapshot()?)?;
        let dirty_state = dirty.state(&claim_reference)?;
        let domain = &candidate.compiled().ir.domain;
        let mut objects = fixture.objects.clone();
        objects.insert(
            format!(
                "/api/v1/namespaces/{}/configmaps/apsides-registry-{}",
                domain.control_namespace, domain.namespace
            ),
            state_object(
                &format!("apsides-registry-{}", domain.namespace),
                &domain.control_namespace,
                &dirty_state,
                REGISTRY_UID,
                "43",
            )?,
        );
        let before_claims = fixture
            .claim_store
            .rows
            .lock()
            .map_err(|_| "qualification claims lock")?
            .clone();
        let before_reviews = reviews.writes.load(Ordering::SeqCst);
        let (client, calls) = wire_objects(objects);
        let error = platform(client, candidate, fixture, reviews.clone())?
            .reserve_qualification_candidate(
                &fixture.request,
                "executor-a",
                candidate,
                &fixture.target,
                &MemoryInventoryStore::default(),
            )
            .await
            .err()
            .ok_or("qualification accepted dirty protected state")?;
        if !error
            .to_string()
            .contains("fresh protected allocation state")
            || *fixture
                .claim_store
                .rows
                .lock()
                .map_err(|_| "qualification claims lock")?
                != before_claims
            || reviews.writes.load(Ordering::SeqCst) != before_reviews
            || calls
                .lock()
                .map_err(|_| "qualification calls lock")?
                .iter()
                .any(|(method, _, _)| method != "GET")
        {
            return Err(format!("current {kind} did not fail closed").into());
        }
    }
    Ok(())
}
