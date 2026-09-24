//! The Apsides handler engine (see the Apsides v0.3 plan, "Phase 5"):
//! embeds a pinned `deno_core`/V8 to run one untrusted ES module handler
//! per invocation, in a fresh isolate, with no filesystem access and no
//! network access except through the three typed, capability-checked ops
//! in [`ops`]. Grew out of the Phase 5a spike gate, which proved the
//! embedding itself was feasible; this is now the real Phase 5b engine
//! API ([`invoke_handler`] and friends), IR-agnostic and not yet wired
//! into `runtime` (that waits for Phase 4's handler IR).
//!
//! Every invocation gets: a capped `ArrayBuffer` allocator, a heap limit
//! whose near-heap-limit callback requests isolate termination on
//! approach, restrictive V8 flags (`--jitless`, no `WebAssembly`, no
//! `SharedArrayBuffer`) applied before platform init, a JS-level bootstrap
//! guard that closes the resizable-`ArrayBuffer` gap those caps leave open
//! (including through `deno_core`'s own `Deno.core.deserialize`/
//! `structuredClone`, which reach V8's structured-clone deserializer
//! directly and never call the JS-visible `ArrayBuffer` constructor at all
//! — see [`BOOTSTRAP_GUARD_SCRIPT`]) and that replaces `deno_core`'s own
//! globals with a small frozen `apsides` object (`apsides.fetch`,
//! `apsides.writeOutput`, `apsides.log`), a module loader with no
//! filesystem access serving only the caller's bundle, and a watchdog
//! thread enforcing `Limits::timeout` even against a synchronous,
//! never-yielding script.
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

mod net;
mod ops;
mod v8_alloc;

use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

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
/// exactly the three `op_apsides_*` op functions this engine's own curated
/// `globalThis.apsides` surface needs (`fetch`/`writeOutput`/`log` -- see
/// `ops.rs`), then deletes `globalThis.Deno` and `globalThis.__bootstrap`
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
/// The `apsides.fetch`/`writeOutput`/`log` wrapper functions extend the
/// "captured-values-only" invariant below in one respect specific to them:
/// they call the captured `opFetch`/`opOutputWrite`/`opLog` references with
/// **plain call syntax** (`opFetch(args)`), never `.call`/`.apply`/
/// `Reflect.apply`. Plain call syntax is a raw `[[Call]]`, which -- unlike
/// `Function.prototype.call`/`.apply`/`Reflect.apply` -- cannot be
/// intercepted by any global reassignment (there is no user-reassignable
/// path a `[[Call]]` consults), so it is *safer* here than routing through
/// the captured `apply` the way `guardResizable`'s `transfer` wrapper below
/// does. `transfer` uses `apply` only because it must forward a dynamic
/// `this` (the specific `ArrayBuffer` instance `.transfer()` was called on)
/// to the native method, which plain call syntax cannot do; none of the
/// `op_apsides_*` ops are methods needing a receiver, so they have no such
/// need and plain calls are both simpler and safe. What still matters, and
/// is preserved here: the op function references themselves are captured
/// once, at the top of this script, and never become reachable to user
/// code by any other path (they are not `Deno.core.ops.*` properties any
/// more once `Deno` is deleted below, and they are never passed as `this`
/// to anything user code could have already replaced).
///
/// **Layer 2 -- make growing *any* `ArrayBuffer`/`SharedArrayBuffer`
/// impossible, regardless of how it was constructed.** The capped allocator
/// in `v8_alloc.rs` only governs ordinary, fixed-size `ArrayBuffer`
/// allocations: a *resizable* one (`new ArrayBuffer(0, {maxByteLength: N})`,
/// then `.resize(N)`) is backed by memory V8 reserves/commits through its
/// own page allocator, never calling into the custom
/// `v8::ArrayBuffer::Allocator` vtable at all.
///
/// **No V8 build flag disables this**, confirmed two ways on this exact
/// pinned build (not just by inspecting `flag-definitions.h`, which has no
/// `resizable`/`growable`/`rab_gsab`/`max_byte_length` flag anywhere, nor
/// any flag mentioned near the `RAB_GSAB_TYPED_ARRAYS`/
/// `GrowableSharedArrayBufferByteLength` machinery in `src/objects/` and
/// `src/runtime/` -- resizable buffers are unconditional, permanent, stable
/// V8 behavior with no flag gate in this version, not merely a feature this
/// crate forgot to disable): (1) `v8::V8::set_flags_from_string("--no-harmony-rab-gsab
/// --harmony-rab-gsab=false")` (the review's suggested name and a plausible
/// alternative) prints `Error: unrecognized flag --no-harmony-rab-gsab` to
/// stderr -- V8 does not know this flag; (2) `v8::V8::set_flags_from_string("--help")`'s
/// full flag dump (grepped for "resiz"/"array.?buffer"/"max.?byte") lists
/// only unrelated flags (`--resize-large-object`, `--track-array-buffer-views`,
/// `--concurrent-array-buffer-sweeping`, a test-only `--mock-arraybuffer-allocator`
/// pair, `--js-immutable-arraybuffer`), none of which gate resizability.
/// Since there is no structural fix available, this layer closes it at the
/// JS level instead:
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
///     JS-visible global binding. The `constructor` property is defined
///     non-writable and non-configurable.
///   - Replaces `NativeCtor.prototype.resize` with a function that
///     **always throws, unconditionally, regardless of the receiver**, and
///     `.transfer` with one that throws when a captured reference to the
///     native `resizable` getter (invoked through the captured `apply`,
///     never `this.resizable` -- see the invariant below) says `true`
///     (`.transfer()` on a resizable buffer produces another resizable
///     buffer, preserving resizability; `.transferToFixedLength()` is left
///     alone, since it
///     always produces a fixed buffer). Every `ArrayBuffer` instance in one
///     realm, however it was constructed, shares this one
///     `%ArrayBuffer.prototype%` object, so this closes growth for any
///     construction route uniformly -- **but does not by itself stop the
///     *initial* length of a resizable buffer from being committed**: a
///     third review round found that `Object.setPrototypeOf(Guarded, Native)`
///     (an earlier version of the next bullet's static-preservation fix)
///     put `Native` back within reach via `Object.getPrototypeOf(ArrayBuffer)`,
///     and `new (that Native)(256 << 20, {maxByteLength: 256 << 20})`
///     commits the *initial* 256 MiB through V8's page allocator at
///     construction time, before `.resize()`/`.transfer()` are ever called
///     -- so blocking those two methods did not help.
///   - Copies exactly the two static members user code needs --
///     `ArrayBuffer.isView` (wrapped: `Guarded.isView` calls a captured
///     reference to `Native.isView`, never exposing `Native` itself) and
///     `[Symbol.species]` (a getter returning `this`, matching the spec's
///     own definition) -- as `Guarded`'s own properties, instead of
///     (the review-flagged bug) making `Native` reachable at all via
///     `Object.setPrototypeOf(Guarded, Native)`. `Guarded`'s own
///     `[[Prototype]]` is left as the ordinary `Function.prototype`: nothing
///     in this crate ever calls `Object.setPrototypeOf` on `Guarded`, and
///     `Native` is not stored anywhere reachable from `globalThis` once this
///     script returns (it lives only in `guardResizable`'s closure, captured
///     by the handful of functions defined inside it).
///   - **Never looks up `Reflect`, `TypeError`, or any other global/built-in
///     by name inside a function that can run after bootstrap** (a fourth
///     review round's finding: `Guarded` called `Reflect.construct(Native,
///     ...)` *at call time* -- i.e. every time user code did `new
///     ArrayBuffer(...)`, not once at bootstrap -- so user code could
///     replace `Reflect.construct` with a function that records and returns
///     its `target` argument, then call `new ArrayBuffer(0)` to receive
///     `Native` itself out of that recorded argument, fully bypassing every
///     guard above; `Reflect.apply` in the `transfer` wrapper leaked
///     `nativeTransfer`, the same way, whenever `.transfer()` was called).
///     `Reflect.construct`, `Reflect.apply`, and `TypeError` are captured
///     into local `const`s at the very top of this script, before any other
///     statement runs, and every later reference inside `guardResizable`'s
///     nested functions uses those captured bindings, never the bare
///     identifiers. **This is the general invariant, not just a fix for
///     this one instance**: any function defined in this script that is
///     still callable after bootstrap finishes (`Guarded`, its `isView`
///     wrapper, the `[Symbol.species]` getter, `resize`, `transfer`) may
///     only ever use values already captured in a closure by the time this
///     IIFE returns control to `deno_core` -- never a fresh lookup of a
///     global variable or a built-in's static/prototype method, however
///     deep (`Object.freeze`, `Object.defineProperty`, etc. are fine to
///     call directly in `guardResizable`'s own top-level body, which runs
///     exactly once, synchronously, before user code exists to have
///     replaced anything -- but never inside a closure that persists past
///     that point). Every future addition to this script must be checked
///     against that invariant, not just against the specific bypasses this
///     review found.
///
/// **Audited list of V8 sites that can create a resizable/growable backing
/// store**, and why each is closed (re-check this list, not just this
/// script, before bumping the pinned `deno_core`/`v8` versions):
///   - The `ArrayBuffer`/`SharedArrayBuffer` constructor: closed by
///     `guardResizable` above (never forwards `options`, and the native
///     constructor is unreachable per the invariant above).
///   - `ArrayBuffer.prototype.transfer`: closed by the wrapper above.
///     Needs an existing resizable buffer to begin with, which cannot exist
///     (`.transferToFixedLength()`, which can never produce a resizable
///     result regardless of its receiver, is left unwrapped).
///   - V8's structured-clone `ValueDeserializer` (`Deno.core.deserialize`/
///     `structuredClone`, which can decode a resizable buffer directly from
///     a serialized tag, entirely bypassing the JS-visible constructor):
///     only reachable via `Deno.core`, deleted by Layer 1 above.
///   - WebAssembly (a growable `WebAssembly.Memory` is backed by the same
///     kind of resizable allocation): `wasm-js.cc`'s exact rule is
///     `expose_wasm = !jitless || correctness_fuzzer_suppressions ||
///     wasm_jitless` (checked against `v8-150.4.0/src/wasm/wasm-js.cc`),
///     and this build sets `jitless` while never setting
///     `correctness_fuzzer_suppressions`; `wasm_jitless` is
///     `DEFINE_BOOL_READONLY(wasm_jitless, false, ...)` on every target but
///     tvOS (where `jitless` implies it) on a non-`DrumBrake` build such as
///     this one's prebuilt, so `expose_wasm` is `false` and `WebAssembly`
///     is never installed on `globalThis`. Separately, `deno_core` itself
///     still compiles a module tagged `ModuleType::Wasm` as WebAssembly
///     (`modules/map.rs`) regardless of this flag -- that path is closed
///     only because [`StaticModuleLoader`] (used in `prepare_isolate`)
///     always tags every module it serves `ModuleType::JavaScript`; any
///     future change to the module loader must keep rejecting/never
///     producing a `ModuleType::Wasm` source, independently of the V8 flag
///     above.
///   - V8's C++-only `v8::ArrayBuffer::NewResizableBackingStore` embedder
///     API: has no JS-reachable caller; neither `deno_core` nor this
///     crate's own Rust code (`v8_alloc.rs`) calls it.
///   - The V8 snapshot deserializer (which can restore heap objects,
///     including resizable buffers, from a startup snapshot blob): this
///     crate never builds or loads a startup snapshot (`RuntimeOptions::startup_snapshot`
///     is left `None` in `prepare_isolate`), and even if it did, a snapshot
///     blob is embedder-supplied, never user input.
///   - `%_TheHole()`-style runtime-test intrinsics (`%ArrayBufferMaxByteLength()`
///     and friends, which can construct engine-internal objects directly):
///     require the `--allow-natives-syntax` V8 flag, and `deno_core`'s own
///     `setup.rs` only ever adds that flag when
///     `RuntimeOptions::unsafe_expose_natives_and_gc` is `true` (it is
///     `cfg`-gated to the `test`/`unsafe_runtime_options` feature and
///     defaults to `false` even then). This crate builds with neither the
///     `unsafe_runtime_options` Cargo feature nor a test-`cfg` build, and
///     never sets that field, so `%`-prefixed syntax is a plain
///     `SyntaxError` in every isolate this crate builds.
///
/// See `tests/spike.rs` for the regression tests: one per bypass route the
/// four review rounds found, plus the static-preservation, prototype-chain,
/// and Deno-unreachability checks.
const BOOTSTRAP_GUARD_SCRIPT: &str = r#""use strict";
(() => {
  // Captured before any other statement runs, so nothing below this point
  // ever performs a fresh lookup of a global/built-in that user code could
  // have replaced by the time these values are actually used (see the
  // invariant in this constant's doc comment). `construct`/`apply` are the
  // fourth review round's fix: `Guarded`/`transfer` used to call
  // `Reflect.construct`/`Reflect.apply` *at call time* -- i.e. every time
  // user code did `new ArrayBuffer(...)`/`.transfer()`, not once here --
  // so user code could replace `Reflect.construct` with a function that
  // records and returns its `target` argument, then trigger a call to
  // receive `Native` (or `nativeTransfer`) directly out of that recorded
  // argument, bypassing every guard below.
  const construct = Reflect.construct;
  const apply = Reflect.apply;
  const NativeTypeError = TypeError;
  // Captured for `apsides.log`'s non-string argument formatting and
  // `apsides.fetch`'s body encoding, respectively -- see the invariant
  // above: every value a persisting closure below uses is captured here,
  // before any user code has had a chance to run.
  const NativeJSONStringify = JSON.stringify;
  const NativeUint8Array = Uint8Array;

  // Layer 1: hide deno_core's own internals. Captures only the three op
  // functions this engine's own `apsides` global needs, before deleting
  // every other route to any of them (or to anything else under `Deno`).
  const opFetch = globalThis.Deno.core.ops.op_apsides_fetch;
  const opOutputWrite = globalThis.Deno.core.ops.op_apsides_output_write;
  const opLog = globalThis.Deno.core.ops.op_apsides_log;

  // A minimal, self-contained UTF-8 encoder: bare deno_core has no
  // TextEncoder (that is a `deno_web` extension, not registered here), and
  // this must stay usable from a closure that persists past bootstrap, so
  // it touches nothing but its own arguments/locals and the captured
  // NativeUint8Array.
  function utf8Encode(text) {
    const bytes = [];
    for (let i = 0; i < text.length; i++) {
      let code = text.codePointAt(i);
      if (code > 0xffff) {
        i++;
      }
      if (code < 0x80) {
        bytes.push(code);
      } else if (code < 0x800) {
        bytes.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f));
      } else if (code < 0x10000) {
        bytes.push(
          0xe0 | (code >> 12),
          0x80 | ((code >> 6) & 0x3f),
          0x80 | (code & 0x3f),
        );
      } else {
        bytes.push(
          0xf0 | (code >> 18),
          0x80 | ((code >> 12) & 0x3f),
          0x80 | ((code >> 6) & 0x3f),
          0x80 | (code & 0x3f),
        );
      }
    }
    return NativeUint8Array.from(bytes);
  }

  function toBytes(input) {
    if (input === undefined || input === null) {
      return new NativeUint8Array(0);
    }
    if (input instanceof NativeUint8Array) {
      return input;
    }
    if (typeof input === "string") {
      return utf8Encode(input);
    }
    throw new NativeTypeError("body must be a string or a Uint8Array");
  }

  function throwIfError(outcome) {
    if (outcome.outcome === "error") {
      const err = new NativeTypeError(outcome.message);
      err.code = outcome.code;
      throw err;
    }
  }

  const apsides = Object.freeze({
    fetch: async (url, options) => {
      const opts = options || {};
      const method = opts.method || "GET";
      const headers = opts.headers ? Object.entries(opts.headers) : [];
      const bodyBytes = toBytes(opts.body);
      const outcome = await opFetch({ url, method, headers }, bodyBytes);
      throwIfError(outcome);
      return {
        status: outcome.status,
        headers: outcome.headers,
        body: NativeUint8Array.from(outcome.body),
      };
    },
    writeOutput: (id, key, data) => {
      const outcome = opOutputWrite(id, key, toBytes(data));
      throwIfError(outcome);
    },
    log: (...args) => {
      let line = "";
      for (let i = 0; i < args.length; i++) {
        if (i > 0) {
          line += " ";
        }
        line += typeof args[i] === "string" ? args[i] : NativeJSONStringify(args[i]);
      }
      opLog(line);
    },
  });
  Object.defineProperty(globalThis, "apsides", {
    value: apsides,
    writable: false,
    configurable: false,
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
        throw new NativeTypeError(`Constructor ${name} requires 'new'`);
      }
      // Never forwards a second argument: the native constructor is only
      // ever invoked with `length`, so there is no `options` value --
      // plain or a hostile getter -- for it to ever read a
      // `maxByteLength` from. Uses the captured `construct`, not
      // `Reflect.construct` (see the top of this script).
      return construct(Native, [length], new.target);
    }
    Guarded.prototype = Native.prototype;
    Object.defineProperty(Guarded, "name", { value: name, configurable: true });
    Object.defineProperty(Guarded, "length", { value: 1, configurable: true });
    // Copies exactly the two statics user code needs, instead of making
    // Native reachable via Object.setPrototypeOf(Guarded, Native) (the
    // review-flagged bug: that put Native within reach via
    // Object.getPrototypeOf(ArrayBuffer), and calling it directly with
    // {maxByteLength} commits the buffer's *initial* length through V8's
    // page allocator before .resize()/.transfer() are ever involved).
    {
      // `isView` is an `ArrayBuffer`-only static (`SharedArrayBuffer` has
      // no such method), hence the guard.
      if (typeof Native.isView === "function") {
        const nativeIsView = Native.isView;
        Object.defineProperty(Guarded, "isView", {
          value: (arg) => nativeIsView(arg),
          writable: false,
          configurable: true,
        });
      }
      Object.defineProperty(Guarded, Symbol.species, {
        get() {
          return this;
        },
        configurable: true,
      });
    }
    Object.defineProperty(Native.prototype, "constructor", {
      value: Guarded,
      writable: false,
      configurable: false,
    });

    Object.defineProperty(Native.prototype, "resize", {
      value: function resize() {
        throw new NativeTypeError(`${name}.prototype.resize is disabled in this sandbox`);
      },
      writable: false,
      configurable: false,
    });
    if (typeof Native.prototype.transfer === "function") {
      const nativeTransfer = Native.prototype.transfer;
      // Captured here, not read as `this.resizable` inside `transfer`
      // below: a plain `this.resizable` property read is itself a runtime
      // lookup that walks `this`'s prototype chain, which -- while not
      // exploitable today, since no resizable buffer instance can exist
      // for it to be checked against once construction is closed -- is
      // exactly the shape of lookup the invariant above forbids inside a
      // function that runs after bootstrap. Reading the descriptor here
      // instead, while `Native.prototype` is still pristine, and invoking
      // the captured getter through the captured `apply` keeps `transfer`
      // itself free of any post-bootstrap lookup, full stop.
      const resizableGetter = Object.getOwnPropertyDescriptor(
        Native.prototype,
        "resizable",
      ).get;
      Object.defineProperty(Native.prototype, "transfer", {
        value: function transfer(...args) {
          if (apply(resizableGetter, this, [])) {
            throw new NativeTypeError(
              `${name}.prototype.transfer of a resizable buffer is disabled in this sandbox`,
            );
          }
          // Uses the captured `apply`, not `Reflect.apply` (see the top of
          // this script): `Reflect.apply` at call time would hand
          // `nativeTransfer` to whatever function user code had replaced
          // `Reflect.apply` with.
          return apply(nativeTransfer, this, args);
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

/// Isolate creation limits, plus the wall-clock budget, for a single
/// handler invocation.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Wall-clock budget for the whole invocation. [`invoke_handler_with_watchdog`]
    /// enforces this with a watchdog `std::thread` that survives a
    /// synchronous, never-yielding handler body -- not just a
    /// `tokio::time::timeout` around the `.await` chain, which cannot
    /// preempt one (see that function's doc comment). Also the source of
    /// the absolute deadline `op_apsides_fetch` derives its own "total
    /// time <= timeout" budget from.
    pub timeout: Duration,
    /// V8 heap size (young + old generation) at which the near-heap-limit
    /// callback fires and terminates the isolate.
    pub max_heap_bytes: usize,
    /// Hard cap on outstanding V8 `ArrayBuffer` backing-store bytes for this
    /// isolate, enforced by the allocator in `v8_alloc`. A breach currently
    /// terminates the whole isolate rather than surfacing a catchable JS
    /// exception -- see the doc comment on
    /// `array_buffer_allocation_over_the_cap_terminates_the_isolate_without_aborting_the_process`
    /// in `tests/spike.rs` for why. Does not by itself cover resizable
    /// `ArrayBuffer`s or `Deno.core.deserialize`/`structuredClone` (see
    /// `BOOTSTRAP_GUARD_SCRIPT`) or op-created backing stores (see
    /// `v8_alloc.rs`'s module doc comment). Not named in the Phase 5b
    /// plan's own `Limits` sketch, but kept as a field here (not a fixed
    /// constant) since the capped allocator this crate inherited from the
    /// Phase 5a spike needs a byte cap to construct at all.
    pub max_array_buffer_bytes: usize,
}

/// Egress policy for `apsides.fetch`.
#[derive(Debug, Clone, Default)]
pub struct EgressPolicy {
    /// `(host, port)` pairs a request's URL host:port must match exactly
    /// before `op_apsides_fetch` will even resolve it.
    pub allowed: Vec<(String, u16)>,
    /// When set, replaces the real `hickory-resolver` DNS lookup with this
    /// fixed `host -> addr` map -- used by tests to point a request at a
    /// local fixture server, or to exercise the "private address from DNS"
    /// denial deterministically, without needing real DNS or (that is a
    /// *separate* gate; see [`FixtureEgressOverride`]) an extra trusted TLS
    /// certificate authority.
    pub resolver_override: Option<HashMap<String, IpAddr>>,
    pub fixture_trust_anchor: Option<FixtureEgressOverride>,
}

/// The exact string [`FixtureEgressOverride::acknowledgement`] must equal
/// for its extra TLS trust anchor to ever be used.
///
/// Mirrors the `FIXTURE_ACKNOWLEDGEMENT` pattern in `runtime/src/lib.rs`: a
/// single, grep-able constant gating a deliberately dangerous capability,
/// so it can never be reached by a default, empty, or merely-similar
/// value.
pub const FIXTURE_ACKNOWLEDGEMENT: &str = "disposable-cluster-only";

/// An explicit, separately-gated extra TLS trust anchor for
/// `op_apsides_fetch`'s TLS handshake, on top of the compiled-in
/// `webpki-roots` set.
///
/// **Not** enabled merely by setting `EgressPolicy::resolver_override`. A
/// fixture-mode DNS override alone is safe on its own: it only changes
/// which address a request goes to, still subject to every other check,
/// including ordinary public-CA TLS verification against that address.
/// Trusting an *extra, caller-supplied* certificate authority is a
/// materially more dangerous capability -- it lets a request succeed
/// against a TLS endpoint no public CA vouches for -- so it is gated
/// behind `acknowledgement` matching [`FIXTURE_ACKNOWLEDGEMENT`] exactly.
/// Every `invoke_handler*` function refuses to build an isolate at all
/// (returning [`InvokeError::FixtureAcknowledgementMissing`]) if
/// `acknowledgement` is set to anything else, including an empty string or
/// a close-but-wrong value.
#[derive(Debug, Clone)]
pub struct FixtureEgressOverride {
    pub acknowledgement: String,
    /// DER-encoded self-signed test CA certificate to trust for this
    /// invocation's `apsides.fetch` calls only.
    pub trust_anchor_der: Vec<u8>,
}

/// One output this invocation may write via `apsides.writeOutput(id, key,
/// data)`.
#[derive(Debug, Clone)]
pub struct OutputDeclaration {
    pub id: String,
    pub max_bytes: usize,
}

/// A human-facing diagnostic from [`OutputSink::write_output`].
///
/// Carries only *why the sink itself* rejected an otherwise-valid write
/// (id declared, key valid, size within cap) -- e.g. a transient store
/// failure -- surfaced to the handler as `Error.message` with
/// `.code === "SINK_REJECTED"`.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct OutputSinkError {
    pub message: String,
}

/// Implemented by the caller to actually persist an `apsides.writeOutput`
/// call this crate has already validated.
///
/// Validation already done by the time this is called: declared output
/// id, key matching `[-._a-zA-Z0-9]+`, data within the declared
/// `max_bytes`. Kept as a trait, not a concrete type, so this crate never
/// touches Kubernetes (or any other storage) directly -- see the crate doc
/// comment's IR-agnostic goal.
///
/// This crate does **not** implement unchanged-data skipping (skip a write
/// whose `data` is byte-identical to what is already stored for
/// `(id, key)`) or a minimum-write-interval throttle: both are policy
/// decisions about *when a write is worth persisting*, which depends on
/// what the sink is backed by (a `ConfigMap` with its own revision-churn
/// cost, a database row, ...) in a way this crate has no visibility into.
/// A caller that wants either behavior implements it inside its own
/// `write_output`.
///
/// Called synchronously, on the isolate's own thread, for every
/// `apsides.writeOutput` call that passes this crate's own validation --
/// an implementation that needs to do I/O should keep it fast (comparable
/// to a local write) or hand off to its own background task and return
/// quickly, since this blocks the isolate (and thus the whole invocation's
/// wall-clock budget) until it returns.
pub trait OutputSink: Send + Sync {
    /// # Errors
    ///
    /// Returns `Err` if the sink itself rejects an otherwise-valid write.
    /// Never called for a write this crate's own validation already
    /// rejected.
    fn write_output(&self, id: &str, key: &str, data: &[u8]) -> Result<(), OutputSinkError>;
}

/// One handler invocation.
///
/// The ESM `bundle` source, the `export`ed async function to call with
/// `event`, resource `limits`, the `egress` policy `apsides.fetch` is
/// checked against, and the `outputs` this invocation may write via
/// `apsides.writeOutput` (persisted through `sink`).
pub struct HandlerInvocation<'a> {
    pub bundle: &'a str,
    pub export: &'a str,
    pub event: serde_json::Value,
    pub limits: Limits,
    pub egress: EgressPolicy,
    pub outputs: Vec<OutputDeclaration>,
    pub sink: Arc<dyn OutputSink>,
}

/// A completed handler invocation's result.
///
/// The JSON value the handler's exported function resolved with, and
/// every line captured by `apsides.log` (already capped at invocation
/// time -- 64 lines of at most 4 KiB each; see `ops.rs`).
#[derive(Debug, Clone)]
pub struct HandlerOutput {
    pub value: serde_json::Value,
    pub logs: Vec<String>,
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
    #[error("failed to read the module namespace: {0}")]
    Namespace(#[source] deno_core::error::CoreError),
    #[error("handler call failed: {0}")]
    Call(#[source] deno_core::error::CoreError),
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
            Self::Evaluate(err) | Self::EventLoop(err) | Self::Namespace(err) | Self::Call(err) => {
                err
            }
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
/// knows `bundle` (see `StaticModuleLoader`'s docs -- no filesystem access,
/// no other module reachable), and `BOOTSTRAP_GUARD_SCRIPT` has already
/// run. Produced only by [`prepare_isolate`].
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
/// isolate with a [`StaticModuleLoader`] that serves only `bundle` under a
/// fixed specifier and nothing else (no `FsModuleLoader`: that would let a
/// static or dynamic `import` in `bundle` read arbitrary files from disk,
/// independently of any OS-level lockdown), registers `ops::apsides_ops`
/// with `egress`/`outputs`/`sink`/`deadline` as its per-invocation
/// `OpState` (see `ops::EngineOpState`), and runs
/// [`BOOTSTRAP_GUARD_SCRIPT`] before returning.
#[allow(clippy::too_many_arguments, reason = "each argument is a distinct, independently-owned piece of one invocation's isolate configuration; bundling them into a struct here would only move the same field list one level up")]
fn prepare_isolate(
    limits: Limits,
    bundle: &str,
    usage: Arc<AtomicUsize>,
    egress: EgressPolicy,
    outputs: HashMap<String, usize>,
    sink: Arc<dyn OutputSink>,
    deadline: Instant,
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

    let specifier = handler_module_specifier()?;
    // `bundle.to_owned()`, not the borrowed `&str` directly: a real
    // invocation's bundle is not `'static` (it comes from storage/network
    // at request time), and `StaticModuleLoader` accepts an owned `String`
    // (`IntoModuleCodeString`) just as well as a `&'static str`, at the
    // cost of one copy per invocation.
    let module_loader = StaticModuleLoader::with(specifier.clone(), bundle.to_owned());

    let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(Rc::new(module_loader)),
        extensions: vec![ops::apsides_ops::init(egress, outputs, sink, deadline)],
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

fn handler_module_specifier() -> Result<ModuleSpecifier, EngineError> {
    ModuleSpecifier::parse("file:///apsides-engine/handler.js").map_err(EngineError::Specifier)
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
/// `Limits::max_heap_bytes`.
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
/// for Phase 5b, not fixed here -- see `Limits::max_array_buffer_bytes`'s
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
/// [`StaticModuleLoader`]), evaluates it, drives the event loop to
/// completion, and returns the loaded module's id (for
/// [`call_handler_export`] to look its namespace up afterward).
#[allow(
    clippy::future_not_send,
    reason = "deno_core::JsRuntime is deliberately !Send; see the crate's `invoke_handler` doc comment"
)]
async fn load_and_evaluate(
    runtime: &mut JsRuntime,
    specifier: &ModuleSpecifier,
) -> Result<deno_core::ModuleId, EngineError> {
    let module_id = runtime
        .load_main_es_module(specifier)
        .await
        .map_err(EngineError::Load)?;
    let evaluated = runtime.mod_evaluate(module_id);
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .map_err(EngineError::EventLoop)?;
    evaluated.await.map_err(EngineError::Evaluate)?;
    Ok(module_id)
}

/// Looks up `export` on `module_id`'s namespace object, calls it with
/// `event` as its one argument, awaits its result (running the event loop
/// concurrently via `with_event_loop_promise`, so a `.then`/`await` inside
/// the handler, or an in-flight `apsides.fetch`, can make progress), and
/// marshals the resolved value back to `serde_json::Value`.
#[allow(
    clippy::future_not_send,
    reason = "deno_core::JsRuntime is deliberately !Send; see the crate's `invoke_handler` doc comment"
)]
async fn call_handler_export(
    runtime: &mut JsRuntime,
    module_id: deno_core::ModuleId,
    export: &str,
    event: &serde_json::Value,
) -> Result<serde_json::Value, InvokeError> {
    let namespace = runtime
        .get_module_namespace(module_id)
        .map_err(EngineError::Namespace)?;

    let (func_global, event_global) = {
        deno_core::scope!(scope, runtime);
        let namespace_local = v8::Local::new(scope, namespace);
        let key = v8::String::new(scope, export)
            .ok_or_else(|| InvokeError::ExportNotFound(export.to_owned()))?;
        let value = namespace_local
            .get(scope, key.into())
            .ok_or_else(|| InvokeError::ExportNotFound(export.to_owned()))?;
        let func = v8::Local::<v8::Function>::try_from(value)
            .map_err(|_conversion_err| InvokeError::ExportNotCallable(export.to_owned()))?;
        let func_global = v8::Global::new(scope, func);

        let event_local = deno_core::serde_v8::to_v8(scope, event)
            .map_err(|err| InvokeError::EventMarshal(err.to_string()))?;
        let event_global = v8::Global::new(scope, event_local);
        (func_global, event_global)
    };

    let call = runtime.call_with_args(&func_global, &[event_global]);
    let result_global = runtime
        .with_event_loop_promise(call, deno_core::PollEventLoopOptions::default())
        .await
        .map_err(EngineError::Call)?;

    deno_core::scope!(scope, runtime);
    let result_local = v8::Local::new(scope, result_global);
    deno_core::serde_v8::from_v8::<serde_json::Value>(scope, result_local)
        .map_err(|err| InvokeError::ResultMarshal(err.to_string()))
}

/// Refuses to proceed if `egress` carries a [`FixtureEgressOverride`] whose
/// `acknowledgement` is not exactly [`FIXTURE_ACKNOWLEDGEMENT`] -- checked
/// before any isolate is built, so a missing/wrong acknowledgement can
/// never even momentarily install the extra trust anchor.
fn validate_fixture_acknowledgement(egress: &EgressPolicy) -> Result<(), InvokeError> {
    match &egress.fixture_trust_anchor {
        Some(fixture) if fixture.acknowledgement == FIXTURE_ACKNOWLEDGEMENT => Ok(()),
        Some(_wrong_acknowledgement) => Err(InvokeError::FixtureAcknowledgementMissing),
        None => Ok(()),
    }
}

fn output_caps(outputs: &[OutputDeclaration]) -> HashMap<String, usize> {
    outputs
        .iter()
        .map(|decl| (decl.id.clone(), decl.max_bytes))
        .collect()
}

/// Runs one [`HandlerInvocation`] to completion in a fresh isolate.
///
/// Builds it (see [`prepare_isolate`]), loads/evaluates `invocation.bundle`
/// as the main module, calls its `invocation.export`ed function with
/// `invocation.event`, and returns its resolved value plus every
/// `apsides.log` line the invocation captured.
///
/// Does **not** itself enforce `invocation.limits.timeout` against a
/// synchronous, never-yielding handler body -- see
/// [`invoke_handler_with_watchdog`] for the mechanism that does. The
/// caller's own tokio runtime must have its network/IO driver enabled
/// (`enable_io()`/`enable_all()`), not just `enable_time()`: a handler that
/// calls `apsides.fetch` needs it.
///
/// `usage` reports the capped `ArrayBuffer` allocator's live byte usage
/// (see `v8_alloc::capped_array_buffer_allocator`); pass a fresh
/// `Arc::new(AtomicUsize::new(0))` if the caller does not need to inspect
/// it.
///
/// # Errors
///
/// Returns `Err` if `invocation.egress.fixture_trust_anchor` is set without
/// the required acknowledgement, if the module specifier cannot be parsed
/// (never, in practice: it is a fixed literal), if the bootstrap guard
/// script fails, if the bundle fails to load/evaluate, if the event loop
/// reports a failure (including a heap-limit or array-buffer-cap
/// termination -- see [`EngineError::is_isolate_termination`]), if
/// `invocation.export` does not name a callable export, or if the event/
/// result value fails to marshal across the V8 boundary.
///
/// This function's returned future is not `Send`: `deno_core::JsRuntime`
/// (and the V8 isolate it owns) is deliberately single-threaded and must
/// only ever be driven from the one current-thread tokio runtime that owns
/// it, matching the "one fresh isolate per invocation, never shared across
/// threads" model.
#[allow(
    clippy::future_not_send,
    reason = "deno_core::JsRuntime is deliberately !Send; see the doc comment above"
)]
pub async fn invoke_handler(
    invocation: HandlerInvocation<'_>,
    usage: &Arc<AtomicUsize>,
) -> Result<HandlerOutput, InvokeError> {
    validate_fixture_acknowledgement(&invocation.egress)?;
    let deadline = Instant::now() + invocation.limits.timeout;
    let outputs = output_caps(&invocation.outputs);

    let PreparedIsolate {
        mut runtime,
        isolate_handle,
        specifier,
    } = prepare_isolate(
        invocation.limits,
        invocation.bundle,
        Arc::clone(usage),
        invocation.egress,
        outputs,
        invocation.sink,
        deadline,
    )?;

    // Captured before installing the near-heap-limit callback, and moved
    // into it, so the callback can request termination of *this* isolate
    // from wherever V8 invokes it (documented as callable from a GC pause
    // on the isolate's own thread) without needing `&mut JsRuntime`, which
    // the callback signature does not provide.
    install_heap_limit_callback(&mut runtime, isolate_handle);

    let module_id = load_and_evaluate(&mut runtime, &specifier).await?;
    let value =
        call_handler_export(&mut runtime, module_id, invocation.export, &invocation.event).await?;
    let logs = runtime
        .op_state()
        .borrow()
        .borrow::<ops::EngineOpState>()
        .logs
        .clone();

    Ok(HandlerOutput { value, logs })
}

/// Like [`invoke_handler`], but with a watchdog `std::thread` attached and
/// its own dedicated, current-thread tokio runtime (with the network/IO
/// driver enabled, for `apsides.fetch`).
///
/// Spawns a watchdog `std::thread` that calls
/// `IsolateHandle::terminate_execution()` after `invocation.limits.timeout`,
/// runs the isolate on that dedicated runtime, and blocks the calling
/// thread until it finishes.
///
/// This is the mechanism the plan's wall-clock watchdog (`timeoutMillis`)
/// needs, and [`invoke_handler_blocking`]'s `tokio::time::timeout` is *not*
/// a substitute for it: a `tokio::time::timeout` around an `.await` only
/// ever gets a chance to fire at an await point the awaited future
/// actually yields at. A synchronous, never-yielding handler body (`while
/// (true) {}` at module top level, with no `await` in it at all) runs
/// entirely inside a single, uninterrupted `poll()` of that future --
/// `tokio::time::timeout` cannot preempt it, because the executor never
/// gets control back to check the deadline. `IsolateHandle::terminate_execution()`
/// called from a *different* OS thread works regardless, because V8 checks
/// for a pending termination request at points a tight synchronous loop
/// still reaches (that is its documented purpose). See
/// `tests/spike.rs`'s `watchdog_thread_terminates_a_synchronous_infinite_loop`.
///
/// The watchdog thread does not linger past this call: it blocks on a
/// channel with `invocation.limits.timeout` as the `recv_timeout`
/// deadline instead of a bare `sleep`, so dropping the sending half here
/// (once the invocation finishes, however it finishes) wakes it
/// immediately with `Disconnected` rather than `Timeout`, and it exits
/// without calling `terminate_execution()`. This function joins it before
/// returning either way, so it never returns with the watchdog thread
/// still alive.
///
/// # Errors
///
/// See [`invoke_handler`]'s `# Errors`; additionally returns
/// `Err(InvokeError::Runtime(_))` if the tokio runtime fails to build.
pub fn invoke_handler_with_watchdog(
    invocation: HandlerInvocation<'_>,
    usage: &Arc<AtomicUsize>,
) -> Result<HandlerOutput, InvokeError> {
    validate_fixture_acknowledgement(&invocation.egress)?;
    let timeout = invocation.limits.timeout;
    let deadline = Instant::now() + timeout;
    let outputs = output_caps(&invocation.outputs);

    let PreparedIsolate {
        mut runtime,
        isolate_handle,
        specifier,
    } = prepare_isolate(
        invocation.limits,
        invocation.bundle,
        Arc::clone(usage),
        invocation.egress,
        outputs,
        invocation.sink,
        deadline,
    )?;

    install_heap_limit_callback(&mut runtime, isolate_handle.clone());

    // Built *before* the watchdog thread is spawned below: if this fails,
    // it returns before any thread but the caller's own exists, so there
    // is nothing to join. `enable_io()`: a handler that calls
    // `apsides.fetch` needs the tokio net driver, not just timers.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
        .map_err(InvokeError::Runtime)?;

    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        // `Timeout`: the deadline passed with the run still going --
        // terminate it. `Disconnected` (the sender was dropped below,
        // meaning the run already finished): exit without touching the
        // isolate. Nothing is ever sent on this channel; it exists only
        // for its `Disconnected` signal.
        if matches!(
            cancel_rx.recv_timeout(timeout),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            isolate_handle.terminate_execution();
        }
    });

    let result = rt.block_on(async {
        let module_id = load_and_evaluate(&mut runtime, &specifier).await?;
        let value =
            call_handler_export(&mut runtime, module_id, invocation.export, &invocation.event)
                .await?;
        let logs = runtime
            .op_state()
            .borrow()
            .borrow::<ops::EngineOpState>()
            .logs
            .clone();
        Ok::<HandlerOutput, InvokeError>(HandlerOutput { value, logs })
    });

    drop(cancel_tx);
    // A panicking watchdog thread (it cannot: the closure above has no
    // fallible/panicking calls) would be the only way `join` returns
    // `Err`; there is nothing more to do with that here than let it drop.
    let _ = watchdog.join();

    result
}

/// Runs [`invoke_handler`] on a fresh single-threaded current-thread tokio
/// runtime (with the network/IO driver enabled) and blocks on it.
///
/// `invocation.limits.timeout` here is a `tokio::time::timeout` around the
/// whole `.await` chain, which only helps between await points (op calls,
/// event loop turns) -- it is **not** a substitute for a real wall-clock
/// watchdog against a synchronous, never-yielding script; see
/// [`invoke_handler_with_watchdog`]'s doc comment for why, and for the
/// mechanism a real deployment needs (a separate OS thread calling
/// `IsolateHandle::terminate_execution()`).
///
/// `usage` is forwarded to [`invoke_handler`]; see its doc comment.
///
/// # Errors
///
/// Returns `Err` if the tokio runtime fails to build, if the invocation
/// does not finish within `invocation.limits.timeout`, or if
/// [`invoke_handler`] itself returns `Err` (see its docs).
pub fn invoke_handler_blocking(
    invocation: HandlerInvocation<'_>,
    usage: &Arc<AtomicUsize>,
) -> Result<HandlerOutput, InvokeError> {
    let timeout = invocation.limits.timeout;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
        .map_err(InvokeError::Runtime)?;
    rt.block_on(async move {
        tokio::time::timeout(timeout, invoke_handler(invocation, usage))
            .await
            .map_err(|_elapsed| InvokeError::Timeout)?
    })
}

#[derive(Debug, Error)]
pub enum InvokeError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("export {0:?} was not found on the module")]
    ExportNotFound(String),
    #[error("export {0:?} is not a callable function")]
    ExportNotCallable(String),
    #[error("failed to marshal the event value into the isolate: {0}")]
    EventMarshal(String),
    #[error("failed to read the handler's return value: {0}")]
    ResultMarshal(String),
    #[error(
        "egress.fixture_trust_anchor was set without acknowledgement == FIXTURE_ACKNOWLEDGEMENT"
    )]
    FixtureAcknowledgementMissing,
    #[error("failed to build tokio runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("handler invocation timed out")]
    Timeout,
}

impl InvokeError {
    /// See [`EngineError::is_isolate_termination`].
    #[must_use]
    pub fn is_isolate_termination(&self) -> bool {
        matches!(self, Self::Engine(err) if err.is_isolate_termination())
    }
}
