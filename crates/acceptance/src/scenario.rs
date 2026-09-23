//! One scenario's disposable cluster, from creation to deletion.
//!
//! [`run`] creates a kind cluster named `aps-acc-…`, hands the scenario body a
//! [`Scenario`] holding the admin client, the node and the private state
//! `aps` is given, collects allowlisted diagnostics if the body fails, deletes
//! the cluster, confirms by exact name that kind and Docker no longer know it,
//! and writes the redacted report.

use crate::{
    Result,
    aps::{Aps, Run},
    bootstrap::{self, Platform, Settings},
    cnpg, ensure,
    inputs::Inputs,
    node,
    objects::Objects,
    pair::Candidate,
    record::Recorder,
    wait,
};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api,
    api::{ListParams, LogParams},
};
use platform::PolicyState;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use test_cluster::{
    NODE_IMAGE,
    cluster::{Cluster, Disposition, Endpoint},
    process::{Ending, Invocation, Output, Running},
};

/// Every cluster this suite creates is named with this prefix.
pub const CLUSTER_PREFIX: &str = "aps-acc-";

/// Namespaces whose objects a failed scenario's diagnostics list.
const DIAGNOSED: &[&str] = &[
    crate::pair::CONTROL_NAMESPACE,
    crate::pair::WORKLOAD_NAMESPACE,
    "cnpg-system",
];

/// What a scenario body works with.
pub struct Scenario<'a> {
    pub inputs: &'a Inputs,
    pub cluster: &'a Cluster,
    /// The cluster administrator's view of every kind the suite handles.
    pub objects: Objects,
    /// The kind node's container name.
    pub node: String,
    /// The node's `os/architecture`.
    pub platform: String,
    pub recorder: &'a mut Recorder,
    /// The one database `aps` and bootstrap share, as a deployment job's
    /// `APSIDES_INVENTORY_DATABASE`.
    pub database: PathBuf,
    /// The executor identity every `aps deploy` of the scenario passes.
    pub executor: String,
    endpoint: Endpoint,
    /// The issuer kubeconfig, refreshed before every `aps` run.
    issuer: PathBuf,
    aps: Aps,
}

/// Run `body` on a fresh cluster and record its outcome under `name`.
///
/// # Errors
/// Fails when the inputs are missing, the cluster cannot be created, the
/// body fails, or the cluster is not deleted afterwards.
pub async fn run(name: &str, body: impl AsyncFnOnce(&mut Scenario<'_>) -> Result) -> Result {
    let inputs = Inputs::load()?;
    let results = inputs.results.join(name);
    if results.exists() {
        std::fs::remove_dir_all(&results)?;
    }
    let mut recorder = Recorder::new(results);
    test_cluster::cluster::preflight().await?;
    let state = private_directory()?;
    let cluster = Cluster::with_prefix(CLUSTER_PREFIX, false)?;
    let cluster_name = cluster.name().to_owned();
    note(&format!("{name}: creating cluster {cluster_name}"));
    let outcome = match cluster.create().await {
        Ok(()) => {
            let connected =
                Scenario::connect(&inputs, &cluster, &mut recorder, state.path(), name).await;
            match connected {
                Ok(mut scenario) => {
                    let outcome = body(&mut scenario).await;
                    if outcome.is_err() {
                        scenario.diagnose().await;
                    }
                    outcome
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error.into()),
    };
    let disposition = cluster.finish().await;
    let absent = cluster_absent(&cluster_name).await;
    recorder.write(&json!({
        "scenario": name,
        "passed": outcome.is_ok()
            && disposition == Disposition::Deleted
            && matches!(absent, Ok(true)),
        "error": outcome.as_ref().err().map(ToString::to_string),
        "cluster": cluster_name,
        "clusterDisposition": disposition,
        "clusterAbsent": absent.as_ref().ok(),
    }))?;
    outcome?;
    ensure(
        disposition == Disposition::Deleted && absent?,
        format!("the acceptance cluster {cluster_name} was not deleted"),
    )
}

impl<'a> Scenario<'a> {
    async fn connect(
        inputs: &'a Inputs,
        cluster: &'a Cluster,
        recorder: &'a mut Recorder,
        state: &Path,
        name: &str,
    ) -> Result<Self> {
        let objects = Objects::new(cluster.admin_client().await?);
        let platform = node::platform(&objects).await?;
        let database = state.join("inventory.db");
        let issuer = cluster.directory().join("issuer.kubeconfig");
        Ok(Self {
            inputs,
            cluster,
            node: cluster.node(),
            platform,
            recorder,
            aps: Aps::new(inputs.aps.clone(), issuer.clone(), database.clone()),
            endpoint: cluster.endpoint()?,
            database,
            issuer,
            executor: format!("acceptance-{name}"),
            objects,
        })
    }

    /// Put every image the platform and `candidates` need on the node,
    /// install the `CloudNativePG` operator, and bootstrap the platform for
    /// `candidates` under `policies`.
    ///
    /// # Errors
    /// Fails when an image, the operator or any bootstrap object is refused.
    pub async fn prepare(
        &mut self,
        candidates: &[&Candidate],
        policies: &BTreeMap<String, PolicyState>,
    ) -> Result<Platform> {
        let mut images = BTreeSet::from([
            platform::ENVOY_IMAGE.to_owned(),
            platform::ADMIN_IMAGE.to_owned(),
            cnpg::OPERATOR_IMAGE.to_owned(),
        ]);
        for candidate in candidates {
            images.extend(workload_images(candidate)?);
        }
        for image in &images {
            node::pull(&self.node, image, &self.platform).await?;
        }
        for candidate in candidates {
            node::import_release(&self.node, candidate, &self.platform).await?;
        }
        note("images on the node; installing the CNPG operator");
        let operator = cnpg::operator_documents(&std::fs::read(&self.inputs.operator_manifest)?)?;
        let installed = cnpg::install_operator(&self.objects, &operator).await?;
        self.recorder.check("cnpg-operator-installed", &installed);
        let trust_policy: Value =
            serde_json::from_slice(&std::fs::read(&self.inputs.trust_policy)?)?;
        let qualification = compiler_core::digest(&json!({
            "schemaVersion": "apsides.dev/acceptance-environment/v1",
            "nodeImage": NODE_IMAGE,
            "envoyEnvironmentCanary": "not run by the acceptance suite",
        }))?;
        let platform = bootstrap::bootstrap(
            &self.objects,
            &Settings {
                candidates,
                policies,
                trust_policy: &trust_policy,
                database: &self.database,
                node: &self.node,
                node_image_digest: NODE_IMAGE.rsplit_once('@').map_or("", |(_, digest)| digest),
                environment_qualification: &qualification,
            },
        )
        .await?;
        for key in &platform.rollout_keys {
            self.recorder.canary(key);
            self.recorder.canary(crate::workload::hex(key).as_bytes());
        }
        note("platform bootstrapped");
        Ok(platform)
    }

    /// Refresh the issuer kubeconfig and record its token as a canary.
    async fn issue(&mut self, platform: &Platform) -> Result {
        let token = platform
            .issuer_kubeconfig(&self.endpoint, &self.issuer)
            .await?;
        self.recorder.canary(token.as_bytes());
        Ok(())
    }

    /// `aps deploy` of `release` with the scenario's executor, recorded as
    /// `label`.
    ///
    /// # Errors
    /// Fails when the issuer token cannot be minted or `aps` cannot run.
    pub async fn deploy(
        &mut self,
        platform: &Platform,
        release: &Path,
        label: &str,
    ) -> Result<Run> {
        self.issue(platform).await?;
        let run = self.aps.deploy(release, &self.executor).await?;
        self.recorder
            .document(format!("aps/{label}.stdout.json"), run.stdout.clone());
        self.recorder
            .document(format!("aps/{label}.stderr.log"), run.stderr.clone());
        Ok(run)
    }

    /// Start `aps deploy` of `release` with the scenario's executor and
    /// return without waiting for it.
    ///
    /// # Errors
    /// Fails when the issuer token cannot be minted or `aps` cannot start.
    pub async fn spawn(&mut self, platform: &Platform, release: &Path) -> Result<Running> {
        self.issue(platform).await?;
        self.aps.spawn(release, &self.executor)
    }

    /// A client of the cluster that authenticates with `token` alone.
    ///
    /// # Errors
    /// Fails when the client cannot be configured.
    pub async fn token_client(&self, token: &str) -> Result<kube::Client> {
        const CONTEXT: &str = "acceptance-token";
        let kubeconfig = serde_json::from_value(self.endpoint.token_kubeconfig(CONTEXT, token))?;
        Ok(test_cluster::cluster::client(kubeconfig, CONTEXT).await?)
    }

    /// Record a spawned run's output as `label`.
    pub fn document(&mut self, label: &str, output: &Output) {
        self.recorder
            .document(format!("aps/{label}.stdout.json"), output.stdout.clone());
        self.recorder
            .document(format!("aps/{label}.stderr.log"), output.stderr.clone());
    }

    /// Logs of every container of every Pod in the control namespace.
    ///
    /// # Errors
    /// Fails when the Pods cannot be listed.
    pub async fn controller_logs(&mut self, label: &str) -> Result {
        let pods: Api<Pod> = Api::namespaced(
            self.objects.client().clone(),
            crate::pair::CONTROL_NAMESPACE,
        );
        for pod in pods.list(&ListParams::default()).await?.items {
            let name = pod.metadata.name.clone().unwrap_or_default();
            for container in pod.spec.iter().flat_map(|spec| spec.containers.iter()) {
                let params = LogParams {
                    container: Some(container.name.clone()),
                    ..LogParams::default()
                };
                let log = pods.logs(&name, &params).await.unwrap_or_default();
                self.recorder
                    .document(format!("logs/{label}/{name}.{}.log", container.name), log);
            }
        }
        Ok(())
    }

    /// Every value of the workload and control namespaces' Secrets, other
    /// than certificates, key IDs and user names, as canaries.
    ///
    /// # Errors
    /// Fails when the Secrets cannot be listed.
    pub async fn secret_canaries(&mut self) -> Result {
        use base64::Engine as _;
        for namespace in [
            crate::pair::WORKLOAD_NAMESPACE,
            crate::pair::CONTROL_NAMESPACE,
        ] {
            for secret in self
                .objects
                .list("v1", "Secret", Some(namespace), None)
                .await?
            {
                for (key, value) in secret["data"].as_object().into_iter().flatten() {
                    let public = Path::new(key)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("crt"))
                        || key == "keyId"
                        || key == "username";
                    if public {
                        continue;
                    }
                    if let Some(bytes) = value.as_str().and_then(|value| {
                        base64::engine::general_purpose::STANDARD.decode(value).ok()
                    }) {
                        self.recorder.canary(&bytes);
                    }
                }
            }
        }
        Ok(())
    }

    /// Allowlisted diagnostics after a failure.
    async fn diagnose(&mut self) {
        let _ = self.controller_logs("at-failure").await;
        let diagnostics = wait::diagnostics(&self.objects, DIAGNOSED).await;
        if let Ok(bytes) = serde_json::to_vec_pretty(&diagnostics) {
            self.recorder.document("diagnostics.json", bytes);
        }
    }
}

/// Every digest-pinned container image `candidate`'s resources run: CNPG
/// Cluster images and Deployment containers.
///
/// # Errors
/// Fails when such an image is not pinned by digest.
pub fn workload_images(candidate: &Candidate) -> Result<BTreeSet<String>> {
    let mut images = BTreeSet::new();
    for resource in &candidate.plan.ir.resources {
        let desired = &resource.desired;
        let declared: Vec<&Value> = match resource.kind.as_str() {
            "Cluster" => desired.pointer("/spec/imageName").into_iter().collect(),
            "Deployment" => desired
                .pointer("/spec/template/spec/containers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|container| &container["image"])
                .collect(),
            _ => Vec::new(),
        };
        for image in declared {
            let image = image.as_str().unwrap_or_default();
            ensure(
                image.contains("@sha256:"),
                format!("{} {} does not pin its image", resource.kind, resource.name),
            )?;
            images.insert(image.to_owned());
        }
    }
    Ok(images)
}

/// A progress line on stderr, so a CI log shows which step a run reached.
pub fn note(step: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "acceptance: {step}");
}

/// A fresh directory only this user can read.
fn private_directory() -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("aps-acceptance-")
        .tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

/// Whether kind no longer lists the cluster and Docker has no node container
/// of that name.
async fn cluster_absent(name: &str) -> Result<bool> {
    let clusters = Invocation::new("kind")
        .args(["get", "clusters"])
        .run()
        .await?;
    let containers = Invocation::new("docker")
        .args(["ps", "--all", "--quiet", "--filter"])
        .arg(format!("name=^/{name}-control-plane$"))
        .run()
        .await?;
    Ok(!clusters.lines().any(|line| line.trim() == name) && containers.trim().is_empty())
}

/// How a stopped `aps` run ended, for a report.
#[must_use]
pub fn ending(ending: Ending) -> Value {
    match ending {
        Ending::Killed => json!({"stoppedBy": "SIGKILL"}),
        Ending::Exited { success } => json!({"stoppedBy": "exit", "success": success}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_are_reported_by_how_they_ended() {
        assert_eq!(ending(Ending::Killed), json!({"stoppedBy": "SIGKILL"}));
        assert_eq!(
            ending(Ending::Exited { success: false }),
            json!({"stoppedBy": "exit", "success": false})
        );
    }
}
