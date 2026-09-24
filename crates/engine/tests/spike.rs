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
        const pong = __apsidesOps.ping();
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
            const pong = __apsidesOps.ping();
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

/// Regression test, route 1 of 3 the second review round found (1 MiB cap,
/// 256 MiB resizable buffer, allocator counter stayed 0 throughout):
/// `Deno.core.deserialize` reaches V8's structured-clone deserializer
/// directly, which builds `ArrayBuffer`s (including resizable ones, from a
/// suitable serialized tag) at the C++ level -- never through the
/// JS-visible `ArrayBuffer` constructor `BOOTSTRAP_GUARD_SCRIPT`'s Layer 2
/// guards, so Layer 2 alone did not close this route. Layer 1 (hiding
/// `Deno` entirely) does: reproduces the review's exact byte sequence and
/// confirms the whole route -- not just the resizable-buffer part of it --
/// throws a `ReferenceError` before `Deno.core.deserialize` is ever called.
#[test]
fn deserialize_route_is_closed_by_hiding_deno() {
    let cap_bytes = 1024 * 1024;
    let limits = EngineLimits {
        max_array_buffer_bytes: cap_bytes,
        max_heap_bytes: 64 * 1024 * 1024,
    };
    let module = r"
        let threw = false;
        let errorName = '';
        try {
            // The review's exact repro: this byte sequence is a V8
            // structured-clone encoding of a resizable ArrayBuffer.
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
    ";
    let usage = no_usage();

    let result = run_spike_module_blocking(limits, module, &usage, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected the deserialize route to be closed (caught ReferenceError), \
         got {result:?}"
    );
    let observed_usage = usage.load(Ordering::SeqCst);
    assert_eq!(
        observed_usage, 0,
        "the capped allocator's own usage counter must have stayed at 0: \
         Deno.core.deserialize must never have run at all"
    );
}

/// Regression test, route 2 of 3: `Deno.core.structuredClone` reaches the
/// same V8 structured-clone machinery as `deserialize` (see
/// `deserialize_route_is_closed_by_hiding_deno`'s doc comment) and would
/// pass a resizable buffer straight through. Closed the same way, by the
/// same fix (Layer 1 of `BOOTSTRAP_GUARD_SCRIPT`): `Deno` is unreachable, so
/// `Deno.core.structuredClone` throws `ReferenceError` before it is ever
/// called.
#[test]
fn structured_clone_route_is_closed_by_hiding_deno() {
    let cap_bytes = 1024 * 1024;
    let limits = EngineLimits {
        max_array_buffer_bytes: cap_bytes,
        max_heap_bytes: 64 * 1024 * 1024,
    };
    let module = r"
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
    ";
    let usage = no_usage();

    let result = run_spike_module_blocking(limits, module, &usage, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected the structuredClone route to be closed (caught \
         ReferenceError), got {result:?}"
    );
    let observed_usage = usage.load(Ordering::SeqCst);
    assert_eq!(
        observed_usage, 0,
        "the capped allocator's own usage counter must have stayed at 0"
    );
}

/// Regression test, route 3 of 3: the guarded `ArrayBuffer` constructor
/// itself. An earlier version read `options.maxByteLength` once and
/// rejected it if present -- a TOCTOU bug, since a hostile `options` object
/// could use a getter that lies on that one read. Confirms the actual fix
/// (never read or forward `options` at all: the getter below must be called
/// zero times) and, as defense in depth in case any construction route ever
/// does produce a resizable buffer, that `.resize`/`.transfer` on it are
/// unconditionally disabled regardless of receiver -- including via the
/// `prototype.constructor` bypass (native `TypedArray` construction reaches
/// the native `ArrayBuffer` constructor directly, not through the
/// JS-visible global binding).
#[test]
fn guarded_constructor_toctou_and_resize_transfer_are_closed() {
    let cap_bytes = 1024 * 1024;
    let limits = EngineLimits {
        max_array_buffer_bytes: cap_bytes,
        max_heap_bytes: 64 * 1024 * 1024,
    };
    let module = r"
        let getterCalls = 0;
        const evilOptions = {
            // Simulates the real TOCTOU shape: lies (undefined, not
            // resizable) on the first read, in case something still checks
            // `maxByteLength !== undefined` once and only once, then tells
            // the truth (a real, dangerous value) on any further read -- the
            // read the *native* constructor itself would perform if this
            // object were ever forwarded to it.
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
    ";
    let usage = no_usage();

    let result = run_spike_module_blocking(limits, module, &usage, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected the TOCTOU/resize/transfer checks to pass without \
         terminating or otherwise erroring the isolate, got {result:?}"
    );
    let observed_usage = usage.load(Ordering::SeqCst);
    assert_eq!(
        observed_usage, 0,
        "no actual ArrayBuffer memory should have been committed by this test \
         (every buffer constructed is 0 bytes); the capped allocator's own \
         usage counter confirms nothing slipped through"
    );
}

/// Review item 3: confirms `BOOTSTRAP_GUARD_SCRIPT`'s Layer 1 actually
/// removed `Deno`/`globalThis.__bootstrap` from what a user module can see,
/// not merely that the three specific exploit routes above happen to throw.
#[test]
fn deno_and_bootstrap_internals_are_unreachable_from_user_code() {
    let module = r#"
        if (typeof globalThis.__bootstrap !== "undefined") {
            throw new Error("globalThis.__bootstrap must be undefined");
        }
        if (typeof Deno !== "undefined") {
            throw new Error("Deno must be undefined");
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
        "expected Deno/__bootstrap to be unreachable, got {result:?}"
    );
}

/// Regression test, route 4 (found by a third review round): an earlier fix
/// for `array_buffer_statics_are_preserved`'s concern --
/// `Object.setPrototypeOf(Guarded, Native)`, so static methods like
/// `ArrayBuffer.isView` would resolve through `Guarded`'s own prototype
/// chain -- put the *native, unguarded* constructor back within reach via
/// `Object.getPrototypeOf(ArrayBuffer)`. Calling it directly with
/// `{maxByteLength}` commits the buffer's *initial* length through V8's own
/// page allocator at construction time, before `.resize()`/`.transfer()`
/// (guarded since the second review round) are ever involved -- so that
/// guard did not help here; the buffer never needs to grow.
/// `BOOTSTRAP_GUARD_SCRIPT` no longer calls `Object.setPrototypeOf` on
/// `Guarded` at all (see its updated doc comment); this test confirms the
/// whole static prototype chain from `ArrayBuffer` upward, plus the two
/// concrete repros the review named, plus the `constructor` property
/// descriptor's shape.
#[test]
fn native_array_buffer_constructor_is_unreachable_via_prototype_chain() {
    let cap_bytes = 1024 * 1024;
    let limits = EngineLimits {
        max_array_buffer_bytes: cap_bytes,
        max_heap_bytes: 64 * 1024 * 1024,
    };
    let module = r#"
        if (Object.getPrototypeOf(ArrayBuffer) !== Function.prototype) {
            throw new Error("Object.getPrototypeOf(ArrayBuffer) must be Function.prototype");
        }
        if (Reflect.getPrototypeOf(ArrayBuffer) !== Function.prototype) {
            throw new Error("Reflect.getPrototypeOf(ArrayBuffer) must be Function.prototype");
        }

        // Function.prototype itself must not be usable as a constructor at
        // all (it has no [[Construct]] internal method), closing the
        // review's exact repro: `new (Object.getPrototypeOf(ArrayBuffer))(...)`.
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

        // The review's second named repro: the prototype.constructor route
        // (native TypedArray construction reaches the native ArrayBuffer
        // constructor directly) must resolve to the guarded constructor,
        // not the native one, and a resizable buffer must not be
        // constructible through it either. Uses length 0 (not 256 MiB):
        // this route resolves to the *guarded* constructor (checked just
        // above), which enforces the array-buffer-byte cap on an ordinary
        // large fixed-length request too (covered by a different existing
        // test) -- mixing that unrelated, legitimate cap-termination path
        // into this resizability check would conflate two different
        // findings.
        const viaPrototype = new Uint8Array(4).buffer.constructor;
        if (viaPrototype !== ArrayBuffer) {
            throw new Error("new Uint8Array(4).buffer.constructor must be the guarded ArrayBuffer");
        }
        const viaPrototypeBuf = new viaPrototype(0, { maxByteLength: 256 * 1024 * 1024 });
        if (viaPrototypeBuf.resizable) {
            throw new Error("prototype.constructor route must not accept maxByteLength");
        }

        // Walk the whole static prototype chain from ArrayBuffer upward: it
        // must terminate at Object.prototype -> null within a handful of
        // hops, and never contain anything the native constructor could
        // hide behind.
        let walker = ArrayBuffer;
        for (let hops = 0; walker !== null; hops++) {
            if (hops > 10) {
                throw new Error("prototype chain walk did not terminate");
            }
            walker = Object.getPrototypeOf(walker);
        }

        // The constructor property itself: non-writable, non-configurable,
        // and pointing at the guarded constructor (globalThis.ArrayBuffer),
        // not the native one.
        const descriptor = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "constructor");
        if (descriptor.value !== ArrayBuffer) {
            throw new Error("ArrayBuffer.prototype.constructor must be the guarded ArrayBuffer");
        }
        if (descriptor.writable !== false || descriptor.configurable !== false) {
            throw new Error("ArrayBuffer.prototype.constructor must be non-writable and non-configurable");
        }
    "#;
    let usage = no_usage();

    let result = run_spike_module_blocking(limits, module, &usage, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected the native-constructor-unreachable checks to pass, got {result:?}"
    );
    let observed_usage = usage.load(Ordering::SeqCst);
    assert_eq!(
        observed_usage, 0,
        "the capped allocator's own usage counter must have stayed at 0: no \
         256 MiB resizable buffer's initial length may have been committed"
    );
}

/// Regression test, route 5 (found by a fourth review round, via source
/// audit rather than a repro): `Guarded` and the `transfer` wrapper in
/// `BOOTSTRAP_GUARD_SCRIPT` used to call `Reflect.construct`/`Reflect.apply`
/// *at call time* -- i.e. every time user code did `new ArrayBuffer(...)`
/// or `.transfer()`, not once at bootstrap. Since `Reflect.construct`/
/// `Reflect.apply` are ordinary, reassignable global properties, user code
/// could replace either with a hook that records its `target` argument
/// (the native, unguarded constructor or native `transfer` function,
/// respectively) before forwarding the call on normally -- fully stealthy,
/// and handing user code exactly the reference every other layer of this
/// guard exists to keep unreachable. Fixed by capturing both (`construct`/
/// `apply`) into local `const`s at the very top of the script, before any
/// other statement runs, so the guard's own machinery never performs a
/// fresh `Reflect.*` lookup that user code could have already replaced.
///
/// Installs exactly that hook (recording `target` and forwarding to a
/// separately-captured, un-hijacked native `Reflect.construct`/`apply`, so
/// the buffer operations below still succeed normally either way -- this
/// test needs to observe whether the hook *fires*, not merely whether
/// construction still works), then performs the two operations the guard's
/// internal code actually calls `Reflect.*` for (an ordinary construction
/// and a `.transfer()`) plus the full resizable-buffer repro, and asserts
/// the hooks recorded zero calls: with the fix, the guard's own code never
/// touches the (now hijacked) `Reflect.construct`/`Reflect.apply` at all,
/// so there is nothing for the hooks to ever record.
#[test]
fn reflect_construct_and_apply_are_captured_before_hijack() {
    let cap_bytes = 1024 * 1024;
    let limits = EngineLimits {
        max_array_buffer_bytes: cap_bytes,
        max_heap_bytes: 64 * 1024 * 1024,
    };
    let module = r#"
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

        // The two operations the guard's own internal code performs on
        // every call: an ordinary construction (Guarded) and a transfer.
        const buf = new ArrayBuffer(4);
        const transferred = buf.transfer();

        // The exact exploit repro, attempted again with the hooks live.
        const attempt = new ArrayBuffer(0, { maxByteLength: 256 * 1024 * 1024 });

        if (constructCalls.length !== 0) {
            throw new Error(
                "the guard must never call Reflect.construct at call time (would " +
                "leak the native constructor to a hijacked Reflect.construct); " +
                "recorded " + constructCalls.length + " call(s)",
            );
        }
        if (applyCalls.length !== 0) {
            throw new Error(
                "the guard must never call Reflect.apply at call time (would " +
                "leak the native transfer function to a hijacked Reflect.apply); " +
                "recorded " + applyCalls.length + " call(s)",
            );
        }
        if (buf.resizable || transferred.resizable || attempt.resizable) {
            throw new Error("a resizable buffer was constructed with Reflect hijacked");
        }
    "#;
    let usage = no_usage();

    let result = run_spike_module_blocking(limits, module, &usage, Duration::from_secs(10));

    assert!(
        result.is_ok(),
        "expected the Reflect-hijack checks to pass, got {result:?}"
    );
    let observed_usage = usage.load(Ordering::SeqCst);
    assert!(
        observed_usage <= cap_bytes,
        "the capped allocator's own usage counter reported {observed_usage} bytes, \
         over the {cap_bytes} byte cap -- this test only ever constructs \
         small, fixed-length buffers, so it should stay far under it"
    );
}

/// Static methods/properties user code depends on -- `ArrayBuffer.isView`,
/// `ArrayBuffer[Symbol.species]` -- must keep working even though
/// `BOOTSTRAP_GUARD_SCRIPT` no longer makes the native constructor reachable
/// via `Object.setPrototypeOf` (see
/// `native_array_buffer_constructor_is_unreachable_via_prototype_chain`'s
/// doc comment): it copies them onto the guarded constructor as its own
/// properties instead (`isView` wraps a captured reference to the native
/// function; `[Symbol.species]` is a getter returning `this`, matching the
/// spec's own definition).
#[test]
fn array_buffer_statics_are_preserved() {
    let module = r#"
        if (ArrayBuffer.isView(new Uint8Array(1)) !== true) {
            throw new Error("ArrayBuffer.isView(new Uint8Array(1)) must be true");
        }
        if (ArrayBuffer.isView({}) !== false) {
            throw new Error("ArrayBuffer.isView({}) must be false");
        }
        if (ArrayBuffer[Symbol.species] !== ArrayBuffer) {
            throw new Error("ArrayBuffer[Symbol.species] must be ArrayBuffer itself");
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
        "expected ArrayBuffer's static methods to still work, got {result:?}"
    );
}

/// Self-check probe (review ask: "run a quick self-check probe for other
/// resizable/growable creation routes"). Tried, with the outcome found:
///   - `eval("new ArrayBuffer(0, {maxByteLength:...})")` and
///     `new Function(...)` returning the same expression: both run in this
///     isolate's one realm, so both resolve the identifier `ArrayBuffer` to
///     the same guarded constructor ordinary code gets -- neither is a
///     distinct route. Asserted below, not just reasoned about.
///   - `new Response(...)`/other Fetch API constructors: not present (bare
///     `deno_core` with no `deno_fetch`/`deno_web` extension registered --
///     `typeof Response === "undefined"`; asserted below).
///   - `DataView` over a resizable buffer, tracking its live length: no
///     resizable buffer can exist at all (Layers 1+2), so this has no
///     buffer to attach to; `DataView` itself is asserted still present and
///     usable over an ordinary fixed buffer, since it is a legitimate,
///     harmless API this crate has no reason to remove.
///   - `Atomics.waitAsync`: requires a `SharedArrayBuffer`-backed view
///     (throws `TypeError` otherwise, per spec); `SharedArrayBuffer` itself
///     is `undefined` (`wasm_and_shared_array_buffer_are_not_exposed`), so
///     this route has no reachable input. Not re-asserted here (already
///     covered by that test); listed for completeness of the probe.
///   - `WebAssembly.Memory` (growable): needs `WebAssembly`, already
///     confirmed `undefined` under `--jitless` by
///     `wasm_and_shared_array_buffer_are_not_exposed`.
#[test]
fn other_construction_routes_do_not_bypass_the_array_buffer_guard() {
    let module = r#"
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
    "#;

    let result = run_spike_module_blocking(
        GENEROUS_LIMITS,
        module,
        &no_usage(),
        Duration::from_secs(10),
    );

    assert!(
        result.is_ok(),
        "expected the probed routes to be closed (or, for DataView, to keep \
         working normally over ordinary buffers), got {result:?}"
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
            const pong = __apsidesOps.ping();
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
