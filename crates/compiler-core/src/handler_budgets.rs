//! Shared sizing constants for the handler engine.
//!
//! Used by both `runtime` (to build `engine::Limits` for one invocation)
//! and `platform` (to size the controller pod's memory limit), so the two
//! formulas cannot silently drift apart. `engine` itself cannot depend on
//! this crate (it is deliberately IR-agnostic -- see its own crate doc
//! comment), so its two internal constants that also feed this formula
//! (`HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES` and
//! `ops::MAX_FETCH_REQUEST_BODY_BYTES`, both in `crates/engine/src`) are
//! mirrored here rather than imported; `runtime` (which depends on both
//! crates) carries a test asserting the two stay equal.

const BYTES_PER_MI: u64 = 1024 * 1024;

/// Fixed reconciler/runtime baseline the controller pod always needs,
/// independent of whether the domain declares any handlers at all.
pub const POD_MEMORY_BASELINE_BYTES: u64 = 256 * BYTES_PER_MI;

/// Fixed slack folded into the pod's memory limit.
///
/// On top of every other named budget below, paid once per pod regardless
/// of handler count -- covers the V8 platform/isolate bookkeeping and
/// embedded bundle(s) that `engine` always links into the controller image
/// (even a domain with no declared handlers), plus rounding/safety margin.
pub const POD_MEMORY_SLACK_BYTES: u64 = 32 * BYTES_PER_MI;

/// Hard cap on `engine::Limits::max_array_buffer_bytes` regardless of a
/// handler's declared heap -- see [`array_buffer_cap_bytes`].
pub const HANDLER_ARRAY_BUFFER_CAP_BYTES: u64 = 32 * BYTES_PER_MI;

/// `engine::Limits::max_fetch_bytes_total`: the cumulative `apsides.fetch`
/// response budget for one whole invocation.
pub const HANDLER_FETCH_BUDGET_TOTAL_BYTES: u64 = 16 * BYTES_PER_MI;

/// `engine::Limits::max_concurrent_fetches`.
pub const HANDLER_MAX_CONCURRENT_FETCHES: u64 = 4;

/// Per-call `apsides.fetch` request body cap `engine::ops` enforces
/// internally (`MAX_FETCH_REQUEST_BODY_BYTES`) -- see this module's doc
/// comment on why this is mirrored, not imported.
pub const HANDLER_FETCH_REQUEST_BODY_BYTES: u64 = BYTES_PER_MI;

/// Total heap-limit-termination headroom `engine` may grant one isolate.
///
/// Mirrors `HEAP_LIMIT_TERMINATION_HEADROOM_TOTAL_CAP_BYTES` in
/// `crates/engine/src/lib.rs` -- see this module's doc comment on why this
/// is mirrored, not imported.
pub const HANDLER_HEAP_HEADROOM_TOTAL_BYTES: u64 = 32 * BYTES_PER_MI;

/// `engine::Limits::max_array_buffer_bytes` for a handler declaring
/// `heap_bytes` of V8 heap: at most half the heap, and never more than
/// [`HANDLER_ARRAY_BUFFER_CAP_BYTES`].
///
/// `ArrayBuffer` backing stores live outside the V8 heap the heap limit
/// watches, so an uncapped `max_array_buffer_bytes` would let them alone
/// approach the full declared heap *on top of* the heap itself, doubling
/// worst-case process memory rather than sharing a bound with it.
#[must_use]
pub const fn array_buffer_cap_bytes(heap_bytes: u64) -> u64 {
    let half = heap_bytes / 2;
    if half < HANDLER_ARRAY_BUFFER_CAP_BYTES {
        half
    } else {
        HANDLER_ARRAY_BUFFER_CAP_BYTES
    }
}

/// The controller pod's memory limit, in bytes, for a domain whose largest
/// declared handler needs `max_handler_heap_bytes` of V8 heap (`0` for a
/// domain with no handlers at all).
///
/// `256Mi + heap + arrayBufferCap(heap) + fetchBudget + heapHeadroom +
/// requestBodies + 32Mi slack`, where the heap-dependent terms
/// (`arrayBufferCap`, `fetchBudget`, `heapHeadroom`, `requestBodies`) are
/// all `0` when `max_handler_heap_bytes` is `0` -- a domain with no
/// handlers never builds an isolate, so none of that per-invocation
/// budget is ever actually needed; the fixed baseline/slack terms still
/// apply regardless.
#[must_use]
pub const fn controller_memory_limit_bytes(max_handler_heap_bytes: u64) -> u64 {
    let handler_terms = if max_handler_heap_bytes == 0 {
        0
    } else {
        max_handler_heap_bytes
            + array_buffer_cap_bytes(max_handler_heap_bytes)
            + HANDLER_FETCH_BUDGET_TOTAL_BYTES
            + HANDLER_HEAP_HEADROOM_TOTAL_BYTES
            + HANDLER_MAX_CONCURRENT_FETCHES * HANDLER_FETCH_REQUEST_BODY_BYTES
    };
    POD_MEMORY_BASELINE_BYTES + POD_MEMORY_SLACK_BYTES + handler_terms
}

#[cfg(test)]
mod tests {
    use super::{
        HANDLER_ARRAY_BUFFER_CAP_BYTES, array_buffer_cap_bytes, controller_memory_limit_bytes,
    };

    #[test]
    fn array_buffer_cap_is_half_the_heap_under_the_fixed_ceiling() {
        assert_eq!(array_buffer_cap_bytes(8 * 1024 * 1024), 4 * 1024 * 1024);
    }

    #[test]
    fn array_buffer_cap_saturates_at_the_fixed_ceiling_for_a_large_heap() {
        assert_eq!(
            array_buffer_cap_bytes(256 * 1024 * 1024),
            HANDLER_ARRAY_BUFFER_CAP_BYTES
        );
    }

    #[test]
    fn no_handlers_pays_only_the_fixed_baseline_and_slack() {
        assert_eq!(controller_memory_limit_bytes(0), (256 + 32) * 1024 * 1024);
    }

    #[test]
    fn a_declared_handler_adds_every_named_term() {
        let heap = 8 * 1024 * 1024_u64;
        let expected = (256 + 32) * 1024 * 1024
            + heap
            + 4 * 1024 * 1024
            + 16 * 1024 * 1024
            + 32 * 1024 * 1024
            + 4 * 1024 * 1024;
        assert_eq!(controller_memory_limit_bytes(heap), expected);
    }
}
