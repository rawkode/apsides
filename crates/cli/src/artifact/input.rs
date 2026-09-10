use crate::{Result, cli};
use release::{FetchedRelease, OrasTool, ReleaseReference};
use std::{
    env,
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

/// Where a deployment reads its release from when nothing else is configured.
const DEFAULT_ARTIFACT: &str = "dist/apsides";

/// The largest release reference document the CLI will read.
const MAX_REFERENCE_BYTES: u64 = 16 * 1024;

/// Own remote downloads for the entire deployment, including verification,
/// staging and retry checks that read files again.
pub enum ArtifactInput {
    Local(PathBuf),
    Fetched(FetchedRelease),
}
impl ArtifactInput {
    pub fn expected_digest(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Fetched(fetched) => Some(fetched.release_digest()),
        }
    }
    pub fn path(&self) -> &Path {
        match self {
            Self::Local(path) => path,
            Self::Fetched(fetched) => fetched.path(),
        }
    }
}

/// Open the release this deployment should use, downloading it when the caller
/// supplied an immutable OCI reference.
///
/// # Errors
/// Returns an error if both a local artifact and a reference are in play, if
/// the reference is unbounded, malformed or not immutable, or if the configured
/// `oras` tool is missing.
pub async fn open(arguments: &cli::Deploy) -> Result<ArtifactInput> {
    let reference = arguments
        .artifact_reference
        .clone()
        .or_else(|| env::var_os("APSIDES_RELEASE_REFERENCE").map(PathBuf::from));
    let local = select_local(arguments.artifact.clone(), reference.as_deref())?;
    let Some(reference) = reference else {
        return Ok(ArtifactInput::Local(local));
    };
    let reference = read_reference(&reference)?;
    let executable = env::var_os("APSIDES_ORAS")
        .map(PathBuf::from)
        .ok_or("OCI deployment requires APSIDES_ORAS with an absolute tool path")?;
    let sha256 = env::var("APSIDES_ORAS_SHA256")
        .map_err(|_| "OCI deployment requires APSIDES_ORAS_SHA256")?;
    let tool = OrasTool {
        executable,
        sha256,
        ca_file: env::var_os("APSIDES_REGISTRY_CA").map(PathBuf::from),
    };
    Ok(ArtifactInput::Fetched(
        release::fetch_release(&reference, &tool).await?,
    ))
}

fn select_local(local: Option<PathBuf>, reference: Option<&Path>) -> Result<PathBuf> {
    if local.is_some() && reference.is_some() {
        return Err("choose a local artifact or an OCI release reference, not both".into());
    }
    Ok(local.unwrap_or_else(|| PathBuf::from(DEFAULT_ARTIFACT)))
}

fn read_reference(path: &Path) -> Result<ReleaseReference> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_REFERENCE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REFERENCE_BYTES {
        return Err("release reference exceeds 16 KiB".into());
    }
    let input = std::str::from_utf8(&bytes)?;
    let reference: ReleaseReference = serde_json::from_value(compiler_core::parse_strict(input)?)?;
    reference.validate()?;
    Ok(reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_selection_is_explicit_and_does_not_mask_remote_configuration() -> Result<()> {
        assert_eq!(select_local(None, None)?, PathBuf::from(DEFAULT_ARTIFACT));
        assert_eq!(
            select_local(Some("build".into()), None)?,
            PathBuf::from("build")
        );
        assert!(select_local(Some("build".into()), Some(Path::new("release-ref.json"))).is_err());
        Ok(())
    }
    #[test]
    fn reference_file_is_bounded_strict_and_immutable() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("reference.json");
        let digest = format!("sha256:{}", "a".repeat(64));
        let reference = serde_json::json!({"oci":format!("ghcr.io/example/controller@{digest}"),"releaseDigest":digest});
        std::fs::write(&path, serde_json::to_vec(&reference)?)?;
        read_reference(&path)?;
        for bytes in [
            b"{\"oci\":\"one\",\"oci\":\"two\"}".to_vec(),
            vec![b' '; 16 * 1024 + 1],
            b"{\"oci\":\"ghcr.io/example/controller:latest\",\"releaseDigest\":\"invalid\"}"
                .to_vec(),
        ] {
            std::fs::write(&path, bytes)?;
            assert!(read_reference(&path).is_err());
        }
        Ok(())
    }
}
