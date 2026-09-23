//! Running the public `aps` CLI the way a deployment job does.
//!
//! Each run gets a cleared environment: the inherited tool allowlist, the
//! scenario's issuer kubeconfig as `KUBECONFIG`, and the scenario's inventory
//! database. Nothing else reaches the process.

use crate::{Error, Result, ensure, valid_digest};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use test_cluster::process::{Invocation, Output, Running};

/// Longer than `aps deploy`'s own 300 s protocol deadline plus connection and
/// verification time.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(600);

/// The CLI and the only state it is given.
pub struct Aps {
    executable: PathBuf,
    kubeconfig: PathBuf,
    database: PathBuf,
}

/// One finished `aps` run.
pub struct Run {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Aps {
    #[must_use]
    pub const fn new(executable: PathBuf, kubeconfig: PathBuf, database: PathBuf) -> Self {
        Self {
            executable,
            kubeconfig,
            database,
        }
    }

    /// `aps deploy --artifact <release> --executor <executor>`.
    ///
    /// # Errors
    /// Fails only when the process cannot run or exceeds its deadline; an
    /// unsuccessful deployment is reported in [`Run::success`].
    pub async fn deploy(&self, release: &Path, executor: &str) -> Result<Run> {
        Ok(self.invocation(release, executor).output().await?.into())
    }

    /// Start `aps deploy --artifact <release> --executor <executor>` and
    /// return at once, for a scenario that stops it part-way.
    ///
    /// # Errors
    /// Fails when the process cannot start.
    pub fn spawn(&self, release: &Path, executor: &str) -> Result<Running> {
        Ok(self.invocation(release, executor).spawn()?)
    }

    fn invocation(&self, release: &Path, executor: &str) -> Invocation {
        Invocation::new(&self.executable)
            .arg("deploy")
            .arg("--artifact")
            .arg(release)
            .args(["--executor", executor])
            .env("KUBECONFIG", &self.kubeconfig)
            .env("APSIDES_INVENTORY_DATABASE", &self.database)
            .timeout(DEPLOY_TIMEOUT)
    }
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Self {
            success: output.success,
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

/// Require `run` to have failed with `diagnostic` on stderr.
///
/// # Errors
/// Fails when the run succeeded or reported something else.
pub fn refused(run: &Run, diagnostic: &str) -> Result {
    ensure(
        !run.success && run.stderr.contains(diagnostic),
        format!(
            "aps deploy was not refused with `{diagnostic}`: {}",
            test_cluster::process::tail(&run.stderr, 2000)
        ),
    )
}

/// The fields of an `already-committed` report: a rerun of a deployment
/// whose transition had committed, which republishes its inventory.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlreadyCommitted {
    pub status: String,
    pub transition: String,
    pub release_digest: String,
    pub inventory: Inventory,
}

/// Parse the report of a rerun that found `transition` already committed.
///
/// # Errors
/// Fails when the run failed or reported anything else.
pub fn already_committed(
    run: &Run,
    release_digest: &str,
    transition: &str,
) -> Result<AlreadyCommitted> {
    ensure(
        run.success,
        format!(
            "aps deploy rerun failed: {}",
            test_cluster::process::tail(&run.stderr, 2000)
        ),
    )?;
    let report: AlreadyCommitted = serde_json::from_str(&run.stdout).map_err(|error| {
        Error::Check(format!(
            "aps deploy rerun report is not already-committed: {error}"
        ))
    })?;
    ensure(
        report.status == "already-committed"
            && report.release_digest == release_digest
            && report.transition == transition,
        "the rerun did not report the committed transition",
    )?;
    Ok(report)
}

/// The fields of a committed deployment report the scenarios assert on.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Committed {
    pub status: String,
    pub artifact_trust: String,
    pub release_digest: String,
    pub ir_digest: String,
    pub transition: String,
    pub generation: String,
    pub inventory: Inventory,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub revision: String,
    pub snapshot_id: String,
}

/// Parse the report of a successful run and require that it commits exactly
/// the release with `release_digest` and `ir_digest`.
///
/// # Errors
/// Fails when the run failed, its report is not one JSON document, or any
/// field differs from a verified commit of that release.
pub fn committed(run: &Run, release_digest: &str, ir_digest: &str) -> Result<Committed> {
    ensure(
        run.success,
        format!(
            "aps deploy failed: {}",
            test_cluster::process::tail(&run.stderr, 2000)
        ),
    )?;
    let report: Committed = serde_json::from_str(&run.stdout).map_err(|error| {
        Error::Check(format!(
            "aps deploy report is not the committed contract: {error}"
        ))
    })?;
    let bounded = |value: &str| !value.is_empty() && value.len() <= 256;
    ensure(
        report.status == "committed"
            && report.artifact_trust == "verified"
            && report.release_digest == release_digest
            && report.ir_digest == ir_digest
            && bounded(&report.transition)
            && bounded(&report.generation)
            && valid_digest(&report.inventory.snapshot_id)
            && !report.inventory.revision.is_empty()
            && report
                .inventory
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_digit()),
        "CLI did not report the exact committed candidate",
    )?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(report: &serde_json::Value) -> Run {
        Run {
            success: true,
            stdout: report.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn only_the_exact_committed_release_is_accepted() -> Result {
        let release = format!("sha256:{}", "a".repeat(64));
        let ir = format!("sha256:{}", "b".repeat(64));
        let report = json!({
            "status": "committed", "artifactTrust": "verified", "releaseDigest": release,
            "irDigest": ir, "transition": "t-1", "generation": "g-1",
            "inventory": {"revision": "3", "snapshotId": format!("sha256:{}", "c".repeat(64))},
            "review": {},
        });
        let committed = committed(&run(&report), &release, &ir)?;
        assert_eq!(committed.generation, "g-1");
        let mut other = report.clone();
        other["releaseDigest"] = json!(ir);
        assert!(super::committed(&run(&other), &release, &ir).is_err());
        let mut pending = report.clone();
        pending["status"] = json!("already-committed");
        assert!(super::committed(&run(&pending), &release, &ir).is_err());
        let mut revision = report;
        revision["inventory"]["revision"] = json!("3a");
        assert!(super::committed(&run(&revision), &release, &ir).is_err());
        let failed = Run {
            success: false,
            stdout: String::new(),
            stderr: "aps: denied".into(),
        };
        assert!(super::committed(&failed, &release, &ir).is_err());
        Ok(())
    }
}
