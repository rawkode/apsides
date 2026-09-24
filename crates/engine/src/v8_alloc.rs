// Phase 5a spike gate: the one unsafe module in this crate.
//
// `v8::new_rust_allocator` is `unsafe fn` because it hands V8 a vtable of
// `unsafe extern "C" fn` pointers plus an opaque handle pointer, and V8 calls
// them back across the FFI boundary for the isolate's lifetime with no
// borrow-checker supervision. There is no safe wrapper for a *custom*
// (capped) V8 ArrayBuffer allocator anywhere in the `v8`/`deno_core` public
// API; `v8::array_buffer::new_default_allocator()` is safe but is a bare
// malloc/free allocator with no cap. Confined here per the workspace hard
// rule (see crates/engine/Cargo.toml header comment and
// /home/user/apsides-PLAN.md "5a. Spike gate").
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
    /// then return a null pointer, which V8 treats as allocation failure
    /// (surfaced to JS as a catchable `RangeError`, not a crash).
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
/// `cap_bytes`. Returns the allocator (to install via
/// `v8::CreateParams::array_buffer_allocator`) plus a live byte-usage
/// counter for tests/observability.
pub fn capped_array_buffer_allocator(
    cap_bytes: usize,
) -> (v8::UniqueRef<v8::Allocator>, Arc<AtomicUsize>) {
    let usage = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(CappedAllocatorState {
        used: Arc::clone(&usage),
        cap_bytes,
    });
    let handle = Arc::into_raw(state);
    // SAFETY: `handle` is a valid `Arc<CappedAllocatorState>` pointer just
    // produced above by `Arc::into_raw`, kept alive until `drop_handle`
    // reclaims it (V8 owns that call), and `VTABLE`'s function pointers all
    // match the `T = CappedAllocatorState` type of `handle`.
    let allocator = unsafe { v8::new_rust_allocator(handle, &VTABLE) };
    (allocator, usage)
}
