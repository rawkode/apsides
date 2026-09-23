//! Platform bootstrap: what a platform administrator installs once before any
//! `aps deploy` can run (docs/reference/PLATFORM-CONTRACT.md).
//!
//! The protected control objects, the empty namespace registry and its
//! authority anchor, the issuer and deployer identities with their
//! permissions, and the rollout key. The registry is initialised through
//! `PlatformClient::bootstrap_registry` with its claim, audit and review
//! stores in the same database file `aps` is given as
//! `APSIDES_INVENTORY_DATABASE`, exactly as a deployment job would find them.
//!
//! The permissions mirror what the retired Python harness granted: a fixed
//! deployer Role in the control namespace, every capability any release the
//! scenario deploys delegates (the deployer must hold what it grants), a read
//! of every object those releases manage and deletion of those they delete on
//! cleanup, exact reads of the registry authority, the target namespaces and
//! the node, and an issuer that can only read its trust inputs and mint the
//! deployer's token. One deployer serves every domain the registry is
//! bootstrapped for.

use crate::{
    Error, Result, ensure,
    objects::{Objects, text},
    pair::Candidate,
};
use compiler_core::{DeletionPolicy, Domain};
use k8s_openapi::api::{
    authentication::v1::{TokenRequest, TokenRequestSpec},
    core::v1::ServiceAccount,
};
use kube::{Api, api::PostParams};
use platform::{DEPLOYMENT_IDENTITY_SCHEMA_V2, DeploymentIdentity, PlatformClient, PolicyState};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};
use test_cluster::cluster::Endpoint;

/// The identity `aps` authenticates as: it can only mint the deployer token.
pub const ISSUER: &str = "ceiling-issuer";
/// The identity `aps deploy` acts as once it has minted a token.
pub const DEPLOYER: &str = "ceiling-deployer";
/// The Secret the deployer token is bound to.
pub const BINDING_SECRET: &str = "ceiling-binding";
/// The separately administered namespace holding the registry authority.
pub const PLATFORM_NAMESPACE: &str = "apsides-platform";
/// The rollout key's identifier.
pub const ROLLOUT_KEY_ID: &str = "cnpg-demo";
/// The cluster-scoped read of the target namespaces and node.
const TARGET_ROLE: &str = "apsides-acceptance-target";
/// How long each issuer token lives: long enough for one `aps deploy`.
const ISSUER_TOKEN_SECONDS: i64 = 600;

/// The bootstrapped platform, as the administrator who installed it.
pub struct Platform {
    objects: Objects,
    admin: PlatformClient,
    control: String,
    /// Each domain's policy `ConfigMap`, as last written.
    policies: BTreeMap<String, Value>,
    /// Each domain's 32-byte rollout key, for the secret-canary scan.
    pub rollout_keys: Vec<Vec<u8>>,
}

/// What bootstrap needs beyond the cluster.
///
/// Every release the scenario deploys, each domain's starting policy, the
/// trust policy, the database `aps` will be given, the node's name and the
/// digest recorded as the proxy's environment qualification.
pub struct Settings<'a> {
    /// Every release any step deploys. They share one scope: cluster
    /// binding, control namespace and workload namespace.
    pub candidates: &'a [&'a Candidate],
    /// The policy each domain starts with, by domain ID; exactly the domains
    /// of `candidates`.
    pub policies: &'a BTreeMap<String, PolicyState>,
    pub trust_policy: &'a Value,
    pub database: &'a Path,
    pub node: &'a str,
    pub node_image_digest: &'a str,
    pub environment_qualification: &'a str,
}

/// Install everything `aps deploy` requires, under each domain's starting
/// policy.
///
/// # Errors
/// Fails when the candidates do not share one scope, the policies do not
/// name exactly their domains, any object is refused or the registry cannot
/// be initialised.
pub async fn bootstrap(objects: &Objects, settings: &Settings<'_>) -> Result<Platform> {
    let scope = settings.scope()?;
    let control = scope.control_namespace.as_str();
    for namespace in [control, PLATFORM_NAMESPACE] {
        objects
            .create(
                &json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": namespace}}),
            )
            .await?;
    }
    let mut policies = BTreeMap::new();
    let mut rollout_keys = Vec::new();
    for (domain, policy) in settings.policies {
        let written = control_objects(objects, settings, scope, domain, policy).await?;
        policies.insert(domain.clone(), written);
    }
    let admin = registry(objects, settings, scope).await?;
    let accounts = accounts(objects, control).await?;
    for domain in settings.policies.keys() {
        rollout_keys.push(identity(objects, scope, domain, &accounts).await?);
    }
    permissions(objects, settings, scope).await?;
    Ok(Platform {
        objects: objects.clone(),
        admin,
        control: control.to_owned(),
        policies,
        rollout_keys,
    })
}

impl Settings<'_> {
    /// The scope every candidate shares, after checking that the policies
    /// name exactly the candidates' domains.
    fn scope(&self) -> Result<&Domain> {
        let [first, ..] = self.candidates else {
            return Err(Error::Input("bootstrap needs at least one release".into()));
        };
        let scope = &first.plan.ir.domain;
        let mut domains = BTreeSet::new();
        for candidate in self.candidates {
            let domain = &candidate.plan.ir.domain;
            ensure(
                domain.cluster_binding == scope.cluster_binding
                    && domain.control_namespace == scope.control_namespace
                    && domain.namespace == scope.namespace,
                "the scenario's releases do not share one scope",
            )?;
            domains.insert(domain.id.clone());
        }
        ensure(
            self.policies.keys().cloned().collect::<BTreeSet<_>>() == domains,
            "bootstrap policies do not name exactly the releases' domains",
        )?;
        Ok(scope)
    }
}

/// One domain's trust policy, policy and proxy policy; returns the policy
/// `ConfigMap`.
async fn control_objects(
    objects: &Objects,
    settings: &Settings<'_>,
    scope: &Domain,
    domain: &str,
    policy: &PolicyState,
) -> Result<Value> {
    let control = scope.control_namespace.as_str();
    state(
        objects,
        control,
        &format!("apsides-trust-{domain}"),
        settings.trust_policy,
    )
    .await?;
    let policy = state(
        objects,
        control,
        &format!("apsides-policy-{domain}"),
        &serde_json::to_value(policy)?,
    )
    .await?;
    state(
        objects,
        control,
        &format!("apsides-proxy-policy-{domain}"),
        &json!({
            "schemaVersion": platform::SCHEMA_VERSION,
            "domainId": domain,
            "clusterBinding": scope.cluster_binding,
            "envoyImage": platform::ENVOY_IMAGE,
            "adminImage": platform::ADMIN_IMAGE,
            "configurationTemplateDigest": compiler_core::digest(&platform::envoy_configuration("qualified-template")?)?,
            "environmentQualificationDigest": settings.environment_qualification,
            "nodeImageDigest": settings.node_image_digest,
            "certificateLifetimeSeconds": 86_400,
            "maxLedgerBytes": platform::MAX_PROXY_LEDGER_BYTES,
        }),
    )
    .await?;
    Ok(policy)
}

/// The empty registry, its authority anchor, and its initialisation for
/// every domain with the shared stores; returns the administrator's client
/// over those stores.
async fn registry(
    objects: &Objects,
    settings: &Settings<'_>,
    domain: &Domain,
) -> Result<PlatformClient> {
    let control = domain.control_namespace.as_str();
    let workload = domain.namespace.as_str();
    let registry = objects
        .create(&json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": format!("apsides-registry-{workload}"), "namespace": control},
        }))
        .await?;
    let registry_uid = text(&registry, "/metadata/uid").to_owned();
    state(
        objects,
        PLATFORM_NAMESPACE,
        &format!("apsides-registry-authority.{control}.{workload}"),
        &json!({
            "schemaVersion": platform::REGISTRY_AUTHORITY_SCHEMA_VERSION,
            "registryUid": registry_uid,
            "clusterBinding": domain.cluster_binding,
            "controlNamespace": control,
            "workloadNamespace": workload,
        }),
    )
    .await?;
    let store = Arc::new(coordination::SqliteCoordination::open_local(settings.database).await?);
    let admin = PlatformClient::new(
        objects.client().clone(),
        control,
        workload,
        &domain.cluster_binding,
    )?
    .with_claim_store(store.clone())
    .with_audit_store(store.clone())
    .with_review_store(store);
    let domains = settings.policies.keys().cloned().collect::<Vec<_>>();
    admin
        .bootstrap_registry(
            &registry_uid,
            text(&registry, "/metadata/resourceVersion"),
            &domains,
        )
        .await?;
    Ok(admin)
}

/// The issuer and deployer `ServiceAccount` UIDs and the binding Secret's.
struct Accounts {
    deployer: String,
    binding: String,
}

/// The issuer and deployer `ServiceAccounts` and the binding Secret every
/// domain's deployment identity names.
async fn accounts(objects: &Objects, control: &str) -> Result<Accounts> {
    let mut deployer = String::new();
    for name in [ISSUER, DEPLOYER] {
        let account = objects
            .create(&json!({
                "apiVersion": "v1", "kind": "ServiceAccount", "automountServiceAccountToken": false,
                "metadata": {"name": name, "namespace": control},
            }))
            .await?;
        if name == DEPLOYER {
            text(&account, "/metadata/uid").clone_into(&mut deployer);
        }
    }
    let binding = objects
        .create(&json!({
            "apiVersion": "v1", "kind": "Secret", "immutable": true, "type": "Opaque",
            "metadata": {"name": BINDING_SECRET, "namespace": control},
        }))
        .await?;
    Ok(Accounts {
        deployer,
        binding: text(&binding, "/metadata/uid").to_owned(),
    })
}

/// One domain's deployment identity policy and rollout key; returns the key.
async fn identity(
    objects: &Objects,
    scope: &Domain,
    domain: &str,
    accounts: &Accounts,
) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let control = scope.control_namespace.as_str();
    let identity = DeploymentIdentity {
        schema_version: DEPLOYMENT_IDENTITY_SCHEMA_V2.into(),
        domain_id: domain.to_owned(),
        cluster_binding: scope.cluster_binding.clone(),
        service_account_name: DEPLOYER.into(),
        service_account_uid: accounts.deployer.clone(),
        binding_secret_name: BINDING_SECRET.into(),
        binding_secret_uid: accounts.binding.clone(),
        audience: "https://kubernetes.default.svc.cluster.local".into(),
        lifetime_seconds: 900,
        preview: None,
    };
    state(
        objects,
        control,
        &format!("apsides-deploy-identity-{domain}"),
        &serde_json::to_value(&identity)?,
    )
    .await?;
    let rollout_key = crate::random_bytes(32)?;
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    objects
        .create(&json!({
            "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
            "metadata": {"name": format!("apsides-rollout-key-{domain}"), "namespace": control},
            "data": {"keyId": encode(ROLLOUT_KEY_ID.as_bytes()), "key": encode(&rollout_key)},
        }))
        .await?;
    Ok(rollout_key)
}

/// Create a protected state `ConfigMap` holding canonical `value`.
async fn state(objects: &Objects, namespace: &str, name: &str, value: &Value) -> Result<Value> {
    let bytes = compiler_core::canonical_bytes(value)?;
    let data =
        String::from_utf8(bytes).map_err(|_| Error::Infrastructure("state is not UTF-8".into()))?;
    objects
        .create(&json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": name, "namespace": namespace},
            "data": {"state.json": data},
        }))
        .await
}

fn rule(group: &str, resources: &[&str], verbs: &[&str], names: &[&str]) -> Value {
    let mut value = json!({"apiGroups": [group], "resources": resources, "verbs": verbs});
    if !names.is_empty() {
        value["resourceNames"] = json!(names);
    }
    value
}

/// A Role (or `ClusterRole`) and its binding to `account` in the control
/// namespace.
async fn role(
    objects: &Objects,
    namespace: Option<&str>,
    name: &str,
    rules: &[Value],
    account: &str,
    control: &str,
) -> Result {
    let (kind, metadata) = namespace.map_or_else(
        || ("ClusterRole", json!({"name": name})),
        |namespace| ("Role", json!({"name": name, "namespace": namespace})),
    );
    objects
        .create(&json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": kind,
            "metadata": metadata, "rules": rules,
        }))
        .await?;
    objects
        .create(&json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": format!("{kind}Binding"),
            "metadata": metadata,
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": kind, "name": name},
            "subjects": [{"kind": "ServiceAccount", "name": account, "namespace": control}],
        }))
        .await?;
    Ok(())
}

async fn permissions(objects: &Objects, settings: &Settings<'_>, scope: &Domain) -> Result {
    let control = scope.control_namespace.as_str();
    let workload = scope.namespace.as_str();
    let mut updated = vec![format!("apsides-registry-{workload}")];
    let mut trust_inputs = Vec::new();
    for domain in settings.policies.keys() {
        updated.push(format!("apsides-activation-{domain}"));
        updated.push(format!("apsides-inventory-{domain}"));
        trust_inputs.push(format!("apsides-trust-{domain}"));
        trust_inputs.push(format!("apsides-deploy-identity-{domain}"));
    }
    let updated = updated.iter().map(String::as_str).collect::<Vec<_>>();
    let trust_inputs = trust_inputs.iter().map(String::as_str).collect::<Vec<_>>();
    let control_rules = [
        rule("", &["configmaps"], &["get", "create"], &[]),
        rule("", &["configmaps"], &["update"], &updated),
        rule("", &["secrets"], &["get", "create"], &[]),
        rule("", &["serviceaccounts"], &["get", "create", "delete"], &[]),
        rule("", &["pods"], &["get", "list"], &[]),
        rule("", &["pods/exec"], &["get"], &[]),
        rule("", &["services"], &["get", "create"], &[]),
        rule("apps", &["deployments"], &["get", "create", "update"], &[]),
        rule("apps", &["replicasets"], &["get"], &[]),
        rule(
            "rbac.authorization.k8s.io",
            &["roles", "rolebindings"],
            &["get", "create"],
            &[],
        ),
        rule(
            "rbac.authorization.k8s.io",
            &["rolebindings"],
            &["delete"],
            &[],
        ),
    ];
    role(
        objects,
        Some(control),
        DEPLOYER,
        &control_rules,
        DEPLOYER,
        control,
    )
    .await?;
    delegated_roles(objects, settings, scope).await?;
    let authority = format!("apsides-registry-authority.{control}.{workload}");
    role(
        objects,
        Some(PLATFORM_NAMESPACE),
        "registry-authority",
        &[rule("", &["configmaps"], &["get"], &[&authority])],
        DEPLOYER,
        control,
    )
    .await?;
    role(
        objects,
        Some(control),
        ISSUER,
        &[
            rule("", &["configmaps"], &["get"], &trust_inputs),
            rule("", &["serviceaccounts"], &["get"], &[DEPLOYER]),
            rule("", &["serviceaccounts/token"], &["create"], &[DEPLOYER]),
            rule("", &["secrets"], &["get"], &[BINDING_SECRET]),
        ],
        ISSUER,
        control,
    )
    .await?;
    role(
        objects,
        None,
        TARGET_ROLE,
        &[
            rule(
                "",
                &["namespaces"],
                &["get"],
                &["kube-system", control, workload],
            ),
            rule("", &["nodes"], &["get"], &[settings.node]),
        ],
        DEPLOYER,
        control,
    )
    .await
}

/// Every capability any release grants its runtime, per namespace (the
/// deployer can only create a Role holding permissions it has itself), a read
/// of every object a release manages, which inventory capture needs, and
/// deletion of every object a release deletes when an upgrade removes it.
async fn delegated_roles(objects: &Objects, settings: &Settings<'_>, scope: &Domain) -> Result {
    let control = scope.control_namespace.as_str();
    let workload = scope.namespace.as_str();
    let mut delegated: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut seen = Vec::new();
    for capability in settings
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.plan.capabilities)
    {
        if seen.contains(&capability) {
            continue;
        }
        seen.push(capability);
        ensure(
            capability.namespace == control || capability.namespace == workload,
            "candidate capability escapes the CNPG demo namespaces",
        )?;
        let resource = capability.subresource.as_ref().map_or_else(
            || capability.resource.clone(),
            |subresource| format!("{}/{subresource}", capability.resource),
        );
        let names = if capability.verb == "create" {
            Vec::new()
        } else {
            capability.names.iter().map(String::as_str).collect()
        };
        delegated
            .entry(capability.namespace.clone())
            .or_default()
            .push(rule(
                &capability.api_group,
                &[&resource],
                &[&capability.verb],
                &names,
            ));
    }
    let mut managed = BTreeSet::new();
    for resource in settings
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.plan.ir.resources)
    {
        let identity = deploy::resource_identity(resource)?;
        let verbs: &[&str] = if resource.lifecycle.deletion_policy == DeletionPolicy::Delete {
            &["get", "delete"]
        } else {
            &["get"]
        };
        managed.insert((identity, verbs));
    }
    for (identity, verbs) in managed {
        delegated
            .entry(identity.namespace.clone())
            .or_default()
            .push(rule(
                &identity.api_group,
                &[&identity.resource],
                verbs,
                &[&identity.name],
            ));
    }
    delegated.entry(workload.to_owned()).or_default().extend([
        rule(
            "rbac.authorization.k8s.io",
            &["roles", "rolebindings"],
            &["get", "create"],
            &[],
        ),
        rule(
            "rbac.authorization.k8s.io",
            &["rolebindings"],
            &["delete"],
            &[],
        ),
    ]);
    for (namespace, rules) in &delegated {
        role(
            objects,
            Some(namespace),
            "candidate-capabilities",
            rules,
            DEPLOYER,
            control,
        )
        .await?;
    }
    Ok(())
}

impl Platform {
    /// Mint a fresh issuer token and write a kubeconfig holding only it to
    /// `path`, readable by the owner alone. Returns the token so the canary
    /// scan can look for it.
    ///
    /// # Errors
    /// Fails when the token cannot be issued or the file cannot be written.
    pub async fn issuer_kubeconfig(&self, endpoint: &Endpoint, path: &Path) -> Result<String> {
        let accounts: Api<ServiceAccount> =
            Api::namespaced(self.objects.client().clone(), &self.control);
        let request = TokenRequest {
            spec: Some(TokenRequestSpec {
                expiration_seconds: Some(ISSUER_TOKEN_SECONDS),
                ..TokenRequestSpec::default()
            }),
            ..TokenRequest::default()
        };
        let issued = accounts
            .create_token_request(ISSUER, &PostParams::default(), &request)
            .await?;
        let token = issued
            .status
            .and_then(|status| status.token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error::Infrastructure("the API server issued no issuer token".into()))?;
        let kubeconfig = endpoint.token_kubeconfig("acceptance-issuer", &token);
        write_private(path, &serde_json::to_vec(&kubeconfig)?)?;
        Ok(token)
    }

    /// The namespace registry as the administrator reads it.
    ///
    /// # Errors
    /// Fails when the registry or its claim snapshot cannot be read.
    pub async fn registry(&self) -> Result<platform::Versioned<platform::NamespaceRegistry>> {
        Ok(self.admin.load_registry().await?)
    }

    /// Replace `domain`'s platform policy with `policy`, replacing the policy
    /// `ConfigMap` under its UID and resourceVersion.
    ///
    /// # Errors
    /// Fails when the domain was not bootstrapped, or the replacement is
    /// refused or is not exactly `policy`.
    pub async fn replace_policy(&mut self, domain: &str, policy: &PolicyState) -> Result<Value> {
        let data = String::from_utf8(compiler_core::canonical_bytes(policy)?)
            .map_err(|_| Error::Infrastructure("policy is not UTF-8".into()))?;
        let Some(current) = self.policies.get_mut(domain) else {
            return Err(Error::Input(format!("{domain} was not bootstrapped")));
        };
        let mut replacement = current.clone();
        replacement["data"]["state.json"] = json!(data);
        let written = self.objects.replace(&replacement).await?;
        let before = &current["metadata"];
        let after = &written["metadata"];
        ensure(
            after["uid"] == before["uid"]
                && after["resourceVersion"] != before["resourceVersion"]
                && after["deletionTimestamp"].is_null()
                && written["data"]["state.json"] == data,
            "successor policy replacement was not exact",
        )?;
        let change = json!({
            "uid": after["uid"],
            "beforeResourceVersion": before["resourceVersion"],
            "afterResourceVersion": after["resourceVersion"],
            "policyDigest": compiler_core::digest(policy)?,
        });
        *current = written;
        Ok(change)
    }
}

/// Write `bytes` to `path` with mode 0600, replacing any earlier file
/// atomically.
fn write_private(path: &Path, bytes: &[u8]) -> Result {
    use std::io::Write as _;
    let pending = path.with_extension("next");
    let _ = std::fs::remove_file(&pending);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(&pending)?.write_all(bytes)?;
    std::fs::rename(&pending, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_name_resources_only_when_given_names() {
        assert_eq!(
            rule("", &["configmaps"], &["get"], &[]),
            json!({"apiGroups": [""], "resources": ["configmaps"], "verbs": ["get"]})
        );
        assert_eq!(
            rule("apps", &["deployments"], &["update"], &["api"])["resourceNames"],
            json!(["api"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn kubeconfigs_are_private_and_replaced_whole() -> Result {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("issuer.kubeconfig");
        write_private(&path, b"first")?;
        write_private(&path, b"second")?;
        assert_eq!(std::fs::read(&path)?, b"second");
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }
}
