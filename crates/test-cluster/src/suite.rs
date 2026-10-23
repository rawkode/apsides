//! One cluster run, from offline checks to the written report.
//!
//! Everything that can be checked without a cluster is checked first. Once
//! the cluster exists, any failure or interruption still ends with its logs
//! and diagnostics collected, the cluster deleted and a report written; the
//! error is recorded in that report.

use crate::{
    Error, FIXTURE_IMAGE, NODE_IMAGE, PROXY_IMAGE, Result,
    cases::{Live, Record, mutation_cases},
    cluster::{self, Cluster, Disposition, POD_SUBNET, SERVICE_SUBNET},
    contract::{FixtureRelease, ROLLOUT_KEY},
    crds::{self, CrdManifest},
    driver::{Driver, wait_for},
    images::{self, ArchiveImage},
    input, manifests,
    manifests::{CONTROLLER, OUTSIDE, SENTINEL, SENTINEL_PREFLIGHT},
    offline::{self, PodProbe},
    report::{
        self, ArtifactIdentity, CanaryScan, CaseRow, CaseStatus, ClusterReport, Isolation,
        NetworkProbe, Outcome, Reach, Report, Runner,
    },
};
use compiler_core::{InputMode, SemanticReport};
use k8s_openapi::api::{
    authentication::v1::{SelfSubjectReview, SelfSubjectReviewStatus},
    authorization::v1::{ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec},
    core::v1::Node,
};
use kube::{Api, api::PostParams};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const SETUP: Duration = Duration::from_secs(120);
/// Bounds of the controller's token lifetime. The API server refuses less
/// than ten minutes.
const MIN_TOKEN_SECONDS: i64 = 600;
const MAX_TOKEN_SECONDS: i64 = 3600;
/// Token lifetime granted per case, beyond the minimum.
const TOKEN_SECONDS_PER_CASE: i64 = 30;
/// The run must end this long before the controller's token expires.
const TOKEN_MARGIN_SECONDS: i64 = 60;
/// How long an interrupted run spends collecting logs before cleanup.
const SALVAGE: Duration = Duration::from_secs(20);

/// What to test and where to write the results.
#[derive(Clone, Debug)]
pub struct Options {
    /// A fixture release directory, as `aps release` assembles it from an
    /// `aps build --mode fixture` artifact.
    pub release: PathBuf,
    /// An OCI-layout archive of the workload fixture image, tagged
    /// `apsides-fixture:local`.
    pub fixture_image: PathBuf,
    /// CRD manifests for custom resources the program manages.
    pub crds: Vec<PathBuf>,
    /// A fresh directory for the report and diagnostics.
    pub output: PathBuf,
    /// Leave the cluster running for inspection.
    pub keep_cluster: bool,
    /// Receives one line per step and per case as the run proceeds.
    pub progress: Option<fn(&str)>,
}

impl Options {
    fn note(&self, line: &str) {
        if let Some(progress) = self.progress {
            progress(line);
        }
    }
}

/// Everything collected while the cluster exists.
#[derive(Default)]
struct Progress {
    runner: Runner,
    artifact: ArtifactIdentity,
    isolation: Isolation,
    cluster_created: bool,
    cases: Vec<CaseRow>,
    /// Logs, canaries and egress output the cases recorded.
    record: Arc<Record>,
    /// Controller logs read after an interruption, beyond what was recorded.
    salvaged_log: String,
    canaries: BTreeSet<String>,
    diagnostics: Vec<(String, Value)>,
    crd_installations: Vec<Value>,
    /// Documents written before the cluster, scanned and redacted with the
    /// rest.
    semantic_results: String,
    seed: String,
}

impl Progress {
    /// What is known before the cluster exists.
    fn new(
        release: &FixtureRelease,
        semantic: &SemanticReport,
        controller: &ArchiveImage,
        fixture: &ArchiveImage,
    ) -> Result<Self> {
        let cases = release.plan.test_plan.len() + mutation_cases(release).len();
        Ok(Self {
            runner: Runner {
                aps_version: env!("CARGO_PKG_VERSION").into(),
                node_image: NODE_IMAGE.into(),
                proxy_image: PROXY_IMAGE.into(),
                ..Runner::default()
            },
            artifact: ArtifactIdentity {
                mode: "fixture-substituted".into(),
                release_digest: release.release_digest.clone(),
                ir_digest: release.plan.ir_digest.clone(),
                production_ir_digest: release.production_ir_digest.clone(),
                overlay_digest: release.overlay_digest.clone(),
                controller_image: release.manifest.image.reference.clone(),
                controller_config_digest: controller.config.clone(),
                fixture_image_config_digest: fixture.config.clone(),
            },
            isolation: Isolation {
                controller_token_seconds: token_seconds(cases),
                ..Isolation::default()
            },
            canaries: initial_canaries(release),
            semantic_results: pretty(&serde_json::to_value(semantic)?)?,
            seed: pretty(&release.seed())?,
            ..Self::default()
        })
    }
}

/// The checked inputs of one run.
struct Run<'a> {
    release: &'a FixtureRelease,
    semantic: &'a SemanticReport,
    crds: &'a [CrdManifest],
    options: &'a Options,
    controller: &'a ArchiveImage,
    fixture: &'a ArchiveImage,
}

/// How the cluster phase ended.
struct Ending {
    name: Option<String>,
    disposition: Disposition,
    kubeconfig: Option<PathBuf>,
    error: Option<String>,
    interrupted: bool,
}

/// Run every generated case of a fixture release on a disposable cluster and
/// write the report into `options.output`.
///
/// When `interrupt` completes, with the name of what interrupted the run, the
/// cases stop, logs are collected briefly, the cluster is deleted (unless
/// kept) and the report records the interruption.
///
/// # Errors
/// Returns an error when the inputs are rejected before a cluster is created
/// or the output cannot be written. Failures on the cluster are recorded in
/// the returned report instead.
pub async fn run(options: &Options, interrupt: impl Future<Output = String>) -> Result<Report> {
    if options.output.exists() {
        return input("test output already exists; choose a fresh directory");
    }
    let release = FixtureRelease::load(&options.release)?;
    let crds = crds::snapshot(&options.crds, &release.plan)?;
    let semantic = compiler_core::run_semantic_tests(&release.plan)?;
    let (controller, fixture) = inspect_images(&release, &options.fixture_image)?;

    fs::create_dir_all(&options.output)?;
    for manifest in &crds {
        fs::write(options.output.join(&manifest.file), &manifest.raw)?;
    }
    let mut progress = Progress::new(&release, &semantic, &controller, &fixture)?;

    options.note("checking Docker, kind and the node image");
    let toolchain = match cluster::preflight().await {
        Ok(toolchain) => toolchain,
        Err(error) => {
            let ending = Ending {
                name: None,
                disposition: Disposition::NotCreated,
                kubeconfig: None,
                error: Some(error.to_string()),
                interrupted: false,
            };
            return finish(options, progress, ending);
        }
    };
    progress.runner.kind_version = toolchain.kind_version;
    progress.runner.container_engine = toolchain.container_engine.to_owned();
    progress.runner.container_engine_version = toolchain.container_engine_version;
    progress.runner.node_image_id = toolchain.node_image_id;

    let cluster = Cluster::new(options.keep_cluster)?;
    let name = cluster.name().to_owned();
    options.note(&format!("creating kind cluster {name}"));
    let budget = Duration::from_secs(
        u64::try_from(progress.isolation.controller_token_seconds - TOKEN_MARGIN_SECONDS)
            .unwrap_or_default(),
    );
    let mut interrupt = std::pin::pin!(interrupt);
    let finished: std::result::Result<Result<()>, String> = tokio::select! {
        biased;
        signal = &mut interrupt => Err(signal),
        outcome = async {
            cluster.create().await?;
            progress.cluster_created = true;
            let run = Run {
                release: &release,
                semantic: &semantic,
                crds: &crds,
                options,
                controller: &controller,
                fixture: &fixture,
            };
            let exercised = exercise(&cluster, &run, &mut progress);
            tokio::time::timeout(budget, exercised).await.map_err(|_| {
                Error::Deadline(format!(
                    "the run exceeded {}s, the bound set by the controller's token lifetime",
                    budget.as_secs()
                ))
            })?
        } => Ok(outcome),
    };
    let (error, interrupted) = match finished {
        Ok(Ok(())) => (None, false),
        Ok(Err(error)) => (Some(error.to_string()), false),
        Err(signal) => (Some(format!("interrupted by {signal}")), true),
    };
    if interrupted {
        options.note("interrupted; collecting logs, then cleaning up the cluster");
        if progress.cluster_created {
            let _ = tokio::time::timeout(SALVAGE, salvage(&cluster, &release, &mut progress)).await;
        }
    }
    let (disposition, kubeconfig) = dispose(cluster, options, progress.cluster_created).await?;
    let error = error.or_else(|| crds_unchanged(&crds).err().map(|error| error.to_string()));
    let ending = Ending {
        name: Some(name),
        disposition,
        kubeconfig,
        error,
        interrupted,
    };
    finish(options, progress, ending)
}

/// The controller and workload fixture images, each checked against what the
/// release and the fixture IR name.
fn inspect_images(
    release: &FixtureRelease,
    fixture_archive: &Path,
) -> Result<(ArchiveImage, ArchiveImage)> {
    let platform = release.manifest.image.platform.as_str();
    let controller = images::inspect(&release.root.join("controller.oci.tar"), platform)?;
    if release
        .manifest
        .image
        .reference
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        != Some(controller.manifest.as_str())
    {
        return input("the controller archive does not hold the release's image");
    }
    let fixture = images::inspect(fixture_archive, platform)?;
    let fixture_name = format!("{}:local", images::repository(FIXTURE_IMAGE)?);
    if !fixture.names.contains(&fixture_name) {
        return input(format!(
            "the fixture image archive must be an OCI-layout save of {fixture_name}"
        ));
    }
    Ok((controller, fixture))
}

/// Keep the cluster, with its kubeconfig copied into the output, or delete
/// it.
async fn dispose(
    cluster: Cluster,
    options: &Options,
    created: bool,
) -> Result<(Disposition, Option<PathBuf>)> {
    let kubeconfig = if options.keep_cluster && created {
        let path = options.output.join("kubeconfig");
        cluster.export_kubeconfig(&path)?;
        Some(path)
    } else {
        None
    };
    options.note(if options.keep_cluster {
        "keeping the cluster"
    } else {
        "deleting the cluster"
    });
    Ok((cluster.finish().await, kubeconfig))
}

/// The controller's token lifetime: enough for every case, within the API
/// server's bounds.
fn token_seconds(cases: usize) -> i64 {
    let cases = i64::try_from(cases).unwrap_or(i64::MAX);
    MIN_TOKEN_SECONDS
        .saturating_add(cases.saturating_mul(TOKEN_SECONDS_PER_CASE))
        .clamp(MIN_TOKEN_SECONDS, MAX_TOKEN_SECONDS)
}

/// Everything that happens while the cluster exists.
async fn exercise(cluster: &Cluster, run: &Run<'_>, progress: &mut Progress) -> Result<()> {
    let (release, options) = (run.release, run.options);
    let driver = Driver::new(cluster.admin_client().await?);
    let prepared = prepare(cluster, run, &driver, progress).await;
    let (live, images, nodes) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            collect_diagnostics(&driver, release, progress).await;
            return Err(error);
        }
    };
    let outcome = async {
        options.note("starting the controller");
        start_controller(&live, &images, progress).await?;
        let mutations = mutation_cases(release);
        let total = release.plan.test_plan.len() + mutations.len();
        for (index, case) in release.plan.test_plan.iter().enumerate() {
            note_case(options, index, total, &case.reaction_id, &case.scenario);
            let row = live.run_case(case).await;
            note_result(options, &row);
            progress.cases.push(row);
        }
        for (index, case) in mutations.iter().enumerate() {
            let index = release.plan.test_plan.len() + index;
            note_case(options, index, total, &case.reaction_id, &case.scenario);
            let row = live.mutation_case(case).await;
            note_result(options, &row);
            progress.cases.push(row);
        }
        options.note("re-checking node egress after the cases");
        for node in &nodes {
            offline::installed(node).await?;
            progress
                .isolation
                .probes
                .push(offline::external_tcp("node-external-tcp-after-cases", node).await?);
        }
        Ok::<_, Error>(())
    }
    .await;
    live.preserve_logs().await;
    collect_diagnostics(&driver, release, progress).await;
    outcome
}

fn note_case(options: &Options, index: usize, total: usize, reaction: &str, scenario: &str) {
    options.note(&format!("[{}/{total}] {reaction} {scenario}", index + 1));
}

fn note_result(options: &Options, row: &CaseRow) {
    if row.status != CaseStatus::Passed {
        options.note(&format!(
            "  {}: {}",
            serde_json::to_value(row.status)
                .ok()
                .and_then(|status| status.as_str().map(str::to_owned))
                .unwrap_or_default(),
            row.detail
        ));
    }
}

/// Image references the Pods use, after import.
struct Images {
    controller: String,
    proxy: String,
}

async fn prepare<'a>(
    cluster: &Cluster,
    run: &Run<'a>,
    driver: &Driver,
    progress: &mut Progress,
) -> Result<(Live<'a>, Images, Vec<String>)> {
    let (release, options) = (run.release, run.options);
    let domain = &release.plan.ir.domain;
    require_node_platform(driver, &release.manifest.image.platform).await?;
    options.note("importing images into the node");
    let images = import_images(cluster, run, progress).await?;

    options.note("blocking egress at the node");
    let api_address = api_address(driver).await?;
    let nodes = block_node_egress(cluster, &api_address, progress).await?;

    for namespace in [
        &domain.namespace,
        &domain.control_namespace,
        &OUTSIDE.to_owned(),
    ] {
        driver.put(&manifests::namespace(namespace)).await?;
    }
    install_crds(driver, run.crds, progress).await?;

    driver
        .put(&manifests::service_account(
            &domain.control_namespace,
            CONTROLLER,
        ))
        .await?;
    let rules = manifests::capability_rules(&release.plan)?;
    for (namespace, namespace_rules) in &rules {
        driver
            .put(&manifests::role(namespace, namespace_rules))
            .await?;
        driver
            .put(&manifests::role_binding(
                namespace,
                &domain.control_namespace,
            ))
            .await?;
    }
    for account in manifests::workload_service_accounts(&release.plan) {
        driver.put(&account).await?;
    }
    for object in manifests::seed_inputs(release)? {
        driver.put(&object).await?;
    }

    let context = cluster.context();
    let token = driver
        .token(
            &domain.control_namespace,
            CONTROLLER,
            progress.isolation.controller_token_seconds,
        )
        .await?;
    progress.canaries.insert(token.clone());
    let endpoint = cluster.endpoint()?;
    let runtime = cluster::client(
        serde_json::from_value(endpoint.token_kubeconfig(&context, &token))?,
        &context,
    )
    .await?;
    require_separate_credential(&runtime, &domain.control_namespace).await?;
    progress.isolation.separate_credential = true;
    driver
        .put(&manifests::runtime_secret(
            &domain.control_namespace,
            &manifests::runtime_config(&release.plan, &context),
            &manifests::pod_kubeconfig(&endpoint.authority, &context, &token),
        ))
        .await?;

    options.note("isolating the namespaces and probing egress from Pods");
    let api_cidr = isolate_network(
        driver,
        (&domain.namespace, &domain.control_namespace),
        &images.proxy,
        &api_address,
        progress,
    )
    .await?;

    let live = Live {
        release,
        driver: driver.clone(),
        runtime,
        workload: domain.namespace.clone(),
        control: domain.control_namespace.clone(),
        domain_manager: format!("apsides-{}", domain.id),
        original_rules: rules.get(&domain.namespace).cloned().unwrap_or_default(),
        api_cidr,
        semantic: run
            .semantic
            .results
            .iter()
            .map(|row| {
                (
                    (row.case.reaction_id.clone(), row.case.scenario.clone()),
                    row.clone(),
                )
            })
            .collect(),
        record: Arc::clone(&progress.record),
    };
    Ok((live, images, nodes))
}

/// Install the egress rules in every node, then run the node canaries and
/// positive controls. Returns the nodes.
async fn block_node_egress(
    cluster: &Cluster,
    api_address: &str,
    progress: &mut Progress,
) -> Result<Vec<String>> {
    let nodes = cluster.nodes().await?;
    for node in &nodes {
        offline::block(node).await?;
    }
    progress.isolation.node_egress_blocked = true;
    for node in &nodes {
        let probes = offline::canaries(node, api_address).await?;
        progress.isolation.probes.extend(probes);
    }
    Ok(nodes)
}

/// The API server's Service address.
async fn api_address(driver: &Driver) -> Result<String> {
    let api = driver
        .require("v1", "Service", Some("default"), "kubernetes")
        .await?;
    api["spec"]["clusterIP"]
        .as_str()
        .filter(|address| !address.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Infrastructure("the kubernetes Service has no address".into()))
}

/// Import the controller, workload fixture and proxy images into the node
/// and confirm the CRI resolves each to the archived image.
async fn import_images(
    cluster: &Cluster,
    run: &Run<'_>,
    progress: &mut Progress,
) -> Result<Images> {
    let (release, controller, fixture) = (run.release, run.controller, run.fixture);
    let fixture_archive = run.options.fixture_image.as_path();
    let node = cluster.node();
    let platform = release.manifest.image.platform.as_str();
    let repository = images::repository(&release.manifest.image.reference)?;
    let archive = release.root.join("controller.oci.tar");
    images::import(&node, &archive, &repository, platform).await?;
    let controller_reference = format!("{repository}@{}", controller.manifest);
    images::require_resolves(&node, &controller_reference, &controller.config).await?;

    // Workloads name the fixture by the tag the fixture IR pins.
    let repository = images::repository(FIXTURE_IMAGE)?;
    images::import(&node, fixture_archive, &repository, platform).await?;
    images::require_resolves(&node, &format!("{repository}:local"), &fixture.config).await?;

    let archive = images::save(PROXY_IMAGE, cluster.directory(), "proxy.tar").await?;
    let proxy = images::inspect(&archive, platform)?;
    let repository = images::repository(PROXY_IMAGE)?;
    images::import(&node, &archive, &repository, platform).await?;
    let proxy_reference = format!("{repository}@{}", proxy.manifest);
    images::require_resolves(&node, &proxy_reference, &proxy.config).await?;
    progress.runner.proxy_image_config_digest = proxy.config;
    progress.isolation.images_imported = true;
    Ok(Images {
        controller: controller_reference,
        proxy: proxy_reference,
    })
}

async fn require_node_platform(driver: &Driver, platform: &str) -> Result<()> {
    let nodes: Api<Node> = Api::all(driver.client().clone());
    let architectures = nodes
        .list(&kube::api::ListParams::default())
        .await?
        .items
        .into_iter()
        .filter_map(|node| {
            node.status?
                .node_info
                .map(|info| format!("{}/{}", info.operating_system, info.architecture))
        })
        .collect::<BTreeSet<_>>();
    if architectures.len() != 1 || !architectures.contains(platform) {
        return Err(Error::Infrastructure(format!(
            "the release's controller is {platform} but the node is {architectures:?}"
        )));
    }
    Ok(())
}

/// The controller's token must authenticate as its own `ServiceAccount` and
/// must not be able to create objects outside its namespaces.
async fn require_separate_credential(runtime: &kube::Client, control: &str) -> Result<()> {
    let reviews: Api<SelfSubjectReview> = Api::all(runtime.clone());
    let review = reviews
        .create(&PostParams::default(), &SelfSubjectReview::default())
        .await?;
    let username = review
        .status
        .and_then(|status: SelfSubjectReviewStatus| status.user_info)
        .and_then(|user| user.username);
    let expected = format!("system:serviceaccount:{control}:{CONTROLLER}");
    if username.as_deref() != Some(expected.as_str()) {
        return Err(Error::Infrastructure(
            "the controller's kubeconfig does not authenticate as its own ServiceAccount".into(),
        ));
    }
    let access: Api<SelfSubjectAccessReview> = Api::all(runtime.clone());
    let decision = access
        .create(
            &PostParams::default(),
            &SelfSubjectAccessReview {
                spec: SelfSubjectAccessReviewSpec {
                    resource_attributes: Some(ResourceAttributes {
                        namespace: Some(OUTSIDE.into()),
                        verb: Some("create".into()),
                        resource: Some("configmaps".into()),
                        ..ResourceAttributes::default()
                    }),
                    ..SelfSubjectAccessReviewSpec::default()
                },
                ..SelfSubjectAccessReview::default()
            },
        )
        .await?;
    if decision.status.is_none_or(|status| status.allowed) {
        return Err(Error::Infrastructure(
            "the controller's credential can create objects outside its namespaces".into(),
        ));
    }
    Ok(())
}

async fn install_crds(
    driver: &Driver,
    crds: &[CrdManifest],
    progress: &mut Progress,
) -> Result<()> {
    for manifest in crds {
        for document in &manifest.documents {
            driver
                .apply(document, "aps-test-fixture-crds", true)
                .await?;
            let name = document["metadata"]["name"].as_str().unwrap_or_default();
            let installed = wait_for(&format!("CRD {name} established"), SETUP, async || {
                let crd = driver
                    .get(
                        "apiextensions.k8s.io/v1",
                        "CustomResourceDefinition",
                        None,
                        name,
                    )
                    .await?;
                Ok(crd.filter(|crd| {
                    crd["status"]["conditions"]
                        .as_array()
                        .is_some_and(|conditions| {
                            conditions.iter().any(|condition| {
                                condition["type"] == "Established" && condition["status"] == "True"
                            })
                        })
                }))
            })
            .await?;
            let served = document["spec"]["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|version| version["served"] == true)
                .map(|version| version["name"].clone())
                .collect::<Vec<_>>();
            progress.crd_installations.push(json!({
                "name": name, "uid": installed["metadata"]["uid"],
                "sourceFile": manifest.file, "sourceDigest": manifest.digest,
                "group": document["spec"]["group"], "plural": document["spec"]["names"]["plural"],
                "servedVersions": served,
            }));
        }
    }
    Ok(())
}

/// Deny all egress from the workload and control namespaces and allow the
/// controller only the API server. Probe Pods show the sentinel the
/// controller's canaries aim at is reachable without a policy, that DNS and
/// Services still work under the node's egress rules, and that external
/// addresses and names are unreachable from Pods with and without a policy.
/// Returns the API server's CIDR.
async fn isolate_network(
    driver: &Driver,
    (workload, control): (&str, &str),
    proxy: &str,
    api_address: &str,
    progress: &mut Progress,
) -> Result<String> {
    for object in manifests::sentinel(proxy) {
        driver.put(&object).await?;
    }
    wait_for("egress sentinel available", SETUP, async || {
        let deployment = driver
            .require("apps/v1", "Deployment", Some(OUTSIDE), SENTINEL)
            .await?;
        Ok((deployment["status"]["availableReplicas"]
            .as_i64()
            .unwrap_or_default()
            >= 1)
            .then_some(()))
    })
    .await?;
    let sentinel_ip = sentinel_ip(driver).await?;
    let before = [PodProbe::tcp(
        "pod-sentinel-before-policies",
        &sentinel_ip,
        8080,
        Reach::Reachable,
    )];
    let probes = probe_pod(driver, OUTSIDE, SENTINEL_PREFLIGHT, proxy, &before).await?;
    progress.isolation.probes.extend(probes);

    let api_cidr = if api_address.contains(':') {
        format!("{api_address}/128")
    } else {
        format!("{api_address}/32")
    };
    for namespace in [workload, control] {
        driver.put(&manifests::deny_all_egress(namespace)).await?;
    }
    driver
        .put(&manifests::controller_api_egress(control, &api_cidr, true))
        .await?;
    driver.put(&manifests::proxy_script(control)).await?;

    let outside = [
        PodProbe::tcp(
            "pod-sentinel-by-dns",
            &manifests::sentinel_host(),
            8080,
            Reach::Reachable,
        ),
        PodProbe::tcp("pod-api-server-service", api_address, 443, Reach::Reachable),
        PodProbe::tcp(
            "pod-external-tcp",
            offline::EXTERNAL_ADDRESS,
            443,
            Reach::Blocked,
        ),
        PodProbe::dns("pod-external-dns", offline::EXTERNAL_NAME, Reach::Blocked),
    ];
    let probes = probe_pod(driver, OUTSIDE, "offline-probe", proxy, &outside).await?;
    progress.isolation.probes.extend(probes);
    let isolated = [
        // kindnet applies a NetworkPolicy to a new Pod only once it has seen
        // it, so the denial is awaited; the node rules have no such gap.
        PodProbe::tcp("workload-pod-sentinel", &sentinel_ip, 8080, Reach::Blocked).settling(),
        PodProbe::tcp(
            "workload-pod-external-tcp",
            offline::EXTERNAL_ADDRESS,
            443,
            Reach::Blocked,
        ),
    ];
    let probes = probe_pod(driver, workload, "offline-probe", proxy, &isolated).await?;
    progress.isolation.probes.extend(probes);
    Ok(api_cidr)
}

/// Run `probes` from a one-shot Pod in `namespace`, then remove it.
async fn probe_pod(
    driver: &Driver,
    namespace: &str,
    name: &str,
    image: &str,
    probes: &[PodProbe],
) -> Result<Vec<NetworkProbe>> {
    let script = offline::pod_script(probes);
    driver
        .put(&manifests::probe_pod(namespace, name, image, &script))
        .await?;
    wait_for(
        &format!("probe Pod {namespace}/{name}"),
        SETUP,
        async || {
            let pod = driver.require("v1", "Pod", Some(namespace), name).await?;
            match pod["status"]["phase"].as_str() {
                Some("Succeeded") => Ok(Some(())),
                Some("Failed") => Err(Error::Infrastructure(format!(
                    "probe Pod {namespace}/{name} failed"
                ))),
                _ => Ok(None),
            }
        },
    )
    .await?;
    let logs = driver.logs(namespace, name, "probe", false).await;
    let _ = driver.delete("v1", "Pod", Some(namespace), name).await;
    Ok(offline::pod_results(
        &logs,
        &format!("Pod {namespace}/{name}"),
        probes,
    ))
}

async fn start_controller(live: &Live<'_>, images: &Images, progress: &mut Progress) -> Result<()> {
    let sentinel_ip = sentinel_ip(&live.driver).await?;
    live.driver
        .put(&manifests::controller_deployment(
            &live.control,
            &images.controller,
            &images.proxy,
            &sentinel_ip,
        ))
        .await?;
    wait_for("controller Pod running", SETUP, async || {
        Ok(live.controller_pod().await?.map(|_| ()))
    })
    .await?;
    let probes = live.assert_proxy_canaries().await?;
    progress.isolation.probes.extend(probes);
    live.establish_baseline().await
}

/// After an interruption: whatever controller logs and object state can be
/// read quickly, with a fresh client.
async fn salvage(cluster: &Cluster, release: &FixtureRelease, progress: &mut Progress) {
    let Ok(client) = cluster.admin_client().await else {
        return;
    };
    let driver = Driver::new(client);
    let control = &release.plan.ir.domain.control_namespace;
    if let Ok(pods) = driver.pods(control, &format!("app={CONTROLLER}")).await {
        for pod in pods.iter().filter_map(|pod| pod.metadata.name.as_deref()) {
            for previous in [true, false] {
                let log = driver.logs(control, pod, "controller", previous).await;
                if !log.trim().is_empty() {
                    progress.salvaged_log.push_str(&log);
                    progress.salvaged_log.push('\n');
                }
            }
        }
    }
    collect_diagnostics(&driver, release, progress).await;
}

/// Object state from both namespaces, never Secrets.
async fn collect_diagnostics(driver: &Driver, release: &FixtureRelease, progress: &mut Progress) {
    let domain = &release.plan.ir.domain;
    for namespace in [&domain.namespace, &domain.control_namespace] {
        for (api_version, kind) in [
            ("v1", "Pod"),
            ("apps/v1", "Deployment"),
            ("v1", "ConfigMap"),
            ("v1", "Event"),
            ("networking.k8s.io/v1", "NetworkPolicy"),
        ] {
            if let Ok(items) = driver.list(api_version, kind, namespace).await {
                progress
                    .diagnostics
                    .push((format!("{namespace}-{}", kind.to_lowercase()), items));
            }
        }
    }
}

/// The rollout key, every synthetic Secret value the fixtures seed, and
/// every value a managed Secret declares, in plain and stored form.
fn initial_canaries(release: &FixtureRelease) -> BTreeSet<String> {
    use base64::Engine as _;
    let mut canaries = BTreeSet::from([ROLLOUT_KEY.to_owned()]);
    let strings = |data: &Value| {
        data.as_object()
            .map(|data| {
                data.values()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    for input in release
        .plan
        .ir
        .inputs
        .iter()
        .filter(|input| input.kind == "Secret" && input.mode == InputMode::Observed)
    {
        let values = release
            .fixture(&input.id)
            .map(|fixture| strings(&fixture["data"]))
            .unwrap_or_default();
        for value in values {
            canaries.insert(manifests::encode("Secret", &value));
            canaries.insert(value);
        }
    }
    for resource in release
        .plan
        .ir
        .resources
        .iter()
        .filter(|resource| resource.kind == "Secret")
    {
        for value in strings(&resource.desired["stringData"]) {
            canaries.insert(manifests::encode("Secret", &value));
            canaries.insert(value);
        }
        for value in strings(&resource.desired["data"]) {
            if let Ok(plain) = base64::engine::general_purpose::STANDARD.decode(&value)
                && let Ok(plain) = String::from_utf8(plain)
            {
                canaries.insert(plain);
            }
            canaries.insert(value);
        }
    }
    canaries.retain(|canary| !canary.is_empty());
    canaries
}

async fn sentinel_ip(driver: &Driver) -> Result<String> {
    let service = driver
        .require("v1", "Service", Some(OUTSIDE), SENTINEL)
        .await?;
    service["spec"]["clusterIP"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Infrastructure("the sentinel Service has no address".into()))
}

fn crds_unchanged(crds: &[CrdManifest]) -> Result<()> {
    use sha2::Digest as _;
    for manifest in crds {
        let bytes = fs::read(&manifest.source)?;
        if format!("sha256:{:x}", sha2::Sha256::digest(&bytes)) != manifest.digest {
            return Err(Error::Input("a CRD manifest changed during the run".into()));
        }
    }
    Ok(())
}

/// Scan everything the run wrote or reports for canaries, redact any found,
/// and write the report.
fn finish(options: &Options, mut progress: Progress, ending: Ending) -> Result<Report> {
    let output = &options.output;
    let diagnostics = output.join("diagnostics");
    fs::create_dir_all(&diagnostics)?;
    let record = std::mem::take(&mut progress.record);
    let joined =
        |logs: &Mutex<Vec<String>>| logs.lock().map(|logs| logs.join("\n")).unwrap_or_default();
    let controller_log = format!("{}\n{}", joined(&record.logs), progress.salvaged_log)
        .trim()
        .to_owned();
    let egress_log = joined(&record.egress_logs);
    if let Ok(canaries) = record.canaries.lock() {
        progress.canaries.extend(canaries.iter().cloned());
    }
    let (canaries, too_short) = report::searchable(&progress.canaries);

    let mut files = vec![
        (output.join("controller.log"), controller_log.clone()),
        (output.join("egress-canary.log"), egress_log),
        (
            output.join("semantic-results.json"),
            progress.semantic_results.clone(),
        ),
        (output.join("test-seed.json"), progress.seed.clone()),
    ];
    for (name, items) in &progress.diagnostics {
        files.push((diagnostics.join(format!("{name}.json")), pretty(items)?));
    }
    if !progress.crd_installations.is_empty() {
        files.push((
            output.join("fixture-crds.json"),
            pretty(&json!({"installations": progress.crd_installations}))?,
        ));
    }
    let reported = [
        serde_json::to_string(&progress.cases)?,
        serde_json::to_string(&progress.isolation)?,
        ending.error.clone().unwrap_or_default(),
    ];
    let documents = files
        .iter()
        .map(|(_, text)| text.as_str())
        .chain(reported.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let occurrences = report::canary_occurrences(&documents, &canaries);
    let scan = CanaryScan {
        canaries: canaries.len(),
        canaries_too_short: too_short,
        documents_scanned: documents.len(),
        occurrences,
        controller_log_bytes: controller_log.len(),
        passed: occurrences == 0 && !controller_log.is_empty(),
    };
    for (path, text) in &files {
        fs::write(path, report::redact(text, &canaries))?;
    }
    write_json(
        &output.join("canary-scan.json"),
        &serde_json::to_value(&scan)?,
    )?;

    for row in &mut progress.cases {
        row.detail = report::redact(&row.detail, &canaries);
    }
    let report = report::shape(Outcome {
        runner: progress.runner,
        artifact: progress.artifact,
        cluster: ClusterReport {
            name: ending.name,
            disposition: ending.disposition,
            pod_subnet: POD_SUBNET,
            service_subnet: SERVICE_SUBNET,
            kubeconfig: ending.kubeconfig,
        },
        isolation: progress.isolation,
        scan,
        cases: progress.cases,
        error: ending.error.map(|error| report::redact(&error, &canaries)),
        interrupted: ending.interrupted,
    });
    fs::write(
        output.join("results.json"),
        report::redact(&pretty(&serde_json::to_value(&report)?)?, &canaries),
    )?;
    fs::write(
        output.join("junit.xml"),
        report::redact(&report::junit(&report), &canaries),
    )?;
    Ok(report)
}

fn pretty(value: &Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    Ok(text)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::write(path, pretty(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_outlives_the_cases_within_the_api_server_bounds() {
        assert_eq!(token_seconds(0), MIN_TOKEN_SECONDS);
        assert_eq!(token_seconds(46), 600 + 46 * 30);
        assert_eq!(token_seconds(10_000), MAX_TOKEN_SECONDS);
        assert!(token_seconds(1) > TOKEN_MARGIN_SECONDS);
    }
}
