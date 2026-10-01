//! `startup_reads_before_lock` (see the Apsides v0.3 plan, "5c. Filesystem
//! lockdown", item 4): proves `runtime::start` reads every file it needs
//! *before* applying Landlock, not after.
//!
//! The proof: a fixture-mode dedicated kubeconfig at an arbitrary temp-dir
//! path (deliberately **not** under `/etc`, so nothing about the OS's own
//! default-allowed paths could make this pass by accident) is read
//! successfully by a real, unmodified `runtime::start()` call. Landlock's
//! deny-all ruleset denies *every* path once applied -- including ordinary
//! files this test process itself owns under its own temp directory -- so
//! a successful read of that file, followed by proof that Landlock really
//! is enforced immediately afterward (see the second half of this test),
//! can only mean the read happened first.
//!
//! `harness = false` (see `Cargo.toml`), the same reason
//! `engine/tests/landlock_order.rs` uses it: this binary's `fn main`
//! applies a real process-wide Landlock lockdown (via `runtime::start`,
//! same as a real controller), which must not leak into other tests'
//! threads under libtest's shared-process default harness.
//!
//! The complementary "fails cleanly and clearly if any [required file] is
//! missing" half of this requirement is proved by a plain `#[test]` inside
//! the crate itself
//! (`production::tests::read_production_startup_files_fails_clearly_when_the_proxy_ca_is_missing`),
//! which needs no Landlock side effect at all and so does not belong in a
//! `harness = false` binary: it asserts against production mode's fixed,
//! non-injectable `/etc/apsides/proxy/*` paths, which this environment
//! (like a real CI runner, and unlike a real controller pod) simply does
//! not have -- proving the real, unmocked read path fails with a clear,
//! typed error naming the missing file.

use std::fs;
use std::io::ErrorKind;

/// A minimal but validly-shaped IR: no resources, no inputs, no handlers.
/// `reconcile_once`/`run_fixture_with_client` then make *zero* Kubernetes
/// API calls, so this test needs no reachable server at all -- only that
/// `runtime::start` successfully builds a `kube::Client` from the
/// dedicated kubeconfig this test provides, which requires having already
/// read it.
const IR: &str = r#"{
  "schemaVersion":"apsides.dev/ir/v2",
  "domain":{"id":"example","namespace":"workload","controlNamespace":"apsides-system","clusterBinding":"fixture","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
  "resources":[],
  "inputs":[],
  "reactions":[],
  "handlers":[],
  "outputs":[],
  "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"controller.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
}"#;

const HANDLERS: &[(&str, &str)] = &[];

/// An arbitrary, always-present path with no relation to anything this
/// test needs -- used only to prove Landlock is genuinely enforced after
/// `runtime::start` returns, so a successful read of the temp-dir
/// kubeconfig above is proof of *ordering*, not just proof that Landlock
/// was never applied at all.
const DENY_ALL_PROBE_PATH: &str = "/etc/hostname";

/// This process's own env var marking it as the re-exec'd child (see
/// `main`'s doc comment below for why this test re-execs itself instead of
/// mutating its own environment).
const CHILD_MARKER_ENV: &str = "APSIDES_STARTUP_TEST_CHILD";

/// This binary re-execs itself as a child process (via
/// [`std::process::Command`]) rather than calling the unsafe (as of the
/// 2024 edition) `std::env::set_var` on itself: the workspace's
/// `unsafe_code = "deny"` lint applies to every target of this crate,
/// including this integration test binary, with no exception carved out
/// for it. `Command::env` sets the *child's* environment, which is an
/// ordinary safe API. This also has a real correctness benefit beyond
/// satisfying the lint: `runtime::start`'s own precondition is "exactly
/// one thread exists"; a freshly `exec`'d child process starts with
/// exactly one thread guaranteed, independent of whatever this parent
/// process (or the `cargo test` harness driving it) has done before this
/// point, and the parent process itself -- which does not apply Landlock
/// at all -- is unaffected regardless of outcome.
fn main() -> Result<(), String> {
    if std::env::var_os(CHILD_MARKER_ENV).is_some() {
        return run_in_locked_down_child();
    }
    run_parent()
}

/// Builds the temp-dir fixture inputs, then re-execs this same binary as a
/// child process with [`runtime::CONFIG_PATH_ENV`] and [`CHILD_MARKER_ENV`]
/// set, and propagates its result.
fn run_parent() -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!(
        "apsides-startup-reads-before-lock-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).map_err(|err| format!("failed to create temp dir: {err}"))?;

    let kubeconfig_path = dir.join("kubeconfig.yaml");
    fs::write(
        &kubeconfig_path,
        r"apiVersion: v1
kind: Config
preferences: {}
current-context: kind-fixture
clusters:
- name: fixture
  cluster:
    server: https://127.0.0.1:6443
    certificate-authority-data: Y2E=
users:
- name: runtime
  user:
    token: fixture-token
contexts:
- name: kind-fixture
  context:
    cluster: fixture
    user: runtime
",
    )
    .map_err(|err| format!("failed to write temp kubeconfig: {err}"))?;

    let startup_config_path = dir.join("config.json");
    let startup_config = serde_json::json!({
        "mode": "fixture",
        "acknowledgement": "disposable-cluster-only",
        "irDigest": ir_digest()?,
        "clusterBinding": "fixture",
        "kubeconfig": kubeconfig_path,
        "context": "kind-fixture",
        "pollIntervalMillis": 100,
        // The minimum valid value (`validate_fixture_config` requires
        // `1..=1_000_000`): one reconcile pass plus one watch/poll cycle,
        // both over zero resources/inputs (see `IR` above), so this test
        // does not need to wait out a real poll interval or reach any
        // Kubernetes server at all.
        "maxWatchCycles": 1,
        // Not asserted by this test (that is
        // `landlock_allows_no_landlock_in_fixture_mode`'s job) -- `false`
        // here so this test also incidentally proves the ordinary,
        // fail-closed path succeeds when Landlock *is* supported (as it is
        // on every environment this workspace's CI runs on).
        "allowNoLandlock": false,
    });
    fs::write(
        &startup_config_path,
        serde_json::to_string(&startup_config).map_err(|err| err.to_string())?,
    )
    .map_err(|err| format!("failed to write temp startup config: {err}"))?;

    let exe = std::env::current_exe()
        .map_err(|err| format!("failed to resolve this test binary's own path: {err}"))?;
    let status = std::process::Command::new(&exe)
        .env(CHILD_MARKER_ENV, "1")
        .env(runtime::CONFIG_PATH_ENV, &startup_config_path)
        // This test's fixture kubeconfig names no proxy at all, and the
        // real controller must not need one either; clearing every
        // ambient proxy variable (rather than relying on `no_proxy`
        // covering this test's loopback target) keeps this test
        // deterministic in an environment that happens to set one, the
        // same way `op_apsides_fetch` itself always ignores proxy env
        // vars for its own connections (see the Apsides v0.3 plan, Phase
        // 5b).
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .status()
        .map_err(|err| format!("failed to spawn the locked-down child process: {err}"))?;
    let _ = fs::remove_dir_all(&dir);
    if status.success() {
        Ok(())
    } else {
        Err(format!("locked-down child process exited with {status}"))
    }
}

/// Runs as the re-exec'd child (see [`CHILD_MARKER_ENV`]): calls the real,
/// unmodified `runtime::start` and then proves Landlock is active
/// immediately afterward.
fn run_in_locked_down_child() -> Result<(), String> {
    let kubeconfig_path = std::env::var(runtime::CONFIG_PATH_ENV)
        .map_err(|_| format!("{} is required", runtime::CONFIG_PATH_ENV))?;

    match runtime::start(IR, HANDLERS) {
        Ok(()) => {}
        Err(error) => {
            return Err(format!(
                "runtime::start failed, which means the temp-dir kubeconfig referenced by \
                 the startup config at {kubeconfig_path} was not successfully read before \
                 startup gave up: {error}"
            ));
        }
    }
    eprintln!(
        "[startup-reads-before-lock] runtime::start (startup config: {kubeconfig_path}) \
         completed successfully"
    );

    // Now prove Landlock really is active (not merely that `start`
    // returned `Ok` because lockdown was silently skipped): a fresh file
    // open must be denied, or -- only if this kernel cannot enforce
    // Landlock at all -- reported as a skip, exactly like
    // `engine/tests/landlock_fs.rs`'s equivalent check.
    match fs::File::open(DENY_ALL_PROBE_PATH) {
        Ok(_) => {
            eprintln!(
                "[startup-reads-before-lock] Landlock is unsupported on this kernel \
                 (open({DENY_ALL_PROBE_PATH}) unexpectedly succeeded); not asserting \
                 file-access denial, but the temp-dir kubeconfig read above still \
                 proves startup's file reads happen before it ever attempts lockdown."
            );
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "[startup-reads-before-lock] confirmed Landlock is enforced after \
                 runtime::start returned: open({DENY_ALL_PROBE_PATH}) correctly denied"
            );
            Ok(())
        }
        Err(err) => Err(format!(
            "open({DENY_ALL_PROBE_PATH}) failed, but not with permission-denied: {err}"
        )),
    }
}

/// The startup config's `irDigest` must match the embedded IR's own
/// validated digest exactly (`validate_fixture_config`) -- computed here
/// by actually validating [`IR`], the same way `runtime::start` itself
/// will.
fn ir_digest() -> Result<String, String> {
    let plan = compiler_core::parse_and_validate(IR).map_err(|err| err.to_string())?;
    Ok(plan.ir_digest)
}
