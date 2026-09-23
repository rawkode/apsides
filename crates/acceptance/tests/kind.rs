//! Acceptance scenarios on disposable kind clusters.
//!
//! `cuenv task acceptancePrepare` builds and signs the releases and writes
//! `work/acceptance/inputs.json`; `cuenv task acceptanceTest` runs these with
//! `APSIDES_ACCEPTANCE_INPUTS` naming it. Each scenario creates its own
//! cluster named `aps-acc-…`, never reads the caller's kubeconfig, deletes the
//! cluster when it ends, and writes a redacted report and logs under the
//! inputs' results directory.

use acceptance::{
    Result,
    aps::{self, Aps, Committed, Run},
    bootstrap::{self, Platform, Settings},
    cnpg::{self, DatabaseIdentity},
    ensure,
    inputs::Inputs,
    node,
    objects::{Objects, text},
    pair::{self, CONTROL_NAMESPACE, Pair, WORKLOAD_NAMESPACE},
    record::Recorder,
    wait,
    workload::{self, Workload},
};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api,
    api::{ListParams, LogParams},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};
use test_cluster::{
    NODE_IMAGE,
    cluster::{Cluster, Disposition, Endpoint},
    process::Invocation,
};

/// Every cluster this suite creates is named with this prefix.
const CLUSTER_PREFIX: &str = "aps-acc-";
/// The executor identity every `aps deploy` of the scenario passes.
const EXECUTOR: &str = "acceptance-cnpg-upgrade";

/// P0, spec §28 "Add a capability above the ceiling", "Upgrade to a build
/// with different permissions", "Reconcile unchanged ConfigMap/Secret input
/// repeatedly" and "Inspect artifacts, logs, metrics, and inventory":
///
/// (a) under a ceiling that allows only the initial release, `aps deploy`
///     of the initial release commits;
/// (b) `aps deploy` of the successor is rejected for the capability the
///     ceiling lacks, before any successor object exists, with the registry
///     and the running initial writer untouched;
/// (c) once the ceiling is widened, `aps deploy` of the successor commits a
///     new generation;
/// (d) the old generation's `RoleBinding`s and `ServiceAccount` are retired and
///     the new `ServiceAccount` is distinct;
/// (e) a database row written before the upgrade survives, with the CNPG
///     Cluster and volume claim UIDs unchanged;
/// (f) a `ConfigMap` change rolls the API out, equal data causes no rollout
///     for 30 s, and a Secret change rolls it out and back;
/// (g) no Secret value, rollout key or issuer token appears in `aps` output,
///     the inventory database or the controllers' logs.
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
async fn cnpg_upgrade_through_the_public_cli() -> Result {
    let inputs = Inputs::load()?;
    let pair = Pair::load(&inputs.initial_release, &inputs.successor_release)?;
    let operator = cnpg::operator_documents(&std::fs::read(&inputs.operator_manifest)?)?;
    let trust_policy: Value = serde_json::from_slice(&std::fs::read(&inputs.trust_policy)?)?;
    let results = inputs.results.join("cnpg-upgrade");
    if results.exists() {
        std::fs::remove_dir_all(&results)?;
    }
    let mut recorder = Recorder::new(results);
    test_cluster::cluster::preflight().await?;
    let state = private_directory()?;
    let cluster = Cluster::with_prefix(CLUSTER_PREFIX, false)?;
    let name = cluster.name().to_owned();
    let outcome = match cluster.create().await {
        Ok(()) => {
            let context = Context {
                cluster: &cluster,
                inputs: &inputs,
                pair: &pair,
                operator: &operator,
                trust_policy: &trust_policy,
                database: state.path().join("inventory.db"),
                issuer: cluster.directory().join("issuer.kubeconfig"),
            };
            let outcome = context.run(&mut recorder).await;
            if outcome.is_err() {
                context.diagnose(&mut recorder).await;
            }
            outcome
        }
        Err(error) => Err(error.into()),
    };
    let disposition = cluster.finish().await;
    let absent = cluster_absent(&name).await;
    recorder.write(&json!({
        "scenario": "cnpg-upgrade",
        "passed": outcome.is_ok()
            && disposition == Disposition::Deleted
            && matches!(absent, Ok(true)),
        "error": outcome.as_ref().err().map(ToString::to_string),
        "cluster": name,
        "clusterDisposition": disposition,
        "clusterAbsent": absent.as_ref().ok(),
    }))?;
    outcome?;
    ensure(
        disposition == Disposition::Deleted && absent?,
        format!("the acceptance cluster {name} was not deleted"),
    )
}

/// Everything the scenario's steps share.
struct Context<'a> {
    cluster: &'a Cluster,
    inputs: &'a Inputs,
    pair: &'a Pair,
    operator: &'a [Value],
    trust_policy: &'a Value,
    /// The one database `aps` and bootstrap share, as a deployment job's
    /// `APSIDES_INVENTORY_DATABASE`.
    database: PathBuf,
    /// The issuer kubeconfig, refreshed before every `aps` run.
    issuer: PathBuf,
}

/// What the initial generation left for later steps to compare against.
struct Initial {
    committed: Committed,
    database: DatabaseIdentity,
    cluster_uid: String,
    runtime_account: String,
    retirement: Vec<(String, String)>,
}

impl Context<'_> {
    async fn run(&self, recorder: &mut Recorder) -> Result {
        let objects = Objects::new(self.cluster.admin_client().await?);
        let node = self.cluster.node();
        let platform_name = node::platform(&objects).await?;
        for image in [
            platform::ENVOY_IMAGE,
            platform::ADMIN_IMAGE,
            cnpg::OPERATOR_IMAGE,
            &postgres_image(self.pair)?,
            &self.pair.api_image,
        ] {
            node::pull(&node, image, &platform_name).await?;
        }
        node::import_release(&node, &self.pair.initial, &platform_name).await?;
        node::import_release(&node, &self.pair.successor, &platform_name).await?;
        note("images on the node; installing the CNPG operator");
        let operator = cnpg::install_operator(&objects, self.operator).await?;
        recorder.check("cnpg-operator-installed", &operator);

        let qualification = compiler_core::digest(&json!({
            "schemaVersion": "apsides.dev/acceptance-environment/v1",
            "nodeImage": NODE_IMAGE,
            "envoyEnvironmentCanary": "not run by the acceptance suite",
        }))?;
        let mut platform = bootstrap::bootstrap(
            &objects,
            &Settings {
                pair: self.pair,
                trust_policy: self.trust_policy,
                database: &self.database,
                node: &node,
                node_image_digest: NODE_IMAGE.rsplit_once('@').map_or("", |(_, digest)| digest),
                environment_qualification: &qualification,
            },
        )
        .await?;
        recorder.canary(&platform.rollout_key);
        recorder.canary(workload::hex(&platform.rollout_key).as_bytes());
        let endpoint = self.cluster.endpoint()?;
        let aps = Aps::new(
            self.inputs.aps.clone(),
            self.issuer.clone(),
            self.database.clone(),
        );

        note("platform bootstrapped; (a) deploying the initial release");
        let initial = self
            .initial_commits(&objects, &platform, &endpoint, &aps, recorder)
            .await?;
        note("(b) deploying the successor under the initial ceiling");
        self.successor_rejected(&objects, &platform, &endpoint, &aps, &initial, recorder)
            .await?;
        note("(c) widening the ceiling and deploying the successor");
        let change = platform.replace_policy(&self.pair.successor_policy).await?;
        let successor = self
            .successor_commits(&platform, &endpoint, &aps, &initial, change, recorder)
            .await?;
        note("(d) checking the old generation's identity is retired");
        self.old_identity_retired(&objects, &platform, &initial, &successor, recorder)
            .await?;
        note("(e) checking the database survived");
        self.database_preserved(&objects, &initial, recorder)
            .await?;
        note("(f) exercising the input reactions");
        self.inputs_roll_out(&objects, &node, recorder).await?;
        note("(g) scanning for secret values");
        self.nothing_leaks(&objects, recorder).await
    }

    async fn deploy(
        &self,
        platform: &Platform,
        endpoint: &Endpoint,
        aps: &Aps,
        release: &Path,
        label: &str,
        recorder: &mut Recorder,
    ) -> Result<Run> {
        let token = platform.issuer_kubeconfig(endpoint, &self.issuer).await?;
        recorder.canary(token.as_bytes());
        let run = aps.deploy(release, EXECUTOR).await?;
        recorder.document(format!("aps/{label}.stdout.json"), run.stdout.clone());
        recorder.document(format!("aps/{label}.stderr.log"), run.stderr.clone());
        Ok(run)
    }

    /// (a)
    async fn initial_commits(
        &self,
        objects: &Objects,
        platform: &Platform,
        endpoint: &Endpoint,
        aps: &Aps,
        recorder: &mut Recorder,
    ) -> Result<Initial> {
        let initial = &self.pair.initial;
        let run = self
            .deploy(
                platform,
                endpoint,
                aps,
                &initial.root,
                "deploy-initial",
                recorder,
            )
            .await?;
        let committed = aps::committed(&run, &initial.release_digest, &initial.manifest.ir_digest)?;
        let ready = cnpg::wait_ready(objects, WORKLOAD_NAMESPACE).await?;
        self.secret_canaries(objects, recorder).await?;
        let database = cnpg::persistence(objects, WORKLOAD_NAMESPACE, None).await?;
        objects
            .create(&json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "api-settings", "namespace": WORKLOAD_NAMESPACE},
                "data": {"PGRST_DB_MAX_ROWS": "100"},
            }))
            .await?;
        let registry = platform.registry().await?;
        let checkpoint = registry
            .value
            .active_writer_checkpoints
            .get(platform.domain())
            .ok_or_else(|| {
                acceptance::Error::Check("the initial generation has no writer checkpoint".into())
            })?;
        ensure(
            checkpoint.allocation.generation_id == committed.generation
                && checkpoint.allocation.transition_id == committed.transition,
            "the active writer is not the committed initial generation",
        )?;
        recorder.check(
            "a-initial-commits-under-restricted-ceiling",
            &json!({"report": report(&committed), "cnpg": ready, "database": database}),
        );
        Ok(Initial {
            cluster_uid: text(&ready, "/clusterUid").to_owned(),
            runtime_account: checkpoint.runtime_binding.service_account_name.clone(),
            retirement: checkpoint
                .retirement_identity_uids
                .iter()
                .map(|(key, uid)| (key.clone(), uid.clone()))
                .collect(),
            committed,
            database,
        })
    }

    /// (b)
    /// `aps deploy` of the successor runs twice under the initial policy. The
    /// first run captures the active claims' live UIDs into the registry (the
    /// public CLI does this before planning any upgrade) and is then rejected;
    /// the check allows exactly that registry change. The second run has
    /// nothing left to capture, so its rejection must leave the registry's
    /// resourceVersion identical. Both leave every object and the initial
    /// writer untouched.
    async fn successor_rejected(
        &self,
        objects: &Objects,
        platform: &Platform,
        endpoint: &Endpoint,
        aps: &Aps,
        initial: &Initial,
        recorder: &mut Recorder,
    ) -> Result {
        let registry = platform.registry().await?;
        let checkpoint = registry
            .value
            .active_writer_checkpoints
            .get(platform.domain())
            .ok_or_else(|| {
                acceptance::Error::Check("the initial writer checkpoint is absent".into())
            })?;
        let runtime = checkpoint.runtime_binding.service_account_name.clone();
        let proxy = checkpoint.proxy_identity.deployment_name.clone();
        let sequence = registry.value.next_sequence;
        let next = deploy::allocation_names(&registry.value.registry_uid, sequence)?;
        let writers =
            workload::writer_identity(objects, CONTROL_NAMESPACE, &runtime, &proxy).await?;
        let before = inventory(objects).await?;
        self.require_successor_absent(objects, &next).await?;

        self.rejected(
            platform,
            endpoint,
            aps,
            "deploy-successor-rejected",
            recorder,
        )
        .await?;
        let captured = platform.registry().await?;
        let uids = only_uid_capture(objects, &registry.value, &captured.value, initial).await?;
        let registry_object = registry_metadata(objects).await?;
        self.rejected(
            platform,
            endpoint,
            aps,
            "deploy-successor-rejected-again",
            recorder,
        )
        .await?;
        ensure(
            registry_metadata(objects).await? == registry_object
                && platform.registry().await?.value == captured.value,
            "the rejected deployment changed the registry",
        )?;
        let after = inventory(objects).await?;
        ensure(
            after == before,
            format!(
                "the rejected deployment changed objects: added {:?}, removed {:?}",
                after.difference(&before).collect::<Vec<_>>(),
                before.difference(&after).collect::<Vec<_>>()
            ),
        )?;
        self.require_successor_absent(objects, &next).await?;
        ensure(
            workload::writer_identity(objects, CONTROL_NAMESPACE, &runtime, &proxy).await?
                == writers,
            "permission rejection changed the active initial writer",
        )?;
        ensure(
            !captured.value.transitions.contains_key(&next.transition_id)
                && captured.value.next_sequence == sequence
                && captured
                    .value
                    .histories
                    .get(platform.domain())
                    .and_then(|history| history.active_allocation.as_ref())
                    .is_some_and(|active| active.generation_id == initial.committed.generation),
            "the rejected deployment reserved a transition or moved the active generation",
        )?;
        self.controller_logs(objects, "initial", recorder).await?;
        recorder.check(
            "b-successor-rejected-before-successor-objects",
            &json!({
                "diagnostic": pair::rejection(&self.pair.missing),
                "capturedClaimUids": uids,
                "registryUnchangedByRejection": registry_object,
                "unallocatedServiceAccount": next.service_account_name,
                "initialWriters": writers,
                "objectsCompared": before.len(),
            }),
        );
        Ok(())
    }

    /// One `aps deploy` of the successor, which must fail with the ceiling
    /// diagnostic for the capability the initial policy lacks.
    async fn rejected(
        &self,
        platform: &Platform,
        endpoint: &Endpoint,
        aps: &Aps,
        label: &str,
        recorder: &mut Recorder,
    ) -> Result {
        let run = self
            .deploy(
                platform,
                endpoint,
                aps,
                &self.pair.successor.root,
                label,
                recorder,
            )
            .await?;
        let expected = pair::rejection(&self.pair.missing);
        ensure(
            !run.success && run.stderr.contains(&expected),
            format!("aps deploy of the successor was not rejected for `{expected}`"),
        )
    }

    /// No object of the successor's API or of the next generation exists.
    async fn require_successor_absent(
        &self,
        objects: &Objects,
        next: &deploy::AllocationNames,
    ) -> Result {
        for (kind, api_version) in [("Deployment", "apps/v1"), ("Service", "v1")] {
            ensure(
                objects
                    .get(api_version, kind, Some(WORKLOAD_NAMESPACE), "database-api")
                    .await?
                    .is_none(),
                "unapproved successor object exists",
            )?;
        }
        let names = inventory(objects).await?;
        ensure(
            !names.iter().any(|entry| {
                entry.contains(&next.service_account_name) || entry.contains(&next.generation_id)
            }),
            "an object of the unapproved generation exists",
        )
    }

    /// (c)
    async fn successor_commits(
        &self,
        platform: &Platform,
        endpoint: &Endpoint,
        aps: &Aps,
        initial: &Initial,
        policy_change: Value,
        recorder: &mut Recorder,
    ) -> Result<Committed> {
        let successor = &self.pair.successor;
        let run = self
            .deploy(
                platform,
                endpoint,
                aps,
                &successor.root,
                "deploy-successor",
                recorder,
            )
            .await?;
        let committed = aps::committed(
            &run,
            &successor.release_digest,
            &successor.manifest.ir_digest,
        )?;
        ensure(
            committed.transition != initial.committed.transition
                && committed.generation != initial.committed.generation,
            "CLI upgrade reused the initial allocation",
        )?;
        let registry = platform.registry().await?;
        ensure(
            registry
                .value
                .histories
                .get(platform.domain())
                .and_then(|history| history.active_allocation.as_ref())
                .is_some_and(|active| active.generation_id == committed.generation),
            "the successor generation is not the active allocation",
        )?;
        recorder.check(
            "c-successor-commits-after-ceiling-widened",
            &json!({"policyChange": policy_change, "report": report(&committed)}),
        );
        Ok(committed)
    }

    /// (d)
    async fn old_identity_retired(
        &self,
        objects: &Objects,
        platform: &Platform,
        initial: &Initial,
        successor: &Committed,
        recorder: &mut Recorder,
    ) -> Result {
        let retired = initial
            .retirement
            .iter()
            .filter(|(key, _)| {
                key.contains("/RoleBinding/") || key.starts_with("v1/ServiceAccount/")
            })
            .collect::<Vec<_>>();
        let account_key = format!(
            "v1/ServiceAccount/{CONTROL_NAMESPACE}/{}",
            initial.runtime_account
        );
        let old_account_uid = retired
            .iter()
            .find(|(key, _)| *key == account_key)
            .map(|(_, uid)| uid.clone())
            .ok_or_else(|| {
                acceptance::Error::Check(
                    "the old ServiceAccount is not recorded for retirement".into(),
                )
            })?;
        ensure(
            retired.iter().any(|(key, _)| key.contains("/RoleBinding/")),
            "no old-generation RoleBinding is recorded for retirement",
        )?;
        for (key, _) in &retired {
            let (api_version, kind, namespace, name) = identity_key(key)?;
            ensure(
                objects
                    .get(&api_version, &kind, Some(&namespace), &name)
                    .await?
                    .is_none(),
                format!("old-generation {kind} {namespace}/{name} was not retired"),
            )?;
        }
        let registry = platform.registry().await?;
        let checkpoint = registry
            .value
            .active_writer_checkpoints
            .get(platform.domain())
            .ok_or_else(|| {
                acceptance::Error::Check("the successor writer checkpoint is absent".into())
            })?;
        ensure(
            checkpoint.allocation.generation_id == successor.generation,
            "the active writer is not the successor generation",
        )?;
        let account = checkpoint.runtime_binding.service_account_name.clone();
        let current = objects
            .require("v1", "ServiceAccount", Some(CONTROL_NAMESPACE), &account)
            .await?;
        ensure(
            account != initial.runtime_account
                && text(&current, "/metadata/uid") != old_account_uid,
            "the successor ServiceAccount is not distinct from the old one",
        )?;
        let old_subjects = [
            initial.runtime_account.clone(),
            format!("{}-process", initial.runtime_account),
        ];
        for namespace in [CONTROL_NAMESPACE, WORKLOAD_NAMESPACE] {
            for binding in objects
                .list(
                    "rbac.authorization.k8s.io/v1",
                    "RoleBinding",
                    Some(namespace),
                    None,
                )
                .await?
            {
                ensure(
                    !binding["subjects"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|subject| {
                            old_subjects
                                .iter()
                                .any(|old| subject["name"] == old.as_str())
                        }),
                    "a RoleBinding still grants the old generation's identity",
                )?;
            }
        }
        recorder.check(
            "d-old-generation-identity-retired",
            &json!({
                "retired": retired.iter().map(|(key, _)| key).collect::<Vec<_>>(),
                "oldServiceAccount": initial.runtime_account,
                "newServiceAccount": account,
            }),
        );
        Ok(())
    }

    /// (e)
    async fn database_preserved(
        &self,
        objects: &Objects,
        initial: &Initial,
        recorder: &mut Recorder,
    ) -> Result {
        let preserved =
            cnpg::persistence(objects, WORKLOAD_NAMESPACE, Some(&initial.database)).await?;
        ensure(
            preserved.cluster_uid == initial.cluster_uid,
            "the CNPG Cluster was replaced",
        )?;
        recorder.check(
            "e-row-and-volumes-preserved",
            &json!({"publicRowPreserved": true, "identity": preserved}),
        );
        Ok(())
    }

    /// (f)
    async fn inputs_roll_out(
        &self,
        objects: &Objects,
        node: &str,
        recorder: &mut Recorder,
    ) -> Result {
        let workload = Workload {
            objects,
            namespace: WORKLOAD_NAMESPACE,
            node,
            image: &self.pair.api_image,
        };
        objects
            .require("v1", "Service", Some(WORKLOAD_NAMESPACE), "database-api")
            .await?;
        let before = workload.identity(None, Duration::from_secs(300)).await?;
        let first = workload.messages(2, &before).await?;
        let (after, second) = workload.configmap_rollout(&before).await?;
        let equal = workload.equal_data_no_rollout(&after).await?;
        let probe = {
            use base64::Engine as _;
            let bytes = acceptance::random_bytes(24)?;
            recorder.canary(&bytes);
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        recorder.canary(probe.as_bytes());
        let secret = workload.secret_reaction(&after, &probe).await?;
        recorder.check(
            "f-input-reactions",
            &json!({
                "postgrestMessagesLive": first,
                "configmapDrivenPodTurnover": {"before": before, "after": after},
                "postgrestMessagesAfterRollout": second,
                "equalDataConfigmapNoRollout": equal,
                "secretKeyReactionAndRestoration": secret,
            }),
        );
        Ok(())
    }

    /// (g)
    async fn nothing_leaks(&self, objects: &Objects, recorder: &mut Recorder) -> Result {
        self.secret_canaries(objects, recorder).await?;
        self.controller_logs(objects, "successor", recorder).await?;
        let mut stores = Vec::new();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut path = self.database.clone().into_os_string();
            path.push(suffix);
            if let Ok(bytes) = std::fs::read(&path) {
                stores.push((format!("inventory.db{suffix}"), bytes));
            }
        }
        ensure(!stores.is_empty(), "the inventory database was not written")?;
        let leaks = recorder.leaks(&stores);
        let (searched, too_short) = recorder.canary_counts();
        ensure(
            leaks.is_empty(),
            format!("a Secret value, rollout key or token appears in {leaks:?}"),
        )?;
        recorder.check(
            "g-secret-and-rollout-key-canary-scan",
            &json!({
                "canariesSearched": searched,
                "canariesTooShort": too_short,
                "inventoryFiles": stores.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                "occurrences": 0,
            }),
        );
        Ok(())
    }

    /// Every value of the database's Secrets, and every private key or
    /// token in the control namespace's Secrets, as canaries.
    async fn secret_canaries(&self, objects: &Objects, recorder: &mut Recorder) -> Result {
        use base64::Engine as _;
        for namespace in [WORKLOAD_NAMESPACE, CONTROL_NAMESPACE] {
            for secret in objects.list("v1", "Secret", Some(namespace), None).await? {
                for (key, value) in secret["data"].as_object().into_iter().flatten() {
                    let public = std::path::Path::new(key)
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
                        recorder.canary(&bytes);
                    }
                }
            }
        }
        Ok(())
    }

    /// Logs of every container of every Pod in the control namespace.
    async fn controller_logs(
        &self,
        objects: &Objects,
        label: &str,
        recorder: &mut Recorder,
    ) -> Result {
        let pods: Api<Pod> = Api::namespaced(objects.client().clone(), CONTROL_NAMESPACE);
        for pod in pods.list(&ListParams::default()).await?.items {
            let name = pod.metadata.name.clone().unwrap_or_default();
            for container in pod.spec.iter().flat_map(|spec| spec.containers.iter()) {
                let params = LogParams {
                    container: Some(container.name.clone()),
                    ..LogParams::default()
                };
                let log = pods.logs(&name, &params).await.unwrap_or_default();
                recorder.document(format!("logs/{label}/{name}.{}.log", container.name), log);
            }
        }
        Ok(())
    }

    /// Allowlisted diagnostics after a failure.
    async fn diagnose(&self, recorder: &mut Recorder) {
        let Ok(client) = self.cluster.admin_client().await else {
            return;
        };
        let objects = Objects::new(client);
        let _ = self.controller_logs(&objects, "at-failure", recorder).await;
        let diagnostics = wait::diagnostics(
            &objects,
            &[CONTROL_NAMESPACE, WORKLOAD_NAMESPACE, "cnpg-system"],
        )
        .await;
        if let Ok(bytes) = serde_json::to_vec_pretty(&diagnostics) {
            recorder.document("diagnostics.json", bytes);
        }
    }
}

fn report(committed: &Committed) -> Value {
    json!({
        "status": committed.status,
        "artifactTrust": committed.artifact_trust,
        "releaseDigest": committed.release_digest,
        "irDigest": committed.ir_digest,
        "transition": committed.transition,
        "generation": committed.generation,
        "inventory": {"revision": committed.inventory.revision, "snapshotId": committed.inventory.snapshot_id},
    })
}

/// The `PostgreSQL` image both releases pin for the CNPG Cluster.
fn postgres_image(pair: &Pair) -> Result<String> {
    let image = pair
        .initial
        .plan
        .ir
        .resources
        .iter()
        .find(|resource| resource.kind == "Cluster")
        .and_then(|resource| resource.desired.pointer("/spec/imageName"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    ensure(
        image.contains("@sha256:"),
        "the CNPG Cluster does not pin its image",
    )?;
    Ok(image.to_owned())
}

/// Require `after` to differ from `before` only by the live UIDs of the
/// initial generation's active claims, and return those UIDs.
async fn only_uid_capture(
    objects: &Objects,
    before: &platform::NamespaceRegistry,
    after: &platform::NamespaceRegistry,
    initial: &Initial,
) -> Result<Vec<Value>> {
    let mut expected = before.clone();
    let mut captured = Vec::new();
    ensure(
        before.claims.len() == after.claims.len(),
        "the rejected deployment changed the protected claims",
    )?;
    for (claim, observed) in expected.claims.iter_mut().zip(&after.claims) {
        if claim.uid.is_some() || claim.identity != observed.identity {
            continue;
        }
        let Some(uid) = observed.uid.clone() else {
            continue;
        };
        let (api_version, kind) = match (
            claim.identity.api_group.as_str(),
            claim.identity.resource.as_str(),
        ) {
            ("", "configmaps") => ("v1", "ConfigMap"),
            ("postgresql.cnpg.io", "clusters") => ("postgresql.cnpg.io/v1", "Cluster"),
            _ => {
                return Err(acceptance::Error::Check(format!(
                    "unexpected captured claim {:?}",
                    claim.identity
                )));
            }
        };
        let live = objects
            .require(
                api_version,
                kind,
                Some(&claim.identity.namespace),
                &claim.identity.name,
            )
            .await?;
        ensure(
            claim.state == deploy::ClaimState::Active
                && claim.generation_id == initial.committed.generation
                && text(&live, "/metadata/uid") == uid,
            "a captured claim UID is not the live object of the active generation",
        )?;
        captured.push(json!({"kind": kind, "name": claim.identity.name, "uid": uid}));
        claim.uid = Some(uid);
    }
    ensure(
        &expected == after,
        "the rejected deployment changed the registry beyond capturing active claim UIDs",
    )?;
    Ok(captured)
}

/// The registry `ConfigMap`'s UID and resourceVersion.
async fn registry_metadata(objects: &Objects) -> Result<Value> {
    let registry = objects
        .require(
            "v1",
            "ConfigMap",
            Some(CONTROL_NAMESPACE),
            &format!("apsides-registry-{WORKLOAD_NAMESPACE}"),
        )
        .await?;
    Ok(
        json!({"uid": registry["metadata"]["uid"], "resourceVersion": registry["metadata"]["resourceVersion"]}),
    )
}

/// `kind/namespace/name/uid` of every object a deployment could create in
/// the control and workload namespaces.
async fn inventory(objects: &Objects) -> Result<BTreeSet<String>> {
    let mut entries = BTreeSet::new();
    for namespace in [CONTROL_NAMESPACE, WORKLOAD_NAMESPACE] {
        for (api_version, kind) in [
            ("v1", "ServiceAccount"),
            ("v1", "ConfigMap"),
            ("v1", "Secret"),
            ("v1", "Service"),
            ("apps/v1", "Deployment"),
            ("rbac.authorization.k8s.io/v1", "Role"),
            ("rbac.authorization.k8s.io/v1", "RoleBinding"),
        ] {
            for item in objects
                .list(api_version, kind, Some(namespace), None)
                .await?
            {
                entries.insert(format!(
                    "{kind}/{namespace}/{}/{}",
                    text(&item, "/metadata/name"),
                    text(&item, "/metadata/uid")
                ));
            }
        }
    }
    Ok(entries)
}

/// `(apiVersion, kind, namespace, name)` of a recorded identity key such as
/// `rbac.authorization.k8s.io/v1/RoleBinding/<namespace>/<name>`.
fn identity_key(key: &str) -> Result<(String, String, String, String)> {
    let parts = key.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["v1", kind, namespace, name] => Ok((
            "v1".into(),
            (*kind).into(),
            (*namespace).into(),
            (*name).into(),
        )),
        [group, version, kind, namespace, name] => Ok((
            format!("{group}/{version}"),
            (*kind).into(),
            (*namespace).into(),
            (*name).into(),
        )),
        _ => Err(acceptance::Error::Check(format!(
            "recorded identity key {key} is invalid"
        ))),
    }
}

/// A progress line on stderr, so a CI log shows which step a run reached.
fn note(step: &str) {
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
