//! The full cluster suite against a prepared fixture release.
//!
//! `cuenv task reactorFixture` prepares the inputs; this test then needs
//! Docker and kind, and names them through the environment:
//!
//! ```sh
//! APSIDES_REACTOR_RELEASE=work/reactor/release \
//! APSIDES_REACTOR_FIXTURE_IMAGE=work/reactor/fixture-image.tar \
//!   cargo test -p test-cluster --test kind -- --ignored
//! ```

use std::{env, path::PathBuf};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn required(name: &str) -> Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} must name a prepared input").into())
}

#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and a fixture release from `cuenv task reactorFixture`"]
async fn every_generated_case_passes_on_a_disposable_cluster() -> Result {
    let output = tempfile::tempdir()?;
    let options = test_cluster::Options {
        release: required("APSIDES_REACTOR_RELEASE")?,
        fixture_image: required("APSIDES_REACTOR_FIXTURE_IMAGE")?,
        crds: Vec::new(),
        output: output.path().join("results"),
        keep_cluster: false,
        progress: None,
    };
    let report = test_cluster::run(&options, std::future::pending()).await?;
    let failed = report
        .cases
        .iter()
        .filter(|row| row.status != test_cluster::CaseStatus::Passed)
        .map(|row| format!("{}:{} {}", row.reaction_id, row.scenario, row.detail))
        .collect::<Vec<_>>();
    assert!(failed.is_empty(), "{failed:#?}");
    assert!(report.passed(), "{:?}", report.error);
    Ok(())
}
