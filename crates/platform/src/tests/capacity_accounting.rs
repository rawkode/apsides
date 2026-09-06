//! Byte accounting fixtures do not establish lifecycle authority for cloned domains.
use super::*;
use crate::state_capacity::{measure, validate_domain_candidate, validate_reserved_capacity};
use compiler_core::canonical_bytes;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn complete_maps() -> TestResult<NamespaceRegistryState> {
    let registry = qualified_upgrade_fixture()?;
    let reference = registry.claim_snapshot()?.reference()?;
    let mut state = registry.state(&reference)?;
    let previous = state
        .transitions
        .get(TRANSITION_A)
        .ok_or("predecessor missing")?;
    state.audit_heads.insert(
        "example".into(),
        coordination::AuditRef {
            scope: coordination::AuditScope {
                claim_scope: reference.scope,
                domain_id: "example".into(),
                transition_id: TRANSITION_A.into(),
                sequence: previous.allocation().sequence,
            },
            digest: digest(&"archived metadata")?,
        },
    );
    Ok(state)
}

#[tokio::test]
async fn capacity_partition_covers_all_four_maps_and_escaped_keys() -> TestResult {
    let mut state = complete_maps()?;
    for sequence in [1, 999, u64::MAX] {
        state.next_sequence = sequence;
        let footprint = measure(&state)?;
        let accounted = footprint.domain_bytes.values().sum::<usize>() + footprint.fixed_bytes;
        assert_eq!(
            accounted,
            canonical_bytes(&state)?.len() + 4 + u64::MAX.to_string().len()
                - sequence.to_string().len()
        );
    }
    let stored = state
        .transitions
        .remove(TRANSITION_B)
        .ok_or("pending missing")?;
    state.transitions.insert("escaped-\"-\\".into(), stored);
    let footprint = measure(&state)?;
    assert_eq!(
        footprint.domain_bytes.values().sum::<usize>() + footprint.fixed_bytes,
        canonical_bytes(&state)?.len() + 4
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_pending_domains_cannot_spend_each_others_reserved_space() -> TestResult {
    let mut state = complete_maps()?;
    let mut other = state
        .transitions
        .get(TRANSITION_B)
        .ok_or("pending missing")?
        .clone();
    other.inputs.registry.generation_history.domain_id = "other".into();
    state.histories.insert(
        "other".into(),
        other.inputs.registry.generation_history.clone(),
    );
    state.transitions.insert("other-pending".into(), other);
    // Hold every stored ceiling at six digits so choosing the exact boundary
    // does not change the actual DTO sizes during this arithmetic test.
    for stored in state.transitions.values_mut() {
        stored.capacity.domain_bytes = 200_000;
    }
    let footprint = measure(&state)?;
    let available = MAX_STATE_BYTES
        .checked_sub(footprint.fixed_bytes + 200_000)
        .ok_or("fixture too large")?;
    state
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("pending missing")?
        .capacity
        .domain_bytes = available as u64;
    validate_reserved_capacity(&state)?;
    validate_domain_candidate(&state, "other", 200_000)?;
    assert!(validate_domain_candidate(&state, "other", 200_001).is_err());
    state
        .transitions
        .get_mut("other-pending")
        .ok_or("other missing")?
        .capacity
        .domain_bytes += 1;
    assert!(validate_reserved_capacity(&state).is_err());
    Ok(())
}

#[tokio::test]
async fn forged_pending_ceiling_cannot_hide_actual_domain_bytes() -> TestResult {
    let mut state = complete_maps()?;
    state
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("pending missing")?
        .capacity
        .domain_bytes = 1;
    assert!(validate_reserved_capacity(&state).is_err());
    Ok(())
}

#[test]
fn capacity_template_covers_every_counter_and_ceiling_width() -> TestResult {
    let template =
        CapacityAdmission::projection_placeholder(crate::completion_envelope::initial_profile());
    let maximum = canonical_bytes(&template)?.len();
    let mut value = template.clone();
    let mut revisions = vec![0, u64::MAX];
    for exponent in 0..20 {
        if let Some(power) = 10u64.checked_pow(exponent) {
            revisions.extend([power - 1, power]);
        }
    }
    for revision in revisions {
        value.revision = revision;
        for bytes in [
            1,
            9,
            10,
            99,
            100,
            999,
            1000,
            9999,
            10_000,
            99_999,
            100_000,
            MAX_STATE_BYTES as u64,
        ] {
            value.domain_bytes = bytes;
            value.activation_bytes = bytes;
            value.inventory_bytes = bytes;
            value.validate()?;
            assert!(canonical_bytes(&value)?.len() <= maximum);
        }
    }
    Ok(())
}

fn next_initial_event(record: &deploy::TransitionRecord) -> TestResult<deploy::Event> {
    use deploy::{Event, Phase, Quiescence};
    let event_id = format!("event-{}", record.revision);
    Ok(match record.phase {
        Phase::Planned => Event::Staged {
            event_id,
            generation_id: GENERATION_A.into(),
            service_account_name: deploy::allocation_names(REGISTRY_UID, 1)?.service_account_name,
            registry_cas_receipt_digest: digest(&record.revision)?,
            verification_epoch: record.verification_epoch,
        },
        Phase::Staged => Event::Quiesced {
            event_id,
            evidence: Quiescence::InitialInstallation {
                verification_epoch: record.verification_epoch,
            },
        },
        Phase::Quiesced => Event::Activated {
            event_id,
            generation_id: GENERATION_A.into(),
            activation_receipt_digest: digest(&record.revision)?,
            verification_epoch: record.verification_epoch,
        },
        Phase::Activated => Event::Cleaning { event_id },
        Phase::Cleaning => Event::Committed {
            event_id,
            cleanup_receipts: vec![],
            obsolete_identity_retirement_digest: digest(&record.revision)?,
        },
        _ => return Err("terminal fixture has no next success event".into()),
    })
}

#[tokio::test]
async fn completion_record_envelope_covers_replayed_recovery_and_failure_paths() -> TestResult {
    use deploy::{Event, Phase};
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    let plan = registry.plan(TRANSITION_A)?;
    let maximum = crate::envelope_record::maximum(registry.transition(TRANSITION_A)?, plan)?;
    let maximum_bytes = canonical_bytes(&maximum)?.len();
    for block_at in [Phase::Planned, Phase::Staged, Phase::Quiesced] {
        let mut record = deploy::begin(plan, "executor-a")?;
        for recovery in 0..=crate::envelope_record::MAX_RESUMES {
            // Every prefix can either complete or fail. Replay each alternate
            // suffix against the real reducer before comparing serialized size.
            let mut completion = record.clone();
            while completion.phase != Phase::Committed {
                let failed = deploy::advance(
                    plan,
                    &completion,
                    completion.revision,
                    "executor-a",
                    Event::Blocked {
                        event_id: format!("failure-{}", completion.revision),
                        reason: "\0".repeat(1024),
                    },
                )?;
                deploy::verify_record(plan, &failed)?;
                assert!(canonical_bytes(&failed)?.len() <= maximum_bytes);
                let event = next_initial_event(&completion)?;
                completion =
                    deploy::advance(plan, &completion, completion.revision, "executor-a", event)?;
            }
            deploy::verify_record(plan, &completion)?;
            assert!(canonical_bytes(&completion)?.len() <= maximum_bytes);
            if recovery == crate::envelope_record::MAX_RESUMES {
                break;
            }
            while record.phase != block_at {
                let event = next_initial_event(&record)?;
                record = deploy::advance(plan, &record, record.revision, "executor-a", event)?;
            }
            record = deploy::advance(
                plan,
                &record,
                record.revision,
                "executor-a",
                Event::Blocked {
                    event_id: format!("blocked-{recovery}"),
                    reason: "\0".repeat(1024),
                },
            )?;
            record = deploy::advance(
                plan,
                &record,
                record.revision,
                "executor-a",
                Event::Resume {
                    event_id: format!("resume-{recovery}"),
                    recovery_receipt_digest: digest(&recovery)?,
                },
            )?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn registry_rejects_secret_commitments_and_unbounded_stage_keys_without_writes() -> TestResult
{
    for secret in [true, false] {
        let (mut registry, _, _, _, _) = staged_initial_fixture()?;
        let stored = registry
            .transitions
            .get_mut(TRANSITION_A)
            .ok_or("transition missing")?;
        let key = if secret {
            format!(
                "v1/Secret/control/{}-tls",
                stored.allocation().service_account_name
            )
        } else {
            "v1/ConfigMap/control/unbounded-extra".into()
        };
        stored
            .staged_content_digests
            .insert(key, digest(&"private-hash-canary")?);
        let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
        let platform = platform_client(client, "control", "workload", "cluster-a")?;
        let error = platform
            .load_registry()
            .await
            .err()
            .ok_or("invalid stage commitment accepted")?;
        assert!(error.to_string().contains("bounded support set"), "{error}");
        assert!(!error.to_string().contains("private-hash-canary"));
        assert!(
            calls
                .lock()
                .map_err(|_| "calls lock poisoned")?
                .iter()
                .all(|call| call.0 == "GET")
        );
    }
    Ok(())
}

struct CountingClaims {
    puts: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ClaimStore for CountingClaims {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> coordination::Result<ClaimRef> {
        self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        fixture_claim_store().put_claims(snapshot).await
    }
    async fn read_claims(
        &self,
        reference: &ClaimRef,
    ) -> coordination::Result<Option<ClaimSnapshotV1>> {
        fixture_claim_store().read_claims(reference).await
    }
}

#[tokio::test]
async fn reservation_refuses_aggregate_completion_overflow_before_claim_publication() -> TestResult
{
    let ReservationFixture {
        mut registry,
        policy,
        request,
    } = reservation_fixture()?;
    for index in 0..850 {
        let domain = format!("domain-{index:04}-{}", "x".repeat(240));
        registry.histories.insert(
            domain.clone(),
            deploy::GenerationHistory {
                domain_id: domain,
                cluster_binding: "cluster-a".into(),
                revision: "42".into(),
                active_allocation: None,
            },
        );
    }
    let state = registry.state(&registry.claim_snapshot()?.reference()?)?;
    let actual = canonical_bytes(&state)?.len();
    assert!(
        actual < MAX_STATE_BYTES && actual > 450_000,
        "fixture bytes: {actual}"
    );
    validate_reserved_capacity(&state)?;
    let (client, calls) = wire(vec![
        registry_object(&registry, REGISTRY_UID, "42")?,
        object("reakt-policy-example", &policy, "policy-uid", "44"),
    ]);
    let claims = Arc::new(CountingClaims {
        puts: std::sync::atomic::AtomicUsize::new(0),
    });
    let platform = platform_client(client, "control", "workload", "cluster-a")?
        .with_claim_store(claims.clone());
    let error = Box::pin(reserve_test_transition(&platform, &request, "executor-a"))
        .await
        .err()
        .ok_or("overcommitted reservation accepted")?;
    assert!(
        error.to_string().contains("completion capacity exceeds"),
        "{error}"
    );
    assert_eq!(claims.puts.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|call| call.0 == "GET")
    );
    Ok(())
}

#[tokio::test]
async fn oversized_activation_create_version_stops_before_inventory_creation() -> TestResult {
    let (mut registry, _, policy, _, _) = staged_initial_fixture()?;
    let transition = registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("transition missing")?;
    let activation_key = "v1/ConfigMap/control/reakt-activation-example";
    transition.staged_identity_uids.remove(activation_key);
    transition.staged_content_digests.remove(activation_key);
    registry.admit(TRANSITION_A)?;
    let protected = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, calls) = wire(vec![
        json!({"wireHttpStatus":404,"body":{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"fixture activation absent","code":404}}),
        json!("echo-activation-create-large-rv"),
        object("reakt-policy-example", &policy, "policy-uid", "44"),
        protected,
        json!("echo-put"),
    ]);
    let platform = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let mut protected = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let stored = platform.selected(&protected, TRANSITION_A)?;
    let binding = stored.runtime_binding.as_ref().ok_or("binding missing")?;
    let error = Box::pin(platform.stage_initial_controller_resources(
        &mut protected,
        &stored,
        binding,
        None,
    ))
    .await
    .err()
    .ok_or("oversized activation version accepted")?;
    assert!(
        error
            .to_string()
            .contains("control object UID or resourceVersion exceeds"),
        "{error}"
    );
    let calls = calls.lock().map_err(|_| "calls lock poisoned")?;
    let creates = calls
        .iter()
        .filter(|call| call.0 == "POST")
        .collect::<Vec<_>>();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].2["metadata"]["name"], "reakt-activation-example");
    assert!(
        !calls
            .iter()
            .any(|call| call.1.contains("reakt-inventory-example")
                || call.2["metadata"]["name"] == "reakt-inventory-example")
    );
    Ok(())
}
