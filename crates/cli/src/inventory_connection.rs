//! Deployment connection configuration never enters resource inventory records.
use crate::Result;
use inventory::{InventoryScope, InventoryStore, LibsqlInventory, VersionedInventory};
use platform::{NamespaceRegistry, PlatformClient};
use std::{path::Path, sync::Arc, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(30);

pub async fn connect() -> Result<Box<dyn InventoryStore>> {
    let (database, token) = configuration()?;
    open(&database, token).await
}

/// Claim coordination shares the configured persistent metadata database, using
/// separate immutable tables. These credentials are never passed to the runtime.
pub async fn connect_claims() -> Result<Arc<dyn coordination::ClaimStore>> {
    let (database, token) = configuration()?;
    let store = tokio::time::timeout(TIMEOUT, async {
        if Path::new(&database).is_absolute() {
            if token.is_some() {
                return Err("local coordination does not accept a remote token".into());
            }
            Ok(coordination::LibsqlCoordination::open_local(Path::new(&database)).await?)
        } else {
            let token = token.ok_or("remote coordination requires REAKT_INVENTORY_TOKEN")?;
            Ok::<_, Box<dyn std::error::Error>>(
                coordination::LibsqlCoordination::open_remote(database, token).await?,
            )
        }
    })
    .await
    .map_err(|_| "coordination connection timed out")??;
    Ok(Arc::new(store))
}

fn configuration() -> Result<(String, Option<String>)> {
    let database = std::env::var("REAKT_INVENTORY_DATABASE").map_err(
        |_| "set REAKT_INVENTORY_DATABASE to a persistent absolute path or TLS libSQL endpoint",
    )?;
    let token = match std::env::var("REAKT_INVENTORY_TOKEN") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err("inventory token must be valid UTF-8".into()),
    };
    Ok((database, token))
}

async fn open(database: &str, token: Option<String>) -> Result<Box<dyn InventoryStore>> {
    let store = tokio::time::timeout(TIMEOUT, async {
        if Path::new(database).is_absolute() {
            if token.is_some() {
                return Err("local inventory does not accept a remote token".into());
            }
            Ok(LibsqlInventory::open_local(Path::new(database)).await?)
        } else {
            let endpoint = url::Url::parse(database).map_err(|_| "invalid inventory endpoint")?;
            if !matches!(endpoint.scheme(), "https" | "libsql")
                || endpoint.host_str().is_none()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
                || !matches!(endpoint.path(), "" | "/")
            {
                return Err(
                    "inventory endpoint must use TLS and contain no credentials, query, or path"
                        .into(),
                );
            }
            let token = token
                .filter(|value| {
                    !value.is_empty() && value.len() <= 8192 && !value.chars().any(char::is_control)
                })
                .ok_or("remote inventory requires REAKT_INVENTORY_TOKEN")?;
            Ok::<_, Box<dyn std::error::Error>>(
                LibsqlInventory::open_remote(database.into(), token).await?,
            )
        }
    })
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

pub async fn capture_active(
    platform: &PlatformClient,
    store: &dyn InventoryStore,
    domain: &compiler_core::Domain,
) -> Result<platform::Versioned<NamespaceRegistry>> {
    let head = load(store, domain)
        .await?
        .ok_or("active inventory is missing; recovery required")?;
    let registry = platform
        .capture_active_claim_uids(&domain.id, &head.snapshot)
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
    use inventory::{INVENTORY_SCHEMA, InventorySnapshot};

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
        }
    }

    #[tokio::test]
    async fn local_connection_reopens_and_requires_exact_predecessor() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.db");
        let path = path.to_str().ok_or("test path must be UTF-8")?;
        let store = open(path, None).await?;
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
        let store = open(path, None).await?;
        require_head(store.as_ref(), &domain(), Some("g1")).await?;
        assert!(require_head(store.as_ref(), &domain(), None).await.is_err());
        assert!(
            require_head(store.as_ref(), &domain(), Some("g2"))
                .await
                .is_err()
        );
        drop(store);
        std::fs::write(path, b"corrupt database")?;
        assert!(open(path, None).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn invalid_connection_configuration_is_rejected_without_secrets() -> Result<()> {
        for (database, token) in [
            ("relative.db", None),
            ("http://example.invalid", Some("private-token".into())),
            (
                "https://private-user:private-password@example.invalid",
                Some("private-token".into()),
            ),
            (
                "libsql://example.invalid?token=private-value",
                Some("private-token".into()),
            ),
            ("https://example.invalid/path", Some("private-token".into())),
            ("libsql://example.invalid", None),
            ("/unused-inventory.db", Some("private-token".into())),
        ] {
            let Err(error) = open(database, token).await else {
                return Err("invalid configuration accepted".into());
            };
            assert!(!error.to_string().contains("private-"));
        }
        Ok(())
    }
}
