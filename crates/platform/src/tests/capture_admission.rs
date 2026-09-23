use super::*;
use coordination::{AuditRef, AuditStore, ClaimStore, TransitionAuditV1};
use inventory::{InventoryStore, SqliteInventory};
use std::sync::atomic::{AtomicUsize, Ordering};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

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

#[derive(Default)]
struct CountingAudit {
    puts: AtomicUsize,
    store: audit_support::MemoryAuditStore,
}

#[async_trait::async_trait]
impl AuditStore for CountingAudit {
    async fn put_audit(&self, audit: &TransitionAuditV1) -> coordination::Result<AuditRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.store.put_audit(audit).await
    }

    async fn read_audit(
        &self,
        reference: &AuditRef,
    ) -> coordination::Result<Option<TransitionAuditV1>> {
        self.store.read_audit(reference).await
    }
}

/// A plan refused by policy, here the runtime ceiling, must be refused on the
/// captured UIDs before any of them is written: the registry keeps its state
/// and resourceVersion, and the claim, audit and inventory stores are untouched.
#[tokio::test]
async fn ceiling_refusal_during_uid_capture_writes_no_protected_state() -> TestResult {
    let mut registry = qualified_upgrade_fixture()?;
    registry.transitions.remove(TRANSITION_B);
    let previous = registry.plan(TRANSITION_A)?.request.clone();
    let mut ceiling = previous.runtime_ceiling.clone();
    ceiling.capabilities.clear();
    let (scope, snapshot) = deploy::generation_inventory(&previous.next, &registry.claims)?;
    ensure_unknown_uids(&snapshot)?;
    let directory = tempfile::tempdir()?;
    let store = SqliteInventory::open_local(&directory.path().join("inventory.db")).await?;
    let head = store.publish(&scope, None, &snapshot).await?;

    let control = registry_object(&registry, REGISTRY_UID, "42")?;
    let resource = snapshot.resources.first().ok_or("resource missing")?;
    let live = json!({"apiVersion":resource.api_version,"kind":resource.kind,"metadata":{
        "name":resource.identity.name,"namespace":resource.identity.namespace,
        "uid":"captured-uid","resourceVersion":"9",
        "managedFields":[{"manager":"apsides-example","operation":"Apply","apiVersion":resource.api_version,"fieldsType":"FieldsV1","fieldsV1":{}}]
    }});
    let (client, calls) = wire(vec![control.clone(), live, control, json!("echo-put")])?;
    let claims = Arc::new(CountingClaims::default());
    let audit = Arc::new(CountingAudit::default());
    let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
        .with_claim_store(claims.clone())
        .with_audit_store(audit.clone())
        .with_review_store(fixture_review_store().clone())
        .with_test_issued_identity(
            fixture_deployment_identity_approval(),
            FIXTURE_CREDENTIAL_EXPIRY,
        )?;

    let refusal = platform
        .capture_active_claim_uids(
            "example",
            &head.snapshot,
            |planned, planned_inventory| async move {
                // Admission sees the capture as it would be persisted, at the
                // observed revision, with the inventory completed to match.
                let claim = planned.value.claims.first().ok_or_else(|| {
                    crate::Error::Invalid("planned registry lost its claim".into())
                })?;
                let tracked = planned_inventory.resources.first().ok_or_else(|| {
                    crate::Error::Invalid("planned inventory lost its resource".into())
                })?;
                if planned.resource_version != "42"
                    || claim.uid.as_deref() != Some("captured-uid")
                    || tracked.uid.as_deref() != Some("captured-uid")
                {
                    return Err(crate::Error::Invalid(
                        "admission did not see the captured UIDs".into(),
                    ));
                }
                compiler_core::validate_ceiling(&previous.next.compiled, &ceiling)?;
                Ok(())
            },
        )
        .await;

    let Err(error) = refusal else {
        return Err("a plan outside the ceiling was admitted".into());
    };
    assert!(
        error.to_string().contains("is not explicitly allowed"),
        "{error}"
    );
    let requests: Vec<_> = calls
        .lock()
        .map_err(|_| "request lock poisoned")?
        .iter()
        .map(|call| (call.0.clone(), call.1.clone()))
        .collect();
    assert!(
        requests.iter().all(|(method, _)| method == "GET"),
        "a refused capture wrote Kubernetes state: {requests:?}"
    );
    assert_eq!(claims.puts.load(Ordering::SeqCst), 0);
    assert_eq!(audit.puts.load(Ordering::SeqCst), 0);
    assert_eq!(store.load(&scope).await?, Some(head));
    Ok(())
}

fn ensure_unknown_uids(snapshot: &inventory::InventorySnapshot) -> TestResult {
    if snapshot.resources.is_empty() || snapshot.resources.iter().any(|r| r.uid.is_some()) {
        return Err("fixture must start with uncaptured active UIDs".into());
    }
    Ok(())
}
