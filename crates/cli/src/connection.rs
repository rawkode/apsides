//! Deployment connection configuration never enters resource inventory records.
use crate::Result;
use inventory::{
    InventoryScope, InventorySnapshot, InventoryStore, SqliteInventory, VersionedInventory,
};
use platform::{NamespaceRegistry, PlatformClient};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(30);

pub async fn connect() -> Result<Box<dyn InventoryStore>> {
    open(&database_path()?).await
}

/// Claim and audit coordination share the configured persistent metadata database, using
/// separate immutable tables. These credentials are never passed to the runtime.
pub async fn connect_coordination() -> Result<Arc<coordination::SqliteCoordination>> {
    open_coordination(false).await
}

pub async fn connect_audit_reader() -> Result<Arc<coordination::SqliteCoordination>> {
    open_coordination(true).await
}

async fn open_coordination(read_only: bool) -> Result<Arc<coordination::SqliteCoordination>> {
    let database = database_path()?;
    let store = tokio::time::timeout(TIMEOUT, async {
        if read_only {
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                coordination::SqliteCoordination::open_local_read_only(&database).await?,
            )
        } else {
            Ok(coordination::SqliteCoordination::open_local(&database).await?)
        }
    })
    .await
    .map_err(|_| "coordination connection timed out")??;
    Ok(Arc::new(store))
}

/// The metadata database is one absolute path on application-owned storage.
///
/// There is no remote endpoint: the compare-and-set publication protocol reads
/// back what it just committed, which a replica cannot promise.
fn database_path() -> Result<PathBuf> {
    configured_path(std::env::var("APSIDES_INVENTORY_DATABASE").ok().as_deref())
}

fn configured_path(database: Option<&str>) -> Result<PathBuf> {
    let database =
        database.ok_or("set APSIDES_INVENTORY_DATABASE to a persistent absolute path")?;
    let path = PathBuf::from(database);
    if !path.is_absolute() {
        return Err("APSIDES_INVENTORY_DATABASE must be an absolute path".into());
    }
    Ok(path)
}

async fn open(database: &Path) -> Result<Box<dyn InventoryStore>> {
    let store = tokio::time::timeout(TIMEOUT, SqliteInventory::open_local(database))
        .await
        .map_err(|_| "inventory connection timed out")??;
    Ok(Box::new(store))
}

pub async fn publish(
    platform: &PlatformClient,
    store: &dyn InventoryStore,
    registry: &NamespaceRegistry,
    transition: &str,
) -> Result<VersionedInventory> {
    let stored = registry.transition(transition)?;
    Ok(tokio::time::timeout(
        TIMEOUT,
        platform.publish_committed_inventory(store, transition, &stored.record.executor_id),
    )
    .await
    .map_err(|_| "inventory publication timed out; retry the same transition")??)
}

pub async fn require_head(
    store: &dyn InventoryStore,
    domain: &compiler_core::Domain,
    generation: Option<&str>,
) -> Result<()> {
    let head = load(store, domain).await?;
    if head.as_ref().map(|head| head.snapshot.generation.as_str()) != generation {
        return Err(
            "inventory head differs from the protected predecessor; recovery required".into(),
        );
    }
    Ok(())
}

pub(crate) async fn load(
    store: &dyn InventoryStore,
    domain: &compiler_core::Domain,
) -> Result<Option<VersionedInventory>> {
    let scope = InventoryScope {
        cluster: domain.cluster_binding.clone(),
        namespace: domain.namespace.clone(),
        controller: domain.id.clone(),
    };
    let head = tokio::time::timeout(TIMEOUT, store.load(&scope))
        .await
        .map_err(|_| "inventory read timed out")??;
    Ok(head)
}

pub async fn require_request(
    store: &dyn InventoryStore,
    request: &deploy::DeploymentRequest,
) -> Result<()> {
    let head = load(store, &request.next.compiled.ir.domain).await?;
    deploy::validate_transition_inventory(request, head.as_ref().map(|head| &head.snapshot))?;
    Ok(())
}

/// Capture the active claims' live UIDs into the registry and inventory only
/// after `admit` accepts the plan built from them; a rejection writes nothing.
pub async fn capture_active<F, Fut>(
    platform: &PlatformClient,
    store: &dyn InventoryStore,
    domain: &compiler_core::Domain,
    admit: F,
) -> Result<platform::Versioned<NamespaceRegistry>>
where
    F: FnOnce(platform::Versioned<NamespaceRegistry>, InventorySnapshot) -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send,
{
    let head = load(store, domain)
        .await?
        .ok_or("active inventory is missing; recovery required")?;
    let registry = platform
        .capture_active_claim_uids(&domain.id, &head.snapshot, admit)
        .await?;
    tokio::time::timeout(
        TIMEOUT,
        platform.publish_captured_inventory(store, &domain.id, &registry, &head),
    )
    .await
    .map_err(|_| "captured inventory publication timed out; retry deployment")??;
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inventory::INVENTORY_SCHEMA;

    fn domain() -> compiler_core::Domain {
        compiler_core::Domain {
            id: "controller".into(),
            namespace: "workload".into(),
            control_namespace: "control".into(),
            cluster_binding: "cluster".into(),
            policy_ref: "policy".into(),
            enforcement: "strict".into(),
            budgets: compiler_core::Budgets {
                max_resources: 1,
                max_inputs: 1,
                max_reactions: 1,
                max_object_bytes: 1024,
                max_queue_depth: 1,
                max_concurrent_requests: 1,
            },
            min_kubernetes_version: "1.34".into(),
        }
    }

    #[tokio::test]
    async fn local_connection_reopens_and_requires_exact_predecessor() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.db");
        let store = open(&path).await?;
        require_head(store.as_ref(), &domain(), None).await?;
        assert!(
            require_head(store.as_ref(), &domain(), Some("g1"))
                .await
                .is_err()
        );
        let scope = InventoryScope {
            cluster: "cluster".into(),
            namespace: "workload".into(),
            controller: "controller".into(),
        };
        store
            .publish(
                &scope,
                None,
                &InventorySnapshot {
                    schema_version: INVENTORY_SCHEMA.into(),
                    generation: "g1".into(),
                    resources: vec![],
                },
            )
            .await?;
        drop(store);
        let store = open(&path).await?;
        require_head(store.as_ref(), &domain(), Some("g1")).await?;
        assert!(require_head(store.as_ref(), &domain(), None).await.is_err());
        assert!(
            require_head(store.as_ref(), &domain(), Some("g2"))
                .await
                .is_err()
        );
        drop(store);
        std::fs::write(&path, b"corrupt database")?;
        assert!(open(&path).await.is_err());
        Ok(())
    }

    #[test]
    fn configuration_is_local_only_and_never_echoes_its_value() -> Result<()> {
        // The store is a local database; there is no endpoint.
        for database in [
            Some("private-relative.db"),
            Some("https://private-host.invalid"),
            None,
        ] {
            let Err(error) = configured_path(database) else {
                return Err("invalid configuration accepted".into());
            };
            assert!(!error.to_string().contains("private-"), "{error}");
        }
        assert_eq!(
            configured_path(Some("/var/lib/apsides/inventory.db"))?,
            PathBuf::from("/var/lib/apsides/inventory.db")
        );
        Ok(())
    }
}
