use super::*;
use coordination::ReviewMetadataV1;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Only the successor release and immutable predecessor metadata survive setup.
struct PrunedUpgrade {
    committed: Versioned<NamespaceRegistry>,
    next: deploy::GenerationPlan,
    inventory: MemoryInventoryStore,
    previous_review: ReviewMetadataV1,
    next_review: ReviewMetadataV1,
    policy: PolicyState,
}

async fn commit_and_discard_predecessor() -> TestResult<PrunedUpgrade> {
    let registry = audit_retention::prepare_cleaning_upgrade()?;
    let plan = registry.plan(TRANSITION_B)?;
    let next = plan.request.next.clone();
    let policy = policy(&next.compiled);
    let predecessor = plan.request.predecessor().ok_or("predecessor missing")?;
    let inventory = MemoryInventoryStore::default();
    inventory.seed(&predecessor.scope, &predecessor.inventory)?;
    let stored = registry.transition(TRANSITION_B)?;
    let planning_claims = ClaimSnapshotV1 {
        schema_version: coordination::CLAIM_SNAPSHOT_SCHEMA.into(),
        scope: stored.inputs.registry.claim_snapshot.scope.clone(),
        claims: plan.request.registry.claims.clone(),
    };
    assert_eq!(
        fixture_claim_store().seed(planning_claims)?,
        stored.inputs.registry.claim_snapshot
    );
    let previous_review = fixture_review_store()
        .read(
            stored
                .previous_review_metadata
                .as_ref()
                .ok_or("previous review missing")?,
        )?
        .ok_or("previous review body missing")?;
    let next_review = fixture_review_store()
        .read(&stored.next_review_metadata)?
        .ok_or("next review body missing")?;
    let before = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let (wire_client, calls) = wire(vec![
        object("apsides-policy-example", &policy, "policy-uid", "40")?,
        registry_object(&registry, REGISTRY_UID, "42")?,
        json!("echo-put"),
    ])?;
    let client = registry.bind(
        wire_client,
        "control",
        "workload",
        "cluster-a",
        TRANSITION_B,
    )?;
    let committed = Box::pin(client.transition_event(
        &before,
        TRANSITION_B,
        "executor-b",
        audit_retention::committed_event("committed-review-b")?,
    ))
    .await?;
    assert!(!committed.value.transitions.contains_key(TRANSITION_A));
    assert_eq!(
        committed.value.transition(TRANSITION_B)?.record.phase,
        deploy::Phase::Committed
    );
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "calls lock")?
            .iter()
            .filter(|(method, _, _)| method == "PUT")
            .count(),
        1
    );
    // Neither the fixture plan map nor the pre-commit registry/selection escapes.
    drop(client);
    drop(before);
    drop(registry);
    Ok(PrunedUpgrade {
        committed,
        next,
        inventory,
        previous_review,
        next_review,
        policy,
    })
}

fn reload_client(fixture: &PrunedUpgrade) -> TestResult<(PlatformClient, Calls)> {
    let registry = registry_object(
        &fixture.committed.value,
        &fixture.committed.uid,
        &fixture.committed.resource_version,
    )?;
    let trust = object(
        "apsides-trust-example",
        &json!({"platform":"approved"}),
        "trust-uid",
        "43",
    )?;
    let policy = object(
        "apsides-policy-example",
        &fixture.policy,
        "policy-uid",
        "40",
    )?;
    let (client, calls) = wire(vec![
        registry.clone(),
        trust.clone(),
        policy.clone(),
        registry,
        trust,
        policy,
    ])?;
    Ok((
        platform_client(client, "control", "workload", "cluster-a")?,
        calls,
    ))
}

#[tokio::test]
async fn committed_successor_resolves_previous_review_without_predecessor_plan() -> TestResult {
    let fixture = Box::pin(commit_and_discard_predecessor()).await?;
    let (client, calls) = reload_client(&fixture)?;
    let session = Box::pin(client.bind_release(
        &fixture.next.compiled,
        &fixture.next.artifact_digest,
        &digest(&json!({"platform":"approved"}))?,
        &fixture.inventory,
        TRANSITION_B,
    ))
    .await?;
    let resolved = session.resolve().await?;
    assert!(
        !resolved
            .registry()
            .value
            .transitions
            .contains_key(TRANSITION_A)
    );
    assert_eq!(
        resolved.previous_review_metadata(),
        Some(&fixture.previous_review)
    );
    assert_eq!(resolved.next_review_metadata(), &fixture.next_review);
    assert_eq!(resolved.plan().request.next.compiled, fixture.next.compiled);
    assert_eq!(fixture.previous_review.scope.transition_id, TRANSITION_A);
    assert_eq!(fixture.next_review.scope.transition_id, TRANSITION_B);
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock")?
            .iter()
            .all(|(method, _, _)| method == "GET")
    );
    Ok(())
}

#[tokio::test]
async fn committed_successor_requires_immutable_predecessor_snapshots_without_writes() -> TestResult
{
    let fixture = Box::pin(commit_and_discard_predecessor()).await?;
    for missing_review in [false, true] {
        let (mut client, calls) = reload_client(&fixture)?;
        let empty_inventory = MemoryInventoryStore::default();
        let inventory = if missing_review {
            let store = Arc::new(review_support::MemoryReviewStore::default());
            store.seed(fixture.next_review.clone())?;
            client = client.with_review_store(store);
            &fixture.inventory
        } else {
            &empty_inventory
        };
        let result = Box::pin(client.bind_release(
            &fixture.next.compiled,
            &fixture.next.artifact_digest,
            &digest(&json!({"platform":"approved"}))?,
            inventory,
            TRANSITION_B,
        ))
        .await;
        let Err(error) = result else {
            return Err("missing predecessor snapshot unexpectedly resolved".into());
        };
        let expected = if missing_review {
            "protected review metadata missing"
        } else {
            "frozen predecessor inventory missing"
        };
        assert!(
            error.to_string().contains(expected),
            "unexpected failure: {error}"
        );
        assert!(
            calls
                .lock()
                .map_err(|_| "calls lock")?
                .iter()
                .all(|(method, _, _)| method == "GET")
        );
    }
    Ok(())
}
