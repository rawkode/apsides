//! Phase 5a spike gate for the Apsides handler engine (see the Apsides v0.3
//! plan, "Phase 5 -> 5a. Spike gate").
//!
//! This crate is deliberately small: it proves that a pinned `deno_core`
//! can be embedded with a capped `ArrayBuffer` allocator, a heap limit that
//! requests isolate termination on approach, restrictive V8 flags
//! (`--jitless`, no `WebAssembly`, no `SharedArrayBuffer`) applied before
//! platform init, a JS-level bootstrap guard that closes the resizable-
//! `ArrayBuffer` gap those caps leave open (including through `deno_core`'s
//! own `Deno.core.deserialize`/`structuredClone`, which reach V8's
//! structured-clone deserializer directly and never call the JS-visible
//! `ArrayBuffer` constructor at all — see [`BOOTSTRAP_GUARD_SCRIPT`]), and a
//! module loader with no filesystem access — one op, one in-memory module,
//! one isolate per call. It is not the Phase 5b event-plane engine (no ops
//! beyond the spike ping, no fetch/output ops, no queueing).
//!
//! Kept separate from `runtime` because `runtime/src/lib.rs` carries
//! `#![forbid(unsafe_code)]`, which cannot be locally relaxed; see the
//! justification in `Cargo.toml`.
//!
//! [`prepare_isolate`] is the *only* place in this crate that constructs a
//! `JsRuntime`, and it always calls [`ensure_platform_initialized`] first —
//! so there is no path through this crate that can build an isolate without
//! the V8 flags, capped allocator, in-memory-only module loader, and
//! bootstrap guard script all being applied.

mod ops;
mod v8_alloc;

use std::rc::Rc;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use deno_core::v8;
use deno_core::{JsRuntime, ModuleSpecifier, RuntimeOptions, StaticModuleLoader};
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
/// setting the flag **true** (what this constant does) is what switches to
/// the gated, per-context callback-controlled path
/// (`v8::Isolate::SetSharedArrayBufferConstructorEnabledCallback`, which
/// `deno_core` never calls, so with the callback unset `SharedArrayBuffer`
/// ends up disabled). The flag's own doc comment in V8's
/// `flag-definitions.h` reads the same way on close reading, but the
/// observable default behavior only matched inspection once this was
/// actually run.
const SPIKE_V8_FLAGS: &str = "--jitless --enable-sharedarraybuffer-per-context";

/// Runs once, synchronously, in every isolate this crate builds (see
/// `prepare_isolate`), before any user module code. Two layers, found
/// necessary by two rounds of review:
///
/// **Layer 1 -- hide `deno_core`'s own internals from user code.** Captures
/// exactly the one op this engine's own curated `globalThis.__apsidesOps`
/// surface needs, then deletes `globalThis.Deno` and `globalThis.__bootstrap`
/// entirely (a pattern `deno_core` itself anticipates and supports -- see
/// `01_core.js`'s comment on the "harness" that "deletes
/// `globalThis.__bootstrap`"). This is what actually closes the review's
/// found bypasses through `Deno.core.deserialize`/`Deno.core.structuredClone`
/// (and, incidentally, everything else under `Deno.core`, e.g.
/// `Deno.core.evalContext`): those reach V8's structured-clone deserializer,
/// which builds `ArrayBuffer`s (including resizable ones, from a suitable
/// serialized tag) directly in C++, never through the JS-visible
/// `ArrayBuffer` constructor Layer 2 guards -- so Layer 2 alone did not
/// close them; they were only reachable at all via `Deno.core`, which no
/// longer exists once this runs.
///
/// **Layer 2 -- make growing *any* `ArrayBuffer`/`SharedArrayBuffer`
/// impossible, regardless of how it was constructed.** The capped allocator
/// in `v8_alloc.rs` only governs ordinary, fixed-size `ArrayBuffer`
/// allocations: a *resizable* one (`new ArrayBuffer(0, {maxByteLength: N})`,
/// then `.resize(N)`) is backed by memory V8 reserves/commits through its
/// own page allocator, never calling into the custom
/// `v8::ArrayBuffer::Allocator` vtable at all. There is no V8 build flag to
/// disable resizable `ArrayBuffer`s in this pinned version (checked: no
/// `resizable`/`growable`/`max_byte_length` flag exists in
/// `v8-150.4.0/src/flags/flag-definitions.h`). This layer:
///   - Replaces the constructor with one that **never reads or forwards a
///     second (`options`) argument at all** (found in review: an earlier
///     version that read `options.maxByteLength` once and rejected it if
///     present was a TOCTOU bug -- a getter on `options.maxByteLength` that
///     returns `undefined` on that one read but a real value if the native
///     constructor were to read it again could have smuggled a resizable
///     buffer through; never forwarding `options` at all removes that class
///     of bug structurally, since the native constructor is now never
///     invoked with anything that could contain `maxByteLength`).
///   - Also patches `NativeCtor.prototype.constructor`, not just the
///     `globalThis` binding: patching only the global name would still let
///     user code recover the native, unguarded constructor via any existing
///     instance's prototype chain (e.g. `new Uint8Array(1).buffer.constructor`),
///     since ordinary `TypedArray` construction allocates its backing
///     `ArrayBuffer` through an internal V8 builtin that never consults the
///     JS-visible global binding.
///   - Most importantly, replaces `NativeCtor.prototype.resize` with a
///     function that **always throws, unconditionally, regardless of the
///     receiver**, and `.transfer` with one that throws when `this.resizable`
///     (`.transfer()` on a resizable buffer produces another resizable
///     buffer, preserving resizability; `.transferToFixedLength()` is left
///     alone, since it always produces a fixed buffer). Every `ArrayBuffer`
///     instance in one realm, however it was constructed -- via this guarded
///     constructor, via the raw native one if somehow still reachable, or
///     via `Deno.core.deserialize` before Layer 1 ran -- shares this one
///     `%ArrayBuffer.prototype%` object, so this is the layer that actually
///     closes every construction route uniformly, including ones this
///     review (or a future one) did not enumerate.
///   - Sets `Guarded`'s own `[[Prototype]]` to `Native` (`Object.setPrototypeOf`),
///     so static methods/properties users depend on --
///     `ArrayBuffer.isView`, `ArrayBuffer[Symbol.species]` -- still resolve
///     through the guarded constructor (found in review: without this,
///     `Guarded.isView` would be `undefined`, since a plain function's own
///     `[[Prototype]]` is `Function.prototype`, not `Native`).
///
/// See `tests/spike.rs` for the regression tests: one per bypass route this
/// and the prior review round found, plus the static-preservation and
/// Deno-unreachability checks.
const BOOTSTRAP_GUARD_SCRIPT: &str = r#""use strict";
(() => {
  // Layer 1: hide deno_core's own internals. Captures only the one op
  // function this spike needs before deleting every other route to it.
  const opPing = globalThis.Deno.core.ops.op_apsides_spike_ping;
  globalThis.__apsidesOps = Object.freeze({
    ping: () => opPing(),
  });
  delete globalThis.Deno;
  delete globalThis.__bootstrap;

  // Layer 2: make growing any ArrayBuffer/SharedArrayBuffer impossible,
  // regardless of construction route.
  function guardResizable(name) {
    const Native = globalThis[name];
    if (typeof Native !== "function") {
      return;
    }
    function Guarded(length) {
      if (new.target === undefined) {
        throw new TypeError(`Constructor ${name} requires 'new'`);
      }
      // Never forwards a second argument: the native constructor is only
      // ever invoked with `length`, so there is no `options` value --
      // plain or a hostile getter -- for it to ever read a
      // `maxByteLength` from.
      return Reflect.construct(Native, [length], new.target);
    }
    Guarded.prototype = Native.prototype;
    Object.setPrototypeOf(Guarded, Native);
    Object.defineProperty(Guarded, "name", { value: name, configurable: true });
    Object.defineProperty(Guarded, "length", { value: 1, configurable: true });
    Object.defineProperty(Native.prototype, "constructor", {
      value: Guarded,
      writable: false,
      configurable: false,
    });

    Object.defineProperty(Native.prototype, "resize", {
      value: function resize() {
        throw new TypeError(`${name}.prototype.resize is disabled in this sandbox`);
      },
      writable: false,
      configurable: false,
    });
    if (typeof Native.prototype.transfer === "function") {
      const nativeTransfer = Native.prototype.transfer;
      Object.defineProperty(Native.prototype, "transfer", {
        value: function transfer(...args) {
          if (this.resizable) {
            throw new TypeError(
              `${name}.prototype.transfer of a resizable buffer is disabled in this sandbox`,
            );
          }
          return Reflect.apply(nativeTransfer, this, args);
        },
        writable: false,
        configurable: false,
      });
    }

    Object.defineProperty(globalThis, name, {
      value: Guarded,
      writable: false,
      configurable: false,
    });
    Object.freeze(Guarded);
    Object.freeze(Native.prototype);
  }
  guardResizable("ArrayBuffer");
  guardResizable("SharedArrayBuffer");
})();
"#;

static PLATFORM_INIT: Once = Once::new();

/// Sets the spike's V8 flags and initializes the V8 platform, exactly once
/// per process.
///
/// Must run before the first `JsRuntime` is constructed anywhere in the
/// process; [`prepare_isolate`] (the crate's only `JsRuntime::new` call
/// site) calls it, so nothing else in this crate needs to. Exposed
/// publicly only so a caller that also needs to coordinate with something
/// outside this crate (e.g. applying a Landlock lockdown before V8 spawns
/// its worker threads — see `tests/landlock_order.rs`) can force it at a
/// specific point.
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
    /// design question this leaves for Phase 5b. Does not by itself cover
    /// resizable `ArrayBuffer`s or `Deno.core.deserialize`/`structuredClone` (see `BOOTSTRAP_GUARD_SCRIPT`) or
    /// op-created backing stores (see `v8_alloc.rs`'s module doc comment).
    pub max_array_buffer_bytes: usize,
    /// V8 heap size (young + old generation) at which the near-heap-limit
    /// callback fires and terminates the isolate.
    pub max_heap_bytes: usize,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("bootstrap script failed: {0}")]
    Bootstrap(#[source] Box<deno_core::error::JsError>),
    #[error("module load failed: {0}")]
    Load(#[source] deno_core::error::CoreError),
    #[error("module evaluation failed: {0}")]
    Evaluate(#[source] deno_core::error::CoreError),
    #[error("event loop failed: {0}")]
    EventLoop(#[source] deno_core::error::CoreError),
    #[error("module specifier was rejected: {0}")]
    Specifier(#[source] deno_core::url::ParseError),
}

impl EngineError {
    /// True if this error is V8 isolate termination surfacing as
    /// `deno_core`'s synthesized "execution terminated" error (see
    /// `deno_core::error`'s handling of `scope.is_execution_terminating()`),
    /// which is what the near-heap-limit callback in [`install_heap_limit_callback`]
    /// produces by calling `IsolateHandle::terminate_execution()`.
    ///
    /// Distinguishes a real termination from an ordinary JS exception (e.g.
    /// a thrown `Error`), which carries its own message and stack frames;
    /// a termination's synthesized error has neither.
    #[must_use]
    pub fn is_isolate_termination(&self) -> bool {
        let core_error = match self {
            Self::Evaluate(err) | Self::EventLoop(err) => err,
            Self::Bootstrap(_) | Self::Load(_) | Self::Specifier(_) => {
                return false;
            }
        };
        let deno_core::error::CoreErrorKind::Js(js_error) = &*core_error.0 else {
            return false;
        };
        js_error.message.as_deref() == Some("execution terminated") && js_error.frames.is_empty()
    }
}

/// A freshly built, not-yet-run isolate: the V8 platform is initialized,
/// the capped allocator and heap limits are set, the module loader only
/// knows `module_code` (see `StaticModuleLoader`'s docs -- no filesystem
/// access, no other module reachable), and `BOOTSTRAP_GUARD_SCRIPT` has
/// already run. Produced only by [`prepare_isolate`].
struct PreparedIsolate {
    runtime: JsRuntime,
    isolate_handle: v8::IsolateHandle,
    specifier: ModuleSpecifier,
}

/// The crate's *only* `JsRuntime::new` call site (see the crate doc
/// comment). Always calls [`ensure_platform_initialized`] first, installs
/// the capped `ArrayBuffer` allocator (`limits.max_array_buffer_bytes`,
/// reporting into `usage`) and heap limits (`limits.max_heap_bytes`, used
/// as both `initial` and `max` -- see the comment inline below), builds the
/// isolate with a [`StaticModuleLoader`] that serves only `module_code`
/// under a fixed specifier and nothing else (no `FsModuleLoader`: that
/// would let a static or dynamic `import` in `module_code` read arbitrary
/// files from disk, independently of any OS-level lockdown), and runs
/// [`BOOTSTRAP_GUARD_SCRIPT`] before returning.
fn prepare_isolate(
    limits: EngineLimits,
    module_code: &'static str,
    usage: Arc<AtomicUsize>,
) -> Result<PreparedIsolate, EngineError> {
    ensure_platform_initialized();

    let allocator = v8_alloc::capped_array_buffer_allocator(limits.max_array_buffer_bytes, usage);
    // `initial` is set equal to `max`, not 0: with `initial=0` V8 picks its
    // own (much smaller) starting ceiling and grows it heuristically, which
    // in `tests/spike.rs` produced a spurious near-heap-limit callback --
    // and therefore isolate termination -- from a single small, unrelated
    // allocation, well before `max_heap_bytes` was anywhere close to being
    // reached. Fixing the ceiling removes that ambiguity for a one-shot,
    // short-lived handler isolate, at the cost of the (here irrelevant)
    // memory-saving benefit `heap_limits`' own docs describe for `initial=0`.
    let create_params = v8::CreateParams::default()
        .array_buffer_allocator(allocator)
        .heap_limits(limits.max_heap_bytes, limits.max_heap_bytes);

    let specifier = spike_module_specifier()?;
    let module_loader = StaticModuleLoader::with(specifier.clone(), module_code);

    let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(Rc::new(module_loader)),
        extensions: vec![ops::apsides_spike::init()],
        create_params: Some(create_params),
        ..Default::default()
    });

    runtime
        .execute_script(
            "ext:apsides-engine/bootstrap_guard.js",
            BOOTSTRAP_GUARD_SCRIPT,
        )
        .map_err(EngineError::Bootstrap)?;

    let isolate_handle = runtime.v8_isolate().thread_safe_handle();

    Ok(PreparedIsolate {
        runtime,
        isolate_handle,
        specifier,
    })
}

fn spike_module_specifier() -> Result<ModuleSpecifier, EngineError> {
    ModuleSpecifier::parse("file:///apsides-engine-spike/main.js").map_err(EngineError::Specifier)
}

/// Bytes of heap-limit headroom granted per call by
/// [`install_heap_limit_callback`], up to [`HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES`]
/// in total.
const HEAP_LIMIT_TERMINATION_HEADROOM_STEP_BYTES: usize = 8 * 1024 * 1024;

/// Hard total cap on cumulative headroom [`install_heap_limit_callback`]
/// will ever grant one isolate, across every call. Bounds how long
/// termination can be deferred while still giving V8 a few chances (not
/// just one) to unwind cleanly; see that function's doc comment for why one
/// grant was found, in review, not always to be enough.
const HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES: usize =
    4 * HEAP_LIMIT_TERMINATION_HEADROOM_STEP_BYTES;

/// Installs the near-heap-limit callback that terminates `runtime`'s
/// isolate (via `isolate_handle`) when its V8 heap approaches
/// `EngineLimits::max_heap_bytes`.
///
/// Grants a fixed headroom step on each call, up to a hard total cap
/// (found in review to need fixing twice: the original version granted
/// `current * 1.5` on *every* call, unbounded, which would let a genuinely
/// runaway isolate keep re-triggering the callback and keep growing the
/// limit indefinitely instead of actually being terminated; a
/// single-headroom-ever version fixed that but was too strict --
/// `terminate_execution()` does not always unwind within one GC pass, so a
/// large runaway allocation could hit the callback again before actually
/// stopping, with no headroom left to survive it).
///
/// **This does not guarantee the controller process never aborts.** Once
/// the total cap is exhausted, every further call returns `current`
/// unchanged while still requesting termination again; if V8 is still not
/// making progress after that many grants, V8's own fallback is
/// `V8::FatalProcessOutOfMemory`, which aborts the process. This crate
/// cannot close that gap without deeper V8 integration (e.g. inspecting
/// `Isolate::get_heap_statistics()` inside the callback to distinguish "GC
/// is still reclaiming, give it another grant" from "this isolate is
/// genuinely never going to stop", which review left as an open question
/// for Phase 5b, not fixed here -- see `EngineLimits::max_array_buffer_bytes`'s
/// doc comment for the same trade-off on the array-buffer-cap path). What
/// this callback does guarantee: termination is requested on the very
/// first call, and every isolate this crate's own tests actually exercise
/// (including the deliberately-runaway ones) terminates well within the
/// total cap -- see `tests/spike.rs`.
fn install_heap_limit_callback(runtime: &mut JsRuntime, isolate_handle: v8::IsolateHandle) {
    let mut headroom_granted_bytes: usize = 0;
    runtime.add_near_heap_limit_callback(move |current, initial| {
        isolate_handle.terminate_execution();
        if headroom_granted_bytes >= HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES {
            return current;
        }
        headroom_granted_bytes =
            headroom_granted_bytes.saturating_add(HEAP_LIMIT_TERMINATION_HEADROOM_STEP_BYTES);
        initial.saturating_add(headroom_granted_bytes)
    });
}

/// Loads `specifier` as the main module (via `runtime`'s
/// [`StaticModuleLoader`]), evaluates it, and drives the event loop to
/// completion.
#[allow(
    clippy::future_not_send,
    reason = "deno_core::JsRuntime is deliberately !Send; see the crate's `run_spike_module` doc comment"
)]
async fn drive_to_completion(
    runtime: &mut JsRuntime,
    specifier: &ModuleSpecifier,
) -> Result<(), EngineError> {
    let module_id = runtime
        .load_main_es_module(specifier)
        .await
        .map_err(EngineError::Load)?;
    let evaluated = runtime.mod_evaluate(module_id);
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .map_err(EngineError::EventLoop)?;
    evaluated.await.map_err(EngineError::Evaluate)
}

/// Runs `module_code` as the main (and only reachable) ES module of a fresh
/// isolate built with `limits`, registering the spike's one op extension.
///
/// Returns `Err` on any JS-side failure, including a heap-limit or
/// array-buffer-cap termination (see [`EngineError::is_isolate_termination`]).
/// Every termination this crate's own tests exercise stays well within
/// [`install_heap_limit_callback`]'s headroom cap and so surfaces as this
/// `Err`, not a process abort -- see `tests/spike.rs` -- but that callback's
/// doc comment explains why this crate cannot *guarantee* the process never
/// aborts in the pathological case where V8 still cannot unwind after the
/// full headroom cap is spent.
///
/// `usage` reports the capped `ArrayBuffer` allocator's live byte usage
/// (see `v8_alloc::capped_array_buffer_allocator`); pass a fresh
/// `Arc::new(AtomicUsize::new(0))` if the caller does not need to inspect
/// it. Because `usage` is caller-owned (only a clone is moved into the
/// isolate), it stays inspectable after this call returns regardless of
/// whether it returned `Ok`, `Err`, or (via [`run_spike_module_blocking`])
/// timed out.
///
/// # Errors
///
/// Returns `Err` if the module specifier cannot be parsed (never, in
/// practice: it is a fixed literal), if the bootstrap guard script fails,
/// if `module_code` fails to load, if the module fails to evaluate, or if
/// the event loop reports a failure — which includes a heap-limit or
/// array-buffer-cap termination (see `EngineLimits`'s field docs).
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
    usage: &Arc<AtomicUsize>,
) -> Result<(), EngineError> {
    let PreparedIsolate {
        mut runtime,
        isolate_handle,
        specifier,
    } = prepare_isolate(limits, module_code, Arc::clone(usage))?;

    // Captured before installing the near-heap-limit callback, and moved
    // into it, so the callback can request termination of *this* isolate
    // from wherever V8 invokes it (documented as callable from a GC pause
    // on the isolate's own thread) without needing `&mut JsRuntime`, which
    // the callback signature does not provide.
    install_heap_limit_callback(&mut runtime, isolate_handle);

    drive_to_completion(&mut runtime, &specifier).await
}

/// Like [`run_spike_module`], but with a watchdog `std::thread` attached.
///
/// Spawns a watchdog `std::thread` that calls
/// `IsolateHandle::terminate_execution()` after `terminate_after`, runs the
/// isolate on its own dedicated current-thread tokio runtime, and blocks
/// the calling thread until it finishes.
///
/// This is the mechanism Phase 5b's real wall-clock watchdog (`timeoutMillis`
/// in the plan) needs, and `run_spike_module_blocking`'s `tokio::time::timeout`
/// is *not* a substitute for it: a `tokio::time::timeout` around an
/// `.await` only ever gets a chance to fire at an await point the awaited
/// future actually yields at. A synchronous, never-yielding handler body
/// (`while (true) {}` at module top level, with no `await` in it at all)
/// runs entirely inside a single, uninterrupted `poll()` of that future --
/// `tokio::time::timeout` cannot preempt it, because the executor never
/// gets control back to check the deadline. `IsolateHandle::terminate_execution()`
/// called from a *different* OS thread works regardless, because V8 checks
/// for a pending termination request at points a tight synchronous loop
/// still reaches (that is its documented purpose). See
/// `tests/spike.rs`'s `watchdog_thread_terminates_a_synchronous_infinite_loop`.
///
/// The watchdog thread does not linger past this call (found in review to
/// need fixing: an earlier version just `sleep`d for `terminate_after`
/// unconditionally, so a run that finished well before the deadline still
/// left a thread parked asleep until the full duration elapsed): it blocks
/// on a channel with `terminate_after` as the `recv_timeout` deadline
/// instead of a bare `sleep`, so dropping the sending half here (once
/// `drive_to_completion` returns, however it returns) wakes it immediately
/// with `Disconnected` rather than `Timeout`, and it exits without calling
/// `terminate_execution()`. This function joins it before returning either
/// way, so it never returns with the watchdog thread still alive.
///
/// # Errors
///
/// See [`run_spike_module`]'s `# Errors`; additionally returns
/// `Err(SpikeRunError::Runtime(_))` if the tokio runtime fails to build.
pub fn run_spike_module_with_watchdog(
    limits: EngineLimits,
    module_code: &'static str,
    usage: &Arc<AtomicUsize>,
    terminate_after: Duration,
) -> Result<(), SpikeRunError> {
    let PreparedIsolate {
        mut runtime,
        isolate_handle,
        specifier,
    } = prepare_isolate(limits, module_code, Arc::clone(usage)).map_err(SpikeRunError::Engine)?;

    install_heap_limit_callback(&mut runtime, isolate_handle.clone());

    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        // `Timeout`: the deadline passed with the run still going --
        // terminate it. `Disconnected` (the sender was dropped below,
        // meaning the run already finished): exit without touching the
        // isolate. Nothing is ever sent on this channel; it exists only
        // for its `Disconnected` signal.
        if matches!(
            cancel_rx.recv_timeout(terminate_after),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            isolate_handle.terminate_execution();
        }
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(SpikeRunError::Runtime)?;
    let result = rt
        .block_on(drive_to_completion(&mut runtime, &specifier))
        .map_err(SpikeRunError::Engine);

    drop(cancel_tx);
    // A panicking watchdog thread (it cannot: the closure above has no
    // fallible/panicking calls) would be the only way `join` returns
    // `Err`; there is nothing more to do with that here than let it drop.
    let _ = watchdog.join();

    result
}

/// Runs `run_spike_module` on a fresh single-threaded current-thread tokio
/// runtime and blocks on it.
///
/// The `timeout` here is a `tokio::time::timeout` around the whole
/// `.await` chain, which only helps between await points (op calls, event
/// loop turns) -- it is **not** a substitute for a real wall-clock
/// watchdog against a synchronous, never-yielding script; see
/// [`run_spike_module_with_watchdog`]'s doc comment for why, and for the
/// mechanism Phase 5b actually needs (a separate OS thread calling
/// `IsolateHandle::terminate_execution()`).
///
/// `usage` is forwarded to `run_spike_module`; see its doc comment.
///
/// # Errors
///
/// Returns `Err` if the tokio runtime fails to build, if `module_code` does
/// not finish within `timeout`, or if `run_spike_module` itself returns
/// `Err` (see its docs).
pub fn run_spike_module_blocking(
    limits: EngineLimits,
    module_code: &'static str,
    usage: &Arc<AtomicUsize>,
    timeout: Duration,
) -> Result<(), SpikeRunError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(SpikeRunError::Runtime)?;
    rt.block_on(async move {
        tokio::time::timeout(timeout, run_spike_module(limits, module_code, usage))
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

impl SpikeRunError {
    /// See [`EngineError::is_isolate_termination`].
    #[must_use]
    pub fn is_isolate_termination(&self) -> bool {
        matches!(self, Self::Engine(err) if err.is_isolate_termination())
    }
}
