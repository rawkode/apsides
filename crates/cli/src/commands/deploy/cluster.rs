//! Connecting to the cluster and binding the release to protected trust policy.
//!
//! Authentication comes from the invoking environment's standard Kubernetes
//! configuration, never from files or executable hooks inside the release.

use crate::Result;
use compiler_core::{Capability, CompiledPlan, Resource};
use kube::{Client, Config};
use platform::{IssuedDeployment, PlatformClient};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

pub(super) struct DeploymentContext {
    pub(super) issued: IssuedDeployment,
    pub(super) verified: release::IntegrityVerifiedCandidate,
    pub(super) compatibility: Value,
}

pub(super) async fn connect(
    root: &Path,
    expected_digest: Option<&str>,
) -> Result<DeploymentContext> {
    let (_, plan, discovered_manifest) = release::read_release(root)?;
    let domain = &plan.ir.domain;
    // Authentication comes from the invoking environment's standard Kubernetes
    // configuration, never from files or executable hooks inside the release.
    let mut config = Config::infer().await?;
    if config.cluster_url.scheme_str() != Some("https") || config.accept_invalid_certs {
        return Err("deployment requires an authenticated TLS Kubernetes endpoint".into());
    }
    config.default_retry = false;
    config.connect_timeout = Some(Duration::from_secs(10));
    config.read_timeout = Some(Duration::from_secs(30));
    config.write_timeout = Some(Duration::from_secs(30));
    let bootstrap = Client::try_from(config.clone())?;
    let platform = PlatformClient::new(
        bootstrap.clone(),
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
    )?;
    let trust = platform.load_trust_policy(&domain.id).await?;
    let policy = serde_json::from_value(trust.value)?;
    let verified = release::verify_candidate(root, &policy).await?;
    verify_artifact_binding(
        &discovered_manifest,
        verified.release_digest(),
        expected_digest,
    )?;
    let domain = &verified.compiled().ir.domain;
    let compatibility = compatibility(&bootstrap, verified.compiled()).await?;
    let coordination = crate::connection::connect_coordination().await?;
    let issued = IssuedDeployment::connect(
        config,
        &domain.control_namespace,
        &domain.namespace,
        &domain.cluster_binding,
        &domain.id,
    )
    .await?
    .with_stores(coordination.clone(), coordination.clone(), coordination);
    Ok(DeploymentContext {
        issued,
        verified,
        compatibility,
    })
}

fn verify_artifact_binding(
    discovered: &[u8],
    verified_digest: &str,
    requested: Option<&str>,
) -> Result<()> {
    use sha2::{Digest as _, Sha256};
    let discovered_digest = format!("sha256:{:x}", Sha256::digest(discovered));
    if discovered_digest != verified_digest {
        return Err("release changed between scope discovery and policy verification".into());
    }
    if requested.is_some_and(|expected| expected != verified_digest) {
        return Err("verified release differs from the requested OCI release".into());
    }
    Ok(())
}

/// Requires the observed server's `major`/`minor` discovery fields to meet
/// or exceed `min_kubernetes_version` (the domain's declared floor). The
/// absent-only conditional SSA creation contract itself is qualified only
/// on v1.34.3; a raised floor still runs against whatever server the
/// operator deploys to.
fn require_minimum_server_version(
    version: &k8s_openapi::apimachinery::pkg::version::Info,
    min_kubernetes_version: &str,
) -> Result<()> {
    let minimum = compiler_core::parse_min_version(min_kubernetes_version)
        .ok_or("domain.minKubernetesVersion is invalid")?;
    let observed =
        compiler_core::parse_server_version(&version.major, &version.minor).ok_or_else(|| {
            format!(
                "Kubernetes server version is unparsable: major={}, minor={}",
                version.major, version.minor
            )
        })?;
    if observed < minimum {
        return Err(format!(
            "Kubernetes {} does not meet the domain's declared minimum {}.{}; the absent-only SSA creation contract is qualified on v1.34.3",
            version.git_version, minimum.0, minimum.1
        )
        .into());
    }
    Ok(())
}

async fn compatibility(client: &Client, plan: &CompiledPlan) -> Result<Value> {
    let version = client.apiserver_version().await?;
    require_minimum_server_version(&version, &plan.ir.domain.min_kubernetes_version)?;
    let targets = discovery_targets(&plan.ir.resources, &plan.capabilities)?;
    let group_versions: BTreeSet<_> = targets.values().flatten().collect();
    let mut served = BTreeMap::new();
    for group_version in group_versions {
        served.insert(
            group_version.clone(),
            served_resources(client, group_version).await?,
        );
    }
    require_served(&plan.capabilities, &targets, &served)?;
    Ok(
        json!({"serverVersion":version.git_version,"requiredApis":"available","fieldValidation":"Strict","admission":"each mutation requires successful server dry-run"}),
    )
}

/// One entry of a served group-version's API resource list.
#[derive(Debug)]
struct ServedResource {
    name: String,
    namespaced: bool,
    verbs: Vec<String>,
}

/// An RBAC-shaped `(api group, resource plural)` pair.
type GroupResource = (String, String);

/// Lists one group-version's resources; a group-version the server does not
/// serve reports no resources, so the capability check names what is missing.
async fn served_resources(client: &Client, group_version: &str) -> Result<Vec<ServedResource>> {
    let listed = if group_version.contains('/') {
        client.list_api_group_resources(group_version).await
    } else {
        client.list_core_api_resources(group_version).await
    };
    let list = match listed {
        Ok(list) => list,
        Err(kube::Error::Api(status)) if status.code == 404 => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(list
        .resources
        .into_iter()
        .map(|resource| ServedResource {
            name: resource.name,
            namespaced: resource.namespaced,
            verbs: resource.verbs,
        })
        .collect())
}

/// Maps every capability's `(group, plural)` to the group-versions the IR
/// declares for it. Core resources that appear only as inputs are served at
/// `v1`; any other undeclared group fails closed.
fn discovery_targets(
    resources: &[Resource],
    capabilities: &[Capability],
) -> Result<BTreeMap<GroupResource, BTreeSet<String>>> {
    let mut declared: BTreeMap<GroupResource, BTreeSet<String>> = BTreeMap::new();
    for resource in resources {
        let api = compiler_core::resolve_resource_api(resource)?;
        let group_version = if api.api_group.is_empty() {
            api.version
        } else {
            format!("{}/{}", api.api_group, api.version)
        };
        declared
            .entry((api.api_group, api.plural))
            .or_default()
            .insert(group_version);
    }
    let mut targets = BTreeMap::new();
    for capability in capabilities {
        let key = (capability.api_group.clone(), capability.resource.clone());
        let versions = match declared.get(&key) {
            Some(versions) => versions.clone(),
            None if capability.api_group.is_empty() => BTreeSet::from(["v1".to_owned()]),
            None => {
                return Err(format!(
                    "required API capability has no declared version: {}/{}",
                    capability.api_group, capability.resource
                )
                .into());
            }
        };
        targets.insert(key, versions);
    }
    Ok(targets)
}

/// Requires each capability's resource, with its verb, to be namespaced and
/// served in a group-version the IR declared for it.
fn require_served(
    capabilities: &[Capability],
    targets: &BTreeMap<GroupResource, BTreeSet<String>>,
    served: &BTreeMap<String, Vec<ServedResource>>,
) -> Result<()> {
    for capability in capabilities {
        let name = capability.subresource.as_ref().map_or_else(
            || capability.resource.clone(),
            |sub| format!("{}/{sub}", capability.resource),
        );
        let key = (capability.api_group.clone(), capability.resource.clone());
        let available = targets.get(&key).is_some_and(|versions| {
            versions.iter().any(|group_version| {
                served.get(group_version).is_some_and(|resources| {
                    resources.iter().any(|r| {
                        r.name == name && r.namespaced && r.verbs.contains(&capability.verb)
                    })
                })
            })
        });
        if !available {
            return Err(format!(
                "required namespaced API capability is unavailable: {}/{name} {}",
                capability.api_group, capability.verb
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ServedResource, discovery_targets, require_minimum_server_version, require_served,
        verify_artifact_binding,
    };
    use compiler_core::{
        ApiResourceMapping, ApiResourceScope, Capability, DeletionPolicy, Lifecycle, Resource,
        SourceLocation,
    };
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use std::collections::BTreeMap;

    fn custom(api_version: &str, kind: &str, plural: &str) -> Resource {
        Resource {
            force: false,
            id: plural.into(),
            api_version: api_version.into(),
            kind: kind.into(),
            api_resource: Some(ApiResourceMapping {
                plural: plural.into(),
                scope: ApiResourceScope::Namespaced,
                schema_sha256: format!("sha256:{}", "a".repeat(64)),
            }),
            name: "example".into(),
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
        }
    }

    fn capability(api_group: &str, resource: &str, verb: &str) -> Capability {
        Capability {
            namespace: "workload".into(),
            api_group: api_group.into(),
            resource: resource.into(),
            subresource: None,
            verb: verb.into(),
            names: vec!["example".into()],
        }
    }

    fn served(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<ServedResource>> {
        entries
            .iter()
            .map(|(group_version, names)| {
                let resources = names
                    .iter()
                    .map(|name| ServedResource {
                        name: (*name).into(),
                        namespaced: true,
                        verbs: vec!["get".into(), "list".into(), "watch".into(), "patch".into()],
                    })
                    .collect();
                ((*group_version).into(), resources)
            })
            .collect()
    }

    fn check(
        resources: &[Resource],
        capabilities: &[Capability],
        served: &BTreeMap<String, Vec<ServedResource>>,
    ) -> crate::Result<()> {
        require_served(
            capabilities,
            &discovery_targets(resources, capabilities)?,
            served,
        )
    }

    #[test]
    fn one_group_reaches_each_resource_at_its_own_declared_version() -> crate::Result<()> {
        let resources = [
            custom("gateway.networking.k8s.io/v1", "HTTPRoute", "httproutes"),
            custom(
                "gateway.networking.k8s.io/v1beta1",
                "ReferenceGrant",
                "referencegrants",
            ),
        ];
        let capabilities = [
            capability("gateway.networking.k8s.io", "httproutes", "patch"),
            capability("gateway.networking.k8s.io", "referencegrants", "patch"),
        ];
        let served = served(&[
            ("gateway.networking.k8s.io/v1", &["httproutes"]),
            ("gateway.networking.k8s.io/v1beta1", &["referencegrants"]),
        ]);
        check(&resources, &capabilities, &served)
    }

    #[test]
    fn alpha_only_custom_resource_is_discovered_at_its_declared_version() -> crate::Result<()> {
        let resources = [custom(
            "authzed.com/v1alpha1",
            "SpiceDBCluster",
            "spicedbclusters",
        )];
        let capabilities = [capability("authzed.com", "spicedbclusters", "list")];
        let served = served(&[("authzed.com/v1alpha1", &["spicedbclusters"])]);
        check(&resources, &capabilities, &served)
    }

    #[test]
    fn resource_served_only_at_another_version_is_unavailable() {
        let resources = [custom(
            "external-secrets.io/v1beta1",
            "ExternalSecret",
            "externalsecrets",
        )];
        let capabilities = [capability(
            "external-secrets.io",
            "externalsecrets",
            "patch",
        )];
        let served = served(&[("external-secrets.io/v1", &["externalsecrets"])]);
        assert!(check(&resources, &capabilities, &served).is_err());
    }

    #[test]
    fn undeclared_non_core_group_fails_closed() {
        let capabilities = [capability(
            "external-secrets.io",
            "externalsecrets",
            "patch",
        )];
        let served = served(&[("external-secrets.io/v1", &["externalsecrets"])]);
        assert!(check(&[], &capabilities, &served).is_err());
    }

    #[test]
    fn core_inputs_are_discovered_at_v1() -> crate::Result<()> {
        let capabilities = [
            capability("", "configmaps", "get"),
            capability("", "secrets", "watch"),
        ];
        let targets = discovery_targets(&[], &capabilities)?;
        assert!(
            targets
                .values()
                .all(|versions| versions.len() == 1 && versions.contains("v1"))
        );
        require_served(
            &capabilities,
            &targets,
            &served(&[("v1", &["configmaps", "secrets"])]),
        )?;
        assert!(
            require_served(&capabilities, &targets, &served(&[("v1", &["configmaps"])])).is_err()
        );
        Ok(())
    }

    #[test]
    fn scope_discovery_and_approved_release_must_be_the_same_snapshot() -> crate::Result<()> {
        let discovered = br#"{"scope":"first"}"#;
        let same = format!("sha256:{:x}", Sha256::digest(discovered));
        let changed = format!("sha256:{:x}", Sha256::digest(br#"{"scope":"second"}"#));
        verify_artifact_binding(discovered, &same, None)?;
        verify_artifact_binding(discovered, &same, Some(&same))?;
        assert!(verify_artifact_binding(discovered, &changed, None).is_err());
        assert!(verify_artifact_binding(discovered, &changed, Some(&changed)).is_err());
        assert!(verify_artifact_binding(discovered, &same, Some(&changed)).is_err());
        Ok(())
    }

    #[test]
    fn deploy_compatibility_requires_the_declared_minimum_server_version() {
        let server = |major: &str, minor: &str, git_version: &str| {
            k8s_openapi::apimachinery::pkg::version::Info {
                major: major.into(),
                minor: minor.into(),
                git_version: git_version.into(),
                ..Default::default()
            }
        };
        assert!(require_minimum_server_version(&server("1", "35", "v1.35.0"), "1.34").is_ok());
        assert!(require_minimum_server_version(&server("1", "34", "v1.34.3"), "1.34").is_ok());
        assert!(require_minimum_server_version(&server("1", "33", "v1.33.9"), "1.34").is_err());
        assert!(require_minimum_server_version(&server("1", "34", "v1.34.3"), "1.35").is_err());
        assert!(require_minimum_server_version(&server("garbage", "garbage", ""), "1.34").is_err());
    }
}
