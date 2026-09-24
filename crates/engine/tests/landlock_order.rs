//! Phase 5a spike gate, ordering-correct filesystem experiment (see the
//! Apsides v0.3 plan, "Phase 5 -> 5a. Spike gate" and "5c. Filesystem
//! lockdown").
//!
//! `tests/landlock_fs.rs` applies Landlock on a `#[test]`'s own thread
//! *after* this process's V8 platform may already have been initialized by
//! some other test (order is not controlled there) -- proving the ruleset
//! itself works, but not that threads V8/tokio spawn *after* lockdown
//! inherit it. Review flagged this as the real gap: Landlock's
//! `restrict_self()` only restricts the *calling* thread directly (this
//! kernel's Landlock ABI, V7, has no thread-sync capability to retroactively
//! restrict existing sibling threads -- `all_threads: false` in the status
//! `tests/landlock_fs.rs` prints), but a *new* thread created after
//! `restrict_self()` inherits the calling thread's security context at
//! `clone()`/spawn time regardless of that ABI limitation. So the ordering
//! that actually matters for production (Phase 5c) is: Landlock **first**,
//! on the main thread, before anything spawns a single other thread; V8
//! platform init (and thus its worker thread pool) **after** that; the
//! tokio runtime (and *its* worker threads) after that.
//!
//! `harness = false` (see `Cargo.toml`): this binary's own `fn main()`
//! controls that order directly, with no risk of Rust's default per-test
//! thread pool doing something (running a different test, or this one, on a
//! thread landlock has already touched) out of the intended sequence.
//!
//! Kept fail-closed when Landlock is unsupported: the file-open-denial
//! assertions below are skipped (not silently passed) whenever
//! `apply_deny_all_or_fail_closed` reports `RulesetStatus::NotEnforced`;
//! `tests/landlock_fs.rs`'s `fails_closed_without_escape_hatch_iff_landlock_is_unsupported`
//! is what actually asserts the fail-closed *contract* itself (that
//! behavior does not depend on this binary's thread-ordering concern).

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use common::{
    LockdownOutcome, SPIKE_HTTPS_HOST, apply_deny_all_or_fail_closed, assert_file_open_denied,
    count_process_threads, handler_invocation, wrap_as_handler,
};

fn main() -> Result<(), String> {
    // 0. Verify this test's own premise, rather than assuming it: exactly
    //    one thread (this one) must exist right now, before Landlock is
    //    applied. If a future edit moved V8 platform init or the tokio
    //    runtime build above step 1 below, threads would already exist
    //    here, this assertion would fail, and steps (b)/(c) further down
    //    would otherwise have silently stopped proving anything (they'd
    //    still find *later* threads restricted, but not the ones created
    //    by that hypothetical earlier, unrestricted init).
    let thread_count = count_process_threads().map_err(|err| err.to_string())?;
    if thread_count != 1 {
        return Err(format!(
            "expected exactly one thread before Landlock lockdown (this \
             test's whole point is that lockdown happens before any other \
             thread, including V8's or tokio's worker threads, is \
             created), found {thread_count} entries in /proc/self/task"
        ));
    }

    // 1. Landlock deny-all FIRST -- before V8 platform init, before any
    //    tokio runtime, before any other thread exists in this process at
    //    all -- so every thread spawned from here on inherits it.
    let status = apply_deny_all_or_fail_closed(true)?;
    let supported = status != LockdownOutcome::NotEnforced;
    eprintln!("[landlock-order] landlock status: {status:?} (supported={supported})");

    if supported {
        assert_file_open_denied("main thread, immediately after restrict_self")?;
    } else {
        eprintln!(
            "[landlock-order] Landlock unsupported on this kernel; continuing \
             without file-denial assertions. The fail-closed *contract* itself \
             is asserted regardless of this environment in \
             tests/landlock_fs.rs's fails_closed_without_escape_hatch_iff_landlock_is_unsupported."
        );
    }

    // 2. THEN initialize the V8 platform -- this is what spawns V8's
    //    background worker thread pool (GC, compilation). If V8 init
    //    itself needed to open any file, either it would fail here (it
    //    doesn't -- see the successful GC-heavy handler run below, which
    //    depends on a working platform) or it would silently degrade in a
    //    way step 3 below would likely surface as a hang or a JS-visible
    //    failure. Neither happened in this run: V8 init under lockdown
    //    needed no additional path in this environment.
    engine::ensure_platform_initialized();

    if supported {
        assert_file_open_denied("main thread, after V8 platform init")?;
    }

    // 3. GC-heavy work: forces V8's background worker threads (spawned in
    //    step 2, under lockdown) to actually run compaction/sweep passes
    //    under load, not just exist idle. This is a functional check (does
    //    V8 still work at all under a restricted process?), not a security
    //    assertion by itself -- (b) and (c) below are the security checks
    //    for threads spawned after lockdown.
    run_gc_heavy_handler()?;

    if supported {
        // (b) a std::thread spawned after lockdown + platform init must
        // inherit the restriction from the main thread's security context.
        let handle = std::thread::spawn(|| assert_file_open_denied("spawned std::thread"));
        handle
            .join()
            .map_err(|_panic| "spawned std::thread panicked".to_owned())??;
    }

    // 4. THEN build the tokio runtime -- multi-threaded, so its own worker
    //    threads (also spawned after lockdown) get checked too.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|err| format!("failed to build tokio runtime: {err}"))?;

    rt.block_on(async {
        if supported {
            // (c) a tokio worker thread (this task may run on any of the
            // runtime's core worker threads, all spawned after lockdown)
            // must inherit the restriction too.
            tokio::spawn(async { assert_file_open_denied("tokio worker thread") })
                .await
                .map_err(|err| format!("tokio worker task panicked: {err}"))??;
        }

        // DNS + TCP (+ TLS, best-effort re this sandbox's own transparent
        // TLS interception -- see common::https_get_via_hickory's doc
        // comment) via hickory, proving no glibc getaddrinfo /
        // /etc/resolv.conf read is needed under lockdown. Gated behind an
        // env var (not run by default) since it needs outbound network
        // access CI may not have.
        if std::env::var_os("APSIDES_SPIKE_NETWORK_TEST").is_some() {
            common::https_get_via_hickory(SPIKE_HTTPS_HOST).await?;
        } else {
            eprintln!(
                "[landlock-order] APSIDES_SPIKE_NETWORK_TEST not set; skipping the \
                 DNS+TCP(+TLS) leg. Set APSIDES_SPIKE_NETWORK_TEST=1 to run it."
            );
        }

        Ok::<(), String>(())
    })?;

    eprintln!(
        "[landlock-order] NOT exercised by this spike (reporting explicitly, per \
         review): (1) running any of this inside the pinned distroless runtime \
         image -- Docker was unavailable in the session that wrote this spike \
         (`docker info` reported no daemon socket to connect to); (2) a real \
         Kubernetes API request through a hickory-based connector -- only a \
         plain DNS+TCP(+TLS) request to a public host (example.com) was \
         exercised above, not an actual `kube`/API-server call."
    );

    Ok(())
}

/// Runs a handler module that allocates and drops many small typed arrays
/// across many rounds (forcing V8's GC, including its background worker
/// threads, to do real work), then calls the spike op once more to confirm
/// the isolate is still healthy afterward.
///
/// Goes through `invoke_handler_with_watchdog`, the production entry
/// point, not the test-only `invoke_handler_blocking` -- this is exactly
/// the GC-under-allocation-pressure shape review round 6's X1 finding
/// needed the production entry point to reproduce (see
/// `tests/spike.rs`'s `allocation_churn_survives_through_the_watchdog_entry_point`),
/// and this binary's whole reason to exist is proving production-shaped
/// behavior (V8 + Landlock ordering) under a real lockdown, so it should
/// not itself lean on a test-only shortcut.
fn run_gc_heavy_handler() -> Result<(), String> {
    let usage = Arc::new(AtomicUsize::new(0));
    let bundle = wrap_as_handler(
        r#"
        for (let round = 0; round < 200; round++) {
            let chunks = [];
            for (let i = 0; i < 2000; i++) {
                chunks.push(new Uint8Array(1024));
            }
            chunks = null;
        }
        apsides.log("info", "pong-from-rust");
    "#,
    );
    let mut invocation = handler_invocation(&bundle);
    invocation.limits.timeout = Duration::from_secs(30);
    engine::invoke_handler_with_watchdog(invocation, &usage)
        .result
        .map(|_value| ())
        .map_err(|err| format!("GC-heavy handler failed under lockdown: {err}"))
}
