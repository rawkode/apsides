//! Phase 5a spike gate, filesystem experiment (see the Apsides v0.3 plan,
//! "Phase 5 -> 5a. Spike gate" and "5c. Filesystem lockdown").
//!
//! These tests apply Landlock *after* this process's V8 platform may
//! already have been initialized elsewhere (order is not controlled here),
//! and only restrict the calling thread. That is enough to prove the
//! deny-all ruleset itself works and the fail-closed contract holds, but it
//! is *not* proof that threads V8/tokio spawn inherit the restriction --
//! see `tests/landlock_order.rs` for that (stricter, ordering-correct)
//! experiment, which is the authoritative one for the plan's Landlock
//! allowlist question.
//!
//! Each `#[test]` here runs on its own OS thread (the default Rust test
//! harness behavior) and calls `landlock::RulesetCreated::restrict_self`,
//! which -- per the `landlock` crate's `RestrictionStatus::all_threads`
//! field (`false` on this kernel's Landlock ABI, which lacks the
//! thread-sync capability newer ABIs add) -- restricts only the calling
//! thread, so these tests do not interfere with each other or with
//! `spike.rs`'s tests sharing the same process.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use common::{
    SPIKE_HTTPS_HOST, apply_deny_all_or_fail_closed, assert_file_open_denied, handler_invocation,
    https_get_via_hickory, wrap_as_handler,
};
use landlock::RulesetStatus;

/// The plan's "if Landlock is unavailable here, ... run the test anyway to
/// show the unsupported path fails closed": detect support (via the escape
/// hatch, which never itself fails), then re-check that asking for deny-all
/// *without* the escape hatch agrees -- `Ok` when the kernel can actually
/// enforce it, `Err` otherwise. This holds regardless of which environment
/// it runs in.
#[test]
fn fails_closed_without_escape_hatch_iff_landlock_is_unsupported() -> Result<(), String> {
    let detected = apply_deny_all_or_fail_closed(true)?;
    let supported = detected.ruleset != RulesetStatus::NotEnforced;
    eprintln!("[landlock spike] kernel support detected: {supported} (status = {detected:?})");

    let strict = apply_deny_all_or_fail_closed(false);
    if strict.is_ok() != supported {
        return Err(format!(
            "fail-closed contract violated: supported={supported}, \
             strict (no escape hatch) result={strict:?}"
        ));
    }
    Ok(())
}

/// If Landlock actually enforced the deny-all ruleset (`FullyEnforced` or
/// `PartiallyEnforced`), a plain file open of an unrelated path must now
/// fail. If it reports `NotEnforced`, nothing is asserted about file
/// access; the outcome is only logged, since deny-all was never actually
/// applied.
///
/// This environment's `/sys/kernel/security/lsm` does *not* list
/// `landlock` (securityfs shows it empty here), which looked at first like
/// Landlock being unavailable -- but the `landlock` crate's own probe (the
/// actual `landlock_create_ruleset` syscall, which is authoritative) reports
/// `Available { effective_abi: V7 }` and this ruleset comes back
/// `PartiallyEnforced` (ABI V7 lacks a couple of the rights the pinned
/// `ABI::V9` requests, e.g. `IoctlDev`, so it is partial rather than full,
/// but the basic read/open rights this test exercises are covered). Do not
/// trust `/sys/kernel/security/lsm`'s absence as "Landlock unavailable" --
/// probe with the syscall, as this test does.
#[test]
fn deny_all_blocks_file_access_iff_actually_enforced() -> Result<(), String> {
    let status = apply_deny_all_or_fail_closed(true)?;

    match status.ruleset {
        RulesetStatus::NotEnforced => {
            eprintln!(
                "[landlock spike] Landlock unsupported here (landlock status \
                 = {:?}); not asserting on file access",
                status.landlock
            );
            Ok(())
        }
        RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => {
            assert_file_open_denied("main thread (per-thread restrict_self)")
        }
    }
}

/// The full 5a filesystem experiment in the order this test binary can
/// control (V8 platform init happens implicitly, wherever `spike.rs`'s or
/// this file's own tests first call into `engine`, so it is *not*
/// guaranteed to be before or after this test's `apply_deny_all_or_fail_closed`
/// call -- see `tests/landlock_order.rs` for the version that pins that
/// order deliberately): apply deny-all Landlock, then build the tokio
/// runtime and run both a handler module invocation and a
/// `hickory-resolver`-based HTTPS request. Uses the escape hatch
/// unconditionally so this exercises the handler+DNS+TLS path on kernels
/// without Landlock too; `deny_all_blocks_file_access_iff_actually_enforced`
/// above is what actually asserts the enforcement property.
///
/// Ignored by default (needs outbound network access for DNS/TCP/TLS, which
/// CI may not have): run explicitly with `cargo test -- --ignored` or
/// `APSIDES_SPIKE_NETWORK_TEST=1 cargo test -- --include-ignored`.
#[test]
#[ignore = "needs outbound network access (DNS + TCP + TLS); see tests/common/mod.rs's https_get_via_hickory"]
fn handler_and_hickory_https_work_after_v8_init_and_landlock_lockdown() -> Result<(), String> {
    let status = apply_deny_all_or_fail_closed(true)?;
    eprintln!("[landlock spike] lockdown before handler+DNS+TLS: {status:?}");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to build tokio runtime after lockdown: {err}"))?;

    rt.block_on(async {
        run_handler_module_under_lockdown().await?;
        https_get_via_hickory(SPIKE_HTTPS_HOST).await
    })
}

// `deno_core::JsRuntime` (and its `invoke_handler`) is deliberately
// !Send; see the matching `#[allow]` and doc comment on `invoke_handler`
// in `src/lib.rs`.
//
// This is one of the few places this crate's own tests still reach for
// the test-only `invoke_handler` rather than the production
// `invoke_handler_with_watchdog` (review round 6: prefer the production
// entry point unless a test genuinely needs the test-only one). This one
// does: the caller above (`handler_and_hickory_https_work_after_v8_init_and_landlock_lockdown`)
// needs the handler invocation and the raw `https_get_via_hickory` probe
// to run back-to-back on the *same* already-built, already-entered tokio
// runtime, inside one `rt.block_on` -- `invoke_handler_with_watchdog`
// builds and owns its own separate runtime internally, which cannot be
// composed into an existing async chain like that.
#[allow(
    clippy::future_not_send,
    reason = "awaits engine::invoke_handler, which is itself !Send by design"
)]
async fn run_handler_module_under_lockdown() -> Result<(), String> {
    let bundle = wrap_as_handler(
        r#"
        apsides.log("info", "pong-from-rust");
    "#,
    );
    let usage = Arc::new(AtomicUsize::new(0));
    engine::invoke_handler(handler_invocation(&bundle), &usage)
        .await
        .result
        .map_err(|err| format!("handler module failed under lockdown: {err}"))?;
    Ok(())
}
