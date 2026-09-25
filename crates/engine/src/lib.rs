//! The Apsides handler engine (see the Apsides v0.3 plan, "Phase 5"):
//! embeds a pinned `deno_core`/V8 to run one untrusted ES module handler
//! per invocation, in a fresh isolate, with no filesystem access and no
//! network access except through the three typed, capability-checked ops
//! in [`ops`]. Grew out of the Phase 5a spike gate, which proved the
//! embedding itself was feasible; this is now the real Phase 5b engine
//! API ([`invoke_handler`] and friends), IR-agnostic: `runtime`'s
//! `handlers` module maps each IR handler onto an invocation and drives it
//! through [`invoke_handler_with_watchdog`] in production mode.
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
//! bootstrap guard script all being applied. It also always requires a
//! `&tokio::runtime::Handle` and enters it before constructing the
//! isolate (review round 6, X1): `deno_core` needs a tokio runtime
//! already present to register the isolate against, not merely one built
//! later, or a V8-internal foreground task posted during real execution
//! (e.g. the GC memory reducer, under allocation churn) has nothing to
//! post itself onto and aborts the whole process. Every `invoke_handler*`
//! entry point therefore builds (or obtains) its runtime *before* calling
//! [`begin_invocation`], never after.

mod lockdown;
mod net;
mod ops;
#[cfg(feature = "test-util")]
pub mod test_support;
mod v8_alloc;

pub use lockdown::{
    LockdownError, LockdownOutcome, apply_deny_all_or_fail_closed, count_process_threads,
};
pub use net::{EgressResolver, ResolveError, resolver_config_from_resolv_conf};
/// Mirrored by `compiler_core::HANDLER_FETCH_REQUEST_BODY_BYTES` for the
/// controller-pod memory formula -- see that constant's doc comment. A
/// cross-crate test in `runtime` (which depends on both crates) asserts
/// the two stay equal.
pub use ops::MAX_FETCH_REQUEST_BODY_BYTES;

use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use deno_core::v8;
use deno_core::{JsRuntime, ModuleSpecifier, RuntimeOptions, StaticModuleLoader};
use serde::Serialize;
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
  // Every one of these is captured here, at the very top of the script,
  // for the same reason `construct`/`apply`/`NativeTypeError` are: the
  // `apsides.*` wrapper functions below persist past bootstrap and must
  // never perform a fresh lookup of a global/built-in a handler could have
  // already replaced by the time they run -- see the invariant above. The
  // specific guarantee this buys, and the only one it needs to: no
  // privileged value (an `op_apsides_*` function reference, or the native,
  // unguarded `ArrayBuffer` constructor Layer 2 hides) is ever handed to
  // code the handler could have replaced. Capturing every built-in these
  // wrappers touch (not just the ones with an obvious leak shape) is
  // defense in depth for that one guarantee, not a claim that hijacking,
  // say, `Object.prototype.toString`'s tag output can never affect this
  // script's own behavior at all -- see `isUint8Array`'s doc comment for
  // where that line is actually drawn, and
  // `apsides_wrappers_are_robust_to_prototype_and_symbol_hijacking_of_uint8array_handling`
  // in `tests/spike.rs` for what is and is not asserted about it.
  const NativeJSONStringify = JSON.stringify;
  const NativeUint8Array = Uint8Array;
  const nativeObjectEntries = Object.entries;
  const nativeObjectToString = Object.prototype.toString;
  const nativeCodePointAt = String.prototype.codePointAt;

  // Layer 1: hide deno_core's own internals. Captures only the three op
  // functions this engine's own `apsides` global needs, before deleting
  // every other route to any of them (or to anything else under `Deno`).
  const opFetch = globalThis.Deno.core.ops.op_apsides_fetch;
  const opOutputWrite = globalThis.Deno.core.ops.op_apsides_output_write;
  const opLog = globalThis.Deno.core.ops.op_apsides_log;

  // Avoids `value instanceof NativeUint8Array`, which consults
  // `NativeUint8Array[Symbol.hasInstance]` -- itself an ordinary,
  // reassignable property -- at every call, not just once at bootstrap.
  // `Object.prototype.toString`'s `[object <Tag>]` behavior does not
  // consult `Symbol.hasInstance`, but per spec it *does* consult the
  // argument's own `Symbol.toStringTag` before falling back to its
  // built-in tag, so `Uint8Array.prototype[Symbol.toStringTag]` being
  // reassigned can still make this misclassify a real `Uint8Array` (or
  // classify a look-alike object as one). That is an accepted, bounded
  // gap, not a guard defeat: a misclassification only changes which
  // branch of `toBytes` runs (a `TypeError`, or treating a non-buffer as
  // one) -- it never exposes `opFetch`/`opOutputWrite`/`opLog`, and the
  // actual `#[buffer]` marshalling at the Rust/V8 boundary (`ops.rs`)
  // independently validates the real V8 object kind regardless of what
  // this check, or `Object.prototype.toString`, concluded -- a value that
  // is not truly a typed array can never reach the ops as bytes no matter
  // what this function returns.
  function isUint8Array(value) {
    return apply(nativeObjectToString, value, []) === "[object Uint8Array]";
  }

  // A minimal, self-contained UTF-8 encoder: bare deno_core has no
  // TextEncoder (that is a `deno_web` extension, not registered here), and
  // this must stay usable from a closure that persists past bootstrap, so
  // it touches nothing but its own arguments/locals and the captured
  // values above. Two passes (`utf8ByteLength` then this) so the result
  // can be written directly into a preallocated `Uint8Array` via integer-
  // indexed `[[Set]]` -- unlike a plain `Array`'s indexed assignment, a
  // typed array's integer-indexed exotic `[[Set]]` for an in-range index
  // writes straight to the backing buffer and never consults the
  // prototype chain, so it cannot be intercepted by an
  // `Object.defineProperty(Array.prototype, "0", { set() {...} })`-style
  // hijack the way building a plain `Array` first (via index assignment,
  // then `Uint8Array.from`) could have been -- `Array.prototype.from`'s
  // iterable path also consults `Symbol.iterator`, another hijack surface
  // this sidesteps entirely by never producing an intermediate `Array`.
  function utf8ByteLength(text) {
    let length = 0;
    for (let i = 0; i < text.length; i++) {
      const code = apply(nativeCodePointAt, text, [i]);
      if (code > 0xffff) {
        i++;
      }
      if (code < 0x80) {
        length += 1;
      } else if (code < 0x800) {
        length += 2;
      } else if (code < 0x10000) {
        length += 3;
      } else {
        length += 4;
      }
    }
    return length;
  }

  function utf8Encode(text) {
    const result = new NativeUint8Array(utf8ByteLength(text));
    let offset = 0;
    for (let i = 0; i < text.length; i++) {
      let code = apply(nativeCodePointAt, text, [i]);
      if (code > 0xffff) {
        i++;
      }
      if (code < 0x80) {
        result[offset++] = code;
      } else if (code < 0x800) {
        result[offset++] = 0xc0 | (code >> 6);
        result[offset++] = 0x80 | (code & 0x3f);
      } else if (code < 0x10000) {
        result[offset++] = 0xe0 | (code >> 12);
        result[offset++] = 0x80 | ((code >> 6) & 0x3f);
        result[offset++] = 0x80 | (code & 0x3f);
      } else {
        result[offset++] = 0xf0 | (code >> 18);
        result[offset++] = 0x80 | ((code >> 12) & 0x3f);
        result[offset++] = 0x80 | ((code >> 6) & 0x3f);
        result[offset++] = 0x80 | (code & 0x3f);
      }
    }
    return result;
  }

  function toBytes(input) {
    if (input === undefined || input === null) {
      return new NativeUint8Array(0);
    }
    if (isUint8Array(input)) {
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

  function isValidLogLevel(level) {
    return (
      level === "debug" || level === "info" || level === "warn" || level === "error"
    );
  }

  const apsides = Object.freeze({
    fetch: async (url, options) => {
      const opts = options || {};
      const method = opts.method || "GET";
      const headers = opts.headers ? nativeObjectEntries(opts.headers) : [];
      const bodyBytes = toBytes(opts.body);
      const outcome = await opFetch({ url, method, headers }, bodyBytes);
      throwIfError(outcome);
      // `outcome.body` already arrives as a real `Uint8Array` (the op
      // returns it as a `deno_core::ToJsBuffer`, not a plain JSON number
      // array), so no conversion is needed here.
      return {
        status: outcome.status,
        headers: outcome.headers,
        body: outcome.body,
      };
    },
    /**
     * Writes `key` of declared output `id`. `data` must be a `string` or a
     * `Uint8Array` of **valid UTF-8 text** -- the value lands in the output
     * ConfigMap's `data` field (never `binaryData`), and Kubernetes
     * ConfigMap `data` values are themselves required to be UTF-8, so a
     * `data` argument that is not valid UTF-8 throws synchronously here,
     * before anything is buffered or written. Buffered in memory for the
     * rest of this invocation and committed to Kubernetes only after the
     * handler returns successfully, as one server-side apply per declared
     * output (see IR-CONTRACT.md, "Handlers and outputs").
     *
     * Contract per output, across invocations: the keys written in *this*
     * invocation (if it writes to this output at all) replace all of this
     * handler's own keys in it. Keys another field manager owns -- another
     * handler's output, or a key set by `kubectl apply`/`kubectl edit` --
     * are never touched and always survive. An invocation that never calls
     * `writeOutput` for this output leaves this handler's own keys in it
     * unchanged; a handler can therefore never empty an output simply by
     * not writing to it this invocation.
     * @param {string} id a declared output id
     * @param {string} key `[-._a-zA-Z0-9]+`, at most 253 characters, never
     *   exactly `.` or `..`
     * @param {string | Uint8Array} data valid UTF-8 text
     * @throws {Error} with `.code` `UNDECLARED_OUTPUT_ID`, `INVALID_KEY`,
     *   `DATA_TOO_LARGE`, `OPS_CLOSED` (called after the handler returned),
     *   or `SINK_REJECTED` (the controller's sink refuses `data` that is not
     *   valid UTF-8)
     */
    writeOutput: (id, key, data) => {
      const outcome = opOutputWrite(id, key, toBytes(data));
      throwIfError(outcome);
    },
    log: (level, message) => {
      if (!isValidLogLevel(level)) {
        throw new NativeTypeError(
          `apsides.log level must be one of "debug", "info", "warn", "error"; got ${level}`,
        );
      }
      const text = typeof message === "string" ? message : NativeJSONStringify(message);
      opLog(level, text);
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

/// How many times V8 has actually invoked the entropy source
/// [`ensure_platform_initialized`] installs. Exists only so a test can
/// assert V8 genuinely called it (see `tests/entropy_source.rs`) --
/// production code never reads this.
static ENTROPY_SOURCE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// How many times V8 has invoked the entropy source installed by
/// [`ensure_platform_initialized`].
///
/// `0` before that function has ever run (or if V8 never actually asked
/// for entropy, which would itself be surprising -- every isolate seeds
/// its own RNG at creation).
#[must_use]
pub fn entropy_source_invocation_count() -> usize {
    ENTROPY_SOURCE_CALLS.load(Ordering::Relaxed)
}

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
        // Under a Landlock lockdown (see the Apsides v0.3 plan, Phase 5c),
        // `/dev/urandom` is denied like every other path. Without an
        // explicit entropy source, V8 silently falls back to a time-based
        // seed for the platform RNG (used for, among other things, mmap/ASLR
        // hints) and for each isolate's own RNG (`Math.random` and the
        // internal string-hash seed -- a predictable hash seed reopens hash
        // flooding through attacker-influenced input, e.g. an
        // `apsides.fetch` response header/body the handler then indexes
        // into an object). `getrandom::fill` on Linux uses the
        // `getrandom(2)` syscall directly (confirmed against this pinned
        // version's own backend selection: `x86_64`/`aarch64`-class targets
        // route through `linux_android_with_fallback`, which calls the
        // syscall first and only touches `/dev/urandom` if the syscall
        // itself is unavailable -- ENOSYS on a pre-3.17 kernel, or EPERM
        // under a seccomp filter that blocks it; Landlock is neither), so
        // this needs no file access under lockdown. Set before platform
        // init, per `v8::V8::set_entropy_source`'s own contract.
        v8::V8::set_entropy_source(|buffer| {
            ENTROPY_SOURCE_CALLS.fetch_add(1, Ordering::Relaxed);
            getrandom::fill(buffer).is_ok()
        });
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
    /// Hard cap on the total bytes of every `apsides.fetch` response body
    /// across the *whole invocation* (summed, not per call).
    ///
    /// A response body arrives as a V8 `ArrayBuffer` backing store built
    /// directly by `serde_v8::ToJsBuffer`
    /// (`v8::ArrayBuffer::new_backing_store_from_boxed_slice`), which never
    /// goes through the capped allocator in `v8_alloc.rs` and is not
    /// counted against `max_heap_bytes` either -- an op-created backing
    /// store lives outside the heap the near-heap-limit callback watches.
    /// Without this separate budget, a handler could pull far more memory
    /// into the process than `max_heap_bytes`/`max_array_buffer_bytes`
    /// suggest, simply by calling `apsides.fetch` repeatedly (each
    /// individual call is already capped at 4 MiB by `ops::fetch`, but
    /// nothing else would stop a handler from calling it 1000 times).
    ///
    /// This budget is *reserved* synchronously, before any `.await` point,
    /// the moment `apsides.fetch` is called -- not charged only after the
    /// response finishes -- and concurrent calls each reserve against the
    /// same running total (see `max_concurrent_fetches`). A version that
    /// only charged completed responses let `N` concurrent calls each read
    /// the same "budget still available" snapshot and all proceed,
    /// together pulling `N` times this budget into the process at once
    /// (review round 6, N1).
    ///
    /// The reservation is taken only *after* every check that can fail
    /// for a reason unrelated to available budget (ops already closed, a
    /// malformed method/header, a non-`https` scheme, an IP-literal host,
    /// a host not on the egress allowlist) has already passed, and is
    /// refunded in full if the call then fails for any reason -- a failed
    /// fetch never hands a response buffer to JS, so nothing is gained by
    /// keeping its reservation charged. A version that reserved first, or
    /// kept a failed call's reservation charged, let a burst of e.g.
    /// `HOST_NOT_ALLOWED` calls or connect failures exhaust an
    /// invocation's entire budget without ever admitting a single byte
    /// into the process (review round 6, R1). Only a call that actually
    /// receives a response keeps a charge -- and then only for that
    /// response's real size, not the worst-case allowance it reserved.
    pub max_fetch_bytes_total: usize,
    /// Hard cap on `apsides.fetch` calls in flight at once for this
    /// invocation. Without this, a handler could open unboundedly many
    /// concurrent requests (e.g. `Promise.all` of hundreds of calls to an
    /// address that never responds), each holding open a TCP connection
    /// and a file descriptor until `Limits::timeout`, independent of
    /// `max_fetch_bytes_total` (which caps bytes, not connection/FD
    /// count) -- review round 6, N1.
    pub max_concurrent_fetches: usize,
}

/// Egress policy for `apsides.fetch`.
#[derive(Debug, Clone, Default)]
pub struct EgressPolicy {
    /// `(host, port)` pairs a request's URL host:port must match exactly
    /// before `op_apsides_fetch` will even resolve it. Normalized (each
    /// host lowercased with `to_ascii_lowercase`) and validated (no empty
    /// host) once, by [`EgressPolicy::normalized`], before any isolate is
    /// built -- never left to `ops::fetch` to discover per request, and
    /// never silently mismatching a request's already-lowercase parsed URL
    /// host because the caller wrote a mixed-case entry here.
    pub allowed: Vec<(String, u16)>,
    /// When set, replaces the real `hickory-resolver` DNS lookup with this
    /// fixed `host -> addrs` map (every candidate address for that host,
    /// tried in the given order) -- used by tests to point a request at a
    /// local fixture server, or to exercise the "private address from DNS"
    /// denial deterministically, without needing real DNS or (that is a
    /// *separate* gate; see [`FixtureEgressOverride`]) an extra trusted TLS
    /// certificate authority.
    pub resolver_override: Option<HashMap<String, Vec<IpAddr>>>,
    pub fixture_trust_anchor: Option<FixtureEgressOverride>,
}

impl EgressPolicy {
    /// Lowercases every `allowed` host (DNS names are case-insensitive,
    /// and a request's parsed URL host from the `url` crate is always
    /// already lowercase, so a mixed-case entry here would otherwise
    /// silently never match) and rejects an empty host entry outright.
    /// Called once, by every `invoke_handler*` entry point, before any
    /// isolate is built.
    fn normalized(mut self) -> Result<Self, InvokeError> {
        for (host, _port) in &mut self.allowed {
            if host.trim().is_empty() {
                return Err(InvokeError::InvalidEgressAllowlistEntry(host.clone()));
            }
            *host = host.to_ascii_lowercase();
        }
        Ok(self)
    }
}

/// The exact string [`FixtureEgressOverride::acknowledgement`] must equal
/// for its extra TLS trust anchor to ever be used.
///
/// Mirrors the `FIXTURE_ACKNOWLEDGEMENT` pattern in `runtime/src/lib.rs`: a
/// single, grep-able constant gating a deliberately dangerous capability,
/// so it can never be reached by a default, empty, or merely-similar
/// value.
pub const FIXTURE_ACKNOWLEDGEMENT: &str = "disposable-cluster-only";

/// An explicit, separately-gated bundle of two test-only relaxations for
/// `op_apsides_fetch`.
///
/// An extra TLS trust anchor, on top of the compiled-in `webpki-roots`
/// set, and skipping the "resolved address must be global unicast" check
/// (see `ops::fetch`) -- a local HTTPS test server binds to a loopback
/// address by construction, which that check would otherwise always
/// reject.
///
/// **Not** enabled merely by setting `EgressPolicy::resolver_override`. A
/// fixture-mode DNS override alone is safe on its own: it only changes
/// which address a request goes to, still subject to every other check,
/// including the global-unicast check and ordinary public-CA TLS
/// verification against that address. This bundle is a materially more
/// dangerous capability -- trusting an *extra, caller-supplied*
/// certificate authority lets a request succeed against a TLS endpoint no
/// public CA vouches for, and skipping the global-unicast check lets a
/// request reach a loopback/private address at all -- so both are gated
/// together behind `acknowledgement` matching [`FIXTURE_ACKNOWLEDGEMENT`]
/// exactly. Every `invoke_handler*` function refuses to build an isolate
/// at all (returning [`InvokeError::FixtureAcknowledgementMissing`]) if
/// `acknowledgement` is set to anything else, including an empty string or
/// a close-but-wrong value.
///
/// `crates/runtime`'s production path never constructs a
/// `FixtureEgressOverride` at all (its `production_egress_policy` has no
/// config surface that could supply one), and nothing may ever build one
/// from anything a request or a handler's own event payload could
/// influence -- the same posture as `runtime`'s own
/// `FIXTURE_ACKNOWLEDGEMENT` constant and `allowNoLandlock` escape hatch.
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
/// Validation already done by the time this is called, all before the
/// sink is ever invoked: the output id was declared for this invocation;
/// the key matches `[-._a-zA-Z0-9]+`, is at most 253 characters, and is
/// never exactly `.` or `..`; and the *running total* of `key.len() +
/// data.len()` across every key ever written to this output id in this
/// invocation (this write's own contribution replacing, not adding to,
/// any earlier write of the same key) stays within the output's declared
/// `max_bytes` -- the cap bounds one output's total footprint, not just
/// one call's payload. Kept as a trait, not a concrete type, so this crate
/// never touches Kubernetes (or any other storage) directly -- see the
/// crate doc comment's IR-agnostic goal.
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

/// Where `apsides.log` writes each line.
///
/// Receives each capped, `serde_json`-escaped JSON line from
/// `ops::op_apsides_log`, and the one final "dropped N log lines" record
/// when the per-invocation cap was exceeded (see
/// `EngineOpState::dropped_log_lines`). Defaults to real stderr
/// ([`StderrLogWriter`]); tests inject a recording implementation instead
/// of trying to capture the process's actual stderr.
pub trait LogWriter: Send + Sync {
    fn write_line(&self, line: &str);
}

/// The default [`LogWriter`]: writes each line to the process's real
/// stderr, one line per call.
pub struct StderrLogWriter;

impl LogWriter for StderrLogWriter {
    fn write_line(&self, line: &str) {
        eprintln!("{line}");
    }
}

/// One handler invocation.
///
/// The ESM `bundle` source, the `export`ed async function to call with
/// `event`, resource `limits`, the `egress` policy `apsides.fetch` is
/// checked against (and the `resolver` it dials through), and the
/// `outputs` this invocation may write via `apsides.writeOutput`
/// (persisted through `sink`).
pub struct HandlerInvocation<'a> {
    pub bundle: &'a str,
    pub export: &'a str,
    pub event: serde_json::Value,
    pub limits: Limits,
    pub egress: EgressPolicy,
    pub outputs: Vec<OutputDeclaration>,
    pub sink: Arc<dyn OutputSink>,
    /// The shared, built-once DNS resolver `apsides.fetch` dials through
    /// -- see [`EgressResolver::new`]'s doc comment for why this must be
    /// built once by the caller (before any Landlock lockdown) and reused
    /// across every invocation, never rebuilt per call. `None` is only
    /// valid when every host `egress.allowed` lists is also covered by
    /// `egress.resolver_override` (fixture/test mode); a real deployment
    /// always sets this.
    pub resolver: Option<Arc<EgressResolver>>,
    /// A caller-supplied identifier for the handler this invocation runs,
    /// attached (with `invocation_id`) to every `apsides.log` line this
    /// crate emits via `log_writer` (see [`LogLine`]) so multiple
    /// handlers' and invocations' interleaved log lines can be told apart.
    pub handler_id: &'a str,
    pub invocation_id: &'a str,
    /// Where `apsides.log` lines are written -- see [`LogWriter`]. Pass
    /// `Arc::new(StderrLogWriter)` for real stderr.
    pub log_writer: Arc<dyn LogWriter>,
}

/// One `apsides.log` line: the level the handler passed, and the message
/// (already capped at 4 KiB, at a UTF-8 char boundary; see `ops.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub message: String,
}

/// The outcome of one [`HandlerInvocation`], from
/// [`invoke_handler_with_watchdog`].
///
/// `result` is `Ok` with the handler's resolved JSON value, or `Err` with
/// why the invocation failed. Either way, `logs` carries every
/// `apsides.log` line the invocation captured before it stopped (already
/// capped -- see `ops.rs`): a handler that logs its own diagnosis right
/// before throwing must not lose that diagnosis just because the
/// invocation itself failed. `logs` is empty only when the invocation
/// failed before any isolate was ever built (an invalid `egress.allowed`
/// entry, or a missing/wrong fixture acknowledgement).
#[derive(Debug)]
pub struct InvocationReport {
    pub result: Result<serde_json::Value, InvokeError>,
    pub logs: Vec<LogLine>,
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
///
/// Takes `handle: &tokio::runtime::Handle` and enters it for the whole
/// function body as structural, not just documented, proof that a tokio
/// runtime exists at `JsRuntime::new` time (review round 6, X1): without
/// an entered runtime, `deno_core` registers the isolate with no `Handle`
/// to post tasks onto, and a *later* V8-internal foreground task (e.g. the
/// GC memory reducer, under real allocation churn) reaching
/// `spawn_delayed_task` with nothing to spawn *onto* is documented to call
/// `abort()`, killing the whole controller process -- not a catchable JS
/// exception, not an isolate termination this crate could recover from.
/// Requiring the handle here, rather than leaving "call this only from
/// inside an entered runtime" as a doc-comment convention, means no future
/// call site can silently reintroduce the bug: there is no `Handle` to
/// pass without first having a runtime to get one from.
#[allow(
    clippy::too_many_arguments,
    reason = "each argument is a distinct, independently-owned piece of one invocation's isolate configuration; bundling them into a struct here would only move the same field list one level up"
)]
fn prepare_isolate(
    handle: &tokio::runtime::Handle,
    limits: Limits,
    bundle: &str,
    usage: Arc<AtomicUsize>,
    egress: EgressPolicy,
    outputs: HashMap<String, usize>,
    sink: Arc<dyn OutputSink>,
    resolver: Option<Arc<EgressResolver>>,
    deadline: Instant,
    handler_id: String,
    invocation_id: String,
    log_writer: Arc<dyn LogWriter>,
) -> Result<PreparedIsolate, EngineError> {
    ensure_platform_initialized();
    // See the doc comment above: entered for the rest of this function,
    // covering `JsRuntime::new` and the bootstrap script it then runs.
    let _runtime_guard = handle.enter();

    let fetch_budget = ops::FetchBudget {
        deadline,
        max_fetch_bytes_total: limits.max_fetch_bytes_total,
        max_concurrent_fetches: limits.max_concurrent_fetches,
    };
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
        extensions: vec![ops::apsides_ops::init(
            egress,
            outputs,
            sink,
            resolver,
            fetch_budget,
            ops::InvocationIds {
                handler_id,
                invocation_id,
            },
            log_writer,
        )],
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

/// Bytes of heap-limit headroom granted per call.
///
/// Granted by [`install_heap_limit_callback`], up to
/// [`HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES`] in total.
pub const HEAP_LIMIT_TERMINATION_HEADROOM_STEP_BYTES: usize = 8 * 1024 * 1024;

/// Hard total cap on cumulative heap-limit-termination headroom.
///
/// [`install_heap_limit_callback`] will ever grant one isolate, across
/// every call. Bounds how long termination can be deferred while still
/// giving V8 a few chances (not just one) to unwind cleanly; see that
/// function's doc comment for why one grant was found, in review, not
/// always to be enough.
pub const HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES: usize =
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
/// `terminated` is set `true` the moment this callback first calls
/// `terminate_execution()` -- see [`InvokeError::Terminated`] for why this
/// crate tracks that itself rather than only inferring it from the error
/// V8 surfaces afterward: a termination that interrupts a *call to the
/// handler's exported async function* (as opposed to top-level module
/// evaluation) does not always come back as `deno_core`'s usual "execution
/// terminated" `CoreErrorKind::Js` -- it can instead leave the handler's
/// returned promise permanently unsettled, which `with_event_loop_promise`
/// reports as `CoreErrorKind::PendingPromiseResolution` once the event loop
/// otherwise goes idle. Both shapes mean the same thing operationally (the
/// isolate was terminated by *this callback*), so this flag is the
/// authoritative signal, not the error's shape.
fn install_heap_limit_callback(
    runtime: &mut JsRuntime,
    isolate_handle: v8::IsolateHandle,
    terminated: Arc<AtomicBool>,
) {
    let mut headroom_granted_bytes: usize = 0;
    runtime.add_near_heap_limit_callback(move |current, initial| {
        terminated.store(true, Ordering::SeqCst);
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

    // Closed *before* marshalling the return value, not after: a handler
    // can return an object with a getter (or a `toJSON` method) that runs
    // arbitrary code, including another `apsides.fetch`/`writeOutput`
    // call, during `serde_v8::from_v8`'s property walk below -- i.e.
    // *after* the handler's own promise has already resolved. Every op
    // checks this flag and fails fast with a typed "ops closed" error
    // rather than doing any real work once it is set. See
    // `tests/ops.rs`'s `ops_are_closed_before_the_return_value_is_marshalled`.
    runtime
        .op_state()
        .borrow_mut()
        .borrow_mut::<ops::EngineOpState>()
        .closed = true;

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

/// Loads/evaluates `specifier` and calls `export(event)`. Does not itself
/// collect `apsides.log` lines -- [`collect_logs`] does that separately,
/// from `runtime`'s still-alive `OpState`, after this returns (`Ok` or
/// `Err`), so a handler's diagnostic logging right before it throws is
/// never lost.
#[allow(
    clippy::future_not_send,
    reason = "deno_core::JsRuntime is deliberately !Send; see the crate's `invoke_handler_with_watchdog` doc comment"
)]
async fn run_invocation(
    runtime: &mut JsRuntime,
    specifier: &ModuleSpecifier,
    export: &str,
    event: &serde_json::Value,
) -> Result<serde_json::Value, InvokeError> {
    let module_id = load_and_evaluate(runtime, specifier).await?;
    call_handler_export(runtime, module_id, export, event).await
}

/// Called once, after `run_invocation` returns (`Ok` or `Err`): emits the
/// final "dropped N log lines" record (if any lines were dropped for
/// exceeding the per-invocation cap -- see `ops::EngineOpState`) and
/// returns every captured line.
fn collect_logs(runtime: &JsRuntime) -> Vec<LogLine> {
    let op_state = runtime.op_state();
    let op_state_ref = op_state.borrow();
    let engine_state = op_state_ref.borrow::<ops::EngineOpState>();
    engine_state.emit_dropped_log_record_if_needed();
    engine_state.logs.clone()
}

/// If `result` is `Err` and `terminated` is set, replaces it with
/// [`InvokeError::Terminated`] -- see that variant's doc comment for why
/// this crate tracks termination itself rather than only inferring it from
/// the error `deno_core` happens to surface.
fn mark_terminated(
    result: Result<serde_json::Value, InvokeError>,
    terminated: &AtomicBool,
) -> Result<serde_json::Value, InvokeError> {
    match result {
        Err(err) if terminated.load(Ordering::SeqCst) => {
            Err(InvokeError::Terminated(Box::new(err)))
        }
        other => other,
    }
}

/// Everything [`begin_invocation`] hands back to a caller that will drive
/// the isolate to completion itself (with or without a watchdog thread).
struct BegunInvocation<'a> {
    isolate: PreparedIsolate,
    terminated: Arc<AtomicBool>,
    timeout: Duration,
    export: &'a str,
    event: serde_json::Value,
}

/// Validates/normalizes `invocation.egress`'s allowlist and fixture
/// acknowledgement, builds a fresh isolate, and installs the heap-limit
/// callback -- shared by every `invoke_handler*` entry point so each only
/// has to add its own runtime/watchdog concerns on top.
///
/// Takes `handle: &tokio::runtime::Handle`, forwarded to
/// [`prepare_isolate`] -- see that function's doc comment (review round 6,
/// X1). Every caller must obtain this from a runtime it has *already
/// built* (`Runtime::handle()`) or is *already running on*
/// (`Handle::current()`), never build one after calling this.
///
/// On any failure before an isolate exists at all (an invalid
/// `egress.allowed` entry, or a missing/wrong fixture acknowledgement),
/// returns `Err` directly, with no logs to collect.
fn begin_invocation<'a>(
    handle: &tokio::runtime::Handle,
    invocation: HandlerInvocation<'a>,
    usage: &Arc<AtomicUsize>,
) -> Result<BegunInvocation<'a>, InvokeError> {
    let egress = invocation.egress.normalized()?;
    validate_fixture_acknowledgement(&egress)?;
    let timeout = invocation.limits.timeout;
    let deadline = Instant::now() + timeout;
    let outputs = output_caps(&invocation.outputs);

    let PreparedIsolate {
        mut runtime,
        isolate_handle,
        specifier,
    } = prepare_isolate(
        handle,
        invocation.limits,
        invocation.bundle,
        Arc::clone(usage),
        egress,
        outputs,
        invocation.sink,
        invocation.resolver,
        deadline,
        invocation.handler_id.to_owned(),
        invocation.invocation_id.to_owned(),
        invocation.log_writer,
    )?;

    // Captured before installing the near-heap-limit callback, and moved
    // into it, so the callback can request termination of *this* isolate
    // from wherever V8 invokes it (documented as callable from a GC pause
    // on the isolate's own thread) without needing `&mut JsRuntime`, which
    // the callback signature does not provide.
    let terminated = Arc::new(AtomicBool::new(false));
    install_heap_limit_callback(
        &mut runtime,
        isolate_handle.clone(),
        Arc::clone(&terminated),
    );

    Ok(BegunInvocation {
        isolate: PreparedIsolate {
            runtime,
            isolate_handle,
            specifier,
        },
        terminated,
        timeout,
        export: invocation.export,
        event: invocation.event,
    })
}

/// Runs one [`HandlerInvocation`] to completion in a fresh isolate, on
/// whatever tokio runtime is already driving the calling `.await` chain.
///
/// Test-only: gated behind the `test-util` Cargo feature (enabled
/// automatically for this crate's own `tests/*.rs`, via a self
/// dev-dependency in `Cargo.toml`). Does **not** itself enforce
/// `invocation.limits.timeout` against a synchronous, never-yielding
/// handler body -- see [`invoke_handler_with_watchdog`], the crate's one
/// production entry point, for the mechanism that does and for why a bare
/// `tokio::time::timeout` (as [`invoke_handler_blocking`] uses) is not a
/// substitute for a real watchdog thread. The caller's own tokio runtime
/// must have its network/IO driver enabled (`enable_io()`/`enable_all()`),
/// not just `enable_time()`: a handler that calls `apsides.fetch` needs
/// it.
///
/// This function's returned future is not `Send`: `deno_core::JsRuntime`
/// (and the V8 isolate it owns) is deliberately single-threaded and must
/// only ever be driven from the one current-thread tokio runtime that owns
/// it, matching the "one fresh isolate per invocation, never shared across
/// threads" model.
#[cfg(feature = "test-util")]
#[allow(
    clippy::future_not_send,
    reason = "deno_core::JsRuntime is deliberately !Send; see the doc comment above"
)]
pub async fn invoke_handler(
    invocation: HandlerInvocation<'_>,
    usage: &Arc<AtomicUsize>,
) -> InvocationReport {
    // `Handle::current()`, not a freshly built runtime: this function's own
    // contract is "run on whatever runtime is already driving the caller",
    // and by the time an `async fn` is actually being polled, that runtime
    // is necessarily already entered -- see `begin_invocation`/
    // `prepare_isolate`'s doc comments (review round 6, X1).
    let handle = tokio::runtime::Handle::current();
    let mut begun = match begin_invocation(&handle, invocation, usage) {
        Ok(begun) => begun,
        Err(err) => {
            return InvocationReport {
                result: Err(err),
                logs: Vec::new(),
            };
        }
    };

    let result = run_invocation(
        &mut begun.isolate.runtime,
        &begun.isolate.specifier,
        begun.export,
        &begun.event,
    )
    .await;
    let result = mark_terminated(result, &begun.terminated);
    let logs = collect_logs(&begun.isolate.runtime);
    InvocationReport { result, logs }
}

/// The crate's one production entry point.
///
/// Runs one [`HandlerInvocation`] to completion in a fresh isolate, with a
/// watchdog `std::thread` attached and its own dedicated, current-thread
/// tokio runtime (with the network/IO driver enabled, for
/// `apsides.fetch`).
///
/// Spawns a watchdog `std::thread` that calls
/// `IsolateHandle::terminate_execution()` after `invocation.limits.timeout`,
/// runs the isolate on that dedicated runtime, and blocks the calling
/// thread until it finishes.
///
/// This is the mechanism the plan's wall-clock watchdog (`timeoutMillis`)
/// needs: a `tokio::time::timeout` around an `.await` only ever gets a
/// chance to fire at an await point the awaited future actually yields at.
/// A synchronous, never-yielding handler body (`while (true) {}` at module
/// top level, with no `await` in it at all) runs entirely inside a single,
/// uninterrupted `poll()` of that future -- `tokio::time::timeout` cannot
/// preempt it, because the executor never gets control back to check the
/// deadline. `IsolateHandle::terminate_execution()` called from a
/// *different* OS thread works regardless, because V8 checks for a
/// pending termination request at points a tight synchronous loop still
/// reaches (that is its documented purpose). See `tests/spike.rs`'s
/// `watchdog_thread_terminates_a_synchronous_infinite_loop`.
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
/// Always returns an [`InvocationReport`] (never a bare `Result`):
/// `result` carries the handler's resolved value or why the invocation
/// failed, and `logs` carries every `apsides.log` line captured either
/// way -- see that type's doc comment.
///
/// Builds its tokio runtime **before** doing anything else, including
/// before [`begin_invocation`] (and therefore before `JsRuntime::new`) --
/// review round 6, X1. A prior version built the isolate first and the
/// runtime second, so `deno_core` registered the isolate with no tokio
/// `Handle` to post tasks onto at all (not merely "not currently
/// entered"). That was invisible to this crate's own test suite, which
/// drives everything through `invoke_handler`/`invoke_handler_blocking`
/// inside an already-built `rt.block_on`, but under real allocation churn
/// in production it let V8's own GC memory reducer reach
/// `deno_core`'s `spawn_delayed_task` with nothing to spawn onto, which
/// calls `abort()` and kills the whole controller process outright --
/// not a catchable JS exception, not an isolate termination this crate
/// could turn into an `InvokeError`. See
/// `allocation_churn_survives_through_the_watchdog_entry_point` in
/// `tests/spike.rs` for the regression test.
pub fn invoke_handler_with_watchdog(
    invocation: HandlerInvocation<'_>,
    usage: &Arc<AtomicUsize>,
) -> InvocationReport {
    // `enable_io()`: a handler that calls `apsides.fetch` needs the tokio
    // net driver, not just timers. Built *first*, before the isolate
    // exists at all -- see the doc comment above.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            return InvocationReport {
                result: Err(InvokeError::Runtime(err)),
                logs: Vec::new(),
            };
        }
    };

    let mut begun = match begin_invocation(rt.handle(), invocation, usage) {
        Ok(begun) => begun,
        Err(err) => {
            return InvocationReport {
                result: Err(err),
                logs: Vec::new(),
            };
        }
    };

    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let terminated_for_watchdog = Arc::clone(&begun.terminated);
    let isolate_handle = begun.isolate.isolate_handle.clone();
    let timeout = begun.timeout;
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
            terminated_for_watchdog.store(true, Ordering::SeqCst);
            isolate_handle.terminate_execution();
        }
    });

    let result = rt.block_on(run_invocation(
        &mut begun.isolate.runtime,
        &begun.isolate.specifier,
        begun.export,
        &begun.event,
    ));
    let result = mark_terminated(result, &begun.terminated);
    let logs = collect_logs(&begun.isolate.runtime);

    drop(cancel_tx);
    // A panicking watchdog thread (it cannot: the closure above has no
    // fallible/panicking calls) would be the only way `join` returns
    // `Err`; there is nothing more to do with that here than let it drop.
    let _ignored = watchdog.join();

    InvocationReport { result, logs }
}

/// Runs [`invoke_handler`] on a fresh single-threaded current-thread tokio
/// runtime (with the network/IO driver enabled) and blocks on it.
///
/// Test-only, gated the same way as [`invoke_handler`]. The
/// `tokio::time::timeout` this wraps the whole `.await` chain in only
/// helps between await points (op calls, event loop turns) -- it is
/// **not** a substitute for a real wall-clock watchdog against a
/// synchronous, never-yielding script; see
/// [`invoke_handler_with_watchdog`]'s doc comment for why. A timeout here
/// drops the in-flight invocation's future, so `logs` is empty in that one
/// case (the watchdog-based entry point does not have this gap).
#[cfg(feature = "test-util")]
pub fn invoke_handler_blocking(
    invocation: HandlerInvocation<'_>,
    usage: &Arc<AtomicUsize>,
) -> InvocationReport {
    let timeout = invocation.limits.timeout;
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            return InvocationReport {
                result: Err(InvokeError::Runtime(err)),
                logs: Vec::new(),
            };
        }
    };
    rt.block_on(async move {
        match tokio::time::timeout(timeout, invoke_handler(invocation, usage)).await {
            Ok(report) => report,
            Err(_elapsed) => InvocationReport {
                result: Err(InvokeError::Timeout),
                logs: Vec::new(),
            },
        }
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
    #[error("egress.allowed entry {0:?} is invalid (empty host)")]
    InvalidEgressAllowlistEntry(String),
    #[error("failed to build tokio runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("handler invocation timed out")]
    Timeout,
    /// The isolate was terminated by this crate's own heap-limit callback
    /// or watchdog thread (tracked directly via a shared flag, not
    /// inferred from `deno_core`'s error shape -- see
    /// `install_heap_limit_callback`'s doc comment for why: a termination
    /// that interrupts a *call to the handler's exported function* does
    /// not always come back as the usual "execution terminated"
    /// `CoreErrorKind::Js`; it can instead surface as
    /// `CoreErrorKind::PendingPromiseResolution` once the event loop goes
    /// idle with the handler's promise never settled). The wrapped error
    /// is whatever `deno_core` actually reported, kept for diagnostics.
    #[error("isolate was terminated (heap limit or timeout): {0}")]
    Terminated(#[source] Box<Self>),
}

impl InvokeError {
    /// True if this invocation ended because *this crate's own*
    /// heap-limit callback or watchdog thread terminated the isolate (see
    /// [`Self::Terminated`]), or -- for a termination that happened during
    /// top-level module evaluation, before the handler's exported function
    /// was ever called -- because the underlying [`EngineError`] carries
    /// `deno_core`'s usual "execution terminated" shape (see
    /// [`EngineError::is_isolate_termination`]).
    #[must_use]
    pub fn is_isolate_termination(&self) -> bool {
        if matches!(self, Self::Terminated(_)) {
            return true;
        }
        matches!(self, Self::Engine(err) if err.is_isolate_termination())
    }
}
