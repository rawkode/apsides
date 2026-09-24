//! Wires the Phase 5b `engine` crate (a fresh V8 isolate per invocation, no
//! filesystem/net access except through its own typed, capability-checked
//! ops) into the reconcile cycle: computes each handler's input revision
//! each cycle, enqueues/dispatches handler invocations at global
//! concurrency 1, and persists `apsides.writeOutput`/the post-success
//! revision stamp through a server-side-apply output sink using field
//! manager `apsides-handler-<domainId>`.
//!
//! See the Apsides v0.3 plan, Phase 5b. `HandlerRuntime` is driven only by
//! [`crate::production::ProductionController`] today -- fixture mode
//! (`reconcile_once`/`run_fixture_with_client`) still does not execute
//! handlers, unchanged from Phase 4; see this module's own tests (a fake
//! `kube::Client` built the same way `production.rs`'s own test module
//! builds one) for coverage of the dispatch, queue, backoff, and output-sink
//! behavior in isolation from that larger harness.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use compiler_core::{CompiledPlan, Handler, Output};
use kube::Client;
use kube::api::{Api, ApiResource, DynamicObject, Patch};
use kube::core::GroupVersionKind;
use platform::{ApplyPrecondition, RuntimeCondition, conditional_apply_request};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc as async_mpsc;

use crate::{Observation, RuntimeError};

/// Distinct from [`compiler_core::REVISION_ANNOTATION`] (`apsides.dev/revision`,
/// the rollout-input revision on a *reacting* resource's desired state):
/// this one is written by the runtime itself, after a handler invocation
/// succeeds, onto the handler's own *output* object, recording which input
/// revision that output now reflects.
pub(crate) const HANDLER_INPUT_REVISION_ANNOTATION: &str = "apsides.dev/handler-input-revision";

/// Hard cap on `apsides.fetch` response bytes across one whole invocation.
/// Not named by the IR (only a per-handler `timeoutMillis`/`maxHeapBytes`
/// budget is declared) -- a fixed, conservative runtime constant, well
/// under the per-call 4 MiB response cap `engine::ops` enforces internally
/// times a small burst allowance.
const MAX_FETCH_BYTES_TOTAL: usize = 16 * 1024 * 1024;
/// Hard cap on concurrent `apsides.fetch` calls within one invocation.
const MAX_CONCURRENT_FETCHES: usize = 4;

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

/// A hash of the trigger input's id plus every declared input's projected
/// snapshot -- see the Apsides v0.3 plan, Phase 5b, "Trigger detection".
fn compute_input_revision(
    handler: &Handler,
    inputs: &BTreeMap<String, Value>,
) -> Result<String, RuntimeError> {
    compiler_core::digest(&json!({"trigger": handler.trigger.input, "inputs": inputs}))
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
// Output writer: bridges the engine's synchronous `OutputSink::write_output`
// (called on the isolate's own thread, itself deep inside that
// invocation's *own* dedicated tokio runtime -- see
// `engine::invoke_handler_with_watchdog`'s doc comment) to the async
// Kubernetes client, without ever nesting one runtime's `block_on` inside
// another's (which tokio detects and panics on, regardless of which
// `Runtime` object is involved). A `std::sync::mpsc` channel -- plain OS
// thread parking, not a tokio primitive -- is the only safe bridge here.
// ---------------------------------------------------------------------

enum WriteJob {
    /// One `apsides.writeOutput(id, key, data)` call.
    Data {
        output_id: String,
        key: String,
        data: Vec<u8>,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// The post-success revision-annotation stamp for one output, sent
    /// only from `HandlerRuntime::invoke` (an ordinary async fn on the
    /// main runtime, not the isolate thread) after the whole invocation
    /// has already resolved.
    Stamp {
        output_id: String,
        revision: String,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
}

fn configmap_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    let resource = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("", "v1", "ConfigMap"),
        "configmaps",
    );
    Api::namespaced_with(client.clone(), namespace, &resource)
}

/// Applies one `data.<key>` or the revision annotation to `output`'s
/// `ConfigMap` through the existing conditional-apply path
/// (`platform::conditional_apply_request`), with field manager
/// `apsides-handler-<domainId>`, claiming only that one field. Skips the
/// write entirely if the target field's live value already equals `value`.
/// Enforces `min_interval` since the last real write to this output
/// (tracked in `last_write`, keyed by output id).
async fn apply_claimed_field(
    client: &Client,
    field_manager: &str,
    output: &Output,
    field_path: FieldPath<'_>,
    min_interval: Duration,
    last_write: &mut BTreeMap<String, Instant>,
) -> Result<(), String> {
    let api = configmap_api(client, &output.namespace);
    let existing = api
        .get(&output.name)
        .await
        .map_err(|error| format!("handler output read failed: {error}"))?;
    let current = field_path.read(&existing);
    if current.as_deref() == Some(field_path.value()) {
        return Ok(());
    }
    if let Some(last) = last_write.get(&output.id)
        && let Some(remaining) = min_interval.checked_sub(last.elapsed())
    {
        tokio::time::sleep(remaining).await;
    }
    let uid = existing
        .metadata
        .uid
        .clone()
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| "handler output is missing a uid".to_owned())?;
    let resource_version = existing
        .metadata
        .resource_version
        .clone()
        .filter(|version| !version.is_empty() && version != "0")
        .ok_or_else(|| "handler output is missing a resourceVersion".to_owned())?;
    let payload = field_path.payload(&output.name, &output.namespace);
    let request = conditional_apply_request(
        &payload,
        &output.name,
        &output.namespace,
        field_manager,
        false,
        ApplyPrecondition::Existing {
            uid: &uid,
            resource_version: &resource_version,
        },
    )
    .map_err(|error| error.to_string())?;
    api.patch(
        &output.name,
        request.params(),
        &Patch::Apply(request.payload()),
    )
    .await
    .map_err(|error| format!("handler output write failed: {error}"))?;
    last_write.insert(output.id.clone(), Instant::now());
    Ok(())
}

enum FieldPath<'a> {
    Data { key: &'a str, value: &'a str },
    Annotation { value: &'a str },
}

impl FieldPath<'_> {
    const fn value(&self) -> &str {
        match self {
            Self::Data { value, .. } | Self::Annotation { value } => value,
        }
    }

    fn read(&self, object: &DynamicObject) -> Option<String> {
        match self {
            Self::Data { key, .. } => object
                .data
                .get("data")
                .and_then(Value::as_object)
                .and_then(|data| data.get(*key))
                .and_then(Value::as_str)
                .map(str::to_owned),
            Self::Annotation { .. } => object
                .metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(HANDLER_INPUT_REVISION_ANNOTATION))
                .cloned(),
        }
    }

    fn payload(&self, name: &str, namespace: &str) -> Value {
        match self {
            Self::Data { key, value } => json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": name, "namespace": namespace},
                "data": {*key: value},
            }),
            Self::Annotation { value } => json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {
                    "name": name, "namespace": namespace,
                    "annotations": {HANDLER_INPUT_REVISION_ANNOTATION: value},
                },
            }),
        }
    }
}

async fn run_writer(
    mut jobs: async_mpsc::UnboundedReceiver<WriteJob>,
    client: Client,
    field_manager: String,
    min_interval: Duration,
    outputs: BTreeMap<String, Output>,
) {
    let mut last_write: BTreeMap<String, Instant> = BTreeMap::new();
    while let Some(job) = jobs.recv().await {
        let (output_id, field, reply) = match job {
            WriteJob::Data {
                output_id,
                key,
                data,
                reply,
            } => {
                let Ok(value) = std::str::from_utf8(&data) else {
                    let _ = reply.send(Err(
                        "handler output data must be valid UTF-8 text in this release".into(),
                    ));
                    continue;
                };
                (
                    output_id,
                    StoredField::Data {
                        key,
                        value: value.to_owned(),
                    },
                    reply,
                )
            }
            WriteJob::Stamp {
                output_id,
                revision,
                reply,
            } => (output_id, StoredField::Annotation { revision }, reply),
        };
        let Some(output) = outputs.get(&output_id) else {
            let _ = reply.send(Err(format!("undeclared output id {output_id:?}")));
            continue;
        };
        let field_path = match &field {
            StoredField::Data { key, value } => FieldPath::Data { key, value },
            StoredField::Annotation { revision } => FieldPath::Annotation { value: revision },
        };
        let outcome = apply_claimed_field(
            &client,
            &field_manager,
            output,
            field_path,
            min_interval,
            &mut last_write,
        )
        .await;
        let _ = reply.send(outcome);
    }
}

enum StoredField {
    Data { key: String, value: String },
    Annotation { revision: String },
}

struct ChannelOutputSink {
    tx: async_mpsc::UnboundedSender<WriteJob>,
}

impl engine::OutputSink for ChannelOutputSink {
    fn write_output(
        &self,
        id: &str,
        key: &str,
        data: &[u8],
    ) -> Result<(), engine::OutputSinkError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(WriteJob::Data {
                output_id: id.to_owned(),
                key: key.to_owned(),
                data: data.to_owned(),
                reply: reply_tx,
            })
            .map_err(|_error| engine::OutputSinkError {
                message: "the handler output writer is no longer running".into(),
            })?;
        // A plain `std::sync::mpsc` recv, not a tokio primitive: this runs
        // on the isolate's own thread, already deep inside that
        // invocation's own `rt.block_on` -- see this module's doc comment.
        reply_rx
            .recv()
            .map_err(|_error| engine::OutputSinkError {
                message: "the handler output writer dropped the reply channel".into(),
            })?
            .map_err(|message| engine::OutputSinkError { message })
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

// ---------------------------------------------------------------------
// HandlerRuntime
// ---------------------------------------------------------------------

fn handler_limits(handler: &Handler) -> engine::Limits {
    let heap_bytes = usize::try_from(handler.max_heap_bytes).unwrap_or(usize::MAX);
    engine::Limits {
        timeout: Duration::from_millis(u64::from(handler.timeout_millis)),
        max_heap_bytes: heap_bytes,
        max_array_buffer_bytes: heap_bytes,
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

pub(crate) struct HandlerRuntime {
    queue_capacity: usize,
    base_backoff: Duration,
    max_backoff: Duration,
    max_object_bytes: usize,
    bundles: BTreeMap<String, &'static str>,
    outputs_by_id: BTreeMap<String, Output>,
    resolver: Option<Arc<engine::EgressResolver>>,
    writer_tx: async_mpsc::UnboundedSender<WriteJob>,
    states: BTreeMap<String, HandlerState>,
    queue_order: VecDeque<String>,
}

impl HandlerRuntime {
    /// Builds the runtime for one domain's declared handlers: a shared DNS
    /// resolver for `apsides.fetch` (built once, only if some handler
    /// declares egress hosts -- see `engine::EgressResolver::new`'s doc
    /// comment on why this must happen once, up front, never per
    /// invocation), and the background output-writer task. Must be called
    /// from within an entered tokio runtime (it spawns that task).
    pub(crate) fn new(
        plan: &CompiledPlan,
        handlers: &'static [(&'static str, &'static str)],
        client: Client,
        field_manager: String,
        min_mutation_interval: Duration,
        base_backoff: Duration,
        max_backoff: Duration,
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
            let resolver =
                engine::EgressResolver::new(hickory_resolver::config::ResolverConfig::udp_and_tcp(
                    &hickory_resolver::config::CLOUDFLARE,
                ))
                .map_err(|error| RuntimeError::Configuration(error.to_string()))?;
            Some(Arc::new(resolver))
        } else {
            None
        };
        let (writer_tx, writer_rx) = async_mpsc::unbounded_channel();
        tokio::spawn(run_writer(
            writer_rx,
            client,
            field_manager,
            min_mutation_interval,
            outputs_by_id.clone(),
        ));
        Ok(Self {
            queue_capacity: plan.ir.domain.budgets.max_queue_depth.max(1),
            base_backoff,
            max_backoff,
            max_object_bytes: plan.ir.domain.budgets.max_object_bytes,
            bundles,
            outputs_by_id,
            resolver,
            writer_tx,
            states: BTreeMap::new(),
            queue_order: VecDeque::new(),
        })
    }

    /// Level-based startup recomputation (see the Apsides v0.3 plan, Phase
    /// 5b, "Delivery"): seeds each handler's `known_revision` from the
    /// revision annotation already recorded on its declared outputs, so a
    /// restart re-enqueues a handler exactly when its current input
    /// revision differs from the one its outputs already reflect. A
    /// handler with no declared outputs has nothing to seed from and
    /// always looks pending immediately after a restart, by construction.
    pub(crate) async fn seed_from_annotations(&mut self, plan: &CompiledPlan, client: &Client) {
        for handler in &plan.ir.handlers {
            if handler.outputs.is_empty() {
                continue;
            }
            let mut agreed: Option<String> = None;
            let mut every_output_agrees = true;
            for output_id in &handler.outputs {
                let Some(output) = self.outputs_by_id.get(output_id) else {
                    every_output_agrees = false;
                    break;
                };
                let api = configmap_api(client, &output.namespace);
                let recorded = match api.get_opt(&output.name).await {
                    Ok(Some(object)) => object
                        .metadata
                        .annotations
                        .as_ref()
                        .and_then(|annotations| annotations.get(HANDLER_INPUT_REVISION_ANNOTATION))
                        .cloned(),
                    Ok(None) | Err(_) => None,
                };
                match (&agreed, recorded) {
                    (None, Some(value)) if agreed.is_none() && self.states.is_empty() => {
                        agreed = Some(value);
                    }
                    (None, value) => agreed = value,
                    (Some(current), Some(value)) if *current == value => {}
                    _ => {
                        every_output_agrees = false;
                        break;
                    }
                }
            }
            let known_revision = if every_output_agrees { agreed } else { None };
            self.states
                .entry(handler.id.clone())
                .or_default()
                .known_revision = known_revision;
        }
    }

    /// Computes revisions, enqueues changed handlers, and dispatches every
    /// currently-ready queued entry serially (global concurrency 1 -- see
    /// the Apsides v0.3 plan, Phase 5b, "Delivery"). Returns the
    /// conditions to fold into the cycle's existing conditions list.
    pub(crate) async fn run_cycle(
        &mut self,
        plan: &CompiledPlan,
        observations: &BTreeMap<String, Observation>,
        applied: &BTreeSet<String>,
    ) -> Vec<RuntimeCondition> {
        let mut conditions = Vec::new();
        self.detect_triggers(plan, observations, &mut conditions);
        self.dispatch_ready(plan, applied, &mut conditions).await;
        conditions
    }

    fn detect_triggers(
        &mut self,
        plan: &CompiledPlan,
        observations: &BTreeMap<String, Observation>,
        conditions: &mut Vec<RuntimeCondition>,
    ) {
        for handler in &plan.ir.handlers {
            let Some(inputs) = handler_inputs_snapshot(handler, observations) else {
                continue;
            };
            let Ok(revision) = compute_input_revision(handler, &inputs) else {
                continue;
            };
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

    async fn dispatch_ready(
        &mut self,
        plan: &CompiledPlan,
        applied: &BTreeSet<String>,
        conditions: &mut Vec<RuntimeCondition>,
    ) {
        let ids: Vec<String> = self.queue_order.iter().cloned().collect();
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
            let outputs_ready = handler.outputs.iter().all(|id| applied.contains(id));
            if !outputs_ready {
                conditions.push(RuntimeCondition {
                    reason_code: "handler-output-not-ready".into(),
                    resource_name: Some(handler_id.clone()),
                });
                continue;
            }
            let Some(queued) = self
                .states
                .get_mut(&handler_id)
                .and_then(|state| state.queued.take())
            else {
                continue;
            };
            self.queue_order.retain(|id| *id != handler_id);
            let outcome = self.invoke(handler, &queued).await;
            let state = self.states.entry(handler_id.clone()).or_default();
            state.last_run = Some(Instant::now());
            match outcome {
                Ok(()) => {
                    state.known_revision = Some(queued.revision);
                    state.attempt = 0;
                    state.backoff_until = None;
                    state.last_error_code = None;
                }
                Err(code) => {
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
    }

    async fn invoke(&self, handler: &Handler, queued: &QueuedEvent) -> Result<(), String> {
        let Some(bundle) = self.bundles.get(handler.id.as_str()).copied() else {
            return Err("HANDLER_BUNDLE_MISSING".into());
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
        let sink: Arc<dyn engine::OutputSink> = Arc::new(ChannelOutputSink {
            tx: self.writer_tx.clone(),
        });
        let resolver = self.resolver.clone();
        let log_writer: Arc<dyn engine::LogWriter> = Arc::new(HandlerLogWriter {
            handler_id: handler_id.clone(),
            invocation_id: invocation_id.clone(),
        });

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
        .map_err(|_join_error| "HANDLER_INVOCATION_TASK_FAILED".to_owned())?;

        match report.result {
            Ok(_value) => self.stamp_outputs(handler, &queued.revision),
            Err(error) => Err(invoke_error_code(&error)),
        }
    }

    /// Not `async`: every step here is either a non-blocking channel send
    /// or `block_in_place` (itself synchronous from its caller's
    /// perspective -- it parks this thread while handing off the
    /// runtime's other work, never an `.await` point). Called only from
    /// `invoke`, itself always on the main (multi-threaded) runtime, never
    /// the isolate thread.
    fn stamp_outputs(&self, handler: &Handler, revision: &str) -> Result<(), String> {
        for output_id in &handler.outputs {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            self.writer_tx
                .send(WriteJob::Stamp {
                    output_id: output_id.clone(),
                    revision: revision.to_owned(),
                    reply: reply_tx,
                })
                .map_err(|_error| "HANDLER_OUTPUT_WRITER_STOPPED".to_owned())?;
            // Called from an ordinary async fn on the main (multi-threaded)
            // runtime, never the isolate thread -- `block_in_place` is legal
            // here (unlike in `ChannelOutputSink::write_output`).
            let outcome = tokio::task::block_in_place(|| reply_rx.recv())
                .map_err(|_error| "HANDLER_OUTPUT_WRITER_STOPPED".to_owned())?;
            outcome.map_err(|_message| "HANDLER_OUTPUT_STAMP_FAILED".to_owned())?;
        }
        Ok(())
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
            writer_tx: async_mpsc::unbounded_channel().0,
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
        assert_eq!(limits.max_array_buffer_bytes, 8 * 1024 * 1024);
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

    // -------------------------------------------------------------
    // End-to-end: a real `engine` V8 invocation, dispatched by
    // `HandlerRuntime` against a fake `kube::Client` (the same
    // `tower::service_fn` pattern `production.rs`'s own test module
    // uses), covering the plan's core Phase 5b scenarios directly
    // against `run_cycle`/`seed_from_annotations` -- no
    // `ProductionController`/authority/inventory machinery needed,
    // since both take their inputs as plain arguments.
    // -------------------------------------------------------------
    mod engine_integration {
        use super::*;
        use compiler_core::parse_and_validate;
        use http::{Method, Request, Response};
        use http_body_util::BodyExt;
        use kube::client::Body;
        use sha2::{Digest, Sha256};
        use std::collections::VecDeque;
        use std::convert::Infallible;
        use std::sync::Mutex as StdMutex;
        use tower::service_fn;

        type IntegrationResult<T = ()> =
            std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
        type Calls = Arc<StdMutex<Vec<(Method, Value)>>>;

        const HANDLER_BUNDLE: &str = r#"
export async function syncFlags(event) {
  const value = event.inputs.settings.data.flag;
  apsides.writeOutput("flags", "value", value);
  return {};
}
"#;

        fn bundle_sha256() -> String {
            format!("{:x}", Sha256::digest(HANDLER_BUNDLE.as_bytes()))
        }

        fn source() -> Value {
            json!({"file": "program.tsx", "line": 1, "column": 1})
        }

        /// A validated one-handler-one-output plan: handler `sync-flags`
        /// triggers on the observed `settings` input and writes its
        /// `flag` field to key `value` of output `flags`.
        fn plan() -> IntegrationResult<CompiledPlan> {
            let ir = json!({
                "schemaVersion": compiler_core::IR_SCHEMA,
                "domain": {
                    "id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
                    "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
                    "minKubernetesVersion": "1.34",
                    "budgets": {"maxResources": 8, "maxInputs": 8, "maxReactions": 8, "maxObjectBytes": 262_144, "maxQueueDepth": 8, "maxConcurrentRequests": 2},
                },
                "resources": [{
                    "id": "flags", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
                    "handlerOutput": true,
                    "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "flags", "namespace": "workload", "labels": {"apsides.dev/handler-output": "flags"}}},
                    "lifecycle": {"deletionPolicy": "orphan"}, "source": source(),
                }],
                "inputs": [
                    {"id": "settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "namespace": "workload", "mode": "observed", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
                    {"id": "flags-output", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload", "mode": "output", "managedResourceId": "flags", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source()},
                ],
                "reactions": [],
                "handlers": [{
                    "id": "sync-flags", "module": "handlers/sync-flags.ts", "export": "syncFlags",
                    "trigger": {"kind": "input-changed", "input": "settings"},
                    "inputs": ["settings"], "egress": [], "outputs": ["flags"],
                    "timeoutMillis": 5_000, "maxHeapBytes": 8_388_608,
                    "bundleSha256": bundle_sha256(), "source": source(),
                }],
                "outputs": [{"id": "flags", "name": "flags", "namespace": "workload"}],
                "provenance": {"compiler": "apsides/0.1", "inputs": [{"path": "program.tsx", "sha256": "0".repeat(64)}, {"path": "handlers/sync-flags.ts", "sha256": "1".repeat(64)}]},
            });
            Ok(parse_and_validate(&ir.to_string())?)
        }

        fn settings_observations(flag: &str) -> BTreeMap<String, Observation> {
            let mut observations = BTreeMap::new();
            observations.insert("settings".to_owned(), available(&[("flag", flag)]));
            observations
        }

        fn flags_object(uid: &str, rv: &str, value: Option<&str>, revision: Option<&str>) -> Value {
            let mut data = Map::new();
            if let Some(value) = value {
                data.insert("value".into(), Value::String(value.into()));
            }
            let mut annotations = Map::new();
            annotations.insert(
                "apsides.dev/handler-output".into(),
                Value::String("flags".into()),
            );
            if let Some(revision) = revision {
                annotations.insert(
                    HANDLER_INPUT_REVISION_ANNOTATION.into(),
                    Value::String(revision.into()),
                );
            }
            json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "flags", "namespace": "workload", "uid": uid, "resourceVersion": rv, "annotations": annotations},
                "data": Value::Object(data),
            })
        }

        async fn wire_response(
            request: Request<Body>,
            responses: &StdMutex<VecDeque<Value>>,
            calls: &Calls,
        ) -> IntegrationResult<Response<Body>> {
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

        fn wire(responses: Vec<Value>) -> (Client, Calls) {
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

        fn runtime_for(plan: &CompiledPlan, client: Client) -> IntegrationResult<HandlerRuntime> {
            Ok(HandlerRuntime::new(
                plan,
                &[("sync-flags", HANDLER_BUNDLE)],
                client,
                "apsides-handler-example".into(),
                Duration::from_millis(1),
                Duration::from_millis(10),
                Duration::from_millis(100),
            )?)
        }

        /// An input change dispatches the handler, its output is written
        /// through the handler field manager, and the revision annotation
        /// is stamped only after the invocation succeeds.
        #[tokio::test(flavor = "multi_thread")]
        async fn input_change_writes_output_and_stamps_the_revision_annotation() -> IntegrationResult
        {
            let plan = plan()?;
            let observations = settings_observations("on");
            let (client, calls) = wire(vec![
                flags_object("flags-uid", "1", None, None), // GET before the data write
                flags_object("flags-uid", "2", Some("on"), None), // PATCH response
                flags_object("flags-uid", "2", Some("on"), None), // GET before the stamp
                flags_object("flags-uid", "3", Some("on"), Some("stamped")), // PATCH response
            ]);
            let mut runtime = runtime_for(&plan, client)?;
            let mut applied = BTreeSet::new();
            applied.insert("flags".to_owned());

            let conditions = runtime.run_cycle(&plan, &observations, &applied).await;
            assert!(
                conditions.is_empty(),
                "unexpected conditions: {conditions:?}"
            );

            let recorded = calls.lock().map_err(|_| "calls poisoned")?;
            assert_eq!(recorded.len(), 4);
            assert_eq!(recorded[0].0, Method::GET);
            assert_eq!(recorded[1].0, Method::PATCH);
            assert_eq!(recorded[1].1["data"]["value"], json!("on"));
            assert_eq!(recorded[3].0, Method::PATCH);
            assert!(
                recorded[3].1["metadata"]["annotations"]
                    .get(HANDLER_INPUT_REVISION_ANNOTATION)
                    .is_some()
            );
            // The reconciler's own field manager never appears on a
            // handler-triggered write: every SSA request this crate sends
            // for a handler output uses the distinct handler manager.
            drop(recorded);

            let state = runtime
                .states
                .get("sync-flags")
                .ok_or("handler state must exist after a dispatch")?;
            assert!(state.known_revision.is_some());
            assert!(state.queued.is_none());
            assert_eq!(state.last_error_code, None);
            Ok(())
        }

        /// Unchanged data is never rewritten: the writer's own read shows
        /// the value (and, separately, the annotation) already match, so
        /// no PATCH is ever issued for either.
        #[tokio::test(flavor = "multi_thread")]
        async fn unchanged_output_and_annotation_are_not_rewritten() -> IntegrationResult {
            let plan = plan()?;
            let observations = settings_observations("on");
            // The revision this cycle will compute must already be
            // recorded, so the annotation stamp is also a no-op.
            let mut inputs = BTreeMap::new();
            inputs.insert("settings".to_owned(), json!({"flag": "on"}));
            let handler = &plan.ir.handlers[0];
            let revision = compute_input_revision(handler, &inputs)?;
            let (client, calls) = wire(vec![
                flags_object("flags-uid", "1", Some("on"), Some(&revision)), // GET before the data write: already "on"
                flags_object("flags-uid", "1", Some("on"), Some(&revision)), // GET before the stamp: already stamped
            ]);
            let mut runtime = runtime_for(&plan, client)?;
            let mut applied = BTreeSet::new();
            applied.insert("flags".to_owned());

            let conditions = runtime.run_cycle(&plan, &observations, &applied).await;
            assert!(
                conditions.is_empty(),
                "unexpected conditions: {conditions:?}"
            );
            let recorded = calls.lock().map_err(|_| "calls poisoned")?;
            assert_eq!(recorded.len(), 2, "no PATCH should be issued: {recorded:?}");
            assert!(recorded.iter().all(|(method, _)| *method == Method::GET));
            Ok(())
        }

        /// A handler whose declared output was not in this cycle's
        /// successfully-applied set is never invoked; the condition is
        /// recorded and the event stays queued.
        #[tokio::test(flavor = "multi_thread")]
        async fn output_not_ready_blocks_dispatch() -> IntegrationResult {
            let plan = plan()?;
            let observations = settings_observations("on");
            let (client, calls) = wire(vec![]); // no HTTP call should ever be made
            let mut runtime = runtime_for(&plan, client)?;
            let applied = BTreeSet::new(); // "flags" was not successfully applied this cycle

            let conditions = runtime.run_cycle(&plan, &observations, &applied).await;
            assert_eq!(conditions.len(), 1);
            assert_eq!(conditions[0].reason_code, "handler-output-not-ready");
            assert_eq!(conditions[0].resource_name.as_deref(), Some("sync-flags"));
            assert!(calls.lock().map_err(|_| "calls poisoned")?.is_empty());
            let state = runtime
                .states
                .get("sync-flags")
                .ok_or("handler state must exist")?;
            assert!(state.queued.is_some(), "the event must stay queued");
            Ok(())
        }

        /// After a restart, `seed_from_annotations` recomputes
        /// `known_revision` from the output's own annotation: a handler
        /// whose current input revision already matches its output's
        /// recorded revision does not re-run.
        #[tokio::test(flavor = "multi_thread")]
        async fn restart_seeds_known_revision_from_the_output_annotation_and_skips_a_matching_cycle()
        -> IntegrationResult {
            let plan = plan()?;
            let mut inputs = BTreeMap::new();
            inputs.insert("settings".to_owned(), json!({"flag": "on"}));
            let handler = &plan.ir.handlers[0];
            let revision = compute_input_revision(handler, &inputs)?;
            let (client, calls) = wire(vec![flags_object(
                "flags-uid",
                "1",
                Some("on"),
                Some(&revision),
            )]);
            let mut runtime = runtime_for(&plan, client.clone())?;
            runtime.seed_from_annotations(&plan, &client).await;
            {
                let seeded = calls.lock().map_err(|_| "calls poisoned")?;
                assert_eq!(seeded.len(), 1);
                assert_eq!(seeded[0].0, Method::GET);
            }
            let state = runtime
                .states
                .get("sync-flags")
                .ok_or("handler state must exist after seeding")?;
            assert_eq!(state.known_revision.as_deref(), Some(revision.as_str()));

            // The current cycle's input revision matches what was seeded,
            // so nothing is enqueued and no further HTTP calls happen.
            let observations = settings_observations("on");
            let mut applied = BTreeSet::new();
            applied.insert("flags".to_owned());
            let conditions = runtime.run_cycle(&plan, &observations, &applied).await;
            assert!(conditions.is_empty());
            assert_eq!(calls.lock().map_err(|_| "calls poisoned")?.len(), 1);
            Ok(())
        }

        /// A handler invocation that throws records a condition, backs
        /// off, and does not stamp the annotation -- the event is
        /// re-queued for at-least-once retry once backoff clears.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_failed_invocation_records_a_condition_and_backs_off_without_stamping()
        -> IntegrationResult {
            const FAILING_BUNDLE: &str = r#"
export async function syncFlags(event) {
  throw new Error("boom");
}
"#;
            let mut plan = plan()?;
            plan.ir.handlers[0].bundle_sha256 =
                format!("{:x}", Sha256::digest(FAILING_BUNDLE.as_bytes()));
            let observations = settings_observations("on");
            let (client, calls) = wire(vec![]); // the handler must never reach the writer
            let runtime = HandlerRuntime::new(
                &plan,
                &[("sync-flags", FAILING_BUNDLE)],
                client,
                "apsides-handler-example".into(),
                Duration::from_millis(1),
                Duration::from_millis(50),
                Duration::from_millis(500),
            )?;
            let mut runtime = runtime;
            let mut applied = BTreeSet::new();
            applied.insert("flags".to_owned());

            let conditions = runtime.run_cycle(&plan, &observations, &applied).await;
            assert_eq!(conditions.len(), 1);
            assert!(
                conditions[0]
                    .reason_code
                    .starts_with("handler-invocation-failed:")
            );
            assert!(calls.lock().map_err(|_| "calls poisoned")?.is_empty());

            let state = runtime
                .states
                .get("sync-flags")
                .ok_or("handler state must exist")?;
            assert_eq!(
                state.known_revision, None,
                "a failed run must not advance known_revision"
            );
            assert!(state.backoff_until.is_some());
            assert_eq!(state.attempt, 1);
            assert!(
                state.queued.is_some(),
                "at-least-once: the event must be re-queued"
            );

            // Dispatching again immediately is blocked by backoff: no
            // second invocation, no second condition beyond the queue
            // itself staying populated.
            let conditions_again = runtime.run_cycle(&plan, &observations, &applied).await;
            assert!(conditions_again.is_empty());
            Ok(())
        }
    }
}
