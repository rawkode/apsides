async fn bootstrap(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Bootstrap)?;
    let target = required_target(settings)?;
    let uid = qualification_driver::required(
        settings.registry_uid.as_deref(),
        "bootstrap registry UID is missing",
    )?;
    let revision = qualification_driver::required(
        settings.registry_resource_version.as_deref(),
        "bootstrap registry resourceVersion is missing",
    )?;
    let coordination = coordination_store().await?;
    let _inventory = inventory_store().await?;
    let platform = bootstrap_platform(&pair.initial, coordination).await?;
    platform
        .authorize_candidate_qualification_pair(
            CandidatePairRole::Initial,
            &pair.initial,
            &pair.successor,
            target,
        )
        .await
        .map_err(|_| DriverError("candidate pair target authorization failed"))?;
    let domain = &pair.initial.compiled().ir.domain.id;
    let written = platform
        .bootstrap_registry(uid, revision, std::slice::from_ref(domain))
        .await
        .map_err(|_| DriverError("protected registry bootstrap failed"))?;
    qualification_driver::bounded_metadata(&written.uid, &written.resource_version)?;
    emit(
        "bootstrapped",
        pair,
        json!({"registryUid":written.uid,"registryResourceVersion":written.resource_version}),
    )
}

async fn prepare(
    settings: &Settings,
    pair: &CandidatePair,
    policy_role: CandidatePairRole,
) -> Result<()> {
    require_fields(settings, FieldSet::TargetOnly)?;
    let coordination = coordination_store().await?;
    let inventory = inventory_store().await?;
    let issued = issued_deployment(&pair.initial, coordination).await?;
    let platform = issued.platform();
    let _authorized = authorize(platform, policy_role, pair, required_target(settings)?).await?;
    let request_role = match settings.action {
        Action::PrepareInitial => CandidatePairRole::Initial,
        Action::PrepareRejection | Action::PrepareSuccessor => CandidatePairRole::Successor,
        _ => return Err(DriverError("prepare action is invalid")),
    };
    let prepared = prepare_request(
        platform,
        candidate(pair, request_role),
        &inventory,
        RequestPreparation::CapturePredecessor,
    )
    .await?;
    let names = deploy::allocation_names(
        &prepared.registry.uid,
        prepared.registry.value.next_sequence,
    )
    .map_err(|_| DriverError("canonical allocation derivation failed"))?;
    let (creation_targets, managed_targets) =
        prepared_targets(&prepared.request, required_target(settings)?, request_role)?;
    emit(
        match settings.action {
            Action::PrepareInitial => "initial-prepared",
            Action::PrepareRejection => "rejection-prepared",
            Action::PrepareSuccessor => "successor-prepared",
            _ => return Err(DriverError("prepare action is invalid")),
        },
        pair,
        json!({
            "generation":names.generation_id,
            "serviceAccountName":names.service_account_name,
            "creationTargetsSource":"authorized-candidate-policy-projection",
            "creationTargets":creation_targets,
            "managedTargets":managed_targets,
            "transition":names.transition_id,
            "registryUid":prepared.registry.uid,
            "registryResourceVersion":prepared.registry.resource_version,
            "requestDigest":prepared.request_digest,
            "policyRole":role_name(policy_role),
        }),
    )
}

async fn reserve_initial(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::RequestMutation)?;
    let platform = issued_platform(&pair.initial).await?;
    let inventory = inventory_store().await?;
    let authorized = authorize(
        &platform,
        CandidatePairRole::Initial,
        pair,
        required_target(settings)?,
    )
    .await?;
    let prepared = prepare_request(
        &platform,
        &pair.initial,
        &inventory,
        RequestPreparation::ReadOnly,
    )
    .await?;
    require_request_digest(settings, &prepared.request_digest)?;
    let written = platform
        .reserve_pair_initial_candidate(&prepared.request, &authorized, &inventory)
        .await
        .map_err(|_| DriverError("initial candidate reservation failed"))?;
    report_registry(
        "initial-reserved",
        pair,
        &written,
        &prepared.request.transition_id,
        &prepared.request_digest,
    )
}

async fn reject_upgrade(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Rejection)?;
    if !matches!(
        settings.purpose,
        platform_api::CandidatePairPurpose::PermissionExpansion {}
    ) {
        return Err(DriverError(
            "permission rejection requires expansion purpose",
        ));
    }
    let missing = settings
        .expected_missing_capability
        .as_ref()
        .ok_or(DriverError("expected missing capability is absent"))?;
    let platform = issued_platform(&pair.initial).await?;
    let inventory = inventory_store().await?;
    let authorized = authorize(
        &platform,
        CandidatePairRole::Initial,
        pair,
        required_target(settings)?,
    )
    .await?;
    let prepared = prepare_request(
        &platform,
        &pair.successor,
        &inventory,
        RequestPreparation::ReadOnly,
    )
    .await?;
    require_request_digest(settings, &prepared.request_digest)?;
    platform
        .reject_pair_upgrade_candidate(&prepared.request, &authorized, &inventory, missing)
        .await
        .map_err(|_| DriverError("typed permission upgrade rejection failed"))?;
    let durable = platform
        .load_registry()
        .await
        .map_err(|_| DriverError("rejection durable readback failed"))?;
    if durable.uid != prepared.registry.uid
        || durable.resource_version != prepared.registry.resource_version
        || durable.value != prepared.registry.value
    {
        return Err(DriverError("rejection changed protected registry state"));
    }
    let missing_digest = compiler_core::digest(missing)
        .map_err(|_| DriverError("missing capability digest failed"))?;
    emit(
        "upgrade-rejected",
        pair,
        json!({
            "registryUid":durable.uid,
            "registryResourceVersion":durable.resource_version,
            "requestDigest":prepared.request_digest,
            "missingCapabilityDigest":missing_digest,
        }),
    )
}

async fn reserve_successor(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::RequestMutation)?;
    let platform = issued_platform(&pair.successor).await?;
    let inventory = inventory_store().await?;
    let authorized = authorize(
        &platform,
        CandidatePairRole::Successor,
        pair,
        required_target(settings)?,
    )
    .await?;
    let prepared = prepare_request(
        &platform,
        &pair.successor,
        &inventory,
        RequestPreparation::ReadOnly,
    )
    .await?;
    require_request_digest(settings, &prepared.request_digest)?;
    let written = platform
        .reserve_pair_upgrade_candidate(&prepared.request, &authorized, &inventory)
        .await
        .map_err(|_| DriverError("successor candidate reservation failed"))?;
    report_registry(
        "successor-reserved",
        pair,
        &written,
        &prepared.request.transition_id,
        &prepared.request_digest,
    )
}

async fn reject_uncertain_writer(settings: &Settings, pair: &CandidatePair) -> Result<()> {
    require_fields(settings, FieldSet::Transition)?;
    let platform = issued_platform(&pair.successor).await?;
    let inventory = inventory_store().await?;
    let authorized = authorize(
        &platform,
        CandidatePairRole::Successor,
        pair,
        required_target(settings)?,
    )
    .await?;
    let transition = required_transition(settings)?;
    let before = platform
        .load_registry()
        .await
        .map_err(|_| DriverError("uncertain writer registry readback failed"))?;
    if before
        .value
        .transition(transition)
        .map_err(|_| DriverError("uncertain writer transition is absent"))?
        .record
        .phase
        != deploy::Phase::Quiesced
    {
        return Err(DriverError("uncertain writer experiment requires Quiesced"));
    }
    let result = platform
        .run_pair_lifecycle_operation(
            CandidatePairLifecycleOperation::ActivateUpgrade,
            &authorized,
            &inventory,
            transition,
        )
        .await;
    if !matches!(result, Err(platform_api::Error::Invalid(message))
        if message == "live writer set differs from complete protected Pod history")
    {
        return Err(DriverError(
            "uncertain writer activation did not refuse missing protected Pod history",
        ));
    }
    let after = platform
        .load_registry()
        .await
        .map_err(|_| DriverError("uncertain writer refusal readback failed"))?;
    if before.uid != after.uid
        || before.resource_version != after.resource_version
        || before.value != after.value
    {
        return Err(DriverError(
            "uncertain writer refusal changed protected registry state",
        ));
    }
    report_registry("uncertain-writer-rejected", pair, &after, transition, "")
}

async fn lifecycle(
    settings: &Settings,
    pair: &CandidatePair,
    operation: CandidatePairLifecycleOperation,
) -> Result<()> {
    require_fields(settings, FieldSet::Transition)?;
    let role = operation_role(operation);
    let platform = issued_platform(candidate(pair, role)).await?;
    let inventory = inventory_store().await?;
    let authorized = authorize(&platform, role, pair, required_target(settings)?).await?;
    let transition = required_transition(settings)?;
    let written = platform
        .run_pair_lifecycle_operation(operation, &authorized, &inventory, transition)
        .await;
    match written {
        Ok(written) => report_registry(operation_name(operation), pair, &written, transition, ""),
        Err(platform_api::Error::Deploy(deploy::Error::CleanupUidMismatch))
            if operation == CandidatePairLifecycleOperation::CommitUpgrade
                && matches!(
                    settings.purpose,
                    platform_api::CandidatePairPurpose::LastTypeCleanup { .. }
                ) =>
        {
            let resolved = platform
                .resolve_pair_transition(
                    CandidatePairRole::Successor,
                    &authorized,
                    &inventory,
                    transition,
                )
                .await
                .map_err(|_| DriverError("cleanup refusal state readback failed"))?;
            require_retained_cleanup_claim(&resolved, &settings.purpose)?;
            report_registry(
                "cleanup-uid-rejected",
                pair,
                resolved.registry(),
                transition,
                "",
            )
        }
        Err(platform_api::Error::Pending(_)) => {
            if operation == CandidatePairLifecycleOperation::CommitUpgrade
                && matches!(
                    settings.purpose,
                    platform_api::CandidatePairPurpose::LastTypeCleanup { .. }
                )
            {
                let resolved = platform
                    .resolve_pair_transition(
                        CandidatePairRole::Successor,
                        &authorized,
                        &inventory,
                        transition,
                    )
                    .await
                    .map_err(|_| DriverError("pending cleanup state readback failed"))?;
                require_retained_cleanup_claim(&resolved, &settings.purpose)?;
                return report_registry(
                    operation_pending_name(operation),
                    pair,
                    resolved.registry(),
                    transition,
                    "",
                );
            }
            let registry = platform
                .load_registry()
                .await
                .map_err(|_| DriverError("pending lifecycle state readback failed"))?;
            report_registry(
                operation_pending_name(operation),
                pair,
                &registry,
                transition,
                "",
            )
        }
        Err(_) => Err(DriverError("pair lifecycle operation failed")),
    }
}

fn require_retained_cleanup_claim(
    resolved: &platform_api::ResolvedTransition,
    purpose: &platform_api::CandidatePairPurpose,
) -> Result<()> {
    let platform_api::CandidatePairPurpose::LastTypeCleanup { identity } = purpose else {
        return Err(DriverError("cleanup refusal requires cleanup purpose"));
    };
    let plan = resolved.plan();
    let [item] = plan.cleanup.as_slice() else {
        return Err(DriverError(
            "cleanup refusal lacks one exact cleanup target",
        ));
    };
    let claims: Vec<_> = resolved
        .registry()
        .value
        .claims
        .iter()
        .filter(|claim| &claim.identity == identity)
        .collect();
    let [claim] = claims.as_slice() else {
        return Err(DriverError("cleanup refusal lost the protected claim"));
    };
    if resolved.stored().record.phase != deploy::Phase::Cleaning
        || &item.identity != identity
        || item.policy != compiler_core::DeletionPolicy::Delete
        || item.expected_uid.as_ref().is_none_or(String::is_empty)
        || claim.uid != item.expected_uid
        || claim.state != deploy::ClaimState::Releasing
        || claim.transition_id != plan.request.transition_id
        || claim.generation_id != item.prior_generation_id
        || claim.domain_id != plan.request.next.compiled.ir.domain.id
    {
        return Err(DriverError(
            "cleanup refusal changed protected cleanup authority",
        ));
    }
    Ok(())
}

async fn publish(settings: &Settings, pair: &CandidatePair, role: CandidatePairRole) -> Result<()> {
    require_fields(settings, FieldSet::Transition)?;
    let platform = issued_platform(candidate(pair, role)).await?;
    let inventory = inventory_store().await?;
    let authorized = authorize(&platform, role, pair, required_target(settings)?).await?;
    let transition = required_transition(settings)?;
    let published = platform
        .publish_pair_inventory(role, &authorized, &inventory, transition)
        .await
        .map_err(|_| DriverError("committed pair inventory publication failed"))?;
    qualification_driver::bounded_metadata(&published.snapshot_id, &published.revision)?;
    emit(
        if role == CandidatePairRole::Initial {
            "initial-inventory-published"
        } else {
            "successor-inventory-published"
        },
        pair,
        json!({
            "transition":transition,
            "inventoryGeneration":published.snapshot.generation,
            "inventorySnapshotId":published.snapshot_id,
            "inventoryRevision":published.revision,
        }),
    )
}

async fn resolve(settings: &Settings, pair: &CandidatePair, role: CandidatePairRole) -> Result<()> {
    require_fields(settings, FieldSet::Transition)?;
    let platform = issued_platform(candidate(pair, role)).await?;
    let inventory = inventory_store().await?;
    let authorized = authorize(&platform, role, pair, required_target(settings)?).await?;
    let transition = required_transition(settings)?;
    let resolved = platform
        .resolve_pair_transition(role, &authorized, &inventory, transition)
        .await
        .map_err(|_| DriverError("pair transition resolution failed"))?;
    let stored = resolved.stored();
    qualification_driver::bounded_metadata(
        &resolved.registry().uid,
        &resolved.registry().resource_version,
    )?;
    emit(
        if role == CandidatePairRole::Initial {
            "initial-resolved"
        } else {
            "successor-resolved"
        },
        pair,
        json!({
            "transition":transition,
            "generation":stored.allocation().generation_id,
            "phase":phase_name(stored.record.phase),
            "registryUid":resolved.registry().uid,
            "registryResourceVersion":resolved.registry().resource_version,
        }),
    )
}

async fn authorize<'a>(
    platform: &PlatformClient,
    role: CandidatePairRole,
    pair: &'a CandidatePair,
    target: &'a CandidatePairQualificationTarget,
) -> Result<platform_api::CandidateQualificationPair<'a>> {
    platform
        .authorize_candidate_qualification_pair(role, &pair.initial, &pair.successor, target)
        .await
        .map_err(|_| DriverError("candidate pair target authorization failed"))
}
