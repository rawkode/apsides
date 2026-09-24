//! Phase 5a spike gate for the Apsides handler engine (see
//! `/home/user/apsides-PLAN.md`, "Phase 5 -> 5a. Spike gate").
//!
//! This crate is deliberately small: it proves that a pinned `deno_core`
//! can be embedded with a capped `ArrayBuffer` allocator, a heap limit that
//! terminates the isolate (not the process) on approach, and restrictive V8
//! flags (`--jitless`, no `WebAssembly`, no `SharedArrayBuffer`) applied
//! before platform init — one op, one module, one isolate per call. It is
//! not the Phase 5b event-plane engine (no ops beyond the spike ping, no
//! fetch/output ops, no watchdog, no queueing).
//!
//! Kept separate from `runtime` because `runtime/src/lib.rs` carries
//! `#![forbid(unsafe_code)]`, which cannot be locally relaxed; see the
//! justification in `Cargo.toml`.

mod ops;
mod v8_alloc;

use std::rc::Rc;
use std::sync::Once;
use std::time::Duration;

use deno_core::v8;
use deno_core::{FsModuleLoader, JsRuntime, ModuleSpecifier, RuntimeOptions};
use thiserror::Error;

/// V8 flags applied before `JsRuntime::init_platform`/the first
/// `JsRuntime::new`, in addition to (added to, not replacing) the fixed
/// flags `deno_core::runtime::setup::v8_init` sets internally at the same
/// point. `--jitless` disables both the Ignition JIT tiers *and*, on a
/// non-`DrumBrake` V8 build (the default for the `denoland/rusty_v8`
/// prebuilt linux/darwin/windows archives; `DrumBrake` is the Wasm-interpreter
/// path used mainly for tvOS/iOS), it leaves `wasm_jitless` at its
/// `DEFINE_BOOL_READONLY(wasm_jitless, false, ...)` default, so V8's own
/// `wasm-js.cc`
/// (`expose_wasm = !v8_flags.jitless || v8_flags.wasm_jitless`) computes
/// `expose_wasm = false` and never installs `globalThis.WebAssembly`.
///
/// `--enable-sharedarraybuffer-per-context` is the *opposite* of what its
/// name suggests and was only discovered by running `tests/spike.rs`
/// (`wasm_and_shared_array_buffer_are_not_exposed` failed without it): per
/// `Genesis::InitializeGlobal_sharedarraybuffer` in `bootstrapper.cc`, the
/// guard is `if (v8_flags.enable_sharedarraybuffer_per_context) { return; }`
/// -- i.e. the flag being **false** (V8's own default) is what makes V8
/// install `SharedArrayBuffer` on the global object *unconditionally*, and
/// setting the flag **true** is what switches to the gated, per-context
/// callback-controlled path (`v8::Isolate::SetSharedArrayBufferConstructorEnabledCallback`,
/// which `deno_core` never calls, so with the callback unset `SharedArrayBuffer`
/// ends up disabled). The flag's own doc comment in V8's
/// `flag-definitions.h` reads the same way on close reading, but the
/// observable default behavior only matched inspection once this was
/// actually run.
const SPIKE_V8_FLAGS: &str = "--jitless --enable-sharedarraybuffer-per-context";

static PLATFORM_INIT: Once = Once::new();

/// Sets the spike's V8 flags and initializes the V8 platform, exactly once
/// per process.
///
/// Must run before the first `JsRuntime` is constructed anywhere in the
/// process; `run_spike_module` calls it, so callers that only use
/// `run_spike_module` never need to call this directly. Exposed for tests
/// that want to force platform init before doing anything else.
pub fn ensure_platform_initialized() {
    PLATFORM_INIT.call_once(|| {
        // Must happen before `JsRuntime::init_platform`/`v8::V8::initialize`;
        // `v8::V8::set_flags_from_string` is additive and may be called
        // multiple times as long as every call precedes
        // `v8::V8::initialize_platform`. `JsRuntime::init_platform` is the
        // first thing in `deno_core` that can trigger that, so this call
        // must come strictly first, which the `Once` above guarantees.
        v8::V8::set_flags_from_string(SPIKE_V8_FLAGS);
        JsRuntime::init_platform(None);
    });
}

/// Isolate creation limits for a single spike invocation.
#[derive(Debug, Clone, Copy)]
pub struct EngineLimits {
    /// Hard cap on outstanding V8 `ArrayBuffer` backing-store bytes for this
    /// isolate, enforced by the allocator in `v8_alloc`. A breach currently
    /// terminates the whole isolate rather than surfacing a catchable JS
    /// exception -- see the doc comment on
    /// `array_buffer_allocation_over_the_cap_terminates_the_isolate_without_aborting_the_process`
    /// in `tests/spike.rs` for why, and the Phase 5a report for the open
    /// design question this leaves for Phase 5b.
    pub max_array_buffer_bytes: usize,
    /// V8 heap size (young + old generation) at which the near-heap-limit
    /// callback fires and terminates the isolate.
    pub max_heap_bytes: usize,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("module load failed: {0}")]
    Load(#[source] deno_core::error::CoreError),
    #[error("module evaluation failed: {0}")]
    Evaluate(#[source] deno_core::error::CoreError),
    #[error("event loop failed: {0}")]
    EventLoop(#[source] deno_core::error::CoreError),
    #[error("module specifier was rejected: {0}")]
    Specifier(#[source] deno_core::url::ParseError),
}

/// Runs `module_code` as the main ES module of a fresh isolate built with
/// `limits`, registering the spike's one op extension.
///
/// Returns `Err` on any JS-side failure, including a heap-limit termination
/// — never aborts the process (that is the property this spike exists to
/// prove; see `tests/spike.rs`).
///
/// # Errors
///
/// Returns `Err` if the module specifier cannot be parsed (never, in
/// practice: it is a fixed literal), if `module_code` fails to load, if the
/// module fails to evaluate, or if the event loop reports a failure — which
/// includes a heap-limit or array-buffer-cap termination (see
/// `EngineLimits`'s field docs).
///
/// This function's returned future is not `Send`: `deno_core::JsRuntime`
/// (and the V8 isolate it owns) is deliberately single-threaded and is only
/// ever driven from the one current-thread tokio runtime that
/// `run_spike_module_blocking` builds for it, matching the "one fresh
/// isolate per invocation, never shared across threads" model this spike
/// exists to validate.
#[allow(
    clippy::future_not_send,
    reason = "deno_core::JsRuntime is deliberately !Send; see the doc comment above"
)]
pub async fn run_spike_module(
    limits: EngineLimits,
    module_code: &'static str,
) -> Result<(), EngineError> {
    ensure_platform_initialized();

    let (allocator, _usage) =
        v8_alloc::capped_array_buffer_allocator(limits.max_array_buffer_bytes);
    // `initial` is set equal to `max`, not 0: see the "initial=0" note in
    // the spike report / commit message. With `initial=0` V8 picks its own
    // (much smaller) starting ceiling and grows it heuristically, which in
    // `tests/spike.rs` produced a spurious near-heap-limit callback -- and
    // therefore isolate termination -- from a single small, unrelated
    // allocation, well before `max_heap_bytes` was anywhere close to being
    // reached. Fixing the ceiling removes that ambiguity for a one-shot,
    // short-lived handler isolate, at the cost of the (here irrelevant)
    // memory-saving benefit `heap_limits`' own docs describe for `initial=0`.
    let create_params = v8::CreateParams::default()
        .array_buffer_allocator(allocator)
        .heap_limits(limits.max_heap_bytes, limits.max_heap_bytes);

    let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(Rc::new(FsModuleLoader)),
        extensions: vec![ops::apsides_spike::init()],
        create_params: Some(create_params),
        ..Default::default()
    });

    // Captured before installing the near-heap-limit callback below, and
    // moved into it, so the callback can request termination of *this*
    // isolate from wherever V8 invokes it (documented as callable from a GC
    // pause on the isolate's own thread) without needing `&mut JsRuntime`,
    // which the callback signature does not provide.
    let isolate_handle = runtime.v8_isolate().thread_safe_handle();
    runtime.add_near_heap_limit_callback(move |current, _initial| {
        isolate_handle.terminate_execution();
        // Grow the limit so V8 can finish unwinding the terminated script
        // instead of immediately re-entering this callback and eventually
        // calling `V8::FatalProcessOutOfMemory` (an abort) before the
        // termination exception has had a chance to propagate out to
        // `run_event_loop`'s `Result`.
        current
            .saturating_add(current / 2)
            .max(current.saturating_add(1))
    });

    let specifier = spike_module_specifier()?;
    let module_id = runtime
        .load_main_es_module_from_code(&specifier, module_code)
        .await
        .map_err(EngineError::Load)?;
    let evaluated = runtime.mod_evaluate(module_id);
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .map_err(EngineError::EventLoop)?;
    evaluated.await.map_err(EngineError::Evaluate)
}

fn spike_module_specifier() -> Result<ModuleSpecifier, EngineError> {
    ModuleSpecifier::parse("file:///apsides-engine-spike/main.js").map_err(EngineError::Specifier)
}

/// Runs `run_spike_module` on a fresh single-threaded current-thread tokio
/// runtime and blocks on it, with a wall-clock watchdog.
///
/// This mirrors the "one fresh isolate per invocation" shape Phase 5b uses,
/// without any of its queueing/dispatch machinery.
///
/// # Errors
///
/// Returns `Err` if the tokio runtime fails to build, if `module_code` does
/// not finish within `timeout`, or if `run_spike_module` itself returns
/// `Err` (see its docs).
pub fn run_spike_module_blocking(
    limits: EngineLimits,
    module_code: &'static str,
    timeout: Duration,
) -> Result<(), SpikeRunError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(SpikeRunError::Runtime)?;
    rt.block_on(async move {
        tokio::time::timeout(timeout, run_spike_module(limits, module_code))
            .await
            .map_err(|_elapsed| SpikeRunError::Timeout)?
            .map_err(SpikeRunError::Engine)
    })
}

#[derive(Debug, Error)]
pub enum SpikeRunError {
    #[error("failed to build tokio runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("spike module timed out")]
    Timeout,
    #[error(transparent)]
    Engine(#[from] EngineError),
}
