use super::*;
use coordination::{ReviewMetadata, ReviewRef, ReviewStore};
use std::sync::atomic::{AtomicUsize, Ordering};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[test]
fn cnpg_review_preserves_the_imported_api_mapping() -> TestResult {
    let mut request = reservation_fixture()?.request;
    let mut ir = serde_json::to_value(&request.next.compiled.ir)?;
    let source = ir["resources"][0]["source"].clone();
    ir["resources"] = json!([{
        "id":"database", "apiVersion":"postgresql.cnpg.io/v1", "kind":"Cluster",
        "apiResource":{"plural":"clusters", "scope":"Namespaced",
            "schemaSha256":format!("sha256:{}", "a".repeat(64))},
        "name":"database", "namespace":"workload",
        "desired":{"apiVersion":"postgresql.cnpg.io/v1", "kind":"Cluster",
            "metadata":{"name":"database", "namespace":"workload"},
            "spec":{"instances":1, "storage":{"size":"1Gi"}}},
        "lifecycle":{"deletionPolicy":"orphan"}, "source":source
    }]);
    ir["reactions"] = json!([]);
    request.next.compiled = parse_and_validate(&ir.to_string())?;
    let accepted = policy(&request.next.compiled);
    request.runtime_ceiling = accepted.runtime_ceiling;
    request.cleanup_ceiling = accepted.cleanup_ceiling;
    let review = crate::project_review_metadata(&deploy::plan_transition(&request)?)?;
    let resource = review.resources.first().ok_or("missing CNPG review")?;
    let mapping = resource
        .api_resource
        .as_ref()
        .ok_or("missing CRD mapping")?;
    assert_eq!(resource.identity.api_group, "postgresql.cnpg.io");
    assert_eq!(resource.identity.resource, "clusters");
    assert_eq!(mapping.plural, "clusters");
    assert_eq!(mapping.schema_sha256, format!("sha256:{}", "a".repeat(64)));
    assert_eq!(
        mapping.scope,
        coordination::ReviewApiResourceScope::Namespaced
    );
    review.validate()?;
    ir["resources"][0]["force"] = json!(true);
    request.next.compiled = parse_and_validate(&ir.to_string())?;
    assert!(deploy::plan_transition(&request).is_err());
    let accepted = policy(&request.next.compiled);
    request.runtime_ceiling = accepted.runtime_ceiling;
    request.cleanup_ceiling = accepted.cleanup_ceiling;
    let forced = crate::project_review_metadata(&deploy::plan_transition(&request)?)?;
    assert!(
        forced
            .resources
            .first()
            .is_some_and(|resource| resource.force)
    );
    assert!(
        forced
            .enforcement
            .indirect_risks
            .contains(&"force-apply:Cluster/workload/database".into())
    );
    forced.validate()?;
    Ok(())
}

#[derive(Default)]
struct CountingClaims {
    puts: AtomicUsize,
}
#[async_trait::async_trait]
impl ClaimStore for CountingClaims {
    async fn put_claims(&self, snapshot: &ClaimSnapshotV1) -> coordination::Result<ClaimRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        fixture_claim_store().put_claims(snapshot).await
    }
    async fn read_claims(
        &self,
        reference: &ClaimRef,
    ) -> coordination::Result<Option<ClaimSnapshotV1>> {
        fixture_claim_store().read_claims(reference).await
    }
}

struct FaultyReview {
    mode: u8,
    expected: ReviewMetadata,
}
#[async_trait::async_trait]
impl ReviewStore for FaultyReview {
    async fn put_review(&self, metadata: &ReviewMetadata) -> coordination::Result<ReviewRef> {
        if self.mode == 0 {
            return Err(coordination::Error::Backend("injected publication failure"));
        }
        let mut reference = metadata.reference()?;
        if self.mode == 1 {
            reference.digest = format!("sha256:{}", "f".repeat(64));
        }
        Ok(reference)
    }
    async fn read_review(&self, _: &ReviewRef) -> coordination::Result<Option<ReviewMetadata>> {
        if self.mode == 2 {
            return Ok(None);
        }
        let mut metadata = self.expected.clone();
        metadata.enforcement.runtime_ceiling.policy_ref = "other-policy".into();
        Ok(Some(metadata))
    }
}

#[tokio::test]
async fn review_publication_failures_precede_claim_and_registry_writes() -> TestResult {
    for mode in 0..4 {
        let ReservationFixture {
            registry,
            policy,
            request,
        } = reservation_fixture()?;
        let plan = deploy::plan_transition(&request)?;
        let claims = Arc::new(CountingClaims::default());
        let (client, calls) = wire(vec![
            registry_object(&registry, REGISTRY_UID, "42")?,
            object("policy", &policy, "policy-uid", "40")?,
        ])?;
        let client = platform_client(client, "control", "workload", "cluster-a")?
            .with_claim_store(claims.clone())
            .with_review_store(Arc::new(FaultyReview {
                mode,
                expected: crate::project_review_metadata(&plan)?,
            }));
        assert!(
            reserve_test_transition(&client, &request, "ci-job")
                .await
                .is_err()
        );
        assert_eq!(claims.puts.load(Ordering::SeqCst), 0, "fault mode {mode}");
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

#[tokio::test]
async fn lost_reservation_cas_does_not_poison_the_unconsumed_review_scope() -> TestResult {
    let ReservationFixture {
        registry,
        policy,
        request,
    } = reservation_fixture()?;
    let store = Arc::new(review_support::MemoryReviewStore::default());
    let before = registry_object(&registry, REGISTRY_UID, "42")?;
    let (client, _) = wire(vec![
        before.clone(),
        object("policy", &policy, "policy-uid", "40")?,
        before.clone(),
        json!({"wireHttpStatus":409,"body":{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Conflict","message":"injected CAS loss","code":409}}),
    ])?;
    let client = platform_client(client, "control", "workload", "cluster-a")?
        .with_review_store(store.clone());
    assert!(
        reserve_test_transition(&client, &request, "ci-a")
            .await
            .is_err()
    );
    let first = crate::project_review_metadata(&deploy::plan_transition(&request)?)?;
    assert_eq!(
        store.read_review(&first.reference()?).await?,
        Some(first.clone())
    );

    let mut second_request = request;
    let mut ir = serde_json::to_value(&second_request.next.compiled.ir)?;
    ir["resources"][0]["source"]["line"] = json!(73);
    second_request.next.compiled = parse_and_validate(&serde_json::to_string(&ir)?)?;
    let second = crate::project_review_metadata(&deploy::plan_transition(&second_request)?)?;
    assert_eq!(first.scope, second.scope);
    assert_ne!(first.reference()?, second.reference()?);
    let (client, _) = wire(vec![
        before.clone(),
        object("policy", &policy, "policy-uid", "40")?,
        before,
        json!("echo-put"),
    ])?;
    let client = platform_client(client, "control", "workload", "cluster-a")?
        .with_review_store(store.clone());
    let committed_reservation = reserve_test_transition(&client, &second_request, "ci-b").await?;
    assert_eq!(
        committed_reservation
            .value
            .transition(TRANSITION_A)?
            .next_review_metadata,
        second.reference()?
    );
    assert_eq!(store.read_review(&first.reference()?).await?, Some(first));
    assert_eq!(store.read_review(&second.reference()?).await?, Some(second));
    Ok(())
}

#[tokio::test]
async fn unsupported_schema_and_missing_review_bindings_are_rejected() -> TestResult {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    for unsupported_schema in [true, false] {
        let mut object = registry_object(&registry, REGISTRY_UID, "42")?;
        let mut state: Value = serde_json::from_str(
            object["data"]["state.json"]
                .as_str()
                .ok_or("state missing")?,
        )?;
        if unsupported_schema {
            state["schemaVersion"] = json!("apsides.dev/namespace-registry/v6");
        } else {
            state["transitions"][TRANSITION_A]
                .as_object_mut()
                .ok_or("transition missing")?
                .remove("nextReviewMetadata");
        }
        object["data"]["state.json"] = json!(serde_json::to_string(&state)?);
        let (client, calls) = wire(vec![object])?;
        assert!(
            platform_client(client, "control", "workload", "cluster-a")?
                .load_registry()
                .await
                .is_err()
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

#[tokio::test]
async fn selected_sessions_reject_changed_references_and_backend_substitution() -> TestResult {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    let (client, calls) = wire(vec![])?;
    let selected = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let protected = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let mut changed = protected.clone();
    changed
        .value
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("transition missing")?
        .next_review_metadata
        .digest = format!("sha256:{}", "f".repeat(64));
    assert!(selected.selected(&changed, TRANSITION_A).is_err());
    let expected = crate::project_review_metadata(registry.plan(TRANSITION_A)?)?;
    let selected = selected.with_review_store(Arc::new(FaultyReview { mode: 3, expected }));
    assert!(selected.validate_selection(&protected).await.is_err());
    assert!(calls.lock().map_err(|_| "calls lock")?.is_empty());
    Ok(())
}

/// A review store that serves the exact projected metadata for the selected
/// plan, optionally with only its `surfaceDigest` changed (simulating a
/// release whose surface changed after the transition was reviewed).
/// Resume revalidates every selected session against a fresh projection of
/// the exact same plan, so this drift must be caught exactly like any other
/// authenticated metadata mismatch, with no dedicated surface-comparison
/// code needed.
struct SurfaceDriftReview {
    expected: ReviewMetadata,
    tamper: bool,
}
#[async_trait::async_trait]
impl ReviewStore for SurfaceDriftReview {
    async fn put_review(&self, metadata: &ReviewMetadata) -> coordination::Result<ReviewRef> {
        metadata.reference()
    }
    async fn read_review(&self, _: &ReviewRef) -> coordination::Result<Option<ReviewMetadata>> {
        let mut metadata = self.expected.clone();
        if self.tamper {
            metadata.surface_digest = format!("sha256:{}", "d".repeat(64));
        }
        Ok(Some(metadata))
    }
}

/// `validate_selection`'s rejection on a metadata mismatch
/// (`"selected transition review metadata changed"`) is a generic
/// whole-body comparison and does not name which field differed, so this
/// cannot assert the error text mentions `surfaceDigest` specifically.
/// Instead it isolates the field by construction and contrast: the control
/// case serves the exact projected metadata back unmodified and the
/// resumed session is accepted; changing only `surfaceDigest` (every other
/// field, including `scope` and every resource/input/handler/egress/output
/// row, stays identical) and nothing else about the fixture is what turns
/// that same session into a rejection.
#[tokio::test]
async fn resumed_session_accepts_an_unchanged_review_and_rejects_only_a_drifted_surface_digest()
-> TestResult {
    let (registry, _, policy, _, _) = staged_initial_fixture()?;
    let protected = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.into(),
        resource_version: "42".into(),
    };
    let expected = crate::project_review_metadata(registry.plan(TRANSITION_A)?)?;

    let (client, calls) = wire(vec![object("policy", &policy, "policy-uid", "40")?])?;
    let selected = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let selected = selected.with_review_store(Arc::new(SurfaceDriftReview {
        expected: expected.clone(),
        tamper: false,
    }));
    selected.validate_selection(&protected).await?;
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock")?
            .iter()
            .all(|(method, _, _)| method == "GET")
    );

    let (client, calls) = wire(vec![])?;
    let selected = registry.bind(client, "control", "workload", "cluster-a", TRANSITION_A)?;
    let selected = selected.with_review_store(Arc::new(SurfaceDriftReview {
        expected,
        tamper: true,
    }));
    let error = selected
        .validate_selection(&protected)
        .await
        .err()
        .ok_or("expected a resumed session with a drifted surface digest to be rejected")?;
    assert!(error.to_string().contains("review metadata"));
    assert!(calls.lock().map_err(|_| "calls lock")?.is_empty());
    Ok(())
}

#[tokio::test]
async fn protected_review_references_reject_foreign_scope_and_allocation() -> TestResult {
    let (registry, _, _, _, _) = staged_initial_fixture()?;
    for mutation in 0..3 {
        let mut object = registry_object(&registry, REGISTRY_UID, "42")?;
        let mut state: Value = serde_json::from_str(
            object["data"]["state.json"]
                .as_str()
                .ok_or("state missing")?,
        )?;
        let reference = &mut state["transitions"][TRANSITION_A]["nextReviewMetadata"];
        match mutation {
            0 => reference["scope"]["domainId"] = json!("foreign"),
            1 => reference["scope"]["sequence"] = json!("2"),
            _ => reference["scope"]["claimScope"]["workloadNamespace"] = json!("foreign"),
        }
        object["data"]["state.json"] = json!(serde_json::to_string(&state)?);
        let (client, calls) = wire(vec![object])?;
        assert!(
            platform_client(client, "control", "workload", "cluster-a")?
                .load_registry()
                .await
                .is_err()
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

#[tokio::test]
async fn preflight_baseline_requires_the_exact_planning_registry_revision() -> TestResult {
    let registry = qualified_upgrade_fixture()?;
    let expected =
        fixture_review_store().read(&registry.transition(TRANSITION_A)?.next_review_metadata)?;
    for (uid, revision, accepted) in [
        (REGISTRY_UID, "42", true),
        (REGISTRY_UID, "43", false),
        ("foreign-uid", "42", false),
    ] {
        let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?])?;
        let result = platform_client(client, "control", "workload", "cluster-a")?
            .load_active_review_metadata("example", uid, revision)
            .await;
        if accepted {
            assert_eq!(result?, expected);
        } else {
            let Err(error) = result else {
                return Err("changed planning revision accepted".into());
            };
            assert!(
                error
                    .to_string()
                    .contains("review baseline registry revision changed")
            );
        }
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
