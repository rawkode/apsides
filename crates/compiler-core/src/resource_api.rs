use crate::{ApiResourceMapping, ApiResourceScope, Error, Resource, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedApiResource {
    pub api_group: String,
    pub version: String,
    pub kind: String,
    pub plural: String,
    pub scope: ApiResourceScope,
}

fn dns_label(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn dns_subdomain(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(|part| dns_label(part, 63))
}

fn kind(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn builtin(kind: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match kind {
        "ConfigMap" => Some(("v1", "", "configmaps")),
        "Service" => Some(("v1", "", "services")),
        "Deployment" => Some(("apps/v1", "apps", "deployments")),
        _ => None,
    }
}

/// Validate a resolved namespaced Kubernetes REST identity.
///
/// # Errors
/// Rejects malformed GVK/GVR pairs and alternate routing for builtin resources.
pub fn resolve_namespaced_api(
    api_version: &str,
    kind_name: &str,
    api_group: &str,
    plural: &str,
) -> Result<ResolvedApiResource> {
    let (derived_group, version) = api_version
        .split_once('/')
        .map_or(("", api_version), |(group, version)| (group, version));
    let custom = builtin(kind_name).is_none();
    let valid_version = dns_label(version, 63);
    let valid_group = if custom {
        !derived_group.is_empty() && dns_subdomain(derived_group)
    } else {
        derived_group.is_empty() || dns_subdomain(derived_group)
    };
    let builtin_matches = builtin(kind_name).is_none_or(|(version, group, resource)| {
        api_version == version && api_group == group && plural == resource
    });
    if api_version.matches('/').count() > 1
        || derived_group != api_group
        || !valid_group
        || !valid_version
        || !kind(kind_name)
        || !dns_label(plural, 63)
        || !builtin_matches
    {
        return Err(Error::Invalid(
            "invalid namespaced Kubernetes API resource mapping".into(),
        ));
    }
    Ok(ResolvedApiResource {
        api_group: api_group.into(),
        version: version.into(),
        kind: kind_name.into(),
        plural: plural.into(),
        scope: ApiResourceScope::Namespaced,
    })
}

/// Resolve the canonical REST identity for one managed resource.
///
/// # Errors
/// Rejects unsupported builtins, remapped builtins, malformed custom resource
/// mappings, cluster-scoped resources, and invalid schema digests.
pub fn resolve_resource_api(resource: &Resource) -> Result<ResolvedApiResource> {
    if resource.kind == "Secret" {
        return Err(Error::Invalid(
            "Secret is observation-only and cannot be a managed resource".into(),
        ));
    }
    match (builtin(&resource.kind), &resource.api_resource) {
        (Some((api_version, group, plural)), None) if resource.api_version == api_version => {
            resolve_namespaced_api(&resource.api_version, &resource.kind, group, plural)
        }
        (Some(_), _) => Err(Error::Invalid(
            "builtin Kubernetes resources require their canonical REST mapping".into(),
        )),
        (
            None,
            Some(ApiResourceMapping {
                plural,
                scope,
                schema_sha256,
            }),
        ) => {
            if *scope != ApiResourceScope::Namespaced || !digest(schema_sha256) {
                return Err(Error::Invalid(
                    "invalid custom resource scope or schema digest".into(),
                ));
            }
            let group = resource
                .api_version
                .split_once('/')
                .map_or("", |(group, _)| group);
            resolve_namespaced_api(&resource.api_version, &resource.kind, group, plural)
        }
        (None, None) => Err(Error::Invalid(
            "custom managed resource requires an explicit REST mapping".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeletionPolicy, Lifecycle, SourceLocation};
    use serde_json::json;

    fn resource(api_version: &str, kind: &str) -> Resource {
        Resource {
            force: false,
            id: "database".into(),
            api_version: api_version.into(),
            kind: kind.into(),
            api_resource: None,
            name: "database".into(),
            namespace: "workload".into(),
            desired: json!({}),
            lifecycle: Lifecycle {
                deletion_policy: DeletionPolicy::Orphan,
            },
            source: SourceLocation {
                file: "program.tsx".into(),
                line: 1,
                column: 1,
            },
            handler_output: false,
        }
    }

    fn mapping() -> ApiResourceMapping {
        ApiResourceMapping {
            plural: "clusters".into(),
            scope: ApiResourceScope::Namespaced,
            schema_sha256: format!("sha256:{}", "a".repeat(64)),
        }
    }

    #[test]
    fn resolves_builtin_and_custom_namespaced_resources() -> Result<()> {
        assert_eq!(
            resolve_resource_api(&resource("apps/v1", "Deployment"))?,
            ResolvedApiResource {
                api_group: "apps".into(),
                version: "v1".into(),
                kind: "Deployment".into(),
                plural: "deployments".into(),
                scope: ApiResourceScope::Namespaced,
            }
        );
        let mut custom = resource("postgresql.cnpg.io/v1", "Cluster");
        custom.api_resource = Some(mapping());
        assert_eq!(resolve_resource_api(&custom)?.plural, "clusters");
        assert_eq!(
            resolve_resource_api(&custom)?.api_group,
            "postgresql.cnpg.io"
        );
        Ok(())
    }

    #[test]
    fn rejects_builtin_remapping_and_malformed_custom_mappings() {
        let mut builtin = resource("v1", "ConfigMap");
        builtin.api_resource = Some(mapping());
        assert!(resolve_resource_api(&builtin).is_err());

        for (api_version, kind, plural, digest) in [
            (
                "v1",
                "Cluster",
                "clusters",
                format!("sha256:{}", "a".repeat(64)),
            ),
            (
                "postgresql.cnpg.io/v1",
                "cluster",
                "clusters",
                format!("sha256:{}", "a".repeat(64)),
            ),
            (
                "postgresql.cnpg.io/v1",
                "Cluster",
                "Clusters",
                format!("sha256:{}", "a".repeat(64)),
            ),
            (
                "postgresql.cnpg.io/v1",
                "Cluster",
                "clusters",
                "sha256:wrong".into(),
            ),
        ] {
            let mut custom = resource(api_version, kind);
            custom.api_resource = Some(ApiResourceMapping {
                plural: plural.into(),
                scope: ApiResourceScope::Namespaced,
                schema_sha256: digest,
            });
            assert!(resolve_resource_api(&custom).is_err());
        }
    }
}
