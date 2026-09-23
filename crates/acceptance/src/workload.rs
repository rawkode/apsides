//! The successor's `PostgREST` workload: its rollout identity, its HTTP
//! answers, and how it reacts to its observed `ConfigMap` and Secret.
//!
//! A rollout identity binds the Deployment, its revision annotation, the one
//! ready Pod, that Pod's `ReplicaSet` and the image the node runs, which must
//! be the signed successor's pinned image index or one of its children.

use crate::{
    Error, Result, digest, ensure, node,
    objects::{Objects, controlled_by, pod_ready, text},
    wait,
};
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const NAME: &str = "database-api";
const SELECTOR: &str = "app=database-api";
const PORT: u16 = 3000;
const MAX_HTTP_BYTES: usize = 64 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `PostgREST` may take to answer after a rollout.
const HTTP_STARTUP: Duration = Duration::from_secs(120);
/// How long a rollout may take.
const ROLLOUT: Duration = Duration::from_secs(300);
/// How long an equal-data update is watched for a rollout it must not cause.
pub const EQUAL_DATA_OBSERVATION: Duration = Duration::from_secs(30);
/// The key the Secret reaction adds and removes.
const PROBE_KEY: &str = "aps-demo-probe";

/// The workload in `namespace`, running on the kind node `node`.
pub struct Workload<'a> {
    pub objects: &'a Objects,
    pub namespace: &'a str,
    pub node: &'a str,
    /// The successor's pinned `PostgREST` image.
    pub image: &'a str,
}

/// One observed rollout of the workload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub deployment_uid: String,
    pub deployment_resource_version: String,
    pub revision: String,
    pub replica_set_uid: String,
    pub pod_uid: String,
    pub pod_name: String,
    pub container_image_id: String,
    pub selected_image_manifest_digest: String,
    pub restart_count: i64,
}

impl Identity {
    /// The identity without the Deployment's resourceVersion, which status
    /// updates change without any rollout.
    #[must_use]
    pub fn stable(&self) -> Self {
        Self {
            deployment_resource_version: String::new(),
            ..self.clone()
        }
    }
}

/// Why one HTTP observation failed. It never carries response content.
#[derive(Debug, PartialEq, Eq)]
pub enum Http {
    Transport,
    Status(u16),
    OversizedBody,
    InvalidJson,
    UnexpectedRows,
}

impl Workload<'_> {
    /// Wait for a single ready Pod (other than `previous_pod`) with a complete
    /// rollout identity.
    ///
    /// # Errors
    /// Fails when none appears within `timeout`, or the running image is not
    /// the signed successor's.
    pub async fn identity(
        &self,
        previous_pod: Option<&str>,
        timeout: Duration,
    ) -> Result<Identity> {
        wait::until(
            "PostgREST workload did not reach the required identity",
            timeout,
            async || self.observe(previous_pod).await,
        )
        .await
    }

    async fn observe(&self, previous_pod: Option<&str>) -> Result<Option<Identity>> {
        let Some(deployment) = self
            .objects
            .get("apps/v1", "Deployment", Some(self.namespace), NAME)
            .await?
        else {
            return Ok(None);
        };
        let pods = self
            .objects
            .list("v1", "Pod", Some(self.namespace), Some(SELECTOR))
            .await?;
        let ready = pods.iter().filter(|pod| pod_ready(pod)).collect::<Vec<_>>();
        let [pod] = ready.as_slice() else {
            return Ok(None);
        };
        if previous_pod.is_some_and(|previous| text(pod, "/metadata/uid") == previous) {
            return Ok(None);
        }
        let owners = pod["metadata"]["ownerReferences"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|owner| owner["kind"] == "ReplicaSet")
            .collect::<Vec<_>>();
        let statuses = pod["status"]["containerStatuses"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        let revision = text(
            &deployment,
            "/spec/template/metadata/annotations/apsides.dev~1revision",
        );
        let ([owner], [status]) = (owners.as_slice(), statuses) else {
            return Ok(None);
        };
        if revision.is_empty() {
            return Ok(None);
        }
        let replica = self
            .objects
            .require(
                "apps/v1",
                "ReplicaSet",
                Some(self.namespace),
                text(owner, "/name"),
            )
            .await?;
        let deployment_uid = text(&deployment, "/metadata/uid");
        let replica_owners = replica["metadata"]["ownerReferences"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|owner| owner["kind"] == "Deployment")
            .collect::<Vec<_>>();
        let image_id = text(status, "/imageID");
        if text(&replica, "/metadata/uid") != text(owner, "/uid")
            || !matches!(replica_owners.as_slice(), [only] if text(only, "/uid") == deployment_uid)
            || status["ready"] != true
            || status["restartCount"] != 0
            || image_id.is_empty()
        {
            return Ok(None);
        }
        let selected = self
            .selected_child(image_id, text(pod, "/spec/nodeName"))
            .await?;
        Ok(Some(Identity {
            deployment_uid: deployment_uid.to_owned(),
            deployment_resource_version: text(&deployment, "/metadata/resourceVersion").to_owned(),
            revision: revision.to_owned(),
            replica_set_uid: text(&replica, "/metadata/uid").to_owned(),
            pod_uid: text(pod, "/metadata/uid").to_owned(),
            pod_name: text(pod, "/metadata/name").to_owned(),
            container_image_id: image_id.to_owned(),
            selected_image_manifest_digest: selected,
            restart_count: 0,
        }))
    }

    /// The manifest the Pod runs, which must be the pinned index or one of
    /// its children as the node stores them.
    async fn selected_child(&self, image_id: &str, node_name: &str) -> Result<String> {
        ensure(
            node_name == self.node,
            "PostgREST Pod ran on an unexpected node",
        )?;
        let index = self.image.rsplit_once('@').map_or("", |(_, index)| index);
        let children = node::index_children(self.node, index).await?;
        let selected = image_id
            .rsplit_once('@')
            .map_or(image_id, |(_, selected)| selected);
        ensure(
            crate::valid_digest(selected),
            "PostgREST CRI image identity is invalid",
        )?;
        ensure(
            selected == index || children.iter().any(|child| child == selected),
            "PostgREST CRI image is not a child of the signed image index",
        )?;
        Ok(selected.to_owned())
    }

    /// UIDs of every `ReplicaSet` of the workload, sorted.
    ///
    /// # Errors
    /// Fails when they cannot be listed.
    pub async fn replica_sets(&self) -> Result<Vec<String>> {
        let mut uids = self
            .objects
            .list(
                "apps/v1",
                "ReplicaSet",
                Some(self.namespace),
                Some(SELECTOR),
            )
            .await?
            .iter()
            .map(|item| text(item, "/metadata/uid").to_owned())
            .collect::<Vec<_>>();
        uids.sort();
        Ok(uids)
    }

    /// Ask the Pod of `expected` for the demo messages until it answers with
    /// the first `count` rows, requiring the workload identity to stay
    /// `expected` throughout. Only transport failures and 503s are retried.
    ///
    /// # Errors
    /// Fails on any other HTTP outcome, an identity change, or the deadline.
    pub async fn messages(&self, count: usize, expected: &Identity) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + HTTP_STARTUP;
        loop {
            self.require_stable(expected).await?;
            match self.get_messages(&expected.pod_name, count).await {
                Ok(evidence) => {
                    self.require_stable(expected).await?;
                    ensure(
                        tokio::time::Instant::now() < deadline,
                        "PostgREST startup deadline exceeded",
                    )?;
                    return Ok(evidence);
                }
                Err(Http::Transport | Http::Status(503)) => {}
                Err(failure) => {
                    return Err(Error::Check(format!(
                        "PostgREST HTTP observation failed: {failure:?}"
                    )));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Check("PostgREST startup deadline exceeded".into()));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn require_stable(&self, expected: &Identity) -> Result {
        let current = self.identity(None, Duration::from_secs(1)).await?;
        ensure(
            current.stable() == expected.stable(),
            "workload identity changed during HTTP startup",
        )
    }

    async fn get_messages(&self, pod: &str, count: usize) -> std::result::Result<Value, Http> {
        let pods: Api<Pod> = Api::namespaced(self.objects.client().clone(), self.namespace);
        let (status, body) = tokio::time::timeout(HTTP_TIMEOUT, request(&pods, pod))
            .await
            .map_err(|_| Http::Transport)??;
        if status != 200 {
            return Err(Http::Status(status));
        }
        let rows: Value = serde_json::from_slice(&body).map_err(|_| Http::InvalidJson)?;
        let expected = [
            json!({"id": 1, "message": "Hello from CloudNativePG"}),
            json!({"id": 2, "message": "Configured by Apsides"}),
        ];
        if rows.as_array().map(Vec::as_slice) != expected.get(..count) {
            return Err(Http::UnexpectedRows);
        }
        Ok(json!({"status": status, "rowCount": count, "bodyDigest": digest(&body)}))
    }

    /// Change `PGRST_DB_MAX_ROWS` from 100 to 1 and require a rollout to a
    /// new revision whose API returns one row.
    ///
    /// # Errors
    /// Fails when the change does not roll the workload out as required.
    pub async fn configmap_rollout(&self, before: &Identity) -> Result<(Identity, Value)> {
        let mut settings = self
            .objects
            .require("v1", "ConfigMap", Some(self.namespace), "api-settings")
            .await?;
        ensure(
            settings["data"] == json!({"PGRST_DB_MAX_ROWS": "100"}),
            "api-settings does not hold the initial row limit",
        )?;
        settings["data"]["PGRST_DB_MAX_ROWS"] = json!("1");
        self.objects.replace(&settings).await?;
        let after = self.identity(Some(&before.pod_uid), ROLLOUT).await?;
        ensure(
            after.deployment_uid == before.deployment_uid
                && after.revision != before.revision
                && after.replica_set_uid != before.replica_set_uid,
            "ConfigMap change did not produce the expected PostgREST rollout",
        )?;
        let http = self.messages(1, &after).await?;
        Ok((after, http))
    }

    /// Rewrite `api-settings` with equal data and require no rollout for
    /// [`EQUAL_DATA_OBSERVATION`].
    ///
    /// # Errors
    /// Fails when the write changes data or the workload changes.
    pub async fn equal_data_no_rollout(&self, previous: &Identity) -> Result<Value> {
        let original = self
            .objects
            .require("v1", "ConfigMap", Some(self.namespace), "api-settings")
            .await?;
        let mut update = original.clone();
        let probe = hex(&crate::random_bytes(16)?);
        if !update["metadata"]["annotations"].is_object() {
            update["metadata"]["annotations"] = json!({});
        }
        update["metadata"]["annotations"][PROBE_KEY] = json!(probe);
        let sets = self.replica_sets().await?;
        let written = self.objects.replace(&update).await?;
        ensure(
            written["metadata"]["uid"] == original["metadata"]["uid"]
                && written["metadata"]["resourceVersion"]
                    != original["metadata"]["resourceVersion"]
                && written["data"] == original["data"]
                && written["binaryData"].is_null()
                && original["binaryData"].is_null(),
            "equal-data ConfigMap update was not an unchanged-data write",
        )?;
        let started = tokio::time::Instant::now();
        loop {
            let current = self.identity(None, Duration::from_secs(1)).await?;
            ensure(
                current.stable() == previous.stable() && self.replica_sets().await? == sets,
                "equal-data ConfigMap update changed workload identity",
            )?;
            if started.elapsed() >= EQUAL_DATA_OBSERVATION {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Ok(json!({"observationSeconds": EQUAL_DATA_OBSERVATION.as_secs()}))
    }

    /// Add `probe` (base64, as Secret data) to `database-app` under a
    /// synthetic key, require a rollout to a new HMAC revision under the same
    /// rollout key, then remove it and require the original revision back.
    /// The Secret is restored even when a check fails.
    ///
    /// # Errors
    /// Fails when either rollout differs from the required one or the Secret
    /// cannot be restored exactly.
    pub async fn secret_reaction(&self, previous: &Identity, probe: &str) -> Result<Value> {
        ensure(
            hmac_revision(&previous.revision),
            "Secret reaction requires an opaque HMAC revision",
        )?;
        let configs = self.config_versions().await?;
        let original = self.secret().await?;
        ensure(
            original["data"][PROBE_KEY].is_null() && !text(&original, "/data/uri").is_empty(),
            "Secret reaction fixture precondition failed",
        )?;
        let changed = self
            .secret_change(previous, &original, &configs, probe)
            .await;
        let restored = self.restore_secret(&original, probe).await;
        let changed = changed?;
        restored?;
        let restored = self.identity(Some(&changed.pod_uid), ROLLOUT).await?;
        ensure(
            restored.deployment_uid == previous.deployment_uid
                && restored.container_image_id == previous.container_image_id
                && restored.selected_image_manifest_digest
                    == previous.selected_image_manifest_digest
                && restored.revision == previous.revision
                && restored.replica_set_uid != changed.replica_set_uid
                && self.config_versions().await? == configs,
            "Secret restoration did not restore the original workload revision",
        )?;
        self.messages(1, &restored).await?;
        Ok(json!({"revisionChanged": true, "podTurnover": true, "restored": true}))
    }

    async fn secret_change(
        &self,
        previous: &Identity,
        original: &Value,
        configs: &Value,
        probe: &str,
    ) -> Result<Identity> {
        let mut update = original.clone();
        update["data"][PROBE_KEY] = json!(probe);
        self.objects.replace(&update).await?;
        let changed = self.identity(Some(&previous.pod_uid), ROLLOUT).await?;
        let key_id = |revision: &str| revision.split(':').nth(1).map(str::to_owned);
        ensure(
            hmac_revision(&changed.revision)
                && key_id(&changed.revision) == key_id(&previous.revision)
                && changed.deployment_uid == previous.deployment_uid
                && changed.container_image_id == previous.container_image_id
                && changed.selected_image_manifest_digest
                    == previous.selected_image_manifest_digest
                && changed.revision != previous.revision
                && changed.pod_uid != previous.pod_uid
                && changed.replica_set_uid != previous.replica_set_uid,
            "Secret change did not produce the expected workload rollout",
        )?;
        let observed = self.secret().await?;
        let mut expected = original["data"].clone();
        expected[PROBE_KEY] = json!(probe);
        ensure(
            observed["metadata"]["uid"] == original["metadata"]["uid"]
                && observed["data"] == expected
                && &self.config_versions().await? == configs,
            "Secret reaction inputs changed outside the synthetic key",
        )?;
        self.messages(1, &changed).await?;
        Ok(changed)
    }

    async fn restore_secret(&self, original: &Value, probe: &str) -> Result {
        let mut current = self.secret().await?;
        ensure(
            current["metadata"]["uid"] == original["metadata"]["uid"],
            "Secret reaction identity changed",
        )?;
        if current["data"][PROBE_KEY] == probe {
            if let Some(data) = current["data"].as_object_mut() {
                data.remove(PROBE_KEY);
            }
            self.objects.replace(&current).await?;
        }
        let restored = self.secret().await?;
        ensure(
            restored["metadata"]["uid"] == original["metadata"]["uid"]
                && restored["data"] == original["data"],
            "Secret reaction fixture restoration failed",
        )
    }

    async fn secret(&self) -> Result<Value> {
        self.objects
            .require("v1", "Secret", Some(self.namespace), "database-app")
            .await
    }

    async fn config_versions(&self) -> Result<Value> {
        let mut result = serde_json::Map::new();
        for name in ["api-config", "api-settings"] {
            let config = self
                .objects
                .require("v1", "ConfigMap", Some(self.namespace), name)
                .await?;
            result.insert(
                name.to_owned(),
                json!({"uid": config["metadata"]["uid"], "resourceVersion": config["metadata"]["resourceVersion"]}),
            );
        }
        Ok(Value::Object(result))
    }
}

/// `GET /messages` on the Pod's port 3000 through a port-forward. HTTP/1.0
/// keeps the response unchunked.
async fn request(pods: &Api<Pod>, pod: &str) -> std::result::Result<(u16, Vec<u8>), Http> {
    let mut forwarder = pods
        .portforward(pod, &[PORT])
        .await
        .map_err(|_| Http::Transport)?;
    let mut stream = forwarder.take_stream(PORT).ok_or(Http::Transport)?;
    stream
        .write_all(
            b"GET /messages?select=id,message&order=id.asc HTTP/1.0\r\nHost: database-api\r\nAccept: application/json\r\n\r\n",
        )
        .await
        .map_err(|_| Http::Transport)?;
    let mut buffer = Vec::new();
    let header_end = loop {
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            break end;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(Http::OversizedBody);
        }
        let mut chunk = [0; 8192];
        let read = stream.read(&mut chunk).await.map_err(|_| Http::Transport)?;
        if read == 0 {
            return Err(Http::Transport);
        }
        buffer.extend_from_slice(chunk.get(..read).unwrap_or_default());
    };
    let head = String::from_utf8_lossy(buffer.get(..header_end).unwrap_or_default()).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(Http::Transport)?;
    let length = head.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    let mut body = buffer.split_off(header_end + 4);
    loop {
        if body.len() > MAX_HTTP_BYTES {
            return Err(Http::OversizedBody);
        }
        if length.is_some_and(|length| body.len() >= length) {
            break;
        }
        let mut chunk = [0; 8192];
        let read = stream.read(&mut chunk).await.map_err(|_| Http::Transport)?;
        if read == 0 {
            if length.is_some() {
                return Err(Http::Transport);
            }
            break;
        }
        body.extend_from_slice(chunk.get(..read).unwrap_or_default());
    }
    if let Some(length) = length {
        body.truncate(length);
    }
    drop(stream);
    forwarder.abort();
    Ok((status, body))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether `revision` is `hmac-sha256:<key id>:<64 hex>`.
#[must_use]
pub fn hmac_revision(revision: &str) -> bool {
    let mut parts = revision.splitn(3, ':');
    let (Some("hmac-sha256"), Some(key), Some(mac)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        && mac.len() == 64
        && mac
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The identities of one generation's writers, which a rejected deployment
/// must leave unchanged.
///
/// The runtime Deployment `runtime` and proxy Deployment `proxy` in
/// `namespace` must each be active with exactly one owned `ReplicaSet` and
/// one ready Pod.
///
/// # Errors
/// Fails when a writer is absent, inactive or ambiguous.
pub async fn writer_identity(
    objects: &Objects,
    namespace: &str,
    runtime: &str,
    proxy: &str,
) -> Result<Value> {
    let deployments = objects
        .list("apps/v1", "Deployment", Some(namespace), None)
        .await?;
    let replicas = objects
        .list("apps/v1", "ReplicaSet", Some(namespace), None)
        .await?;
    let pods = objects.list("v1", "Pod", Some(namespace), None).await?;
    let mut result = Vec::new();
    for name in [runtime, proxy] {
        let matching = deployments
            .iter()
            .filter(|item| text(item, "/metadata/name") == name)
            .collect::<Vec<_>>();
        let [deployment] = matching.as_slice() else {
            return Err(Error::Check(
                "initial writer Deployment is absent or ambiguous".into(),
            ));
        };
        let uid = text(deployment, "/metadata/uid");
        ensure(
            !uid.is_empty()
                && deployment["metadata"]["deletionTimestamp"].is_null()
                && deployment["spec"]["replicas"] == 1
                && deployment["status"]["readyReplicas"] == 1,
            "initial writer Deployment is not active",
        )?;
        let mut owned = replicas
            .iter()
            .filter(|item| controlled_by(item, "Deployment", uid))
            .map(|item| text(item, "/metadata/uid").to_owned())
            .collect::<Vec<_>>();
        owned.sort();
        ensure(
            owned.len() == 1 && owned.iter().all(|item| !item.is_empty()),
            "initial writer ReplicaSet identity is ambiguous",
        )?;
        let mut writers = Vec::new();
        for pod in &pods {
            let Some(replica) = owned
                .iter()
                .find(|replica| controlled_by(pod, "ReplicaSet", replica))
            else {
                continue;
            };
            let containers = pod["status"]["containerStatuses"]
                .as_array()
                .map_or(&[][..], Vec::as_slice);
            ensure(
                pod_ready(pod)
                    && !containers.is_empty()
                    && containers.iter().all(|item| {
                        item["ready"] == true && !text(item, "/containerID").is_empty()
                    }),
                "initial writer Pod is not active",
            )?;
            let mut recorded = containers
                .iter()
                .map(|item| {
                    json!({"name": item["name"], "containerId": item["containerID"], "restartCount": item["restartCount"]})
                })
                .collect::<Vec<_>>();
            recorded.sort_by_key(|item| item["name"].as_str().unwrap_or_default().to_owned());
            writers.push(json!({
                "name": pod["metadata"]["name"],
                "uid": pod["metadata"]["uid"],
                "nodeName": pod["spec"]["nodeName"],
                "replicaSetUid": replica,
                "containers": recorded,
            }));
        }
        ensure(writers.len() == 1, "initial writer Pod set is not singular")?;
        result.push(json!({"name": name, "uid": uid, "replicaSetUids": owned, "pods": writers}));
    }
    Ok(Value::Array(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_revisions_are_recognised_exactly() {
        let mac = "a".repeat(64);
        assert!(hmac_revision(&format!("hmac-sha256:cnpg-demo:{mac}")));
        assert!(!hmac_revision(&format!("sha256:cnpg-demo:{mac}")));
        assert!(!hmac_revision(&format!("hmac-sha256::{mac}")));
        assert!(!hmac_revision("hmac-sha256:cnpg-demo:ABC"));
        assert!(!hmac_revision(&format!("hmac-sha256:key id:{mac}")));
    }

    #[test]
    fn stable_identities_ignore_only_the_resource_version() {
        let identity = Identity {
            deployment_uid: "d".into(),
            deployment_resource_version: "1".into(),
            revision: "r".into(),
            replica_set_uid: "rs".into(),
            pod_uid: "p".into(),
            pod_name: "pod".into(),
            container_image_id: "i".into(),
            selected_image_manifest_digest: "s".into(),
            restart_count: 0,
        };
        let mut status = identity.clone();
        status.deployment_resource_version = "2".into();
        assert_eq!(status.stable(), identity.stable());
        let mut turned = identity.clone();
        turned.pod_uid = "q".into();
        assert_ne!(turned.stable(), identity.stable());
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }
}
