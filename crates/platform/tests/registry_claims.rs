//! Registry authority must not depend on a `ClaimStore` implementation being honest.
use coordination::{CLAIM_SNAPSHOT_SCHEMA, ClaimRef, ClaimScope, ClaimSnapshotV1, ClaimStore};
use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use platform::{NamespaceRegistryState, PlatformClient};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tower::service_fn;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
const UID: &str = "11111111-1111-4111-8111-111111111111";

fn snapshot() -> ClaimSnapshotV1 {
    ClaimSnapshotV1 {
        schema_version: CLAIM_SNAPSHOT_SCHEMA.into(),
        scope: ClaimScope {
            registry_uid: UID.into(),
            cluster_binding: "cluster-a".into(),
            control_namespace: "control".into(),
            workload_namespace: "workload".into(),
        },
        claims: vec![],
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Missing,
    WrongSnapshot,
    WrongReference,
    Backend,
}
struct Store(Fault);
#[async_trait::async_trait]
impl ClaimStore for Store {
    async fn put_claims(&self, value: &ClaimSnapshotV1) -> coordination::Result<ClaimRef> {
        if matches!(self.0, Fault::Backend) {
            return Err(coordination::Error::Backend("fixture failure"));
        }
        let mut reference = value.reference()?;
        if matches!(self.0, Fault::WrongReference) {
            reference.digest = format!("sha256:{}", "0".repeat(64));
        }
        Ok(reference)
    }
    async fn read_claims(&self, _: &ClaimRef) -> coordination::Result<Option<ClaimSnapshotV1>> {
        if matches!(self.0, Fault::Backend) {
            return Err(coordination::Error::Backend("fixture failure"));
        }
        if matches!(self.0, Fault::Missing) {
            return Ok(None);
        }
        let mut value = snapshot();
        if matches!(self.0, Fault::WrongSnapshot) {
            value.scope.cluster_binding = "foreign".into();
        }
        Ok(Some(value))
    }
}

type Calls = Arc<Mutex<Vec<String>>>;
fn client(initial: Option<Value>, mutate_response: bool) -> (Client, Calls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let service = service_fn(move |request: Request<Body>| {
        let captured = captured.clone();
        let initial = initial.clone();
        async move {
            let (parts, body) = request.into_parts();
            captured
                .lock()
                .map_err(|_| std::io::Error::other("fixture lock"))?
                .push(parts.method.to_string());
            let mut response = if parts.method == http::Method::PUT {
                let bytes = body
                    .collect()
                    .await
                    .map_err(std::io::Error::other)?
                    .to_bytes();
                let mut value: Value = serde_json::from_slice(&bytes)?;
                value["metadata"]["resourceVersion"] = json!("2");
                if mutate_response {
                    let encoded = value["data"]["state.json"]
                        .as_str()
                        .ok_or_else(|| std::io::Error::other("missing state"))?;
                    let mut state: Value = serde_json::from_str(encoded)?;
                    state["nextSequence"] = json!("2");
                    value["data"]["state.json"] = json!(serde_json::to_string(&state)?);
                }
                value
            } else if parts.uri.path().contains("registry-authority.") {
                json!({"metadata":{"uid":"authority","resourceVersion":"1"},"data":{"state.json":serde_json::to_string(&json!({
                    "schemaVersion":platform::REGISTRY_AUTHORITY_SCHEMA_VERSION,
                    "registryUid":UID,"controlNamespace":"control","workloadNamespace":"workload","clusterBinding":"cluster-a"
                }))?}})
            } else {
                let mut value = json!({"metadata":{"uid":UID,"resourceVersion":"1"}});
                if let Some(state) = initial {
                    value["data"] = json!({"state.json":serde_json::to_string(&state)?});
                }
                value
            };
            response["apiVersion"] = json!("v1");
            response["kind"] = json!("ConfigMap");
            let mut reply = Response::new(Body::from(serde_json::to_vec(&response)?));
            reply.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            Ok::<_, std::io::Error>(reply)
        }
    });
    (Client::new(service, "control"), calls)
}

fn state() -> Result<Value> {
    Ok(serde_json::to_value(NamespaceRegistryState {
        audit_heads: BTreeMap::new(),
        registry_uid: UID.into(),
        next_sequence: 1,
        schema_version: platform::REGISTRY_SCHEMA_VERSION.into(),
        cluster_binding: "cluster-a".into(),
        workload_namespace: "workload".into(),
        claim_snapshot: snapshot().reference()?,
        histories: BTreeMap::default(),
        active_writer_checkpoints: BTreeMap::default(),
        transitions: BTreeMap::default(),
    })?)
}
fn methods(calls: &Calls) -> Result<Vec<String>> {
    Ok(calls.lock().map_err(|_| "fixture lock")?.clone())
}

#[tokio::test]
async fn missing_store_fails_before_kubernetes_io() -> Result {
    for claims_configured in [false, true] {
        let (client, calls) = client(Some(state()?), false);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?;
        let platform = if claims_configured {
            platform.with_claim_store(Arc::new(Store(Fault::None)))
        } else {
            platform
        };
        let error = platform
            .load_registry()
            .await
            .err()
            .ok_or("missing store accepted")?;
        if claims_configured {
            assert!(error.to_string().contains("audit store"));
        }
        assert!(platform.bootstrap_registry(UID, "1", &[]).await.is_err());
        assert!(methods(&calls)?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn selected_missing_corrupt_and_failed_storage_never_become_empty_claims() -> Result {
    for fault in [Fault::Missing, Fault::WrongSnapshot, Fault::Backend] {
        let (client, calls) = client(Some(state()?), false);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
            .with_claim_store(Arc::new(Store(fault)))
            .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()));
        assert!(platform.load_registry().await.is_err());
        assert_eq!(methods(&calls)?, ["GET", "GET"]);
    }
    Ok(())
}

#[tokio::test]
async fn bootstrap_rejects_lying_store_before_any_kubernetes_write() -> Result {
    for fault in [
        Fault::Missing,
        Fault::WrongSnapshot,
        Fault::WrongReference,
        Fault::Backend,
    ] {
        let (client, calls) = client(None, false);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
            .with_claim_store(Arc::new(Store(fault)))
            .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()));
        assert!(
            platform
                .bootstrap_registry(UID, "1", &["example".into()])
                .await
                .is_err()
        );
        assert_eq!(methods(&calls)?, ["GET", "GET"]);
    }
    Ok(())
}

#[tokio::test]
async fn bootstrap_checks_exact_admission_response() -> Result {
    for mutate in [false, true] {
        let (client, calls) = client(None, mutate);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
            .with_claim_store(Arc::new(Store(Fault::None)))
            .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()));
        let result = platform
            .bootstrap_registry(UID, "1", &["example".into()])
            .await;
        assert_eq!(result.is_err(), mutate);
        assert_eq!(methods(&calls)?, ["GET", "GET", "GET", "PUT"]);
        if let Ok(registry) = result {
            assert!(registry.value.claims.is_empty());
        }
    }
    Ok(())
}

#[tokio::test]
async fn legacy_registry_requires_explicit_recovery() -> Result {
    for version in ["v2", "v3", "v4"] {
        let mut old = state()?;
        old["schemaVersion"] = json!(format!("reakt.dev/namespace-registry/{version}"));
        let (client, calls) = client(Some(old), false);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
            .with_claim_store(Arc::new(Store(Fault::None)))
            .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()));
        let error = platform
            .load_registry()
            .await
            .err()
            .ok_or("legacy registry accepted")?;
        assert!(error.to_string().contains("explicit recovery"));
        assert_eq!(methods(&calls)?, ["GET"]);
    }
    Ok(())
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn protected_reference_survives_real_local_database_reopen() -> Result {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("metadata.db");
    {
        let store = coordination::LibsqlCoordination::open_local(&path).await?;
        assert_eq!(
            store.put_claims(&snapshot()).await?,
            snapshot().reference()?
        );
    }
    for _ in 0..2 {
        let store = coordination::LibsqlCoordination::open_local(&path).await?;
        let (client, calls) = client(Some(state()?), false);
        let platform = PlatformClient::new(client, "control", "workload", "cluster-a")?
            .with_claim_store(Arc::new(store))
            .with_audit_store(Arc::new(audit_support::MemoryAuditStore::default()));
        let loaded = platform.load_registry().await?;
        assert_eq!(loaded.value.claim_snapshot()?, snapshot());
        assert_eq!(methods(&calls)?, ["GET", "GET"]);
    }
    Ok(())
}
#[path = "support/audit.rs"]
mod audit_support;
