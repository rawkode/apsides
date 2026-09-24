use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base64::Engine as _;
use compiler_core::{
    CompiledPlan, InputMode, InputSnapshot, Resource, RolloutKey, RolloutOutcome, evaluate_rollout,
};
use deploy::{ResourceIdentity, RuntimeObservation};
use hickory_resolver::config::ResolverConfig;
use http_body_util::Full;
use hyper::{Request, Response, StatusCode, body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use kube::{
    Client, Config,
    api::{DynamicObject, ListParams, Patch},
};
use platform::{
    ActivationStatus, ApplyPrecondition, ApprovedClaim, RuntimeAuthority, RuntimeBinding,
    RuntimeCondition, RuntimeInventory, SCHEMA_VERSION, VerifiedActivation, Versioned,
    conditional_apply_request,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write as _;
use tokio::sync::{Mutex, RwLock, watch};

use crate::{
    KubeRuntimeClient, LockdownState, Observation, RuntimeClient, RuntimeError, block_reason,
    insert_revision, manager_main_resource_entries, normalized_object, validate_exact_apply_owner,
};

const MAX_DIAGNOSTIC_CONDITIONS: usize = 256;
const MAX_POLL_MILLIS: u64 = 30_000;
const MAX_BACKOFF_MILLIS: u64 = 60_000;
const MAX_MUTATION_INTERVAL_MILLIS: u64 = 60_000;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApiAccess {
    Proxy,
}

const PROXY_CA_PATH: &str = "/etc/apsides/proxy/ca.crt";
const PROXY_CERT_PATH: &str = "/etc/apsides/proxy/tls.crt";
const PROXY_KEY_PATH: &str = "/etc/apsides/proxy/tls.key";
/// Read once, in `read_production_startup_files`, during `startup::start`'s
/// synchronous phase, before Landlock lockdown -- see the Apsides v0.3
/// plan, Phase 5c. Every production startup reads this file: the proxy
/// Service is reached by hostname (`apsides-proxy-<generation>.<namespace>.svc`),
/// so DNS is needed on every (re)connect, independent of whether any
/// declared handler also has egress hosts.
const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";

/// Small, fixed upper bounds for each proxy identity file -- a CA bundle,
/// leaf certificate or private key is at most a few KiB in practice; these
/// are generous enough for a multi-certificate chain while still bounding
/// how much of `read_production_startup_files`'s one-shot pre-lockdown read
/// budget any single file can consume.
const PROXY_CA_MAX_BYTES: u64 = 64 * 1024;
const PROXY_CERT_MAX_BYTES: u64 = 64 * 1024;
const PROXY_KEY_MAX_BYTES: u64 = 64 * 1024;
const RESOLV_CONF_MAX_BYTES: u64 = 64 * 1024;

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
    /// The effective filesystem-lockdown state established at startup
    /// (`startup::start`, before this controller's diagnostics server ever
    /// binds) -- see the Apsides v0.3 plan, Phase 5c. Always
    /// `LockdownState::Landlock` in practice: `start` fails closed before
    /// reaching `run_production` at all for every other case, since
    /// production mode never accepts the fixture-only `allowNoLandlock`
    /// escape hatch. Exposed anyway so the fail-closed contract is directly
    /// observable, not merely asserted by tests elsewhere.
    lockdown: LockdownState,
    phase: DiagnosticPhase,
    cache: CacheState,
    blocked_targets: usize,
    conditions: Vec<DiagnosticCondition>,
    handlers: Vec<HandlerDiagnostic>,
    field_or_version_conflicts: usize,
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

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticCondition {
    reason_code: String,
    resource_id: Option<String>,
}

/// One declared handler's dispatch status -- see the Apsides v0.3 plan,
/// Phase 5b, "Conditions and observability". `queue_depth` is 0 or 1: the
/// queue coalesces per handler (see `handlers::HandlerRuntime::enqueue`),
/// so at most one event is ever pending for a given handler at once.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HandlerDiagnostic {
    id: String,
    last_run_millis_ago: Option<u64>,
    last_error_code: Option<String>,
    queue_depth: usize,
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
    fn new(binding: &RuntimeBinding, lockdown: LockdownState) -> Self {
        Self {
            snapshot: RwLock::new(DiagnosticSnapshot {
                schema_version: "apsides.dev/runtime-status/v1",
                domain: binding.domain_id.clone(),
                build_digest: binding.build_digest.clone(),
                generation: binding.generation_id.clone(),
                lockdown,
                phase: DiagnosticPhase::Starting,
                cache: CacheState::Pending,
                blocked_targets: 0,
                conditions: Vec::new(),
                handlers: Vec::new(),
                field_or_version_conflicts: 0,
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
        conditions: &[RuntimeCondition],
        reason: Option<&str>,
    ) {
        let mut snapshot = self.snapshot.write().await;
        snapshot.phase = if conditions.is_empty() {
            phase
        } else {
            DiagnosticPhase::Degraded
        };
        snapshot.cache = cache;
        snapshot.conditions = conditions
            .iter()
            .take(MAX_DIAGNOSTIC_CONDITIONS)
            .map(|condition| DiagnosticCondition {
                reason_code: condition.reason_code.clone(),
                resource_id: condition.resource_name.clone(),
            })
            .collect();
        snapshot.blocked_targets = snapshot.conditions.len();
        snapshot.field_or_version_conflicts = snapshot
            .conditions
            .iter()
            .filter(|condition| condition.reason_code == "field-or-version-conflict")
            .count();
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

    async fn set_handlers(&self, handlers: Vec<HandlerDiagnostic>) {
        self.snapshot.write().await.handlers = handlers;
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

pub(crate) struct ProductionController {
    plan: CompiledPlan,
    config: ProductionConfig,
    kube: KubeRuntimeClient,
    authority: RuntimeAuthority,
    diagnostics: Arc<Diagnostics>,
    terminating: Arc<AtomicBool>,
    last_mutation: Mutex<Option<Instant>>,
    last_successful_apply: Mutex<BTreeMap<ResourceIdentity, (String, String)>>,
    /// `None` for a domain with no declared handlers (no isolate, no
    /// background writer task, no DNS resolver ever built). `Some` is
    /// behind a `Mutex` for interior mutability -- every `&self` reconcile
    /// method needs `&mut` access to run one dispatch cycle, and handler
    /// dispatch is itself serialized to global concurrency 1 (see the
    /// Apsides v0.3 plan, Phase 5b), so the lock is never contended.
    handler_runtime: Option<Mutex<crate::handlers::HandlerRuntime>>,
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

/// Everything `read_production_startup_files` reads from the filesystem and
/// derives from those bytes, during `startup::start`'s synchronous
/// pre-lockdown phase: the proxy client identity, already converted into
/// the exact in-memory shapes `kube::Config` takes (never a file path), and
/// the DNS material `kube_connector::build_client` needs to reach the proxy
/// Service by hostname without glibc's `getaddrinfo`.
pub(crate) struct ProductionStartupFiles {
    /// `PROXY_CA_PATH`'s PEM bundle, parsed into DER certificates --
    /// `kube::Config::root_cert`'s exact shape. `root_cert_file` is never
    /// set: the file is read exactly once, here.
    root_cert_der: Vec<Vec<u8>>,
    /// `PROXY_CERT_PATH`'s PEM bytes, base64-encoded --
    /// `kube::Config::auth_info::client_certificate_data`'s exact shape
    /// (`kube-client` base64-decodes it straight back to these same bytes;
    /// see `AuthInfo::load_client_certificate`). `client_certificate` (the
    /// file-path field) is never set.
    client_certificate_data: String,
    /// `PROXY_KEY_PATH`'s PEM bytes, base64-encoded, the same way as
    /// `client_certificate_data` above.
    client_key_data: SecretString,
    /// The proxy's certificate SAN / mTLS SNI
    /// (`apsides-proxy-<generation>.<namespace>.svc`) -- unchanged from
    /// before Phase 5c, and unrelated to DNS resolution: `cluster_url`
    /// below is the resolvable FQDN, but the TLS handshake still presents
    /// (and must still validate against) this exact short name, since
    /// that's what `platform::tls`'s proxy certificate issuance signs.
    server_name: String,
    /// `https://<server_name>.<cluster_domain>:8443` -- the FQDN this
    /// process can actually resolve without search-list expansion.
    /// `<server_name>` alone (e.g. `apsides-proxy-x.ns.svc`) is not itself a
    /// resolvable record; real cluster DNS only serves the fully-qualified
    /// `<name>.<namespace>.svc.<cluster-domain>` form, and a plain DNS
    /// client normally reaches it by trying `resolv.conf`'s `search`
    /// suffixes in turn (glibc's `ndots`-gated expansion). This process
    /// builds the FQDN itself instead of relying on that expansion (see
    /// `cluster_domain_from_resolv_conf`), so `kube_connector`'s resolver
    /// only ever needs a single, unambiguous lookup.
    cluster_url: http::Uri,
    /// `RESOLV_CONF_PATH`'s `nameserver` lines, for
    /// `kube_connector::build_client`'s own hickory resolver.
    kube_resolver_config: ResolverConfig,
    /// `RESOLV_CONF_PATH`'s raw contents, for `HandlerRuntime::new`'s own
    /// (separately built, handler-egress-only) resolver config -- see the
    /// Apsides v0.3 plan, Phase 5b.
    resolv_conf_contents: String,
}

/// Reads every file `ProductionConfig` needs and converts each into the
/// in-memory shape its consumer takes -- see [`ProductionStartupFiles`].
/// Called once, by `startup::start`, before Landlock lockdown; nothing
/// downstream of it (`run_production` and everything it calls) ever opens
/// a file again.
///
/// # Errors
///
/// Returns an error if the generation identifier is not a DNS label, if
/// any required file is missing, oversized or not UTF-8 (`resolv.conf`
/// only) or not valid PEM (the CA bundle only), or if `resolv.conf` has no
/// `nameserver` line or no `search` domain to derive the cluster domain
/// from.
pub(crate) fn read_production_startup_files(
    config: &ProductionConfig,
) -> Result<ProductionStartupFiles, RuntimeError> {
    let ApiAccess::Proxy = config.api_access;
    let generation = &config.binding.generation_id;
    if !valid_dns_label(generation) {
        return Err(RuntimeError::Configuration(
            "proxy generation identifier is not a DNS label".into(),
        ));
    }
    let server_name = format!(
        "apsides-proxy-{generation}.{}.svc",
        config.control_namespace
    );

    let ca_pem = crate::read_bounded_file(
        std::path::Path::new(PROXY_CA_PATH),
        PROXY_CA_MAX_BYTES,
        "proxy client CA",
    )?;
    let cert_pem = crate::read_bounded_file(
        std::path::Path::new(PROXY_CERT_PATH),
        PROXY_CERT_MAX_BYTES,
        "proxy client certificate",
    )?;
    let key_pem = crate::read_bounded_file(
        std::path::Path::new(PROXY_KEY_PATH),
        PROXY_KEY_MAX_BYTES,
        "proxy client key",
    )?;
    let resolv_conf_bytes = crate::read_bounded_file(
        std::path::Path::new(RESOLV_CONF_PATH),
        RESOLV_CONF_MAX_BYTES,
        "resolv.conf",
    )?;
    let resolv_conf_contents = String::from_utf8(resolv_conf_bytes)
        .map_err(|_error| RuntimeError::Configuration("resolv.conf is not UTF-8".into()))?;

    let root_cert_der = parse_ca_der(&ca_pem)?;
    let client_certificate_data = base64::engine::general_purpose::STANDARD.encode(&cert_pem);
    let client_key_data =
        SecretString::from(base64::engine::general_purpose::STANDARD.encode(&key_pem));

    let kube_resolver_config = engine::resolver_config_from_resolv_conf(&resolv_conf_contents)
        .map_err(|error| RuntimeError::Configuration(format!("resolv.conf: {error}")))?;
    let cluster_domain =
        cluster_domain_from_resolv_conf(&resolv_conf_contents).ok_or_else(|| {
            RuntimeError::Configuration(
                "resolv.conf has no search domain to derive the cluster domain from".into(),
            )
        })?;
    let cluster_url = format!("https://{server_name}.{cluster_domain}:8443")
        .parse()
        .map_err(|_| RuntimeError::Configuration("invalid fixed proxy API URL".into()))?;

    Ok(ProductionStartupFiles {
        root_cert_der,
        client_certificate_data,
        client_key_data,
        server_name,
        cluster_url,
        kube_resolver_config,
        resolv_conf_contents,
    })
}

/// Parses a PEM bundle into the DER-encoded `CERTIFICATE` blocks it
/// contains, in file order -- `kube::Config::root_cert`'s exact shape.
/// Mirrors `kube_client::config::certs` (which is `pub(crate)` to that
/// crate, not reusable directly).
///
/// # Errors
///
/// Returns an error if the bytes are not valid PEM, or contain no
/// `CERTIFICATE` block at all.
fn parse_ca_der(pem_bytes: &[u8]) -> Result<Vec<Vec<u8>>, RuntimeError> {
    let blocks = pem::parse_many(pem_bytes)
        .map_err(|error| RuntimeError::Configuration(format!("proxy client CA: {error}")))?;
    let der: Vec<Vec<u8>> = blocks
        .into_iter()
        .filter(|block| block.tag() == "CERTIFICATE")
        .map(pem::Pem::into_contents)
        .collect();
    if der.is_empty() {
        return Err(RuntimeError::Configuration(
            "proxy client CA contains no CERTIFICATE PEM blocks".into(),
        ));
    }
    Ok(der)
}

/// The cluster's DNS domain suffix (e.g. `cluster.local`), read from
/// `resolv.conf`'s `search` line. Kubelet always writes the search list as
/// `<pod-namespace>.svc.<cluster-domain> svc.<cluster-domain>
/// <cluster-domain>`, in that order, so the *last* entry is the bare
/// cluster domain regardless of what it is actually named -- this makes no
/// assumption that it equals the literal string `cluster.local`.
///
/// This function performs no I/O: the caller reads `resolv.conf` and hands
/// the contents in (see `read_production_startup_files`), the same
/// discipline `engine::resolver_config_from_resolv_conf` documents for the
/// same reason.
fn cluster_domain_from_resolv_conf(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let uncommented = line.split(['#', ';']).next().unwrap_or("");
        let mut tokens = uncommented.split_whitespace();
        if tokens.next() != Some("search") {
            return None;
        }
        tokens.last().map(str::to_owned)
    })
}

pub(crate) async fn run_production(
    plan: &CompiledPlan,
    config: ProductionConfig,
    files: ProductionStartupFiles,
    handlers: &'static [(&'static str, &'static str)],
    lockdown: LockdownState,
) -> Result<(), RuntimeError> {
    validate_production_config(plan, &config)?;
    crate::install_rustls_provider()?;
    let mut kube_config = production_kube_config(&files);
    kube_config.connect_timeout = Some(Duration::from_secs(5));
    kube_config.read_timeout = Some(Duration::from_secs(15));
    kube_config.write_timeout = Some(Duration::from_secs(15));
    kube_config.default_retry = false;
    let client =
        crate::kube_connector::build_client(&kube_config, files.kube_resolver_config.clone())?;
    run_production_with_client(
        plan,
        config,
        client,
        &files.resolv_conf_contents,
        handlers,
        lockdown,
    )
    .await
}

/// Builds the `kube::Config` for the proxy connection entirely from
/// already-in-memory data (see [`ProductionStartupFiles`]): `root_cert` as
/// DER certificates, `auth_info.client_certificate_data`/`client_key_data`
/// as base64 PEM. `root_cert_file`, `auth_info.client_certificate` and
/// `auth_info.client_key` (the file-path fields) are never set -- nothing
/// this `Config` is used to build ever opens a file, including on
/// reconnect (see `kube_connector::build_client`).
fn production_kube_config(files: &ProductionStartupFiles) -> Config {
    let mut kube_config = Config::new(files.cluster_url.clone());
    kube_config.root_cert = Some(files.root_cert_der.clone());
    kube_config.auth_info.client_certificate_data = Some(files.client_certificate_data.clone());
    kube_config.auth_info.client_key_data = Some(files.client_key_data.clone());
    kube_config.tls_server_name = Some(files.server_name.clone());
    kube_config
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
    resolv_conf_contents: &str,
    handlers: &'static [(&'static str, &'static str)],
    lockdown: LockdownState,
) -> Result<(), RuntimeError> {
    let authority = RuntimeAuthority::new(
        client.clone(),
        &config.control_namespace,
        config.binding.clone(),
    )?;
    let diagnostics = Arc::new(Diagnostics::new(&config.binding, lockdown));
    let listener = tokio::net::TcpListener::bind(config.diagnostics_bind)
        .await
        .map_err(|error| RuntimeError::Configuration(format!("diagnostics listener: {error}")))?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_diagnostics(listener, diagnostics.clone(), shutdown_rx.clone());
    let terminating = Arc::new(AtomicBool::new(false));
    spawn_shutdown_listener(shutdown_tx.clone(), terminating.clone());

    let handler_runtime = if plan.ir.handlers.is_empty() {
        None
    } else {
        // `resolv_conf_contents` was already read once, before Landlock
        // lockdown, by `read_production_startup_files` -- `HandlerRuntime::new`
        // itself never reads the file, only parses the contents it is
        // handed. See the Apsides v0.3 plan, Phase 5b review item 3 and
        // Phase 5c.
        let mut runtime = crate::handlers::HandlerRuntime::new(
            plan,
            handlers,
            crate::handlers::handler_field_manager(&plan.ir.domain.id),
            Duration::from_millis(config.base_backoff_millis),
            Duration::from_millis(config.max_backoff_millis),
            Some(resolv_conf_contents),
        )?;
        runtime.seed_from_annotations(plan, &client).await;
        Some(Mutex::new(runtime))
    };

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
        handler_runtime,
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
    /// Whether shutdown has been requested -- checked by
    /// `handlers::HandlerRuntime` (which does not otherwise touch this
    /// struct's fields) immediately before dispatching a handler
    /// invocation, so a shutdown in progress never starts a new one (see
    /// the Apsides v0.3 plan, Phase 5b review item E).
    pub(crate) fn is_terminating(&self) -> bool {
        self.terminating.load(Ordering::Acquire)
    }

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
                        &[],
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
                            &[],
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
                self.wait_to_retry(&mut attempt, &mut shutdown).await?;
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
                conditions,
                reason.as_deref(),
            )
            .await;
        if let Some(handler_runtime) = &self.handler_runtime {
            let statuses = handler_runtime.lock().await.statuses();
            self.diagnostics
                .set_handlers(
                    statuses
                        .into_iter()
                        .map(|status| HandlerDiagnostic {
                            id: status.id,
                            last_run_millis_ago: status.last_run_millis_ago,
                            last_error_code: status.last_error_code,
                            queue_depth: status.queue_depth,
                        })
                        .collect(),
                )
                .await;
        }
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
        self.diagnostics
            .update(
                DiagnosticPhase::Degraded,
                CacheState::Pending,
                &[],
                Some(reason),
            )
            .await;
        self.wait_to_retry(attempt, shutdown).await
    }

    // A completed degraded cycle already recorded its synchronized cache and
    // specific conditions. Waiting must not replace those with an API failure.
    async fn wait_to_retry(
        &self,
        attempt: &mut u32,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), RuntimeError> {
        self.diagnostics.errors.fetch_add(1, Ordering::Relaxed);
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
        // The uid this very cycle's reconciler apply observed for each
        // handler output, keyed by the output's resource id -- NOT
        // re-derived from a later, separate GET. `commit_handler_outputs`
        // binds its commit to this value so a handler output replaced
        // (deleted and recreated under the same name) while the handler
        // ran can never receive that handler's data; see the Apsides v0.3
        // plan, Phase 5b runtime-integration review (uid-binding item).
        let mut output_uids = BTreeMap::new();

        for resource in self
            .plan
            .ir
            .resources
            .iter()
            .filter(|resource| resource.kind == "ConfigMap")
        {
            match self.apply(resource, &resource.desired, inventory).await {
                Ok(observation) => {
                    if resource.handler_output {
                        output_uids.insert(resource.id.clone(), observation.uid.clone());
                    }
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
        if let Some(handler_runtime) = &self.handler_runtime {
            let mut handler_runtime = handler_runtime.lock().await;
            let new_conditions = handler_runtime
                .run_cycle(
                    &self.plan,
                    &current,
                    &applied,
                    &output_uids,
                    &self.kube.client,
                    self,
                )
                .await?;
            conditions.extend(new_conditions);
        }
        for resource in self
            .plan
            .ir
            .resources
            .iter()
            .filter(|resource| resource.kind != "ConfigMap")
        {
            let reacts = self
                .plan
                .ir
                .reactions
                .iter()
                .any(|reaction| reaction.target == resource.id);
            let mut desired = resource.desired.clone();
            if reacts {
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
        conditions.truncate(MAX_DIAGNOSTIC_CONDITIONS);
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
        let api = self.kube.resource_api(resource)?;
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
                &[],
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
        &controller.kube.field_manager,
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
    let request = conditional_apply_request(
        desired,
        &resource.name,
        &resource.namespace,
        &controller.kube.field_manager,
        resource.force,
        ApplyPrecondition::Absent {
            list_resource_version: list_version,
        },
    )?;
    let dry_run = request.dry_run_params();
    api.patch(&resource.name, &dry_run, &Patch::Apply(request.payload()))
        .await
        .map_err(|error| write_api_error("conditional-create-dry-run", resource, &error))?;
    if controller.terminating.load(Ordering::Acquire) {
        return Err(RuntimeError::Terminating);
    }
    let current = controller.authority.verify(&controller.plan).await?;
    ensure_same_claim(approved, approved_claim(&current, resource)?)?;
    let created = api
        .patch(
            &resource.name,
            request.params(),
            &Patch::Apply(request.payload()),
        )
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

/// The authority/claim re-check reconciler mutations use before their real
/// (non-dry-run) SSA write: re-verify the activation, and confirm the
/// claim this write was approved under has not changed since. Generalized
/// over `field_manager` (rather than always using
/// `controller.kube.field_manager`) so `commit_handler_outputs` can reuse
/// this exact path, under the distinct `apsides-handler-<domainId>`
/// manager, instead of duplicating it -- see the Apsides v0.3 plan, Phase
/// 5b review item E.
///
/// `last_successful_apply`'s cache-skip is keyed only by resource identity,
/// not by `field_manager`, so a resource written by two different managers
/// (a handler output: metadata by the reconciler, `data` by the handler)
/// shares one cache slot -- interleaving between the two managers just
/// occasionally misses the early-exit and falls through to an extra
/// dry-run round-trip; `already_owned`/the dry-run-vs-existing comparison
/// below are both scoped to `field_manager` and stay correct regardless.
#[allow(
    clippy::too_many_arguments,
    reason = "each argument is a distinct, independently-owned piece of one write's identity/claim/precondition state; bundling them into a struct would only move the same field list one level up"
)]
async fn apply_existing_claimed(
    controller: &ProductionController,
    api: &kube::Api<DynamicObject>,
    resource: &Resource,
    desired: &Value,
    approved: &ApprovedClaim,
    expected_uid: &str,
    existing: DynamicObject,
    field_manager: &str,
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
    let already_owned =
        validate_exact_apply_owner(&existing, resource, field_manager, "claim preflight").is_ok();
    if already_owned
        && controller
            .successful_apply_is_current(resource, desired, &existing)
            .await?
    {
        return Ok(existing);
    }
    let request = conditional_apply_request(
        desired,
        &resource.name,
        &resource.namespace,
        field_manager,
        resource.force,
        ApplyPrecondition::Existing {
            uid: expected_uid,
            resource_version: version,
        },
    )?;
    let dry_run = request.dry_run_params();
    let projected = api
        .patch(&resource.name, &dry_run, &Patch::Apply(request.payload()))
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
        .patch(
            &resource.name,
            request.params(),
            &Patch::Apply(request.payload()),
        )
        .await
        .map_err(|error| write_api_error("conditional-apply", resource, &error))?;
    if applied.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(RuntimeError::Kubernetes(format!(
            "conditional-apply-uid-mismatch:{}/{}",
            resource.kind, resource.name
        )));
    }
    validate_exact_apply_owner(&applied, resource, field_manager, "conditional claim SSA")?;
    controller
        .diagnostics
        .mutations
        .fetch_add(1, Ordering::Relaxed);
    Ok(applied)
}

/// Commits one successful handler invocation's buffered output writes.
///
/// One server-side apply per output the handler *declared*
/// (`handler_outputs`, not just the ones it actually wrote this
/// invocation), each containing that output's *complete* `data` map plus
/// the `apsides.dev/handler-input-revision` annotation -- see the Apsides
/// v0.3 plan, Phase 5b review item 1. An output the handler wrote this
/// invocation gets exactly the keys `written` holds for it; an output it
/// did **not** write is left untouched *and* re-applied with its own
/// current live `data` unchanged, so the annotation stamp never drops data
/// a previous invocation already wrote there.
///
/// Every commit goes through [`apply_existing_claimed`] -- the SAME
/// authority/claim re-check reconciler mutations use -- under
/// `field_manager` (`apsides-handler-<domainId>`, distinct from the
/// reconciler's own manager), so a handler commit can never bypass that
/// re-check (review item E; "reuse that path, don't duplicate it").
/// `controller.is_terminating()` is checked before each output's commit,
/// not just once for the whole batch, so a shutdown mid-commit stops as
/// early as possible.
///
/// If any output's commit fails, the whole invocation counts as failed
/// (the caller does not advance `known_revision` or clear backoff) --
/// outputs already committed earlier in this same call keep whatever they
/// were just given, rather than being rolled back. That does not leave a
/// silently-inconsistent "done" state: every declared output must record
/// the *same* revision for the handler to ever be treated as up to date
/// again (`HandlerRuntime::seed_from_annotations`/`outputs_still_agree`),
/// so a partial commit here always shows up as "must re-run", not as
/// success.
///
/// Returns the committed `uid` of every output actually committed, keyed
/// by output id, for the caller to refresh `HandlerState::output_uids`.
///
/// `output_uids` binds each output to the uid *this reconcile cycle's own
/// reconciler apply* observed for it (see `reconcile`'s `output_uids`
/// map) -- never a uid re-derived from this function's own GET of the
/// live object, which would make the uid precondition trivially true
/// against itself. If the live object's uid no longer matches, the
/// output was deleted and recreated (or otherwise replaced) while the
/// handler ran, and the commit is refused with a
/// `handler-output-replaced` error instead of silently writing the
/// handler's data into the replacement object; the caller retries the
/// same event next cycle, once the reconciler has observed the
/// replacement afresh.
///
/// # Errors
///
/// Returns `Err` if an output id is undeclared, has no backing resource,
/// no approved claim or no bound uid from this cycle, if the live
/// object's uid no longer matches that bound uid
/// (`handler-output-replaced`), or if any step of its commit fails --
/// including `RuntimeError::Terminating` if shutdown was requested.
pub(crate) async fn commit_handler_outputs(
    controller: &ProductionController,
    handler_outputs: &[String],
    outputs_by_id: &BTreeMap<String, compiler_core::Output>,
    field_manager: &str,
    written: &crate::handlers::BufferedWrites,
    revision: &str,
    output_uids: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, RuntimeError> {
    let mut committed_uids = BTreeMap::new();
    for output_id in handler_outputs {
        if controller.terminating.load(Ordering::Acquire) {
            return Err(RuntimeError::Terminating);
        }
        let output = outputs_by_id.get(output_id).ok_or_else(|| {
            RuntimeError::Plan(format!("undeclared handler output id {output_id:?}"))
        })?;
        let resource = controller
            .plan
            .ir
            .resources
            .iter()
            .find(|resource| resource.id == *output_id)
            .ok_or_else(|| {
                RuntimeError::Plan(format!(
                    "no backing resource for handler output {output_id:?}"
                ))
            })?;
        let bound_uid = output_uids
            .get(output_id)
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| {
                RuntimeError::Kubernetes(format!(
                    "handler-output-uid-unbound:{}/{}",
                    resource.kind, resource.name
                ))
            })?;
        let api = controller.kube.resource_api(resource)?;
        // The raw `kube::Error` is discarded, not interpolated: an API
        // error body can echo back request/response content, and this
        // reason ends up in the world-readable diagnostics conditions
        // list -- the same discipline `write_api_error` and the
        // reconciler's own ownership-preflight GET/LIST calls already
        // apply to every other Kubernetes error this crate surfaces.
        let existing = api.get(&output.name).await.map_err(|_error| {
            RuntimeError::Kubernetes(format!(
                "handler-output-read-failed:{}/{}",
                resource.kind, resource.name
            ))
        })?;
        if existing.metadata.uid.as_deref() != Some(bound_uid.as_str()) {
            return Err(RuntimeError::Kubernetes(format!(
                "handler-output-replaced:{}/{}",
                resource.kind, resource.name
            )));
        }
        let uid = bound_uid.clone();
        let data: serde_json::Map<String, Value> = written.get(output_id).map_or_else(
            || handler_owned_data_keys(&existing, field_manager),
            |keys| {
                keys.iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect()
            },
        );
        let desired = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": output.name,
                "namespace": output.namespace,
                "annotations": {
                    crate::handlers::HANDLER_INPUT_REVISION_ANNOTATION: revision,
                },
            },
            "data": Value::Object(data),
        });
        let initial = controller.authority.verify(&controller.plan).await?;
        let approved = approved_claim(&initial, resource)?.clone();
        let applied = apply_existing_claimed(
            controller,
            &api,
            resource,
            &desired,
            &approved,
            &uid,
            existing,
            field_manager,
        )
        .await?;
        controller
            .remember_successful_apply(resource, &desired, &applied)
            .await?;
        let applied_uid = applied
            .metadata
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .unwrap_or(uid);
        committed_uids.insert(output_id.clone(), applied_uid);
    }
    Ok(committed_uids)
}

/// The subset of `existing`'s live `data` map that `field_manager`
/// currently owns, read from `existing`'s own `managedFields` rather than
/// assumed -- the actual writeOutput contract (see
/// `docs/reference/IR-CONTRACT.md`): re-applying an output a handler did
/// not write this invocation re-asserts only the handler's own
/// previously-written keys, never a foreign field manager's keys, so the
/// handler never becomes a co-owner of data another writer put there.
///
/// Empty (no keys, or no matching `Apply` entry) when the handler has
/// never written this output before -- there is nothing yet for it to
/// own.
fn handler_owned_data_keys(
    existing: &DynamicObject,
    field_manager: &str,
) -> serde_json::Map<String, Value> {
    let owned_keys: BTreeSet<String> =
        manager_main_resource_entries(existing, field_manager, "Apply")
            .into_iter()
            .filter_map(|entry| entry.fields_v1.as_ref())
            .filter_map(|fields| fields.0.get("f:data"))
            .filter_map(Value::as_object)
            .flat_map(serde_json::Map::keys)
            .filter_map(|key| key.strip_prefix("f:"))
            .map(str::to_owned)
            .collect();
    let live_data = existing.data.get("data").and_then(Value::as_object);
    owned_keys
        .into_iter()
        .filter_map(|key| {
            let value = live_data?.get(&key)?.clone();
            Some((key, value))
        })
        .collect()
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
    deploy::resource_identity(resource).map_err(|error| RuntimeError::Plan(error.to_string()))
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
                            matches!(input.mode, InputMode::Observed | InputMode::Output)
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
    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
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
    fn write_diagnostics_preserve_status_categories_without_api_values() -> TestResult {
        let plan = plan()?;
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
        Ok(())
    }

    type Calls = Arc<StdMutex<Vec<(Method, String, Value)>>>;

    fn plan() -> TestResult<CompiledPlan> {
        Ok(parse_and_validate(&json!({
            "schemaVersion":"apsides.dev/ir/v2", "testOnly":false,
            "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262_144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
            "resources":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"config","namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload"},"data":{"value":"approved"}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}}],
            "inputs":[],"reactions":[],"handlers":[],"outputs":[],"provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
        }).to_string())?)
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
            image_pull_secrets: vec![],
            schema_version: SCHEMA_VERSION.into(),
            runtime_ceiling: ceiling.clone(),
            cleanup_ceiling: ceiling,
        }
    }

    fn activation(
        plan: &CompiledPlan,
        policy: &PolicyState,
        uid: Option<&str>,
    ) -> TestResult<ActivationState> {
        Ok(ActivationState {
            schema_version: SCHEMA_VERSION.into(),
            binding: RuntimeBinding {
                domain_id: "example".into(),
                cluster_binding: "cluster-a".into(),
                generation_id: "generation-a".into(),
                build_digest: format!("sha256:{}", "a".repeat(64)),
                ir_digest: plan.ir_digest.clone(),
                policy_digest: digest(policy)?,
                capability_digest: digest(&plan.capabilities)?,
                service_account_name: "apsides-example-generation-a".into(),
                controller_image: format!("example.invalid/controller@sha256:{}", "b".repeat(64)),
                max_handler_heap_bytes: 0,
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
        })
    }

    fn control_object(
        name: &str,
        state: &impl Serialize,
        uid: &str,
        rv: &str,
    ) -> TestResult<Value> {
        Ok(
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":"control","uid":uid,"resourceVersion":rv},"data":{"state.json":serde_json::to_string(state)?}}),
        )
    }

    fn target_object(value: &str, uid: &str, rv: &str) -> Value {
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload","uid":uid,"resourceVersion":rv,
            "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{"f:value":{}}},"manager":"apsides-example","operation":"Apply"}]},"data":{"value":value}})
    }

    fn target_list(items: &[Value], rv: &str) -> Value {
        json!({"apiVersion":"v1","kind":"ConfigMapList","metadata":{"resourceVersion":rv},"items":items})
    }

    fn inventory(
        activation_uid: &str,
        activation_rv: &str,
        observations: Vec<RuntimeObservation>,
    ) -> TestResult<RuntimeInventory> {
        Ok(RuntimeInventory {
            schema_version: SCHEMA_VERSION.into(),
            binding: activation(&plan()?, &policy(&plan()?), None)?.binding,
            activation_uid: activation_uid.into(),
            activation_resource_version: activation_rv.into(),
            observations,
            conditions: vec![],
            drained: false,
        })
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

    /// A fixture problem inside the mock transport, which itself cannot fail.
    type FixtureError = Box<dyn std::error::Error + Send + Sync>;

    /// The transport signature is infallible, so a fixture problem becomes a
    /// 500 naming it, which the assertions then report.
    fn fixture_failure(error: &FixtureError) -> Response<Body> {
        Response::builder()
            .status(500)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "fixtureError": error.to_string() })
                    .to_string()
                    .into_bytes(),
            ))
            .unwrap_or_else(|_| Response::new(Body::from(Vec::new())))
    }

    async fn wire_response(
        request: Request<Body>,
        responses: &StdMutex<VecDeque<Value>>,
        calls: &Calls,
    ) -> std::result::Result<Response<Body>, FixtureError> {
        let (parts, body) = request.into_parts();
        let bytes = body.collect().await?.to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        calls
            .lock()
            .map_err(|_| "wire call log is poisoned")?
            .push((parts.method, parts.uri.to_string(), value));
        let response = responses
            .lock()
            .map_err(|_| "queued responses are poisoned")?
            .pop_front()
            .ok_or("fixture ran out of queued responses")?;
        Ok(Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&response)?))?)
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
                    Ok::<_, Infallible>(match wire_response(request, &responses, &calls).await {
                        Ok(response) => response,
                        Err(error) => fixture_failure(&error),
                    })
                }
            }
        });
        (Client::new(service, "control"), calls)
    }

    fn controller(
        plan: &CompiledPlan,
        activation: &ActivationState,
        client: Client,
    ) -> TestResult<ProductionController> {
        let config = ProductionConfig {
            api_access: ApiAccess::Proxy,
            control_namespace: "control".into(),
            binding: activation.binding.clone(),
            poll_interval_millis: 100,
            base_backoff_millis: 100,
            max_backoff_millis: 1_000,
            min_mutation_interval_millis: 10,
            diagnostics_bind: "127.0.0.1:0".parse()?,
        };
        Ok(ProductionController {
            plan: plan.clone(),
            authority: RuntimeAuthority::new(
                client.clone(),
                "control",
                activation.binding.clone(),
            )?,
            kube: KubeRuntimeClient::from_client(client, 2, "example"),
            diagnostics: Arc::new(Diagnostics::new(
                &activation.binding,
                LockdownState::Landlock,
            )),
            terminating: Arc::new(AtomicBool::new(false)),
            last_mutation: Mutex::new(None),
            last_successful_apply: Mutex::new(BTreeMap::new()),
            handler_runtime: None,
            config,
        })
    }

    #[test]
    fn backoff_and_poll_bounds_are_finite() {
        assert_eq!(MAX_POLL_MILLIS, 30_000);
        assert_eq!(MAX_BACKOFF_MILLIS, 60_000);
        assert_eq!(MAX_MUTATION_INTERVAL_MILLIS, 60_000);
    }

    #[test]
    fn api_access_rejects_ambient_alternatives() {
        assert!(serde_json::from_str::<ApiAccess>("\"proxy\"").is_ok());
        assert!(serde_json::from_str::<ApiAccess>("\"inCluster\"").is_err());
        assert!(serde_json::from_str::<ApiAccess>("\"kubeconfig\"").is_err());
        assert!(serde_json::from_str::<ApiAccess>("{\"proxy\":\"https://example\"}").is_err());
    }

    /// A syntactically valid, arbitrary (not cryptographically real) PEM
    /// `CERTIFICATE` block -- `parse_ca_der` only checks PEM structure and
    /// the block tag, never certificate validity (the same as
    /// `kube::Config::root_cert`'s own contract: it is trusted TLS
    /// material, not independently re-validated at `Config`-construction
    /// time).
    const TEST_CA_PEM: &str =
        "-----BEGIN CERTIFICATE-----\nZmFrZS1jYS1kYXRh\n-----END CERTIFICATE-----\n";

    fn test_startup_files() -> TestResult<ProductionStartupFiles> {
        Ok(ProductionStartupFiles {
            root_cert_der: parse_ca_der(TEST_CA_PEM.as_bytes())?,
            client_certificate_data: base64::engine::general_purpose::STANDARD
                .encode(b"fake-cert-pem"),
            client_key_data: SecretString::from(
                base64::engine::general_purpose::STANDARD.encode(b"fake-key-pem"),
            ),
            server_name: "apsides-proxy-generation-a.control.svc".into(),
            cluster_url: "https://apsides-proxy-generation-a.control.svc.cluster.local:8443"
                .parse()
                .map_err(|error| format!("{error}"))?,
            kube_resolver_config: ResolverConfig::from_parts(
                None,
                vec![],
                vec![hickory_resolver::config::NameServerConfig::udp_and_tcp(
                    "127.0.0.1"
                        .parse::<std::net::IpAddr>()
                        .map_err(|e| e.to_string())?,
                )],
            ),
            resolv_conf_contents: "nameserver 127.0.0.1\n".into(),
        })
    }

    /// `production_kube_config` builds a `kube::Config` entirely from
    /// [`ProductionStartupFiles`]'s in-memory fields: `root_cert` as DER,
    /// `client_certificate_data`/`client_key_data` as base64 PEM, and
    /// (critically for the Apsides v0.3 plan, Phase 5c) never sets
    /// `root_cert_file`, `auth_info.client_certificate` or
    /// `auth_info.client_key` -- the three file-path fields that would
    /// make `kube-client` open a file again after Landlock lockdown.
    #[test]
    fn production_kube_config_uses_only_in_memory_mtls_identity() -> TestResult {
        let files = test_startup_files()?;
        let config = production_kube_config(&files);
        assert_eq!(
            config.cluster_url.to_string(),
            "https://apsides-proxy-generation-a.control.svc.cluster.local:8443/"
        );
        assert_eq!(
            config.tls_server_name.as_deref(),
            Some("apsides-proxy-generation-a.control.svc")
        );
        assert!(config.root_cert_file.is_none());
        assert!(config.auth_info.client_certificate.is_none());
        assert!(config.auth_info.client_key.is_none());
        assert_eq!(config.root_cert, Some(vec![b"fake-ca-data".to_vec()]));
        assert_eq!(
            config.auth_info.client_certificate_data.as_deref(),
            Some(
                base64::engine::general_purpose::STANDARD
                    .encode(b"fake-cert-pem")
                    .as_str()
            )
        );
        assert_eq!(
            config
                .auth_info
                .client_key_data
                .as_ref()
                .map(ExposeSecret::expose_secret),
            Some(
                base64::engine::general_purpose::STANDARD
                    .encode(b"fake-key-pem")
                    .as_str()
            )
        );
        assert!(config.auth_info.token.is_none());
        assert!(config.auth_info.token_file.is_none());
        Ok(())
    }

    #[test]
    fn parse_ca_der_extracts_only_certificate_blocks() -> TestResult {
        let der = parse_ca_der(TEST_CA_PEM.as_bytes())?;
        assert_eq!(der, vec![b"fake-ca-data".to_vec()]);
        Ok(())
    }

    #[test]
    fn parse_ca_der_rejects_a_bundle_with_no_certificate_block() {
        let key_only = "-----BEGIN PRIVATE KEY-----\nZmFrZQ==\n-----END PRIVATE KEY-----\n";
        assert!(parse_ca_der(key_only.as_bytes()).is_err());
    }

    #[test]
    fn cluster_domain_is_the_last_search_entry() {
        let contents = "nameserver 10.0.0.10\nsearch control.svc.cluster.local svc.cluster.local cluster.local\noptions ndots:5\n";
        assert_eq!(
            cluster_domain_from_resolv_conf(contents).as_deref(),
            Some("cluster.local")
        );
    }

    #[test]
    fn cluster_domain_is_none_without_a_search_line() {
        assert_eq!(
            cluster_domain_from_resolv_conf("nameserver 10.0.0.10\n"),
            None
        );
    }

    /// `startup_reads_before_lock` (see the Apsides v0.3 plan, Phase 5c):
    /// `/etc/apsides/proxy/ca.crt` does not exist in this test environment
    /// (nothing in this repository's test suites creates it -- only a real
    /// platform executor does, inside a real pod), so this exercises the
    /// real, unmocked read path and its real fail-closed error, not a
    /// simulated one. Proves the read happens (and fails clearly) entirely
    /// independent of Landlock: nothing about this test applies any
    /// lockdown at all.
    #[test]
    fn read_production_startup_files_fails_clearly_when_the_proxy_ca_is_missing() -> TestResult {
        let plan = plan()?;
        let config = ProductionConfig {
            api_access: ApiAccess::Proxy,
            control_namespace: plan.ir.domain.control_namespace.clone(),
            binding: RuntimeBinding {
                domain_id: plan.ir.domain.id.clone(),
                cluster_binding: plan.ir.domain.cluster_binding.clone(),
                generation_id: "generation-a".into(),
                build_digest: format!("sha256:{}", "1".repeat(64)),
                ir_digest: plan.ir_digest,
                policy_digest: format!("sha256:{}", "3".repeat(64)),
                capability_digest: format!("sha256:{}", "4".repeat(64)),
                service_account_name: "controller".into(),
                controller_image: format!("example/controller@sha256:{}", "5".repeat(64)),
                max_handler_heap_bytes: 0,
            },
            poll_interval_millis: 1_000,
            base_backoff_millis: 100,
            max_backoff_millis: 1_000,
            min_mutation_interval_millis: 10,
            diagnostics_bind: "127.0.0.1:0".parse().map_err(|e| format!("{e}"))?,
        };
        let error = read_production_startup_files(&config)
            .err()
            .ok_or("expected a missing-file error")?;
        assert!(
            error.to_string().contains("proxy client CA"),
            "expected the error to name the missing file: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn diagnostics_never_include_input_values_or_key_material() -> TestResult {
        let diagnostics = Arc::new(Diagnostics::new(
            &RuntimeBinding {
                domain_id: "domain".into(),
                cluster_binding: "cluster".into(),
                generation_id: "generation".into(),
                build_digest: format!("sha256:{}", "1".repeat(64)),
                ir_digest: format!("sha256:{}", "2".repeat(64)),
                policy_digest: format!("sha256:{}", "3".repeat(64)),
                capability_digest: format!("sha256:{}", "4".repeat(64)),
                service_account_name: "controller".into(),
                controller_image: format!("example/controller@sha256:{}", "5".repeat(64)),
                max_handler_heap_bytes: 0,
            },
            LockdownState::Landlock,
        ));
        diagnostics
            .update(
                DiagnosticPhase::Ready,
                CacheState::Synchronized,
                &[RuntimeCondition {
                    reason_code: "input-unavailable".into(),
                    resource_name: Some("target".into()),
                }],
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
                &[RuntimeCondition {
                    reason_code: "input-unavailable".into(),
                    resource_name: Some("target".into()),
                }],
                Some("input-unavailable"),
            )
            .await;
        let request = Request::builder().uri("/status").body(())?;
        let response = diagnostics_response(request, diagnostics).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await?.to_bytes();
        let body = std::str::from_utf8(&body)?;
        assert!(!body.contains("secret-value"));
        assert!(!body.contains("rollout-key"));
        let body: Value = serde_json::from_str(body)?;
        assert_eq!(body["applyCacheEntries"], 2);
        assert_eq!(body["applyCacheSerializedBytes"], 123);
        assert_eq!(body["pendingCoalescedTriggers"], 0);
        assert_eq!(body["triggerToCycleMicrosCount"], 1);
        assert_eq!(body["triggerToCycleMicrosSum"], 7);
        assert_eq!(body["triggerToCycleMicrosMax"], 7);
        Ok(())
    }

    async fn diagnostic_json(
        diagnostics: &Arc<Diagnostics>,
        path: &str,
    ) -> Result<(StatusCode, Value), Box<dyn std::error::Error + Send + Sync>> {
        let request = Request::builder().uri(path).body(())?;
        let response = diagnostics_response(request, diagnostics.clone()).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        Ok((status, serde_json::from_slice(&body)?))
    }

    #[tokio::test]
    async fn completed_cycle_conditions_remain_visible_during_backoff()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let runtime = controller(&plan, &activation, wire(vec![]).0)?;
        let mut last_reason = None;
        let mut attempt = 0;
        for (index, reason) in ["input-missing", "field-or-version-conflict"]
            .into_iter()
            .enumerate()
        {
            runtime
                .record_cycle(
                    &[
                        RuntimeCondition {
                            reason_code: reason.into(),
                            resource_name: Some("target-a".into()),
                        },
                        RuntimeCondition {
                            reason_code: "input-unavailable".into(),
                            resource_name: Some("target-b".into()),
                        },
                    ],
                    &mut last_reason,
                )
                .await;
            let (shutdown, mut receiver) = watch::channel(false);
            let waiting = runtime.wait_to_retry(&mut attempt, &mut receiver);
            tokio::pin!(waiting);
            if !matches!(
                futures_util::poll!(waiting.as_mut()),
                std::task::Poll::Pending
            ) {
                return Err("degraded cycle did not enter retry wait".into());
            }
            let (status, body) = diagnostic_json(&runtime.diagnostics, "/status").await?;
            if status != StatusCode::OK
                || body["phase"] != "degraded"
                || body["cache"] != "synchronized"
                || body["blockedTargets"] != 2
                || body["lastReasonCode"] != reason
                || body["conditions"]
                    != json!([
                        {"reasonCode": reason, "resourceId": "target-a"},
                        {"reasonCode": "input-unavailable", "resourceId": "target-b"},
                    ])
                || body["fieldOrVersionConflicts"] != json!(index)
                || body["errors"] != json!(index + 1)
            {
                return Err("retry wait erased completed cycle diagnostics".into());
            }
            if diagnostic_json(&runtime.diagnostics, "/readyz").await?.0
                != StatusCode::SERVICE_UNAVAILABLE
                || diagnostic_json(&runtime.diagnostics, "/livez").await?.0 != StatusCode::OK
            {
                return Err("degraded cycle probe status differs".into());
            }
            shutdown.send(true)?;
            waiting.await?;
        }
        runtime.record_cycle(&[], &mut last_reason).await;
        let (_, body) = diagnostic_json(&runtime.diagnostics, "/status").await?;
        if body["phase"] != "ready"
            || body["cache"] != "synchronized"
            || body["blockedTargets"] != 0
            || body["conditions"] != json!([])
            || body["fieldOrVersionConflicts"] != 0
            || !body["lastReasonCode"].is_null()
            || diagnostic_json(&runtime.diagnostics, "/readyz").await?.0 != StatusCode::OK
        {
            return Err("successful cycle did not clear degraded diagnostics".into());
        }
        Ok(())
    }

    #[tokio::test]
    async fn operational_retry_replaces_stale_completed_cycle_diagnostics()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let runtime = controller(&plan, &activation, wire(vec![]).0)?;
        runtime
            .record_cycle(
                &[RuntimeCondition {
                    reason_code: "field-or-version-conflict".into(),
                    resource_name: Some("target".into()),
                }],
                &mut None,
            )
            .await;
        let (shutdown, mut receiver) = watch::channel(false);
        let mut attempt = 0;
        let waiting = runtime.retry(&mut attempt, "activation-unavailable", &mut receiver);
        tokio::pin!(waiting);
        if !matches!(
            futures_util::poll!(waiting.as_mut()),
            std::task::Poll::Pending
        ) {
            return Err("operational failure did not enter retry wait".into());
        }
        let (_, body) = diagnostic_json(&runtime.diagnostics, "/status").await?;
        if body["phase"] != "degraded"
            || body["cache"] != "pending"
            || body["blockedTargets"] != 0
            || body["conditions"] != json!([])
            || body["fieldOrVersionConflicts"] != 0
            || body["lastReasonCode"] != "activation-unavailable"
            || body["errors"] != 1
            || diagnostic_json(&runtime.diagnostics, "/readyz").await?.0
                != StatusCode::SERVICE_UNAVAILABLE
        {
            return Err("operational failure retained stale cycle diagnostics".into());
        }
        shutdown.send(true)?;
        waiting.await?;
        Ok(())
    }

    #[tokio::test]
    async fn diagnostic_conditions_are_bounded_and_exclude_api_values()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let runtime = controller(&plan, &activation, wire(vec![]).0)?;
        let resource = plan.ir.resources.first().ok_or("test resource missing")?;
        let api_error = kube::Error::Api(
            kube::core::Status::failure("private-api-value", "private-api-value")
                .with_code(409)
                .boxed(),
        );
        let error = write_api_error("conditional-apply", resource, &api_error);
        let mut conditions = vec![condition_for(resource, &error); MAX_DIAGNOSTIC_CONDITIONS + 1];
        let first = conditions.first_mut().ok_or("test condition missing")?;
        first.reason_code = "input-missing".into();
        runtime.record_cycle(&conditions, &mut None).await;
        let (_, body) = diagnostic_json(&runtime.diagnostics, "/status").await?;
        let retained = body["conditions"].as_array().ok_or("conditions missing")?;
        if retained.len() != MAX_DIAGNOSTIC_CONDITIONS
            || body["blockedTargets"] != MAX_DIAGNOSTIC_CONDITIONS
            || body["fieldOrVersionConflicts"] != MAX_DIAGNOSTIC_CONDITIONS - 1
            || retained
                .first()
                .and_then(|entry| entry["reasonCode"].as_str())
                != Some("input-missing")
            || retained
                .iter()
                .any(|entry| entry["resourceId"] != resource.id)
            || body.to_string().contains("private-api-value")
        {
            return Err("diagnostic conditions lost bounds or disclosed API values".into());
        }
        for phase in [DiagnosticPhase::Draining, DiagnosticPhase::Inactive] {
            runtime
                .diagnostics
                .update(phase, CacheState::Pending, &[], Some("activation-inactive"))
                .await;
            let (_, body) = diagnostic_json(&runtime.diagnostics, "/status").await?;
            if body["conditions"] != json!([])
                || body["blockedTargets"] != 0
                || body["fieldOrVersionConflicts"] != 0
            {
                return Err("inactive diagnostic state retained old conditions".into());
            }
        }
        Ok(())
    }

    fn handler_output_plan() -> TestResult<CompiledPlan> {
        Ok(parse_and_validate(&json!({
            "schemaVersion": compiler_core::IR_SCHEMA, "testOnly": false,
            "domain": {"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262_144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
            "resources":[{"id":"flags","apiVersion":"v1","kind":"ConfigMap","name":"flags","namespace":"workload","handlerOutput":true,"desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"flags","namespace":"workload","labels":{"apsides.dev/handler-output":"flags"}}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}}],
            "inputs":[{"id":"flags-output","apiVersion":"v1","kind":"ConfigMap","name":"flags","namespace":"workload","mode":"output","managedResourceId":"flags","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.ts","line":1,"column":1}}],
            "reactions":[],"handlers":[],"outputs":[{"id":"flags","name":"flags","namespace":"workload"}],
            "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.ts","sha256":"0".repeat(64)}]},
        }).to_string())?)
    }

    fn handler_output_activation(
        plan: &CompiledPlan,
        policy: &PolicyState,
    ) -> TestResult<ActivationState> {
        Ok(ActivationState {
            schema_version: SCHEMA_VERSION.into(),
            binding: RuntimeBinding {
                domain_id: "example".into(),
                cluster_binding: "cluster-a".into(),
                generation_id: "generation-a".into(),
                build_digest: format!("sha256:{}", "a".repeat(64)),
                ir_digest: plan.ir_digest.clone(),
                policy_digest: digest(policy)?,
                capability_digest: digest(&plan.capabilities)?,
                service_account_name: "apsides-example-generation-a".into(),
                controller_image: format!("example.invalid/controller@sha256:{}", "b".repeat(64)),
                max_handler_heap_bytes: 0,
            },
            status: ActivationStatus::Active,
            approved_claims: vec![ApprovedClaim {
                claim: ProtectedClaim {
                    identity: ResourceIdentity {
                        api_group: String::new(),
                        resource: "configmaps".into(),
                        namespace: "workload".into(),
                        name: "flags".into(),
                    },
                    domain_id: "example".into(),
                    generation_id: "generation-a".into(),
                    transition_id: "transition-a".into(),
                    state: ClaimState::Active,
                    uid: None,
                },
                deletion_policy: compiler_core::DeletionPolicy::Orphan,
                allow_create: true,
            }],
            rollout_key_id: None,
        })
    }

    fn handler_output_object(uid: &str, rv: &str) -> Value {
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"flags","namespace":"workload","uid":uid,"resourceVersion":rv,
            "labels":{"apsides.dev/handler-output":"flags"},
            "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:metadata":{"f:labels":{}}},"manager":"apsides-example","operation":"Apply"}]}})
    }

    fn handler_output_inventory(
        binding: &RuntimeBinding,
        activation_uid: &str,
        activation_rv: &str,
        observations: Vec<RuntimeObservation>,
    ) -> RuntimeInventory {
        RuntimeInventory {
            schema_version: SCHEMA_VERSION.into(),
            binding: binding.clone(),
            activation_uid: activation_uid.into(),
            activation_resource_version: activation_rv.into(),
            observations,
            conditions: vec![],
            drained: false,
        }
    }

    /// The Apsides v0.3 plan, Phase 5b: "The reconciler's own SSA of
    /// outputs must still never claim `data`." Drives `apply()` -- the
    /// reconciler's real apply path, the same one `reconcile()`'s
    /// `ConfigMap` loop uses for every handler-output resource -- against a
    /// handler-output resource whose `desired` carries only `metadata`
    /// (as the compiler always emits for one; see `validate.rs`'s
    /// `validate_handler_output_label`), and asserts every request body
    /// this crate ever sends for it lacks a `data` key, using field
    /// manager `apsides-<domainId>` (the reconciler's own), never
    /// `apsides-handler-<domainId>`.
    #[tokio::test]
    async fn reconcilers_own_apply_of_a_handler_output_never_claims_data() -> TestResult {
        let plan = handler_output_plan()?;
        let policy = policy(&plan);
        let activation = handler_output_activation(&plan, &policy)?;
        let previous_value =
            handler_output_inventory(&activation.binding, "activation-uid", "19", vec![]);
        let mut previous = versioned_inventory(previous_value.clone(), "18");
        let observation = RuntimeObservation {
            identity: ResourceIdentity {
                api_group: String::new(),
                resource: "configmaps".into(),
                namespace: "workload".into(),
                name: "flags".into(),
            },
            uid: "flags-uid".into(),
        };
        let next_value = handler_output_inventory(
            &activation.binding,
            "activation-uid",
            "20",
            vec![observation],
        );
        let responses = vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(&[], "30"),
            handler_output_object("flags-uid", "31"),
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            handler_output_object("flags-uid", "32"),
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object(
                "apsides-inventory-example",
                &previous_value,
                "inventory-uid",
                "18",
            )?,
            control_object(
                "apsides-inventory-example",
                &next_value,
                "inventory-uid",
                "21",
            )?,
        ];
        let (client, calls) = wire(responses);
        let controller = controller(&plan, &activation, client)?;
        let observed = controller
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await?;
        assert_eq!(observed.uid, "flags-uid");
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        let patches: Vec<_> = calls
            .iter()
            .filter(|call| call.0 == Method::PATCH)
            .collect();
        assert_eq!(
            patches.len(),
            2,
            "expected exactly the dry-run and real create PATCH"
        );
        for (method, query, body) in &patches {
            let _ = method;
            assert!(
                query.contains("fieldManager=apsides-example"),
                "handler-output resources are still owned by the reconciler's own field manager, query={query}"
            );
            assert!(
                !query.contains("apsides-handler"),
                "the reconciler's own apply must never use the handler field manager, query={query}"
            );
            assert!(
                body.get("data").is_none(),
                "the reconciler's own SSA of a handler output must never claim data, body={body}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn claimed_absent_create_reverifies_activation_after_dry_run() -> TestResult {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let previous_value = inventory("activation-uid", "19", vec![])?;
        let mut previous = versioned_inventory(previous_value.clone(), "18");
        let next_value = inventory("activation-uid", "20", vec![target_observation()])?;
        let responses = vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(&[], "30"),
            target_object("approved", "target-uid", "31"),
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_object("approved", "target-uid", "32"),
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object(
                "apsides-inventory-example",
                &previous_value,
                "inventory-uid",
                "18",
            )?,
            control_object(
                "apsides-inventory-example",
                &next_value,
                "inventory-uid",
                "21",
            )?,
        ];
        let (client, calls) = wire(responses);
        let controller = controller(&plan, &activation, client)?;
        let observed = controller
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await?;
        assert_eq!(observed.uid, "target-uid");
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
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
        Ok(())
    }

    #[tokio::test]
    async fn uidless_claim_never_adopts_a_preexisting_object() -> TestResult {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let (client, calls) = wire(vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(&[target_object("foreign", "foreign-uid", "30")], "30"),
        ]);
        let controller = controller(&plan, &activation, client)?;
        let mut previous = versioned_inventory(inventory("activation-uid", "20", vec![])?, "18");
        let error = controller
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .err()
            .ok_or("expected this call to fail")?;
        assert!(error.to_string().contains("claim-forbids-existing"));
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|call| call.0 != Method::PATCH));
        Ok(())
    }

    #[tokio::test]
    async fn uidless_claim_resumes_only_from_bound_inventory_uid() -> TestResult {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let mut previous = versioned_inventory(
            inventory("activation-uid", "20", vec![target_observation()])?,
            "18",
        );
        let existing = target_object("approved", "target-uid", "30");
        let (client, calls) = wire(vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(std::slice::from_ref(&existing), "30"),
            existing,
        ]);
        let controller = controller(&plan, &activation, client)?;
        let observed = controller
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await?;
        assert_eq!(observed.uid, "target-uid");
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 4);
        assert!(calls[3].1.contains("dryRun=All"));
        Ok(())
    }

    #[tokio::test]
    async fn successful_apply_cache_skips_only_same_version_and_desired_state() -> TestResult {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let mut previous = versioned_inventory(
            inventory("activation-uid", "20", vec![target_observation()])?,
            "18",
        );
        let existing = target_object("approved", "target-uid", "30");
        let (client, calls) = wire(vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(std::slice::from_ref(&existing), "30"),
            existing.clone(),
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(std::slice::from_ref(&existing), "30"),
        ]);
        let controller = controller(&plan, &activation, client)?;
        for _ in 0..2 {
            controller
                .apply(
                    &plan.ir.resources[0],
                    &plan.ir.resources[0].desired,
                    &mut previous,
                )
                .await?;
        }
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 7);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.1.contains("dryRun=All"))
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn successful_apply_cache_invalidates_on_live_version_change() -> TestResult {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let mut previous = versioned_inventory(
            inventory("activation-uid", "20", vec![target_observation()])?,
            "18",
        );
        let existing = target_object("approved", "target-uid", "30");
        let drifted = target_object("approved", "target-uid", "31");
        let (client, calls) = wire(vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(std::slice::from_ref(&existing), "30"),
            existing,
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(std::slice::from_ref(&drifted), "31"),
            drifted,
        ]);
        let controller = controller(&plan, &activation, client)?;
        for _ in 0..2 {
            controller
                .apply(
                    &plan.ir.resources[0],
                    &plan.ir.resources[0].desired,
                    &mut previous,
                )
                .await?;
        }
        let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
        assert_eq!(calls.len(), 8);
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.1.contains("dryRun=All"))
                .count(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_created_uid_persistence_never_falls_back_to_manager_metadata() -> TestResult {
        let plan = plan()?;
        let policy = policy(&plan);
        let activation = activation(&plan, &policy, None)?;
        let mut previous = versioned_inventory(inventory("activation-uid", "20", vec![])?, "18");
        let (client, _) = wire(vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(&[], "30"),
            target_object("approved", "target-uid", "31"),
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_object("approved", "target-uid", "32"),
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "21",
            )?,
        ]);
        let runtime = controller(&plan, &activation, client)?;
        let error = runtime
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .err()
            .ok_or("expected this call to fail")?;
        assert!(error.to_string().contains("active inventory publication"));
        assert!(previous.value.observations.is_empty());

        let existing = target_object("approved", "target-uid", "32");
        let (client, calls) = wire(vec![
            control_object(
                "apsides-activation-example",
                &activation,
                "activation-uid",
                "20",
            )?,
            control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
            target_list(&[existing], "32"),
        ]);
        let runtime = controller(&plan, &activation, client)?;
        let error = runtime
            .apply(
                &plan.ir.resources[0],
                &plan.ir.resources[0].desired,
                &mut previous,
            )
            .await
            .err()
            .ok_or("expected this call to fail")?;
        assert!(error.to_string().contains("claim-forbids-existing"));
        assert_eq!(
            calls.lock().map_err(|_| "calls mutex is poisoned")?.len(),
            3
        );
        Ok(())
    }

    /// Phase 5b review items 1 and E: `commit_handler_outputs` is the fix
    /// for "the SSA writes delete each other" (each declared output gets
    /// exactly one server-side apply, carrying its complete `data` map),
    /// and it must go through the exact same authority/claim re-check
    /// (`apply_existing_claimed`) reconciler mutations use.
    mod handler_commit {
        use super::*;

        fn handler_plan() -> TestResult<CompiledPlan> {
            Ok(parse_and_validate(&json!({
                "schemaVersion":"apsides.dev/ir/v2", "testOnly":false,
                "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262_144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
                "resources":[{
                    "id":"flags","apiVersion":"v1","kind":"ConfigMap","name":"flags","namespace":"workload",
                    "handlerOutput":true,
                    "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"flags","namespace":"workload","labels":{"apsides.dev/handler-output":"flags"}}},
                    "lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}
                }],
                "inputs":[
                    {"id":"settings","apiVersion":"v1","kind":"ConfigMap","name":"settings","namespace":"workload","mode":"observed","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.ts","line":1,"column":1}},
                    {"id":"flags-output","apiVersion":"v1","kind":"ConfigMap","name":"flags","namespace":"workload","mode":"output","managedResourceId":"flags","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.ts","line":1,"column":1}}
                ],
                "reactions":[],
                "handlers":[{
                    "id":"sync-flags","module":"handlers/sync-flags.ts","export":"syncFlags",
                    "trigger":{"kind":"input-changed","input":"settings"},
                    "inputs":["settings"],"egress":[],"outputs":["flags"],
                    "timeoutMillis":5_000,"maxHeapBytes":8_388_608,
                    "bundleSha256": "0".repeat(64),"source":{"file":"program.ts","line":1,"column":1}
                }],
                "outputs":[{"id":"flags","name":"flags","namespace":"workload"}],
                "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.ts","sha256":"0".repeat(64)},{"path":"handlers/sync-flags.ts","sha256":"1".repeat(64)}]}
            }).to_string())?)
        }

        /// Two declared outputs, `flags-a`/`flags-b`, on the same handler --
        /// for the "untouched output keeps its own data" test.
        fn handler_plan_two_outputs() -> TestResult<CompiledPlan> {
            Ok(parse_and_validate(&json!({
                "schemaVersion":"apsides.dev/ir/v2", "testOnly":false,
                "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262_144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
                "resources":[
                    {"id":"flags-a","apiVersion":"v1","kind":"ConfigMap","name":"flags-a","namespace":"workload","handlerOutput":true,
                     "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"flags-a","namespace":"workload","labels":{"apsides.dev/handler-output":"flags-a"}}},
                     "lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}},
                    {"id":"flags-b","apiVersion":"v1","kind":"ConfigMap","name":"flags-b","namespace":"workload","handlerOutput":true,
                     "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"flags-b","namespace":"workload","labels":{"apsides.dev/handler-output":"flags-b"}}},
                     "lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.ts","line":1,"column":1}}
                ],
                "inputs":[
                    {"id":"settings","apiVersion":"v1","kind":"ConfigMap","name":"settings","namespace":"workload","mode":"observed","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.ts","line":1,"column":1}},
                    {"id":"flags-a-output","apiVersion":"v1","kind":"ConfigMap","name":"flags-a","namespace":"workload","mode":"output","managedResourceId":"flags-a","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.ts","line":1,"column":1}},
                    {"id":"flags-b-output","apiVersion":"v1","kind":"ConfigMap","name":"flags-b","namespace":"workload","mode":"output","managedResourceId":"flags-b","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.ts","line":1,"column":1}}
                ],
                "reactions":[],
                "handlers":[{
                    "id":"sync-flags","module":"handlers/sync-flags.ts","export":"syncFlags",
                    "trigger":{"kind":"input-changed","input":"settings"},
                    "inputs":["settings"],"egress":[],"outputs":["flags-a","flags-b"],
                    "timeoutMillis":5_000,"maxHeapBytes":8_388_608,
                    "bundleSha256": "0".repeat(64),"source":{"file":"program.ts","line":1,"column":1}
                }],
                "outputs":[{"id":"flags-a","name":"flags-a","namespace":"workload"},{"id":"flags-b","name":"flags-b","namespace":"workload"}],
                "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.ts","sha256":"0".repeat(64)},{"path":"handlers/sync-flags.ts","sha256":"1".repeat(64)}]}
            }).to_string())?)
        }

        fn handler_activation(
            plan: &CompiledPlan,
            policy: &PolicyState,
            output_names: &[&str],
        ) -> TestResult<ActivationState> {
            let approved_claims = output_names
                .iter()
                .map(|name| ApprovedClaim {
                    claim: ProtectedClaim {
                        identity: ResourceIdentity {
                            api_group: String::new(),
                            resource: "configmaps".into(),
                            namespace: "workload".into(),
                            name: (*name).to_owned(),
                        },
                        domain_id: "example".into(),
                        generation_id: "generation-a".into(),
                        transition_id: "transition-a".into(),
                        state: ClaimState::Active,
                        uid: Some(format!("{name}-uid")),
                    },
                    deletion_policy: compiler_core::DeletionPolicy::Orphan,
                    allow_create: false,
                })
                .collect();
            Ok(ActivationState {
                schema_version: SCHEMA_VERSION.into(),
                binding: RuntimeBinding {
                    domain_id: "example".into(),
                    cluster_binding: "cluster-a".into(),
                    generation_id: "generation-a".into(),
                    build_digest: format!("sha256:{}", "a".repeat(64)),
                    ir_digest: plan.ir_digest.clone(),
                    policy_digest: digest(policy)?,
                    capability_digest: digest(&plan.capabilities)?,
                    service_account_name: "apsides-example-generation-a".into(),
                    controller_image: format!(
                        "example.invalid/controller@sha256:{}",
                        "b".repeat(64)
                    ),
                    max_handler_heap_bytes: 8 * 1024 * 1024,
                },
                status: ActivationStatus::Active,
                approved_claims,
                rollout_key_id: None,
            })
        }

        fn outputs_by_id(plan: &CompiledPlan) -> BTreeMap<String, compiler_core::Output> {
            plan.ir
                .outputs
                .iter()
                .map(|output| (output.id.clone(), output.clone()))
                .collect()
        }

        /// The critical regression test for review item 1: a single SSA per
        /// output carries the handler's *complete* written data map plus
        /// the revision annotation, not a series of field-clobbering
        /// partial writes.
        #[tokio::test]
        async fn commit_writes_the_complete_data_map_and_annotation_in_one_apply() -> TestResult {
            let plan = handler_plan()?;
            let policy = policy(&plan);
            let activation = handler_activation(&plan, &policy, &["flags"])?;
            let existing_flags = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags","namespace":"workload","uid":"flags-uid","resourceVersion":"5"},
                "data":{},
            });
            let projected_flags = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags","namespace":"workload","uid":"flags-uid","resourceVersion":"5"},
                "data":{"value":"on"},
            });
            let applied_flags = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags","namespace":"workload","uid":"flags-uid","resourceVersion":"6",
                    "annotations":{"apsides.dev/handler-input-revision":"rev-1"},
                    "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{},"f:metadata":{"f:annotations":{}}},"manager":"apsides-handler-example","operation":"Apply"}]},
                "data":{"value":"on","other":"kept"},
            });
            let responses = vec![
                existing_flags,
                control_object(
                    "apsides-activation-example",
                    &activation,
                    "activation-uid",
                    "20",
                )?,
                control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
                projected_flags,
                control_object(
                    "apsides-activation-example",
                    &activation,
                    "activation-uid",
                    "20",
                )?,
                control_object("apsides-policy-example", &policy, "policy-uid", "19")?,
                applied_flags,
            ];
            let (client, calls) = wire(responses);
            let controller = controller(&plan, &activation, client)?;
            let mut written: crate::handlers::BufferedWrites = BTreeMap::new();
            written.insert(
                "flags".to_owned(),
                BTreeMap::from([("value".to_owned(), "on".to_owned())]),
            );
            let output_uids = BTreeMap::from([("flags".to_owned(), "flags-uid".to_owned())]);
            let committed = commit_handler_outputs(
                &controller,
                &["flags".to_owned()],
                &outputs_by_id(&plan),
                "apsides-handler-example",
                &written,
                "rev-1",
                &output_uids,
            )
            .await?;
            assert_eq!(
                committed.get("flags").map(String::as_str),
                Some("flags-uid")
            );

            let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
            assert_eq!(calls.len(), 7);
            let real_patch = calls
                .iter()
                .rev()
                .find(|(method, uri, _)| *method == Method::PATCH && !uri.contains("dryRun"))
                .ok_or("expected exactly one real (non-dry-run) PATCH")?;
            assert_eq!(
                real_patch.2.pointer("/data/value").and_then(Value::as_str),
                Some("on"),
                "the real SSA must carry the written key"
            );
            let annotations = real_patch
                .2
                .get("metadata")
                .and_then(|metadata| metadata.get("annotations"))
                .ok_or("expected annotations on the real SSA payload")?;
            assert_eq!(
                annotations
                    .get(crate::handlers::HANDLER_INPUT_REVISION_ANNOTATION)
                    .and_then(Value::as_str),
                Some("rev-1"),
                "the real SSA must carry the revision annotation alongside the data"
            );
            // Exactly one real PATCH is sent for this one output -- not one
            // per key plus a separate stamp (the bug this test guards
            // against: separate SSAs under the same field manager delete
            // each other's fields).
            let real_patch_count = calls
                .iter()
                .filter(|(method, uri, _)| *method == Method::PATCH && !uri.contains("dryRun"))
                .count();
            assert_eq!(real_patch_count, 1);
            Ok(())
        }

        /// The BLOCKING uid-binding regression test (Phase 5b
        /// runtime-integration re-review): if the live object's uid no
        /// longer matches the uid this cycle's own reconciler apply
        /// observed for it (bound via `output_uids`), the commit MUST
        /// refuse with `handler-output-replaced` and MUST NOT issue any
        /// PATCH -- proving the uid precondition is checked against an
        /// externally-bound value, never re-derived from the very GET
        /// it's supposed to guard.
        #[tokio::test]
        async fn commit_refuses_an_output_replaced_since_the_reconciler_observed_it() -> TestResult
        {
            let plan = handler_plan()?;
            let policy = policy(&plan);
            let activation = handler_activation(&plan, &policy, &["flags"])?;
            // The live object now has a different uid than the reconciler's
            // own apply observed earlier this cycle -- as if it was deleted
            // and recreated under the same name while the handler ran.
            let existing_flags = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags","namespace":"workload","uid":"flags-uid-replaced","resourceVersion":"1"},
                "data":{},
            });
            let (client, calls) = wire(vec![existing_flags]);
            let controller = controller(&plan, &activation, client)?;
            let mut written: crate::handlers::BufferedWrites = BTreeMap::new();
            written.insert(
                "flags".to_owned(),
                BTreeMap::from([("value".to_owned(), "on".to_owned())]),
            );
            let output_uids = BTreeMap::from([("flags".to_owned(), "flags-uid".to_owned())]);
            let outcome = commit_handler_outputs(
                &controller,
                &["flags".to_owned()],
                &outputs_by_id(&plan),
                "apsides-handler-example",
                &written,
                "rev-1",
                &output_uids,
            )
            .await;
            let Err(error) = outcome else {
                return Err("a replaced output must refuse the commit".into());
            };
            assert!(
                error.to_string().contains("handler-output-replaced"),
                "unexpected error: {error}"
            );
            let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
            assert!(
                calls.iter().all(|(method, _, _)| *method != Method::PATCH),
                "a replaced output must never be written to"
            );
            Ok(())
        }

        /// One `authority.verify()` round-trip's worth of wire responses
        /// (activation, then policy) -- `commit_handler_outputs`/
        /// `apply_existing_claimed` each call `verify()` once, so this
        /// appears twice per committed output.
        fn verify_responses(
            activation: &ActivationState,
            policy: &PolicyState,
        ) -> TestResult<Vec<Value>> {
            Ok(vec![
                control_object(
                    "apsides-activation-example",
                    activation,
                    "activation-uid",
                    "20",
                )?,
                control_object("apsides-policy-example", policy, "policy-uid", "19")?,
            ])
        }

        /// Review item 1's recommended contract: an output the handler did
        /// *not* write this invocation is still stamped with the revision
        /// annotation, but its existing live `data` is carried forward
        /// unchanged in that same SSA -- never dropped.
        #[tokio::test]
        async fn commit_preserves_untouched_output_data_when_stamping() -> TestResult {
            let plan = handler_plan_two_outputs()?;
            let policy = policy(&plan);
            let activation = handler_activation(&plan, &policy, &["flags-a", "flags-b"])?;
            let existing_a = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags-a","namespace":"workload","uid":"flags-a-uid","resourceVersion":"5"},
                "data":{},
            });
            let projected_a = existing_a.clone();
            let applied_a = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags-a","namespace":"workload","uid":"flags-a-uid","resourceVersion":"6",
                    "annotations":{"apsides.dev/handler-input-revision":"rev-1"},
                    "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{},"f:metadata":{"f:annotations":{}}},"manager":"apsides-handler-example","operation":"Apply"}]},
                "data":{"value":"written"},
            });
            let existing_b = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags-b","namespace":"workload","uid":"flags-b-uid","resourceVersion":"9",
                    "annotations":{"apsides.dev/handler-input-revision":"rev-0"},
                    "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{"f:previous":{}},"f:metadata":{"f:annotations":{}}},"manager":"apsides-handler-example","operation":"Apply"}]},
                "data":{"previous":"kept-value"},
            });
            // The dry-run projection reflects the new revision annotation
            // ("rev-1") even though `data` is unchanged, so the commit
            // cannot early-exit on "nothing changed" here -- it still owes
            // flags-b a real PATCH to stamp the new revision.
            let projected_b = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags-b","namespace":"workload","uid":"flags-b-uid","resourceVersion":"9",
                    "annotations":{"apsides.dev/handler-input-revision":"rev-1"},
                    "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{"f:previous":{}},"f:metadata":{"f:annotations":{}}},"manager":"apsides-handler-example","operation":"Apply"}]},
                "data":{"previous":"kept-value"},
            });
            let applied_b = json!({
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"flags-b","namespace":"workload","uid":"flags-b-uid","resourceVersion":"10",
                    "annotations":{"apsides.dev/handler-input-revision":"rev-1"},
                    "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{},"f:metadata":{"f:annotations":{}}},"manager":"apsides-handler-example","operation":"Apply"}]},
                "data":{"previous":"kept-value"},
            });
            let mut responses = vec![existing_a];
            responses.extend(verify_responses(&activation, &policy)?);
            responses.push(projected_a);
            responses.extend(verify_responses(&activation, &policy)?);
            responses.push(applied_a);
            responses.push(existing_b);
            responses.extend(verify_responses(&activation, &policy)?);
            responses.push(projected_b);
            responses.extend(verify_responses(&activation, &policy)?);
            responses.push(applied_b);
            let (client, calls) = wire(responses);
            let controller = controller(&plan, &activation, client)?;
            let mut written: crate::handlers::BufferedWrites = BTreeMap::new();
            written.insert(
                "flags-a".to_owned(),
                BTreeMap::from([("value".to_owned(), "written".to_owned())]),
            );
            // "flags-b" is declared but deliberately absent from `written`.
            let output_uids = BTreeMap::from([
                ("flags-a".to_owned(), "flags-a-uid".to_owned()),
                ("flags-b".to_owned(), "flags-b-uid".to_owned()),
            ]);
            let committed = commit_handler_outputs(
                &controller,
                &["flags-a".to_owned(), "flags-b".to_owned()],
                &outputs_by_id(&plan),
                "apsides-handler-example",
                &written,
                "rev-1",
                &output_uids,
            )
            .await?;
            assert_eq!(committed.len(), 2);

            let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
            let flags_b_real_patch = calls
                .iter()
                .rev()
                .filter(|(method, uri, _)| *method == Method::PATCH && !uri.contains("dryRun"))
                .find(|(_, uri, _)| uri.contains("flags-b"))
                .ok_or("expected a real PATCH for flags-b")?;
            assert_eq!(
                flags_b_real_patch
                    .2
                    .pointer("/data/previous")
                    .and_then(Value::as_str),
                Some("kept-value"),
                "an output the handler did not write must keep its existing data, not lose it"
            );
            let annotations = flags_b_real_patch
                .2
                .get("metadata")
                .and_then(|metadata| metadata.get("annotations"))
                .ok_or("expected annotations on flags-b's real SSA payload")?;
            assert_eq!(
                annotations
                    .get(crate::handlers::HANDLER_INPUT_REVISION_ANNOTATION)
                    .and_then(Value::as_str),
                Some("rev-1"),
                "flags-b must still be stamped with the revision even though it was not written"
            );
            Ok(())
        }

        /// Full-pipeline regression test for a CI-only acceptance failure
        /// (`handlers::
        /// a_handler_commits_exactly_its_owned_keys_and_seeds_across_a_restart`,
        /// assertion 1: `Deadline("the output to reflect {a,b}")`, the output
        /// carrying its revision annotation but no data keys). Drives
        /// `HandlerRuntime::run_cycle` -- the exact `detect_triggers` ->
        /// `dispatch_ready` -> `invoke` -> `commit_handler_outputs` path
        /// `reconcile()` itself uses -- with a realistic observed snapshot and
        /// the REAL `handlers/derive-keys.ts` bundle (the same bytes as
        /// `handlers::tests::derive_keys_real_bundle`, which isolates the
        /// event-construction/engine half of this same question; this test
        /// isolates everything downstream of it instead: `HandlerRuntime`'s
        /// queueing, `commit_handler_outputs`'s uid binding and untouched-key
        /// handling, and the real SSA request this crate builds from the
        /// sink's buffered writes), and inspects the actual wire PATCH body
        /// `commit_handler_outputs` sends.
        mod handler_full_cycle_real_bundle {
            use super::*;

            /// See `handlers::tests::derive_keys_real_bundle::DERIVE_KEYS_BUNDLE`
            /// for how this was captured.
            const DERIVE_KEYS_BUNDLE: &str = r#"
//#region apsides:/source/handlers/derive-keys.ts
async function deriveKeys(event) {
	const data = event.inputs["handler-source"]?.data ?? {};
	const keys = Object.keys(data).sort();
	for (const key of keys) {
		apsides.writeOutput("handler-derived", key, data[key] ?? "");
	}
	return { keys };
}

//#endregion
export { deriveKeys };
"#;
            const HANDLERS: &[(&str, &str)] = &[("derive-keys", DERIVE_KEYS_BUNDLE)];

            fn real_plan() -> TestResult<CompiledPlan> {
                Ok(parse_and_validate(&json!({
                "schemaVersion":"apsides.dev/ir/v2", "testOnly":false,
                "domain":{"id":"example","namespace":"workload","controlNamespace":"control","clusterBinding":"cluster-a","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262_144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
                "resources":[{
                    "id":"handler-derived","apiVersion":"v1","kind":"ConfigMap","name":"handler-derived","namespace":"workload",
                    "handlerOutput":true,
                    "desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"handler-derived","namespace":"workload","labels":{"apsides.dev/handler-output":"handler-derived"}}},
                    "lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"program.tsx","line":1,"column":1}
                }],
                "inputs":[
                    {"id":"handler-source","apiVersion":"v1","kind":"ConfigMap","name":"handler-source","namespace":"workload","mode":"observed","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.tsx","line":1,"column":1}},
                    {"id":"output-input-handler-derived","apiVersion":"v1","kind":"ConfigMap","name":"handler-derived","namespace":"workload","mode":"output","managedResourceId":"handler-derived","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"program.tsx","line":1,"column":1}}
                ],
                "reactions":[],
                "handlers":[{
                    "id":"derive-keys","module":"handlers/derive-keys.ts","export":"deriveKeys",
                    "trigger":{"kind":"input-changed","input":"handler-source"},
                    "inputs":["handler-source"],"egress":[],"outputs":["handler-derived"],
                    "timeoutMillis":5_000,"maxHeapBytes":8_388_608,
                    "bundleSha256": "0".repeat(64),"source":{"file":"program.tsx","line":1,"column":1}
                }],
                "outputs":[{"id":"handler-derived","name":"handler-derived","namespace":"workload"}],
                "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"program.tsx","sha256":"0".repeat(64)},{"path":"handlers/derive-keys.ts","sha256":"1".repeat(64)}]}
            }).to_string())?)
            }

            /// `existing` (before any handler commit), `projected` (the dry-run
            /// response) and `applied` (the real commit response) for
            /// `handler-derived`, plus the two `authority.verify()` round-trips
            /// `commit_handler_outputs`/`apply_existing_claimed` each need --
            /// see `run_cycle_commits_the_real_bundles_written_keys`.
            fn commit_wire_responses(
                activation: &ActivationState,
                policy: &PolicyState,
            ) -> TestResult<Vec<Value>> {
                let existing = json!({
                    "apiVersion":"v1","kind":"ConfigMap",
                    "metadata":{"name":"handler-derived","namespace":"workload","uid":"handler-derived-uid","resourceVersion":"5",
                        "labels":{"apsides.dev/handler-output":"handler-derived"},
                        "managedFields":[{"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:metadata":{"f:labels":{}}},"manager":"apsides-example","operation":"Apply"}]},
                    "data":{},
                });
                let projected = json!({
                    "apiVersion":"v1","kind":"ConfigMap",
                    "metadata":{"name":"handler-derived","namespace":"workload","uid":"handler-derived-uid","resourceVersion":"5",
                        "labels":{"apsides.dev/handler-output":"handler-derived"},
                        "annotations":{"apsides.dev/handler-input-revision":"placeholder"}},
                    "data":{"a":"1","b":"2"},
                });
                let applied = json!({
                    "apiVersion":"v1","kind":"ConfigMap",
                    "metadata":{"name":"handler-derived","namespace":"workload","uid":"handler-derived-uid","resourceVersion":"6",
                        "labels":{"apsides.dev/handler-output":"handler-derived"},
                        "annotations":{"apsides.dev/handler-input-revision":"placeholder"},
                        "managedFields":[
                            {"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:metadata":{"f:labels":{}}},"manager":"apsides-example","operation":"Apply"},
                            {"apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{},"f:metadata":{"f:annotations":{}}},"manager":"apsides-handler-example","operation":"Apply"}
                        ]},
                    "data":{"a":"1","b":"2"},
                });
                Ok(vec![
                    existing,
                    control_object(
                        "apsides-activation-example",
                        activation,
                        "activation-uid",
                        "20",
                    )?,
                    control_object("apsides-policy-example", policy, "policy-uid", "19")?,
                    projected,
                    control_object(
                        "apsides-activation-example",
                        activation,
                        "activation-uid",
                        "20",
                    )?,
                    control_object("apsides-policy-example", policy, "policy-uid", "19")?,
                    applied,
                ])
            }

            #[tokio::test]
            async fn run_cycle_commits_the_real_bundles_written_keys() -> TestResult {
                let plan = real_plan()?;
                let policy = policy(&plan);
                let activation = handler_activation(&plan, &policy, &["handler-derived"])?;
                let (client, calls) = wire(commit_wire_responses(&activation, &policy)?);
                let controller = controller(&plan, &activation, client)?;
                let (unused_client, _unused_calls) = wire(vec![]);

                let mut handler_runtime = crate::handlers::HandlerRuntime::new(
                    &plan,
                    HANDLERS,
                    "apsides-handler-example".into(),
                    Duration::from_millis(1),
                    Duration::from_millis(10),
                    None,
                )?;

                let mut observations = BTreeMap::new();
                observations.insert(
                    "handler-source".to_owned(),
                    Observation::Available {
                        data: BTreeMap::from([
                            ("a".to_owned(), "1".to_owned()),
                            ("b".to_owned(), "2".to_owned()),
                        ]),
                        resource_version: "1".into(),
                    },
                );
                let applied_set = BTreeSet::from(["handler-derived".to_owned()]);
                let output_uids = BTreeMap::from([(
                    "handler-derived".to_owned(),
                    "handler-derived-uid".to_owned(),
                )]);

                let conditions = handler_runtime
                    .run_cycle(
                        &plan,
                        &observations,
                        &applied_set,
                        &output_uids,
                        &unused_client,
                        &controller,
                    )
                    .await?;
                assert!(
                    conditions.is_empty(),
                    "the handler invocation must succeed with no conditions: {conditions:?}"
                );

                let calls = calls.lock().map_err(|_| "calls mutex is poisoned")?;
                let real_patch = calls
                    .iter()
                    .rev()
                    .find(|(method, uri, _)| *method == Method::PATCH && !uri.contains("dryRun"))
                    .ok_or("expected exactly one real (non-dry-run) PATCH")?;
                assert_eq!(
                    real_patch.2.get("data"),
                    Some(&json!({"a": "1", "b": "2"})),
                    "the real SSA the runtime sends for handler-derived must carry both keys the \
                 real bundle wrote -- this is the exact request body a CI-only failure showed \
                 empty"
                );
                Ok(())
            }
        }
    }
}
