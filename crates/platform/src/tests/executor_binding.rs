use super::*;
use serde_json::json;
use std::collections::BTreeSet;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn bound_fixture(
    registry: &LifecycleRegistry,
    client: Client,
    expires_at: &str,
    identity: Versioned<DeploymentIdentity>,
) -> TestResult<PlatformClient> {
    let stored = registry.transition(TRANSITION_A)?;
    let next = fixture_review_store()
        .read(&stored.next_review_metadata)?
        .ok_or("fixture next review missing")?;
    let previous = stored
        .previous_review_metadata
        .as_ref()
        .map(|reference| {
            fixture_review_store()
                .read(reference)?
                .ok_or(coordination::Error::Invalid(
                    "fixture previous review missing",
                ))
        })
        .transpose()?;
    Ok(
        PlatformClient::new(client, "control", "workload", "cluster-a")?
            .with_claim_store(fixture_claim_store().clone())
            .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()))
            .with_review_store(fixture_review_store().clone())
            .with_test_issued_identity(identity, expires_at)?
            .bind_test_plan(
                registry.plan(TRANSITION_A)?,
                stored.previous_writer_checkpoint.as_ref(),
                &stored.capacity,
                &next,
                previous.as_ref(),
                &stored.executor_binding,
            )?,
    )
}

#[tokio::test]
async fn reservation_persists_exact_executor_binding_in_its_single_registry_cas() -> TestResult {
    let ReservationFixture {
        registry,
        policy,
        request,
    } = reservation_fixture()?;
    let before = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, calls) = wire(vec![
        before.clone(),
        object("policy", &policy, "policy-uid", "40"),
        before,
        json!("echo-put"),
    ]);
    let client = platform_client(client, "control", "workload", "cluster-a")?;
    let written = reserve_test_transition(&client, &request, "ci_job_1").await?;
    let plan = deploy::plan_transition(&request)?;
    let stored = written.value.transition(TRANSITION_A)?;
    let expected = client.executor_binding_for_test(
        REGISTRY_UID,
        &stored.inputs.allocation,
        "example",
        "ci_job_1",
    )?;
    assert_eq!(stored.executor_binding, expected);
    assert_eq!(
        stored.capacity,
        crate::completion_envelope::calculate(&written.value, &plan, &stored.capacity.profile,)?
    );

    let calls = calls.lock().map_err(|_| "request lock poisoned")?;
    let puts = calls
        .iter()
        .filter(|(method, _, _)| method == "PUT")
        .collect::<Vec<_>>();
    assert_eq!(puts.len(), 1);
    let encoded = puts[0]
        .2
        .pointer("/data/state.json")
        .and_then(Value::as_str)
        .ok_or("registry PUT state missing")?;
    let wire: Value = serde_json::from_str(encoded)?;
    assert_eq!(
        wire.pointer(&format!(
            "/transitions/{TRANSITION_A}/executorBinding/identityApprovalUid"
        )),
        Some(&json!("deployment-identity-uid"))
    );
    assert!(!encoded.contains("token"));
    let binding = wire
        .pointer(&format!("/transitions/{TRANSITION_A}/executorBinding"))
        .and_then(Value::as_object)
        .ok_or("wire executor binding missing")?;
    assert_eq!(
        binding.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "audience",
            "bindingSecretName",
            "bindingSecretUid",
            "domainId",
            "executorId",
            "identityApprovalDigest",
            "identityApprovalResourceVersion",
            "identityApprovalUid",
            "reservationCredentialExpiresAt",
            "schemaVersion",
            "scope",
            "sequence",
            "serviceAccountName",
            "serviceAccountUid",
            "transitionId",
        ])
    );
    Ok(())
}

#[tokio::test]
async fn unsupported_schema_or_missing_executor_binding_is_rejected_without_a_write() -> TestResult
{
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    for unsupported_schema in [true, false] {
        let mut control = registry_object(&registry, REGISTRY_UID, "42")?;
        let encoded = control
            .pointer("/data/state.json")
            .and_then(Value::as_str)
            .ok_or("registry state missing")?;
        let mut state: Value = serde_json::from_str(encoded)?;
        if unsupported_schema {
            state["schemaVersion"] = json!("reakt.dev/namespace-registry/v7");
        } else {
            state["transitions"][TRANSITION_A]
                .as_object_mut()
                .ok_or("stored transition missing")?
                .remove("executorBinding");
        }
        control["data"]["state.json"] = json!(serde_json::to_string(&state)?);
        let (client, calls) = wire(vec![control]);
        assert!(
            platform_client(client, "control", "workload", "cluster-a")?
                .load_registry()
                .await
                .is_err()
        );
        assert!(
            calls
                .lock()
                .map_err(|_| "request lock poisoned")?
                .iter()
                .all(|(method, _, _)| method == "GET")
        );
    }
    Ok(())
}

#[tokio::test]
async fn selection_rejects_binding_or_issued_identity_drift_but_allows_credential_renewal()
-> TestResult {
    let (mut registry, _, _, _, _) = staged_initial_fixture()?;
    registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("stored transition missing")?
        .executor_binding
        .reservation_credential_expires_at = "2020-01-01T00:00:00Z".into();
    let protected = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let (client, calls) = wire(Vec::new());
    let renewed = bound_fixture(
        &registry,
        client,
        "2100-01-01T00:00:00Z",
        fixture_deployment_identity_approval(),
    )?;
    let selected = renewed.selected(&protected, TRANSITION_A)?;
    assert_eq!(
        selected.capacity,
        protected.value.transition(TRANSITION_A)?.capacity
    );
    assert_eq!(
        selected.executor_binding.reservation_credential_expires_at,
        "2020-01-01T00:00:00Z"
    );

    let mut changed = protected.clone();
    changed
        .value
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("stored transition missing")?
        .executor_binding
        .service_account_uid = "replacement-service-account".into();
    assert!(renewed.selected(&changed, TRANSITION_A).is_err());

    for mutation in 0..3 {
        let mut approval = fixture_deployment_identity_approval();
        match mutation {
            0 => approval.uid = "replacement-identity".into(),
            1 => approval.resource_version = "32".into(),
            _ => approval.value.binding_secret_uid = "replacement-secret".into(),
        }
        let (client, _) = wire(Vec::new());
        let changed_identity = bound_fixture(&registry, client, "2100-01-01T00:00:00Z", approval)?;
        assert!(changed_identity.selected(&protected, TRANSITION_A).is_err());
    }
    assert!(
        calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn bare_platform_client_cannot_reserve_or_bind_a_transition() -> TestResult {
    let ReservationFixture {
        registry: _,
        policy: _,
        request,
    } = reservation_fixture()?;
    let inventory = MemoryInventoryStore::default();
    let (reserve_client, reserve_calls) =
        wire_with_identity(Vec::new(), deployment_identity_object());
    let reserve_client = PlatformClient::new(reserve_client, "control", "workload", "cluster-a")?
        .with_claim_store(fixture_claim_store().clone())
        .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()))
        .with_review_store(fixture_review_store().clone());
    assert!(
        Box::pin(reserve_client.reserve_authenticated(&request, "executor-a", &inventory, None))
            .await
            .is_err()
    );
    assert!(
        reserve_calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .is_empty()
    );

    let (bind_client, bind_calls) = wire_with_identity(Vec::new(), deployment_identity_object());
    let bind_client = PlatformClient::new(bind_client, "control", "workload", "cluster-a")?
        .with_claim_store(fixture_claim_store().clone())
        .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()))
        .with_review_store(fixture_review_store().clone());
    assert!(
        bind_client
            .bind_release(
                &request.next.compiled,
                &request.next.artifact_digest,
                &format!("sha256:{}", "a".repeat(64)),
                &inventory,
                TRANSITION_A,
            )
            .await
            .is_err()
    );
    assert!(
        bind_calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn changed_live_identity_is_rejected_before_any_lifecycle_write() -> TestResult {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    let mut current = fixture_deployment_identity();
    current.binding_secret_uid = "replacement-secret".into();
    let identity = object(
        "reakt-deploy-identity-example",
        &current,
        "deployment-identity-uid",
        "31",
    );
    let (client, calls) = wire_with_identity(Vec::new(), identity);
    let bound = bound_fixture(
        &registry,
        client,
        FIXTURE_CREDENTIAL_EXPIRY,
        fixture_deployment_identity_approval(),
    )?;
    let protected = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    assert!(bound.validate_selection(&protected).await.is_err());
    let calls = calls.lock().map_err(|_| "request lock poisoned")?;
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .1
            .ends_with("/configmaps/reakt-deploy-identity-example")
    );
    assert!(calls.iter().all(|(method, _, _)| method == "GET"));
    Ok(())
}

#[tokio::test]
async fn invalid_scope_or_expired_issued_witness_never_creates_authority() -> TestResult {
    let (client, calls) = wire_with_identity(Vec::new(), deployment_identity_object());
    let bare = PlatformClient::new(client, "control", "workload", "cluster-a")?;
    let mut foreign = fixture_deployment_identity_approval();
    foreign.value.cluster_binding = "other-cluster".into();
    assert!(
        bare.clone()
            .with_test_issued_identity(foreign, FIXTURE_CREDENTIAL_EXPIRY)
            .is_err()
    );
    assert!(
        bare.with_test_issued_identity(
            fixture_deployment_identity_approval(),
            "2020-01-01T00:00:00Z",
        )
        .is_err()
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "request lock poisoned")?
            .is_empty()
    );
    Ok(())
}
