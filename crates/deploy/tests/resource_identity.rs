use compiler_core::{
    ApiResourceMapping, ApiResourceScope, DeletionPolicy, Lifecycle, Resource, SourceLocation,
};
use deploy::{Error, ResourceIdentity, resource_identity};
use serde_json::json;

fn resource(api_version: &str, kind: &str) -> Resource {
    Resource {
        force: false,
        id: "fixture".into(),
        api_version: api_version.into(),
        kind: kind.into(),
        api_resource: None,
        name: "fixture".into(),
        namespace: "workload".into(),
        desired: json!({}),
        lifecycle: Lifecycle {
            deletion_policy: DeletionPolicy::Orphan,
        },
        source: SourceLocation {
            file: "fixture.tsx".into(),
            line: 1,
            column: 1,
        },
    }
}

#[test]
fn projects_supported_lifecycle_resource_identities() -> Result<(), Error> {
    for (api_version, kind, api_group, plural) in [
        ("v1", "ConfigMap", "", "configmaps"),
        ("v1", "Service", "", "services"),
        ("apps/v1", "Deployment", "apps", "deployments"),
    ] {
        assert_eq!(
            resource_identity(&resource(api_version, kind))?,
            ResourceIdentity {
                api_group: api_group.into(),
                resource: plural.into(),
                namespace: "workload".into(),
                name: "fixture".into(),
            }
        );
    }
    Ok(())
}

#[test]
fn rejects_unsupported_lifecycle_resource_identity() {
    assert_eq!(
        resource_identity(&resource("v1", "Secret")),
        Err(Error::Plan("unsupported lifecycle resource".into()))
    );
}

#[test]
fn projects_custom_resource_identity_and_rejects_builtin_remapping() -> Result<(), Error> {
    let mut cluster = resource("postgresql.cnpg.io/v1", "Cluster");
    cluster.api_resource = Some(ApiResourceMapping {
        plural: "clusters".into(),
        scope: ApiResourceScope::Namespaced,
        schema_sha256: format!("sha256:{}", "a".repeat(64)),
    });
    assert_eq!(
        resource_identity(&cluster)?,
        ResourceIdentity {
            api_group: "postgresql.cnpg.io".into(),
            resource: "clusters".into(),
            namespace: "workload".into(),
            name: "fixture".into(),
        }
    );

    let mut config_map = resource("v1", "ConfigMap");
    config_map.api_resource = cluster.api_resource;
    assert!(resource_identity(&config_map).is_err());
    Ok(())
}
