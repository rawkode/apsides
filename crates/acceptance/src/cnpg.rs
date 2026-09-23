//! The `CloudNativePG` operator and the database the example programs declare.
//!
//! The operator manifest is the upstream v1.30.0 release asset, accepted only
//! when its bytes hash to [`OPERATOR_MANIFEST_SHA256`]. Upstream names the
//! operator image by tag with `imagePullPolicy: Always`; the suite pins that
//! image to [`OPERATOR_IMAGE`] (in the container and in the
//! `OPERATOR_IMAGE_NAME` the operator gives database Pods) and pulls it only
//! if absent, so the node runs exactly the pulled digest.

use crate::{
    Error, Result, ensure, infrastructure,
    objects::{Objects, deleting, text},
    wait,
};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, Client,
    api::{AttachParams, DynamicObject, GroupVersionKind, Patch, PatchParams},
    discovery::{self, Scope},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::io::AsyncReadExt as _;

/// Where `acceptance-prepare` fetches the operator manifest from.
pub const OPERATOR_MANIFEST_URL: &str =
    "https://github.com/cloudnative-pg/cloudnative-pg/releases/download/v1.30.0/cnpg-1.30.0.yaml";
/// SHA-256 of the upstream `cnpg-1.30.0.yaml`.
pub const OPERATOR_MANIFEST_SHA256: &str =
    "sha256:f8bede43fe4ee0d478c2355b204a36876b2ae4faac60f2a9452280b293da3b88";
/// The tag the upstream manifest names the operator image by.
pub const OPERATOR_TAG: &str = "ghcr.io/cloudnative-pg/cloudnative-pg:1.30.0";
/// The operator image index that tag named when the suite was pinned.
pub const OPERATOR_IMAGE: &str = "ghcr.io/cloudnative-pg/cloudnative-pg:1.30.0@sha256:a2701eb97cdd2a34b1fdb2cb51987f544b706e40bec72ae7146cd8580efefebb";

const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
const FIELD_MANAGER: &str = "aps-acceptance-cnpg";
const OPERATOR_NAMESPACE: &str = "cnpg-system";
const OPERATOR_DEPLOYMENT: &str = "cnpg-controller-manager";

/// The operator manifest's documents with the operator image pinned.
///
/// # Errors
/// Fails when the bytes differ from the pinned release asset, or the manifest
/// does not name the operator image exactly twice in one Deployment.
pub fn operator_documents(bytes: &[u8]) -> Result<Vec<Value>> {
    ensure(
        !bytes.is_empty() && bytes.len() <= MAX_MANIFEST_BYTES,
        "CNPG operator manifest is empty or oversized",
    )?;
    ensure(
        crate::digest(bytes) == OPERATOR_MANIFEST_SHA256,
        "pinned CNPG operator manifest failed digest validation",
    )?;
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Input("CNPG operator manifest is not UTF-8".into()))?;
    ensure(
        text.matches(OPERATOR_TAG).count() == 2,
        "CNPG operator manifest does not name one operator image",
    )?;
    let options = serde_saphyr::options! {
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 0,
            max_alias_expansions_per_anchor: 0,
        },
    };
    let mut documents: Vec<Value> = serde_saphyr::from_multiple_with_options(text, options)
        .map_err(|error| {
            Error::Input(format!("CNPG operator manifest is invalid YAML: {error}"))
        })?;
    documents.retain(|document| !document.is_null());
    let mut pinned = 0;
    for document in &mut documents {
        if document["kind"] != "Deployment" {
            continue;
        }
        ensure(
            text_of(document, "/metadata/name") == OPERATOR_DEPLOYMENT,
            "CNPG operator manifest has an unexpected Deployment",
        )?;
        let Some([container]) = document
            .pointer_mut("/spec/template/spec/containers")
            .and_then(Value::as_array_mut)
            .map(Vec::as_mut_slice)
        else {
            return Err(Error::Check(
                "CNPG operator Deployment must run one container".into(),
            ));
        };
        ensure(
            container["image"] == OPERATOR_TAG,
            "CNPG operator image is not the pinned tag",
        )?;
        container["image"] = json!(OPERATOR_IMAGE);
        container["imagePullPolicy"] = json!("IfNotPresent");
        pinned += 1;
        for variable in container["env"].as_array_mut().into_iter().flatten() {
            if variable["name"] == "OPERATOR_IMAGE_NAME" && variable["value"] == OPERATOR_TAG {
                variable["value"] = json!(OPERATOR_IMAGE);
                pinned += 1;
            }
        }
    }
    ensure(
        pinned == 2,
        "CNPG operator image was not pinned in both places",
    )?;
    Ok(documents)
}

fn text_of<'a>(value: &'a Value, path: &str) -> &'a str {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Apply the operator, wait for its CRDs and Deployment, and return the
/// Deployment's identity.
///
/// # Errors
/// Fails when an object is refused, the operator does not become available,
/// or it runs another image than [`OPERATOR_IMAGE`].
pub async fn install_operator(objects: &Objects, documents: &[Value]) -> Result<Value> {
    let client = objects.client();
    let mut resolved = BTreeMap::new();
    for document in documents {
        let api_version = text_of(document, "/apiVersion");
        let kind = text_of(document, "/kind");
        let name = text_of(document, "/metadata/name");
        let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
        let key = (api_version.to_owned(), kind.to_owned());
        if !resolved.contains_key(&key) {
            let pinned =
                discovery::pinned_kind(client, &GroupVersionKind::gvk(group, version, kind))
                    .await?;
            resolved.insert(key.clone(), pinned);
        }
        let Some((resource, capabilities)) = resolved.get(&key) else {
            return infrastructure(format!("no API resource for {api_version} {kind}"));
        };
        let api: Api<DynamicObject> = match capabilities.scope {
            Scope::Namespaced => Api::namespaced_with(
                client.clone(),
                text_of(document, "/metadata/namespace"),
                resource,
            ),
            Scope::Cluster => Api::all_with(client.clone(), resource),
        };
        api.patch(
            name,
            &PatchParams::apply(FIELD_MANAGER),
            &Patch::Apply(document),
        )
        .await?;
    }
    wait::until(
        "the CNPG Cluster CRD to be established",
        Duration::from_secs(120),
        async || {
            let crd = objects
                .get(
                    "apiextensions.k8s.io/v1",
                    "CustomResourceDefinition",
                    None,
                    "clusters.postgresql.cnpg.io",
                )
                .await?;
            Ok(crd.filter(|crd| condition(crd, "Established")).map(|_| ()))
        },
    )
    .await?;
    let deployment = wait::until(
        "the CNPG operator Deployment to become available",
        Duration::from_secs(300),
        async || {
            let deployment = objects
                .require(
                    "apps/v1",
                    "Deployment",
                    Some(OPERATOR_NAMESPACE),
                    OPERATOR_DEPLOYMENT,
                )
                .await?;
            Ok(rolled_out(&deployment).then_some(deployment))
        },
    )
    .await?;
    let containers = deployment["spec"]["template"]["spec"]["containers"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    ensure(
        matches!(containers, [container] if container["image"] == OPERATOR_IMAGE),
        "CNPG operator deployment has an unexpected image contract",
    )?;
    Ok(json!({
        "manifestDigest": OPERATOR_MANIFEST_SHA256,
        "deploymentUid": deployment["metadata"]["uid"],
        "image": OPERATOR_IMAGE,
    }))
}

/// Whether `object` has condition `kind` with status `True`.
#[must_use]
pub fn condition(object: &Value, kind: &str) -> bool {
    object["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item["type"] == kind && item["status"] == "True")
}

/// Whether a Deployment's current generation is fully rolled out.
#[must_use]
pub fn rolled_out(deployment: &Value) -> bool {
    let replicas = deployment["spec"]["replicas"].as_i64().unwrap_or(1);
    let status = &deployment["status"];
    status["observedGeneration"].as_i64() >= deployment["metadata"]["generation"].as_i64()
        && status["updatedReplicas"].as_i64().unwrap_or(0) == replicas
        && status["availableReplicas"].as_i64().unwrap_or(0) == replicas
        && status["replicas"].as_i64().unwrap_or(0) == replicas
}

/// Wait for the `database` Cluster to be Ready and its application Secret to
/// carry a connection URI.
///
/// # Errors
/// Fails when either does not happen in time.
pub async fn wait_ready(objects: &Objects, namespace: &str) -> Result<Value> {
    wait::until(
        "the CNPG Cluster to appear",
        Duration::from_secs(180),
        async || {
            objects
                .get(
                    "postgresql.cnpg.io/v1",
                    "Cluster",
                    Some(namespace),
                    "database",
                )
                .await
        },
    )
    .await?;
    let cluster = wait::until(
        "the CNPG Cluster to become Ready",
        Duration::from_secs(300),
        async || {
            let cluster = objects
                .require(
                    "postgresql.cnpg.io/v1",
                    "Cluster",
                    Some(namespace),
                    "database",
                )
                .await?;
            Ok(condition(&cluster, "Ready").then_some(cluster))
        },
    )
    .await?;
    let secret = wait::until(
        "the CNPG application Secret to become available",
        Duration::from_secs(180),
        async || {
            let secret = objects
                .get("v1", "Secret", Some(namespace), "database-app")
                .await?;
            Ok(secret.filter(|secret| {
                !text(secret, "/metadata/uid").is_empty() && !text(secret, "/data/uri").is_empty()
            }))
        },
    )
    .await?;
    Ok(json!({
        "clusterUid": cluster["metadata"]["uid"],
        "secretUid": secret["metadata"]["uid"],
        "uriKeyPresent": true,
    }))
}

/// The database's primary Pod and the identities an upgrade must preserve.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseIdentity {
    pub cluster_uid: String,
    /// `(name, uid)` of every bound volume claim, sorted by name.
    pub volumes: Vec<(String, String)>,
}

/// The current primary and the database's identity.
///
/// # Errors
/// Fails when the Cluster, its primary or its bound volumes are unavailable
/// or ambiguous.
pub async fn database_identity(
    objects: &Objects,
    namespace: &str,
) -> Result<(String, DatabaseIdentity)> {
    let cluster = objects
        .require(
            "postgresql.cnpg.io/v1",
            "Cluster",
            Some(namespace),
            "database",
        )
        .await?;
    let primary = text(&cluster, "/status/currentPrimary").to_owned();
    let volumes = objects
        .list(
            "v1",
            "PersistentVolumeClaim",
            Some(namespace),
            Some("cnpg.io/cluster=database"),
        )
        .await?;
    ensure(
        !primary.is_empty()
            && !text(&cluster, "/metadata/uid").is_empty()
            && !deleting(&cluster)
            && !volumes.is_empty()
            && volumes.iter().all(|volume| {
                text(volume, "/status/phase") == "Bound"
                    && !deleting(volume)
                    && !text(volume, "/metadata/uid").is_empty()
                    && !text(volume, "/metadata/name").is_empty()
            }),
        "demo database or bound volumes are unavailable",
    )?;
    let mut identities = volumes
        .iter()
        .map(|volume| {
            (
                text(volume, "/metadata/name").to_owned(),
                text(volume, "/metadata/uid").to_owned(),
            )
        })
        .collect::<Vec<_>>();
    identities.sort();
    let mut names = identities.iter().map(|(name, _)| name).collect::<Vec<_>>();
    names.dedup();
    ensure(
        names.len() == identities.len(),
        "demo database volume identities are ambiguous",
    )?;
    Ok((
        primary,
        DatabaseIdentity {
            cluster_uid: text(&cluster, "/metadata/uid").to_owned(),
            volumes: identities,
        },
    ))
}

/// The row the scenario writes before the upgrade and reads after it.
const PERSISTENCE_ROW: &str = "1|survives-upgrade";

/// Write the persistence row (when `previous` is `None`) or read it back
/// (requiring the database identity to equal `previous`), and return the
/// identity observed around the query.
///
/// # Errors
/// Fails when the identity changed, the query fails, or the row differs.
pub async fn persistence(
    objects: &Objects,
    namespace: &str,
    previous: Option<&DatabaseIdentity>,
) -> Result<DatabaseIdentity> {
    let (primary, identity) = database_identity(objects, namespace).await?;
    if let Some(previous) = previous {
        ensure(
            &identity == previous,
            "upgrade replaced the demo database or its volumes",
        )?;
    }
    let select = "SELECT id, value FROM aps_demo_persistence ORDER BY id;";
    let sql = if previous.is_some() {
        select.to_owned()
    } else {
        format!(
            "CREATE TABLE aps_demo_persistence (id integer PRIMARY KEY, value text NOT NULL); INSERT INTO aps_demo_persistence VALUES (1, 'survives-upgrade'); {select}"
        )
    };
    let observed = exec(
        objects.client(),
        namespace,
        &primary,
        "postgres",
        &[
            "psql",
            "-X",
            "-q",
            "-A",
            "-t",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            "/controller/run",
            "-U",
            "postgres",
            "-d",
            "app",
            "-c",
            &sql,
        ],
    )
    .await?;
    ensure(
        observed.trim() == PERSISTENCE_ROW,
        "demo database persistence row differs",
    )?;
    let (_, after) = database_identity(objects, namespace).await?;
    ensure(
        after == identity,
        "demo database identity changed during persistence check",
    )?;
    Ok(identity)
}

/// Run `command` in a container and return its stdout, failing unless it
/// exits zero.
pub(crate) async fn exec(
    client: &Client,
    namespace: &str,
    pod: &str,
    container: &str,
    command: &[&str],
) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let params = AttachParams::default()
        .container(container)
        .stdout(true)
        .stderr(true);
    let mut attached = pods.exec(pod, command.iter().copied(), &params).await?;
    let status = attached.take_status();
    let mut stdout = String::new();
    let mut stderr = String::new();
    let (Some(mut out), Some(mut err)) = (attached.stdout(), attached.stderr()) else {
        return infrastructure("exec streams are unavailable");
    };
    let mut out = (&mut out).take(1024 * 1024);
    let mut err = (&mut err).take(1024 * 1024);
    let (read_out, read_err) = tokio::join!(
        out.read_to_string(&mut stdout),
        err.read_to_string(&mut stderr),
    );
    read_out?;
    read_err?;
    let status = match status {
        Some(status) => status.await,
        None => None,
    };
    attached
        .join()
        .await
        .map_err(|error| Error::Infrastructure(error.to_string()))?;
    ensure(
        status.as_ref().and_then(|status| status.status.as_deref()) == Some("Success"),
        format!(
            "`{}` in {pod}/{container} failed: {}",
            command.first().copied().unwrap_or_default(),
            stderr.trim()
        ),
    )?;
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pinned_upstream_manifest_is_accepted() {
        assert!(operator_documents(b"apiVersion: v1\nkind: Namespace\n").is_err());
        assert!(operator_documents(b"").is_err());
    }

    #[test]
    fn rollout_requires_every_replica_updated_and_available() {
        let mut deployment = json!({
            "metadata": {"generation": 2},
            "spec": {"replicas": 1},
            "status": {"observedGeneration": 2, "replicas": 1, "updatedReplicas": 1, "availableReplicas": 1},
        });
        assert!(rolled_out(&deployment));
        deployment["status"]["observedGeneration"] = json!(1);
        assert!(!rolled_out(&deployment));
        deployment["status"]["observedGeneration"] = json!(2);
        deployment["status"]["replicas"] = json!(2);
        assert!(!rolled_out(&deployment));
    }

    #[test]
    fn conditions_must_be_true() {
        let object = json!({"status": {"conditions": [{"type": "Ready", "status": "False"}]}});
        assert!(!condition(&object, "Ready"));
        assert!(condition(
            &json!({"status": {"conditions": [{"type": "Ready", "status": "True"}]}}),
            "Ready"
        ));
    }
}
