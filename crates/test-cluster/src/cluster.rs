//! The disposable kind cluster and the clients that reach it.
//!
//! The cluster's admin kubeconfig is written into a private temporary
//! directory and every client is built from that file with an explicit
//! context. Nothing here reads `KUBECONFIG`, `~/.kube/config` or in-cluster
//! configuration.

use crate::{
    NODE_IMAGE, Result,
    engine::{self, EngineCommands as _},
    infrastructure,
    process::tail,
};
use kube::{
    Client, Config,
    config::{KubeConfigOptions, Kubeconfig},
};
use serde_json::{Value, json};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Every cluster the runner creates is named with this prefix, so a leaked
/// cluster is recognisable and can be removed with `kind delete cluster`.
///
/// Other suites choose their own prefix with [`Cluster::with_prefix`].
pub const NAME_PREFIX: &str = "aps-test-";

const MAX_KUBECONFIG_BYTES: u64 = 1024 * 1024;

/// The cluster's Pod and Service ranges, set explicitly so the node egress
/// rules can allow exactly them.
pub const POD_SUBNET: &str = "10.244.0.0/16";
pub const SERVICE_SUBNET: &str = "10.96.0.0/16";

/// How long the synchronous fallback deletion may take before it is abandoned.
const DROP_DELETE_SECONDS: u64 = 120;

/// How many times `finish` runs `kind delete cluster`, and the pause between.
const DELETE_ATTEMPTS: u32 = 3;
const DELETE_RETRY_PAUSE: Duration = Duration::from_secs(5);

/// What happened to the cluster at the end of the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    NotCreated,
    Deleted,
    Kept,
    DeleteFailed,
}

/// The local tools the run depends on, recorded in the report.
pub struct Toolchain {
    pub kind_version: String,
    pub container_engine: &'static str,
    pub container_engine_version: String,
    pub node_image_id: String,
}

/// Where the running kernel lists its active Linux Security Modules
/// (comma-separated, e.g. `lockdown,capability,landlock,...`).
const LSM_PATH: &str = "/sys/kernel/security/lsm";

/// Kind nodes share the host kernel, so Landlock support -- required by
/// every generated controller's Phase 5c startup lockdown -- is a property
/// of *this* host, not of the node image. GitHub's `ubuntu-24.04` runners
/// have it; a self-hosted or older runner might not, which would otherwise
/// surface much later and far more confusingly, as every acceptance
/// scenario's controller Pod failing to start (or, if a scenario's fixture
/// mode ever set `allowNoLandlock`, silently running unlocked instead).
///
/// # Errors
/// Returns a clear infrastructure error naming the missing prerequisite,
/// not the kernel's raw error text, if [`LSM_PATH`] cannot be read or does
/// not list `landlock`.
fn ensure_landlock_available() -> Result<()> {
    let lsm = std::fs::read_to_string(LSM_PATH).map_err(|error| {
        crate::Error::Infrastructure(format!(
            "failed to read {LSM_PATH} to confirm the host kernel has Landlock (needed by \
             every generated controller's filesystem lockdown, and shared with kind nodes): \
             {error}"
        ))
    })?;
    check_landlock_listed(&lsm)
}

/// The pure check behind [`ensure_landlock_available`], separated out so it
/// can be tested against sample `/sys/kernel/security/lsm` contents without
/// depending on this host's own kernel configuration.
fn check_landlock_listed(lsm: &str) -> Result<()> {
    if lsm.split(',').any(|name| name.trim() == "landlock") {
        Ok(())
    } else {
        infrastructure(format!(
            "the host kernel does not have Landlock enabled ({LSM_PATH} lists {lsm:?}, not \
             \"landlock\"); every generated controller's startup applies a Landlock lockdown, \
             and kind nodes share this host's kernel, so the acceptance suite cannot run here"
        ))
    }
}

/// Check the container engine, kind and the pinned node image before anything
/// is created, so each missing prerequisite is reported as itself.
///
/// # Errors
/// Names the first prerequisite that is missing.
pub async fn preflight() -> Result<Toolchain> {
    ensure_landlock_available()?;
    let engine = engine::current()?;
    let version = engine
        .command()
        .args(["version", "--format", engine.version_format()])
        .timeout(Duration::from_secs(30))
        .output()
        .await?;
    if !version.success || version.stdout.trim().is_empty() {
        return infrastructure(format!(
            "{} is not reachable; start it and retry: {}",
            engine.name(),
            tail(&version.stderr, 500)
        ));
    }
    let kind = engine
        .kind()
        .arg("version")
        .timeout(Duration::from_secs(30))
        .run()
        .await?;
    let image = engine
        .command()
        .args(["image", "inspect", "--format", "{{.Id}}", NODE_IMAGE])
        .output()
        .await?;
    if !image.success || image.stdout.trim().is_empty() {
        return infrastructure(format!(
            "the kind node image {NODE_IMAGE} is not in the local {} image store; pull it before running the suite",
            engine.name()
        ));
    }
    Ok(Toolchain {
        kind_version: kind.trim().to_owned(),
        container_engine: engine.program(),
        container_engine_version: version.stdout.trim().to_owned(),
        node_image_id: image.stdout.trim().to_owned(),
    })
}

/// A kind cluster this process owns, deleted when finished or dropped.
pub struct Cluster {
    name: String,
    directory: tempfile::TempDir,
    keep: bool,
    finished: bool,
}

impl Cluster {
    /// Choose a fresh name and a private directory for the kubeconfig. Nothing
    /// exists in Docker until [`Cluster::create`].
    ///
    /// # Errors
    /// Fails when the private directory cannot be created.
    pub fn new(keep: bool) -> Result<Self> {
        Self::with_prefix(NAME_PREFIX, keep)
    }

    /// As [`Cluster::new`], naming the cluster with `prefix`, which must be a
    /// lowercase DNS label ending in `-`.
    ///
    /// # Errors
    /// Fails when the prefix is invalid or the private directory cannot be
    /// created.
    pub fn with_prefix(prefix: &str, keep: bool) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("aps-test-cluster-")
            .tempdir()?;
        restrict(directory.path())?;
        Ok(Self {
            name: unique_name(prefix)?,
            directory,
            keep,
            finished: false,
        })
    }

    /// Create the cluster from the pinned node image. The image must already
    /// be local: kind is never allowed to pull it. An existing cluster with
    /// the chosen name is refused rather than reused.
    ///
    /// # Errors
    /// Fails when the name is taken or kind cannot create the cluster.
    pub async fn create(&self) -> Result<()> {
        let existing = engine::current()?
            .kind()
            .args(["get", "clusters"])
            .run()
            .await?;
        if existing.lines().any(|line| line.trim() == self.name) {
            return infrastructure(format!("a kind cluster named {} already exists", self.name));
        }
        let config = self.directory.path().join("kind.yaml");
        std::fs::write(&config, kind_config())?;
        let created = engine::current()?
            .kind()
            .args([
                "create", "cluster", "--name", &self.name, "--image", NODE_IMAGE,
            ])
            .arg("--config")
            .arg(&config)
            .arg("--kubeconfig")
            .arg(self.admin_kubeconfig())
            .args(["--wait", "120s"])
            .timeout(Duration::from_secs(420))
            .output()
            .await?;
        if !created.success {
            return infrastructure(format!(
                "kind could not create {}: {}",
                self.name,
                tail(&created.stderr, 2000)
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn context(&self) -> String {
        format!("kind-{}", self.name)
    }

    /// The container kind runs the single control-plane node in.
    #[must_use]
    pub fn node(&self) -> String {
        format!("{}-control-plane", self.name)
    }

    /// Every node container of the cluster.
    ///
    /// # Errors
    /// Fails when kind cannot list the cluster's nodes or lists none.
    pub async fn nodes(&self) -> Result<Vec<String>> {
        let listed = engine::current()?
            .kind()
            .args(["get", "nodes", "--name", &self.name])
            .run()
            .await?;
        let nodes = listed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            return infrastructure(format!("kind reports no nodes for {}", self.name));
        }
        Ok(nodes)
    }

    /// The private directory holding the cluster's kubeconfig; callers may
    /// keep other private files here for the cluster's lifetime.
    #[must_use]
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }

    fn admin_kubeconfig(&self) -> PathBuf {
        self.directory.path().join("admin.kubeconfig")
    }

    /// Copy the admin kubeconfig to `destination`, readable only by the
    /// owner, so a kept cluster can be inspected.
    ///
    /// # Errors
    /// Fails when the kubeconfig cannot be read or `destination` exists.
    pub fn export_kubeconfig(&self, destination: &Path) -> Result<()> {
        let bytes = bounded(&self.admin_kubeconfig())?;
        write_private(destination, &bytes)
    }

    /// The admin kubeconfig kind wrote, parsed.
    ///
    /// # Errors
    /// Fails when the kubeconfig is missing, oversized or invalid.
    pub fn kubeconfig(&self) -> Result<Kubeconfig> {
        let bytes = bounded(&self.admin_kubeconfig())?;
        Ok(Kubeconfig::from_yaml(&String::from_utf8_lossy(&bytes))?)
    }

    /// A client with the fixture driver's (cluster admin) identity.
    ///
    /// # Errors
    /// Fails when the kubeconfig cannot be read or names no usable context.
    pub async fn admin_client(&self) -> Result<Client> {
        client(self.kubeconfig()?, &self.context()).await
    }

    /// The API server URL and CA the admin kubeconfig names.
    ///
    /// # Errors
    /// Fails when the kubeconfig lacks the cluster, its server or an inline CA.
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

    /// Delete the cluster unless the caller asked to keep it. Deleting a
    /// cluster kind never finished creating removes whatever it left.
    ///
    /// Rootless podman can give up on a node whose processes outlive its
    /// SIGKILL wait (`given PID did not die within timeout`), leaving the
    /// container `Stopping`; it exits moments later, so the delete is retried.
    pub async fn finish(mut self) -> Disposition {
        self.finished = true;
        if self.keep {
            return Disposition::Kept;
        }
        let Ok(engine) = engine::current() else {
            return Disposition::DeleteFailed;
        };
        for attempt in 1..=DELETE_ATTEMPTS {
            let failure = match engine
                .kind()
                .args(["delete", "cluster", "--name", &self.name])
                .arg("--kubeconfig")
                .arg(self.admin_kubeconfig())
                .timeout(Duration::from_secs(180))
                .output()
                .await
            {
                Ok(output) if output.success => return Disposition::Deleted,
                Ok(output) => tail(&output.stderr, 2000),
                Err(error) => error.to_string(),
            };
            let _ = writeln!(
                std::io::stderr().lock(),
                "deleting cluster {} failed (attempt {attempt} of {DELETE_ATTEMPTS}): {failure}",
                self.name
            );
            if attempt < DELETE_ATTEMPTS {
                tokio::time::sleep(DELETE_RETRY_PAUSE).await;
            }
        }
        Disposition::DeleteFailed
    }
}

impl Drop for Cluster {
    /// The fallback for an early return or a panic in a caller: the cluster is
    /// still removed, synchronously and within a bound, before the temporary
    /// kubeconfig goes.
    fn drop(&mut self) {
        if self.finished || self.keep {
            return;
        }
        let Ok(engine) = engine::current() else {
            return;
        };
        let Ok(mut child) = std::process::Command::new("kind")
            .env("KIND_EXPERIMENTAL_PROVIDER", engine.program())
            .args(["delete", "cluster", "--name", &self.name])
            .arg("--kubeconfig")
            .arg(self.admin_kubeconfig())
            .env_remove("KUBECONFIG")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(DROP_DELETE_SECONDS);
        while std::time::Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The kind configuration: one node, IPv4 only, and the Pod and Service
/// ranges the egress rules allow.
fn kind_config() -> String {
    format!(
        "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnetworking:\n  ipFamily: ipv4\n  podSubnet: \"{POD_SUBNET}\"\n  serviceSubnet: \"{SERVICE_SUBNET}\"\nnodes:\n  - role: control-plane\n"
    )
}

/// Where the API server is and the CA that signs its certificate.
pub struct Endpoint {
    pub server: String,
    /// Base64 PEM, as it appears in a kubeconfig.
    pub authority: String,
}

impl Endpoint {
    /// A kubeconfig that authenticates with `token` alone.
    #[must_use]
    pub fn token_kubeconfig(&self, context: &str, token: &str) -> Value {
        token_kubeconfig(&self.server, &self.authority, context, token, None)
    }
}

/// A single-context kubeconfig whose only credential is a bearer token. The
/// fixture-mode runtime accepts nothing else.
#[must_use]
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
///
/// # Errors
/// Fails when the context is absent or the configuration is unusable.
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
fn unique_name(prefix: &str) -> Result<String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| crate::Error::Infrastructure(error.to_string()))?
        .as_secs();
    let name = format!("{prefix}{}-{seconds}", std::process::id());
    if !valid_name(&name, prefix) {
        return infrastructure(format!("generated cluster name {name} is invalid"));
    }
    Ok(name)
}

/// Kind names become Docker container names and kubeconfig context names, so
/// they are held to a lowercase DNS label.
fn valid_name(name: &str, prefix: &str) -> bool {
    prefix.ends_with('-')
        && prefix.len() > 1
        && name.starts_with(prefix)
        && name.len() > prefix.len()
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
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
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
        let name = unique_name(NAME_PREFIX)?;
        assert!(valid_name(&name, NAME_PREFIX), "{name}");
        assert!(!valid_name("kind", NAME_PREFIX));
        assert!(!valid_name("aps-test-UPPER", NAME_PREFIX));
        assert!(!valid_name("aps-test-", NAME_PREFIX));
        assert!(!valid_name(
            &format!("{NAME_PREFIX}{}", "9".repeat(60)),
            NAME_PREFIX
        ));
        let other = unique_name("aps-acc-")?;
        assert!(valid_name(&other, "aps-acc-"), "{other}");
        assert!(!valid_name(&other, NAME_PREFIX));
        assert!(unique_name("Bad_").is_err());
        Ok(())
    }

    #[test]
    fn the_kind_config_pins_the_ranges_the_egress_rules_allow() {
        let config = kind_config();
        assert!(config.contains(&format!("podSubnet: \"{POD_SUBNET}\"")));
        assert!(config.contains(&format!("serviceSubnet: \"{SERVICE_SUBNET}\"")));
        assert!(config.contains("ipFamily: ipv4"));
    }

    /// See the Apsides v0.3 plan, Phase 5c, item 5: `aps test --cluster`/the
    /// acceptance suite must report a clear infrastructure error, not a
    /// confusing downstream controller-pod failure, when the host kernel
    /// lacks Landlock.
    #[test]
    fn landlock_listed_is_accepted_in_any_position() {
        assert!(check_landlock_listed("landlock").is_ok());
        assert!(check_landlock_listed("lockdown,capability,landlock,yama,apparmor").is_ok());
        assert!(check_landlock_listed("lockdown,capability,landlock\n").is_ok());
    }

    #[test]
    fn landlock_missing_is_a_clear_infrastructure_error() -> Result<()> {
        let Err(error) = check_landlock_listed("lockdown,capability,yama,apparmor") else {
            return Err(crate::Error::Case(
                "missing landlock must be rejected".into(),
            ));
        };
        let message = error.to_string();
        assert!(message.contains("Landlock"), "{message}");
        assert!(message.contains(LSM_PATH), "{message}");
        Ok(())
    }

    #[test]
    fn landlock_check_does_not_match_a_substring_of_another_module_name() {
        // A hostile or coincidental LSM name containing "landlock" as a
        // substring (rather than being exactly it) must not pass.
        assert!(check_landlock_listed("not_landlock_at_all").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exported_kubeconfigs_are_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("kubeconfig");
        write_private(&path, b"apiVersion: v1")?;
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        assert!(write_private(&path, b"again").is_err(), "never overwrites");
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
