//! `entropy_source` (see the Apsides v0.3 plan, "5c. Filesystem lockdown"
//! review): proves `ensure_platform_initialized`'s
//! `v8::V8::set_entropy_source` call actually supplies V8 with real
//! randomness under a Landlock lockdown, instead of the silent
//! time-based-seed fallback V8 uses when it cannot read `/dev/urandom`
//! (denied like every other path once Landlock is applied) and no entropy
//! source is installed.
//!
//! A time-based fallback seed affects the platform RNG (used for, among
//! other things, mmap/ASLR hints) and every isolate's own RNG
//! (`Math.random` and the internal string-hash seed -- a predictable hash
//! seed reopens hash flooding through attacker-influenced input, e.g. an
//! `apsides.fetch` response header/body a handler then indexes into an
//! object).
//!
//! The proof: two entirely separate, independently locked-down processes
//! (not just two isolates in one process, which could share a coarse
//! process-start-time-based fallback seed) each apply Landlock, initialize
//! the V8 platform, run one handler that returns `Math.random()`, and print
//! it. The parent process asserts the two values differ, and that
//! [`engine::entropy_source_invocation_count`] is nonzero in the child
//! (proving V8 genuinely called the installed source, the "at minimum"
//! bar) -- not merely that `v8::V8::set_entropy_source` was *called*
//! (`ensure_platform_initialized` calls it unconditionally, every run,
//! which alone would prove nothing about whether V8 ever uses it).
//!
//! `harness = false` (see `Cargo.toml`), the same reason
//! `tests/landlock_order.rs` uses it: the child process's own `fn main`
//! applies a real Landlock lockdown to its main thread before any other
//! thread exists, exactly like a real controller's `runtime::start`.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use common::{count_process_threads, handler_invocation, wrap_as_handler};

const CHILD_MARKER_ENV: &str = "APSIDES_ENTROPY_TEST_CHILD";

fn main() -> Result<(), String> {
    if std::env::var_os(CHILD_MARKER_ENV).is_some() {
        return run_child();
    }
    run_parent()
}

/// Spawns two independent child processes (see [`CHILD_MARKER_ENV`]) and
/// compares their `Math.random()` output.
fn run_parent() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("failed to resolve this test binary's own path: {err}"))?;
    let value_a = spawn_child(&exe)?;
    let value_b = spawn_child(&exe)?;
    if (value_a - value_b).abs() < f64::EPSILON {
        return Err(format!(
            "two independent locked-down processes produced the identical Math.random() \
             value ({value_a}); V8's entropy source is not providing real randomness"
        ));
    }
    eprintln!(
        "[entropy-source] two independent locked-down processes produced distinct \
         Math.random() values: {value_a} != {value_b}"
    );
    Ok(())
}

fn spawn_child(exe: &std::path::Path) -> Result<f64, String> {
    let output = std::process::Command::new(exe)
        .env(CHILD_MARKER_ENV, "1")
        .output()
        .map_err(|err| format!("failed to spawn the locked-down child process: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "locked-down child process exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|err| format!("child process wrote non-UTF-8 stdout: {err}"))?;
    stdout
        .trim()
        .parse::<f64>()
        .map_err(|err| format!("child process did not print a number ({stdout:?}): {err}"))
}

/// Runs as the re-exec'd child: lock down, initialize the platform, run one
/// handler that returns `Math.random()`, print it to stdout (the parent's
/// only channel back), and confirm the entropy source was actually
/// invoked.
fn run_child() -> Result<(), String> {
    let thread_count = count_process_threads().map_err(|err| err.to_string())?;
    if thread_count != 1 {
        return Err(format!(
            "expected exactly one thread before Landlock lockdown, found {thread_count} \
             entries in /proc/self/task"
        ));
    }

    let status = common::apply_deny_all_or_fail_closed(true)?;
    let supported = status != common::LockdownOutcome::NotEnforced;
    eprintln!("[entropy-source child] lockdown status: {status:?} (supported={supported})");

    engine::ensure_platform_initialized();

    let value = run_math_random()?;

    let invocations = engine::entropy_source_invocation_count();
    if invocations == 0 {
        return Err(
            "V8 never invoked the entropy source ensure_platform_initialized installed \
             (entropy_source_invocation_count() == 0)"
                .to_owned(),
        );
    }
    eprintln!("[entropy-source child] entropy source invoked {invocations} time(s)");

    println!("{value}");
    Ok(())
}

fn run_math_random() -> Result<f64, String> {
    let usage = Arc::new(AtomicUsize::new(0));
    let bundle = wrap_as_handler("return Math.random();");
    let mut invocation = handler_invocation(&bundle);
    invocation.limits.timeout = std::time::Duration::from_secs(10);
    let report = engine::invoke_handler_with_watchdog(invocation, &usage);
    let value = report
        .result
        .map_err(|error| format!("handler failed: {error}"))?;
    value
        .as_f64()
        .ok_or_else(|| format!("handler did not return a number: {value:?}"))
}
