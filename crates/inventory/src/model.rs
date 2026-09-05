use crate::{Error, Result};
use compiler_core::DeletionPolicy;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const INVENTORY_SCHEMA: &str = "reakt.dev/resource-inventory/v1";
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InventoryScope {
    pub cluster: String,
    pub namespace: String,
    pub controller: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceKey {
    pub api_group: String,
    pub resource: String,
    pub namespace: String,
    pub name: String,
}

/// Deliberately no manifest, annotations, labels, source path, arbitrary map,
/// observed values or content hash. UID is evidence of identity, not authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackedResource {
    pub identity: ResourceKey,
    pub api_version: String,
    pub kind: String,
    pub uid: Option<String>,
    pub deletion_policy: DeletionPolicy,
}

impl TrackedResource {
    /// Project only approved identity fields; never serialize the source resource.
    #[must_use]
    pub fn from_resource(
        resource: &compiler_core::Resource,
        plural: String,
        uid: Option<String>,
    ) -> Self {
        let group = resource
            .api_version
            .split_once('/')
            .map_or("", |(group, _)| group);
        Self {
            identity: ResourceKey {
                api_group: group.to_owned(),
                resource: plural,
                namespace: resource.namespace.clone(),
                name: resource.name.clone(),
            },
            api_version: resource.api_version.clone(),
            kind: resource.kind.clone(),
            uid,
            deletion_policy: resource.lifecycle.deletion_policy,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InventorySnapshot {
    pub schema_version: String,
    pub generation: String,
    pub resources: Vec<TrackedResource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VersionedInventory {
    pub revision: String,
    pub snapshot_id: String,
    pub snapshot: InventorySnapshot,
}

impl InventoryScope {
    pub(crate) fn key(&self) -> Result<String> {
        if (self.cluster.is_empty()
            || self.cluster.len() > 256
            || self.cluster.chars().any(char::is_control))
            || !identifier(&self.namespace, 63)
            || !identifier(&self.controller, 63)
        {
            return Err(Error::Invalid("scope"));
        }
        serde_json::to_string(self).map_err(|_| Error::Invalid("scope encoding"))
    }
}

impl InventorySnapshot {
    /// Validate a bounded, sorted metadata-only snapshot and return canonical bytes.
    /// # Errors
    /// Rejects duplicate identities, foreign namespaces or unsupported metadata.
    pub fn encode(&self, scope: &InventoryScope) -> Result<Vec<u8>> {
        scope.key()?;
        if self.schema_version != INVENTORY_SCHEMA
            || !opaque_identifier(&self.generation, 128)
            || self.resources.len() > 100_000
        {
            return Err(Error::Invalid("snapshot header or resource count"));
        }
        let mut previous = None;
        for resource in &self.resources {
            let key = &resource.identity;
            if key.namespace != scope.namespace
                || (!key.api_group.is_empty() && !identifier(&key.api_group, 253))
                || !identifier(&key.resource, 63)
                || !identifier(&key.name, 253)
                || resource.kind.is_empty()
                || resource.kind.len() > 63
                || !resource.kind.bytes().all(|b| b.is_ascii_alphanumeric())
                || resource
                    .uid
                    .as_ref()
                    .is_some_and(|uid| !opaque_identifier(uid, 128))
                || previous.is_some_and(|last| last >= key)
            {
                return Err(Error::Invalid("resource identity, ordering or UID"));
            }
            let (group, version) = resource
                .api_version
                .split_once('/')
                .unwrap_or(("", &resource.api_version));
            if group != key.api_group || !identifier(version, 63) {
                return Err(Error::Invalid("API version"));
            }
            previous = Some(key);
        }
        let bytes = compiler_core::canonical_bytes(self)
            .map_err(|_| Error::Invalid("snapshot encoding"))?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::Invalid("snapshot size"));
        }
        Ok(bytes)
    }

    /// Select omitted resources using identity only. The prior policy is retained.
    /// Callers still require lifecycle authorization and live UID preconditions.
    #[must_use]
    pub fn removed_from<'a>(&'a self, next: &Self) -> Vec<&'a TrackedResource> {
        let retained: BTreeSet<_> = next.resources.iter().map(|r| &r.identity).collect();
        self.resources
            .iter()
            .filter(|r| !retained.contains(&r.identity))
            .collect()
    }
}

#[cfg(feature = "libsql-backend")]
pub(crate) fn snapshot_id(snapshot: &InventorySnapshot) -> Result<String> {
    compiler_core::digest(snapshot).map_err(|_| Error::Invalid("snapshot digest"))
}

#[cfg(feature = "libsql-backend")]
pub(crate) fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn opaque_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
