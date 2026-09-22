//! One cluster run, from offline checks to the written report.
//!
//! Everything that can be checked without a cluster is checked first. Once
//! the cluster exists, any failure still ends with its logs and diagnostics
//! collected, the cluster deleted and a report written; the error is recorded
//! in that report.

use crate::{
    Error, FIXTURE_IMAGE, NODE_IMAGE, PROXY_IMAGE, Result,
    cases::{Live, mutation_cases},
    cluster::{self, Cluster, Disposition},
    contract::{FixtureRelease, ROLLOUT_KEY},
    crds::{self, CrdManifest},
    driver::{Driver, wait_for},
    images::{self, ArchiveImage},
    input, manifests,
    manifests::{CONTROLLER, OUTSIDE, SENTINEL, SENTINEL_PREFLIGHT},
    report::{self, ArtifactIdentity, CanaryScan, CaseRow, Isolation, Outcome, Report},
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
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

const SETUP: Duration = Duration::from_secs(120);
/// The controller's token outlives the longest expected run.
const TOKEN_SECONDS: i64 = 3600;

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
}

/// Everything collected while the cluster exists.
#[derive(Default)]
struct Progress {
    artifact: ArtifactIdentity,
    isolation: Isolation,
    cases: Vec<CaseRow>,
    controller_log: String,
    egress_log: String,
    canaries: BTreeSet<String>,
    diagnostics: Vec<(String, Value)>,
    crd_installations: Vec<Value>,
}

/// Run every generated case of a fixture release on a disposable cluster and
/// write the report into `options.output`.
///
/// # Errors
/// Returns an error when the inputs are rejected before a cluster is created
/// or the output cannot be written. Failures on the cluster are recorded in
/// the returned report instead.
pub async fn run(options: &Options) -> Result<Report> {
    if options.output.exists() {
        return input("test output already exists; choose a fresh directory");
    }
    let release = FixtureRelease::load(&options.release)?;
    let crds = crds::snapshot(&options.crds, &release.plan)?;
    let semantic = compiler_core::run_semantic_tests(&release.plan)?;
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
    let fixture = images::inspect(&options.fixture_image, platform)?;
    let fixture_name = format!("{}:local", images::repository(FIXTURE_IMAGE)?);
    if !fixture.names.contains(&fixture_name) {
        return input(format!(
            "the fixture image archive must be a `docker image save` of {FIXTURE_IMAGE}"
        ));
    }

    fs::create_dir_all(&options.output)?;
    write_json(
        &options.output.join("semantic-results.json"),
        &serde_json::to_value(&semantic)?,
    )?;
    write_json(&options.output.join("test-seed.json"), &release.seed())?;
    for manifest in &crds {
        fs::write(options.output.join(&manifest.file), &manifest.raw)?;
    }

    let mut progress = Progress {
        artifact: ArtifactIdentity {
            mode: "fixture-substituted".into(),
            release_digest: release.release_digest.clone(),
            ir_digest: release.plan.ir_digest.clone(),
            production_ir_digest: release.production_ir_digest.clone(),
            overlay_digest: release.overlay_digest.clone(),
            controller_image: release.manifest.image.reference.clone(),
            controller_config_digest: controller.config.clone(),
            fixture_image_config_digest: fixture.config.clone(),
            proxy_image_config_digest: String::new(),
            node_image: NODE_IMAGE.into(),
        },
        canaries: initial_canaries(&release),
        ..Progress::default()
    };
    let (name, disposition, error) = match Cluster::create(options.keep_cluster).await {
        Ok(cluster) => {
            let name = cluster.name().to_owned();
            let outcome = exercise(
                &cluster,
                &release,
                &semantic,
                &crds,
                options,
                (&controller, &fixture),
                &mut progress,
            )
            .await;
            (Some(name), cluster.finish().await, outcome.err())
        }
        Err(error) => (None, Disposition::NotCreated, Some(error)),
    };
    let error = error.or_else(|| crds_unchanged(&crds).err());
    finish(
        options,
        progress,
        name,
        disposition,
        error.map(|error| error.to_string()),
    )
}

/// Everything that happens while the cluster exists.
async fn exercise(
    cluster: &Cluster,
    release: &FixtureRelease,
    semantic: &SemanticReport,
    crds: &[CrdManifest],
    options: &Options,
    (controller, fixture): (&ArchiveImage, &ArchiveImage),
    progress: &mut Progress,
) -> Result<()> {
    let driver = Driver::new(cluster.admin_client().await?);
    let prepared = prepare(
        cluster,
        release,
        semantic,
        crds,
        &driver,
        (controller, (fixture, &options.fixture_image)),
        progress,
    )
    .await;
    let (live, images) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            collect_diagnostics(&driver, release, progress).await;
            return Err(error);
        }
    };
    let outcome = async {
        start_controller(&live, &images, progress).await?;
        for case in &release.plan.test_plan {
            progress.cases.push(live.run_case(case).await);
        }
        for case in mutation_cases(release) {
            progress.cases.push(live.mutation_case(&case).await);
        }
        Ok::<_, Error>(())
    }
    .await;
    live.preserve_logs().await;
    progress.controller_log = live
        .logs
        .lock()
        .map(|logs| logs.join("\n"))
        .unwrap_or_default();
    progress.egress_log = live
        .egress_logs
        .lock()
        .map(|logs| logs.join("\n"))
        .unwrap_or_default();
    if let Ok(canaries) = live.canaries.lock() {
        progress.canaries.extend(canaries.iter().cloned());
    }
    collect_diagnostics(&driver, release, progress).await;
    outcome
}

/// Image references the Pods use, after import.
struct Images {
    controller: String,
    proxy: String,
}

async fn prepare<'a>(
    cluster: &Cluster,
    release: &'a FixtureRelease,
    semantic: &SemanticReport,
    crds: &[CrdManifest],
    driver: &Driver,
    (controller, (fixture, fixture_archive)): (&ArchiveImage, (&ArchiveImage, &Path)),
    progress: &mut Progress,
) -> Result<(Live<'a>, Images)> {
    let domain = &release.plan.ir.domain;
    require_node_platform(driver, &release.manifest.image.platform).await?;
    let images = import_images(
        cluster,
        release,
        controller,
        (fixture, fixture_archive),
        progress,
    )
    .await?;

    for namespace in [
        &domain.namespace,
        &domain.control_namespace,
        &OUTSIDE.to_owned(),
    ] {
        driver.put(&manifests::namespace(namespace)).await?;
    }
    install_crds(driver, crds, progress).await?;

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
        .token(&domain.control_namespace, CONTROLLER, TOKEN_SECONDS)
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

    let api_cidr = isolate_network(
        driver,
        &domain.namespace,
        &domain.control_namespace,
        &images.proxy,
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
        semantic: semantic
            .results
            .iter()
            .map(|row| {
                (
                    (row.case.reaction_id.clone(), row.case.scenario.clone()),
                    row.clone(),
                )
            })
            .collect(),
        logs: Mutex::default(),
        egress_logs: Mutex::default(),
        canaries: Mutex::default(),
    };
    Ok((live, images))
}

/// Import the controller, workload fixture and proxy images into the node
/// and confirm the CRI resolves each to the archived image.
async fn import_images(
    cluster: &Cluster,
    release: &FixtureRelease,
    controller: &ArchiveImage,
    (fixture, fixture_archive): (&ArchiveImage, &Path),
    progress: &mut Progress,
) -> Result<Images> {
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
    progress.artifact.proxy_image_config_digest = proxy.config;
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

/// Deny all egress from the workload and control namespaces, allow the
/// controller only the API server, and prove the sentinel the canaries probe
/// is reachable when no policy applies. Returns the API server's CIDR.
async fn isolate_network(
    driver: &Driver,
    workload: &str,
    control: &str,
    proxy: &str,
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
    driver
        .put(&manifests::sentinel_preflight(proxy, &sentinel_ip))
        .await?;
    wait_for("sentinel reachable without a policy", SETUP, async || {
        let pod = driver
            .require("v1", "Pod", Some(OUTSIDE), SENTINEL_PREFLIGHT)
            .await?;
        match pod["status"]["phase"].as_str() {
            Some("Succeeded") => Ok(Some(())),
            Some("Failed") => Err(Error::Infrastructure(
                "the sentinel is unreachable even without a policy".into(),
            )),
            _ => Ok(None),
        }
    })
    .await?;
    let api = driver
        .require("v1", "Service", Some("default"), "kubernetes")
        .await?;
    let address = api["spec"]["clusterIP"].as_str().unwrap_or_default();
    let api_cidr = if address.contains(':') {
        format!("{address}/128")
    } else {
        format!("{address}/32")
    };
    for namespace in [workload, control] {
        driver.put(&manifests::deny_all_egress(namespace)).await?;
    }
    driver
        .put(&manifests::controller_api_egress(control, &api_cidr, true))
        .await?;
    driver.put(&manifests::proxy_script(control)).await?;
    Ok(api_cidr)
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
    live.assert_proxy_canaries().await?;
    progress.isolation.egress_denied = true;
    live.establish_baseline().await
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

/// The rollout key and every synthetic Secret value the fixtures seed.
fn initial_canaries(release: &FixtureRelease) -> BTreeSet<String> {
    let mut canaries = BTreeSet::from([ROLLOUT_KEY.to_owned()]);
    for input in release
        .plan
        .ir
        .inputs
        .iter()
        .filter(|input| input.kind == "Secret" && input.mode == InputMode::Observed)
    {
        let values = release
            .fixture(&input.id)
            .and_then(|fixture| fixture["data"].as_object())
            .map(|data| {
                data.values()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for value in values {
            canaries.insert(manifests::encode("Secret", &value));
            canaries.insert(value);
        }
    }
    canaries
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

fn finish(
    options: &Options,
    progress: Progress,
    cluster_name: Option<String>,
    disposition: Disposition,
    error: Option<String>,
) -> Result<Report> {
    let output = &options.output;
    let diagnostics = output.join("diagnostics");
    fs::create_dir_all(&diagnostics)?;
    let mut documents = vec![progress.controller_log.clone(), progress.egress_log.clone()];
    for (name, items) in &progress.diagnostics {
        let text = serde_json::to_string_pretty(items)?;
        fs::write(diagnostics.join(format!("{name}.json")), &text)?;
        documents.push(text);
    }
    documents.push(serde_json::to_string(&progress.cases)?);
    let borrowed = documents.iter().map(String::as_str).collect::<Vec<_>>();
    let occurrences = report::canary_occurrences(&borrowed, &progress.canaries);
    let scan = CanaryScan {
        canaries: progress.canaries.len(),
        documents_scanned: documents.len(),
        occurrences,
        controller_log_bytes: progress.controller_log.len(),
        passed: occurrences == 0 && !progress.controller_log.trim().is_empty(),
    };
    fs::write(output.join("controller.log"), &progress.controller_log)?;
    fs::write(output.join("egress-canary.log"), &progress.egress_log)?;
    write_json(
        &output.join("canary-scan.json"),
        &serde_json::to_value(&scan)?,
    )?;
    if !progress.crd_installations.is_empty() {
        write_json(
            &output.join("fixture-crds.json"),
            &json!({"installations": progress.crd_installations}),
        )?;
    }
    let report = report::shape(Outcome {
        artifact: progress.artifact,
        cluster_name,
        disposition,
        isolation: progress.isolation,
        scan,
        cases: progress.cases,
        error,
    });
    write_json(
        &output.join("results.json"),
        &serde_json::to_value(&report)?,
    )?;
    fs::write(output.join("junit.xml"), report::junit(&report))?;
    Ok(report)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(path, bytes)?;
    Ok(())
}
