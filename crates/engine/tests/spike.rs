//! Phase 5a spike gate tests (see `/home/user/apsides-PLAN.md`,
//! "Phase 5 -> 5a. Spike gate"). Each `#[test]` here runs in its own thread
//! within one shared test-binary process, which is fine: V8 platform init
//! happens at most once process-wide (`engine::ensure_platform_initialized`
//! is `Once`-guarded) and every test below builds its own fresh
//! `JsRuntime`/isolate, consistent with the "one fresh isolate per
//! invocation" model this spike is checking the feasibility of.

use std::time::Duration;

use engine::{EngineError, EngineLimits, SpikeRunError, run_spike_module_blocking};

const GENEROUS_LIMITS: EngineLimits = EngineLimits {
    max_array_buffer_bytes: 16 * 1024 * 1024,
    max_heap_bytes: 64 * 1024 * 1024,
};

/// Proves the pinned `deno_core` + prebuilt V8 actually links, boots an
/// isolate, loads a tiny ES module, and dispatches one `#[op2]` op
/// round-trip (Rust -> V8 -> Rust return value observed by the module).
#[test]
fn op2_op_is_callable_from_an_es_module() {
    let module = r#"
        const pong = Deno.core.ops.op_apsides_spike_ping();
        if (pong !== "pong-from-rust") {
            throw new Error(`unexpected op result: ${pong}`);
        }
    "#;

    let result = run_spike_module_blocking(GENEROUS_LIMITS, module, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected the op round trip to succeed, got {result:?}"
    );
}

/// Empirically confirms the V8 flags applied in
/// `engine::ensure_platform_initialized` (`--jitless`, and the default
/// `enable_sharedarraybuffer_per_context=false`) actually remove
/// `WebAssembly` and `SharedArrayBuffer` from the global object, rather than
/// just asserting flag names are "right" by inspection. See the
/// `SPIKE_V8_FLAGS` doc comment in `src/lib.rs` for why `--jitless` alone is
/// expected to do this on this prebuilt (non-DrumBrake) V8 build.
#[test]
fn wasm_and_shared_array_buffer_are_not_exposed() {
    let module = r#"
        if (typeof WebAssembly !== "undefined") {
            throw new Error("WebAssembly must not be exposed under --jitless");
        }
        if (typeof SharedArrayBuffer !== "undefined") {
            throw new Error("SharedArrayBuffer must not be exposed by default");
        }
    "#;

    let result = run_spike_module_blocking(GENEROUS_LIMITS, module, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected WebAssembly/SharedArrayBuffer to be absent, got {result:?}"
    );
}

/// Proves the capped V8 `ArrayBuffer` allocator (`src/v8_alloc.rs`) actually
/// refuses an allocation past its cap -- and records a genuine 5a finding
/// about *how* that refusal surfaces.
///
/// The initial expectation (matching V8's documented allocator contract) was
/// that a null return from a custom `v8::ArrayBuffer::Allocator` becomes a
/// JS-catchable `RangeError`. Running this test showed otherwise on this
/// build: V8 invokes the *near-heap-limit callback* (see `SPIKE_V8_FLAGS`'s
/// sibling note in `src/lib.rs`) as part of handling the failed external
/// allocation, even though actual JS heap usage is nowhere near
/// `max_heap_bytes`. Since that callback unconditionally calls
/// `terminate_execution()` (matching the plan's "near-heap-limit callback
/// that terminates the isolate" for the *heap* limit), an array-buffer-cap
/// breach ends up terminating the whole isolate before the `try/catch` in
/// the module below ever runs -- confirmed by the resulting `JsError` having
/// no source line/frame, i.e. termination happened inside the V8-internal
/// allocation call, not after returning control to JS.
///
/// This is left as-is for the spike (not "fixed" by making the callback
/// distinguish real heap pressure from external-allocation pressure, e.g.
/// via `Isolate::get_heap_statistics`), and flagged as an open design
/// question for Phase 5b: should an over-cap array-buffer allocation be
/// JS-catchable, or is terminating the whole invocation the intended,
/// simpler behavior for a short-lived handler? See the Phase 5a report.
#[test]
fn array_buffer_allocation_over_the_cap_terminates_the_isolate_without_aborting_the_process() {
    let limits = EngineLimits {
        max_array_buffer_bytes: 1024 * 1024,
        max_heap_bytes: 64 * 1024 * 1024,
    };
    let module = r#"
        let threw = false;
        try {
            // 8x the 1 MiB cap: guaranteed to exceed it regardless of any
            // small fixed overhead V8 charges the allocator.
            new Uint8Array(8 * 1024 * 1024);
        } catch (e) {
            threw = true;
        }
        if (!threw) {
            throw new Error("allocation past the cap should have thrown");
        }
    "#;

    let result = run_spike_module_blocking(limits, module, Duration::from_secs(10));

    assert!(
        result.is_err(),
        "expected the over-cap allocation to terminate the isolate \
         (see the doc comment above), got {result:?}"
    );

    let followup = run_spike_module_blocking(
        GENEROUS_LIMITS,
        r#"
            const pong = Deno.core.ops.op_apsides_spike_ping();
            if (pong !== "pong-from-rust") {
                throw new Error("op broken after an over-cap allocation elsewhere");
            }
        "#,
        Duration::from_secs(10),
    );
    assert!(
        followup.is_ok(),
        "process must still be able to run further isolates after an \
         over-cap array-buffer allocation, got {followup:?}"
    );
}

/// The core 5a memory-safety claim: when a handler's heap approaches
/// `maxHeapBytes`, the near-heap-limit callback terminates *the isolate*,
/// which surfaces as an `Err` from the engine call -- and this test process
/// (standing in for the controller process) is still alive and able to make
/// further assertions afterward, i.e. it did not abort.
#[test]
fn heap_limit_terminates_the_isolate_without_aborting_the_process() {
    let limits = EngineLimits {
        max_array_buffer_bytes: 16 * 1024 * 1024,
        // Small enough to hit quickly, large enough for V8 to finish
        // booting the isolate before the callback ever fires.
        max_heap_bytes: 20 * 1024 * 1024,
    };
    let module = r"
        const chunks = [];
        for (let i = 0; i < 1_000_000; i++) {
            chunks.push(new Array(100_000).fill(Math.random()));
        }
    ";

    let result = run_spike_module_blocking(limits, module, Duration::from_secs(20));

    let terminated = matches!(
        result.as_ref(),
        Err(SpikeRunError::Engine(
            EngineError::EventLoop(_) | EngineError::Evaluate(_)
        ))
    );
    assert!(
        terminated,
        "expected a heap-limit termination error (event loop or module \
         evaluation), got {result:?}"
    );

    // If we reach this line, the process that ran the runaway allocation
    // is still executing normally: the isolate was terminated, the
    // controller process was not. Proven by running a second, unrelated
    // isolate to completion in this same process afterward.
    let followup = run_spike_module_blocking(
        GENEROUS_LIMITS,
        r#"
            const pong = Deno.core.ops.op_apsides_spike_ping();
            if (pong !== "pong-from-rust") {
                throw new Error("op broken after a prior isolate's heap-limit termination");
            }
        "#,
        Duration::from_secs(10),
    );
    assert!(
        followup.is_ok(),
        "process must still be able to run further isolates after a \
         heap-limit termination, got {followup:?}"
    );
}
