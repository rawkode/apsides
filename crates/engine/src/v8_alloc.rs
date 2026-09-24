// Phase 5a spike gate: the one unsafe module in this crate.
//
// `v8::new_rust_allocator` is `unsafe fn` because it hands V8 a vtable of
// `unsafe extern "C" fn` pointers plus an opaque handle pointer, and V8 calls
// them back across the FFI boundary for the isolate's lifetime with no
// borrow-checker supervision. There is no safe wrapper for a *custom*
// (capped) V8 ArrayBuffer allocator anywhere in the `v8`/`deno_core` public
// API; `v8::array_buffer::new_default_allocator()` is safe but is a bare
// malloc/free allocator with no cap. Confined here per the workspace hard
// rule (see crates/engine/Cargo.toml header comment and the Apsides v0.3
// plan, "Phase 5 -> 5a. Spike gate").
//
// IMPORTANT SCOPE LIMIT, found during review: this allocator only governs
// memory V8 itself requests through `v8::ArrayBuffer::Allocator` -- i.e.
// ordinary `new ArrayBuffer(n)` / `new Uint8Array(n)` allocations. It does
// **not** cover:
//   - Resizable/growable `ArrayBuffer`s (`new ArrayBuffer(0, {maxByteLength})`):
//     V8 reserves/commits their backing memory through its own page
//     allocator, bypassing this vtable entirely, and `.resize()` can then
//     grow one past this cap with zero calls into `allocate`. That JS-level
//     construction path is closed separately, in `lib.rs`'s bootstrap guard
//     script (`ARRAY_BUFFER_GUARD_SCRIPT`), before user code runs; see
//     `tests/spike.rs`'s `resizable_array_buffer_construction_is_rejected`
//     for the regression test.
//   - Backing stores an op builds directly from a Rust `Box<[u8]>` (e.g. via
//     `serde_v8`'s buffer conversions) and hands to V8 as an already-owned
//     buffer. Those never call into this allocator's `allocate`/
//     `allocate_uninitialized` at all -- V8 just takes ownership of Rust
//     memory that was already sized and allocated on the Rust side. Any
//     future op that returns data to JS (e.g. Phase 5b's `op_apsides_fetch`)
//     must therefore bound its own response size itself (the plan's
//     `RESPONSE_TOO_LARGE`/`maxObjectBytes` caps), not rely on this cap.
#![allow(
    unsafe_code,
    reason = "v8::new_rust_allocator is FFI; see module doc comment above"
)]

use std::alloc::{self, Layout};
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use deno_core::v8;

/// Byte alignment requested from the system allocator for V8-owned
/// `ArrayBuffer` backing stores. 16 is what V8's own default allocator uses
/// and is sufficient for every typed-array view V8 constructs over it.
const ALIGN: usize = 16;

/// Shared, thread-safe accounting for a capped V8 `ArrayBuffer` allocator.
///
/// V8 may call the allocator vtable from any of its background threads
/// (e.g. during a background GC sweep that frees a detached buffer), so this
/// must be `Send + Sync`; `AtomicUsize` gives that without a lock.
struct CappedAllocatorState {
    /// Shared with the `Arc<AtomicUsize>` returned to the caller, so tests
    /// can observe live usage without reaching back into V8.
    used: Arc<AtomicUsize>,
    cap_bytes: usize,
}

impl CappedAllocatorState {
    /// Reserves `len` bytes against the cap. Returns `false` (reservation
    /// refused) if it would exceed `cap_bytes`; the allocator vtable must
    /// then return a null pointer, which V8 treats as allocation failure.
    ///
    /// On this pinned V8 build that does *not* reliably surface to JS as a
    /// catchable `RangeError` the way V8's own allocator-failure contract
    /// describes: `tests/spike.rs`'s
    /// `array_buffer_allocation_over_the_cap_terminates_the_isolate...`
    /// found that a failed allocation here instead invokes the isolate's
    /// near-heap-limit callback (`lib.rs`), which terminates the whole
    /// isolate. This function's own job -- refuse the allocation and update
    /// `used` correctly -- is unaffected either way; only the caller-visible
    /// failure shape differs from what the allocator's own contract implies.
    fn try_reserve(&self, len: usize) -> bool {
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(len) else {
                return false;
            };
            if next > self.cap_bytes {
                return false;
            }
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, len: usize) {
        self.used.fetch_sub(len, Ordering::Relaxed);
    }
}

unsafe extern "C" fn allocate(state: &CappedAllocatorState, len: usize) -> *mut c_void {
    allocate_impl(state, len, true)
}

unsafe extern "C" fn allocate_uninitialized(
    state: &CappedAllocatorState,
    len: usize,
) -> *mut c_void {
    allocate_impl(state, len, false)
}

fn allocate_impl(state: &CappedAllocatorState, len: usize, zeroed: bool) -> *mut c_void {
    if !state.try_reserve(len) {
        return std::ptr::null_mut();
    }
    let Ok(layout) = Layout::from_size_align(len.max(1), ALIGN) else {
        state.release(len);
        return std::ptr::null_mut();
    };
    // SAFETY: `layout` has a non-zero size (`len.max(1)`) and `ALIGN` (16) is
    // a non-zero power of two, so `Layout::from_size_align` above already
    // validated the two preconditions `alloc`/`alloc_zeroed` require.
    let ptr = unsafe {
        if zeroed {
            alloc::alloc_zeroed(layout)
        } else {
            alloc::alloc(layout)
        }
    };
    if ptr.is_null() {
        state.release(len);
    }
    ptr.cast()
}

unsafe extern "C" fn free(state: &CappedAllocatorState, data: *mut c_void, len: usize) {
    if data.is_null() {
        return;
    }
    if let Ok(layout) = Layout::from_size_align(len.max(1), ALIGN) {
        // SAFETY: `data` was returned by `allocate`/`allocate_uninitialized`
        // above from a matching `Layout::from_size_align(len.max(1), ALIGN)`,
        // per the `RustAllocatorVtable` contract V8 upholds (it only ever
        // calls `free` with a pointer and length it previously received from
        // `allocate*` with that same length).
        unsafe { alloc::dealloc(data.cast(), layout) };
    }
    state.release(len);
}

unsafe extern "C" fn drop_handle(handle: *const CappedAllocatorState) {
    // SAFETY: `handle` was produced by `Arc::into_raw` in
    // `capped_array_buffer_allocator` below and V8 calls `drop` exactly once,
    // when the `v8::Allocator` it wraps is itself destroyed at isolate
    // teardown, per the `RustAllocatorVtable` contract.
    drop(unsafe { Arc::from_raw(handle) });
}

const VTABLE: v8::RustAllocatorVtable<CappedAllocatorState> = v8::RustAllocatorVtable {
    allocate,
    allocate_uninitialized,
    free,
    drop: drop_handle,
};

/// Builds a V8 `ArrayBuffer` allocator that refuses any allocation once the
/// isolate's outstanding `ArrayBuffer` backing-store bytes would exceed
/// `cap_bytes`. Returns the allocator, to install via
/// `v8::CreateParams::array_buffer_allocator`.
///
/// `usage` is the live byte-usage counter this allocator updates on every
/// `allocate`/`allocate_uninitialized`/`free` call. It is caller-supplied
/// (rather than created here) so the caller can keep its own `Arc` clone and
/// inspect it during and after the isolate's run -- see
/// `tests/spike.rs`'s over-cap tests, which assert usage never exceeded the
/// cap even when the isolate itself was terminated.
pub fn capped_array_buffer_allocator(
    cap_bytes: usize,
    usage: Arc<AtomicUsize>,
) -> v8::UniqueRef<v8::Allocator> {
    let state = Arc::new(CappedAllocatorState {
        used: usage,
        cap_bytes,
    });
    let handle = Arc::into_raw(state);
    // SAFETY: `handle` is a valid `Arc<CappedAllocatorState>` pointer just
    // produced above by `Arc::into_raw`, kept alive until `drop_handle`
    // reclaims it (V8 owns that call), and `VTABLE`'s function pointers all
    // match the `T = CappedAllocatorState` type of `handle`.
    unsafe { v8::new_rust_allocator(handle, &VTABLE) }
}
