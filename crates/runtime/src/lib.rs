#![forbid(unsafe_code)]
//! Native execution of the finite Apsides IR.
//!
//! This crate deliberately has no JavaScript engine. Production execution uses
//! a fixed in-cluster identity and protected platform activation; fixture
//! execution remains a separately acknowledged disposable-cluster mode.

mod handlers;
mod production;

pub use production::{ApiAccess, ProductionConfig};

use std::io::Write as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Read,
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use compiler_core::{
    BlockReason, CompiledPlan, Input, InputMode, InputSnapshot, REVISION_ANNOTATION, Resource,
    RolloutKey, RolloutOutcome, evaluate_rollout, parse_and_validate,
};
use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use kube::{
    Api, Client, Config,
    api::{ApiResource, DynamicObject, ListParams, Patch, PatchParams, ValidationDirective},
    config::{KubeConfigOptions, Kubeconfig},
    core::GroupVersionKind,
    runtime::watcher,
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const CONFIG_PATH_ENV: &str = "APSIDES_RUNTIME_CONFIG_PATH";
pub const FIXTURE_ACKNOWLEDGEMENT: &str = "disposable-cluster-only";
const STARTUP_CONFIG_MAX_BYTES: u64 = 64 * 1024;
const KUBECONFIG_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime output failed: {0}")]
    Output(#[from] std::io::Error),
    #[error("runtime is terminating")]
    Terminating,
    #[error("runtime configuration error: {0}")]
    Configuration(String),
    #[error("runtime plan error: {0}")]
    Plan(String),
    #[error("Kubernetes operation failed: {0}")]
    Kubernetes(String),
    #[error(transparent)]
    Platform(#[from] platform::Error),
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
enum StartupConfig {
    Production(ProductionConfig),
    Fixture(FixtureConfig),
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FixtureConfig {
    pub acknowledgement: String,
    pub ir_digest: String,
    pub cluster_binding: String,
    pub kubeconfig: PathBuf,
    pub context: String,
    pub poll_interval_millis: u64,
    pub max_watch_cycles: usize,
    #[serde(default)]
    pub rollout_key: Option<FixtureRolloutKey>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FixtureRolloutKey {
    pub key_id: String,
    /// Test-only UTF-8 material. The production runtime never reads this; it
    /// loads the granted key from the protected control-namespace Secret.
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    Available {
        data: BTreeMap<String, String>,
        resource_version: String,
    },
    Missing,
    Unauthorized,
    Unavailable(String),
    Invalid(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub applied: Vec<String>,
    pub blocked: Vec<BlockedTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedTarget {
    pub target: String,
    pub inputs: Vec<String>,
    pub reason: String,
}

#[async_trait]
pub trait RuntimeClient: Send + Sync {
    async fn apply_ssa(&self, resource: &Resource, desired: &Value) -> Result<(), RuntimeError>;
    async fn observe_named(&self, input: &Input) -> Result<Observation, RuntimeError>;
    async fn wait_for_named_change(
        &self,
        inputs: &[Input],
        timeout: Duration,
    ) -> Result<(), RuntimeError>;
}

/// Entrypoint used by generated native controllers.
///
/// The sole startup input is a path to an explicit mode document. Production
/// accepts only an explicit in-cluster transport configuration. Fixture mode
/// requires a dedicated absolute kubeconfig path.
///
/// `handlers` is the `(id, bundle source)` pair for every declared event
/// handler, embedded by the generated controller via `include_str!` next to
/// its `ir.json`. In production mode these are wired into the `engine`
/// crate's V8 isolate and actually run (see `handlers.rs` and the Apsides
/// v0.3 plan, Phase 5b); fixture mode still only records that the embedded
/// bundles match the validated IR (`verify_embedded_handlers`, above) and
/// does not execute them.
///
/// # Errors
///
/// Returns an error for invalid IR or startup configuration, failed protected
/// activation, or a Kubernetes operation failure.
pub async fn run(
    ir_json: &'static str,
    handlers: &'static [(&'static str, &'static str)],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let plan = parse_and_validate(ir_json)?;
    verify_embedded_handlers(&plan, handlers)?;
    let path = env::var(CONFIG_PATH_ENV)
        .map_err(|_| RuntimeError::Configuration(format!("{CONFIG_PATH_ENV} is required")))?;
    let startup = read_startup(Path::new(&path))?;
    match startup {
        StartupConfig::Production(config) => {
            production::run_production(&plan, config, handlers).await?;
            Ok(())
        }
        StartupConfig::Fixture(config) => {
            validate_fixture_config(&plan, &config)?;
            install_rustls_provider()?;
            let client = KubeRuntimeClient::from_fixture_config(
                &config,
                plan.ir.domain.budgets.max_concurrent_requests,
                &plan.ir.domain.id,
            )
            .await?;
            let report = run_fixture_with_client(&plan, &config, &client).await?;
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string(&report)
                    .map_err(|error| RuntimeError::Plan(error.to_string()))?
            )?;
            Ok(())
        }
    }
}

/// Cross-check the embedded `(id, bundle source)` pairs against the
/// validated IR's own declared handlers: no duplicate id, the id sets match
/// exactly, and each bundle's actual sha256 matches the IR's own recorded
/// `bundleSha256`. Cheap (one hash per handler, at most `MAX_HANDLERS` of
/// them), and it makes the image self-checking: a build that embedded the
/// wrong bundle, dropped one, or duplicated an id fails closed at startup
/// rather than silently running mismatched code once a later phase adds
/// execution.
fn verify_embedded_handlers(
    plan: &CompiledPlan,
    handlers: &[(&'static str, &'static str)],
) -> Result<(), RuntimeError> {
    let mut seen = std::collections::BTreeSet::new();
    for (id, source) in handlers {
        if !seen.insert(*id) {
            return Err(RuntimeError::Plan(format!(
                "embedded handler bundles contain a duplicate id: {id}"
            )));
        }
        let declared = plan
            .ir
            .handlers
            .iter()
            .find(|handler| handler.id == *id)
            .ok_or_else(|| {
                RuntimeError::Plan(format!(
                    "embedded handler bundle {id} is not declared in the validated IR"
                ))
            })?;
        let actual = format!("{:x}", Sha256::digest(source.as_bytes()));
        if actual != declared.bundle_sha256 {
            return Err(RuntimeError::Plan(format!(
                "embedded handler bundle {id} does not match its declared bundleSha256"
            )));
        }
    }
    if seen.len() != plan.ir.handlers.len() {
        return Err(RuntimeError::Plan(
            "embedded handler bundles do not match the validated IR's declared handlers".into(),
        ));
    }
    Ok(())
}

pub(crate) fn install_rustls_provider() -> Result<(), RuntimeError> {
    platform::install_rustls_ring_provider()
        .map_err(|error| RuntimeError::Configuration(error.to_string()))
}

pub(crate) fn strict_apply_params(manager: &str, force: bool) -> PatchParams {
    let params = PatchParams::apply(manager).validation(ValidationDirective::Strict);
    if force { params.force() } else { params }
}

fn read_startup(path: &Path) -> Result<StartupConfig, RuntimeError> {
    if !path.is_absolute() {
        return Err(RuntimeError::Configuration(
            "runtime config path must be absolute".into(),
        ));
    }
    let bytes = read_bounded_file(path, STARTUP_CONFIG_MAX_BYTES, "runtime config")?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| RuntimeError::Configuration("runtime config is not UTF-8".into()))?;
    let value = compiler_core::parse_strict(text)
        .map_err(|error| RuntimeError::Configuration(error.to_string()))?;
    serde_json::from_value(value).map_err(|error| RuntimeError::Configuration(error.to_string()))
}

fn read_bounded_file(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>, RuntimeError> {
    let file = fs::File::open(path)
        .map_err(|error| RuntimeError::Configuration(format!("{label}: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| RuntimeError::Configuration(format!("{label}: {error}")))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(RuntimeError::Configuration(format!(
            "{label} must be a regular file no larger than {limit} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| RuntimeError::Configuration(format!("{label}: {error}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(RuntimeError::Configuration(format!(
            "{label} exceeds {limit} bytes"
        )));
    }
    Ok(bytes)
}

fn validate_fixture_config(
    plan: &CompiledPlan,
    config: &FixtureConfig,
) -> Result<(), RuntimeError> {
    if config.acknowledgement != FIXTURE_ACKNOWLEDGEMENT {
        return Err(RuntimeError::Configuration(
            "fixture acknowledgement is absent or invalid".into(),
        ));
    }
    if config.ir_digest != plan.ir_digest {
        return Err(RuntimeError::Configuration(
            "fixture IR digest does not match the validated embedded IR".into(),
        ));
    }
    if config.cluster_binding != plan.ir.domain.cluster_binding || config.context.trim().is_empty()
    {
        return Err(RuntimeError::Configuration(
            "fixture cluster binding does not match the IR or context is empty".into(),
        ));
    }
    if !config.kubeconfig.is_absolute() {
        return Err(RuntimeError::Configuration(
            "fixture kubeconfig path must be absolute".into(),
        ));
    }
    if !(100..=60_000).contains(&config.poll_interval_millis)
        || !(1..=1_000_000).contains(&config.max_watch_cycles)
    {
        return Err(RuntimeError::Configuration(
            "fixture polling bounds are invalid".into(),
        ));
    }
    if config
        .rollout_key
        .as_ref()
        .is_some_and(|key| key.key_id.is_empty() || key.value.len() < 32)
    {
        return Err(RuntimeError::Configuration(
            "fixture rollout key requires a nonempty key id and at least 32 bytes".into(),
        ));
    }
    Ok(())
}

/// Runs the finite plan in an explicitly bounded disposable fixture session.
///
/// # Errors
///
/// Returns an error when fixture bindings are invalid or reconciliation cannot
/// complete a required Kubernetes operation.
pub async fn run_fixture_with_client<C: RuntimeClient>(
    plan: &CompiledPlan,
    config: &FixtureConfig,
    client: &C,
) -> Result<RunReport, RuntimeError> {
    validate_fixture_config(plan, config)?;
    let mut report = reconcile_once(plan, config.rollout_key.as_ref(), client).await?;
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        serde_json::json!({"domain":plan.ir.domain.id,"build":plan.ir_digest,"status":report})
    );
    for _ in 0..config.max_watch_cycles {
        let timeout = Duration::from_millis(config.poll_interval_millis);
        // A watch close, expiration, or timeout is only a notification. Every
        // cycle re-lists and exact-gets current state in reconcile_once.
        if client
            .wait_for_named_change(&plan.ir.inputs, timeout)
            .await
            .is_err()
        {
            tokio::time::sleep(timeout).await;
        }
        let next = reconcile_once(plan, config.rollout_key.as_ref(), client).await?;
        if next != report {
            let _ = writeln!(
                std::io::stderr().lock(),
                "{}",
                serde_json::json!({"domain":plan.ir.domain.id,"build":plan.ir_digest,"status":next})
            );
        }
        report = next;
    }
    Ok(report)
}

/// Computes and applies one level-based reconciliation from current inputs.
///
/// # Errors
///
/// Returns an error for inconsistent plan data or a failed Kubernetes
/// operation. Unavailable declared inputs are represented as blocked targets.
pub async fn reconcile_once<C: RuntimeClient>(
    plan: &CompiledPlan,
    key: Option<&FixtureRolloutKey>,
    client: &C,
) -> Result<RunReport, RuntimeError> {
    let mut applied = Vec::new();
    let mut blocked = Vec::new();
    let mut successfully_applied = BTreeSet::new();

    // Authoritative managed inputs must land before their values can
    // contribute to a rollout. Other targets are applied after observations.
    for resource in plan
        .ir
        .resources
        .iter()
        .filter(|resource| resource.kind == "ConfigMap")
    {
        if let Err(error) = client.apply_ssa(resource, &resource.desired).await {
            blocked.push(write_blocked(resource, error)?);
            continue;
        }
        successfully_applied.insert(resource.id.clone());
        applied.push(resource.id.clone());
    }

    let observed = collect_observations(plan, &successfully_applied, client).await?;

    for resource in plan
        .ir
        .resources
        .iter()
        .filter(|resource| resource.kind != "ConfigMap")
    {
        let reactions = plan
            .ir
            .reactions
            .iter()
            .filter(|reaction| reaction.target == resource.id)
            .collect::<Vec<_>>();
        let mut desired = resource.desired.clone();
        if !reactions.is_empty() {
            let input_ids = reactions
                .iter()
                .flat_map(|reaction| reaction.inputs.iter().cloned())
                .collect::<BTreeSet<_>>();
            let snapshots = observed
                .iter()
                .map(|(id, observation)| {
                    let snapshot = match observation {
                        Observation::Available { data, .. } => InputSnapshot::Available {
                            data: Value::Object(
                                data.iter()
                                    .map(|(name, value)| {
                                        (name.clone(), Value::String(value.clone()))
                                    })
                                    .collect(),
                            ),
                            authoritative_applied: plan
                                .ir
                                .inputs
                                .iter()
                                .find(|input| input.id == *id)
                                .is_none_or(|input| {
                                    matches!(input.mode, InputMode::Observed | InputMode::Output)
                                        || input.managed_resource_id.as_ref().is_some_and(
                                            |resource| successfully_applied.contains(resource),
                                        )
                                }),
                        },
                        Observation::Missing => InputSnapshot::Missing,
                        Observation::Unauthorized => InputSnapshot::Unauthorized,
                        Observation::Unavailable(_) => InputSnapshot::Unavailable,
                        Observation::Invalid(_) => InputSnapshot::Invalid,
                    };
                    (id.clone(), snapshot)
                })
                .collect::<BTreeMap<_, _>>();
            let rollout_key = key.map(|key| RolloutKey {
                id: key.key_id.clone(),
                bytes: key.value.as_bytes().to_vec(),
            });
            match evaluate_rollout(plan, &resource.id, &snapshots, rollout_key.as_ref())
                .map_err(|error| RuntimeError::Plan(error.to_string()))?
            {
                RolloutOutcome::Ready { revision } => insert_revision(&mut desired, &revision)?,
                RolloutOutcome::Blocked { input_id, reason } => {
                    blocked.push(BlockedTarget {
                        target: resource.id.clone(),
                        inputs: input_ids.into_iter().collect(),
                        reason: format!("{input_id}:{}", block_reason(&reason)),
                    });
                    continue;
                }
            }
        }
        if let Err(error) = client.apply_ssa(resource, &desired).await {
            blocked.push(write_blocked(resource, error)?);
            continue;
        }
        applied.push(resource.id.clone());
    }
    Ok(RunReport { applied, blocked })
}

fn write_blocked(resource: &Resource, error: RuntimeError) -> Result<BlockedTarget, RuntimeError> {
    let RuntimeError::Kubernetes(message) = error else {
        return Err(error);
    };
    let reason = if message.contains("ownership conflict") {
        "ownership-conflict"
    } else if message.to_lowercase().contains("conflict") || message.contains("409") {
        "field-or-version-conflict"
    } else {
        "kubernetes-write-unavailable"
    };
    Ok(BlockedTarget {
        target: resource.id.clone(),
        inputs: vec![],
        reason: reason.into(),
    })
}

async fn collect_observations<C: RuntimeClient>(
    plan: &CompiledPlan,
    successfully_applied: &BTreeSet<String>,
    client: &C,
) -> Result<BTreeMap<String, Observation>, RuntimeError> {
    let mut observed = BTreeMap::new();
    for input in &plan.ir.inputs {
        let observation = if matches!(input.mode, InputMode::Observed | InputMode::Output) {
            client.observe_named(input).await?
        } else {
            let id = input.managed_resource_id.as_ref().ok_or_else(|| {
                RuntimeError::Plan("validated managed input lost its resource reference".into())
            })?;
            if successfully_applied.contains(id) {
                let resource = plan
                    .ir
                    .resources
                    .iter()
                    .find(|resource| &resource.id == id)
                    .ok_or_else(|| {
                        RuntimeError::Plan("validated managed resource is missing".into())
                    })?;
                observation_from_desired(&resource.desired)
            } else {
                Observation::Unavailable("authoritative managed ConfigMap was not applied".into())
            }
        };
        observed.insert(input.id.clone(), observation);
    }
    Ok(observed)
}

fn observation_from_desired(desired: &Value) -> Observation {
    match desired.get("data") {
        None => Observation::Available {
            data: BTreeMap::new(),
            resource_version: "authoritative-desired".into(),
        },
        Some(Value::Object(data)) => Observation::Available {
            data: data
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect(),
            resource_version: "authoritative-desired".into(),
        },
        Some(_) => Observation::Invalid("data projection is invalid".into()),
    }
}

const fn block_reason(reason: &BlockReason) -> &'static str {
    match reason {
        BlockReason::Missing => "missing",
        BlockReason::Unauthorized => "unauthorized",
        BlockReason::Unavailable => "unavailable",
        BlockReason::Invalid => "invalid",
        BlockReason::ManagedSourceNotApplied => "managed-source-not-applied",
        BlockReason::KeyUnavailable => "key-unavailable",
    }
}

fn insert_revision(desired: &mut Value, revision: &str) -> Result<(), RuntimeError> {
    let template = desired
        .pointer_mut("/spec/template")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            RuntimeError::Plan("rollout target has no Deployment Pod template".into())
        })?;
    let metadata = template
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| RuntimeError::Plan("Deployment template metadata is invalid".into()))?;
    let annotations = metadata
        .entry("annotations")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| RuntimeError::Plan("Deployment template annotations are invalid".into()))?;
    annotations.insert(REVISION_ANNOTATION.into(), Value::String(revision.into()));
    Ok(())
}

pub struct KubeRuntimeClient {
    pub(crate) client: Client,
    max_concurrent: usize,
    pub(crate) field_manager: String,
}

impl KubeRuntimeClient {
    async fn from_fixture_config(
        config: &FixtureConfig,
        max_concurrent: usize,
        domain_id: &str,
    ) -> Result<Self, RuntimeError> {
        let bytes = read_bounded_file(
            &config.kubeconfig,
            KUBECONFIG_MAX_BYTES,
            "dedicated kubeconfig",
        )?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| RuntimeError::Configuration("dedicated kubeconfig is not UTF-8".into()))?;
        let kubeconfig = Kubeconfig::from_yaml(text).map_err(|error| {
            RuntimeError::Configuration(format!("dedicated kubeconfig: {error}"))
        })?;
        validate_fixture_kubeconfig(&kubeconfig, &config.context)?;
        let options = KubeConfigOptions {
            context: Some(config.context.clone()),
            cluster: None,
            user: None,
        };
        let mut client_config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .map_err(|error| {
                RuntimeError::Configuration(format!("dedicated kubeconfig context: {error}"))
            })?;
        client_config.connect_timeout = Some(Duration::from_secs(5));
        client_config.read_timeout = Some(Duration::from_secs(15));
        client_config.write_timeout = Some(Duration::from_secs(15));
        client_config.default_retry = false;
        let client = Client::try_from(client_config)
            .map_err(|error| RuntimeError::Configuration(error.to_string()))?;
        Ok(Self {
            client,
            max_concurrent,
            field_manager: format!("apsides-{domain_id}"),
        })
    }

    pub(crate) fn from_client(client: Client, max_concurrent: usize, domain_id: &str) -> Self {
        Self {
            client,
            max_concurrent,
            field_manager: format!("apsides-{domain_id}"),
        }
    }

    pub(crate) fn api(
        &self,
        api_version: &str,
        kind: &str,
        namespace: &str,
    ) -> Result<Api<DynamicObject>, RuntimeError> {
        let (group, version, plural) = match (api_version, kind) {
            ("v1", "ConfigMap") => ("", "v1", "configmaps"),
            ("v1", "Secret") => ("", "v1", "secrets"),
            ("v1", "Service") => ("", "v1", "services"),
            ("apps/v1", "Deployment") => ("apps", "v1", "deployments"),
            _ => {
                return Err(RuntimeError::Plan(format!(
                    "unsupported Kubernetes type {api_version}/{kind}"
                )));
            }
        };
        let resource =
            ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural);
        Ok(Api::namespaced_with(
            self.client.clone(),
            namespace,
            &resource,
        ))
    }

    pub(crate) fn resource_api(
        &self,
        resource: &Resource,
    ) -> Result<Api<DynamicObject>, RuntimeError> {
        let api = compiler_core::resolve_resource_api(resource)
            .map_err(|error| RuntimeError::Plan(error.to_string()))?;
        let resource_type = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk(&api.api_group, &api.version, &api.kind),
            &api.plural,
        );
        Ok(Api::namespaced_with(
            self.client.clone(),
            &resource.namespace,
            &resource_type,
        ))
    }
}

fn validate_fixture_kubeconfig(
    kubeconfig: &Kubeconfig,
    expected_context: &str,
) -> Result<(), RuntimeError> {
    let ([named_cluster], [named_user], [named_context]) = (
        kubeconfig.clusters.as_slice(),
        kubeconfig.auth_infos.as_slice(),
        kubeconfig.contexts.as_slice(),
    ) else {
        return Err(RuntimeError::Configuration(
            "fixture kubeconfig requires exactly one cluster, user, and context".into(),
        ));
    };
    if kubeconfig.current_context.as_deref() != Some(expected_context)
        || named_context.name != expected_context
        || kubeconfig.extensions.is_some()
        || !kubeconfig.other.is_empty()
    {
        return Err(RuntimeError::Configuration(
            "fixture kubeconfig must select the expected context without extensions".into(),
        ));
    }
    let cluster = named_cluster.cluster.as_ref().ok_or_else(|| {
        RuntimeError::Configuration("fixture kubeconfig cluster is missing".into())
    })?;
    let auth = named_user
        .auth_info
        .as_ref()
        .ok_or_else(|| RuntimeError::Configuration("fixture kubeconfig user is missing".into()))?;
    let context = named_context.context.as_ref().ok_or_else(|| {
        RuntimeError::Configuration("fixture kubeconfig context is missing".into())
    })?;
    if context.cluster != named_cluster.name
        || context.user.as_deref() != Some(named_user.name.as_str())
        || context.extensions.is_some()
        || !context.other.is_empty()
        || !named_context.other.is_empty()
        || !named_cluster.other.is_empty()
        || !named_user.other.is_empty()
    {
        return Err(RuntimeError::Configuration(
            "fixture kubeconfig context must reference its sole cluster and inline-token user"
                .into(),
        ));
    }
    if cluster.server.as_deref().is_none_or(|server| {
        let Ok(uri) = server.parse::<http::Uri>() else {
            return true;
        };
        let Some(authority) = uri.authority() else {
            return true;
        };
        uri.scheme_str() != Some("https")
            || authority.as_str().contains('@')
            || authority
                .host()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .map_or(true, |address| !address.is_loopback())
    }) || cluster
        .certificate_authority_data
        .as_deref()
        .is_none_or(str::is_empty)
        || cluster.certificate_authority.is_some()
        || cluster.insecure_skip_tls_verify == Some(true)
        || cluster.proxy_url.is_some()
        || cluster.extensions.is_some()
        || !cluster.other.is_empty()
    {
        return Err(RuntimeError::Configuration(
            "fixture kubeconfig requires a loopback-IP HTTPS server with inline CA data and forbids proxy or insecure TLS settings; loopback is only an accidental-target guard, not proof of a disposable cluster".into(),
        ));
    }
    if auth
        .token
        .as_ref()
        .is_none_or(|token| token.expose_secret().is_empty())
        || auth.username.is_some()
        || auth.password.is_some()
        || auth.token_file.is_some()
        || auth.client_certificate.is_some()
        || auth.client_certificate_data.is_some()
        || auth.client_key.is_some()
        || auth.client_key_data.is_some()
        || auth.impersonate.is_some()
        || auth.impersonate_uid.is_some()
        || auth.impersonate_groups.is_some()
        || auth.impersonate_user_extra.is_some()
        || auth.extensions.is_some()
        || auth.auth_provider.is_some()
        || auth.exec.is_some()
        || !auth.other.is_empty()
    {
        return Err(RuntimeError::Configuration(
            "fixture kubeconfig requires exactly one inline bearer token and forbids credential plugins, files, client keys, and impersonation".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl RuntimeClient for KubeRuntimeClient {
    async fn apply_ssa(&self, resource: &Resource, desired: &Value) -> Result<(), RuntimeError> {
        let api = self.resource_api(resource)?;
        let selector = format!("metadata.name={}", resource.name);
        let mut listed = api
            .list(&ListParams::default().fields(&selector))
            .await
            .map_err(|error| {
                RuntimeError::Kubernetes(format!(
                    "strong ownership preflight {}/{}: {error}",
                    resource.kind, resource.name
                ))
            })?;
        if listed
            .metadata
            .continue_
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        {
            return Err(RuntimeError::Kubernetes(format!(
                "strong ownership preflight was paginated for {}/{}",
                resource.kind, resource.name
            )));
        }
        if listed.items.len() > 1 {
            return Err(RuntimeError::Kubernetes(format!(
                "exact-name ownership preflight returned multiple {}/{} objects",
                resource.kind, resource.name
            )));
        }
        match listed.items.pop() {
            None => {
                apply_absent_with_list_version(
                    &api,
                    resource,
                    desired,
                    &self.field_manager,
                    listed.metadata.resource_version.as_deref(),
                )
                .await?;
            }
            Some(existing) => {
                apply_existing_owned(&api, resource, desired, &self.field_manager, existing)
                    .await?;
            }
        }
        Ok(())
    }

    async fn observe_named(&self, input: &Input) -> Result<Observation, RuntimeError> {
        let api = self.api(&input.api_version, &input.kind, &input.namespace)?;
        let selector = format!("metadata.name={}", input.name);
        let listed = api
            .list(&ListParams::default().fields(&selector).limit(1))
            .await;
        if let Err(error) = listed {
            return Ok(classify_kube_error(&error));
        }
        match api.get_opt(&input.name).await {
            Ok(Some(object)) => observation_from_object(&object),
            Ok(None) => Ok(Observation::Missing),
            Err(error) => Ok(classify_kube_error(&error)),
        }
    }

    async fn wait_for_named_change(
        &self,
        inputs: &[Input],
        timeout: Duration,
    ) -> Result<(), RuntimeError> {
        // Managed inputs come from this controller's desired/applied state and
        // have no external watch capability. Periodic reconciliation still runs.
        let inputs = inputs
            .iter()
            .filter(|input| matches!(input.mode, InputMode::Observed | InputMode::Output))
            .collect::<Vec<_>>();
        if inputs.is_empty() {
            tokio::time::sleep(timeout).await;
            return Ok(());
        }
        let width = self.max_concurrent.max(1);
        let groups = inputs.len().div_ceil(width);
        let slice_millis = u64::try_from(timeout.as_millis())
            .unwrap_or(u64::MAX)
            .checked_div(u64::try_from(groups).unwrap_or(1))
            .unwrap_or(0)
            .max(1);
        for group in inputs.chunks(width) {
            let slice = Duration::from_millis(slice_millis);
            let seconds = u32::try_from(slice.as_secs().clamp(1, 294)).unwrap_or(294);
            let mut watches = FuturesUnordered::new();
            for input in group {
                watches.push(
                    async move {
                        let api = self.api(&input.api_version, &input.kind, &input.namespace)?;
                        let config = watcher::Config::default()
                            .fields(&format!("metadata.name={}", input.name))
                            .timeout(seconds);
                        let mut stream = watcher::watcher(api, config).boxed();
                        // kube-runtime owns list/watch resource-version recovery. Any
                        // event, recovered relist boundary, or bounded error wakes a fresh
                        // level-based reconciliation from current state.
                        let mut initialized = false;
                        while let Some(event) = stream.next().await {
                            match event {
                                Ok(watcher::Event::InitDone) => initialized = true,
                                Ok(watcher::Event::Apply(_) | watcher::Event::Delete(_))
                                    if initialized =>
                                {
                                    return Ok::<(), RuntimeError>(());
                                }
                                Err(_) => {
                                    return Err(RuntimeError::Kubernetes(format!(
                                        "named watch failed for {}/{}",
                                        input.kind, input.name
                                    )));
                                }
                                _ => {}
                            }
                        }
                        Err(RuntimeError::Kubernetes(format!(
                            "named watch ended before a change for {}/{}",
                            input.kind, input.name
                        )))
                    }
                    .boxed(),
                );
            }
            if let Ok(Some(result)) = tokio::time::timeout(slice, watches.next()).await {
                result?;
                return Ok(());
            }
        }
        Ok(())
    }
}

pub(crate) async fn apply_absent_with_list_version(
    api: &Api<DynamicObject>,
    resource: &Resource,
    desired: &Value,
    manager: &str,
    list_version: Option<&str>,
) -> Result<DynamicObject, RuntimeError> {
    let list_version = list_version
        .filter(|value| !value.is_empty() && *value != "0")
        .ok_or_else(|| {
            RuntimeError::Kubernetes(format!(
                "empty exact-name list omitted a nonzero resourceVersion for {}/{}",
                resource.kind, resource.name
            ))
        })?;
    let mut conditional = desired.clone();
    let metadata = conditional
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| RuntimeError::Plan("desired metadata is missing".into()))?;
    metadata.insert(
        "resourceVersion".into(),
        Value::String(list_version.to_owned()),
    );
    let params = strict_apply_params(manager, resource.force);
    // A strong empty exact-name list supplies a snapshot revision. Reusing
    // that nonzero revision makes an SSA create conflict if another object
    // of this identity appears before either request.
    api.patch(
        &resource.name,
        &params.clone().dry_run(),
        &Patch::Apply(&conditional),
    )
    .await
    .map_err(|error| {
        RuntimeError::Kubernetes(format!(
            "conditional SSA create dry-run {}/{}: {error}",
            resource.kind, resource.name
        ))
    })?;
    let created = api
        .patch(&resource.name, &params, &Patch::Apply(&conditional))
        .await
        .map_err(|error| {
            RuntimeError::Kubernetes(format!(
                "conditional SSA create without force {}/{}: {error}",
                resource.kind, resource.name
            ))
        })?;
    validate_exact_apply_owner(&created, resource, manager, "conditional SSA create")?;
    Ok(created)
}

async fn apply_existing_owned(
    api: &Api<DynamicObject>,
    resource: &Resource,
    desired: &Value,
    manager: &str,
    existing: DynamicObject,
) -> Result<DynamicObject, RuntimeError> {
    let (uid, version) = validate_existing_owner(&existing, resource, manager)?;
    let mut conditional = desired.clone();
    let metadata = conditional
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| RuntimeError::Plan("desired metadata is missing".into()))?;
    metadata.insert("uid".into(), Value::String(uid));
    metadata.insert("resourceVersion".into(), Value::String(version));
    let params = strict_apply_params(manager, resource.force);
    let projected = api
        .patch(
            &resource.name,
            &params.clone().dry_run(),
            &Patch::Apply(&conditional),
        )
        .await
        .map_err(|error| {
            RuntimeError::Kubernetes(format!(
                "conditional SSA dry-run {}/{}: {error}",
                resource.kind, resource.name
            ))
        })?;
    if normalized_object(&existing)? == normalized_object(&projected)? {
        return Ok(existing);
    }
    // Retain the same UID/RV across dry-run and dispatch. A racing
    // replacement or update must force a fresh ownership preflight.
    let applied = api
        .patch(&resource.name, &params, &Patch::Apply(&conditional))
        .await
        .map_err(|error| {
            RuntimeError::Kubernetes(format!(
                "conditional SSA without force {}/{}: {error}",
                resource.kind, resource.name
            ))
        })?;
    validate_exact_apply_owner(&applied, resource, manager, "conditional SSA")?;
    Ok(applied)
}

fn validate_existing_owner(
    existing: &DynamicObject,
    resource: &Resource,
    manager: &str,
) -> Result<(String, String), RuntimeError> {
    if existing.metadata.name.as_deref() != Some(resource.name.as_str())
        || existing.metadata.namespace.as_deref() != Some(resource.namespace.as_str())
    {
        return Err(RuntimeError::Kubernetes(format!(
            "exact-name ownership preflight returned the wrong identity for {}/{}",
            resource.kind, resource.name
        )));
    }
    validate_exact_apply_owner(existing, resource, manager, "ownership preflight")?;
    let uid = existing
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| RuntimeError::Kubernetes("ownership preflight omitted UID".into()))?;
    let version = existing
        .metadata
        .resource_version
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::Kubernetes("ownership preflight omitted resourceVersion".into())
        })?;
    Ok((uid.to_owned(), version.to_owned()))
}

pub(crate) fn validate_exact_apply_owner(
    object: &DynamicObject,
    resource: &Resource,
    manager: &str,
    operation: &str,
) -> Result<(), RuntimeError> {
    let update_entries = manager_main_resource_entries(object, manager, "Update");
    let apply_entries = manager_main_resource_entries(object, manager, "Apply");
    let valid_entry = matches!(apply_entries.as_slice(), [entry]
        if entry.api_version.as_deref() == Some(resource.api_version.as_str())
        && entry.fields_type.as_deref() == Some("FieldsV1") && entry.fields_v1.is_some());
    let valid_identity = object
        .metadata
        .uid
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        && object
            .metadata
            .resource_version
            .as_deref()
            .is_some_and(|value| !value.is_empty() && value != "0");
    if update_entries.is_empty() && valid_entry && valid_identity {
        Ok(())
    } else {
        Err(RuntimeError::Kubernetes(format!(
            "{operation} did not establish exact Apply ownership for {}/{}",
            resource.kind, resource.name
        )))
    }
}

fn manager_main_resource_entries<'a>(
    object: &'a DynamicObject,
    manager: &str,
    operation: &str,
) -> Vec<&'a k8s_openapi::apimachinery::pkg::apis::meta::v1::ManagedFieldsEntry> {
    object
        .metadata
        .managed_fields
        .iter()
        .flatten()
        .filter(|entry| {
            entry.manager.as_deref() == Some(manager)
                && entry.subresource.as_deref().is_none_or(str::is_empty)
                && entry.operation.as_deref() == Some(operation)
        })
        .collect()
}

pub(crate) fn normalized_object(object: &DynamicObject) -> Result<Value, RuntimeError> {
    let mut value = serde_json::to_value(object)
        .map_err(|error| RuntimeError::Kubernetes(error.to_string()))?;
    if let Some(map) = value.as_object_mut() {
        map.remove("status");
    }
    if let Some(metadata) = value.get_mut("metadata").and_then(Value::as_object_mut) {
        for key in [
            "managedFields",
            "resourceVersion",
            "generation",
            "creationTimestamp",
            "uid",
            "selfLink",
        ] {
            metadata.remove(key);
        }
    }
    Ok(value)
}

fn classify_kube_error(error: &kube::Error) -> Observation {
    match error {
        kube::Error::Api(response) if response.code == 403 => Observation::Unauthorized,
        kube::Error::Api(response) if response.code == 404 => Observation::Missing,
        other => Observation::Unavailable(other.to_string()),
    }
}

fn observation_from_object(object: &DynamicObject) -> Result<Observation, RuntimeError> {
    let value = serde_json::to_value(object)
        .map_err(|error| RuntimeError::Kubernetes(error.to_string()))?;
    let Some(resource_version) = value
        .pointer("/metadata/resourceVersion")
        .and_then(Value::as_str)
    else {
        return Ok(Observation::Invalid("resourceVersion is missing".into()));
    };
    let mut projection = BTreeMap::new();
    if let Some(data) = value.get("data") {
        let Some(data) = data.as_object() else {
            return Ok(Observation::Invalid("data projection is invalid".into()));
        };
        for (name, value) in data {
            let Some(value) = value.as_str() else {
                return Ok(Observation::Invalid(
                    "data projection contains a non-string value".into(),
                ));
            };
            projection.insert(name.clone(), value.to_owned());
        }
    }
    Ok(Observation::Available {
        data: projection,
        resource_version: resource_version.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
    use super::*;
    use compiler_core::{
        ApiResourceMapping, ApiResourceScope, DeletionPolicy, Lifecycle, SourceLocation,
    };
    use http::{Method, Request, Response, StatusCode, header::CONTENT_TYPE};
    use http_body_util::BodyExt;
    use kube::client::Body;
    use serde_json::json;
    use std::{
        collections::VecDeque,
        convert::Infallible,
        sync::{Arc, Mutex},
    };
    use tower::service_fn;

    /// A fixture problem inside the mock transport, which itself cannot fail.
    type FixtureError = Box<dyn std::error::Error + Send + Sync>;

    /// The transport signature is infallible, so a fixture problem becomes a
    /// 500 naming it. The assertions then fail with that text, which is more
    /// use than a panic from inside a service future.
    fn fixture_failure(error: &FixtureError) -> Response<Body> {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "fixtureError": error.to_string() })
                    .to_string()
                    .into_bytes(),
            ))
            .unwrap_or_else(|_| Response::new(Body::from(Vec::new())))
    }

    async fn wire_response(
        request: Request<Body>,
        responses: &Mutex<VecDeque<(StatusCode, Value)>>,
        calls: &Mutex<Vec<WireCall>>,
    ) -> std::result::Result<Response<Body>, FixtureError> {
        let (parts, body) = request.into_parts();
        let bytes = body.collect().await?.to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        calls
            .lock()
            .map_err(|_| "wire call log is poisoned")?
            .push(WireCall {
                method: parts.method,
                uri: parts.uri.to_string(),
                content_type: parts
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                body,
            });
        let (status, response) = responses
            .lock()
            .map_err(|_| "queued responses are poisoned")?
            .pop_front()
            .ok_or("fixture ran out of queued responses")?;
        Ok(Response::builder()
            .status(status)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&response)?))?)
    }

    #[derive(Debug)]
    struct WireCall {
        method: Method,
        uri: String,
        content_type: Option<String>,
        body: Value,
    }

    fn resource(desired: Value) -> Resource {
        Resource {
            force: false,
            id: "config".into(),
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            api_resource: None,
            name: "config".into(),
            namespace: "workload".into(),
            desired,
            lifecycle: Lifecycle {
                deletion_policy: DeletionPolicy::Orphan,
            },
            source: SourceLocation {
                file: "controller.ts".into(),
                line: 1,
                column: 1,
            },
            handler_output: false,
        }
    }

    fn object(data: &str, resource_version: &str, manager: &str) -> Value {
        serde_json::json!({
            "apiVersion":"v1",
            "kind":"ConfigMap",
            "metadata":{
                "name":"config",
                "namespace":"workload",
                "uid":"uid-1",
                "resourceVersion":resource_version,
                "managedFields":[{
                    "apiVersion":"v1",
                    "fieldsType":"FieldsV1",
                    "fieldsV1":{"f:data":{"f:message":{}}},
                    "manager":manager,
                    "operation":"Apply"
                }]
            },
            "data":{"message":data}
        })
    }

    fn object_list(items: &[Value], resource_version: &str) -> Value {
        serde_json::json!({
            "apiVersion":"v1",
            "kind":"ConfigMapList",
            "metadata":{"resourceVersion":resource_version},
            "items":items
        })
    }

    fn fixture_kubeconfig() -> Value {
        serde_json::json!({
            "apiVersion":"v1",
            "kind":"Config",
            "preferences":{},
            "current-context":"kind-fixture",
            "clusters":[{"name":"fixture","cluster":{
                "server":"https://127.0.0.1:6443",
                "certificate-authority-data":"Y2E="
            }}],
            "users":[{"name":"runtime","user":{"token":"fixture-token"}}],
            "contexts":[{"name":"kind-fixture","context":{
                "cluster":"fixture","user":"runtime"
            }}]
        })
    }

    fn wire_client(responses: Vec<Value>) -> (KubeRuntimeClient, Arc<Mutex<Vec<WireCall>>>) {
        wire_client_with_status(
            responses
                .into_iter()
                .map(|body| (StatusCode::OK, body))
                .collect(),
        )
    }

    fn wire_client_with_status(
        responses: Vec<(StatusCode, Value)>,
    ) -> (KubeRuntimeClient, Arc<Mutex<Vec<WireCall>>>) {
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let service = service_fn({
            let responses = Arc::clone(&responses);
            let calls = Arc::clone(&calls);
            move |request: Request<Body>| {
                let responses = Arc::clone(&responses);
                let calls = Arc::clone(&calls);
                async move {
                    Ok::<_, Infallible>(match wire_response(request, &responses, &calls).await {
                        Ok(response) => response,
                        Err(error) => fixture_failure(&error),
                    })
                }
            }
        });
        (
            KubeRuntimeClient {
                client: Client::new(service, "workload"),
                max_concurrent: 1,
                field_manager: "apsides-example".into(),
            },
            calls,
        )
    }

    fn watch_input(name: &str, mode: InputMode) -> Input {
        Input {
            id: name.into(),
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            name: name.into(),
            namespace: "workload".into(),
            mode,
            managed_resource_id: matches!(mode, InputMode::Managed | InputMode::Output)
                .then(|| name.into()),
            projection: vec!["data".into()],
            sensitive: false,
            missing: "block-dependent-mutations".into(),
            source: SourceLocation {
                file: "program.tsx".into(),
                line: 1,
                column: 1,
            },
        }
    }

    #[tokio::test]
    async fn named_watch_scopes_list_and_watch_to_observed_inputs()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut changed = object("changed", "11", "external");
        changed["metadata"]["name"] = Value::String("observed".into());
        let (client, calls) = wire_client(vec![
            object_list(&[], "10"),
            serde_json::json!({"type":"ADDED","object":changed}),
        ]);
        client
            .wait_for_named_change(
                &[
                    watch_input("managed", InputMode::Managed),
                    watch_input("observed", InputMode::Observed),
                ],
                Duration::from_secs(1),
            )
            .await?;
        let calls = calls.lock().map_err(|_| "wire calls lock poisoned")?;
        assert_eq!(calls.len(), 2);
        for call in calls.iter() {
            assert_eq!(call.method, Method::GET);
            assert!(call.uri.contains("fieldSelector=metadata.name%3Dobserved"));
            assert!(!call.uri.contains("managed"));
        }
        assert!(!calls[0].uri.contains("watch=true"));
        assert!(calls[1].uri.contains("watch=true"));
        assert!(calls[1].uri.contains("resourceVersion=10"));
        Ok(())
    }

    /// A handler output's `Output`-mode input is observed live exactly like
    /// an `Observed` one (see `InputMode::Output`): it is named-listed and
    /// joins the watch group, never treated as the reconciler's own
    /// authoritative desired state the way a `Managed` input is.
    #[tokio::test]
    async fn named_watch_scopes_list_and_watch_to_output_inputs_too()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut changed = object("changed", "11", "handler-example");
        changed["metadata"]["name"] = Value::String("output-flags".into());
        let (client, calls) = wire_client(vec![
            object_list(&[], "10"),
            serde_json::json!({"type":"ADDED","object":changed}),
        ]);
        client
            .wait_for_named_change(
                &[
                    watch_input("managed", InputMode::Managed),
                    watch_input("output-flags", InputMode::Output),
                ],
                Duration::from_secs(1),
            )
            .await?;
        let calls = calls.lock().map_err(|_| "wire calls lock poisoned")?;
        assert_eq!(calls.len(), 2);
        for call in calls.iter() {
            assert_eq!(call.method, Method::GET);
            assert!(
                call.uri
                    .contains("fieldSelector=metadata.name%3Doutput-flags")
            );
            assert!(!call.uri.contains("managed"));
        }
        assert!(!calls[0].uri.contains("watch=true"));
        assert!(calls[1].uri.contains("watch=true"));
        assert!(calls[1].uri.contains("resourceVersion=10"));
        Ok(())
    }

    #[tokio::test]
    async fn named_watch_managed_only_waits_without_api_requests()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (client, calls) = wire_client(Vec::new());
        let inputs = [watch_input("managed", InputMode::Managed)];
        let wait = client.wait_for_named_change(&inputs, Duration::from_millis(25));
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut wait)
                .await
                .is_err()
        );
        wait.await?;
        assert!(
            calls
                .lock()
                .map_err(|_| "wire calls lock poisoned")?
                .is_empty()
        );
        Ok(())
    }

    fn conflict() -> Value {
        serde_json::json!({
            "apiVersion":"v1",
            "kind":"Status",
            "status":"Failure",
            "message":"Operation cannot be fulfilled: object was modified",
            "reason":"Conflict",
            "code":409
        })
    }

    fn assert_strong_exact_name_list(call: &WireCall) -> TestResult {
        assert_eq!(call.method, Method::GET);
        assert!(
            call.uri
                .starts_with("/api/v1/namespaces/workload/configmaps?")
        );
        let query = call
            .uri
            .split_once('?')
            .ok_or("request URI has no query")?
            .1;
        assert!(query.contains("fieldSelector=metadata.name%3Dconfig"));
        assert!(!query.contains("resourceVersion="));
        assert!(!query.contains("limit="));
        assert!(!query.contains("continue="));
        assert!(!query.contains("labelSelector="));
        Ok(())
    }

    fn assert_conditional_apply(
        call: &WireCall,
        dry_run: bool,
        expected_uid: Option<&str>,
        expected_version: &str,
    ) -> TestResult {
        assert_eq!(call.method, Method::PATCH);
        assert!(
            call.uri
                .starts_with("/api/v1/namespaces/workload/configmaps/config?")
        );
        let query = call
            .uri
            .split_once('?')
            .ok_or("request URI has no query")?
            .1;
        assert!(
            query
                .split('&')
                .any(|part| part == "fieldManager=apsides-example")
        );
        assert!(
            query
                .split('&')
                .any(|part| part == "fieldValidation=Strict")
        );
        assert_eq!(query.split('&').any(|part| part == "dryRun=All"), dry_run);
        assert!(!query.split('&').any(|part| part.starts_with("force=")));
        assert_eq!(
            call.content_type.as_deref(),
            Some("application/apply-patch+yaml")
        );
        assert_eq!(
            call.body.pointer("/metadata/uid").and_then(Value::as_str),
            expected_uid
        );
        assert_eq!(
            call.body
                .pointer("/metadata/resourceVersion")
                .and_then(Value::as_str),
            Some(expected_version)
        );
        Ok(())
    }

    #[tokio::test]
    async fn changed_object_uses_dry_run_then_uid_and_version_conditional_ssa() -> TestResult {
        let mut existing = object("old", "7", "apsides-example");
        existing["data"]["removed"] = Value::String("stale".into());
        let projected = object("new", "8", "apsides-example");
        let persisted = object("new", "8", "apsides-example");
        let desired = serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"workload"},
            "data":{"message":"new"}
        });
        let (client, calls) =
            wire_client(vec![object_list(&[existing], "7"), projected, persisted]);
        client
            .apply_ssa(&resource(desired.clone()), &desired)
            .await?;
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 3);
        assert_strong_exact_name_list(&calls[0])?;
        assert_conditional_apply(&calls[1], true, Some("uid-1"), "7")?;
        assert_conditional_apply(&calls[2], false, Some("uid-1"), "7")?;
        assert!(calls[1].body.pointer("/data/removed").is_none());
        assert!(calls[2].body.pointer("/data/removed").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn explicit_force_preserves_strict_uid_and_version_preconditions() -> TestResult {
        let desired = serde_json::json!({"apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"workload"},"data":{"message":"new"}});
        let mut declared = resource(desired.clone());
        declared.force = true;
        let (client, calls) = wire_client(vec![
            object_list(&[object("old", "7", "apsides-example")], "7"),
            object("new", "8", "apsides-example"),
            object("new", "8", "apsides-example"),
        ]);
        client.apply_ssa(&declared, &desired).await?;
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 3);
        for call in &calls[1..] {
            assert_eq!(call.method, Method::PATCH);
            assert!(call.uri.contains("force=true"));
            assert!(call.uri.contains("fieldValidation=Strict"));
            assert_eq!(call.body["metadata"]["uid"], "uid-1");
            assert_eq!(call.body["metadata"]["resourceVersion"], "7");
        }
        assert!(calls[1].uri.contains("dryRun=All"));
        assert!(!calls[2].uri.contains("dryRun=All"));
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_dry_run_projection_omits_the_real_ssa_write() -> TestResult {
        let existing = object("same", "7", "apsides-example");
        let mut projected = object("same", "8", "apsides-example");
        projected["metadata"]["generation"] = Value::from(2);
        let desired = serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"workload"},
            "data":{"message":"same"}
        });
        let (client, calls) = wire_client(vec![object_list(&[existing], "7"), projected]);
        client
            .apply_ssa(&resource(desired.clone()), &desired)
            .await?;
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 2);
        assert_strong_exact_name_list(&calls[0])?;
        assert_conditional_apply(&calls[1], true, Some("uid-1"), "7")?;
        Ok(())
    }

    #[tokio::test]
    async fn pre_existing_update_only_ownership_is_rejected() -> TestResult {
        let mut existing = object("same", "7", "apsides-example");
        existing["metadata"]["managedFields"][0]["operation"] = Value::String("Update".into());
        let desired = serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"workload"},
            "data":{"message":"same"}
        });
        let (client, calls) = wire_client(vec![object_list(&[existing], "7")]);
        let error = client
            .apply_ssa(&resource(desired.clone()), &desired)
            .await
            .err()
            .ok_or("expected this call to fail")?;
        assert!(
            error
                .to_string()
                .contains("did not establish exact Apply ownership")
        );
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 1);
        assert_strong_exact_name_list(&calls[0])?;
        Ok(())
    }

    #[tokio::test]
    async fn empty_exact_name_list_uses_its_nonzero_version_for_ssa_create() -> TestResult {
        let desired = serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"workload"},
            "data":{"message":"same"}
        });
        let projected = object("same", "8", "apsides-example");
        let created = object("same", "9", "apsides-example");
        let (client, calls) = wire_client(vec![object_list(&[], "7"), projected, created]);
        client
            .apply_ssa(&resource(desired.clone()), &desired)
            .await?;
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 3);
        assert_strong_exact_name_list(&calls[0])?;
        assert_conditional_apply(&calls[1], true, None, "7")?;
        assert_conditional_apply(&calls[2], false, None, "7")?;
        assert!(calls.iter().all(|call| call.method != Method::POST));
        Ok(())
    }

    #[tokio::test]
    async fn ssa_create_race_conflict_blocks_without_fallback_or_adoption() -> TestResult {
        let desired = serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"workload"},
            "data":{"message":"same"}
        });
        let projected = object("same", "8", "apsides-example");
        let (client, calls) = wire_client_with_status(vec![
            (StatusCode::OK, object_list(&[], "7")),
            (StatusCode::OK, projected),
            (StatusCode::CONFLICT, conflict()),
        ]);
        let error = client
            .apply_ssa(&resource(desired.clone()), &desired)
            .await
            .err()
            .ok_or("expected this call to fail")?;
        assert!(
            error
                .to_string()
                .contains("conditional SSA create without force")
        );
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 3);
        assert_strong_exact_name_list(&calls[0])?;
        assert_conditional_apply(&calls[1], true, None, "7")?;
        assert_conditional_apply(&calls[2], false, None, "7")?;
        assert!(calls.iter().all(|call| call.method != Method::POST));
        Ok(())
    }

    #[tokio::test]
    async fn empty_exact_name_list_rejects_zero_or_missing_version() -> TestResult {
        let desired = serde_json::json!({
            "apiVersion":"v1","kind":"ConfigMap",
            "metadata":{"name":"config","namespace":"workload"},
            "data":{"message":"same"}
        });
        for metadata in [
            serde_json::json!({}),
            serde_json::json!({"resourceVersion":"0"}),
        ] {
            let list = serde_json::json!({
                "apiVersion":"v1",
                "kind":"ConfigMapList",
                "metadata":metadata,
                "items":[]
            });
            let (client, calls) = wire_client(vec![list]);
            let error = client
                .apply_ssa(&resource(desired.clone()), &desired)
                .await
                .err()
                .ok_or("expected this call to fail")?;
            assert!(error.to_string().contains("nonzero resourceVersion"));
            assert_eq!(
                calls.lock().map_err(|_| "calls mutex is poisoned")?.len(),
                1
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn custom_resource_uses_declared_namespaced_plural() {
        let desired = serde_json::json!({
            "apiVersion":"postgresql.cnpg.io/v1","kind":"Cluster",
            "metadata":{"name":"config","namespace":"workload"},
            "spec":{"instances":1}
        });
        let mut declared = resource(desired.clone());
        declared.api_version = "postgresql.cnpg.io/v1".into();
        declared.kind = "Cluster".into();
        declared.api_resource = Some(ApiResourceMapping {
            plural: "clusters".into(),
            scope: ApiResourceScope::Namespaced,
            schema_sha256: format!("sha256:{}", "a".repeat(64)),
        });
        let list = serde_json::json!({
            "apiVersion":"postgresql.cnpg.io/v1","kind":"ClusterList",
            "metadata":{"resourceVersion":"0"},"items":[]
        });
        let (client, calls) = wire_client(vec![list]);
        assert!(client.apply_ssa(&declared, &desired).await.is_err());
        let calls = match calls.lock() {
            Ok(calls) => calls,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0]
                .uri
                .starts_with("/apis/postgresql.cnpg.io/v1/namespaces/workload/clusters?")
        );
    }

    #[test]
    fn fixture_kubeconfig_accepts_only_inline_token_and_ca() -> TestResult {
        let valid = fixture_kubeconfig();
        let parsed = Kubeconfig::from_yaml(&valid.to_string())?;
        validate_fixture_kubeconfig(&parsed, "kind-fixture")?;
        let mut ipv6 = valid.clone();
        ipv6["clusters"][0]["cluster"]["server"] = Value::String("https://[::1]:6443".into());
        let parsed = Kubeconfig::from_yaml(&ipv6.to_string())?;
        validate_fixture_kubeconfig(&parsed, "kind-fixture")?;

        for user in [
            serde_json::json!({"token":"fixture-token","exec":{"apiVersion":"client.authentication.k8s.io/v1","command":"credential-plugin","interactiveMode":"Never"}}),
            serde_json::json!({"tokenFile":"/tmp/token"}),
            serde_json::json!({"client-certificate-data":"Y2VydA==","client-key-data":"a2V5"}),
            serde_json::json!({"token":"fixture-token","as":"cluster-admin"}),
            serde_json::json!({"auth-provider":{"name":"oidc","config":{}}}),
        ] {
            let mut candidate = valid.clone();
            candidate["users"][0]["user"] = user;
            let parsed = Kubeconfig::from_yaml(&candidate.to_string())?;
            assert!(validate_fixture_kubeconfig(&parsed, "kind-fixture").is_err());
        }

        for cluster_override in [
            serde_json::json!({"server":"https://127.0.0.1:6443","certificate-authority-data":"Y2E=","proxy-url":"http://proxy.invalid"}),
            serde_json::json!({"server":"https://127.0.0.1:6443","certificate-authority-data":"Y2E=","insecure-skip-tls-verify":true}),
            serde_json::json!({"server":"http://127.0.0.1:8080","certificate-authority-data":"Y2E="}),
            serde_json::json!({"server":"https://user:password@127.0.0.1:6443","certificate-authority-data":"Y2E="}),
            serde_json::json!({"server":"https://localhost:6443","certificate-authority-data":"Y2E="}),
            serde_json::json!({"server":"https://192.0.2.10:6443","certificate-authority-data":"Y2E="}),
            serde_json::json!({"server":"https://127.0.0.1:6443"}),
        ] {
            let mut candidate = valid.clone();
            candidate["clusters"][0]["cluster"] = cluster_override;
            let parsed = Kubeconfig::from_yaml(&candidate.to_string())?;
            assert!(validate_fixture_kubeconfig(&parsed, "kind-fixture").is_err());
        }
        Ok(())
    }

    #[test]
    fn startup_reader_rejects_duplicate_keys_and_oversized_files() -> TestResult {
        let base = std::env::temp_dir().join(format!(
            "apsides-runtime-config-test-{}",
            std::process::id()
        ));
        fs::write(&base, br#"{"mode":"production","mode":"production"}"#)?;
        assert!(read_startup(&base).is_err());
        fs::write(
            &base,
            vec![b' '; usize::try_from(STARTUP_CONFIG_MAX_BYTES + 1)?],
        )?;
        assert!(read_startup(&base).is_err());
        fs::remove_file(base)?;
        Ok(())
    }

    #[test]
    fn rustls_ring_provider_is_explicitly_available() -> TestResult {
        install_rustls_provider()?;
        install_rustls_provider()?;
        Ok(())
    }

    fn handler_source() -> Value {
        json!({"file": "program.tsx", "line": 1, "column": 1})
    }

    /// A validated plan with one declared handler `sync-flags`, bundle sha256
    /// of the fixed literal `"const bundle = 1;"`.
    fn plan_with_one_handler() -> TestResult<compiler_core::CompiledPlan> {
        let bundle = "const bundle = 1;";
        let bundle_sha256 = format!("{:x}", Sha256::digest(bundle.as_bytes()));
        let ir = json!({
            "schemaVersion": compiler_core::IR_SCHEMA,
            "domain": {
                "id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
                "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
                "minKubernetesVersion": "1.34",
                "budgets": {"maxResources": 8, "maxInputs": 8, "maxReactions": 8, "maxObjectBytes": 262_144, "maxQueueDepth": 8, "maxConcurrentRequests": 2},
            },
            "resources": [],
            "inputs": [{"id": "settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "namespace": "workload", "mode": "observed", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": handler_source()}],
            "reactions": [],
            "handlers": [{
                "id": "sync-flags", "module": "handlers/sync-flags.ts", "export": "syncFlags",
                "trigger": {"kind": "input-changed", "input": "settings"},
                "inputs": ["settings"], "egress": [], "outputs": [],
                "timeoutMillis": 30_000, "maxHeapBytes": 67_108_864,
                "bundleSha256": bundle_sha256, "source": handler_source(),
            }],
            "outputs": [],
            "provenance": {"compiler": "apsides/0.1", "inputs": [{"path": "program.tsx", "sha256": "0".repeat(64)}, {"path": "handlers/sync-flags.ts", "sha256": "1".repeat(64)}]},
        });
        Ok(parse_and_validate(&ir.to_string())?)
    }

    #[test]
    fn verify_embedded_handlers_accepts_a_matching_bundle() -> TestResult {
        let plan = plan_with_one_handler()?;
        verify_embedded_handlers(&plan, &[("sync-flags", "const bundle = 1;")])?;
        Ok(())
    }

    #[test]
    fn verify_embedded_handlers_rejects_a_tampered_bundle() -> TestResult {
        let plan = plan_with_one_handler()?;
        let error = verify_embedded_handlers(&plan, &[("sync-flags", "const bundle = 2;")])
            .err()
            .ok_or("tampered bundle was accepted")?;
        assert!(error.to_string().contains("bundleSha256"), "{error}");
        Ok(())
    }

    #[test]
    fn verify_embedded_handlers_rejects_a_duplicate_id() -> TestResult {
        let plan = plan_with_one_handler()?;
        let error = verify_embedded_handlers(
            &plan,
            &[
                ("sync-flags", "const bundle = 1;"),
                ("sync-flags", "const bundle = 1;"),
            ],
        )
        .err()
        .ok_or("duplicate embedded id was accepted")?;
        assert!(error.to_string().contains("duplicate id"), "{error}");
        Ok(())
    }

    #[test]
    fn verify_embedded_handlers_rejects_a_missing_or_extra_bundle() -> TestResult {
        let plan = plan_with_one_handler()?;
        let missing = verify_embedded_handlers(&plan, &[])
            .err()
            .ok_or("missing embedded bundle was accepted")?;
        assert!(missing.to_string().contains("do not match"), "{missing}");
        let extra = verify_embedded_handlers(
            &plan,
            &[
                ("sync-flags", "const bundle = 1;"),
                ("other", "const bundle = 1;"),
            ],
        )
        .err()
        .ok_or("undeclared embedded bundle was accepted")?;
        assert!(extra.to_string().contains("not declared"), "{extra}");
        Ok(())
    }
}
