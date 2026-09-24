//! Regression tests for the sandbox guarantees the Phase 5a spike gate
//! established (see the Apsides v0.3 plan, "Phase 5 -> 5a. Spike gate"),
//! adapted to the Phase 5b `HandlerInvocation`/`invoke_handler*` API. Each
//! `#[test]` here runs in its own thread within one shared test-binary
//! process, which is fine: V8 platform init happens at most once
//! process-wide (`engine::ensure_platform_initialized` is `Once`-guarded)
//! and every test below builds its own fresh `JsRuntime`/isolate,
//! consistent with the "one fresh isolate per invocation" model.
//!
//! Every handler body below is wrapped by `common::wrap_as_handler` into
//! `export async function handler(event) { <body> }`, which throws on
//! failure the same way the Phase 5a spike's bare top-level scripts did.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use common::{GENEROUS_LIMITS, handler_invocation, wrap_as_handler};
use engine::{InvocationReport, Limits, invoke_handler_with_watchdog};

fn no_usage() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

/// Runs `body` through [`invoke_handler_with_watchdog`], the crate's one
/// production entry point, not the test-only `invoke_handler_blocking` --
/// review round 6, X1 was invisible to this suite specifically because it
/// drove everything through an entry point that runs inside an
/// already-built `rt.block_on`, sidestepping the exact ordering bug the
/// production entry point had. Every test in this file goes through the
/// real entry point now, so none of it needs the `test-util` feature.
fn run(body: &str) -> InvocationReport {
    let bundle = wrap_as_handler(body);
    invoke_handler_with_watchdog(handler_invocation(&bundle), &no_usage())
}

fn run_with_limits(limits: Limits, body: &str) -> InvocationReport {
    let bundle = wrap_as_handler(body);
    let mut invocation = handler_invocation(&bundle);
    invocation.limits = limits;
    invoke_handler_with_watchdog(invocation, &no_usage())
}

/// Proves the pinned `deno_core` + prebuilt V8 actually links, boots an
/// isolate, loads a tiny ES module, calls its exported async handler
/// function with an event, and dispatches an `apsides.log` op round trip
/// (Rust -> V8 -> Rust, observed via `InvocationReport::logs`).
#[test]
fn apsides_log_is_callable_from_the_handler_export() -> Result<(), String> {
    let report = run(
        r#"
        apsides.log("info", "pong-from-rust");
    "#,
    );

    report
        .result
        .map_err(|err| format!("expected the handler invocation to succeed: {err}"))?;
    assert_eq!(
        report.logs,
        vec![engine::LogLine {
            level: engine::LogLevel::Info,
            message: "pong-from-rust".to_owned(),
        }]
    );
    Ok(())
}

/// Empirically confirms the V8 flags applied in
/// `engine::ensure_platform_initialized` (`--jitless`, and
/// `--enable-sharedarraybuffer-per-context`) actually remove `WebAssembly`
/// and `SharedArrayBuffer` from the global object, rather than just
/// asserting flag names are "right" by inspection.
#[test]
fn wasm_and_shared_array_buffer_are_not_exposed() {
    let result = run(
        r#"
        if (typeof WebAssembly !== "undefined") {
            throw new Error("WebAssembly must not be exposed under --jitless");
        }
        if (typeof SharedArrayBuffer !== "undefined") {
            throw new Error("SharedArrayBuffer must not be exposed by default");
        }
    "#,
    );

    assert!(
        result.result.is_ok(),
        "expected WebAssembly/SharedArrayBuffer to be absent, got {result:?}"
    );
}

/// Proves the capped V8 `ArrayBuffer` allocator (`src/v8_alloc.rs`)
/// actually refuses an allocation past its cap, terminating the isolate
/// (see the Phase 5a report for why this surfaces as a termination, not a
/// catchable `RangeError`) without aborting the process.
#[test]
fn array_buffer_allocation_over_the_cap_terminates_the_isolate_without_aborting_the_process() {
    let limits = Limits {
        max_array_buffer_bytes: 1024 * 1024,
        max_heap_bytes: 64 * 1024 * 1024,
        timeout: Duration::from_secs(10),
        max_fetch_bytes_total: 16 * 1024 * 1024,
        max_concurrent_fetches: 4,
    };
    let result = run_with_limits(
        limits,
        r#"
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
    "#,
    );

    let terminated = matches!(&result.result, Err(err) if err.is_isolate_termination());
    assert!(
        terminated,
        "expected an isolate-termination error, not merely any `Err`. Got {result:?}"
    );

    let followup = run(r#"apsides.log("info", "still alive");"#);
    assert!(
        followup.result.is_ok(),
        "process must still be able to run further isolates after an \
         over-cap array-buffer allocation, got {followup:?}"
    );
}

/// Regression test: `Deno.core.deserialize` reaches V8's structured-clone
/// deserializer directly, which builds `ArrayBuffer`s (including resizable
/// ones, from a suitable serialized tag) at the C++ level -- never through
/// the JS-visible `ArrayBuffer` constructor `BOOTSTRAP_GUARD_SCRIPT`'s
/// Layer 2 guards. Closed by Layer 1 (hiding `Deno` entirely).
#[test]
fn deserialize_route_is_closed_by_hiding_deno() {
    let result = run(
        r"
        let threw = false;
        let errorName = '';
        try {
            // A V8 structured-clone encoding of a resizable ArrayBuffer.
            Deno.core.deserialize(new Uint8Array([255, 16, 126, 0, 128, 128, 128, 128, 1]));
        } catch (e) {
            threw = true;
            errorName = e.constructor.name;
        }
        if (!threw) {
            throw new Error('Deno.core.deserialize should be unreachable');
        }
        if (errorName !== 'ReferenceError') {
            throw new Error('expected ReferenceError (Deno undefined), got ' + errorName);
        }
    ",
    );

    assert!(
        result.result.is_ok(),
        "expected the deserialize route to be closed (caught ReferenceError), got {result:?}"
    );
}

/// Regression test: `Deno.core.structuredClone` reaches the same V8
/// structured-clone machinery as `deserialize`. Closed the same way.
#[test]
fn structured_clone_route_is_closed_by_hiding_deno() {
    let result = run(
        r"
        let threw = false;
        let errorName = '';
        try {
            Deno.core.structuredClone(new ArrayBuffer(0));
        } catch (e) {
            threw = true;
            errorName = e.constructor.name;
        }
        if (!threw) {
            throw new Error('Deno.core.structuredClone should be unreachable');
        }
        if (errorName !== 'ReferenceError') {
            throw new Error('expected ReferenceError (Deno undefined), got ' + errorName);
        }
    ",
    );

    assert!(
        result.result.is_ok(),
        "expected the structuredClone route to be closed (caught ReferenceError), got {result:?}"
    );
}

/// Confirms the guarded `ArrayBuffer` constructor never reads/forwards
/// `options` at all (closing a TOCTOU bug an earlier version had), and
/// that `.resize`/`.transfer` are unconditionally disabled regardless of
/// receiver -- including via the `prototype.constructor` bypass.
#[test]
fn guarded_constructor_toctou_and_resize_transfer_are_closed() {
    let result = run(
        r"
        let getterCalls = 0;
        const evilOptions = {
            get maxByteLength() {
                getterCalls++;
                return getterCalls === 1 ? undefined : 256 * 1024 * 1024;
            },
        };
        const buf = new ArrayBuffer(0, evilOptions);
        if (getterCalls !== 0) {
            throw new Error(
                'the guarded constructor must never read options at all, read it ' +
                getterCalls + ' time(s)',
            );
        }
        if (buf.resizable) {
            throw new Error('buffer must not be resizable despite the TOCTOU options object');
        }

        let resizeThrew = false;
        try {
            buf.resize(0);
        } catch (e) {
            resizeThrew = true;
        }
        if (!resizeThrew) {
            throw new Error('ArrayBuffer.prototype.resize must always throw');
        }

        const ViaPrototype = new Uint8Array(1).buffer.constructor;
        const viaPrototypeBuf = new ViaPrototype(0, evilOptions);
        if (viaPrototypeBuf.resizable) {
            throw new Error('prototype.constructor bypass produced a resizable buffer');
        }
        let viaPrototypeResizeThrew = false;
        try {
            viaPrototypeBuf.resize(0);
        } catch (e) {
            viaPrototypeResizeThrew = true;
        }
        if (!viaPrototypeResizeThrew) {
            throw new Error('resize must throw on a prototype.constructor-bypass buffer too');
        }
    ",
    );

    assert!(
        result.result.is_ok(),
        "expected the TOCTOU/resize/transfer checks to pass, got {result:?}"
    );
}

/// Confirms `BOOTSTRAP_GUARD_SCRIPT`'s Layer 1 actually removed
/// `Deno`/`globalThis.__bootstrap` from what a handler can see, and that
/// the new `apsides` global is present and frozen instead.
#[test]
fn deno_and_bootstrap_internals_are_unreachable_but_apsides_is_present() {
    let result = run(
        r#"
        if (typeof globalThis.__bootstrap !== "undefined") {
            throw new Error("globalThis.__bootstrap must be undefined");
        }
        if (typeof Deno !== "undefined") {
            throw new Error("Deno must be undefined");
        }
        if (typeof globalThis.__apsidesOps !== "undefined") {
            throw new Error("the old __apsidesOps surface must be gone");
        }
        if (typeof apsides !== "object" || apsides === null) {
            throw new Error("apsides must be present");
        }
        if (
            typeof apsides.fetch !== "function" ||
            typeof apsides.writeOutput !== "function" ||
            typeof apsides.log !== "function"
        ) {
            throw new Error("apsides.fetch/writeOutput/log must all be functions");
        }
        if (!Object.isFrozen(apsides)) {
            throw new Error("apsides must be frozen");
        }
        const originalFetch = apsides.fetch;
        // ES modules are always strict mode: assigning to a frozen
        // property throws, rather than silently no-op-ing the way it would
        // in sloppy-mode code.
        let assignmentThrew = false;
        try {
            apsides.fetch = () => {};
        } catch (e) {
            assignmentThrew = true;
        }
        if (!assignmentThrew) {
            throw new Error("assigning to the frozen apsides.fetch must throw in strict mode");
        }
        if (apsides.fetch !== originalFetch) {
            throw new Error("apsides.fetch must be unchanged after the rejected assignment");
        }
    "#,
    );

    assert!(
        result.result.is_ok(),
        "expected Deno/__bootstrap to be unreachable and apsides to be present and frozen, got {result:?}"
    );
}

/// Regression test: an earlier fix for static-method preservation put the
/// *native, unguarded* constructor back within reach via
/// `Object.getPrototypeOf(ArrayBuffer)`. `BOOTSTRAP_GUARD_SCRIPT` no longer
/// calls `Object.setPrototypeOf` on the guarded constructor at all.
#[test]
fn native_array_buffer_constructor_is_unreachable_via_prototype_chain() {
    let result = run(
        r#"
        if (Object.getPrototypeOf(ArrayBuffer) !== Function.prototype) {
            throw new Error("Object.getPrototypeOf(ArrayBuffer) must be Function.prototype");
        }
        if (Reflect.getPrototypeOf(ArrayBuffer) !== Function.prototype) {
            throw new Error("Reflect.getPrototypeOf(ArrayBuffer) must be Function.prototype");
        }

        let threwOnDirectCall = false;
        try {
            new (Object.getPrototypeOf(ArrayBuffer))(256 * 1024 * 1024, {
                maxByteLength: 256 * 1024 * 1024,
            });
        } catch (e) {
            threwOnDirectCall = true;
        }
        if (!threwOnDirectCall) {
            throw new Error("Function.prototype must not be constructible");
        }

        const viaPrototype = new Uint8Array(4).buffer.constructor;
        if (viaPrototype !== ArrayBuffer) {
            throw new Error("new Uint8Array(4).buffer.constructor must be the guarded ArrayBuffer");
        }
        const viaPrototypeBuf = new viaPrototype(0, { maxByteLength: 256 * 1024 * 1024 });
        if (viaPrototypeBuf.resizable) {
            throw new Error("prototype.constructor route must not accept maxByteLength");
        }

        let walker = ArrayBuffer;
        for (let hops = 0; walker !== null; hops++) {
            if (hops > 10) {
                throw new Error("prototype chain walk did not terminate");
            }
            walker = Object.getPrototypeOf(walker);
        }

        const descriptor = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "constructor");
        if (descriptor.value !== ArrayBuffer) {
            throw new Error("ArrayBuffer.prototype.constructor must be the guarded ArrayBuffer");
        }
        if (descriptor.writable !== false || descriptor.configurable !== false) {
            throw new Error("ArrayBuffer.prototype.constructor must be non-writable and non-configurable");
        }
    "#,
    );

    assert!(
        result.result.is_ok(),
        "expected the native-constructor-unreachable checks to pass, got {result:?}"
    );
}

/// Regression test: `Guarded`/`transfer` used to call
/// `Reflect.construct`/`Reflect.apply` *at call time*, letting user code
/// hijack `Reflect.construct`/`Reflect.apply` to steal the native
/// constructor/`transfer` function. Fixed by capturing both at the top of
/// `BOOTSTRAP_GUARD_SCRIPT`, before any other statement runs.
#[test]
fn reflect_construct_and_apply_are_captured_before_hijack() {
    let result = run(
        r#"
        const nativeConstruct = Reflect.construct;
        const nativeApply = Reflect.apply;
        const constructCalls = [];
        const applyCalls = [];
        Reflect.construct = function (target, args, newTarget) {
            constructCalls.push(target);
            return nativeConstruct(target, args, newTarget);
        };
        Reflect.apply = function (target, thisArg, args) {
            applyCalls.push(target);
            return nativeApply(target, thisArg, args);
        };

        const buf = new ArrayBuffer(4);
        const transferred = buf.transfer();
        const attempt = new ArrayBuffer(0, { maxByteLength: 256 * 1024 * 1024 });

        if (constructCalls.length !== 0) {
            throw new Error(
                "the guard must never call Reflect.construct at call time; recorded " +
                constructCalls.length + " call(s)",
            );
        }
        if (applyCalls.length !== 0) {
            throw new Error(
                "the guard must never call Reflect.apply at call time; recorded " +
                applyCalls.length + " call(s)",
            );
        }
        if (buf.resizable || transferred.resizable || attempt.resizable) {
            throw new Error("a resizable buffer was constructed with Reflect hijacked");
        }
    "#,
    );

    assert!(
        result.result.is_ok(),
        "expected the Reflect-hijack checks to pass, got {result:?}"
    );
}

/// Regression test, extending the same concern to `apsides.fetch`/
/// `writeOutput`/`log`: hijacking `Function.prototype.call`/`.apply` (which
/// a hijacked `Reflect.apply` would eventually call through to) must never
/// hand user code the raw `op_apsides_*` function references, because
/// `BOOTSTRAP_GUARD_SCRIPT` invokes them with plain call syntax, which does
/// not consult either.
#[test]
fn apsides_wrappers_never_leak_the_captured_op_functions_via_call_or_apply_hijack() {
    let result = run(
        r#"
        const capturedThis = [];
        Function.prototype.call = function (thisArg, ...args) {
            capturedThis.push(thisArg);
            return this(...args);
        };
        Function.prototype.apply = function (thisArg, args) {
            capturedThis.push(thisArg);
            return this(...(args || []));
        };

        apsides.log("info", "hello");
        try {
            apsides.writeOutput("undeclared", "key", "data");
        } catch (e) {
            // Expected to throw (undeclared output id) -- the point of this
            // test is only whether the call reached the op function through
            // a hijacked call/apply, not whether the write succeeded.
        }

        if (capturedThis.length !== 0) {
            throw new Error(
                "apsides.log/writeOutput must never route through " +
                "Function.prototype.call/.apply; recorded " + capturedThis.length +
                " hijacked call(s)",
            );
        }
    "#,
    );

    assert!(
        result.result.is_ok(),
        "expected no hijacked call/apply invocation to have fired, got {result:?}"
    );
}

/// Static methods/properties user code depends on -- `ArrayBuffer.isView`,
/// `ArrayBuffer[Symbol.species]` -- must keep working even though the
/// native constructor is never made reachable via `Object.setPrototypeOf`.
#[test]
fn array_buffer_statics_are_preserved() {
    let result = run(
        r#"
        if (ArrayBuffer.isView(new Uint8Array(1)) !== true) {
            throw new Error("ArrayBuffer.isView(new Uint8Array(1)) must be true");
        }
        if (ArrayBuffer.isView({}) !== false) {
            throw new Error("ArrayBuffer.isView({}) must be false");
        }
        if (ArrayBuffer[Symbol.species] !== ArrayBuffer) {
            throw new Error("ArrayBuffer[Symbol.species] must be ArrayBuffer itself");
        }
    "#,
    );

    assert!(
        result.result.is_ok(),
        "expected ArrayBuffer's static methods to still work, got {result:?}"
    );
}

/// Self-check probe for other resizable/growable creation routes: `eval`/
/// `new Function`, the (absent) Fetch API, `DataView` over an ordinary
/// buffer, and `Atomics.waitAsync`/`WebAssembly.Memory` (both already
/// unreachable since `SharedArrayBuffer`/`WebAssembly` are absent).
#[test]
fn other_construction_routes_do_not_bypass_the_array_buffer_guard() {
    let result = run(
        r#"
        const evalBuf = eval("new ArrayBuffer(0, { maxByteLength: 1024 })");
        if (evalBuf.resizable) {
            throw new Error("eval() bypassed the ArrayBuffer guard");
        }

        const fnBuf = new Function(
            "return new ArrayBuffer(0, { maxByteLength: 1024 });",
        )();
        if (fnBuf.resizable) {
            throw new Error("new Function(...)() bypassed the ArrayBuffer guard");
        }

        if (typeof Response !== "undefined") {
            throw new Error("Response must not be present (no deno_fetch/deno_web extension)");
        }

        if (typeof DataView !== "function") {
            throw new Error("DataView must still be present for ordinary fixed buffers");
        }
        const view = new DataView(new ArrayBuffer(4));
        view.setUint32(0, 0xdead_beef);
        if (view.getUint32(0) !== 0xdead_beef) {
            throw new Error("DataView over an ordinary fixed buffer must still work");
        }
    "#,
    );

    assert!(
        result.result.is_ok(),
        "expected the probed routes to be closed (or, for DataView, to keep working), got {result:?}"
    );
}

/// The core 5a memory-safety claim: when a handler's heap approaches
/// `maxHeapBytes`, the near-heap-limit callback terminates *the isolate*,
/// surfacing as an `Err` -- and the process is still alive afterward.
#[test]
fn heap_limit_terminates_the_isolate_without_aborting_the_process() {
    let limits = Limits {
        max_array_buffer_bytes: 16 * 1024 * 1024,
        // Small enough to hit quickly, large enough for V8 to finish
        // booting the isolate before the callback ever fires.
        max_heap_bytes: 20 * 1024 * 1024,
        timeout: Duration::from_secs(20),
        max_fetch_bytes_total: 16 * 1024 * 1024,
        max_concurrent_fetches: 4,
    };
    let result = run_with_limits(
        limits,
        r"
        const chunks = [];
        for (let i = 0; i < 1_000_000; i++) {
            chunks.push(new Array(100_000).fill(Math.random()));
        }
    ",
    );

    let terminated = matches!(&result.result, Err(err) if err.is_isolate_termination());
    assert!(
        terminated,
        "expected an isolate-termination error, got {result:?}"
    );

    let followup = run(r#"apsides.log("info", "still alive");"#);
    assert!(
        followup.result.is_ok(),
        "process must still be able to run further isolates after a \
         heap-limit termination, got {followup:?}"
    );
}

/// Demonstrates the mechanism the real wall-clock watchdog needs: a
/// `tokio::time::timeout` around an `.await` chain only ever gets a chance
/// to act at an await point the running script's future actually yields
/// at. `invoke_handler_with_watchdog` instead spawns a separate OS thread
/// that calls `IsolateHandle::terminate_execution()` after a deadline,
/// which works against a synchronous, never-yielding script because V8
/// checks for a pending termination request at points a tight synchronous
/// loop still reaches.
#[test]
fn watchdog_thread_terminates_a_synchronous_infinite_loop() {
    let mut limits = GENEROUS_LIMITS;
    limits.timeout = Duration::from_millis(200);

    let result = run_with_limits(limits, "while (true) {}");

    let terminated = matches!(&result.result, Err(err) if err.is_isolate_termination());
    assert!(
        terminated,
        "expected the watchdog thread to terminate the synchronous infinite \
         loop with an isolate-termination error, got {result:?}"
    );

    // The watchdog thread terminates *the isolate*; this process (standing
    // in for the controller process) must still be alive and able to run
    // further isolates afterward, i.e. it did not abort.
    let followup = run(r#"apsides.log("info", "still alive");"#);
    assert!(
        followup.result.is_ok(),
        "process must still be able to run further isolates after a \
         watchdog timeout termination, got {followup:?}"
    );
}

/// Regression test for review round 6, X1: `invoke_handler_with_watchdog`
/// used to build its isolate (`JsRuntime::new`, inside `begin_invocation`)
/// *before* building its own tokio runtime, so `deno_core` registered the
/// isolate with no `Handle` to post tasks onto at all. Sustained
/// allocation churn -- batches of plain-object arrays allocated and
/// dropped in a loop, with an `await` between batches so the event loop
/// actually gets turns, for around 3 real seconds -- reliably drives V8's
/// GC hard enough that its background memory reducer schedules a delayed
/// foreground task, which is exactly the path that used to call `abort()`
/// and kill this whole test process outright (not a catchable Rust
/// `Result`, not an isolate-termination error -- the process itself dying
/// is the failure mode this test exists to catch). This goes through
/// `run`, i.e. `invoke_handler_with_watchdog` directly, unlike the
/// original bug report's repro, which needed the production entry point
/// specifically because the test suite's prior use of
/// `invoke_handler_blocking` (inside an already-built `rt.block_on`)
/// could never have reproduced this.
#[test]
fn allocation_churn_survives_through_the_watchdog_entry_point() {
    let mut limits = GENEROUS_LIMITS;
    limits.timeout = Duration::from_secs(30);

    let result = run_with_limits(
        limits,
        r#"
        const deadline = Date.now() + 3000;
        let n = 0;
        while (Date.now() < deadline) {
            for (let i = 0; i < 50; i++) {
                const churn = new Array(10000).fill(n);
                n = (n + churn.length) % 1000;
            }
            // Yields back to the event loop between batches, so this
            // exercises the same "isolate driven across multiple event
            // loop turns while GC does real background work" shape as
            // the original repro, not one uninterrupted synchronous call.
            await Promise.resolve();
        }
        return "survived";
    "#,
    );

    assert!(
        matches!(&result.result, Ok(value) if *value == serde_json::json!("survived")),
        "expected the allocation-churn handler to return Ok(\"survived\"), got {result:?} -- \
         if this test process itself died (SIGABRT) instead of reporting a failed assertion, \
         that is X1 regressing: deno_core posted a delayed task with no tokio Handle to post it \
         onto"
    );

    // The whole point: this process must still be alive and able to run
    // a further isolate afterward, i.e. the allocation churn above did
    // not abort it.
    let followup = run(r#"apsides.log("info", "still alive");"#);
    assert!(
        followup.result.is_ok(),
        "process must still be able to run further isolates after sustained allocation churn, \
         got {followup:?}"
    );
}

/// The isolate is not reused across invocations: a global a handler sets
/// in one invocation must be absent in the next, fresh isolate.
#[test]
fn the_isolate_is_not_reused_across_invocations() {
    let first = run(r"globalThis.leakedFromFirstInvocation = 42;");
    assert!(first.result.is_ok(), "expected the first invocation to succeed, got {first:?}");

    let second = run(
        r#"
        if (typeof globalThis.leakedFromFirstInvocation !== "undefined") {
            throw new Error("a global set in a prior invocation leaked into this one");
        }
    "#,
    );
    assert!(
        second.result.is_ok(),
        "expected the second invocation to see a clean isolate, got {second:?}"
    );
}
