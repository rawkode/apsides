//! Phase 5a spike gate tests (see the Apsides v0.3 plan, "Phase 5 -> 5a.
//! Spike gate"). Each `#[test]` here runs in its own thread within one
//! shared test-binary process, which is fine: V8 platform init happens at
//! most once process-wide (`engine::ensure_platform_initialized` is
//! `Once`-guarded) and every test below builds its own fresh
//! `JsRuntime`/isolate, consistent with the "one fresh isolate per
//! invocation" model this spike is checking the feasibility of.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use engine::{
    EngineLimits, SpikeRunError, run_spike_module_blocking, run_spike_module_with_watchdog,
};

const GENEROUS_LIMITS: EngineLimits = EngineLimits {
    max_array_buffer_bytes: 16 * 1024 * 1024,
    max_heap_bytes: 64 * 1024 * 1024,
};

fn no_usage() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

/// Proves the pinned `deno_core` + prebuilt V8 actually links, boots an
/// isolate, loads a tiny ES module from the in-memory-only module loader,
/// and dispatches one `#[op2]` op round-trip (Rust -> V8 -> Rust return
/// value observed by the module).
#[test]
fn op2_op_is_callable_from_an_es_module() {
    let module = r#"
        const pong = Deno.core.ops.op_apsides_spike_ping();
        if (pong !== "pong-from-rust") {
            throw new Error(`unexpected op result: ${pong}`);
        }
    "#;

    let result = run_spike_module_blocking(
        GENEROUS_LIMITS,
        module,
        &no_usage(),
        Duration::from_secs(10),
    );

    assert!(
        result.is_ok(),
        "expected the op round trip to succeed, got {result:?}"
    );
}

/// Empirically confirms the V8 flags applied in
/// `engine::ensure_platform_initialized` (`--jitless`, and
/// `--enable-sharedarraybuffer-per-context`) actually remove `WebAssembly`
/// and `SharedArrayBuffer` from the global object, rather than just
/// asserting flag names are "right" by inspection. See the `SPIKE_V8_FLAGS`
/// doc comment in `src/lib.rs` for the V8-internal logic this rests on --
/// including why `--enable-sharedarraybuffer-per-context=true` is the flag
/// value that *disables* `SharedArrayBuffer` (backwards from what its name
/// suggests), found only by running this test.
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

    let result = run_spike_module_blocking(
        GENEROUS_LIMITS,
        module,
        &no_usage(),
        Duration::from_secs(10),
    );

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
/// `max_heap_bytes`. Since that callback calls `terminate_execution()`
/// (matching the plan's "near-heap-limit callback that terminates the
/// isolate" for the *heap* limit), an array-buffer-cap breach ends up
/// terminating the whole isolate before the `try/catch` in the module below
/// ever runs. Verified two ways, not just "returned `Err`" (a no-op
/// allocator that let every allocation through, and thus never capped
/// anything, would also make this assert `is_err()` -- so that alone is not
/// evidence the cap did anything): (1) `SpikeRunError::is_isolate_termination`
/// confirms the specific "execution terminated", no-frames error shape a
/// real termination produces, not an ordinary thrown JS error; (2) the
/// allocator's own live usage counter is asserted to have stayed at or
/// under the cap throughout, i.e. the 8x-over-cap allocation the module
/// attempts never actually succeeded.
///
/// This is left as-is for the spike (not "fixed" by making the callback
/// distinguish real heap pressure from external-allocation pressure, e.g.
/// via `Isolate::get_heap_statistics`), and flagged as an open design
/// question for Phase 5b: should an over-cap array-buffer allocation be
/// JS-catchable, or is terminating the whole invocation the intended,
/// simpler behavior for a short-lived handler? See the Phase 5a report.
#[test]
fn array_buffer_allocation_over_the_cap_terminates_the_isolate_without_aborting_the_process() {
    let cap_bytes = 1024 * 1024;
    let limits = EngineLimits {
        max_array_buffer_bytes: cap_bytes,
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
    let usage = no_usage();

    let result = run_spike_module_blocking(limits, module, &usage, Duration::from_secs(10));

    let terminated = matches!(&result, Err(err) if err.is_isolate_termination());
    assert!(
        terminated,
        "expected an isolate-termination error (message \"execution terminated\", \
         no frames), not merely any `Err` (a no-op allocator would also produce \
         one) -- see the doc comment above. Got {result:?}"
    );
    let observed_usage = usage.load(Ordering::SeqCst);
    assert!(
        observed_usage <= cap_bytes,
        "the capped allocator's own usage counter reported {observed_usage} bytes, \
         over the {cap_bytes} byte cap -- the 8x-over-cap allocation must never \
         have actually succeeded"
    );

    let followup = run_spike_module_blocking(
        GENEROUS_LIMITS,
        r#"
            const pong = Deno.core.ops.op_apsides_spike_ping();
            if (pong !== "pong-from-rust") {
                throw new Error("op broken after an over-cap allocation elsewhere");
            }
        "#,
        &no_usage(),
        Duration::from_secs(10),
    );
    assert!(
        followup.is_ok(),
        "process must still be able to run further isolates after an \
         over-cap array-buffer allocation, got {followup:?}"
    );
}

/// Regression test for a review finding: the capped allocator in
/// `v8_alloc.rs` only governs ordinary, fixed-size `ArrayBuffer`s.
/// Resizable ones (`new ArrayBuffer(0, {maxByteLength: N})`, then
/// `.resize(N)`) are backed by memory V8 reserves/commits through its own
/// page allocator, bypassing that allocator's vtable entirely -- so before
/// `ARRAY_BUFFER_GUARD_SCRIPT` (`src/lib.rs`) existed, this exact
/// construction succeeded well past a 1 MiB cap. Reproduces the review's
/// repro precisely and asserts it is now rejected *at construction time*
/// (the guard script throws before `.resize()` is ever reached), with the
/// allocator's usage counter confirming no 256 MiB allocation happened.
#[test]
fn resizable_array_buffer_construction_is_rejected() {
    let cap_bytes = 1024 * 1024;
    let limits = EngineLimits {
        max_array_buffer_bytes: cap_bytes,
        max_heap_bytes: 64 * 1024 * 1024,
    };
    let module = r#"
        let threw = false;
        let message = "";
        try {
            const buf = new ArrayBuffer(0, { maxByteLength: 256 * 1024 * 1024 });
            buf.resize(256 * 1024 * 1024);
        } catch (e) {
            threw = true;
            message = String(e);
        }
        if (!threw) {
            throw new Error("resizable ArrayBuffer construction should have been rejected");
        }
        if (!message.includes("resizable")) {
            throw new Error(`unexpected rejection message: ${message}`);
        }

        // The guard must not be bypassable via the prototype's `constructor`
        // (which native `Uint8Array`/`TypedArray` construction reaches
        // directly, not through the JS-visible global binding): confirm
        // that route is patched too.
        const ViaPrototype = new Uint8Array(1).buffer.constructor;
        let prototypeThrew = false;
        try {
            new ViaPrototype(0, { maxByteLength: 1024 });
        } catch (e) {
            prototypeThrew = true;
        }
        if (!prototypeThrew) {
            throw new Error("prototype.constructor bypass of the guard was not closed");
        }
    "#;
    let usage = no_usage();

    let result = run_spike_module_blocking(limits, module, &usage, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected the resizable-ArrayBuffer construction to be rejected \
         (and caught) in JS, not to terminate or otherwise error the isolate, \
         got {result:?}"
    );
    let observed_usage = usage.load(Ordering::SeqCst);
    assert!(
        observed_usage < cap_bytes,
        "the capped allocator's own usage counter reported {observed_usage} bytes; \
         a 256 MiB resizable ArrayBuffer must never have actually been committed"
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

    let result = run_spike_module_blocking(limits, module, &no_usage(), Duration::from_secs(20));

    let terminated = matches!(&result, Err(err) if err.is_isolate_termination());
    assert!(
        terminated,
        "expected an isolate-termination error (event loop or module \
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
        &no_usage(),
        Duration::from_secs(10),
    );
    assert!(
        followup.is_ok(),
        "process must still be able to run further isolates after a \
         heap-limit termination, got {followup:?}"
    );
}

/// Demonstrates the mechanism Phase 5b's real wall-clock watchdog needs,
/// per review: a `tokio::time::timeout` around an `.await` chain (what
/// `run_spike_module_blocking` uses) only ever gets a chance to act at an
/// await point the running script's future actually yields at. A
/// synchronous, never-yielding script -- `while (true) {}` at module top
/// level, with no `await` anywhere -- runs entirely inside one
/// uninterrupted `poll()` and would hang `run_spike_module_blocking`
/// forever (not tested here: that would need an actual indefinite hang in
/// the test suite). `run_spike_module_with_watchdog` instead spawns a
/// separate OS thread that calls `IsolateHandle::terminate_execution()`
/// after a deadline, which works against exactly this case because V8
/// checks for a pending termination request at points a tight synchronous
/// loop still reaches.
#[test]
fn watchdog_thread_terminates_a_synchronous_infinite_loop() {
    let module = r"while (true) {}";

    let result = run_spike_module_with_watchdog(
        GENEROUS_LIMITS,
        module,
        &no_usage(),
        Duration::from_millis(200),
    );

    let terminated =
        matches!(&result, Err(SpikeRunError::Engine(err)) if err.is_isolate_termination());
    assert!(
        terminated,
        "expected the watchdog thread to terminate the synchronous infinite \
         loop with an isolate-termination error, got {result:?}"
    );
}
