//! `landlock_denies_worker_thread_open` (see the Apsides v0.3 plan, "5c.
//! Filesystem lockdown", item 4): applies the exact same lockdown sequence
//! `runtime::start` uses -- `engine::count_process_threads`'s
//! exactly-one-thread precondition, then
//! `engine::apply_deny_all_or_fail_closed` -- on this binary's own main
//! thread, before any other thread exists, then spawns a multi-threaded
//! tokio runtime and confirms a worker thread it creates (necessarily
//! *after* lockdown) cannot open an arbitrary file.
//!
//! Calls the real `engine` functions `runtime::start` itself calls (see
//! `crates/runtime/src/startup.rs`), not a reimplementation of them, so
//! this is a faithful proof of the production sequence, just without the
//! surrounding IR/kube-config machinery `runtime::start` also needs --
//! that ordering (Landlock, *then* `engine::ensure_platform_initialized`,
//! *then* the tokio runtime) is already proven end-to-end, including V8's
//! own worker threads, by `crates/engine/tests/landlock_order.rs`; this
//! test's own focus is specifically the tokio runtime `runtime::start`
//! itself builds (V8/handlers are out of scope for a domain with no
//! declared handlers, which is the common case this crate's own tests
//! exercise elsewhere).
//!
//! `harness = false` (see `Cargo.toml`), the same reason
//! `engine/tests/landlock_order.rs` uses it: this binary's `fn main`
//! applies a real Landlock lockdown to its own main thread, which must not
//! leak into other tests' threads under libtest's shared-process default
//! harness.
//!
//! Kept fail-closed like its `engine`-level counterpart: if this kernel
//! cannot enforce Landlock at all, the file-denial assertion is skipped
//! (logged, not silently passed) rather than asserting something false.

use std::fs;
use std::io::ErrorKind;

const DENY_ALL_PROBE_PATH: &str = "/etc/passwd";

fn main() -> Result<(), String> {
    let thread_count = engine::count_process_threads().map_err(|err| err.to_string())?;
    if thread_count != 1 {
        return Err(format!(
            "expected exactly one thread before Landlock lockdown, found {thread_count} \
             entries in /proc/self/task"
        ));
    }

    let outcome = engine::apply_deny_all_or_fail_closed(true).map_err(|err| err.to_string())?;
    let supported = outcome != engine::LockdownOutcome::NotEnforced;
    eprintln!("[landlock-worker-thread] lockdown outcome: {outcome:?} (supported={supported})");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|err| format!("failed to build tokio runtime: {err}"))?;

    rt.block_on(async {
        if !supported {
            eprintln!(
                "[landlock-worker-thread] Landlock is unsupported on this kernel; \
                 skipping the file-denial assertion (the fail-closed contract itself is \
                 asserted regardless of environment support by \
                 engine/tests/landlock_fs.rs's \
                 fails_closed_without_escape_hatch_iff_landlock_is_unsupported and this \
                 crate's own fixture-mode allowNoLandlock tests)."
            );
            return Ok(());
        }
        tokio::spawn(async { assert_probe_path_denied() })
            .await
            .map_err(|err| format!("tokio worker task panicked: {err}"))?
    })
}

fn assert_probe_path_denied() -> Result<(), String> {
    match fs::File::open(DENY_ALL_PROBE_PATH) {
        Ok(_) => Err(format!(
            "open({DENY_ALL_PROBE_PATH}) unexpectedly succeeded on a tokio worker thread \
             spawned after Landlock lockdown"
        )),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "[landlock-worker-thread] tokio worker thread: open({DENY_ALL_PROBE_PATH}) \
                 correctly denied: {err}"
            );
            Ok(())
        }
        Err(err) => Err(format!(
            "open({DENY_ALL_PROBE_PATH}) failed, but not with permission-denied: {err}"
        )),
    }
}
