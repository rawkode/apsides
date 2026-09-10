use crate::Result;
use release::{FetchedRelease, OrasTool, ReleaseReference};
use std::{
    collections::BTreeMap,
    env,
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

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

pub async fn open(options: &BTreeMap<String, String>) -> Result<ArtifactInput> {
    let reference = options
        .get("--artifact-reference")
        .map(PathBuf::from)
        .or_else(|| env::var_os("APSIDES_RELEASE_REFERENCE").map(PathBuf::from));
    let local = select_local(
        options.get("--artifact").map(PathBuf::from),
        reference.as_deref(),
    )?;
    let Some(reference) = reference else {
        return Ok(ArtifactInput::Local(local));
    };
    let reference = read_reference(&reference)?;
    let executable = env::var_os("APSIDES_ORAS")
        .map(PathBuf::from)
        .ok_or("OCI deployment requires APSIDES_ORAS with an absolute tool path")?;
    let sha256 =
        env::var("APSIDES_ORAS_SHA256").map_err(|_| "OCI deployment requires APSIDES_ORAS_SHA256")?;
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
    Ok(local.unwrap_or_else(|| PathBuf::from("dist/apsides")))
}

fn read_reference(path: &Path) -> Result<ReleaseReference> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
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
        assert_eq!(select_local(None, None)?, PathBuf::from("dist/apsides"));
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
