async fn prepare_request(
    platform: &PlatformClient,
    candidate: &IntegrityVerifiedCandidate,
    inventory: &dyn InventoryStore,
    preparation: RequestPreparation,
) -> Result<Prepared> {
    let domain = &candidate.compiled().ir.domain;
    let policy = platform
        .load_policy(&domain.id)
        .await
        .map_err(|_| DriverError("protected deployment policy load failed"))?;
    let mut registry = platform
        .load_registry()
        .await
        .map_err(|_| DriverError("protected registry load failed"))?;
    let previous =
        prepare_predecessor(platform, domain, inventory, &mut registry, preparation).await?;
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
    let request = DeploymentRequest {
        transition_id: names.transition_id,
        previous,
        next: GenerationPlan {
            generation_id: names.generation_id,
            artifact_digest: candidate.release_digest().into(),
            compiled: candidate.compiled().clone(),
        },
        runtime_ceiling: policy.value.runtime_ceiling,
        cleanup_ceiling: policy.value.cleanup_ceiling,
        registry: RegistrySnapshot {
            registry_uid: registry.uid.clone(),
            next_sequence: registry.value.next_sequence,
            revision: registry.resource_version.clone(),
            claims: registry.value.claims.clone(),
            generation_history: history,
        },
        observations: vec![],
        ownership_intents: vec![],
    };
    let request_digest = compiler_core::digest(&request)
        .map_err(|_| DriverError("deployment request digest failed"))?;
    Ok(Prepared {
        registry,
        request,
        request_digest,
    })
}

fn candidate_policy(compiled: &compiler_core::CompiledPlan, pull_secrets: &[String]) -> Result<platform_api::PolicyState> {
    let domain = &compiled.ir.domain;
    let ceiling = compiler_core::PermissionCeiling {
        policy_ref: domain.policy_ref.clone(),
        domain_id: domain.id.clone(),
        cluster_binding: domain.cluster_binding.clone(),
        workload_namespace: domain.namespace.clone(),
        control_namespace: domain.control_namespace.clone(),
        capabilities: compiled.capabilities.clone(),
        accepted_enforcement_gaps: compiled.enforcement_gaps.clone(),
        accepted_indirect_risks: compiled.indirect_risks.clone(),
    };
    let policy = platform_api::PolicyState {
        image_pull_secrets: pull_secrets.to_vec(),
        schema_version: platform_api::SCHEMA_VERSION.into(),
        runtime_ceiling: ceiling.clone(),
        cleanup_ceiling: ceiling,
    };
    policy.validate_image_pull_secrets().map_err(|_| DriverError("invalid image pull Secret references"))?;
    Ok(policy)
}

fn prepared_targets(
    request: &DeploymentRequest,
    target: &CandidatePairQualificationTarget,
    role: CandidatePairRole,
    pull_secrets: &[String],
) -> Result<(
    Vec<platform_api::CreationTarget>,
    Vec<platform_api::CreationTarget>,
)> {
    let compiled = &request.next.compiled;
    let policy = match target.purpose {
        platform_api::CandidatePairPurpose::PermissionExpansion {} => candidate_policy(compiled, pull_secrets)?,
        platform_api::CandidatePairPurpose::LastTypeCleanup { .. } => platform_api::PolicyState {
            image_pull_secrets: pull_secrets.to_vec(),
            schema_version: platform_api::SCHEMA_VERSION.into(),
            runtime_ceiling: request.runtime_ceiling.clone(),
            cleanup_ceiling: request.cleanup_ceiling.clone(),
        },
    };
    policy.validate_image_pull_secrets().map_err(|_| DriverError("invalid image pull Secret references"))?;
    let expected = match role {
        CandidatePairRole::Initial => &target.initial_policy_digest,
        CandidatePairRole::Successor => &target.successor_policy_digest,
    };
    if compiler_core::digest(&policy)
        .map_err(|_| DriverError("candidate target policy digest failed"))?
        != *expected
    {
        return Err(DriverError(
            "candidate target projection differs from approved policy",
        ));
    }
    // Project names under the marker-bound candidate policy without changing the
    // actual request: rejection still runs against the current initial policy.
    let mut projected = request.clone();
    projected.runtime_ceiling = policy.runtime_ceiling;
    projected.cleanup_ceiling = policy.cleanup_ceiling;
    let plan = deploy::plan_transition(&projected)
        .map_err(|_| DriverError("candidate target plan projection failed"))?;
    let creates = platform_api::deployment_permissions(&plan)
        .map_err(|_| DriverError("candidate target permission projection failed"))?
        .creation_targets;
    let mut managed = Vec::with_capacity(compiled.ir.resources.len());
    for resource in &compiled.ir.resources {
        let identity = deploy::resource_identity(resource)
            .map_err(|_| DriverError("candidate managed target projection failed"))?;
        managed.push(platform_api::CreationTarget {
            namespace: identity.namespace,
            api_group: identity.api_group,
            resource: identity.resource,
            name: identity.name,
        });
    }
    managed.sort();
    managed.dedup();
    Ok((creates, managed))
}

async fn prepare_predecessor(
    platform: &PlatformClient,
    domain: &compiler_core::Domain,
    inventory: &dyn InventoryStore,
    registry: &mut platform_api::Versioned<platform_api::NamespaceRegistry>,
    preparation: RequestPreparation,
) -> Result<Option<Box<deploy::PredecessorGeneration>>> {
    let scope = inventory::InventoryScope {
        cluster: domain.cluster_binding.clone(),
        namespace: domain.namespace.clone(),
        controller: domain.id.clone(),
    };
    let head = inventory
        .load(&scope)
        .await
        .map_err(|_| DriverError("inventory head load failed"))?;
    let active = registry
        .value
        .histories
        .get(&domain.id)
        .and_then(|history| history.active_allocation.as_ref());
    if head
        .as_ref()
        .map(|value| value.snapshot.generation.as_str())
        != active.map(|allocation| allocation.generation_id.as_str())
    {
        return Err(DriverError(
            "inventory head differs from active pair generation",
        ));
    }
    let Some(head) = head else {
        return Ok(None);
    };
    platform
        .ensure_active_writer_checkpoint(&domain.id)
        .await
        .map_err(|_| DriverError("active writer checkpoint validation failed"))?;
    let missing_uids = head
        .snapshot
        .resources
        .iter()
        .any(|resource| resource.uid.is_none());
    let snapshot = if preparation == RequestPreparation::CapturePredecessor && missing_uids {
        *registry = platform
            .capture_active_claim_uids(&domain.id, &head.snapshot, |_, _| async {
                Ok::<(), platform_api::Error>(())
            })
            .await
            .map_err(claim_capture_error)?;
        platform
            .publish_captured_inventory(inventory, &domain.id, registry, &head)
            .await
            .map_err(|_| DriverError("captured inventory publication failed"))?
            .snapshot
    } else {
        if missing_uids {
            return Err(DriverError(
                "read-only predecessor inventory lacks captured UIDs",
            ));
        }
        head.snapshot
    };
    let checkpoint = registry
        .value
        .active_writer_checkpoints
        .get(&domain.id)
        .ok_or(DriverError("active writer checkpoint is absent"))?;
    checkpoint
        .predecessor_metadata(&scope, &snapshot)
        .map(Box::new)
        .map(Some)
        .map_err(|_| DriverError("predecessor metadata reconstruction failed"))
}

fn claim_capture_error(error: platform_api::Error) -> DriverError {
    match error {
        platform_api::Error::Kubernetes(kube::Error::Api(response)) if response.code == 403 => {
            DriverError("active claim UID capture authorization denied")
        }
        platform_api::Error::Kubernetes(_) => {
            DriverError("active claim UID capture Kubernetes request failed")
        }
        platform_api::Error::Invalid(message)
            if message == "live cleanup candidate lacks exact approved Apply ownership" =>
        {
            DriverError("active claim UID capture lacks approved Apply ownership")
        }
        platform_api::Error::Invalid(message)
            if message == "live resource is a replacement of the protected UID" =>
        {
            DriverError("active claim UID capture observed a replacement")
        }
        platform_api::Error::Coordination(_) => {
            DriverError("active claim UID capture coordination failed")
        }
        _ => DriverError("active claim UID capture failed"),
    }
}

async fn bootstrap_platform(
    candidate: &IntegrityVerifiedCandidate,
    coordination: Arc<coordination::SqliteCoordination>,
) -> Result<PlatformClient> {
    let config = qualification_driver::explicit_config(ConfigIdentity::Admin).await?;
    configured_platform(candidate, config, coordination)
}

async fn issued_deployment(
    candidate: &IntegrityVerifiedCandidate,
    coordination: Arc<coordination::SqliteCoordination>,
) -> Result<IssuedDeployment> {
    let config = qualification_driver::explicit_config(ConfigIdentity::Issuer {
        name: ISSUER,
        error: "permission upgrade kubeconfig is not the dedicated issuer identity",
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

async fn issued_platform(candidate: &IntegrityVerifiedCandidate) -> Result<PlatformClient> {
    let coordination = coordination_store().await?;
    let issued = issued_deployment(candidate, coordination).await?;
    Ok(issued.platform().clone())
}

fn configured_platform(
    candidate: &IntegrityVerifiedCandidate,
    config: kube::Config,
    coordination: Arc<coordination::SqliteCoordination>,
) -> Result<PlatformClient> {
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

async fn coordination_store() -> Result<Arc<coordination::SqliteCoordination>> {
    qualification_driver::coordination_store("APS_PERMISSION_UPGRADE_COORDINATION_DB").await
}

async fn inventory_store() -> Result<inventory::SqliteInventory> {
    qualification_driver::inventory_store("APS_PERMISSION_UPGRADE_INVENTORY_DB").await
}

const fn candidate(pair: &CandidatePair, role: CandidatePairRole) -> &IntegrityVerifiedCandidate {
    match role {
        CandidatePairRole::Initial => &pair.initial,
        CandidatePairRole::Successor => &pair.successor,
    }
}

fn required_target(settings: &Settings) -> Result<&CandidatePairQualificationTarget> {
    settings
        .target
        .as_ref()
        .ok_or(DriverError("candidate pair target is missing"))
}

fn required_transition(settings: &Settings) -> Result<&str> {
    qualification_driver::required(
        settings.transition.as_deref(),
        "pair transition identity is missing",
    )
}

fn require_request_digest(settings: &Settings, actual: &str) -> Result<()> {
    let expected = qualification_driver::required(
        settings.expected_request_digest.as_deref(),
        "expected request digest is missing",
    )?;
    if !qualification_driver::valid_digest(expected) || expected != actual {
        return Err(DriverError(
            "protected request differs from prepared digest",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum FieldSet {
    Verify,
    Bootstrap,
    TargetOnly,
    RequestMutation,
    Rejection,
    Transition,
}

const fn require_fields(settings: &Settings, set: FieldSet) -> Result<()> {
    let target = settings.target.is_some();
    let bootstrap = settings.registry_uid.is_some() || settings.registry_resource_version.is_some();
    let request = settings.expected_request_digest.is_some();
    let missing = settings.expected_missing_capability.is_some();
    let transition = settings.transition.is_some();
    let valid = match set {
        FieldSet::Verify => !target && !bootstrap && !request && !missing && !transition,
        FieldSet::Bootstrap => target && !request && !missing && !transition,
        FieldSet::TargetOnly => target && !bootstrap && !request && !missing && !transition,
        FieldSet::RequestMutation => target && !bootstrap && request && !missing && !transition,
        FieldSet::Rejection => target && !bootstrap && request && missing && !transition,
        FieldSet::Transition => target && !bootstrap && !request && !missing && transition,
    };
    if !valid
        || matches!(set, FieldSet::Bootstrap)
            && (settings.registry_uid.is_none() || settings.registry_resource_version.is_none())
    {
        return Err(DriverError("action settings fields are invalid"));
    }
    Ok(())
}

fn report_registry(
    outcome: &'static str,
    pair: &CandidatePair,
    registry: &platform_api::Versioned<platform_api::NamespaceRegistry>,
    transition_id: &str,
    request_digest: &str,
) -> Result<()> {
    qualification_driver::bounded_metadata(&registry.uid, &registry.resource_version)?;
    let transition = registry
        .value
        .transition(transition_id)
        .map_err(|_| DriverError("pair transition result is absent"))?;
    if transition.record.executor_id != platform_api::candidate_pair_qualification_executor() {
        return Err(DriverError("pair transition executor identity differs"));
    }
    emit(
        outcome,
        pair,
        json!({
            "transition":transition.allocation().transition_id,
            "generation":transition.allocation().generation_id,
            "phase":phase_name(transition.record.phase),
            "registryUid":registry.uid,
            "registryResourceVersion":registry.resource_version,
            "requestDigest":request_digest,
        }),
    )
}

fn emit(outcome: &'static str, pair: &CandidatePair, fields: Value) -> Result<()> {
    let mut value = json!({
        "schemaVersion":RESULT_SCHEMA,
        "outcome":outcome,
        "candidates":candidate_digests(pair),
    });
    let target = value
        .as_object_mut()
        .ok_or(DriverError("qualification result construction failed"))?;
    let Value::Object(fields) = fields else {
        return Err(DriverError("qualification result fields are invalid"));
    };
    for (key, value) in fields {
        if target.insert(key, value).is_some() {
            return Err(DriverError("qualification result field collision"));
        }
    }
    qualification_driver::emit(&value, RESULT_PREFIX)
}

fn candidate_digests(pair: &CandidatePair) -> CandidateDigests<'_> {
    CandidateDigests {
        initial_release: pair.initial.release_digest(),
        initial_ir: &pair.initial.compiled().ir_digest,
        initial_provenance: pair.initial.provenance_digest(),
        successor_release: pair.successor.release_digest(),
        successor_ir: &pair.successor.compiled().ir_digest,
        successor_provenance: pair.successor.provenance_digest(),
        trust_policy: pair.initial.trust_policy_digest(),
    }
}

const fn operation_role(operation: CandidatePairLifecycleOperation) -> CandidatePairRole {
    match operation {
        CandidatePairLifecycleOperation::StageInitial
        | CandidatePairLifecycleOperation::ActivateInitial => CandidatePairRole::Initial,
        CandidatePairLifecycleOperation::StageSuccessor
        | CandidatePairLifecycleOperation::QuiesceUpgrade
        | CandidatePairLifecycleOperation::ActivateUpgrade
        | CandidatePairLifecycleOperation::CommitUpgrade => CandidatePairRole::Successor,
    }
}

const fn operation_name(operation: CandidatePairLifecycleOperation) -> &'static str {
    match operation {
        CandidatePairLifecycleOperation::StageInitial => "initial-staged",
        CandidatePairLifecycleOperation::ActivateInitial => "initial-activated",
        CandidatePairLifecycleOperation::StageSuccessor => "successor-staged",
        CandidatePairLifecycleOperation::QuiesceUpgrade => "upgrade-quiesced",
        CandidatePairLifecycleOperation::ActivateUpgrade => "upgrade-activated",
        CandidatePairLifecycleOperation::CommitUpgrade => "upgrade-committed",
    }
}

const fn operation_pending_name(operation: CandidatePairLifecycleOperation) -> &'static str {
    match operation {
        CandidatePairLifecycleOperation::StageInitial => "initial-stage-pending",
        CandidatePairLifecycleOperation::ActivateInitial => "initial-activation-pending",
        CandidatePairLifecycleOperation::StageSuccessor => "successor-stage-pending",
        CandidatePairLifecycleOperation::QuiesceUpgrade => "upgrade-quiescence-pending",
        CandidatePairLifecycleOperation::ActivateUpgrade => "upgrade-activation-pending",
        CandidatePairLifecycleOperation::CommitUpgrade => "upgrade-commit-pending",
    }
}

const fn role_name(role: CandidatePairRole) -> &'static str {
    match role {
        CandidatePairRole::Initial => "initial",
        CandidatePairRole::Successor => "successor",
    }
}

const fn phase_name(phase: deploy::Phase) -> &'static str {
    match phase {
        deploy::Phase::Planned => "Planned",
        deploy::Phase::Staged => "Staged",
        deploy::Phase::Quiesced => "Quiesced",
        deploy::Phase::Activated => "Activated",
        deploy::Phase::Cleaning => "Cleaning",
        deploy::Phase::Blocked => "Blocked",
        deploy::Phase::Committed => "Committed",
    }
}
