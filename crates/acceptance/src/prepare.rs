//! Building and signing what the scenarios deploy.
//!
//! `acceptance-prepare` builds releases of `examples/cnpg/initial`,
//! `examples/cnpg`, `examples/cnpg/cleanup` and the `cnpg-owner` program with
//! `aps build` and `aps release`, exactly as a user's pipeline would, from a build profile written per run from the repository's
//! pins. It signs each release's provenance with an OpenSSH key generated for
//! this run alone, deletes the private key, and writes the trust policy that
//! approves only that key's public half, the releases' compiler digests and
//! the system `ssh-keygen`. It also fetches the `CloudNativePG` operator
//! manifest and checks its digest. Everything goes under one output
//! directory, which is never committed.

use crate::{
    Error, Result, cnpg, ensure, infrastructure,
    inputs::{Inputs, SCHEMA},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use test_cluster::process::Invocation;

/// The builder image `aps build` compiles controllers in.
pub const BUILDER_IMAGE: &str =
    "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922";
/// The source repository recorded in provenance and approved by the trust
/// policy.
pub const REPOSITORY: &str = "https://github.com/rawkode/apsides";
/// The signer principal the trust policy approves.
pub const PRINCIPAL: &str = "acceptance@apsides.invalid";
/// The verifier the trust policy pins; `release` requires a root-owned path.
pub const VERIFIER: &str = "/usr/bin/ssh-keygen";
/// The OpenSSH signature namespace `release` verifies provenance under.
const SIGNATURE_NAMESPACE: &str = "apsides.provenance.v1";
/// The repository of the controller images the releases record.
const IMAGE_REPOSITORY: &str = "localhost/apsides/cnpg-controller";
pub(crate) const BUILD_TIMEOUT: Duration = Duration::from_mins(45);
/// Every program the scenarios deploy, by the directory prepare writes its
/// release to, in the order `inputs.json` names them.
const RELEASES: [(&str, &str); 4] = [
    ("initial", "examples/cnpg/initial/program.tsx"),
    ("successor", "examples/cnpg/program.tsx"),
    ("cleanup", "examples/cnpg/cleanup/program.tsx"),
    ("owner", "crates/acceptance/programs/cnpg-owner/program.tsx"),
];

/// Where prepare reads from and writes to.
pub struct Options {
    /// The repository checkout holding `examples/` and `scripts/`.
    pub root: PathBuf,
    /// A directory prepare owns; earlier contents are replaced.
    pub output: PathBuf,
    /// The `aps` executable to build and release with.
    pub aps: PathBuf,
    /// `cargo-cyclonedx`, which `aps build` records in the build profile.
    pub sbom_tool: PathBuf,
}

/// Build, release and sign every program in [`RELEASES`] and write
/// `inputs.json`.
///
/// # Errors
/// Fails when any tool is missing or any build, release, signature,
/// verification or download step fails.
pub async fn prepare(options: &Options) -> Result<Inputs> {
    let root = options.root.canonicalize()?;
    let aps = absolute(&options.aps)?;
    let sbom_tool = absolute(&options.sbom_tool)?;
    std::fs::create_dir_all(&options.output)?;
    let output = options.output.canonicalize()?;
    let platform = host_platform()?;
    let commit = Invocation::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .run()
        .await?
        .trim()
        .to_owned();
    let profile = write_profile(&root, &output, &sbom_tool, &platform)?;

    let key = output.join("signing-key");
    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(key.with_extension("pub"));
    Invocation::new(VERIFIER)
        .args(["-q", "-t", "ed25519", "-N", "", "-C", PRINCIPAL, "-f"])
        .arg(&key)
        .run()
        .await?;
    let public = std::fs::read_to_string(key.with_extension("pub"))?;

    let mut releases = Vec::new();
    for (name, source) in RELEASES {
        let directory = output.join(name);
        if directory.exists() {
            std::fs::remove_dir_all(&directory)?;
        }
        std::fs::create_dir_all(&directory)?;
        let release = build_release(&Build {
            aps: &aps,
            source: &root.join(source),
            directory: &directory,
            profile: &profile,
            platform: &platform,
            commit: &commit,
        })
        .await?;
        sign(&key, &release).await?;
        releases.push(release);
    }
    std::fs::remove_file(&key)?;

    let trust_policy = output.join("trust-policy.json");
    std::fs::write(
        &trust_policy,
        serde_json::to_vec_pretty(&trust(&releases, &public)?)?,
    )?;
    for release in &releases {
        Invocation::new(&aps)
            .arg("verify-release")
            .arg("--artifact")
            .arg(release)
            .arg("--trust-policy")
            .arg(&trust_policy)
            .run()
            .await?;
    }

    let operator_manifest = output.join("cnpg-operator.yaml");
    fetch_operator(&operator_manifest).await?;

    let [
        initial_release,
        successor_release,
        cleanup_release,
        owner_release,
    ] = releases.as_slice()
    else {
        return infrastructure("expected one release per program");
    };
    let inputs = Inputs {
        schema_version: SCHEMA.into(),
        aps,
        initial_release: initial_release.clone(),
        successor_release: successor_release.clone(),
        cleanup_release: cleanup_release.clone(),
        owner_release: owner_release.clone(),
        trust_policy,
        operator_manifest,
        results: output.join("results"),
    };
    std::fs::write(
        output.join("inputs.json"),
        serde_json::to_vec_pretty(&inputs)?,
    )?;
    Ok(inputs)
}

struct Build<'a> {
    aps: &'a Path,
    source: &'a Path,
    directory: &'a Path,
    profile: &'a Path,
    platform: &'a str,
    commit: &'a str,
}

/// The build profile, written from the repository's pins; never committed.
///
/// # Errors
/// Fails when the runtime base is not pinned or a file cannot be read or
/// written.
pub fn write_profile(
    root: &Path,
    output: &Path,
    sbom_tool: &Path,
    platform: &str,
) -> Result<PathBuf> {
    let runtime = std::fs::read_to_string(root.join("scripts/runtime-base/reference"))?
        .trim()
        .to_owned();
    ensure(
        runtime.contains("@sha256:"),
        "the runtime base reference is not pinned",
    )?;
    let profile = output.join("apsides.build.json");
    std::fs::write(
        &profile,
        serde_json::to_vec(&json!({
            "schemaVersion": "apsides.dev/build-profile/v1",
            "builderImage": BUILDER_IMAGE,
            "runtimeImage": runtime,
            "platform": platform,
            "sbomTool": sbom_tool,
            "sbomToolSha256": crate::digest(&std::fs::read(sbom_tool)?),
            "sourceDateEpoch": 0,
        }))?,
    )?;
    Ok(profile)
}

/// `aps build` then `aps release`; returns the release directory.
async fn build_release(build: &Build<'_>) -> Result<PathBuf> {
    let artifact = build.directory.join("build");
    Invocation::new(build.aps)
        .arg("build")
        .arg("--source")
        .arg(build.source)
        .arg("--output")
        .arg(&artifact)
        .arg("--profile")
        .arg(build.profile)
        .envs_for_build()
        .timeout(BUILD_TIMEOUT)
        .run()
        .await?;
    let result: Value =
        serde_json::from_slice(&std::fs::read(artifact.join("build-result.json"))?)?;
    let image_digest = result["imageDigest"].as_str().unwrap_or_default();
    ensure(
        crate::valid_digest(image_digest),
        "aps build reported no image digest",
    )?;
    let release = build.directory.join("release");
    Invocation::new(build.aps)
        .arg("release")
        .arg("--artifact")
        .arg(&artifact)
        .arg("--binary")
        .arg(artifact.join("linux-binary/apsides-controller"))
        .arg("--oci")
        .arg(artifact.join("image/controller.oci.tar"))
        .arg("--image")
        .arg(format!("{IMAGE_REPOSITORY}@{image_digest}"))
        .args(["--platform", build.platform])
        .args(["--repository", REPOSITORY])
        .args(["--commit", build.commit])
        .arg("--build-inputs")
        .arg(artifact.join("build-inputs.json"))
        .arg("--sbom")
        .arg(artifact.join("sbom/sbom.json"))
        .arg("--output")
        .arg(&release)
        .run()
        .await?;
    Ok(release)
}

/// Sign `release/provenance.json` into `release/provenance.sig`.
async fn sign(key: &Path, release: &Path) -> Result {
    let statement = release.join("provenance.json");
    let produced = release.join("provenance.json.sig");
    let _ = std::fs::remove_file(&produced);
    Invocation::new(VERIFIER)
        .args(["-Y", "sign", "-n", SIGNATURE_NAMESPACE, "-f"])
        .arg(key)
        .arg(&statement)
        .run()
        .await?;
    std::fs::rename(&produced, release.join("provenance.sig"))?;
    Ok(())
}

/// The trust policy approving the ephemeral signer, the releases' compiler
/// digests and the system `ssh-keygen`.
fn trust(releases: &[PathBuf], public_key: &str) -> Result<Value> {
    let mut compilers = Vec::new();
    for release in releases {
        let (manifest, _, _) = release::read_release(release)?;
        if !compilers.contains(&manifest.compiler_digest) {
            compilers.push(manifest.compiler_digest);
        }
    }
    compilers.sort();
    Ok(json!({
        "schemaVersion": "apsides.dev/trust/v1",
        "repository": REPOSITORY,
        "approvedCompilerDigests": compilers,
        "verifier": {"executable": VERIFIER, "sha256": crate::digest(&std::fs::read(VERIFIER)?)},
        "signature": {
            "kind": "open-ssh",
            "principal": PRINCIPAL,
            "allowedSigners": format!("{PRINCIPAL} {}", public_key.trim()),
        },
    }))
}

/// Fetch the operator manifest unless a copy with the pinned digest is
/// already there, and accept it only with that digest.
async fn fetch_operator(path: &Path) -> Result {
    let current = std::fs::read(path).ok().map(|bytes| crate::digest(&bytes));
    if current.as_deref() != Some(cnpg::OPERATOR_MANIFEST_SHA256) {
        let pending = path.with_extension("download");
        let _ = std::fs::remove_file(&pending);
        Invocation::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-filesize",
                "8388608",
            ])
            .arg("--output")
            .arg(&pending)
            .arg(cnpg::OPERATOR_MANIFEST_URL)
            .timeout(Duration::from_secs(300))
            .run()
            .await?;
        std::fs::rename(&pending, path)?;
    }
    cnpg::operator_documents(&std::fs::read(path)?)?;
    Ok(())
}

/// `linux/<arch>` for the machine `aps build` runs on.
///
/// # Errors
/// Fails on architectures the builder images do not cover.
pub fn host_platform() -> Result<String> {
    match std::env::consts::ARCH {
        "aarch64" => Ok("linux/arm64".into()),
        "x86_64" => Ok("linux/amd64".into()),
        other => Err(Error::Infrastructure(format!(
            "unsupported host architecture {other}"
        ))),
    }
}

/// The first `program` on `PATH`.
#[must_use]
pub fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|error| Error::Input(format!("{}: {error}", path.display())))
}

/// Variables `aps build` needs beyond the inherited allowlist: where Cargo,
/// rustup and Deno keep their caches.
pub(crate) trait BuildEnvironment {
    fn envs_for_build(self) -> Self;
}

impl BuildEnvironment for Invocation {
    fn envs_for_build(self) -> Self {
        ["CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "DENO_DIR"]
            .into_iter()
            .fold(self, |invocation, name| match std::env::var_os(name) {
                Some(value) => invocation.env(name, value),
                None => invocation,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_platform_is_a_linux_platform() -> Result {
        assert!(host_platform()?.starts_with("linux/"));
        Ok(())
    }
}
