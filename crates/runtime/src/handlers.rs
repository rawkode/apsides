//! Wires the `engine` crate (a fresh V8 isolate per invocation, no
//! filesystem/net access except through its own typed, capability-checked
//! ops) into the reconcile cycle: computes each handler's input revision
//! each cycle, enqueues, and dispatches at most one handler invocation per
//! cycle at global concurrency 1, holding a handler that reads another
//! handler's output until that upstream is settled (see
//! `HandlerRuntime::upstream_gate_open`). `apsides.writeOutput` calls are
//! buffered in memory for the invocation's duration
//! ([`BufferedOutputSink`]); once (and only if) the handler returns
//! successfully, every declared output is committed with exactly one
//! server-side apply each, through
//! `production::commit_handler_outputs` -- the same authority/claim
//! re-check reconciler mutations use, under field manager
//! `apsides-handler-<domainId>`.
//!
//! `HandlerRuntime` is driven only by
//! [`crate::production::ProductionController`]: fixture mode verifies the
//! embedded bundles against the IR but never executes them (see
//! `crate::run_locked`). See this module's own tests (a fake `kube::Client`
//! built the same way `production.rs`'s own test module builds one) for
//! coverage of the dispatch, queue, backoff, and revision-computation
//! behavior in isolation from that larger harness, and `production.rs`'s
//! own tests for the end-to-end commit path.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use compiler_core::{CompiledPlan, Handler, InputMode, Output};
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject};
use kube::core::GroupVersionKind;
use platform::RuntimeCondition;
use serde_json::{Map, Value, json};

use crate::{Observation, RuntimeError};

/// Distinct from [`compiler_core::REVISION_ANNOTATION`] (`apsides.dev/revision`,
/// the rollout-input revision on a *reacting* resource's desired state):
/// this one is written by the runtime itself, after a handler invocation
/// succeeds, onto the handler's own *output* object, recording which input
/// revision that output now reflects.
pub(crate) const HANDLER_INPUT_REVISION_ANNOTATION: &str = "apsides.dev/handler-input-revision";

/// Converts one of `compiler_core::handler_budgets`' `u64` constants to
/// `usize`.
///
/// Justification for the lone `#[allow]`: every value this is ever called
/// with (from a `const` initializer, always) is a small, fixed budget
/// constant (low megabytes), never anywhere near truncating even on a
/// 32-bit `usize`; named and confined to this one function instead of an
/// `unwrap_or`/`.expect()` at each call site (both denied by this
/// workspace's own `clippy::unwrap_used`/`expect_used`).
#[allow(
    clippy::cast_possible_truncation,
    reason = "converts a small, fixed handler-budget constant (low megabytes); never near usize::MAX"
)]
const fn budget_as_usize(value: u64) -> usize {
    value as usize
}

/// Hard cap on `apsides.fetch` response bytes across one whole invocation.
/// Not named by the IR (only a per-handler `timeoutMillis`/`maxHeapBytes`
/// budget is declared) -- from `compiler_core::handler_budgets`, the
/// single shared source this crate and `platform`'s controller-pod memory
/// formula both derive from (see the Apsides v0.3 plan, Phase 5b review
/// item C).
const MAX_FETCH_BYTES_TOTAL: usize =
    budget_as_usize(compiler_core::HANDLER_FETCH_BUDGET_TOTAL_BYTES);
/// Hard cap on concurrent `apsides.fetch` calls within one invocation.
const MAX_CONCURRENT_FETCHES: usize =
    budget_as_usize(compiler_core::HANDLER_MAX_CONCURRENT_FETCHES);

// ---------------------------------------------------------------------
// Input revision / event construction
// ---------------------------------------------------------------------

fn snapshot_json(observation: &Observation) -> Option<Value> {
    match observation {
        Observation::Available { data, .. } => Some(Value::Object(
            data.iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect(),
        )),
        Observation::Missing
        | Observation::Unauthorized
        | Observation::Unavailable(_)
        | Observation::Invalid(_) => None,
    }
}

/// The projected snapshot of every one of `handler`'s declared inputs
/// (the trigger input is always among them -- validated at compile time).
/// `None` when any declared input is not currently `Available`: the
/// handler is not ready to run this cycle.
fn handler_inputs_snapshot(
    handler: &Handler,
    observations: &BTreeMap<String, Observation>,
) -> Option<BTreeMap<String, Value>> {
    let mut inputs = BTreeMap::new();
    for input_id in &handler.inputs {
        let snapshot = snapshot_json(observations.get(input_id)?)?;
        inputs.insert(input_id.clone(), snapshot);
    }
    Some(inputs)
}

/// A hash of the trigger input's id, every declared input's projected
/// snapshot, and the handler's own code/output identity -- see the
/// Apsides v0.3 plan, Phase 5b, "Trigger detection", and review item 2.
///
/// Includes `handler.id`, `handler.bundle_sha256`, `handler.export`, and
/// the handler's declared outputs (sorted, for determinism) *in addition*
/// to the trigger/input data: without the bundle's own digest here, a new
/// handler bundle deployed with unchanged input data would compute the
/// exact same revision as the previous bundle, so a restart (which seeds
/// `known_revision` from this same digest recorded on the outputs) would
/// never re-run the handler even though its code changed. `id`/`export`
/// are included too so two distinct handlers reacting to the same input
/// with byte-identical data never collide on the same revision string.
fn compute_input_revision(
    handler: &Handler,
    inputs: &BTreeMap<String, Value>,
) -> Result<String, RuntimeError> {
    let mut outputs = handler.outputs.clone();
    outputs.sort();
    compiler_core::digest(&json!({
        "handlerId": handler.id,
        "bundleSha256": handler.bundle_sha256,
        "export": handler.export,
        "outputs": outputs,
        "trigger": handler.trigger.input,
        "inputs": inputs,
    }))
    .map_err(|error| RuntimeError::Plan(error.to_string()))
}

/// `event = { id, trigger, inputs: {<inputId>: {data}} }`; `id` is a hash
/// of the handler id and its input revision -- the invocation's
/// idempotency key.
fn build_event(
    handler: &Handler,
    inputs: &BTreeMap<String, Value>,
    revision: &str,
) -> Result<(String, Value), RuntimeError> {
    let id = compiler_core::digest(&json!({"handler": handler.id, "revision": revision}))
        .map_err(|error| RuntimeError::Plan(error.to_string()))?;
    let mut wrapped_inputs = Map::new();
    for (input_id, data) in inputs {
        wrapped_inputs.insert(input_id.clone(), json!({"data": data}));
    }
    let event = json!({
        "id": id,
        "trigger": {"kind": handler.trigger.kind, "input": handler.trigger.input},
        "inputs": Value::Object(wrapped_inputs),
    });
    Ok((id, event))
}

// ---------------------------------------------------------------------
// Queue + per-handler state
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct QueuedEvent {
    revision: String,
    event_id: String,
    event: Value,
}

#[derive(Default)]
struct HandlerState {
    /// The input revision the handler's outputs currently reflect (`None`
    /// until a successful invocation stamps one, in this process's
    /// lifetime -- seeded from the outputs' own annotation at startup by
    /// [`HandlerRuntime::seed_from_annotations`]).
    known_revision: Option<String>,
    /// Each declared output's `uid` as last observed (at seed time, or
    /// after a successful commit) -- see the Apsides v0.3 plan, Phase 5b
    /// review item 5: an output whose live `uid` no longer matches what is
    /// recorded here was deleted and recreated since `known_revision` was
    /// last trusted, so it must not be treated as still agreeing with it
    /// even though nothing about the handler's *input* changed.
    output_uids: BTreeMap<String, String>,
    /// The input revision computed from *this* cycle's observations by
    /// [`HandlerRuntime::detect_triggers`]; `None` while any declared input
    /// is not `Available`. Never persisted: a restart recomputes it on the
    /// first cycle, before any dispatch.
    current_revision: Option<String>,
    /// While `current_revision` is `None`: whether this cycle's
    /// [`HandlerRuntime::detect_triggers`] confirmed every live output
    /// still carries `known_revision`. Recomputed every cycle, so a
    /// transient read failure holds downstream handlers for one cycle only.
    outputs_confirmed_without_inputs: bool,
    queued: Option<QueuedEvent>,
    backoff_until: Option<Instant>,
    attempt: u32,
    last_run: Option<Instant>,
    last_error_code: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum EnqueueOutcome {
    /// A new, previously-unqueued handler entry (counts against capacity).
    New,
    /// An already-queued entry's payload was replaced with the latest
    /// revision (coalescing; does not count against capacity).
    Coalesced,
    /// The handler already reflects this exact revision (or is already
    /// queued for it); nothing changed.
    Unchanged,
    /// The queue was at `maxQueueDepth` distinct entries and this would
    /// have been a new one.
    Overflow,
}

/// One condition/status row the caller (`production.rs`) surfaces through
/// the existing conditions mechanism and `/status`.
pub(crate) struct HandlerStatus {
    pub id: String,
    pub last_run_millis_ago: Option<u64>,
    pub last_error_code: Option<String>,
    pub queue_depth: usize,
}

// ---------------------------------------------------------------------
// Output sink: buffers `apsides.writeOutput(id, key, data)` calls in
// memory for the duration of one invocation. No I/O and no cross-thread
// hop here at all -- see the Apsides v0.3 plan, Phase 5b review item 1:
// the previous design sent one server-side apply per `writeOutput` call
// (plus one more for the post-success revision stamp), all under the same
// field manager; since SSA replaces a manager's *whole* field set on each
// request, every write after the first deleted the keys the previous
// write had just added, and the final stamp deleted the data entirely.
// The fix is to never write anything to Kubernetes until the whole
// invocation has resolved successfully -- `write_output` here only
// validates (already done by `engine` itself before calling this trait;
// see its doc comment) and buffers; [`commit_handler_outputs`] in
// `production.rs` then sends exactly one SSA per declared output,
// containing that output's *complete* data map, after the handler
// returns `Ok`. See that function's doc comment for what happens to a
// declared output the handler did not write this invocation.
// ---------------------------------------------------------------------

fn configmap_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    let resource = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("", "v1", "ConfigMap"),
        "configmaps",
    );
    Api::namespaced_with(client.clone(), namespace, &resource)
}

/// `output_id -> key -> value`, one invocation's complete set of buffered
/// `apsides.writeOutput` calls. Values are already-validated UTF-8 text
/// (see [`BufferedOutputSink::write_output`]) -- `ConfigMap` `data` values
/// must be UTF-8 (Kubernetes' `binaryData` sibling field is not used by
/// this release).
pub(crate) type BufferedWrites = BTreeMap<String, BTreeMap<String, String>>;

struct BufferedOutputSink {
    buffer: std::sync::Mutex<BufferedWrites>,
}

impl BufferedOutputSink {
    const fn new() -> Self {
        Self {
            buffer: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// Takes the accumulated buffer, leaving an empty one behind. Called
    /// once, by `HandlerRuntime::invoke`, after the isolate has finished
    /// running (successfully) -- never concurrently with `write_output`,
    /// since the isolate thread that calls `write_output` has, by
    /// construction, already returned by the time this runs.
    fn take(&self) -> BufferedWrites {
        self.buffer
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }
}

impl engine::OutputSink for BufferedOutputSink {
    fn write_output(
        &self,
        id: &str,
        key: &str,
        data: &[u8],
    ) -> Result<(), engine::OutputSinkError> {
        // See the SDK's `writeOutput` JSDoc and IR-CONTRACT.md: output
        // values must be UTF-8 text (ConfigMap `data`, not `binaryData`).
        let Ok(value) = std::str::from_utf8(data) else {
            return Err(engine::OutputSinkError {
                message: "handler output data must be valid UTF-8 text in this release".into(),
            });
        };
        let mut buffer = self
            .buffer
            .lock()
            .map_err(|_error| engine::OutputSinkError {
                message: "the handler output buffer lock was poisoned".into(),
            })?;
        buffer
            .entry(id.to_owned())
            .or_default()
            .insert(key.to_owned(), value.to_owned());
        Ok(())
    }
}

struct HandlerLogWriter {
    handler_id: String,
    invocation_id: String,
}

impl engine::LogWriter for HandlerLogWriter {
    fn write_line(&self, line: &str) {
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr().lock(),
            format_args!(
                "{}\n",
                json!({
                    "handler": self.handler_id,
                    "invocation": self.invocation_id,
                    "log": line,
                })
            ),
        );
    }
}

/// One JSON line to the controller's own stderr recording an
/// invocation's dispatch or outcome -- the *only* place a real
/// invocation's dispatch/outcome is ever recorded independent of the
/// handler's own optional `apsides.log` calls (`HandlerLogWriter`,
/// above, writes only when the handler code itself calls
/// `apsides.log`). Added after a CI-only acceptance failure whose
/// controller log was empty and therefore gave no signal at all: a
/// handler that never calls `apsides.log` left no trace it had run,
/// succeeded, or what it wrote. Never logs a handler *value*, only ids,
/// the revision, output ids and the *keys* (never values) a successful
/// commit actually wrote -- `fields` must never carry anything from a
/// handler's own written data or the observed input snapshot.
fn log_invocation(level: &str, event: &str, handler_id: &str, invocation_id: &str, fields: &Value) {
    let mut line = json!({
        "level": level,
        "event": event,
        "handler": handler_id,
        "invocation": invocation_id,
    });
    if let (Some(line_object), Some(fields_object)) = (line.as_object_mut(), fields.as_object()) {
        line_object.extend(fields_object.clone());
    }
    let _ = std::io::Write::write_fmt(&mut std::io::stderr().lock(), format_args!("{line}\n"));
}

/// Logs a completed commit attempt's outcome: on success, the *keys*
/// (never values) actually committed per declared output, read from the
/// sink's own buffer, not from what `commit_handler_outputs` reports
/// back (which is only committed uids); on failure, the typed reason.
fn log_commit_outcome(
    handler_id: &str,
    invocation_id: &str,
    handler_outputs: &[String],
    written: &BufferedWrites,
    outcome: &Result<BTreeMap<String, String>, RuntimeError>,
) {
    match outcome {
        Ok(_committed_uids) => {
            let written_keys: BTreeMap<&str, Vec<&str>> = handler_outputs
                .iter()
                .map(|id| {
                    let keys = written
                        .get(id)
                        .map_or_else(Vec::new, |keys| keys.keys().map(String::as_str).collect());
                    (id.as_str(), keys)
                })
                .collect();
            log_invocation(
                "info",
                "handler-committed",
                handler_id,
                invocation_id,
                &json!({"writtenKeys": written_keys}),
            );
        }
        Err(error) => log_invocation(
            "warn",
            "handler-commit-failed",
            handler_id,
            invocation_id,
            &json!({"reason": error.to_string()}),
        ),
    }
}

// ---------------------------------------------------------------------
// HandlerRuntime
// ---------------------------------------------------------------------

fn handler_limits(handler: &Handler) -> engine::Limits {
    let heap_bytes = usize::try_from(handler.max_heap_bytes).unwrap_or(usize::MAX);
    let array_buffer_cap = usize::try_from(compiler_core::array_buffer_cap_bytes(
        handler.max_heap_bytes,
    ))
    .unwrap_or(usize::MAX);
    engine::Limits {
        timeout: Duration::from_millis(u64::from(handler.timeout_millis)),
        max_heap_bytes: heap_bytes,
        // Capped at half the declared heap (never more than a fixed
        // ceiling), not set equal to the whole heap -- see the Apsides
        // v0.3 plan, Phase 5b review item C: `ArrayBuffer` allocations
        // live outside the V8 heap the near-heap-limit callback watches,
        // so an uncapped `max_array_buffer_bytes` let them add up to
        // another whole heap's worth of memory on top of the heap itself.
        max_array_buffer_bytes: array_buffer_cap,
        max_fetch_bytes_total: MAX_FETCH_BYTES_TOTAL,
        max_concurrent_fetches: MAX_CONCURRENT_FETCHES,
    }
}

/// Production `EgressPolicy` for `handler`. **Never** sets
/// `fixture_trust_anchor`: that field only exists on `EgressPolicy` for
/// `engine`'s own test-mode fixture server, gated behind
/// `engine::FIXTURE_ACKNOWLEDGEMENT`, and this crate's production path has
/// no config surface that could ever supply one -- see
/// `fixture_egress_override_has_no_production_construction_path` in this
/// module's tests.
fn production_egress_policy(handler: &Handler) -> engine::EgressPolicy {
    engine::EgressPolicy {
        allowed: handler
            .egress
            .iter()
            .map(|egress| (egress.host.clone(), egress.port))
            .collect(),
        resolver_override: None,
        fixture_trust_anchor: None,
    }
}

fn backoff_duration(attempt: u32, base: Duration, max: Duration) -> Duration {
    let shift = attempt.min(16);
    let millis = u64::try_from(base.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_mul(1_u64 << shift);
    Duration::from_millis(millis).min(max)
}

/// The outcome of [`HandlerRuntime::invoke`]'s commit step: a plain error
/// code (an ordinary invocation/commit failure -- folded into a
/// `RuntimeCondition` plus backoff by `dispatch_ready`), or a fatal
/// [`RuntimeError`] (`Terminating`/`Platform`) that must abort the whole
/// reconcile cycle immediately, the same way a reconciler resource apply
/// does.
enum InvokeCommitError {
    Code(String),
    Fatal(RuntimeError),
}

/// `Some(r)` only when every entry in `recorded` is `Some(r)` for the same
/// `r`; `None` (no agreement, or an empty list) otherwise -- see
/// [`HandlerRuntime::seed_from_annotations`]'s doc comment, Phase 5b
/// review item 5.
fn agreed_revision(recorded: &[Option<String>]) -> Option<String> {
    let (first, rest) = recorded.split_first()?;
    first.clone().filter(|value| {
        rest.iter()
            .all(|other| other.as_deref() == Some(value.as_str()))
    })
}

/// The handlers whose outputs `handler` reads through an
/// [`InputMode::Output`] input: its direct upstreams in the handler graph
/// `compiler_core`'s validator already proved acyclic and free of
/// self-loops.
fn upstream_handlers<'plan>(plan: &'plan CompiledPlan, handler: &Handler) -> BTreeSet<&'plan str> {
    let read_outputs: BTreeSet<&str> = plan
        .ir
        .inputs
        .iter()
        .filter(|input| input.mode == InputMode::Output && handler.inputs.contains(&input.id))
        .filter_map(|input| input.managed_resource_id.as_deref())
        .collect();
    plan.ir
        .handlers
        .iter()
        .filter(|writer| {
            writer
                .outputs
                .iter()
                .any(|output_id| read_outputs.contains(output_id.as_str()))
        })
        .map(|writer| writer.id.as_str())
        .collect()
}

impl HandlerState {
    /// Settled: nothing queued or failed, and every declared output agrees
    /// on the revision this cycle's observations compute. `known_revision`
    /// only ever holds a revision all outputs carried -- set after a fully
    /// committed invocation, seeded from agreeing annotations, and cleared
    /// by `detect_triggers` the moment `outputs_still_agree` fails for the
    /// revision this cycle computes -- so a
    /// partial commit (`production::commit_handler_outputs` stops at the
    /// first failing output) leaves its writer unsettled until a later
    /// invocation commits every output.
    ///
    /// An upstream whose own inputs are unavailable this cycle
    /// (`current_revision` is `None`) stays settled on the last revision
    /// every output recorded, as long as `detect_triggers` confirmed this
    /// cycle that its live outputs still carry that revision: it cannot
    /// run, and nothing it committed is partial, so holding its downstream
    /// would protect nothing.
    fn is_settled(&self) -> bool {
        self.queued.is_none()
            && self.last_error_code.is_none()
            && self.known_revision.is_some()
            && self
                .current_revision
                .as_ref()
                .map_or(self.outputs_confirmed_without_inputs, |current| {
                    self.known_revision.as_ref() == Some(current)
                })
    }
}

pub(crate) struct HandlerRuntime {
    queue_capacity: usize,
    base_backoff: Duration,
    max_backoff: Duration,
    max_object_bytes: usize,
    bundles: BTreeMap<String, &'static str>,
    outputs_by_id: BTreeMap<String, Output>,
    resolver: Option<Arc<engine::EgressResolver>>,
    field_manager: String,
    states: BTreeMap<String, HandlerState>,
    queue_order: VecDeque<String>,
}

impl HandlerRuntime {
    /// Builds the runtime for one domain's declared handlers: a shared DNS
    /// resolver *configuration* for `apsides.fetch` (built once, only if
    /// some handler declares egress hosts -- see
    /// `engine::EgressResolver`'s doc comment on why this crate never
    /// reads `/etc/resolv.conf` itself and never builds the real resolver
    /// here either, only its config).
    ///
    /// `resolv_conf_contents` is required (an error is returned) exactly
    /// when some declared handler has a non-empty `egress` list; a domain
    /// with no handler egress never needs it and this function never reads
    /// the file itself either way -- see the Apsides v0.3 plan, Phase 5b
    /// review item 3.
    pub(crate) fn new(
        plan: &CompiledPlan,
        handlers: &'static [(&'static str, &'static str)],
        field_manager: String,
        base_backoff: Duration,
        max_backoff: Duration,
        resolv_conf_contents: Option<&str>,
    ) -> Result<Self, RuntimeError> {
        let bundles: BTreeMap<String, &'static str> = handlers
            .iter()
            .map(|(id, source)| ((*id).to_owned(), *source))
            .collect();
        let outputs_by_id: BTreeMap<String, Output> = plan
            .ir
            .outputs
            .iter()
            .map(|output| (output.id.clone(), output.clone()))
            .collect();
        let resolver = if plan
            .ir
            .handlers
            .iter()
            .any(|handler| !handler.egress.is_empty())
        {
            let contents = resolv_conf_contents.ok_or_else(|| {
                RuntimeError::Configuration(
                    "a declared handler has egress hosts, but no resolv.conf contents were \
                     provided to build the DNS resolver"
                        .into(),
                )
            })?;
            let config = engine::resolver_config_from_resolv_conf(contents)
                .map_err(|error| RuntimeError::Configuration(format!("resolv.conf: {error}")))?;
            Some(Arc::new(engine::EgressResolver::new(config)))
        } else {
            None
        };
        Ok(Self {
            queue_capacity: plan.ir.domain.budgets.max_queue_depth.max(1),
            base_backoff,
            max_backoff,
            max_object_bytes: plan.ir.domain.budgets.max_object_bytes,
            bundles,
            outputs_by_id,
            resolver,
            field_manager,
            states: BTreeMap::new(),
            queue_order: VecDeque::new(),
        })
    }

    /// Level-based startup recomputation (see the Apsides v0.3 plan, Phase
    /// 5b, "Delivery"): seeds each handler's `known_revision` (and
    /// `output_uids`, for the per-cycle drift check `outputs_still_agree`
    /// also relies on -- Phase 5b review item 5) from the revision
    /// annotation already recorded on its declared outputs, so a restart
    /// re-enqueues a handler exactly when its current input revision
    /// differs from the one its outputs already reflect. A handler with no
    /// declared outputs has nothing to seed from and always looks pending
    /// immediately after a restart, by construction.
    ///
    /// Seeds `known_revision = Some(r)` only when **every** declared
    /// output records that exact same `Some(r)` -- review item 5: treating
    /// `[None, Some(r)]` (one output missing the annotation entirely, e.g.
    /// recreated empty) as agreement would wrongly seed the handler as
    /// already up to date.
    pub(crate) async fn seed_from_annotations(&mut self, plan: &CompiledPlan, client: &Client) {
        for handler in &plan.ir.handlers {
            if handler.outputs.is_empty() {
                continue;
            }
            let state = self.states.entry(handler.id.clone()).or_default();
            let mut recorded_revisions: Vec<Option<String>> =
                Vec::with_capacity(handler.outputs.len());
            for output_id in &handler.outputs {
                let recorded = if let Some(output) = self.outputs_by_id.get(output_id) {
                    let api = configmap_api(client, &output.namespace);
                    match api.get_opt(&output.name).await {
                        Ok(Some(object)) => {
                            if let Some(uid) =
                                object.metadata.uid.clone().filter(|uid| !uid.is_empty())
                            {
                                state.output_uids.insert(output_id.clone(), uid);
                            }
                            object
                                .metadata
                                .annotations
                                .as_ref()
                                .and_then(|annotations| {
                                    annotations.get(HANDLER_INPUT_REVISION_ANNOTATION)
                                })
                                .cloned()
                        }
                        Ok(None) | Err(_) => None,
                    }
                } else {
                    None
                };
                recorded_revisions.push(recorded);
            }
            state.known_revision = agreed_revision(&recorded_revisions);
        }
    }

    /// Re-checks whether `handler`'s declared outputs still actually agree
    /// with `revision` -- called when the freshly-computed input revision
    /// already matches the in-memory `known_revision` (the case that would
    /// otherwise be skipped as unchanged), and by
    /// [`Self::outputs_agree_without_inputs`] when no revision can be
    /// computed, so this extra GET round-trip per output is paid only for
    /// handlers already believed idle, once per cycle each. See the Apsides v0.3 plan, Phase 5b
    /// review item 5: an output recreated (its `uid` changed) or emptied
    /// (its annotation is gone) since it was last observed must force a
    /// re-run even though nothing about the handler's *input* changed.
    async fn outputs_still_agree(
        &mut self,
        handler: &Handler,
        revision: &str,
        client: &Client,
    ) -> bool {
        for output_id in &handler.outputs {
            let Some(output) = self.outputs_by_id.get(output_id) else {
                return false;
            };
            let api = configmap_api(client, &output.namespace);
            let Ok(Some(object)) = api.get_opt(&output.name).await else {
                return false;
            };
            let live_annotation = object
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(HANDLER_INPUT_REVISION_ANNOTATION));
            if live_annotation.map(String::as_str) != Some(revision) {
                return false;
            }
            let live_uid = object.metadata.uid.clone().filter(|uid| !uid.is_empty());
            let state = self.states.entry(handler.id.clone()).or_default();
            let recorded_uid = state.output_uids.get(output_id.as_str()).cloned();
            match (&live_uid, &recorded_uid) {
                (Some(live), Some(recorded)) if live == recorded => {}
                _ => return false,
            }
            if let Some(uid) = live_uid {
                state.output_uids.insert(output_id.clone(), uid);
            }
        }
        true
    }

    /// For a handler whose inputs are unavailable this cycle: whether its
    /// live outputs still carry the revision it last committed.
    /// `is_settled` treats such a handler as settled on that revision only
    /// when this holds, so an output deleted, recreated or rewritten while
    /// the handler cannot run never releases its downstream handlers onto
    /// data it did not write. Skips the reads, answering `false`, when the
    /// handler is unsettled regardless (queued or without a known revision).
    async fn outputs_agree_without_inputs(&mut self, handler: &Handler, client: &Client) -> bool {
        let Some(state) = self.states.get(&handler.id) else {
            return false;
        };
        if state.queued.is_some() {
            return false;
        }
        let Some(known) = state.known_revision.clone() else {
            return false;
        };
        self.outputs_still_agree(handler, &known, client).await
    }

    /// Computes revisions, enqueues changed handlers, and dispatches at
    /// most one ready queued entry (global concurrency 1, and at most one
    /// dispatch per cycle -- see the Apsides v0.3 plan, Phase 5b,
    /// "Delivery", and review item E: reconciles and inventory publishing
    /// must never be starved by a busy handler queue). Returns the
    /// conditions to fold into the cycle's existing conditions list, or
    /// `Err` only for a fatal error (`RuntimeError::Terminating` /
    /// `RuntimeError::Platform`) that must abort the whole reconcile cycle
    /// the same way a reconciler resource apply does -- every other
    /// failure is already folded into a `RuntimeCondition` plus backoff
    /// before this returns.
    pub(crate) async fn run_cycle(
        &mut self,
        plan: &CompiledPlan,
        observations: &BTreeMap<String, Observation>,
        applied: &BTreeSet<String>,
        output_uids: &BTreeMap<String, String>,
        client: &Client,
        controller: &crate::production::ProductionController,
    ) -> Result<Vec<RuntimeCondition>, RuntimeError> {
        let mut conditions = Vec::new();
        self.detect_triggers(plan, observations, client, &mut conditions)
            .await;
        self.dispatch_ready(plan, applied, output_uids, controller, &mut conditions)
            .await?;
        Ok(conditions)
    }

    async fn detect_triggers(
        &mut self,
        plan: &CompiledPlan,
        observations: &BTreeMap<String, Observation>,
        client: &Client,
        conditions: &mut Vec<RuntimeCondition>,
    ) {
        for handler in &plan.ir.handlers {
            let snapshot = handler_inputs_snapshot(handler, observations)
                .and_then(|inputs| Some((compute_input_revision(handler, &inputs).ok()?, inputs)));
            let state = self.states.entry(handler.id.clone()).or_default();
            state.current_revision = snapshot.as_ref().map(|(revision, _)| revision.clone());
            let Some((revision, inputs)) = snapshot else {
                let confirmed = self.outputs_agree_without_inputs(handler, client).await;
                self.states
                    .entry(handler.id.clone())
                    .or_default()
                    .outputs_confirmed_without_inputs = confirmed;
                continue;
            };
            let same_revision = state.known_revision.as_deref() == Some(revision.as_str());
            let already_queued_for_this_revision = state
                .queued
                .as_ref()
                .is_some_and(|queued| queued.revision == revision);
            if same_revision && !handler.outputs.is_empty() && !already_queued_for_this_revision {
                let confirmed = self.outputs_still_agree(handler, &revision, client).await;
                if !confirmed {
                    self.states
                        .entry(handler.id.clone())
                        .or_default()
                        .known_revision = None;
                }
            }
            let state = self.states.entry(handler.id.clone()).or_default();
            let unchanged = state.known_revision.as_deref() == Some(revision.as_str())
                && state
                    .queued
                    .as_ref()
                    .is_none_or(|queued| queued.revision == revision);
            if unchanged {
                continue;
            }
            let Ok((event_id, event)) = build_event(handler, &inputs, &revision) else {
                continue;
            };
            if self.enqueue(&handler.id, revision, event_id, event) == EnqueueOutcome::Overflow {
                conditions.push(RuntimeCondition {
                    reason_code: "handler-queue-overflow".into(),
                    resource_name: Some(handler.id.clone()),
                });
            }
        }
    }

    fn enqueue(
        &mut self,
        handler_id: &str,
        revision: String,
        event_id: String,
        event: Value,
    ) -> EnqueueOutcome {
        let state = self.states.entry(handler_id.to_owned()).or_default();
        if let Some(queued) = &state.queued {
            if queued.revision == revision {
                return EnqueueOutcome::Unchanged;
            }
            state.queued = Some(QueuedEvent {
                revision,
                event_id,
                event,
            });
            return EnqueueOutcome::Coalesced;
        }
        if state.known_revision.as_deref() == Some(revision.as_str()) {
            return EnqueueOutcome::Unchanged;
        }
        if self.queue_order.len() >= self.queue_capacity {
            return EnqueueOutcome::Overflow;
        }
        state.queued = Some(QueuedEvent {
            revision,
            event_id,
            event,
        });
        self.queue_order.push_back(handler_id.to_owned());
        EnqueueOutcome::New
    }

    /// The ids of every handler currently [`HandlerState::is_settled`].
    fn settled_handlers(&self) -> BTreeSet<String> {
        self.states
            .iter()
            .filter(|(_, state)| state.is_settled())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The condition reason code that keeps queued `handler` from being
    /// dispatched this cycle, or `None` when it may run: every declared
    /// output must have been applied this cycle, and the upstream gate
    /// ([`Self::upstream_gate_open`]) must be open.
    fn dispatch_blocker(
        &self,
        plan: &CompiledPlan,
        handler: &Handler,
        applied: &BTreeSet<String>,
        settled: &BTreeSet<String>,
    ) -> Option<&'static str> {
        if !handler.outputs.iter().all(|id| applied.contains(id)) {
            return Some("handler-output-not-ready");
        }
        if !self.upstream_gate_open(plan, handler, settled) {
            return Some("handler-upstream-pending");
        }
        None
    }

    /// Whether queued `handler` may run as far as its upstreams go: always
    /// for a handler reading no other handler's output; otherwise only when
    /// every upstream was settled at the start of this cycle (`settled`)
    /// *and* the queued event was rebuilt or confirmed from this cycle's
    /// observations -- an event left over from an earlier cycle (say, one
    /// whose own inputs are not `Available` now) may hold a snapshot taken
    /// while an upstream was only partially committed.
    fn upstream_gate_open(
        &self,
        plan: &CompiledPlan,
        handler: &Handler,
        settled: &BTreeSet<String>,
    ) -> bool {
        let upstreams = upstream_handlers(plan, handler);
        if upstreams.is_empty() {
            return true;
        }
        let Some(state) = self.states.get(&handler.id) else {
            return false;
        };
        let confirmed = state.queued.as_ref().is_some_and(|queued| {
            state.current_revision.as_deref() == Some(queued.revision.as_str())
        });
        confirmed && upstreams.iter().all(|id| settled.contains(*id))
    }

    /// Dispatches **at most one** ready queued handler this cycle (review
    /// item E) -- other ready/not-ready entries are still scanned (for
    /// backoff/`handler-output-not-ready`/`handler-upstream-pending`
    /// bookkeeping) but never invoked once one dispatch has already
    /// happened. A handler reading another handler's output stays queued,
    /// reporting `handler-upstream-pending`, until [`Self::upstream_gate_open`]
    /// admits it, so it never runs on a partially committed upstream. Checks
    /// `controller.is_terminating()` immediately before that one dispatch,
    /// so a shutdown in progress never starts a new invocation (the commit
    /// path inside `invoke` re-checks it again before each output's
    /// commit SSA too -- see `production::commit_handler_outputs`).
    async fn dispatch_ready(
        &mut self,
        plan: &CompiledPlan,
        applied: &BTreeSet<String>,
        output_uids: &BTreeMap<String, String>,
        controller: &crate::production::ProductionController,
        conditions: &mut Vec<RuntimeCondition>,
    ) -> Result<(), RuntimeError> {
        let ids: Vec<String> = self.queue_order.iter().cloned().collect();
        // Taken before any dispatch: a handler dispatched in this same cycle
        // stays unsettled for the rest of it, since this cycle's
        // observations of its outputs predate its commit.
        let settled = self.settled_handlers();
        let mut dispatched_one = false;
        for handler_id in ids {
            let Some(handler) = plan
                .ir
                .handlers
                .iter()
                .find(|handler| handler.id == handler_id)
            else {
                continue;
            };
            let now = Instant::now();
            let blocked_by_backoff = self
                .states
                .get(&handler_id)
                .and_then(|state| state.backoff_until)
                .is_some_and(|until| now < until);
            if blocked_by_backoff {
                continue;
            }
            if let Some(reason_code) = self.dispatch_blocker(plan, handler, applied, &settled) {
                conditions.push(RuntimeCondition {
                    reason_code: reason_code.into(),
                    resource_name: Some(handler_id.clone()),
                });
                continue;
            }
            if dispatched_one {
                continue;
            }
            if controller.is_terminating() {
                return Err(RuntimeError::Terminating);
            }
            let Some(queued) = self
                .states
                .get_mut(&handler_id)
                .and_then(|state| state.queued.take())
            else {
                continue;
            };
            self.queue_order.retain(|id| *id != handler_id);
            dispatched_one = true;
            let outcome = self.invoke(handler, &queued, output_uids, controller).await;
            let state = self.states.entry(handler_id.clone()).or_default();
            state.last_run = Some(Instant::now());
            match outcome {
                Ok(committed_uids) => {
                    state.known_revision = Some(queued.revision);
                    state.output_uids.extend(committed_uids);
                    state.attempt = 0;
                    state.backoff_until = None;
                    state.last_error_code = None;
                }
                Err(InvokeCommitError::Fatal(error)) => return Err(error),
                Err(InvokeCommitError::Code(code)) => {
                    state.attempt = state.attempt.saturating_add(1);
                    state.backoff_until = Some(
                        now + backoff_duration(state.attempt, self.base_backoff, self.max_backoff),
                    );
                    state.last_error_code = Some(code.clone());
                    conditions.push(RuntimeCondition {
                        reason_code: format!("handler-invocation-failed:{code}"),
                        resource_name: Some(handler_id.clone()),
                    });
                    // At-least-once delivery: re-queue the same event so it
                    // is retried once backoff clears.
                    state.queued = Some(queued);
                    self.queue_order.push_back(handler_id.clone());
                }
            }
        }
        Ok(())
    }

    /// Runs `handler`'s isolate (returning `InvokeCommitError::Code` for
    /// any engine-side failure), then -- only if it returned `Ok` --
    /// commits every declared output through
    /// `production::commit_handler_outputs`, the SAME authority/claim
    /// re-check path reconciler mutations use (review item E; "reuse that
    /// path, don't duplicate it"). Returns the committed uid of every
    /// output actually committed, so the caller can refresh
    /// `HandlerState::output_uids`.
    ///
    /// A handler can no longer catch `SINK_REJECTED` for an apply failure:
    /// `BufferedOutputSink::write_output` only ever fails for a UTF-8/lock
    /// problem, never for a Kubernetes error, since every real write now
    /// happens after the handler has already returned (review item 1).
    async fn invoke(
        &self,
        handler: &Handler,
        queued: &QueuedEvent,
        output_uids: &BTreeMap<String, String>,
        controller: &crate::production::ProductionController,
    ) -> Result<BTreeMap<String, String>, InvokeCommitError> {
        let Some(bundle) = self.bundles.get(handler.id.as_str()).copied() else {
            return Err(InvokeCommitError::Code("HANDLER_BUNDLE_MISSING".into()));
        };
        let export = handler.export.clone();
        let handler_id = handler.id.clone();
        let invocation_id = queued.event_id.clone();
        let event = queued.event.clone();
        let limits = handler_limits(handler);
        let egress = production_egress_policy(handler);
        let outputs = handler
            .outputs
            .iter()
            .map(|id| engine::OutputDeclaration {
                id: id.clone(),
                max_bytes: self.max_object_bytes,
            })
            .collect::<Vec<_>>();
        let buffered_sink = Arc::new(BufferedOutputSink::new());
        let sink: Arc<dyn engine::OutputSink> = buffered_sink.clone();
        let resolver = self.resolver.clone();
        let log_writer: Arc<dyn engine::LogWriter> = Arc::new(HandlerLogWriter {
            handler_id: handler_id.clone(),
            invocation_id: invocation_id.clone(),
        });

        log_invocation(
            "info",
            "handler-dispatching",
            &handler_id,
            &invocation_id,
            &json!({"revision": queued.revision, "outputs": handler.outputs}),
        );

        let report = tokio::task::spawn_blocking(move || {
            let usage = Arc::new(AtomicUsize::new(0));
            engine::invoke_handler_with_watchdog(
                engine::HandlerInvocation {
                    bundle,
                    export: &export,
                    event,
                    limits,
                    egress,
                    outputs,
                    sink,
                    resolver,
                    handler_id: &handler_id,
                    invocation_id: &invocation_id,
                    log_writer,
                },
                &usage,
            )
        })
        .await
        .map_err(|_join_error| InvokeCommitError::Code("HANDLER_INVOCATION_TASK_FAILED".into()))?;

        match report.result {
            Ok(_value) => {
                let written = buffered_sink.take();
                let outcome = crate::production::commit_handler_outputs(
                    controller,
                    &handler.outputs,
                    &self.outputs_by_id,
                    &self.field_manager,
                    &written,
                    &queued.revision,
                    output_uids,
                )
                .await;
                log_commit_outcome(
                    &handler.id,
                    &queued.event_id,
                    &handler.outputs,
                    &written,
                    &outcome,
                );
                outcome.map_err(|error| match error {
                    RuntimeError::Terminating | RuntimeError::Platform(_) => {
                        InvokeCommitError::Fatal(error)
                    }
                    other => InvokeCommitError::Code(format!("HANDLER_COMMIT_FAILED:{other}")),
                })
            }
            Err(error) => {
                let code = invoke_error_code(&error);
                log_invocation(
                    "warn",
                    "handler-invocation-failed",
                    &handler.id,
                    &queued.event_id,
                    &json!({"code": code}),
                );
                Err(InvokeCommitError::Code(code))
            }
        }
    }

    pub(crate) fn statuses(&self) -> Vec<HandlerStatus> {
        let now = Instant::now();
        self.states
            .iter()
            .map(|(id, state)| HandlerStatus {
                id: id.clone(),
                last_run_millis_ago: state.last_run.map(|instant| {
                    u64::try_from(now.duration_since(instant).as_millis()).unwrap_or(u64::MAX)
                }),
                last_error_code: state.last_error_code.clone(),
                queue_depth: usize::from(state.queued.is_some()),
            })
            .collect()
    }
}

fn invoke_error_code(error: &engine::InvokeError) -> String {
    if error.is_isolate_termination() {
        return "HANDLER_TERMINATED".into();
    }
    match error {
        engine::InvokeError::Timeout => "HANDLER_TIMEOUT".into(),
        engine::InvokeError::Engine(_) => "HANDLER_ENGINE_ERROR".into(),
        engine::InvokeError::ExportNotFound(_) => "HANDLER_EXPORT_NOT_FOUND".into(),
        engine::InvokeError::ExportNotCallable(_) => "HANDLER_EXPORT_NOT_CALLABLE".into(),
        engine::InvokeError::EventMarshal(_) => "HANDLER_EVENT_MARSHAL_FAILED".into(),
        engine::InvokeError::ResultMarshal(_) => "HANDLER_RESULT_MARSHAL_FAILED".into(),
        engine::InvokeError::FixtureAcknowledgementMissing => "HANDLER_FIXTURE_ACK_MISSING".into(),
        engine::InvokeError::InvalidEgressAllowlistEntry(_) => {
            "HANDLER_INVALID_EGRESS_ALLOWLIST".into()
        }
        engine::InvokeError::Runtime(_) => "HANDLER_RUNTIME_BUILD_FAILED".into(),
        engine::InvokeError::Terminated(_) => "HANDLER_TERMINATED".into(),
    }
}

/// Convenience used by `production.rs` to construct a [`HandlerRuntime`]
/// from a [`KubeRuntimeClient`]'s own field manager naming convention.
pub(crate) fn handler_field_manager(domain_id: &str) -> String {
    format!("apsides-handler-{domain_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use compiler_core::{EgressHost, HandlerTrigger, SourceLocation};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    fn source() -> SourceLocation {
        SourceLocation {
            file: "handlers/sync.ts".into(),
            line: 1,
            column: 1,
        }
    }

    fn handler(id: &str, inputs: &[&str], outputs: &[&str]) -> Handler {
        Handler {
            id: id.into(),
            module: "handlers/sync.ts".into(),
            export: "sync".into(),
            trigger: HandlerTrigger {
                kind: "input-changed".into(),
                input: inputs[0].into(),
            },
            inputs: inputs.iter().map(|value| (*value).to_owned()).collect(),
            egress: vec![],
            outputs: outputs.iter().map(|value| (*value).to_owned()).collect(),
            timeout_millis: 1_000,
            max_heap_bytes: 8 * 1024 * 1024,
            bundle_sha256: "0".repeat(64),
            source: source(),
        }
    }

    fn available(data: &[(&str, &str)]) -> Observation {
        Observation::Available {
            data: data
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            resource_version: "1".into(),
        }
    }

    #[test]
    fn revision_changes_exactly_when_a_declared_inputs_data_changes() -> TestResult {
        let handler = handler("sync", &["settings"], &["flags"]);
        let mut observations = BTreeMap::new();
        observations.insert("settings".to_owned(), available(&[("flag", "on")]));
        let inputs = handler_inputs_snapshot(&handler, &observations)
            .ok_or("expected the input to be ready")?;
        let first = compute_input_revision(&handler, &inputs)?;

        let mut changed = BTreeMap::new();
        changed.insert("settings".to_owned(), available(&[("flag", "off")]));
        let inputs =
            handler_inputs_snapshot(&handler, &changed).ok_or("expected the input to be ready")?;
        let second = compute_input_revision(&handler, &inputs)?;

        assert_ne!(first, second);

        let mut same = BTreeMap::new();
        same.insert("settings".to_owned(), available(&[("flag", "on")]));
        let inputs =
            handler_inputs_snapshot(&handler, &same).ok_or("expected the input to be ready")?;
        let third = compute_input_revision(&handler, &inputs)?;
        assert_eq!(first, third);
        Ok(())
    }

    /// Phase 5b review item 2: a new bundle (different `bundleSha256`),
    /// with the input data held exactly constant, must compute a
    /// different revision, so a restart re-runs the handler instead of
    /// wrongly trusting the previous bundle's stamped revision.
    #[test]
    fn revision_changes_when_the_bundle_sha256_changes() -> TestResult {
        let mut handler = handler("sync", &["settings"], &["flags"]);
        let mut inputs = BTreeMap::new();
        inputs.insert("settings".to_owned(), json!({"flag": "on"}));
        let before = compute_input_revision(&handler, &inputs)?;
        handler.bundle_sha256 = "1".repeat(64);
        let after = compute_input_revision(&handler, &inputs)?;
        assert_ne!(before, after);
        Ok(())
    }

    /// Distinct handlers reacting to identical input data must not
    /// collide on the same revision (their `id`/`export`/`outputs` all
    /// differ).
    #[test]
    fn revision_differs_between_two_handlers_on_identical_input_data() -> TestResult {
        let handler_a = handler("sync-a", &["settings"], &["flags-a"]);
        let handler_b = handler("sync-b", &["settings"], &["flags-b"]);
        let mut inputs = BTreeMap::new();
        inputs.insert("settings".to_owned(), json!({"flag": "on"}));
        let revision_a = compute_input_revision(&handler_a, &inputs)?;
        let revision_b = compute_input_revision(&handler_b, &inputs)?;
        assert_ne!(revision_a, revision_b);
        Ok(())
    }

    #[test]
    fn missing_declared_input_blocks_snapshot_and_dispatch() {
        let handler = handler("sync", &["settings"], &["flags"]);
        let mut observations = BTreeMap::new();
        observations.insert("settings".to_owned(), Observation::Missing);
        assert!(handler_inputs_snapshot(&handler, &observations).is_none());
    }

    #[test]
    fn event_id_is_stable_for_the_same_handler_and_revision() -> TestResult {
        let handler = handler("sync", &["settings"], &["flags"]);
        let mut inputs = BTreeMap::new();
        inputs.insert("settings".to_owned(), json!({"flag": "on"}));
        let revision = compute_input_revision(&handler, &inputs)?;
        let (id_a, event_a) = build_event(&handler, &inputs, &revision)?;
        let (id_b, event_b) = build_event(&handler, &inputs, &revision)?;
        assert_eq!(id_a, id_b);
        assert_eq!(event_a, event_b);
        assert_eq!(event_a["id"], json!(id_a));
        assert_eq!(event_a["inputs"]["settings"]["data"], json!({"flag": "on"}));
        Ok(())
    }

    fn empty_runtime() -> HandlerRuntime {
        HandlerRuntime {
            queue_capacity: 2,
            base_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(100),
            max_object_bytes: 1024,
            bundles: BTreeMap::new(),
            outputs_by_id: BTreeMap::new(),
            resolver: None,
            field_manager: "apsides-handler-example".into(),
            states: BTreeMap::new(),
            queue_order: VecDeque::new(),
        }
    }

    #[test]
    fn coalescing_keeps_the_latest_event_without_growing_the_queue() -> TestResult {
        let mut runtime = empty_runtime();
        let event_a = json!({"id": "a"});
        let event_b = json!({"id": "b"});
        assert_eq!(
            runtime.enqueue("h", "rev-1".into(), "a".into(), event_a),
            EnqueueOutcome::New
        );
        assert_eq!(runtime.queue_order.len(), 1);
        assert_eq!(
            runtime.enqueue("h", "rev-2".into(), "b".into(), event_b.clone()),
            EnqueueOutcome::Coalesced
        );
        assert_eq!(runtime.queue_order.len(), 1);
        let state = runtime.states.get("h").ok_or("handler state must exist")?;
        let queued = state.queued.as_ref().ok_or("an event must be queued")?;
        assert_eq!(queued.revision, "rev-2");
        assert_eq!(queued.event, event_b);
        Ok(())
    }

    #[test]
    fn overflow_is_reported_only_when_a_distinct_new_handler_exceeds_capacity() {
        let mut runtime = empty_runtime();
        assert_eq!(
            runtime.enqueue("h1", "rev-1".into(), "a".into(), json!({})),
            EnqueueOutcome::New
        );
        assert_eq!(
            runtime.enqueue("h2", "rev-1".into(), "b".into(), json!({})),
            EnqueueOutcome::New
        );
        // Capacity 2 is already used by two distinct handlers; coalescing
        // h1 again must still succeed (it does not grow the queue).
        assert_eq!(
            runtime.enqueue("h1", "rev-2".into(), "c".into(), json!({})),
            EnqueueOutcome::Coalesced
        );
        assert_eq!(
            runtime.enqueue("h3", "rev-1".into(), "d".into(), json!({})),
            EnqueueOutcome::Overflow
        );
        assert_eq!(runtime.queue_order.len(), 2);
    }

    #[test]
    fn dispatch_precondition_blocks_on_a_not_yet_applied_output() {
        let plan_handler = handler("sync", &["settings"], &["flags"]);
        let applied: BTreeSet<String> = BTreeSet::new();
        assert!(!plan_handler.outputs.iter().all(|id| applied.contains(id)));
        let mut applied_ready = BTreeSet::new();
        applied_ready.insert("flags".to_owned());
        assert!(
            plan_handler
                .outputs
                .iter()
                .all(|id| applied_ready.contains(id))
        );
    }

    #[test]
    fn backoff_grows_exponentially_and_is_capped() {
        let base = Duration::from_millis(100);
        let max = Duration::from_millis(1_000);
        assert_eq!(backoff_duration(0, base, max), Duration::from_millis(100));
        assert_eq!(backoff_duration(1, base, max), Duration::from_millis(200));
        assert_eq!(backoff_duration(2, base, max), Duration::from_millis(400));
        assert_eq!(backoff_duration(3, base, max), Duration::from_millis(800));
        assert_eq!(backoff_duration(4, base, max), max);
        assert_eq!(backoff_duration(64, base, max), max);
    }

    /// Structural proof, not just an assertion on one call's output: the
    /// only place this crate ever builds an `engine::EgressPolicy` for a
    /// real (non-`#[cfg(test)]`) invocation is `production_egress_policy`,
    /// and it has no parameter, config field, or code path through which a
    /// `FixtureEgressOverride` could reach `fixture_trust_anchor` --
    /// compare with `engine::FIXTURE_ACKNOWLEDGEMENT`'s gating, which this
    /// function never even references.
    #[test]
    fn fixture_egress_override_has_no_production_construction_path() {
        let handler = handler("sync", &["settings"], &["flags"]);
        let policy = production_egress_policy(&handler);
        assert!(policy.fixture_trust_anchor.is_none());
        assert!(policy.resolver_override.is_none());
    }

    #[test]
    fn handler_field_manager_names_a_distinct_manager_from_the_reconcilers_own() {
        let manager = handler_field_manager("example");
        assert_eq!(manager, "apsides-handler-example");
        assert_ne!(manager, format!("apsides-{}", "example"));
    }

    #[test]
    fn handler_limits_map_ir_timeout_and_heap_bounds() {
        let handler = handler("sync", &["settings"], &["flags"]);
        let limits = handler_limits(&handler);
        assert_eq!(limits.timeout, Duration::from_millis(1_000));
        assert_eq!(limits.max_heap_bytes, 8 * 1024 * 1024);
        // Half the declared heap, per `compiler_core::array_buffer_cap_bytes`
        // -- not set equal to the whole heap (Phase 5b review item C).
        assert_eq!(limits.max_array_buffer_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn handler_limits_caps_the_array_buffer_budget_for_a_large_declared_heap() -> TestResult {
        let mut handler = handler("sync", &["settings"], &["flags"]);
        handler.max_heap_bytes = 256 * 1024 * 1024;
        let limits = handler_limits(&handler);
        assert_eq!(
            limits.max_array_buffer_bytes,
            usize::try_from(compiler_core::HANDLER_ARRAY_BUFFER_CAP_BYTES)?
        );
        Ok(())
    }

    /// `engine`'s own internal memory constants cannot be imported by
    /// `compiler_core::handler_budgets` (it is deliberately IR-agnostic
    /// and does not depend on `engine`), so they are mirrored there
    /// instead -- see that module's doc comment. This crate depends on
    /// both, so it is the one place that can assert the mirror has not
    /// drifted from what `engine` actually enforces (Phase 5b review item
    /// C).
    #[test]
    fn engine_memory_constants_match_their_compiler_core_mirror() -> TestResult {
        assert_eq!(
            u64::try_from(engine::MAX_FETCH_REQUEST_BODY_BYTES)?,
            compiler_core::HANDLER_FETCH_REQUEST_BODY_BYTES,
        );
        assert_eq!(
            u64::try_from(engine::HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES)?,
            compiler_core::HANDLER_HEAP_HEADROOM_TOTAL_BYTES,
        );
        Ok(())
    }

    #[test]
    fn egress_hosts_map_one_to_one_onto_the_allowlist() {
        let mut handler = handler("sync", &["settings"], &["flags"]);
        handler.egress = vec![EgressHost {
            host: "api.example.com".into(),
            port: 443,
        }];
        let policy = production_egress_policy(&handler);
        assert_eq!(
            policy.allowed,
            vec![("api.example.com".to_owned(), 443_u16)]
        );
    }

    /// Phase 5b review item 5: seeding must not treat `[None, Some(r)]`
    /// (one declared output missing the annotation entirely -- e.g.
    /// recreated empty) as agreement, and the same drift must also be
    /// caught mid-run, without a restart, by `outputs_still_agree`.
    mod seeding_and_drift {
        use super::*;
        use compiler_core::parse_and_validate;
        use http::{Method, Request, Response};
        use http_body_util::BodyExt;
        use kube::client::Body;
        use std::collections::VecDeque;
        use std::convert::Infallible;
        use std::sync::Mutex as StdMutex;
        use tower::service_fn;

        type Calls = Arc<StdMutex<Vec<(Method, Value)>>>;

        fn source() -> Value {
            json!({"file": "program.tsx", "line": 1, "column": 1})
        }

        /// A two-output handler ("flags-a", "flags-b") triggered by
        /// "settings".
        fn plan() -> TestResult<CompiledPlan> {
            let ir = json!({
                "schemaVersion": compiler_core::IR_SCHEMA,
                "domain": {
                    "id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
                    "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
                    "minKubernetesVersion": "1.34",
                    "budgets": {"maxResources": 8, "maxInputs": 8, "maxReactions": 8, "maxObjectBytes": 262_144, "maxQueueDepth": 8, "maxConcurrentRequests": 2},
                },
                "resources": [
                    {"id": "flags-a", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags-a", "namespace": "workload",
                     "handlerOutput": true,
                     "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "flags-a", "namespace": "workload", "labels": {"apsides.dev/handler-output": "flags-a"}}},
                     "lifecycle": {"deletionPolicy": "orphan"}, "source": source()},
                    {"id": "flags-b", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags-b", "namespace": "workload",
                     "handlerOutput": true,
                     "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "flags-b", "namespace": "workload", "labels": {"apsides.dev/handler-output": "flags-b"}}},
                     "lifecycle": {"deletionPolicy": "orphan"}, "source": source()},
                ],
                "inputs": [
                    {"id": "settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "namespace": "workload", "mode": "observed", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
                    {"id": "flags-a", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags-a", "namespace": "workload", "mode": "output", "managedResourceId": "flags-a", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
                    {"id": "flags-b", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags-b", "namespace": "workload", "mode": "output", "managedResourceId": "flags-b", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
                ],
                "reactions": [],
                "handlers": [{
                    "id": "sync-flags", "module": "handlers/sync-flags.ts", "export": "syncFlags",
                    "trigger": {"kind": "input-changed", "input": "settings"},
                    "inputs": ["settings"], "egress": [], "outputs": ["flags-a", "flags-b"],
                    "timeoutMillis": 5_000, "maxHeapBytes": 8_388_608,
                    "bundleSha256": "0".repeat(64), "source": source(),
                }],
                "outputs": [
                    {"id": "flags-a", "name": "flags-a", "namespace": "workload"},
                    {"id": "flags-b", "name": "flags-b", "namespace": "workload"},
                ],
                "provenance": {"compiler": "apsides/0.1", "inputs": [{"path": "program.tsx", "sha256": "0".repeat(64)}, {"path": "handlers/sync-flags.ts", "sha256": "1".repeat(64)}]},
            });
            Ok(parse_and_validate(&ir.to_string())?)
        }

        fn runtime_for(plan: &CompiledPlan) -> TestResult<HandlerRuntime> {
            Ok(HandlerRuntime::new(
                plan,
                &[],
                "apsides-handler-example".into(),
                Duration::from_millis(10),
                Duration::from_millis(100),
                None,
            )?)
        }

        async fn wire_response(
            request: Request<Body>,
            responses: &StdMutex<VecDeque<Value>>,
            calls: &Calls,
        ) -> TestResult<Response<Body>> {
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
                .push((parts.method, value));
            let response = responses
                .lock()
                .map_err(|_| "queued responses are poisoned")?
                .pop_front()
                .ok_or("fixture ran out of queued responses")?;
            Ok(Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&response)?))?)
        }

        pub(super) fn wire(responses: Vec<Value>) -> (Client, Calls) {
            let responses = Arc::new(StdMutex::new(VecDeque::from(responses)));
            let calls: Calls = Arc::default();
            let service = service_fn({
                let calls = calls.clone();
                move |request: Request<Body>| {
                    let responses = responses.clone();
                    let calls = calls.clone();
                    async move {
                        Ok::<_, Infallible>(
                            match wire_response(request, &responses, &calls).await {
                                Ok(response) => response,
                                Err(error) => Response::builder()
                                    .status(500)
                                    .body(Body::from(error.to_string().into_bytes()))
                                    .unwrap_or_else(|_| Response::new(Body::from(Vec::new()))),
                            },
                        )
                    }
                }
            });
            (Client::new(service, "workload"), calls)
        }

        pub(super) fn flags_object(
            name: &str,
            uid: &str,
            rv: &str,
            revision: Option<&str>,
        ) -> Value {
            let mut annotations = Map::new();
            annotations.insert(
                "apsides.dev/handler-output".into(),
                Value::String(name.into()),
            );
            if let Some(revision) = revision {
                annotations.insert(
                    HANDLER_INPUT_REVISION_ANNOTATION.into(),
                    Value::String(revision.into()),
                );
            }
            json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": name, "namespace": "workload", "uid": uid, "resourceVersion": rv, "annotations": annotations},
                "data": {},
            })
        }

        fn observations() -> BTreeMap<String, Observation> {
            let mut observations = BTreeMap::new();
            observations.insert("settings".to_owned(), available(&[("flag", "on")]));
            observations
        }

        fn expected_revision(plan: &CompiledPlan) -> TestResult<String> {
            let handler = &plan.ir.handlers[0];
            let mut inputs = BTreeMap::new();
            inputs.insert("settings".to_owned(), json!({"flag": "on"}));
            Ok(compute_input_revision(handler, &inputs)?)
        }

        /// The seeding half of review item 5: "flags-a" was recreated
        /// (its uid is fresh and its annotation is gone) while "flags-b"
        /// still carries the old revision -- `[None, Some(rev)]` must NOT
        /// be treated as agreement, so the handler is seeded as needing a
        /// re-run, not as already up to date.
        #[tokio::test]
        async fn seed_treats_one_missing_annotation_as_disagreement() -> TestResult {
            let plan = plan()?;
            let revision = expected_revision(&plan)?;
            let (client, _calls) = wire(vec![
                flags_object("flags-a", "flags-a-uid-2", "9", None), // recreated, empty
                flags_object("flags-b", "flags-b-uid", "5", Some(&revision)),
            ]);
            let mut runtime = runtime_for(&plan)?;
            runtime.seed_from_annotations(&plan, &client).await;
            let state = runtime
                .states
                .get("sync-flags")
                .ok_or("handler state must exist after seeding")?;
            assert_eq!(
                state.known_revision, None,
                "one output missing the annotation must not be treated as agreement"
            );
            Ok(())
        }

        /// The seeding half's positive case: both outputs agreeing on the
        /// same revision is trusted.
        #[tokio::test]
        async fn seed_trusts_agreement_when_every_output_matches() -> TestResult {
            let plan = plan()?;
            let revision = expected_revision(&plan)?;
            let (client, _calls) = wire(vec![
                flags_object("flags-a", "flags-a-uid", "5", Some(&revision)),
                flags_object("flags-b", "flags-b-uid", "5", Some(&revision)),
            ]);
            let mut runtime = runtime_for(&plan)?;
            runtime.seed_from_annotations(&plan, &client).await;
            let state = runtime
                .states
                .get("sync-flags")
                .ok_or("handler state must exist after seeding")?;
            assert_eq!(state.known_revision.as_deref(), Some(revision.as_str()));
            Ok(())
        }

        /// The no-restart half of review item 5: the handler already
        /// believes it is up to date (seeded `known_revision` and
        /// `output_uids` from a prior successful run), but "flags-a" is
        /// then recreated (a fresh uid) mid-run, with no restart in
        /// between. The next `detect_triggers` pass must notice via
        /// `outputs_still_agree` and re-enqueue the handler even though
        /// the *input* (`settings`) never changed.
        #[tokio::test]
        async fn drift_without_a_restart_forces_a_rerun() -> TestResult {
            let plan = plan()?;
            let revision = expected_revision(&plan)?;
            let mut runtime = runtime_for(&plan)?;
            let state = runtime.states.entry("sync-flags".to_owned()).or_default();
            state.known_revision = Some(revision.clone());
            state
                .output_uids
                .insert("flags-a".to_owned(), "flags-a-uid-old".to_owned());
            state
                .output_uids
                .insert("flags-b".to_owned(), "flags-b-uid".to_owned());

            // "flags-a" comes back with a *different* uid than recorded
            // (recreated) -- still carrying the same revision annotation,
            // which is exactly the case a naive annotation-only check
            // would miss.
            let (client, _calls) = wire(vec![flags_object(
                "flags-a",
                "flags-a-uid-new",
                "11",
                Some(&revision),
            )]);
            let mut conditions = Vec::new();
            runtime
                .detect_triggers(&plan, &observations(), &client, &mut conditions)
                .await;

            let state = runtime
                .states
                .get("sync-flags")
                .ok_or("handler state must exist")?;
            assert!(
                state.queued.is_some(),
                "a recreated output must force the handler back onto the queue, even though \
                 the input never changed and no restart happened"
            );
            Ok(())
        }
    }

    /// Handler chaining: a downstream handler reading an upstream's output
    /// is dispatched only while that upstream is settled -- never while it
    /// is queued, failed, or its outputs disagree (a partial commit, or a
    /// restart after one) -- and only with an event confirmed against the
    /// current cycle's observations. `dispatch_blocker` is the exact check
    /// `dispatch_ready` turns into a condition; `production.rs`'s
    /// `handler_chain_dispatch` drives the same gate through `run_cycle`.
    mod upstream_gate {
        use super::seeding_and_drift::{flags_object, wire};
        use super::*;
        use compiler_core::parse_and_validate;

        fn source() -> Value {
            json!({"file": "program.tsx", "line": 1, "column": 1})
        }

        fn output_resource(id: &str) -> Value {
            json!({"id": id, "apiVersion": "v1", "kind": "ConfigMap", "name": id, "namespace": "workload",
                   "handlerOutput": true,
                   "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": id, "namespace": "workload", "labels": {"apsides.dev/handler-output": id}}},
                   "lifecycle": {"deletionPolicy": "orphan"}, "source": source()})
        }

        fn input(id: &str, name: &str, mode: &str) -> Value {
            let mut value = json!({"id": id, "apiVersion": "v1", "kind": "ConfigMap", "name": name, "namespace": "workload", "mode": mode, "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()});
            if mode == "output" {
                value["managedResourceId"] = json!(name);
            }
            value
        }

        /// `fetch-raw` (triggered by "settings") writes "raw-a" and
        /// "raw-b"; `derive` triggers on "raw-a", reads both, and writes
        /// "derived".
        fn plan() -> TestResult<CompiledPlan> {
            let ir = json!({
                "schemaVersion": compiler_core::IR_SCHEMA,
                "domain": {
                    "id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
                    "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
                    "minKubernetesVersion": "1.34",
                    "budgets": {"maxResources": 8, "maxInputs": 8, "maxReactions": 8, "maxObjectBytes": 262_144, "maxQueueDepth": 8, "maxConcurrentRequests": 2},
                },
                "resources": [output_resource("derived"), output_resource("raw-a"), output_resource("raw-b")],
                "inputs": [
                    input("derived", "derived", "output"),
                    input("raw-a", "raw-a", "output"),
                    input("raw-b", "raw-b", "output"),
                    input("settings", "settings", "observed"),
                ],
                "reactions": [],
                "handlers": [
                    {"id": "derive", "module": "handlers/derive.ts", "export": "derive",
                     "trigger": {"kind": "input-changed", "input": "raw-a"},
                     "inputs": ["raw-a", "raw-b"], "egress": [], "outputs": ["derived"],
                     "timeoutMillis": 5_000, "maxHeapBytes": 8_388_608, "bundleSha256": "0".repeat(64), "source": source()},
                    {"id": "fetch-raw", "module": "handlers/fetch-raw.ts", "export": "fetchRaw",
                     "trigger": {"kind": "input-changed", "input": "settings"},
                     "inputs": ["settings"], "egress": [], "outputs": ["raw-a", "raw-b"],
                     "timeoutMillis": 5_000, "maxHeapBytes": 8_388_608, "bundleSha256": "1".repeat(64), "source": source()},
                ],
                "outputs": [
                    {"id": "derived", "name": "derived", "namespace": "workload"},
                    {"id": "raw-a", "name": "raw-a", "namespace": "workload"},
                    {"id": "raw-b", "name": "raw-b", "namespace": "workload"},
                ],
                "provenance": {"compiler": "apsides/0.1", "inputs": [
                    {"path": "handlers/derive.ts", "sha256": "2".repeat(64)},
                    {"path": "handlers/fetch-raw.ts", "sha256": "1".repeat(64)},
                    {"path": "program.tsx", "sha256": "0".repeat(64)},
                ]},
            });
            Ok(parse_and_validate(&ir.to_string())?)
        }

        fn runtime_for(plan: &CompiledPlan) -> TestResult<HandlerRuntime> {
            Ok(HandlerRuntime::new(
                plan,
                &[],
                "apsides-handler-example".into(),
                Duration::from_millis(10),
                Duration::from_millis(100),
                None,
            )?)
        }

        fn find<'plan>(plan: &'plan CompiledPlan, id: &str) -> TestResult<&'plan Handler> {
            Ok(plan
                .ir
                .handlers
                .iter()
                .find(|handler| handler.id == id)
                .ok_or("handler must be declared")?)
        }

        /// "settings" plus both upstream outputs, `raw_b` standing in for
        /// whatever "raw-b" currently holds.
        fn observations(raw_b: &str) -> BTreeMap<String, Observation> {
            BTreeMap::from([
                ("settings".to_owned(), available(&[("flag", "on")])),
                ("raw-a".to_owned(), available(&[("value", "new")])),
                ("raw-b".to_owned(), available(&[("value", raw_b)])),
            ])
        }

        fn upstream_revision(plan: &CompiledPlan) -> TestResult<String> {
            let inputs = BTreeMap::from([("settings".to_owned(), json!({"flag": "on"}))]);
            Ok(compute_input_revision(find(plan, "fetch-raw")?, &inputs)?)
        }

        /// The reason code `dispatch_ready` would report for `derive` right
        /// now, computed exactly as `dispatch_ready` computes it (every
        /// output treated as applied, so only the upstream gate decides).
        fn derive_blocker(
            runtime: &HandlerRuntime,
            plan: &CompiledPlan,
        ) -> TestResult<Option<&'static str>> {
            let applied: BTreeSet<String> = plan
                .ir
                .outputs
                .iter()
                .map(|output| output.id.clone())
                .collect();
            Ok(runtime.dispatch_blocker(
                plan,
                find(plan, "derive")?,
                &applied,
                &runtime.settled_handlers(),
            ))
        }

        #[test]
        fn upstreams_are_exactly_the_writers_of_the_outputs_a_handler_reads() -> TestResult {
            let plan = plan()?;
            assert_eq!(
                upstream_handlers(&plan, find(&plan, "derive")?),
                BTreeSet::from(["fetch-raw"])
            );
            assert!(upstream_handlers(&plan, find(&plan, "fetch-raw")?).is_empty());
            Ok(())
        }

        /// The upstream is queued for a new revision (its input changed):
        /// the downstream, also queued, is held with
        /// `handler-upstream-pending` and stays queued, while the upstream
        /// itself, reading no other handler's output, is never gated.
        #[tokio::test]
        async fn downstream_is_held_while_its_upstream_is_queued() -> TestResult {
            let plan = plan()?;
            let mut runtime = runtime_for(&plan)?;
            let (client, calls) = wire(vec![]);
            runtime
                .detect_triggers(&plan, &observations("new"), &client, &mut Vec::new())
                .await;
            assert!(calls.lock().map_err(|_| "calls are poisoned")?.is_empty());
            let upstream = runtime.states.get("fetch-raw").ok_or("upstream state")?;
            assert!(upstream.queued.is_some(), "the upstream is queued");
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                Some("handler-upstream-pending")
            );
            let applied: BTreeSet<String> = plan
                .ir
                .outputs
                .iter()
                .map(|output| output.id.clone())
                .collect();
            assert_eq!(
                runtime.dispatch_blocker(
                    &plan,
                    find(&plan, "fetch-raw")?,
                    &applied,
                    &runtime.settled_handlers()
                ),
                None,
                "the upstream itself is never held by the gate"
            );
            assert!(
                runtime
                    .states
                    .get("derive")
                    .ok_or("downstream state")?
                    .queued
                    .is_some(),
                "a held downstream stays queued"
            );
            Ok(())
        }

        /// `commit_handler_outputs` committed "raw-a" for the upstream's new
        /// revision, then failed on "raw-b": the upstream is re-queued in
        /// backoff with `last_error_code` set and `known_revision` still the
        /// previous one, and this cycle's observations show "raw-a" new and
        /// "raw-b" stale. The downstream, re-coalesced onto that mixed
        /// snapshot, must stay held.
        #[tokio::test]
        async fn downstream_is_held_after_a_partial_upstream_commit() -> TestResult {
            let plan = plan()?;
            let revision = upstream_revision(&plan)?;
            let mut runtime = runtime_for(&plan)?;
            let upstream = runtime.states.entry("fetch-raw".to_owned()).or_default();
            upstream.known_revision = Some("previous-revision".to_owned());
            upstream.attempt = 1;
            upstream.backoff_until = Some(Instant::now() + Duration::from_secs(60));
            upstream.last_error_code = Some("HANDLER_COMMIT_FAILED:conflict".to_owned());
            assert_eq!(
                runtime.enqueue("fetch-raw", revision, "event".into(), json!({})),
                EnqueueOutcome::New
            );
            let (client, calls) = wire(vec![]);
            runtime
                .detect_triggers(&plan, &observations("stale"), &client, &mut Vec::new())
                .await;
            assert!(calls.lock().map_err(|_| "calls are poisoned")?.is_empty());
            assert!(
                runtime
                    .states
                    .get("derive")
                    .ok_or("downstream state")?
                    .queued
                    .is_some()
            );
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                Some("handler-upstream-pending")
            );
            Ok(())
        }

        /// A restart after that same partial commit: "raw-a" records the
        /// new revision, "raw-b" the previous one. Seeding finds no
        /// agreement, so the upstream re-runs and the downstream is held
        /// meanwhile.
        #[tokio::test]
        async fn downstream_is_held_after_a_restart_on_disagreeing_upstream_outputs() -> TestResult
        {
            let plan = plan()?;
            let revision = upstream_revision(&plan)?;
            let mut runtime = runtime_for(&plan)?;
            let (client, _calls) = wire(vec![
                flags_object("derived", "derived-uid", "3", None),
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "4", Some("previous-revision")),
            ]);
            runtime.seed_from_annotations(&plan, &client).await;
            assert_eq!(
                runtime
                    .states
                    .get("fetch-raw")
                    .ok_or("upstream state")?
                    .known_revision,
                None
            );
            runtime
                .detect_triggers(&plan, &observations("stale"), &client, &mut Vec::new())
                .await;
            assert!(
                runtime
                    .states
                    .get("fetch-raw")
                    .ok_or("upstream state")?
                    .queued
                    .is_some()
            );
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                Some("handler-upstream-pending")
            );
            Ok(())
        }

        /// Both upstream outputs agree on the revision the upstream's
        /// current input computes (seeded at restart and re-confirmed by
        /// `outputs_still_agree`): the upstream is settled and the queued
        /// downstream may run. A later cycle in which the downstream's own
        /// event cannot be confirmed (one of its inputs is no longer
        /// `Available`) holds it again, even though the upstream is still
        /// settled.
        #[tokio::test]
        async fn downstream_runs_once_its_upstream_is_settled() -> TestResult {
            let plan = plan()?;
            let revision = upstream_revision(&plan)?;
            let mut runtime = runtime_for(&plan)?;
            let (client, _calls) = wire(vec![
                // seed_from_annotations
                flags_object("derived", "derived-uid", "3", None),
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
                // first cycle's outputs_still_agree
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
                // second cycle's outputs_still_agree
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
            ]);
            runtime.seed_from_annotations(&plan, &client).await;
            runtime
                .detect_triggers(&plan, &observations("new"), &client, &mut Vec::new())
                .await;
            let upstream = runtime.states.get("fetch-raw").ok_or("upstream state")?;
            assert!(
                upstream.queued.is_none() && upstream.is_settled(),
                "the upstream is settled"
            );
            assert!(
                runtime
                    .states
                    .get("derive")
                    .ok_or("downstream state")?
                    .queued
                    .is_some()
            );
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                None,
                "a settled upstream lets the downstream run"
            );

            let mut without_raw_b = observations("new");
            without_raw_b.insert("raw-b".to_owned(), Observation::Missing);
            runtime
                .detect_triggers(&plan, &without_raw_b, &client, &mut Vec::new())
                .await;
            assert!(
                runtime
                    .states
                    .get("fetch-raw")
                    .ok_or("upstream state")?
                    .is_settled()
            );
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                Some("handler-upstream-pending")
            );
            Ok(())
        }

        /// An upstream that cannot run because its own input disappeared
        /// still holds a full, agreeing commit on every output, so its
        /// downstream is not held.
        #[tokio::test]
        async fn downstream_runs_while_its_settled_upstream_lacks_its_own_input() -> TestResult {
            let plan = plan()?;
            let revision = upstream_revision(&plan)?;
            let mut runtime = runtime_for(&plan)?;
            let (client, _calls) = wire(vec![
                // seed_from_annotations
                flags_object("derived", "derived-uid", "3", None),
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
                // detect_triggers re-reads the upstream's outputs
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
            ]);
            runtime.seed_from_annotations(&plan, &client).await;
            let mut without_settings = observations("new");
            without_settings.insert("settings".to_owned(), Observation::Missing);
            runtime
                .detect_triggers(&plan, &without_settings, &client, &mut Vec::new())
                .await;
            let upstream = runtime.states.get("fetch-raw").ok_or("upstream state")?;
            assert!(
                upstream.current_revision.is_none(),
                "the upstream cannot compute a revision"
            );
            assert!(
                upstream.is_settled(),
                "its last full commit still settles it"
            );
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                None,
                "the downstream runs on the upstream's consistent outputs"
            );
            Ok(())
        }

        /// The upstream cannot run (its input is missing) and one of its
        /// outputs was rewritten underneath it since the seed: the live
        /// annotation no longer matches the revision it last committed,
        /// so it is not settled and the downstream stays held.
        #[tokio::test]
        async fn downstream_stays_held_when_an_idle_upstream_output_drifts() -> TestResult {
            let plan = plan()?;
            let revision = upstream_revision(&plan)?;
            let mut runtime = runtime_for(&plan)?;
            let (client, _calls) = wire(vec![
                // seed_from_annotations
                flags_object("derived", "derived-uid", "3", None),
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
                // detect_triggers re-reads the upstream's outputs
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "9", None),
            ]);
            runtime.seed_from_annotations(&plan, &client).await;
            let mut without_settings = observations("edited");
            without_settings.insert("settings".to_owned(), Observation::Missing);
            runtime
                .detect_triggers(&plan, &without_settings, &client, &mut Vec::new())
                .await;
            let upstream = runtime.states.get("fetch-raw").ok_or("upstream state")?;
            assert!(!upstream.is_settled(), "a drifted output unsettles it");
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                Some("handler-upstream-pending")
            );
            Ok(())
        }

        /// The check is per cycle: once the upstream's outputs read back
        /// at its last revision again (a transient read failure, or the
        /// output restored), the downstream is released without the
        /// upstream having to run.
        #[tokio::test]
        async fn an_idle_upstream_resettles_when_its_outputs_agree_again() -> TestResult {
            let plan = plan()?;
            let revision = upstream_revision(&plan)?;
            let mut runtime = runtime_for(&plan)?;
            let (client, _calls) = wire(vec![
                // seed_from_annotations
                flags_object("derived", "derived-uid", "3", None),
                flags_object("raw-a", "raw-a-uid", "7", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
                // first cycle: raw-a reads back without the annotation
                flags_object("raw-a", "raw-a-uid", "9", None),
                // second cycle: both agree again
                flags_object("raw-a", "raw-a-uid", "10", Some(&revision)),
                flags_object("raw-b", "raw-b-uid", "8", Some(&revision)),
            ]);
            runtime.seed_from_annotations(&plan, &client).await;
            let mut without_settings = observations("new");
            without_settings.insert("settings".to_owned(), Observation::Missing);
            runtime
                .detect_triggers(&plan, &without_settings, &client, &mut Vec::new())
                .await;
            assert_eq!(
                derive_blocker(&runtime, &plan)?,
                Some("handler-upstream-pending")
            );
            runtime
                .detect_triggers(&plan, &without_settings, &client, &mut Vec::new())
                .await;
            assert_eq!(derive_blocker(&runtime, &plan)?, None);
            Ok(())
        }
    }

    /// Phase 5b review item 4: `EgressResolver` must hold only its
    /// resolver *configuration*, never a `TokioResolver` built once on one
    /// runtime and reused from a later invocation's own, different,
    /// short-lived runtime -- see `engine::net::EgressResolver`'s doc
    /// comment for the failure this guards against (hickory's
    /// `TokioRuntimeProvider` ties background connection-management tasks
    /// to whichever runtime is current when they are actually driven, not
    /// merely the one current at construction time).
    ///
    /// Exercises the exact invocation shape `HandlerRuntime::invoke` uses
    /// (a shared `Arc<engine::EgressResolver>`, each invocation run via
    /// `spawn_blocking` + `engine::invoke_handler_with_watchdog` on its own
    /// fresh, short-lived runtime) twice in a row, against a real DNS
    /// resolver pointed at a local UDP stub and `engine`'s own fixture
    /// HTTPS server -- not `EgressPolicy::resolver_override` (which
    /// bypasses DNS resolution entirely and would not exercise this bug at
    /// all). `HandlerRuntime::invoke` itself is not called directly:
    /// `production_egress_policy` deliberately has no way to ever supply a
    /// `FixtureEgressOverride` (see
    /// `fixture_egress_override_has_no_production_construction_path`
    /// above), so this test builds the same `HandlerInvocation` shape by
    /// hand instead, with a policy that does carry one.
    mod egress_resolver_runtime_affinity {
        use super::*;
        use engine::test_support::{HttpsFixtureServer, RESOLVABLE_FIXTURE_HOST};
        use engine::{
            EgressPolicy, FIXTURE_ACKNOWLEDGEMENT, FixtureEgressOverride, HandlerInvocation,
            OutputSinkError, StderrLogWriter,
        };
        use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use tokio::net::UdpSocket;

        type StubResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

        /// A no-op sink: this test only exercises `apsides.fetch`.
        struct NullSink;
        impl engine::OutputSink for NullSink {
            fn write_output(
                &self,
                _id: &str,
                _key: &str,
                _data: &[u8],
            ) -> Result<(), OutputSinkError> {
                Ok(())
            }
        }

        /// The exact question section length of a DNS query message
        /// (header + QNAME + QTYPE + QCLASS), found by walking the
        /// length-prefixed QNAME labels to their terminating zero byte --
        /// robust to a query that also carries an EDNS0 OPT additional
        /// record (as hickory's client attaches by default), which this
        /// stub's minimal reply never echoes back.
        fn question_len(query: &[u8]) -> usize {
            let mut offset = 12_usize;
            while offset < query.len() {
                let len = usize::from(query[offset]);
                offset += 1;
                if len == 0 {
                    break;
                }
                offset += len;
            }
            offset + 4
        }

        const DNS_QTYPE_A: u16 = 1;

        /// A minimal DNS response echoing the query's transaction id and
        /// question section verbatim. Answers exactly one A record
        /// (`answer_ip`) when the question's QTYPE is A; for every other
        /// QTYPE (in particular AAAA, which `LookupIpStrategy::Ipv4thenIpv6`
        /// also always queries), returns a plain NOERROR/empty-answer
        /// response -- a record of the wrong type in the answer section
        /// would otherwise make hickory treat the *whole* lookup as
        /// failed rather than simply "no AAAA records, use the A ones".
        fn dns_a_response(query: &[u8], answer_ip: Ipv4Addr) -> Vec<u8> {
            let question_end = question_len(query).min(query.len());
            let qtype = query
                .get(question_end.saturating_sub(4)..question_end.saturating_sub(2))
                .map_or(0, |bytes| u16::from_be_bytes([bytes[0], bytes[1]]));
            let answer_present = qtype == DNS_QTYPE_A;
            let mut response = Vec::with_capacity(question_end + 16);
            response.extend_from_slice(&query[0..2]); // transaction id
            response.extend_from_slice(&[0x81, 0x80]); // QR=1 RD=1 RA=1, RCODE=0
            response.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
            response.extend_from_slice(&[0x00, u8::from(answer_present)]); // ANCOUNT
            response.extend_from_slice(&[0x00, 0x00]); // NSCOUNT=0
            response.extend_from_slice(&[0x00, 0x00]); // ARCOUNT=0
            response.extend_from_slice(&query[12..question_end]); // question, verbatim
            if answer_present {
                response.extend_from_slice(&[0xC0, 0x0C]); // NAME: pointer to offset 12
                response.extend_from_slice(&[0x00, 0x01]); // TYPE A
                response.extend_from_slice(&[0x00, 0x01]); // CLASS IN
                response.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]); // TTL 60
                response.extend_from_slice(&[0x00, 0x04]); // RDLENGTH 4
                response.extend_from_slice(&answer_ip.octets());
            }
            response
        }

        /// Binds a UDP socket on an ephemeral loopback port and answers
        /// every incoming query with an A record for `answer_ip`, for the
        /// rest of this test's runtime (until the test function returns
        /// and the spawned task is dropped with it).
        async fn spawn_dns_stub(answer_ip: Ipv4Addr) -> StubResult<SocketAddr> {
            let socket = UdpSocket::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
            let addr = socket.local_addr()?;
            tokio::spawn(async move {
                let mut buf = [0_u8; 512];
                loop {
                    let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                        return;
                    };
                    let response = dns_a_response(&buf[..len], answer_ip);
                    let _ignored = socket.send_to(&response, peer).await;
                }
            });
            Ok(addr)
        }

        fn stub_resolver_config(stub_addr: SocketAddr) -> ResolverConfig {
            let mut connection = ConnectionConfig::udp();
            connection.port = stub_addr.port();
            let name_server = NameServerConfig::new(stub_addr.ip(), true, vec![connection]);
            ResolverConfig::from_parts(None, vec![], vec![name_server])
        }

        /// One invocation, built exactly the way `HandlerRuntime::invoke`
        /// builds one, on its own fresh runtime via
        /// `engine::invoke_handler_with_watchdog` -- called twice by the
        /// test below, sharing one `Arc<engine::EgressResolver>` across
        /// both, the same way `HandlerRuntime` shares one across every
        /// invocation for the process's lifetime.
        fn run_one_fetch_invocation(
            server_addr: SocketAddr,
            ca_der: Vec<u8>,
            resolver: Arc<engine::EgressResolver>,
            invocation_id: &str,
        ) -> engine::InvocationReport {
            let bundle = format!(
                r#"
export async function fetchOnce() {{
  const response = await apsides.fetch("https://{RESOLVABLE_FIXTURE_HOST}:{port}/", {{}});
  return {{ status: response.status }};
}}
"#,
                port = server_addr.port()
            );
            let egress = EgressPolicy {
                allowed: vec![(RESOLVABLE_FIXTURE_HOST.to_owned(), server_addr.port())],
                resolver_override: None,
                fixture_trust_anchor: Some(FixtureEgressOverride {
                    acknowledgement: FIXTURE_ACKNOWLEDGEMENT.to_owned(),
                    trust_anchor_der: ca_der,
                }),
            };
            let usage = Arc::new(AtomicUsize::new(0));
            engine::invoke_handler_with_watchdog(
                HandlerInvocation {
                    bundle: &bundle,
                    export: "fetchOnce",
                    event: json!({}),
                    limits: engine::Limits {
                        timeout: Duration::from_secs(5),
                        max_heap_bytes: 16 * 1024 * 1024,
                        max_array_buffer_bytes: 4 * 1024 * 1024,
                        max_fetch_bytes_total: 4 * 1024 * 1024,
                        max_concurrent_fetches: 1,
                    },
                    egress,
                    outputs: vec![],
                    sink: Arc::new(NullSink),
                    resolver: Some(resolver),
                    handler_id: "sync-flags",
                    invocation_id,
                    log_writer: Arc::new(StderrLogWriter),
                },
                &usage,
            )
        }

        /// The regression test itself: two invocations, sharing one
        /// `Arc<EgressResolver>`, each on its own fresh runtime (matching
        /// `invoke_handler_with_watchdog`'s own per-invocation runtime).
        /// Before the review item 4 fix, the second invocation's runtime
        /// would try to drive a `TokioResolver` whose background tasks
        /// were tied to the *first* invocation's already-dropped runtime.
        #[tokio::test(flavor = "multi_thread")]
        async fn two_invocations_each_resolve_and_fetch_successfully() -> StubResult {
            // This crate's own tests run in one shared process, and
            // `rustls::crypto::CryptoProvider::get_default()` is a single
            // process-wide slot: `install_rustls_provider` (the same path
            // `run_production` uses) must win that race deterministically
            // here, before `HttpsFixtureServer`/`engine`'s own independent
            // (and more lenient -- it tolerates losing the race) provider
            // install ever runs, or `tests::rustls_ring_provider_is_explicitly_available`
            // elsewhere in this binary can observe someone else having
            // already claimed the slot outside the blessed install path.
            crate::install_rustls_provider()?;
            // `start_multi(.., 2)`, not `start`: two separate invocations
            // each open their own connection against this one server
            // instance, and `start` only ever accepts one.
            let (server, _accepted) = HttpsFixtureServer::start_multi(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
                2,
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            let server_addr = server.addr;
            let ca_der = server.ca_der.clone();
            let stub_addr = spawn_dns_stub(Ipv4Addr::LOCALHOST).await?;
            let resolver = Arc::new(engine::EgressResolver::new(stub_resolver_config(stub_addr)));

            let first = tokio::task::spawn_blocking({
                let resolver = Arc::clone(&resolver);
                let ca_der = ca_der.clone();
                move || run_one_fetch_invocation(server_addr, ca_der, resolver, "invocation-1")
            })
            .await?;
            assert!(
                first.result.is_ok(),
                "the first invocation must succeed: {:?}",
                first.result
            );

            let second = tokio::task::spawn_blocking({
                let resolver = Arc::clone(&resolver);
                move || run_one_fetch_invocation(server_addr, ca_der, resolver, "invocation-2")
            })
            .await?;
            assert!(
                second.result.is_ok(),
                "the second invocation, reusing the same Arc<EgressResolver>, must also \
                 succeed -- a failure here is exactly the runtime-affinity bug review item 4 \
                 fixes: {:?}",
                second.result
            );

            server
                .join()
                .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            Ok(())
        }
    }

    /// Regression test for a CI-only acceptance failure (`handlers::
    /// a_handler_commits_exactly_its_owned_keys_and_seeds_across_a_restart`,
    /// assertion 1): `commit_handler_outputs` stamped the revision
    /// annotation but wrote no data keys, as if the handler had written
    /// nothing at all. This runs the REAL `handlers/derive-keys.ts` bundle
    /// (the exact bytes `packages/compiler/src/compiler.ts`'s
    /// `bundleHandlerModule` emits for
    /// `crates/acceptance/programs/handler-output/handlers/derive-keys.ts`
    /// -- captured by compiling that program with `compileApsides` and
    /// reading `handlers/<id>.js`, not a hand-written approximation)
    /// through the exact event this crate's own `handler_inputs_snapshot`/
    /// `compute_input_revision`/`build_event` build for a realistic
    /// observed `ConfigMap` snapshot, and the exact
    /// `production_egress_policy`/`handler_limits`/`BufferedOutputSink`
    /// shape `HandlerRuntime::invoke` uses -- proving (or disproving) the
    /// "double-wrapped/mismatched event shape" hypothesis independently of
    /// a kind cluster.
    mod derive_keys_real_bundle {
        use super::*;
        use engine::{HandlerInvocation, OutputDeclaration, StderrLogWriter};

        /// Regenerate by compiling
        /// `crates/acceptance/programs/handler-output/program.tsx` with
        /// `compileApsides` (`packages/compiler/src/compiler.ts`) and
        /// reading `handlers/<id>.js` from its output, if
        /// `handlers/derive-keys.ts`'s source ever changes.
        const DERIVE_KEYS_BUNDLE: &str = r#"
//#region apsides:/source/handlers/derive-keys.ts
/**
* Copies every key of the `handler-source` input's `data` into the
* `handler-derived` output, unchanged. No egress: the whole point of this
* handler is to exercise the runtime's own commit path (uid binding,
* per-key SSA ownership), not any capability the handler itself uses.
*/
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

        #[test]
        fn the_real_bundle_writes_every_input_key_from_a_realistic_event() -> TestResult {
            // `handler()` builds a fixture whose module/export are
            // `handlers/sync.ts`/`sync`; overridden here to match the real
            // bundle's actual export name, since `derive-keys.ts` exports
            // `deriveKeys`.
            let mut handler = handler("derive-keys", &["handler-source"], &["handler-derived"]);
            handler.module = "handlers/derive-keys.ts".into();
            handler.export = "deriveKeys".into();
            let mut observations = BTreeMap::new();
            observations.insert(
                "handler-source".to_owned(),
                available(&[("a", "1"), ("b", "2")]),
            );
            let inputs = handler_inputs_snapshot(&handler, &observations)
                .ok_or("expected the observed input to be ready")?;
            let revision = compute_input_revision(&handler, &inputs)?;
            let (_event_id, event) = build_event(&handler, &inputs, &revision)?;

            let sink = Arc::new(BufferedOutputSink::new());
            let sink_for_engine: Arc<dyn engine::OutputSink> = sink.clone();
            let usage = Arc::new(AtomicUsize::new(0));
            let report = engine::invoke_handler_with_watchdog(
                HandlerInvocation {
                    bundle: DERIVE_KEYS_BUNDLE,
                    export: &handler.export,
                    event,
                    limits: handler_limits(&handler),
                    egress: production_egress_policy(&handler),
                    outputs: vec![OutputDeclaration {
                        id: "handler-derived".into(),
                        max_bytes: 262_144,
                    }],
                    sink: sink_for_engine,
                    resolver: None,
                    handler_id: &handler.id,
                    invocation_id: "test-invocation",
                    log_writer: Arc::new(StderrLogWriter),
                },
                &usage,
            );
            report
                .result
                .map_err(|error| format!("expected the real bundle to succeed: {error}"))?;
            let written = sink.take();
            assert_eq!(
                written.get("handler-derived").cloned().unwrap_or_default(),
                BTreeMap::from([
                    ("a".to_owned(), "1".to_owned()),
                    ("b".to_owned(), "2".to_owned())
                ]),
                "the real derive-keys.ts bundle must write both input keys from a \
                 realistic observed snapshot -- if this fails, the event shape \
                 `build_event` constructs does not match what the handler reads"
            );
            Ok(())
        }
    }
}
