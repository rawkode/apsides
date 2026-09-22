#![forbid(unsafe_code)]
//! Runs a fixture release's generated reactor cases on a disposable kind
//! cluster.
//!
//! The runner never reads the caller's Kubernetes configuration. It creates a
//! kind cluster whose kubeconfig lives in a private temporary directory,
//! imports the release's controller image and the fixture images into the
//! node's containerd by digest, denies all egress except the API server, and
//! starts the release's controller in fixture mode with a short-lived
//! credential for its generated `ServiceAccount`. Every case in the release's
//! test plan is then driven through that cluster, and the cluster is deleted
//! unless the caller asked to keep it.

mod cases;
mod cluster;
mod contract;
mod crds;
mod driver;
mod images;
mod manifests;
mod oracle;
mod process;
mod report;
mod suite;
#[cfg(test)]
mod testing;

pub use contract::FixtureRelease;
pub use report::{
    ArtifactIdentity, Assertion, CanaryScan, CaseRow, CaseStatus, Evidence, Isolation, Report,
};
pub use suite::{Options, run};

/// The only node image the runner starts: Kubernetes v1.34.3, the version
/// `aps deploy` accepts.
pub const NODE_IMAGE: &str =
    "kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48";

/// The image behind the controller's loopback API proxy and the egress
/// sentinel. It must already be in the local Docker image store.
pub const PROXY_IMAGE: &str = "nginxinc/nginx-unprivileged:alpine@sha256:d9083fe47768377ef55dedafd67d4da7c2f2bc2bece7554954f29359deb0dce9";

/// The workload image every fixture IR substitutes for production images.
pub const FIXTURE_IMAGE: &str = "apsides-fixture:local";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fixture release rejected: {0}")]
    Input(String),
    #[error("test infrastructure failed: {0}")]
    Infrastructure(String),
    #[error("{program} failed: {detail}")]
    Process { program: String, detail: String },
    #[error("deadline exceeded: {0}")]
    Deadline(String),
    #[error("{0}")]
    Case(String),
    #[error("Kubernetes: {0}")]
    Kube(#[from] kube::Error),
    #[error("Kubernetes configuration: {0}")]
    KubeConfig(#[from] kube::config::KubeconfigError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Release(#[from] release::Error),
    #[error(transparent)]
    Core(#[from] compiler_core::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn input<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Input(message.into()))
}

fn infrastructure<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Infrastructure(message.into()))
}
