//! Filesystem lockdown primitives (see the Apsides v0.3 plan, "5c.
//! Filesystem lockdown"): applying a deny-all Landlock ruleset to the
//! calling thread, and counting this process's live threads so a caller can
//! assert lockdown happens before any thread but the calling one exists.
//!
//! Moved here from the Phase 5a spike gate's `tests/common/mod.rs` (which
//! now calls these same functions instead of keeping its own copy -- see
//! `tests/landlock_fs.rs` and `tests/landlock_order.rs`), so `runtime`'s
//! real Phase 5c startup path and this crate's own proof-of-ordering tests
//! share one implementation rather than two that could drift apart.
//!
//! **Ordering, proven by `tests/landlock_order.rs`:** Landlock must be
//! applied on the main thread *before* [`crate::ensure_platform_initialized`]
//! (which spawns V8's background worker thread pool) and before any tokio
//! runtime is built -- `restrict_self()` only restricts the calling thread
//! directly (this crate's pinned Landlock ABI has no thread-sync capability
//! to retroactively restrict existing sibling threads), but a *new* thread
//! created after `restrict_self()` inherits the calling thread's security
//! context at `clone()`/spawn time regardless. Applying lockdown any later
//! would leave V8's or tokio's worker threads unrestricted.

use std::fs;

use landlock::{ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetStatus};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LockdownError {
    #[error("failed to read {path}: {source}")]
    ThreadCount {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Ruleset(#[from] landlock::RulesetError),
    #[error(
        "Landlock is not enforced on this kernel; refusing to continue without the \
         fixture-only allowNoLandlock escape hatch"
    )]
    NotEnforced,
}

/// Whether the deny-all ruleset [`apply_deny_all_or_fail_closed`] applied
/// was actually enforced by the running kernel.
///
/// The three `landlock::RulesetStatus` outcomes, kept distinct (not
/// collapsed together) so a caller can report exactly what the kernel
/// enforced, rather than treating `PartiallyEnforced` as indistinguishable
/// from `FullyEnforced`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockdownOutcome {
    /// Every handled filesystem-access right is denied for every path, for
    /// the calling thread and every thread created after it
    /// (`RulesetStatus::FullyEnforced`).
    Enforced,
    /// The running kernel's Landlock ABI does not support every handled
    /// right this crate requested (e.g. ABI V7, the effective ABI on every
    /// kernel older than Linux 7.1, lacks only `ResolveUnix`, added in ABI
    /// V9 -- `IoctlDev` is not the gap, it has been present since ABI V5),
    /// so only a subset is actually denied -- still real enforcement, on a
    /// best-effort basis, not a fail-closed condition
    /// (`RulesetStatus::PartiallyEnforced`; see `tests/landlock_fs.rs`'s own
    /// doc comment on this exact ABI V7 case, where the file-open-denial
    /// guarantee this crate actually relies on still holds). This is the
    /// normal, expected status on any pre-7.1 kernel, not an old-ABI edge
    /// case.
    PartiallyEnforced,
    /// The kernel could not enforce Landlock at all
    /// (`RulesetStatus::NotEnforced`); returned only when the caller passed
    /// `allow_unenforced: true`.
    NotEnforced,
}

/// Counts this process's threads via `/proc/self/task` (one entry per
/// thread, including the calling one).
///
/// Used to assert exactly one thread exists immediately before applying
/// Landlock, so lockdown genuinely happens before any other thread --
/// including one V8's platform init or a tokio runtime would create -- is
/// spawned, not merely before the caller happens to check for one. Reads
/// `/proc/self/task` itself (not e.g. `std::thread::available_parallelism`,
/// which reports CPU count, not live thread count), so it must run *before*
/// Landlock denies filesystem reads.
///
/// # Errors
///
/// Returns `Err` if `/proc/self/task` cannot be read.
pub fn count_process_threads() -> Result<usize, LockdownError> {
    let entries = fs::read_dir("/proc/self/task").map_err(|source| LockdownError::ThreadCount {
        path: "/proc/self/task",
        source,
    })?;
    let mut count = 0usize;
    for entry in entries {
        entry.map_err(|source| LockdownError::ThreadCount {
            path: "/proc/self/task",
            source,
        })?;
        count += 1;
    }
    Ok(count)
}

/// Applies a deny-all Landlock ruleset (every `AccessFs` right this crate's
/// pinned ABI knows, with zero `add_rules` exceptions -- no path is ever
/// allowed) to the calling thread.
///
/// Fails closed: if the running kernel cannot actually enforce Landlock
/// (`RulesetStatus::NotEnforced`), this returns
/// [`LockdownError::NotEnforced`] unless `allow_unenforced` is `true` --
/// which a caller must reserve for the plan's fixture-only
/// `allowNoLandlock` startup-config escape hatch, never for production.
///
/// # Errors
///
/// Returns `Err` if the Landlock ruleset itself could not be built or
/// applied (a real syscall/argument error, distinct from "unsupported
/// kernel"), or if the kernel could not enforce it and `allow_unenforced`
/// is `false`.
pub fn apply_deny_all_or_fail_closed(
    allow_unenforced: bool,
) -> Result<LockdownOutcome, LockdownError> {
    let abi = ABI::V9;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        // No `add_rules` call at all: every handled AccessFs right is
        // denied for every path, with no exceptions.
        .restrict_self()?;

    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(LockdownOutcome::Enforced),
        RulesetStatus::PartiallyEnforced => Ok(LockdownOutcome::PartiallyEnforced),
        RulesetStatus::NotEnforced => {
            if allow_unenforced {
                Ok(LockdownOutcome::NotEnforced)
            } else {
                Err(LockdownError::NotEnforced)
            }
        }
    }
}
