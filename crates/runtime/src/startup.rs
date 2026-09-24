//! The generated controller's real entrypoint (see the Apsides v0.3 plan,
//! "5c. Filesystem lockdown"): a short, synchronous startup phase that
//! reads every file the controller will ever need, builds every in-memory
//! value derived from those files, applies a deny-all Landlock ruleset to
//! this thread -- before any other thread in the process exists -- and only
//! then builds the tokio runtime and hands off to [`crate::run_locked`].
//!
//! **Why Landlock is applied before [`engine::ensure_platform_initialized`],
//! not after.** The Apsides v0.3 plan's own prose describes this ordering
//! the other way around, but `engine`'s Phase 5a spike gate
//! (`crates/engine/tests/landlock_order.rs`, proven, working code, not a
//! sketch) already establishes -- and this module follows -- the opposite,
//! *correct* order, and says so explicitly in its own module doc comment:
//! "Landlock **first**, on the main thread, before anything spawns a single
//! other thread; V8 platform init (and thus its worker thread pool)
//! **after** that; the tokio runtime ... after that." `ensure_platform_initialized`
//! spawns V8's background worker thread pool immediately (see that
//! function's own doc comment), so applying Landlock afterward would leave
//! those threads unrestricted -- and would also make this module's own
//! `count_process_threads() == 1` precondition assert meaningless, since V8
//! would already have created more than one thread by the time it ran.
//! Landlock-then-V8-then-tokio is the order `landlock_order.rs` actually
//! proves threads spawned after lockdown inherit it; this module reuses
//! that exact proof by calling the same [`engine::apply_deny_all_or_fail_closed`]
//! and [`engine::count_process_threads`] functions.

use std::env;
use std::path::Path;

use compiler_core::parse_and_validate;

use crate::{
    CONFIG_PATH_ENV, FixtureConfig, KUBECONFIG_MAX_BYTES, LockdownState, ProductionConfig,
    RuntimeError, StartupConfig, read_bounded_file, read_startup, run_locked,
    verify_embedded_handlers,
};

/// Everything read from the filesystem during startup, already validated
/// and converted into the in-memory values `run_locked` and its callees
/// need -- nothing downstream of this point ever opens a file again.
pub(crate) enum PreparedStartup {
    Production {
        config: ProductionConfig,
        files: Box<crate::production::ProductionStartupFiles>,
    },
    Fixture {
        config: FixtureConfig,
        kubeconfig_bytes: Vec<u8>,
    },
}

/// The generated controller's `fn main` calls this directly.
///
/// Synchronous, no `#[tokio::main]`: every file read, every Landlock
/// precondition and the lockdown itself all happen on this one call stack,
/// on the process's main thread, before a tokio runtime -- and therefore
/// before any of its worker threads -- exists.
///
/// # Errors
///
/// Returns an error for invalid IR or startup configuration, a missing or
/// oversized required file, an unsupported/unenforced Landlock lockdown
/// (unless fixture mode's `allowNoLandlock` escape hatch is set), failed
/// protected activation, or a Kubernetes operation failure.
pub fn start(
    ir_json: &'static str,
    handlers: &'static [(&'static str, &'static str)],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let plan = parse_and_validate(ir_json)?;
    verify_embedded_handlers(&plan, handlers)?;

    let path = env::var(CONFIG_PATH_ENV)
        .map_err(|_| RuntimeError::Configuration(format!("{CONFIG_PATH_ENV} is required")))?;
    let startup = read_startup(Path::new(&path))?;

    // (a)+(b): every required file, read into memory now, and every
    // in-memory value (DER certs, base64 identity data, the kube
    // `Config`/DNS resolver config) derived from those bytes -- never a
    // file path handed to `kube` for it to read again later.
    let (prepared, allow_no_landlock) = match startup {
        StartupConfig::Production(config) => {
            let files = crate::production::read_production_startup_files(&config)?;
            (
                PreparedStartup::Production {
                    config,
                    files: Box::new(files),
                },
                false,
            )
        }
        StartupConfig::Fixture(config) => {
            let kubeconfig_bytes = read_bounded_file(
                &config.kubeconfig,
                KUBECONFIG_MAX_BYTES,
                "dedicated kubeconfig",
            )?;
            let allow_no_landlock = config.allow_no_landlock;
            (
                PreparedStartup::Fixture {
                    config,
                    kubeconfig_bytes,
                },
                allow_no_landlock,
            )
        }
    };

    // (e): deny-all Landlock, on the main thread, before any other thread
    // in this process exists -- checked, not assumed (see this module's
    // doc comment on why this precondition and the lockdown itself both
    // come *before* V8 platform init, not after).
    let thread_count = engine::count_process_threads()
        .map_err(|error| RuntimeError::Configuration(format!("landlock precondition: {error}")))?;
    if thread_count != 1 {
        return Err(Box::new(RuntimeError::Configuration(format!(
            "expected exactly one thread before Landlock lockdown, found {thread_count} \
             entries in /proc/self/task"
        ))));
    }
    let lockdown_outcome = engine::apply_deny_all_or_fail_closed(allow_no_landlock)
        .map_err(|error| RuntimeError::Configuration(format!("Landlock lockdown: {error}")))?;
    let lockdown = lockdown_state_for(lockdown_outcome);

    // (c): only now -- this spawns V8's background worker thread pool,
    // which must inherit the lockdown just applied above.
    engine::ensure_platform_initialized();

    // (f): only now does a tokio runtime -- and therefore any of its own
    // worker threads -- get built.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            RuntimeError::Configuration(format!("failed to build the tokio runtime: {error}"))
        })?;
    rt.block_on(run_locked(&plan, prepared, handlers, lockdown))?;
    Ok(())
}

/// `engine::LockdownOutcome::NotEnforced` is only ever reachable here when
/// `allow_no_landlock` was `true` -- `apply_deny_all_or_fail_closed` itself
/// already refused an unenforced ruleset for `allow_no_landlock == false`,
/// which every production startup passes (`ProductionConfig` has no such
/// field at all -- see [`LockdownState`]'s own doc comment).
const fn lockdown_state_for(outcome: engine::LockdownOutcome) -> LockdownState {
    match outcome {
        engine::LockdownOutcome::Enforced => LockdownState::Landlock,
        engine::LockdownOutcome::NotEnforced => LockdownState::NoneFixture,
    }
}

#[cfg(test)]
mod tests {
    use super::lockdown_state_for;
    use crate::{LockdownState, StartupConfig};

    #[test]
    fn lockdown_state_mapping_matches_the_landlock_outcome() {
        assert_eq!(
            lockdown_state_for(engine::LockdownOutcome::Enforced),
            LockdownState::Landlock
        );
        assert_eq!(
            lockdown_state_for(engine::LockdownOutcome::NotEnforced),
            LockdownState::NoneFixture
        );
    }

    type TestResult<T = ()> = std::result::Result<T, String>;

    /// Fixture-mode acceptance: `allowNoLandlock` parses, and defaults to
    /// `false` when absent -- see the Apsides v0.3 plan, Phase 5c, item 4.
    #[test]
    fn fixture_config_accepts_allow_no_landlock_and_defaults_to_false() -> TestResult {
        let without = serde_json::json!({
            "mode": "fixture",
            "acknowledgement": "disposable-cluster-only",
            "irDigest": "sha256:unused",
            "clusterBinding": "fixture",
            "kubeconfig": "/tmp/kubeconfig",
            "context": "kind-fixture",
            "pollIntervalMillis": 100,
            "maxWatchCycles": 1,
        });
        let StartupConfig::Fixture(config) = serde_json::from_value::<StartupConfig>(without)
            .map_err(|error| format!("valid without the field: {error}"))?
        else {
            return Err("expected fixture mode".into());
        };
        assert!(!config.allow_no_landlock);

        let with = serde_json::json!({
            "mode": "fixture",
            "acknowledgement": "disposable-cluster-only",
            "irDigest": "sha256:unused",
            "clusterBinding": "fixture",
            "kubeconfig": "/tmp/kubeconfig",
            "context": "kind-fixture",
            "pollIntervalMillis": 100,
            "maxWatchCycles": 1,
            "allowNoLandlock": true,
        });
        let StartupConfig::Fixture(config) = serde_json::from_value::<StartupConfig>(with)
            .map_err(|error| format!("valid with the field: {error}"))?
        else {
            return Err("expected fixture mode".into());
        };
        assert!(config.allow_no_landlock);
        Ok(())
    }

    /// Production-mode rejection: `ProductionConfig` has no
    /// `allowNoLandlock` field at all, and `StartupConfig`'s
    /// `deny_unknown_fields` rejects the extra key outright -- the escape
    /// hatch is structurally unreachable in production mode, not merely
    /// unused. See the Apsides v0.3 plan, Phase 5c, item 2.
    #[test]
    fn production_config_rejects_allow_no_landlock() -> TestResult {
        let with_escape_hatch = serde_json::json!({
            "mode": "production",
            "apiAccess": "proxy",
            "controlNamespace": "apsides-system",
            "binding": {
                "domainId": "example",
                "clusterBinding": "cluster",
                "generationId": "generation-a",
                "buildDigest": format!("sha256:{}", "1".repeat(64)),
                "irDigest": format!("sha256:{}", "2".repeat(64)),
                "policyDigest": format!("sha256:{}", "3".repeat(64)),
                "capabilityDigest": format!("sha256:{}", "4".repeat(64)),
                "serviceAccountName": "controller",
                "controllerImage": format!("example/controller@sha256:{}", "5".repeat(64)),
                "maxHandlerHeapBytes": 0,
            },
            "pollIntervalMillis": 1000,
            "baseBackoffMillis": 100,
            "maxBackoffMillis": 1000,
            "minMutationIntervalMillis": 10,
            "diagnosticsBind": "127.0.0.1:9090",
            "allowNoLandlock": true,
        });
        assert!(serde_json::from_value::<StartupConfig>(with_escape_hatch).is_err());

        // The same document, minus the escape hatch, parses fine -- proving
        // the rejection above is specifically about `allowNoLandlock`, not
        // some unrelated shape mistake in this test's fixture.
        let without = serde_json::json!({
            "mode": "production",
            "apiAccess": "proxy",
            "controlNamespace": "apsides-system",
            "binding": {
                "domainId": "example",
                "clusterBinding": "cluster",
                "generationId": "generation-a",
                "buildDigest": format!("sha256:{}", "1".repeat(64)),
                "irDigest": format!("sha256:{}", "2".repeat(64)),
                "policyDigest": format!("sha256:{}", "3".repeat(64)),
                "capabilityDigest": format!("sha256:{}", "4".repeat(64)),
                "serviceAccountName": "controller",
                "controllerImage": format!("example/controller@sha256:{}", "5".repeat(64)),
                "maxHandlerHeapBytes": 0,
            },
            "pollIntervalMillis": 1000,
            "baseBackoffMillis": 100,
            "maxBackoffMillis": 1000,
            "minMutationIntervalMillis": 10,
            "diagnosticsBind": "127.0.0.1:9090",
        });
        let StartupConfig::Production(config) = serde_json::from_value::<StartupConfig>(without)
            .map_err(|error| format!("valid without the field: {error}"))?
        else {
            return Err("expected production mode".into());
        };
        assert_eq!(config.control_namespace, "apsides-system");
        Ok(())
    }
}
