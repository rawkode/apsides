//! The disposable kind cluster and the clients that reach it.
//!
//! The cluster's admin kubeconfig is written into a private temporary
//! directory and every client is built from that file with an explicit
//! context. Nothing here reads `KUBECONFIG`, `~/.kube/config` or in-cluster
//! configuration.

use crate::{
    NODE_IMAGE, Result, infrastructure,
    process::{Invocation, tail},
};
use kube::{
    Client, Config,
    config::{KubeConfigOptions, Kubeconfig},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Every cluster the runner creates is named with this prefix, so a leaked
/// cluster is recognisable and can be removed with `kind delete cluster`.
pub const NAME_PREFIX: &str = "aps-test-";

const MAX_KUBECONFIG_BYTES: u64 = 1024 * 1024;

/// What happened to the cluster at the end of the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    NotCreated,
    Deleted,
    Kept,
    DeleteFailed,
}

pub struct Cluster {
    name: String,
    directory: tempfile::TempDir,
    keep: bool,
    finished: bool,
}

impl Cluster {
    /// Create a new cluster from the pinned node image.
    ///
    /// The node image must already be in the local Docker image store: the
    /// runner never lets kind pull it. An existing cluster with the chosen
    /// name is refused rather than reused.
    pub async fn create(keep: bool) -> Result<Self> {
        let name = unique_name()?;
        if Invocation::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}", NODE_IMAGE])
            .output()
            .await?
            .stdout
            .trim()
            .is_empty()
        {
            return infrastructure(format!(
                "the kind node image {NODE_IMAGE} is not in the local Docker image store; pull it before running the suite"
            ));
        }
        let existing = Invocation::new("kind")
            .args(["get", "clusters"])
            .run()
            .await?;
        if existing.lines().any(|line| line.trim() == name) {
            return infrastructure(format!("a kind cluster named {name} already exists"));
        }
        let directory = tempfile::Builder::new()
            .prefix("aps-test-cluster-")
            .tempdir()?;
        restrict(directory.path())?;
        let cluster = Self {
            name,
            directory,
            keep,
            finished: false,
        };
        let created = Invocation::new("kind")
            .args([
                "create",
                "cluster",
                "--name",
                &cluster.name,
                "--image",
                NODE_IMAGE,
            ])
            .arg("--kubeconfig")
            .arg(cluster.admin_kubeconfig())
            .args(["--wait", "120s"])
            .timeout(Duration::from_secs(420))
            .output()
            .await?;
        if !created.success {
            // Dropping `cluster` removes whatever kind managed to create.
            return infrastructure(format!(
                "kind could not create {}: {}",
                cluster.name,
                tail(&created.stderr, 2000)
            ));
        }
        Ok(cluster)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn context(&self) -> String {
        format!("kind-{}", self.name)
    }

    /// The container kind runs the single control-plane node in.
    pub fn node(&self) -> String {
        format!("{}-control-plane", self.name)
    }

    pub fn directory(&self) -> &Path {
        self.directory.path()
    }

    fn admin_kubeconfig(&self) -> PathBuf {
        self.directory.path().join("admin.kubeconfig")
    }

    /// The admin kubeconfig kind wrote, parsed.
    pub fn kubeconfig(&self) -> Result<Kubeconfig> {
        let bytes = bounded(&self.admin_kubeconfig())?;
        Ok(Kubeconfig::from_yaml(&String::from_utf8_lossy(&bytes))?)
    }

    /// A client with the fixture driver's (cluster admin) identity.
    pub async fn admin_client(&self) -> Result<Client> {
        client(self.kubeconfig()?, &self.context()).await
    }

    /// The API server URL and CA the admin kubeconfig names.
    pub fn endpoint(&self) -> Result<Endpoint> {
        let kubeconfig = self.kubeconfig()?;
        let context = self.context();
        let selected = kubeconfig
            .contexts
            .iter()
            .find(|item| item.name == context)
            .and_then(|item| item.context.as_ref())
            .map(|item| item.cluster.clone());
        let Some(cluster) = kubeconfig
            .clusters
            .iter()
            .find(|item| Some(&item.name) == selected.as_ref())
            .and_then(|item| item.cluster.as_ref())
        else {
            return infrastructure("the kind kubeconfig does not name its cluster");
        };
        let (Some(server), Some(authority)) = (
            cluster.server.clone(),
            cluster.certificate_authority_data.clone(),
        ) else {
            return infrastructure("the kind kubeconfig lacks a server or inline CA");
        };
        Ok(Endpoint { server, authority })
    }

    /// Delete the cluster unless the caller asked to keep it.
    pub async fn finish(mut self) -> Disposition {
        self.finished = true;
        if self.keep {
            return Disposition::Kept;
        }
        let deleted = Invocation::new("kind")
            .args(["delete", "cluster", "--name", &self.name])
            .arg("--kubeconfig")
            .arg(self.admin_kubeconfig())
            .timeout(Duration::from_secs(180))
            .output()
            .await;
        match deleted {
            Ok(output) if output.success => Disposition::Deleted,
            _ => Disposition::DeleteFailed,
        }
    }
}

impl Drop for Cluster {
    /// The fallback for an early return or a panic in a caller: the cluster is
    /// still removed, synchronously, before the temporary kubeconfig goes.
    fn drop(&mut self) {
        if self.finished || self.keep {
            return;
        }
        let _ = std::process::Command::new("kind")
            .args(["delete", "cluster", "--name", &self.name])
            .arg("--kubeconfig")
            .arg(self.admin_kubeconfig())
            .env_remove("KUBECONFIG")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

pub struct Endpoint {
    pub server: String,
    /// Base64 PEM, as it appears in a kubeconfig.
    pub authority: String,
}

impl Endpoint {
    /// A kubeconfig that authenticates with `token` alone.
    pub fn token_kubeconfig(&self, context: &str, token: &str) -> Value {
        token_kubeconfig(&self.server, &self.authority, context, token, None)
    }
}

/// A single-context kubeconfig whose only credential is a bearer token. The
/// fixture-mode runtime accepts nothing else.
pub fn token_kubeconfig(
    server: &str,
    authority: &str,
    context: &str,
    token: &str,
    tls_server_name: Option<&str>,
) -> Value {
    let mut cluster = json!({"server": server, "certificate-authority-data": authority});
    if let Some(name) = tls_server_name {
        cluster["tls-server-name"] = json!(name);
    }
    json!({
        "apiVersion": "v1",
        "kind": "Config",
        "current-context": context,
        "clusters": [{"name": "fixture", "cluster": cluster}],
        "users": [{"name": "runtime", "user": {"token": token}}],
        "contexts": [{"name": context, "context": {"cluster": "fixture", "user": "runtime"}}],
    })
}

/// Build a client from an explicit kubeconfig and context.
pub async fn client(kubeconfig: Kubeconfig, context: &str) -> Result<Client> {
    platform::install_rustls_ring_provider()
        .map_err(|error| crate::Error::Infrastructure(error.to_string()))?;
    let options = KubeConfigOptions {
        context: Some(context.to_owned()),
        cluster: None,
        user: None,
    };
    let config = Config::from_custom_kubeconfig(kubeconfig, &options).await?;
    Ok(Client::try_from(config)?)
}

/// A name no earlier run can have used: the prefix, this process and the
/// current time.
fn unique_name() -> Result<String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| crate::Error::Infrastructure(error.to_string()))?
        .as_secs();
    let name = format!("{NAME_PREFIX}{}-{seconds}", std::process::id());
    if !valid_name(&name) {
        return infrastructure(format!("generated cluster name {name} is invalid"));
    }
    Ok(name)
}

/// Kind names become Docker container names and kubeconfig context names, so
/// they are held to a lowercase DNS label.
fn valid_name(name: &str) -> bool {
    name.starts_with(NAME_PREFIX)
        && name.len() <= 50
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.ends_with('-')
}

fn bounded(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_KUBECONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_KUBECONFIG_BYTES {
        return infrastructure("the kind kubeconfig is oversized");
    }
    Ok(bytes)
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_prefixed_dns_labels() -> Result<()> {
        let name = unique_name()?;
        assert!(valid_name(&name), "{name}");
        assert!(!valid_name("kind"));
        assert!(!valid_name("aps-test-UPPER"));
        assert!(!valid_name("aps-test-"));
        assert!(!valid_name(&format!("{NAME_PREFIX}{}", "9".repeat(60))));
        Ok(())
    }

    #[test]
    fn token_kubeconfigs_carry_only_a_bearer_token() -> Result<()> {
        let value = token_kubeconfig(
            "https://127.0.0.1:8443",
            "Q0E=",
            "kind-aps-test-1",
            "secret-token",
            Some("kubernetes.default.svc"),
        );
        let parsed: Kubeconfig = serde_json::from_value(value)?;
        assert_eq!(parsed.current_context.as_deref(), Some("kind-aps-test-1"));
        let [user] = parsed.auth_infos.as_slice() else {
            return Err(crate::Error::Case("expected one user".into()));
        };
        let auth = user
            .auth_info
            .as_ref()
            .ok_or_else(|| crate::Error::Case("user has no credential".into()))?;
        assert!(auth.token.is_some());
        assert!(auth.client_certificate_data.is_none() && auth.exec.is_none());
        let cluster = parsed
            .clusters
            .first()
            .and_then(|cluster| cluster.cluster.as_ref())
            .ok_or_else(|| crate::Error::Case("expected one cluster".into()))?;
        assert_eq!(
            cluster.tls_server_name.as_deref(),
            Some("kubernetes.default.svc")
        );
        Ok(())
    }
}
