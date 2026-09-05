use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use compiler_core::{
    CompiledPlan, InputMode, InputSnapshot, Resource, RolloutKey, RolloutOutcome, evaluate_rollout,
};
use deploy::{ResourceIdentity, RuntimeObservation};
use http_body_util::Full;
use hyper::{Request, Response, StatusCode, body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use kube::{
    Client, Config,
    api::{DynamicObject, ListParams, Patch},
};
use platform::{
    ActivationStatus, ApprovedClaim, RuntimeAuthority, RuntimeBinding, RuntimeCondition,
    RuntimeInventory, SCHEMA_VERSION, VerifiedActivation, Versioned,
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write as _;
use tokio::sync::{Mutex, RwLock, watch};

use crate::{
    KubeRuntimeClient, Observation, RuntimeClient, RuntimeError, block_reason, insert_revision,
    normalized_object, strict_apply_params, validate_exact_apply_owner,
};

const MAX_POLL_MILLIS: u64 = 30_000;
const MAX_BACKOFF_MILLIS: u64 = 60_000;
const MAX_MUTATION_INTERVAL_MILLIS: u64 = 60_000;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApiAccess {
    InCluster,
    Proxy,
}

const PROXY_CA_PATH: &str = "/etc/reakt/proxy/ca.crt";
const PROXY_CERT_PATH: &str = "/etc/reakt/proxy/tls.crt";
const PROXY_KEY_PATH: &str = "/etc/reakt/proxy/tls.key";

/// Explicit production startup data. None of these values grants authority;
/// protected platform records must independently match the binding.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProductionConfig {
    pub api_access: ApiAccess,
    pub control_namespace: String,
    pub binding: RuntimeBinding,
    pub poll_interval_millis: u64,
    pub base_backoff_millis: u64,
    pub max_backoff_millis: u64,
    pub min_mutation_interval_millis: u64,
    pub diagnostics_bind: SocketAddr,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticSnapshot {
    schema_version: &'static str,
    domain: String,
    build_digest: String,
    generation: String,
    phase: DiagnosticPhase,
    cache: CacheState,
    blocked_targets: usize,
    last_reason_code: Option<String>,
    reconciliations: u64,
    watch_recoveries: u64,
    mutations: u64,
    errors: u64,
    apply_cache_entries: u64,
    apply_cache_serialized_bytes: u64,
    pending_coalesced_triggers: u64,
    trigger_to_cycle_micros_count: u64,
    trigger_to_cycle_micros_sum: u64,
    trigger_to_cycle_micros_max: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum DiagnosticPhase {
    Starting,
    Ready,
    Degraded,
    Draining,
    Inactive,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CacheState {
    Pending,
    Synchronized,
}

struct Diagnostics {
    snapshot: RwLock<DiagnosticSnapshot>,
    reconciliations: AtomicU64,
    watch_recoveries: AtomicU64,
    mutations: AtomicU64,
    errors: AtomicU64,
    apply_cache_entries: AtomicU64,
    apply_cache_serialized_bytes: AtomicU64,
    pending_coalesced_triggers: AtomicU64,
    trigger_to_cycle_micros_count: AtomicU64,
    trigger_to_cycle_micros_sum: AtomicU64,
    trigger_to_cycle_micros_max: AtomicU64,
}

impl Diagnostics {
    fn new(binding: &RuntimeBinding) -> Self {
        Self {
            snapshot: RwLock::new(DiagnosticSnapshot {
                schema_version: "reakt.dev/runtime-status/v1",
                domain: binding.domain_id.clone(),
                build_digest: binding.build_digest.clone(),
                generation: binding.generation_id.clone(),
                phase: DiagnosticPhase::Starting,
                cache: CacheState::Pending,
                blocked_targets: 0,
                last_reason_code: None,
                reconciliations: 0,
                watch_recoveries: 0,
                mutations: 0,
                errors: 0,
                apply_cache_entries: 0,
                apply_cache_serialized_bytes: 0,
                pending_coalesced_triggers: 0,
                trigger_to_cycle_micros_count: 0,
                trigger_to_cycle_micros_sum: 0,
                trigger_to_cycle_micros_max: 0,
            }),
            reconciliations: AtomicU64::new(0),
            watch_recoveries: AtomicU64::new(0),
            mutations: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            apply_cache_entries: AtomicU64::new(0),
            apply_cache_serialized_bytes: AtomicU64::new(0),
            pending_coalesced_triggers: AtomicU64::new(0),
            trigger_to_cycle_micros_count: AtomicU64::new(0),
            trigger_to_cycle_micros_sum: AtomicU64::new(0),
            trigger_to_cycle_micros_max: AtomicU64::new(0),
        }
    }

    async fn update(
        &self,
        phase: DiagnosticPhase,
        cache: CacheState,
        blocked: usize,
        reason: Option<&str>,
    ) {
        let mut snapshot = self.snapshot.write().await;
        snapshot.phase = if blocked == 0 {
            phase
        } else {
            DiagnosticPhase::Degraded
        };
        snapshot.cache = cache;
        snapshot.blocked_targets = blocked;
        snapshot.last_reason_code = reason.map(str::to_owned);
        self.copy_counters(&mut snapshot);
    }

    fn copy_counters(&self, snapshot: &mut DiagnosticSnapshot) {
        snapshot.reconciliations = self.reconciliations.load(Ordering::Relaxed);
        snapshot.watch_recoveries = self.watch_recoveries.load(Ordering::Relaxed);
        snapshot.mutations = self.mutations.load(Ordering::Relaxed);
        snapshot.errors = self.errors.load(Ordering::Relaxed);
        snapshot.apply_cache_entries = self.apply_cache_entries.load(Ordering::Relaxed);
        snapshot.apply_cache_serialized_bytes =
            self.apply_cache_serialized_bytes.load(Ordering::Relaxed);
        snapshot.pending_coalesced_triggers =
            self.pending_coalesced_triggers.load(Ordering::Relaxed);
        snapshot.trigger_to_cycle_micros_count =
            self.trigger_to_cycle_micros_count.load(Ordering::Relaxed);
        snapshot.trigger_to_cycle_micros_sum =
            self.trigger_to_cycle_micros_sum.load(Ordering::Relaxed);
        snapshot.trigger_to_cycle_micros_max =
            self.trigger_to_cycle_micros_max.load(Ordering::Relaxed);
    }

    async fn current_snapshot(&self) -> DiagnosticSnapshot {
        let mut snapshot = self.snapshot.read().await.clone();
        self.copy_counters(&mut snapshot);
        snapshot
    }

    fn trigger_pending(&self) {
        self.pending_coalesced_triggers.store(1, Ordering::Relaxed);
    }

    fn trigger_consumed(&self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.pending_coalesced_triggers.store(0, Ordering::Relaxed);
        let _ = self.trigger_to_cycle_micros_count.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |value| Some(value.saturating_add(1)),
        );
        let _ = self.trigger_to_cycle_micros_sum.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |value| Some(value.saturating_add(micros)),
        );
        self.trigger_to_cycle_micros_max
            .fetch_max(micros, Ordering::Relaxed);
    }

    fn clear_pending_trigger(&self) {
        self.pending_coalesced_triggers.store(0, Ordering::Relaxed);
    }

    fn apply_cache_shape(&self, entries: usize, serialized_bytes: usize) {
        self.apply_cache_entries.store(
            u64::try_from(entries).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.apply_cache_serialized_bytes.store(
            u64::try_from(serialized_bytes).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

struct ProductionController {
    plan: CompiledPlan,
    config: ProductionConfig,
    kube: KubeRuntimeClient,
    authority: RuntimeAuthority,
    diagnostics: Arc<Diagnostics>,
    terminating: Arc<AtomicBool>,
    last_mutation: Mutex<Option<Instant>>,
    last_successful_apply: Mutex<BTreeMap<ResourceIdentity, (String, String)>>,
}

struct CycleResult {
    observations: Vec<RuntimeObservation>,
    conditions: Vec<RuntimeCondition>,
}

enum CycleControl {
    Continue,
    Terminate,
    Retry(&'static str),
}

pub(crate) async fn run_production(
    plan: &CompiledPlan,
    config: ProductionConfig,
) -> Result<(), RuntimeError> {
    validate_production_config(plan, &config)?;
    crate::install_rustls_provider()?;
    let mut kube_config = production_kube_config(&config)?;
    kube_config.connect_timeout = Some(Duration::from_secs(5));
    kube_config.read_timeout = Some(Duration::from_secs(15));
    kube_config.write_timeout = Some(Duration::from_secs(15));
    kube_config.default_retry = false;
    let client = Client::try_from(kube_config)
        .map_err(|error| RuntimeError::Configuration(error.to_string()))?;
    run_production_with_client(plan, config, client).await
}

fn production_kube_config(config: &ProductionConfig) -> Result<Config, RuntimeError> {
    match config.api_access {
        ApiAccess::InCluster => Config::incluster().map_err(|error| {
            RuntimeError::Configuration(format!("in-cluster Kubernetes identity: {error}"))
        }),
        ApiAccess::Proxy => {
            let generation = &config.binding.generation_id;
            if !valid_dns_label(generation) {
                return Err(RuntimeError::Configuration(
                    "proxy generation identifier is not a DNS label".into(),
                ));
            }
            let server_name = format!("reakt-proxy-{generation}.{}.svc", config.control_namespace);
            let uri = format!("https://{server_name}:8443")
                .parse()
                .map_err(|_| RuntimeError::Configuration("invalid fixed proxy API URL".into()))?;
            let mut kube_config = Config::new(uri);
            kube_config.root_cert_file = Some(PathBuf::from(PROXY_CA_PATH));
            kube_config.auth_info.client_certificate = Some(PROXY_CERT_PATH.into());
            kube_config.auth_info.client_key = Some(PROXY_KEY_PATH.into());
            kube_config.tls_server_name = Some(server_name);
            Ok(kube_config)
        }
    }
}

fn valid_dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 51
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

async fn run_production_with_client(
    plan: &CompiledPlan,
    config: ProductionConfig,
    client: Client,
) -> Result<(), RuntimeError> {
    let authority = RuntimeAuthority::new(
        client.clone(),
        &config.control_namespace,
        config.binding.clone(),
    )?;
    let diagnostics = Arc::new(Diagnostics::new(&config.binding));
    let listener = tokio::net::TcpListener::bind(config.diagnostics_bind)
        .await
        .map_err(|error| RuntimeError::Configuration(format!("diagnostics listener: {error}")))?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_diagnostics(listener, diagnostics.clone(), shutdown_rx.clone());
    let terminating = Arc::new(AtomicBool::new(false));
    spawn_shutdown_listener(shutdown_tx.clone(), terminating.clone());

    let controller = ProductionController {
        plan: plan.clone(),
        kube: KubeRuntimeClient::from_client(
            client,
            plan.ir.domain.budgets.max_concurrent_requests,
            &plan.ir.domain.id,
        ),
        authority,
        diagnostics,
        terminating,
        last_mutation: Mutex::new(None),
        last_successful_apply: Mutex::new(BTreeMap::new()),
        config,
    };
    let result = controller.run(shutdown_rx).await;
    let _ = shutdown_tx.send(true);
    result
}

fn validate_production_config(
    plan: &CompiledPlan,
    config: &ProductionConfig,
) -> Result<(), RuntimeError> {
    if plan.ir.test_only {
        return Err(RuntimeError::Configuration(
            "test-only IR cannot run in production mode".into(),
        ));
    }
    if config.binding.ir_digest != plan.ir_digest
        || config.binding.domain_id != plan.ir.domain.id
        || config.binding.cluster_binding != plan.ir.domain.cluster_binding
        || config.control_namespace != plan.ir.domain.control_namespace
    {
        return Err(RuntimeError::Configuration(
            "production binding does not match the validated embedded IR".into(),
        ));
    }
    if !(100..=MAX_POLL_MILLIS).contains(&config.poll_interval_millis)
        || !(100..=10_000).contains(&config.base_backoff_millis)
        || config.max_backoff_millis < config.base_backoff_millis
        || config.max_backoff_millis > MAX_BACKOFF_MILLIS
        || !(10..=MAX_MUTATION_INTERVAL_MILLIS).contains(&config.min_mutation_interval_millis)
    {
        return Err(RuntimeError::Configuration(
            "production polling, retry, or mutation-rate bounds are invalid".into(),
        ));
    }
    Ok(())
}

impl ProductionController {
    async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<(), RuntimeError> {
        let mut attempt = 0_u32;
        let mut last_reason: Option<String> = None;
        let mut pending_trigger: Option<Instant> = None;
        loop {
            if *shutdown.borrow() {
                self.diagnostics.clear_pending_trigger();
                self.diagnostics
                    .update(
                        DiagnosticPhase::Draining,
                        CacheState::Pending,
                        0,
                        Some("termination-requested"),
                    )
                    .await;
                return Ok(());
            }
            let Ok(activation) = self.authority.read_activation().await else {
                self.retry(&mut attempt, "activation-unavailable", &mut shutdown)
                    .await?;
                continue;
            };
            match activation.value.status {
                ActivationStatus::Draining => {
                    self.diagnostics.clear_pending_trigger();
                    if self.drain(activation).await.is_ok() {
                        return Ok(());
                    }
                    self.retry(&mut attempt, "drain-publication-failed", &mut shutdown)
                        .await?;
                    continue;
                }
                ActivationStatus::Inactive => {
                    self.diagnostics.clear_pending_trigger();
                    self.diagnostics
                        .update(
                            DiagnosticPhase::Inactive,
                            CacheState::Pending,
                            0,
                            Some("activation-inactive"),
                        )
                        .await;
                    return Ok(());
                }
                ActivationStatus::Active => {}
            }

            if let Some(enqueued) = pending_trigger.take() {
                self.diagnostics.trigger_consumed(enqueued.elapsed());
            }

            let conditions = match self.active_cycle().await {
                Ok(conditions) => conditions,
                Err(CycleControl::Continue) => continue,
                Err(CycleControl::Terminate) => return Ok(()),
                Err(CycleControl::Retry(reason)) => {
                    self.retry(&mut attempt, reason, &mut shutdown).await?;
                    continue;
                }
            };
            self.record_cycle(&conditions, &mut last_reason).await;
            if !conditions.is_empty() {
                self.retry(&mut attempt, "reconciliation-degraded", &mut shutdown)
                    .await?;
                continue;
            }
            attempt = 0;

            let timeout = Duration::from_millis(self.config.poll_interval_millis);
            tokio::select! {
                _ = shutdown.changed() => {}
                watched = self.kube.wait_for_named_change(&self.plan.ir.inputs, timeout) => {
                    if watched.is_err() {
                        self.diagnostics.watch_recoveries.fetch_add(1, Ordering::Relaxed);
                        self.retry(&mut attempt, "watch-recovery", &mut shutdown).await?;
                    } else {
                        pending_trigger = Some(Instant::now());
                        self.diagnostics.trigger_pending();
                    }
                }
            }
        }
    }

    async fn active_cycle(&self) -> Result<Vec<RuntimeCondition>, CycleControl> {
        let verified = self.authority.verify(&self.plan).await.map_err(|error| {
            if matches!(error, platform::Error::Inactive) {
                CycleControl::Continue
            } else {
                CycleControl::Retry("authority-verification-failed")
            }
        })?;
        let mut previous = self
            .authority
            .read_inventory()
            .await
            .map_err(|_| CycleControl::Retry("inventory-unavailable"))?;
        let key = self
            .rollout_key(&verified)
            .await
            .map_err(|_| CycleControl::Retry("rollout-key-unavailable"))?;
        let cycle = self
            .reconcile(key.as_ref(), &verified, &mut previous)
            .await
            .map_err(|error| match error {
                RuntimeError::Platform(platform::Error::Inactive) => CycleControl::Continue,
                RuntimeError::Terminating => CycleControl::Terminate,
                _ => CycleControl::Retry("reconciliation-authority-failed"),
            })?;
        self.diagnostics
            .reconciliations
            .fetch_add(1, Ordering::Relaxed);
        let next = inventory_for(
            &self.config.binding,
            &verified,
            cycle.observations,
            cycle.conditions.clone(),
            false,
        );
        self.authority
            .publish_inventory(&previous, &verified, next)
            .await
            .map_err(|_| CycleControl::Retry("inventory-publication-failed"))?;
        Ok(cycle.conditions)
    }

    async fn record_cycle(
        &self,
        conditions: &[RuntimeCondition],
        last_reason: &mut Option<String>,
    ) {
        let reason = conditions
            .first()
            .map(|condition| condition.reason_code.clone());
        self.diagnostics
            .update(
                DiagnosticPhase::Ready,
                CacheState::Synchronized,
                conditions.len(),
                reason.as_deref(),
            )
            .await;
        if reason != *last_reason {
            let _ = writeln!(
                std::io::stderr().lock(),
                "{}",
                serde_json::json!({
                    "domain": self.config.binding.domain_id,
                    "build": self.config.binding.build_digest,
                    "generation": self.config.binding.generation_id,
                    "status": if reason.is_some() { "degraded" } else { "ready" },
                    "reasonCode": reason.as_deref(),
                })
            );
            *last_reason = reason;
        }
    }

    async fn retry(
        &self,
        attempt: &mut u32,
        reason: &'static str,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), RuntimeError> {
        self.diagnostics.errors.fetch_add(1, Ordering::Relaxed);
        self.diagnostics
            .update(
                DiagnosticPhase::Degraded,
                CacheState::Pending,
                0,
                Some(reason),
            )
            .await;
        let shift = (*attempt).min(16);
        let exponential = self
            .config
            .base_backoff_millis
            .saturating_mul(1_u64 << shift);
        let capped = exponential.min(self.config.max_backoff_millis);
        let jitter_bound = capped / 4 + 1;
        let jitter = (u64::from(*attempt).wrapping_mul(2_654_435_761)
            ^ self.config.binding.generation_id.len() as u64)
            % jitter_bound;
        *attempt = attempt.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(capped.saturating_add(jitter))) => Ok(()),
            _ = shutdown.changed() => Ok(()),
        }
    }

    async fn rollout_key(
        &self,
        authority: &VerifiedActivation,
    ) -> Result<Option<RolloutKey>, RuntimeError> {
        if authority.state().rollout_key_id.is_none() {
            return Ok(None);
        }
        let key = self.authority.load_rollout_key(authority).await?;
        Ok(Some(RolloutKey {
            id: key.id,
            bytes: key.bytes.expose_secret().to_vec(),
        }))
    }

    async fn reconcile(
        &self,
        key: Option<&RolloutKey>,
        verified: &VerifiedActivation,
        inventory: &mut Versioned<RuntimeInventory>,
    ) -> Result<CycleResult, RuntimeError> {
        let mut applied = BTreeSet::new();
        let mut observations = validated_inventory_observations(inventory, verified)?;
        let mut conditions = Vec::new();

        for resource in self
            .plan
            .ir
            .resources
            .iter()
            .filter(|resource| resource.kind == "ConfigMap")
        {
            match self.apply(resource, &resource.desired, inventory).await {
                Ok(observation) => {
                    observations.insert(observation.identity.clone(), observation);
                    applied.insert(resource.id.clone());
                }
                Err(error @ (RuntimeError::Platform(_) | RuntimeError::Terminating)) => {
                    return Err(error);
                }
                Err(error) => conditions.push(condition_for(resource, &error)),
            }
        }

        let current = crate::collect_observations(&self.plan, &applied, &self.kube).await?;
        for resource in self
            .plan
            .ir
            .resources
            .iter()
            .filter(|resource| resource.kind != "ConfigMap")
        {
            let reactions = self
                .plan
                .ir
                .reactions
                .iter()
                .filter(|reaction| reaction.target == resource.id)
                .collect::<Vec<_>>();
            let mut desired = resource.desired.clone();
            if !reactions.is_empty() {
                let snapshots = snapshots(&self.plan, &current, &applied);
                match evaluate_rollout(&self.plan, &resource.id, &snapshots, key)
                    .map_err(|error| RuntimeError::Plan(error.to_string()))?
                {
                    RolloutOutcome::Ready { revision } => insert_revision(&mut desired, &revision)?,
                    RolloutOutcome::Blocked {
                        input_id: _,
                        reason,
                    } => {
                        conditions.push(RuntimeCondition {
                            reason_code: format!("input-{}", block_reason(&reason)),
                            resource_name: Some(resource.id.clone()),
                        });
                        continue;
                    }
                }
            }
            match self.apply(resource, &desired, inventory).await {
                Ok(observation) => {
                    observations.insert(observation.identity.clone(), observation);
                }
                Err(error @ (RuntimeError::Platform(_) | RuntimeError::Terminating)) => {
                    return Err(error);
                }
                Err(error) => conditions.push(condition_for(resource, &error)),
            }
        }
        conditions.truncate(256);
        Ok(CycleResult {
            observations: observations.into_values().collect(),
            conditions,
        })
    }

    async fn apply(
        &self,
        resource: &Resource,
        desired: &Value,
        inventory: &mut Versioned<RuntimeInventory>,
    ) -> Result<RuntimeObservation, RuntimeError> {
        if self.terminating.load(Ordering::Acquire) {
            return Err(RuntimeError::Terminating);
        }
        let initial = self.authority.verify(&self.plan).await?;
        validated_inventory_observations(inventory, &initial)?;
        let approved = approved_claim(&initial, resource)?.clone();
        let api = self
            .kube
            .api(&resource.api_version, &resource.kind, &resource.namespace)?;
        let selector = format!("metadata.name={}", resource.name);
        let listed = api
            .list(&ListParams::default().fields(&selector))
            .await
            .map_err(|_| {
                RuntimeError::Kubernetes(format!(
                    "ownership-preflight:{}/{}",
                    resource.kind, resource.name
                ))
            })?;
        if listed
            .metadata
            .continue_
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            || listed.items.len() > 1
        {
            return Err(RuntimeError::Kubernetes(format!(
                "ownership-preflight-invalid:{}/{}",
                resource.kind, resource.name
            )));
        }
        let (applied, created) =
            apply_preflight_result(self, &api, resource, desired, &approved, inventory, listed)
                .await?;
        let uid = applied
            .metadata
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| {
                RuntimeError::Kubernetes(format!(
                    "applied-object-missing-uid:{}/{}",
                    resource.kind, resource.name
                ))
            })?;
        let observation = RuntimeObservation {
            identity: identity(resource)?,
            uid,
        };
        if created {
            let next = inventory_with_observation(inventory, &initial, observation.clone());
            *inventory = self
                .authority
                .publish_inventory(inventory, &initial, next)
                .await?;
        }
        self.remember_successful_apply(resource, desired, &applied)
            .await?;
        Ok(observation)
    }

    async fn mutation_spacing(&self) {
        let mut last = self.last_mutation.lock().await;
        if let Some(previous) = *last {
            let minimum = Duration::from_millis(self.config.min_mutation_interval_millis);
            if let Some(remaining) = minimum.checked_sub(previous.elapsed()) {
                tokio::time::sleep(remaining).await;
            }
        }
        *last = Some(Instant::now());
    }

    async fn successful_apply_is_current(
        &self,
        resource: &Resource,
        desired: &Value,
        existing: &DynamicObject,
    ) -> Result<bool, RuntimeError> {
        let version = existing
            .metadata
            .resource_version
            .as_deref()
            .filter(|version| !version.is_empty() && *version != "0")
            .ok_or_else(|| RuntimeError::Plan("live target resourceVersion is missing".into()))?;
        let desired_digest = compiler_core::digest(desired)
            .map_err(|error| RuntimeError::Plan(error.to_string()))?;
        Ok(self
            .last_successful_apply
            .lock()
            .await
            .get(&identity(resource)?)
            .is_some_and(|(cached_version, cached_digest)| {
                cached_version == version && cached_digest == &desired_digest
            }))
    }

    async fn remember_successful_apply(
        &self,
        resource: &Resource,
        desired: &Value,
        applied: &DynamicObject,
    ) -> Result<(), RuntimeError> {
        let version = applied
            .metadata
            .resource_version
            .clone()
            .filter(|version| !version.is_empty() && version != "0")
            .ok_or_else(|| {
                RuntimeError::Plan("applied target resourceVersion is missing".into())
            })?;
        let desired_digest = compiler_core::digest(desired)
            .map_err(|error| RuntimeError::Plan(error.to_string()))?;
        let mut cache = self.last_successful_apply.lock().await;
        cache.insert(identity(resource)?, (version, desired_digest));
        let retained = cache
            .iter()
            .map(|(identity, (resource_version, desired_digest))| {
                serde_json::json!({
                    "identity": identity,
                    "resourceVersion": resource_version,
                    "desiredDigest": desired_digest,
                })
            })
            .collect::<Vec<_>>();
        let serialized_bytes = serde_json::to_vec(&retained)
            .map_err(|error| RuntimeError::Plan(error.to_string()))?
            .len();
        self.diagnostics
            .apply_cache_shape(cache.len(), serialized_bytes);
        Ok(())
    }

    async fn drain(
        &self,
        activation: Versioned<platform::ActivationState>,
    ) -> Result<(), RuntimeError> {
        self.diagnostics
            .update(
                DiagnosticPhase::Draining,
                CacheState::Pending,
                0,
                Some("activation-draining"),
            )
            .await;
        let previous = self.authority.read_inventory().await?;
        let next = RuntimeInventory {
            schema_version: SCHEMA_VERSION.into(),
            binding: self.config.binding.clone(),
            activation_uid: activation.uid,
            activation_resource_version: activation.resource_version,
            observations: previous.value.observations.clone(),
            conditions: vec![RuntimeCondition {
                reason_code: "drained".into(),
                resource_name: None,
            }],
            drained: true,
        };
        self.authority
            .publish_drained_inventory(&previous, next)
            .await?;
        Ok(())
    }
}

async fn apply_preflight_result(
    controller: &ProductionController,
    api: &kube::Api<DynamicObject>,
    resource: &Resource,
    desired: &Value,
    approved: &ApprovedClaim,
    inventory: &Versioned<RuntimeInventory>,
    mut listed: kube::api::ObjectList<DynamicObject>,
) -> Result<(DynamicObject, bool), RuntimeError> {
    let Some(existing) = listed.items.pop() else {
        if approved.claim.uid.is_some() || !approved.allow_create {
            return Err(RuntimeError::Kubernetes(format!(
                "claim-forbids-create:{}/{}",
                resource.kind, resource.name
            )));
        }
        controller.mutation_spacing().await;
        let object = apply_absent_claimed(
            controller,
            api,
            resource,
            desired,
            approved,
            listed.metadata.resource_version.as_deref(),
        )
        .await?;
        controller
            .diagnostics
            .mutations
            .fetch_add(1, Ordering::Relaxed);
        return Ok((object, true));
    };
    let wanted = identity(resource)?;
    let inventory_uid = inventory
        .value
        .observations
        .iter()
        .find(|observation| observation.identity == wanted)
        .map(|observation| observation.uid.as_str());
    if approved
        .claim
        .uid
        .as_deref()
        .zip(inventory_uid)
        .is_some_and(|(protected, narrowed)| protected != narrowed)
    {
        return Err(RuntimeError::Platform(platform::Error::Invalid(
            "protected claim UID differs from runtime inventory".into(),
        )));
    }
    let expected_uid = approved
        .claim
        .uid
        .as_deref()
        .or(inventory_uid)
        .ok_or_else(|| {
            RuntimeError::Kubernetes(format!(
                "claim-forbids-existing:{}/{}",
                resource.kind, resource.name
            ))
        })?;
    let object = apply_existing_claimed(
        controller,
        api,
        resource,
        desired,
        approved,
        expected_uid,
        existing,
    )
    .await?;
    Ok((object, false))
}

async fn apply_absent_claimed(
    controller: &ProductionController,
    api: &kube::Api<DynamicObject>,
    resource: &Resource,
    desired: &Value,
    approved: &ApprovedClaim,
    list_version: Option<&str>,
) -> Result<DynamicObject, RuntimeError> {
    let list_version = list_version
        .filter(|value| !value.is_empty() && *value != "0")
        .ok_or_else(|| {
            RuntimeError::Kubernetes(format!(
                "empty-list-version-invalid:{}/{}",
                resource.kind, resource.name
            ))
        })?;
    let mut conditional = desired.clone();
    let metadata = conditional
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| RuntimeError::Plan("desired metadata is missing".into()))?;
    metadata.insert("resourceVersion".into(), Value::String(list_version.into()));
    let params = strict_apply_params(&controller.kube.field_manager, resource.force);
    api.patch(
        &resource.name,
        &params.clone().dry_run(),
        &Patch::Apply(&conditional),
    )
    .await
    .map_err(|error| write_api_error("conditional-create-dry-run", resource, &error))?;
    if controller.terminating.load(Ordering::Acquire) {
        return Err(RuntimeError::Terminating);
    }
    let current = controller.authority.verify(&controller.plan).await?;
    ensure_same_claim(approved, approved_claim(&current, resource)?)?;
    let created = api
        .patch(&resource.name, &params, &Patch::Apply(&conditional))
        .await
        .map_err(|error| write_api_error("conditional-create-apply", resource, &error))?;
    validate_exact_apply_owner(
        &created,
        resource,
        &controller.kube.field_manager,
        "conditional claim create",
    )?;
    Ok(created)
}

async fn apply_existing_claimed(
    controller: &ProductionController,
    api: &kube::Api<DynamicObject>,
    resource: &Resource,
    desired: &Value,
    approved: &ApprovedClaim,
    expected_uid: &str,
    existing: DynamicObject,
) -> Result<DynamicObject, RuntimeError> {
    if existing.metadata.name.as_deref() != Some(resource.name.as_str())
        || existing.metadata.namespace.as_deref() != Some(resource.namespace.as_str())
        || existing.metadata.uid.as_deref() != Some(expected_uid)
    {
        return Err(RuntimeError::Kubernetes(format!(
            "claim-uid-mismatch:{}/{}",
            resource.kind, resource.name
        )));
    }
    let version = existing
        .metadata
        .resource_version
        .as_deref()
        .filter(|version| !version.is_empty() && *version != "0")
        .ok_or_else(|| {
            RuntimeError::Kubernetes(format!(
                "object-version-missing:{}/{}",
                resource.kind, resource.name
            ))
        })?;
    let already_owned = validate_exact_apply_owner(
        &existing,
        resource,
        &controller.kube.field_manager,
        "claim preflight",
    )
    .is_ok();
    if already_owned
        && controller
            .successful_apply_is_current(resource, desired, &existing)
            .await?
    {
        return Ok(existing);
    }
    let mut conditional = desired.clone();
    let metadata = conditional
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| RuntimeError::Plan("desired metadata is missing".into()))?;
    metadata.insert("uid".into(), Value::String(expected_uid.into()));
    metadata.insert("resourceVersion".into(), Value::String(version.into()));
    let params = strict_apply_params(&controller.kube.field_manager, resource.force);
    let projected = api
        .patch(
            &resource.name,
            &params.clone().dry_run(),
            &Patch::Apply(&conditional),
        )
        .await
        .map_err(|error| write_api_error("conditional-dry-run", resource, &error))?;
    if already_owned && normalized_object(&existing)? == normalized_object(&projected)? {
        return Ok(existing);
    }
    controller.mutation_spacing().await;
    if controller.terminating.load(Ordering::Acquire) {
        return Err(RuntimeError::Terminating);
    }
    let current = controller.authority.verify(&controller.plan).await?;
    ensure_same_claim(approved, approved_claim(&current, resource)?)?;
    let applied = api
        .patch(&resource.name, &params, &Patch::Apply(&conditional))
        .await
        .map_err(|error| write_api_error("conditional-apply", resource, &error))?;
    if applied.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(RuntimeError::Kubernetes(format!(
            "conditional-apply-uid-mismatch:{}/{}",
            resource.kind, resource.name
        )));
    }
    validate_exact_apply_owner(
        &applied,
        resource,
        &controller.kube.field_manager,
        "conditional claim SSA",
    )?;
    controller
        .diagnostics
        .mutations
        .fetch_add(1, Ordering::Relaxed);
    Ok(applied)
}

fn approved_claim<'a>(
    authority: &'a VerifiedActivation,
    resource: &Resource,
) -> Result<&'a ApprovedClaim, RuntimeError> {
    let wanted = identity(resource)?;
    authority
        .state()
        .approved_claims
        .iter()
        .find(|approved| approved.claim.identity == wanted)
        .ok_or_else(|| {
            RuntimeError::Kubernetes(format!(
                "approved-claim-missing:{}/{}",
                resource.kind, resource.name
            ))
        })
}

fn ensure_same_claim(
    expected: &ApprovedClaim,
    current: &ApprovedClaim,
) -> Result<(), RuntimeError> {
    if expected == current {
        Ok(())
    } else {
        Err(RuntimeError::Kubernetes(
            "approved-claim-changed-before-write".into(),
        ))
    }
}

fn identity(resource: &Resource) -> Result<ResourceIdentity, RuntimeError> {
    let (api_group, plural) = match (resource.api_version.as_str(), resource.kind.as_str()) {
        ("v1", "ConfigMap") => ("", "configmaps"),
        ("v1", "Service") => ("", "services"),
        ("apps/v1", "Deployment") => ("apps", "deployments"),
        _ => {
            return Err(RuntimeError::Plan(format!(
                "unsupported Kubernetes type {}/{}",
                resource.api_version, resource.kind
            )));
        }
    };
    Ok(ResourceIdentity {
        api_group: api_group.into(),
        resource: plural.into(),
        namespace: resource.namespace.clone(),
        name: resource.name.clone(),
    })
}

fn snapshots(
    plan: &CompiledPlan,
    observations: &BTreeMap<String, Observation>,
    applied: &BTreeSet<String>,
) -> BTreeMap<String, InputSnapshot> {
    observations
        .iter()
        .map(|(id, observation)| {
            let snapshot = match observation {
                Observation::Available { data, .. } => InputSnapshot::Available {
                    data: Value::Object(
                        data.iter()
                            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                            .collect(),
                    ),
                    authoritative_applied: plan
                        .ir
                        .inputs
                        .iter()
                        .find(|input| input.id == *id)
                        .is_none_or(|input| {
                            input.mode == InputMode::Observed
                                || input
                                    .managed_resource_id
                                    .as_ref()
                                    .is_some_and(|resource| applied.contains(resource))
                        }),
                },
                Observation::Missing => InputSnapshot::Missing,
                Observation::Unauthorized => InputSnapshot::Unauthorized,
                Observation::Unavailable(_) => InputSnapshot::Unavailable,
                Observation::Invalid(_) => InputSnapshot::Invalid,
            };
            (id.clone(), snapshot)
        })
        .collect()
}

fn write_api_error(operation: &str, resource: &Resource, error: &kube::Error) -> RuntimeError {
    // API messages may contain desired or observed values. Preserve only the status category.
    let category = match error {
        kube::Error::Api(response) if response.code == 409 => "field-or-version-conflict",
        kube::Error::Api(response) if response.code == 401 || response.code == 403 => {
            "write-unauthorized"
        }
        kube::Error::Api(response) if response.code == 422 => "write-invalid",
        _ => "write-unavailable",
    };
    RuntimeError::Kubernetes(format!(
        "{operation}:{category}:{}/{}",
        resource.kind, resource.name
    ))
}

fn condition_for(resource: &Resource, error: &RuntimeError) -> RuntimeCondition {
    let reason = match error {
        RuntimeError::Platform(platform::Error::Inactive) => "activation-not-active",
        RuntimeError::Platform(_) => "authority-verification-failed",
        RuntimeError::Kubernetes(message) if message.contains("conflict") => {
            "field-or-version-conflict"
        }
        RuntimeError::Kubernetes(message) if message.contains("claim") => "claim-rejected",
        RuntimeError::Kubernetes(message) if message.contains("write-unauthorized") => {
            "kubernetes-write-unauthorized"
        }
        RuntimeError::Kubernetes(message) if message.contains("write-invalid") => {
            "kubernetes-write-invalid"
        }
        RuntimeError::Kubernetes(_) => "kubernetes-write-unavailable",
        RuntimeError::Configuration(_) => "runtime-configuration-invalid",
        RuntimeError::Plan(_) => "runtime-plan-invalid",
        RuntimeError::Terminating => "termination-requested",
        RuntimeError::Output(_) => "runtime-output-unavailable",
    };
    RuntimeCondition {
        reason_code: reason.into(),
        resource_name: Some(resource.id.clone()),
    }
}

fn validated_inventory_observations(
    inventory: &Versioned<RuntimeInventory>,
    authority: &VerifiedActivation,
) -> Result<BTreeMap<ResourceIdentity, RuntimeObservation>, RuntimeError> {
    if inventory.value.drained
        || inventory.value.activation_uid != authority.uid()
        || inventory.value.observations.len() > authority.state().approved_claims.len()
    {
        return Err(RuntimeError::Platform(platform::Error::Invalid(
            "runtime inventory is not bound to the active generation".into(),
        )));
    }
    let approved = authority
        .state()
        .approved_claims
        .iter()
        .map(|claim| &claim.claim.identity)
        .collect::<BTreeSet<_>>();
    let mut observations = BTreeMap::new();
    for observation in &inventory.value.observations {
        if !approved.contains(&observation.identity)
            || observation.uid.is_empty()
            || observation.uid.len() > 128
            || observations
                .insert(observation.identity.clone(), observation.clone())
                .is_some()
        {
            return Err(RuntimeError::Platform(platform::Error::Invalid(
                "runtime inventory contains an invalid ownership narrowing".into(),
            )));
        }
    }
    Ok(observations)
}

fn inventory_with_observation(
    previous: &Versioned<RuntimeInventory>,
    authority: &VerifiedActivation,
    observation: RuntimeObservation,
) -> RuntimeInventory {
    let mut observations = previous
        .value
        .observations
        .iter()
        .cloned()
        .map(|item| (item.identity.clone(), item))
        .collect::<BTreeMap<_, _>>();
    observations.insert(observation.identity.clone(), observation);
    inventory_for(
        &previous.value.binding,
        authority,
        observations.into_values().collect(),
        previous.value.conditions.clone(),
        false,
    )
}

fn inventory_for(
    binding: &RuntimeBinding,
    authority: &VerifiedActivation,
    observations: Vec<RuntimeObservation>,
    conditions: Vec<RuntimeCondition>,
    drained: bool,
) -> RuntimeInventory {
    RuntimeInventory {
        schema_version: SCHEMA_VERSION.into(),
        binding: binding.clone(),
        activation_uid: authority.uid().into(),
        activation_resource_version: authority.resource_version().into(),
        observations,
        conditions,
        drained,
    }
}

fn spawn_shutdown_listener(shutdown: watch::Sender<bool>, terminating: Arc<AtomicBool>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        terminating.store(true, Ordering::Release);
        let _ = shutdown.send(true);
    });
}

fn spawn_diagnostics(
    listener: tokio::net::TcpListener,
    diagnostics: Arc<Diagnostics>,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        diagnostics.errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let state = diagnostics.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request| diagnostics_response(request, state.clone()));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service).await;
                    });
                }
            }
        }
    });
}

async fn diagnostics_response<B>(
    request: Request<B>,
    diagnostics: Arc<Diagnostics>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let snapshot = diagnostics.current_snapshot().await;
    let (status, body) = match request.uri().path() {
        "/livez" => (StatusCode::OK, serde_json::json!({"live": true})),
        "/readyz" if snapshot.phase == DiagnosticPhase::Ready => {
            (StatusCode::OK, serde_json::json!({"ready": true}))
        }
        "/readyz" => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"ready": false}),
        ),
        "/status" => (
            StatusCode::OK,
            serde_json::to_value(snapshot)
                .unwrap_or_else(|_| serde_json::json!({"status":"unavailable"})),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            serde_json::json!({"status":"not-found"}),
        ),
    };
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use compiler_core::{CompiledPlan, PermissionCeiling, digest, parse_and_validate};
    use deploy::{ClaimState, ProtectedClaim};
    use http::{Method, Response};
    use http_body_util::BodyExt;
    use kube::client::Body;
    use platform::{ActivationState, PolicyState};
    use serde_json::json;
    use std::{collections::VecDeque, sync::Mutex as StdMutex};
    use tower::service_fn;

    #[test]
    fn write_diagnostics_preserve_status_categories_without_api_values() {
        let plan = plan();
        let resource = &plan.ir.resources[0];
        for (code, expected) in [
            (409, "field-or-version-conflict"),
            (401, "kubernetes-write-unauthorized"),
            (403, "kubernetes-write-unauthorized"),
            (422, "kubernetes-write-invalid"),
            (500, "kubernetes-write-unavailable"),
        ] {
            let api_error = kube::Error::Api(
                kube::core::Status::failure("secret-canary-value", "secret-canary-value")
                    .with_code(code)
                    .boxed(),
            );
            let error = write_api_error("conditional-dry-run", resource, &api_error);
            assert_eq!(condition_for(resource, &error).reason_code, expected);
            assert!(!error.to_string().contains("secret-canary-value"));
        }
    }

    type Calls = Arc<StdMutex<Vec<(Method, String, Value)>>>;

    fn plan() -> CompiledPlan {
        parse_and_validate(&json!({
            "schemaVersion":"reakt.dev/ir/v1", "testOnly":false,
            "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262_144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
            "resources":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"config","namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload"},"data":{"value":"approved"}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}}],
            "inputs":[],"reactions":[],"provenance":{"compiler":"reakt/0.1","inputs":[{"path":"program.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
        }).to_string()).unwrap()
    }

    fn policy(plan: &CompiledPlan) -> PolicyState {
        let domain = &plan.ir.domain;
        let ceiling = PermissionCeiling {
            policy_ref: domain.policy_ref.clone(),
            domain_id: domain.id.clone(),
            cluster_binding: domain.cluster_binding.clone(),
            workload_namespace: domain.namespace.clone(),
            control_namespace: domain.control_namespace.clone(),
            capabilities: plan.capabilities.clone(),
            accepted_enforcement_gaps: plan.enforcement_gaps.clone(),
            accepted_indirect_risks: plan.indirect_risks.clone(),
        };
        PolicyState {
            schema_version: SCHEMA_VERSION.into(),
            runtime_ceiling: ceiling.clone(),
            cleanup_ceiling: ceiling,
        }
    }

    fn activation(plan: &CompiledPlan, policy: &PolicyState, uid: Option<&str>) -> ActivationState {
        ActivationState {
            schema_version: SCHEMA_VERSION.into(),
            binding: RuntimeBinding {
                domain_id: "example".into(),
                cluster_binding: "cluster-a".into(),
                generation_id: "generation-a".into(),
                build_digest: format!("sha256:{}", "a".repeat(64)),
                ir_digest: plan.ir_digest.clone(),
                policy_digest: digest(policy).unwrap(),
                capability_digest: digest(&plan.capabilities).unwrap(),
                service_account_name: "reakt-example-generation-a".into(),
                controller_image: format!("example.invalid/controller@sha256:{}", "b".repeat(64)),
            },
            status: ActivationStatus::Active,
            approved_claims: vec![ApprovedClaim {
                claim: ProtectedClaim {
                    identity: ResourceIdentity {
                        api_group: String::new(),
                        resource: "configmaps".into(),
                        namespace: "workload".into(),
                        name: "config".into(),
                    },
                    domain_id: "example".into(),
                    generation_id: "generation-a".into(),
                    transition_id: "transition-a".into(),
                    state: ClaimState::Active,
                    uid: uid.map(str::to_owned),
                },
                deletion_policy: compiler_core::DeletionPolicy::Orphan,
                allow_create: true,
            }],
            rollout_key_id: None,
        }
    }

    fn control_object(name: &str, state: &impl Serialize, uid: &str, rv: &str) -> Value {
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"control","uid":uid,"resourceVersion":rv},"data":{"state.json":serde_json::to_string(state).unwrap()}})
    }

    fn target_object(value: &str, uid: &str, rv: &str) -> Value {
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload","uid":uid,"resourceVersion":rv,
            "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{"f:value":{}}},"manager":"reakt-example","operation":"Apply"}]},"data":{"value":value}})
    }

    fn target_list(items: &[Value], rv: &str) -> Value {
        json!({"apiVersion":"v1","kind":"ConfigMapList","metadata":{"resourceVersion":rv},"items":items})
    }

    fn inventory(
        activation_uid: &str,
        activation_rv: &str,
        observations: Vec<RuntimeObservation>,
    ) -> RuntimeInventory {
        RuntimeInventory {
            schema_version: SCHEMA_VERSION.into(),
            binding: activation(&plan(), &policy(&plan()), None).binding,
            activation_uid: activation_uid.into(),
            activation_resource_version: activation_rv.into(),
            observations,
            conditions: vec![],
            drained: false,
        }
    }

    fn versioned_inventory(value: RuntimeInventory, rv: &str) -> Versioned<RuntimeInventory> {
        Versioned {
            value,
            uid: "inventory-uid".into(),
            resource_version: rv.into(),
        }
    }

    fn target_observation() -> RuntimeObservation {
        RuntimeObservation {
            identity: ResourceIdentity {
                api_group: String::new(),
                resource: "configmaps".into(),
                namespace: "workload".into(),
                name: "config".into(),
            },
            uid: "target-uid".into(),
        }
    }

    fn wire(responses: Vec<Value>) -> (Client, Calls) {
        let responses = Arc::new(StdMutex::new(VecDeque::from(responses)));
        let calls: Calls = Arc::default();
        let service = service_fn({
            let calls = calls.clone();
            move |request: Request<Body>| {
                let responses = responses.clone();
                let calls = calls.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let bytes = body.collect().await.unwrap().to_bytes();
                    let value = if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    };
                    calls
                        .lock()
                        .unwrap()
                        .push((parts.method, parts.uri.to_string(), value));
                    let response = responses
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected request");
                    Ok::<_, Infallible>(
                        Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::from(serde_json::to_vec(&response).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        (Client::new(service, "control"), calls)
    }

    fn controller(
        plan: &CompiledPlan,
        activation: &ActivationState,
        client: Client,
    ) -> ProductionController {
        let config = ProductionConfig {
            api_access: ApiAccess::InCluster,
            control_namespace: "control".into(),
            binding: activation.binding.clone(),
            poll_interval_millis: 100,
            base_backoff_millis: 100,
            max_backoff_millis: 1_000,
            min_mutation_interval_millis: 10,
            diagnostics_bind: "127.0.0.1:0".parse().unwrap(),
        };
        ProductionController {
            plan: plan.clone(),
            authority: RuntimeAuthority::new(client.clone(), "control", activation.binding.clone())
                .unwrap(),
            kube: KubeRuntimeClient::from_client(client, 2, "example"),
            diagnostics: Arc::new(Diagnostics::new(&activation.binding)),
            terminating: Arc::new(AtomicBool::new(false)),
            last_mutation: Mutex::new(None),
            last_successful_apply: Mutex::new(BTreeMap::new()),
            config,
        }
    }

    #[test]
    fn backoff_and_poll_bounds_are_finite() {
        assert_eq!(MAX_POLL_MILLIS, 30_000);
        assert_eq!(MAX_BACKOFF_MILLIS, 60_000);
        assert_eq!(MAX_MUTATION_INTERVAL_MILLIS, 60_000);
    }

    #[test]
    fn api_access_rejects_ambient_alternatives() {
        assert!(serde_json::from_str::<ApiAccess>("\"inCluster\"").is_ok());
        assert!(serde_json::from_str::<ApiAccess>("\"proxy\"").is_ok());
        assert!(serde_json::from_str::<ApiAccess>("\"kubeconfig\"").is_err());
        assert!(serde_json::from_str::<ApiAccess>("{\"proxy\":\"https://example\"}").is_err());
    }

    #[tokio::test]
    async fn proxy_access_uses_only_fixed_mtls_identity() {
        let plan = plan();
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None);
        let mut controller = controller(&plan, &activation, wire(vec![]).0);
        controller.config.api_access = ApiAccess::Proxy;
        let config = production_kube_config(&controller.config).unwrap();
        assert_eq!(
            config.cluster_url.to_string(),
            "https://reakt-proxy-generation-a.control.svc:8443/"
        );
        assert_eq!(config.root_cert_file, Some(PathBuf::from(PROXY_CA_PATH)));
        assert_eq!(
            config.auth_info.client_certificate.as_deref(),
            Some(PROXY_CERT_PATH)
        );
        assert_eq!(config.auth_info.client_key.as_deref(), Some(PROXY_KEY_PATH));
        assert!(config.auth_info.token.is_none());
        assert!(config.auth_info.token_file.is_none());
    }

    #[tokio::test]
    async fn diagnostics_never_include_input_values_or_key_material() {
        let diagnostics = Arc::new(Diagnostics::new(&RuntimeBinding {
            domain_id: "domain".into(),
            cluster_binding: "cluster".into(),
            generation_id: "generation".into(),
            build_digest: format!("sha256:{}", "1".repeat(64)),
            ir_digest: format!("sha256:{}", "2".repeat(64)),
            policy_digest: format!("sha256:{}", "3".repeat(64)),
            capability_digest: format!("sha256:{}", "4".repeat(64)),
            service_account_name: "controller".into(),
            controller_image: format!("example/controller@sha256:{}", "5".repeat(64)),
        }));
        diagnostics
            .update(
                DiagnosticPhase::Ready,
                CacheState::Synchronized,
                1,
                Some("input-unavailable"),
            )
            .await;
        diagnostics.apply_cache_shape(2, 123);
        diagnostics.trigger_pending();
        assert_eq!(
            diagnostics
                .current_snapshot()
                .await
                .pending_coalesced_triggers,
            1
        );
        diagnostics.trigger_consumed(Duration::from_micros(7));
        diagnostics
            .update(
                DiagnosticPhase::Ready,
                CacheState::Synchronized,
                1,
                Some("input-unavailable"),
            )
            .await;
        let request = Request::builder().uri("/status").body(()).unwrap();
        let response = diagnostics_response(request, diagnostics).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("secret-value"));
        assert!(!body.contains("rollout-key"));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["applyCacheEntries"], 2);
        assert_eq!(body["applyCacheSerializedBytes"], 123);
        assert_eq!(body["pendingCoalescedTriggers"], 0);
        assert_eq!(body["triggerToCycleMicrosCount"], 1);
        assert_eq!(body["triggerToCycleMicrosSum"], 7);
        assert_eq!(body["triggerToCycleMicrosMax"], 7);
    }

    #[tokio::test]
    async fn claimed_absent_create_reverifies_activation_after_dry_run() {
        let plan = plan();
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None);
        let previous_value = inventory("activation-uid", "19", vec![]);
        let mut previous = versioned_inventory(previous_value.clone(), "18");
        let next_value = inventory("activation-uid", "20", vec![target_observation()]);
        let responses = vec![
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(&[], "30"),
            target_object("approved", "target-uid", "31"),
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_object("approved", "target-uid", "32"),
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object(
                "reakt-inventory-example",
                &previous_value,
                "inventory-uid",
                "18",
            ),
            control_object(
                "reakt-inventory-example",
                &next_value,
                "inventory-uid",
                "21",
            ),
        ];
        let (client, calls) = wire(responses);
        let controller = controller(&plan, &activation, client);
        let observed = controller
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .unwrap();
        assert_eq!(observed.uid, "target-uid");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 10);
        assert_eq!(calls[0].0, Method::GET);
        assert_eq!(calls[1].0, Method::GET);
        assert!(calls[2].1.contains("fieldSelector=metadata.name%3Dconfig"));
        assert!(calls[3].1.contains("dryRun=All"));
        assert!(calls[3].1.contains("fieldValidation=Strict"));
        assert_eq!(
            calls[4].0,
            Method::GET,
            "activation must be reread before mutation"
        );
        assert_eq!(
            calls[5].0,
            Method::GET,
            "policy must be revalidated before mutation"
        );
        assert_eq!(calls[6].0, Method::PATCH);
        assert!(!calls[6].1.contains("dryRun=All"));
        assert!(calls[6].1.contains("fieldValidation=Strict"));
        assert_eq!(
            calls[6]
                .2
                .pointer("/metadata/resourceVersion")
                .and_then(Value::as_str),
            Some("30")
        );
        assert!(calls.iter().all(|call| call.0 != Method::POST));
        assert_eq!(calls[7].0, Method::GET, "activation gates UID persistence");
        assert_eq!(
            calls[8].0,
            Method::GET,
            "inventory CAS rereads current state"
        );
        assert_eq!(calls[9].0, Method::PUT, "created UID is durably recorded");
        assert_eq!(previous.value.observations[0].uid, "target-uid");
    }

    #[tokio::test]
    async fn uidless_claim_never_adopts_a_preexisting_object() {
        let plan = plan();
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None);
        let (client, calls) = wire(vec![
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(&[target_object("foreign", "foreign-uid", "30")], "30"),
        ]);
        let controller = controller(&plan, &activation, client);
        let mut previous = versioned_inventory(inventory("activation-uid", "20", vec![]), "18");
        let error = controller
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("claim-forbids-existing"));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|call| call.0 != Method::PATCH));
    }

    #[tokio::test]
    async fn uidless_claim_resumes_only_from_bound_inventory_uid() {
        let plan = plan();
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None);
        let mut previous = versioned_inventory(
            inventory("activation-uid", "20", vec![target_observation()]),
            "18",
        );
        let existing = target_object("approved", "target-uid", "30");
        let (client, calls) = wire(vec![
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(std::slice::from_ref(&existing), "30"),
            existing,
        ]);
        let controller = controller(&plan, &activation, client);
        let observed = controller
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .unwrap();
        assert_eq!(observed.uid, "target-uid");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls[3].1.contains("dryRun=All"));
    }

    #[tokio::test]
    async fn successful_apply_cache_skips_only_same_version_and_desired_state() {
        let plan = plan();
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None);
        let mut previous = versioned_inventory(
            inventory("activation-uid", "20", vec![target_observation()]),
            "18",
        );
        let existing = target_object("approved", "target-uid", "30");
        let (client, calls) = wire(vec![
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(std::slice::from_ref(&existing), "30"),
            existing.clone(),
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(std::slice::from_ref(&existing), "30"),
        ]);
        let controller = controller(&plan, &activation, client);
        for _ in 0..2 {
            controller
                .apply(
                    &plan.ir.resources[0],
                    &plan.ir.resources[0].desired,
                    &mut previous,
                )
                .await
                .unwrap();
        }
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 7);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.1.contains("dryRun=All"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn successful_apply_cache_invalidates_on_live_version_change() {
        let plan = plan();
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None);
        let mut previous = versioned_inventory(
            inventory("activation-uid", "20", vec![target_observation()]),
            "18",
        );
        let existing = target_object("approved", "target-uid", "30");
        let drifted = target_object("approved", "target-uid", "31");
        let (client, calls) = wire(vec![
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(std::slice::from_ref(&existing), "30"),
            existing,
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(std::slice::from_ref(&drifted), "31"),
            drifted,
        ]);
        let controller = controller(&plan, &activation, client);
        for _ in 0..2 {
            controller
                .apply(
                    &plan.ir.resources[0],
                    &plan.ir.resources[0].desired,
                    &mut previous,
                )
                .await
                .unwrap();
        }
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 8);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.1.contains("dryRun=All"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn failed_created_uid_persistence_never_falls_back_to_manager_metadata() {
        let plan = plan();
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None);
        let mut previous = versioned_inventory(inventory("activation-uid", "20", vec![]), "18");
        let (client, _) = wire(vec![
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(&[], "30"),
            target_object("approved", "target-uid", "31"),
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_object("approved", "target-uid", "32"),
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "21",
            ),
        ]);
        let runtime = controller(&plan, &activation, client);
        let error = runtime
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("active inventory publication"));
        assert!(previous.value.observations.is_empty());

        let existing = target_object("approved", "target-uid", "32");
        let (client, calls) = wire(vec![
            control_object(
                "reakt-activation-example",
                &activation,
                "activation-uid",
                "20",
            ),
            control_object("reakt-policy-example", &policy, "policy-uid", "19"),
            target_list(&[existing], "32"),
        ]);
        let runtime = controller(&plan, &activation, client);
        let error = runtime
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("claim-forbids-existing"));
        assert_eq!(calls.lock().unwrap().len(), 3);
    }
}
