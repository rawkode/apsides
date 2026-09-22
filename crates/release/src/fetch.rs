//! OCI transport is delegated to ORAS. Transport success never grants release trust.
use crate::{Result, manifest, reject};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseReference {
    pub oci: String,
    /// Digest of release.json, distinct from the enclosing OCI manifest digest.
    pub release_digest: String,
}

impl ReleaseReference {
    /// # Errors
    /// Rejects tags, URLs, credentials and malformed SHA-256 references.
    pub fn validate(&self) -> Result<()> {
        let Some((repository, digest)) = self.oci.split_once('@') else {
            return reject("release reference requires an immutable OCI digest");
        };
        if !manifest::is_digest(digest)
            || !manifest::is_digest(&self.release_digest)
            || repository.is_empty()
            || !repository.contains('/')
            || repository.starts_with(['/', '-', '.'])
            || repository.ends_with('/')
            || repository.contains("//")
            || repository.contains("..")
            || !repository
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_/:".contains(&c))
            || repository
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains(':'))
        {
            return reject("invalid immutable OCI release reference");
        }
        Ok(())
    }
}

/// A trusted CI tool path and its expected binary digest. Never sourced from an artifact.
pub struct OrasTool {
    /// Optional trusted CI CA bundle for a private registry.
    pub ca_file: Option<PathBuf>,
    pub executable: PathBuf,
    pub sha256: String,
}

/// Downloaded content bound to a digest, but not yet approved by any trust policy.
/// Its path may be passed to the normal verifier; it cannot supply staging authority.
pub struct FetchedRelease {
    reference: ReleaseReference,
    directory: tempfile::TempDir,
}

impl FetchedRelease {
    #[must_use]
    pub const fn reference(&self) -> &ReleaseReference {
        &self.reference
    }
    #[must_use]
    pub fn release_digest(&self) -> &str {
        &self.reference.release_digest
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// Fetch the exact referenced content before discovering protected cluster policy.
/// # Errors
/// Rejects invalid references, failed transport and a mismatched release digest.
/// Success establishes content identity only, never signature or deployment trust.
pub async fn fetch_release(
    reference: &ReleaseReference,
    tool: &OrasTool,
) -> Result<FetchedRelease> {
    reference.validate()?;
    let directory = tempfile::tempdir()?;
    pull(reference, tool, directory.path()).await?;
    let bytes = manifest::bounded_read(
        &manifest::contained(directory.path(), "release.json")?,
        manifest::MAX_DOCUMENT,
    )?;
    if manifest::raw_digest(&bytes) != reference.release_digest {
        return reject("resolved release differs from protected reference");
    }
    Ok(FetchedRelease {
        directory,
        reference: reference.clone(),
    })
}

async fn pull(reference: &ReleaseReference, tool: &OrasTool, destination: &Path) -> Result<()> {
    if !tool.executable.is_absolute() || !manifest::is_digest(&tool.sha256) {
        return reject("ORAS must use a trusted absolute path and binary digest");
    }
    let binary = tool.executable.canonicalize()?;
    if manifest::raw_digest(&manifest::bounded_read(&binary, 256 * 1024 * 1024)?) != tool.sha256 {
        return reject("ORAS executable digest mismatch");
    }
    let mut command = Command::new(binary);
    command.arg("pull");
    if let Some(ca_file) = &tool.ca_file {
        if !ca_file.is_absolute() || !ca_file.is_file() {
            return reject("registry CA must be a trusted absolute file path");
        }
        command.arg("--ca-file").arg(ca_file);
    }
    let mut child = command
        .args(["--no-tty", "--concurrency", "3", "--output"])
        .arg(destination)
        .arg(&reference.oci)
        .arg("--plain-http=false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let result = tokio::time::timeout(Duration::from_secs(300), child.wait()).await;
    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(_)) => reject("OCI release retrieval failed"),
        Ok(Err(_)) => reject("OCI release retrieval process failed"),
        Err(_) => {
            let _ = child.kill().await;
            reject("OCI release retrieval timed out")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TrustPolicy, verify_candidate};

    #[test]
    fn immutable_references_reject_ambiguous_transport_and_tags() -> Result<()> {
        let digest = format!("sha256:{}", "a".repeat(64));
        for repository in ["ghcr.io/org/release", "localhost:5000/org/release"] {
            ReleaseReference {
                oci: format!("{repository}@{digest}"),
                release_digest: digest.clone(),
            }
            .validate()?;
        }
        for reference in [
            "ghcr.io/org/release:latest".into(),
            format!("https://ghcr.io/org/release@{digest}"),
            format!("ghcr.io/org/release:tag@{digest}"),
            format!("user:password@ghcr.io/org/release@{digest}"),
            format!("ghcr.io/../release@{digest}"),
            format!("--output/path@{digest}"),
        ] {
            assert!(
                ReleaseReference {
                    oci: reference,
                    release_digest: digest.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            ReleaseReference {
                oci: format!("ghcr.io/org/release@{digest}"),
                release_digest: "bad".into()
            }
            .validate()
            .is_err()
        );
        Ok(())
    }
    #[cfg(unix)]
    fn fake_oras(directory: &Path, script: &str) -> Result<OrasTool> {
        use std::os::unix::fs::PermissionsExt;
        let executable = directory.join("oras");
        std::fs::write(&executable, script)?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        Ok(OrasTool {
            ca_file: None,
            sha256: manifest::raw_digest(script.as_bytes()),
            executable,
        })
    }

    fn reference() -> ReleaseReference {
        ReleaseReference {
            oci: format!("ghcr.io/org/release@sha256:{}", "a".repeat(64)),
            release_digest: format!("sha256:{}", "b".repeat(64)),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transport_uses_fixed_safe_arguments_and_pin() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("output");
        std::fs::create_dir(&output)?;
        let tool = fake_oras(
            directory.path(),
            r#"#!/bin/sh
[ "$#" = 8 ] || exit 10
[ "$1" = pull ] || exit 11
[ "$2" = --no-tty ] || exit 12
[ "$3" = --concurrency ] || exit 13
[ "$4" = 3 ] || exit 14
[ "$5" = --output ] || exit 15
[ "$8" = --plain-http=false ] || exit 16
printf '%s' "$7" > "$6/reference"
"#,
        )?;
        pull(&reference(), &tool, &output).await?;
        assert_eq!(
            std::fs::read_to_string(output.join("reference"))?,
            reference().oci
        );
        std::fs::remove_file(output.join("reference"))?;
        std::fs::write(&tool.executable, "#!/bin/sh\nexit 0\n")?;
        assert!(pull(&reference(), &tool, &output).await.is_err());
        assert!(!output.join("reference").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_diagnostics_do_not_escape_failure_boundary() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let tool = fake_oras(
            directory.path(),
            "#!/bin/sh\nprintf 'private-token-canary' >&2\nexit 1\n",
        )?;
        let result = pull(&reference(), &tool, directory.path()).await;
        match result {
            Err(error) => {
                assert!(error.to_string().contains("retrieval failed"));
                assert!(!error.to_string().contains("private-token-canary"));
            }
            Ok(()) => return reject("failed ORAS command was accepted"),
        }
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn successful_transport_cannot_bypass_reference_or_release_verification() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let tool = fake_oras(
            directory.path(),
            "#!/bin/sh\nprintf '{}' > \"$6/release.json\"\n",
        )?;
        let policy = TrustPolicy {
            schema_version: "apsides.dev/trust/v1".into(),
            repository: "https://github.com/example/project".into(),
            approved_compiler_digests: vec![],
            verifier: crate::VerifierTool {
                executable: "/bin/false".into(),
                sha256: reference().release_digest,
            },
            signature: crate::SignaturePolicy::OpenSsh {
                principal: "test".into(),
                allowed_signers: String::new(),
            },
        };
        let mismatch = fetch_release(&reference(), &tool).await;
        assert!(
            matches!(mismatch, Err(crate::Error::Rejected(message)) if message == "resolved release differs from protected reference")
        );
        let mut matched = reference();
        matched.release_digest = manifest::raw_digest(b"{}");
        let fetched = fetch_release(&matched, &tool).await?;
        assert_eq!(fetched.release_digest(), matched.release_digest);
        assert!(verify_candidate(fetched.path(), &policy).await.is_err());
        Ok(())
    }
    #[tokio::test]
    #[ignore = "requires an isolated TLS registry containing release.json with bytes {} and real ORAS"]
    async fn real_oras_transport_reaches_release_verification() -> Result<()> {
        let executable: PathBuf = std::env::var("APSIDES_TEST_ORAS")
            .map_err(|_| crate::Error::Rejected("test ORAS path missing".into()))?
            .into();
        let oci = std::env::var("APSIDES_TEST_OCI_REFERENCE")
            .map_err(|_| crate::Error::Rejected("test OCI reference missing".into()))?;
        let tool = OrasTool {
            ca_file: std::env::var_os("APSIDES_TEST_REGISTRY_CA").map(PathBuf::from),
            sha256: manifest::raw_digest(&std::fs::read(&executable)?),
            executable,
        };
        let reference = ReleaseReference {
            oci,
            release_digest: manifest::raw_digest(b"{}"),
        };
        let policy = TrustPolicy {
            schema_version: "apsides.dev/trust/v1".into(),
            repository: "https://github.com/example/project".into(),
            approved_compiler_digests: vec![],
            verifier: crate::VerifierTool {
                executable: "/bin/false".into(),
                sha256: reference.release_digest.clone(),
            },
            signature: crate::SignaturePolicy::OpenSsh {
                principal: "test".into(),
                allowed_signers: String::new(),
            },
        };
        // JSON schema failure establishes that real transport and both digest
        // checks succeeded, without treating this deliberately invalid release as trusted.
        let fetched = fetch_release(&reference, &tool).await?;
        assert!(matches!(
            verify_candidate(fetched.path(), &policy).await,
            Err(crate::Error::Json(_))
        ));
        let wrong_release = ReleaseReference {
            release_digest: format!("sha256:{}", "f".repeat(64)),
            ..reference.clone()
        };
        assert!(matches!(fetch_release(&wrong_release, &tool).await,
            Err(crate::Error::Rejected(reason)) if reason == "resolved release differs from protected reference"));
        let repository = reference
            .oci
            .split_once('@')
            .ok_or_else(|| crate::Error::Rejected("test reference malformed".into()))?
            .0;
        let missing = ReleaseReference {
            oci: format!("{repository}@sha256:{}", "0".repeat(64)),
            ..reference.clone()
        };
        assert!(matches!(fetch_release(&missing, &tool).await,
            Err(crate::Error::Rejected(reason)) if reason == "OCI release retrieval failed"));
        let untrusted = OrasTool {
            ca_file: None,
            ..tool
        };
        assert!(matches!(fetch_release(&reference, &untrusted).await,
            Err(crate::Error::Rejected(reason)) if reason == "OCI release retrieval failed"));
        Ok(())
    }
}
