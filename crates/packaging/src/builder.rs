//! Local images by pinned reference: present in the engine's store, Linux,
//! for a supported platform and carrying the pinned repository digest.

use crate::{Error, Platform, Result, Stage, command::Run, redact::TAIL_LINES, reference::pinned};
use container::Engine;
use std::{path::Path, time::Duration};

const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);

/// The builder image as the engine's store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Builder {
    /// The pinned reference from the build profile.
    pub reference: String,
    pub platform: Platform,
    /// The engine's image ID, which the compile runs by.
    pub id: String,
    pub repo_digests: Vec<String>,
    /// The image's `Config.Env`.
    pub env: Vec<String>,
}

/// Inspects the builder image `reference` in `engine`'s store through the
/// engine CLI at `program`. aps never pulls.
///
/// # Errors
/// Fails when the image is not in the store, the engine cannot answer, the
/// store holds something other than one Linux image of a supported platform,
/// or the image does not carry the reference's digest.
pub fn inspect(engine: Engine, program: &Path, reference: &str) -> Result<Builder> {
    inspect_image(engine, program, reference, "builder", Stage::Builder)
}

/// [`inspect`] for the image playing `role`, failing in `stage`.
pub(crate) fn inspect_image(
    engine: Engine,
    program: &Path,
    reference: &str,
    role: &str,
    stage: Stage,
) -> Result<Builder> {
    let fail = |detail: String| Error::new(stage, detail);
    let digest = pinned(reference).ok_or_else(|| {
        fail(format!(
            "{role} image {reference} is not a repository@sha256:<64 hex> reference"
        ))
    })?;
    let captured = Run::new(program, container::inspect_args(reference))
        .timeout(INSPECT_TIMEOUT)
        .capture()
        .map_err(|failure| {
            let missing = failure.tail.to_lowercase();
            if failure.code().is_some()
                && ["no such image", "image not known", "no such object"]
                    .iter()
                    .any(|needle| missing.contains(needle))
            {
                fail(format!(
                    "{role} image {reference} is not in the local {} image store; run `{} pull \
                     {reference}` (aps build never pulls)",
                    engine.name(),
                    program.display()
                ))
            } else {
                fail(failure.describe(&format!("{} image inspect", program.display()), TAIL_LINES))
            }
        })?;
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let images =
        container::parse_inspection(engine, &stdout).map_err(|error| fail(error.to_string()))?;
    let [image] = images.as_slice() else {
        return Err(fail(format!(
            "{} image inspect {reference} returned {} images, not one",
            program.display(),
            images.len()
        )));
    };
    let platform = Platform::from_inspect(&image.os, &image.architecture).ok_or_else(|| {
        fail(format!(
            "{role} image {reference} is {}/{}; only linux/amd64 and linux/arm64 are supported",
            image.os, image.architecture
        ))
    })?;
    let carries_digest = image.repo_digests.iter().any(|repo_digest| {
        repo_digest
            .rsplit_once('@')
            .is_some_and(|(_, d)| d == digest)
    });
    if !carries_digest {
        return Err(fail(format!(
            "local image {} does not carry the pinned repository digest {digest}",
            image.id
        )));
    }
    Ok(Builder {
        reference: reference.to_owned(),
        platform,
        id: image.id.clone(),
        repo_digests: image.repo_digests.clone(),
        env: image.env.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::fake_tool;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    const DIGEST: &str = "sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922";

    fn reference() -> String {
        format!("rust@{DIGEST}")
    }

    fn engine(output: &str) -> std::io::Result<(tempfile::TempDir, std::path::PathBuf)> {
        let directory = tempfile::tempdir()?;
        let program = fake_tool(directory.path(), "podman", output)?;
        Ok((directory, program))
    }

    fn image(architecture: &str, repo_digest: &str) -> String {
        format!(
            "cat <<'EOF'\n[{{\"Id\":\"a3535ab4\",\"Os\":\"linux\",\"Architecture\":\"{architecture}\",\
             \"RepoDigests\":[\"docker.io/library/rust@{repo_digest}\"],\
             \"Config\":{{\"Env\":[\"PATH=/usr/bin\"]}}}}]\nEOF"
        )
    }

    #[test]
    fn an_inspected_image_records_its_identity() -> TestResult {
        let (_directory, program) = engine(&image("amd64", DIGEST))?;
        let builder = inspect(Engine::Podman, &program, &reference())?;
        assert_eq!(
            builder,
            Builder {
                reference: reference(),
                platform: Platform::Amd64,
                id: "a3535ab4".into(),
                repo_digests: vec![format!("docker.io/library/rust@{DIGEST}")],
                env: vec!["PATH=/usr/bin".into()],
            }
        );
        Ok(())
    }

    #[test]
    fn a_missing_image_says_how_to_pull_it() -> TestResult {
        let (_directory, program) =
            engine("echo 'Error: rust@sha256:x: image not known' >&2; exit 125")?;
        let error = inspect(Engine::Podman, &program, &reference())
            .err()
            .ok_or("accepted")?;
        assert_eq!(
            error.detail,
            format!(
                "builder image {} is not in the local Podman image store; run `{} pull {}` (aps \
                 build never pulls)",
                reference(),
                program.display(),
                reference()
            )
        );
        let (_directory, program) =
            engine("echo 'Cannot connect to the Docker daemon' >&2; exit 1")?;
        let error = inspect(Engine::Docker, &program, &reference())
            .err()
            .ok_or("accepted")?;
        assert!(error.detail.contains("image inspect failed"));
        assert!(error.detail.contains("Cannot connect to the Docker daemon"));
        Ok(())
    }

    #[test]
    fn a_foreign_platform_or_digest_is_rejected() -> TestResult {
        let (_directory, program) = engine(&image("s390x", DIGEST))?;
        let error = inspect(Engine::Podman, &program, &reference())
            .err()
            .ok_or("accepted")?;
        assert!(error.detail.contains("is linux/s390x"));
        let other = format!("sha256:{}", "0".repeat(64));
        let (_directory, program) = engine(&image("amd64", &other))?;
        let error = inspect(Engine::Podman, &program, &reference())
            .err()
            .ok_or("accepted")?;
        assert_eq!(
            error.detail,
            format!("local image a3535ab4 does not carry the pinned repository digest {DIGEST}")
        );
        assert!(inspect(Engine::Podman, &program, "rust:latest").is_err());
        Ok(())
    }
}
