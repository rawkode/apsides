use crate::{
    Result,
    manifest::{self, IntegrityVerifiedCandidate, MAX_DOCUMENT, ReleaseMode},
    reject,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifierTool {
    pub executable: PathBuf,
    pub sha256: String,
}

/// This policy is loaded from platform-owned state, never from a release bundle.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustPolicy {
    pub schema_version: String,
    pub repository: String,
    pub approved_compiler_digests: Vec<String>,
    pub verifier: VerifierTool,
    pub signature: SignaturePolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SignaturePolicy {
    GithubActions {
        repository: String,
        #[serde(rename = "signerWorkflow")]
        signer_workflow: String,
        #[serde(rename = "signerDigest")]
        signer_digest: String,
        #[serde(rename = "certificateIdentity")]
        certificate_identity: String,
        #[serde(rename = "sourceRef")]
        source_ref: String,
        #[serde(rename = "trustedRoot")]
        trusted_root: String,
    },
    OpenSsh {
        principal: String,
        #[serde(rename = "allowedSigners")]
        allowed_signers: String,
    },
}

async fn capture(reader: impl AsyncRead + Unpin, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return reject("signature verifier output exceeded its limit");
    }
    Ok(bytes)
}

async fn run(tool: &Path, args: &[String], input: &[u8], directory: &Path) -> Result<Vec<u8>> {
    let mut child = Command::new(tool)
        .args(args)
        .current_dir(directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", directory)
        .env("GH_CONFIG_DIR", directory)
        .env("GH_HOST", "github.com")
        .env("LANG", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| crate::Error::Rejected("verification process stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| crate::Error::Rejected("verification process stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| crate::Error::Rejected("verification process stderr unavailable".into()))?;
    let operation = async {
        let send = async {
            stdin.write_all(input).await?;
            drop(stdin);
            Ok::<_, crate::Error>(())
        };
        let wait = async { Ok::<_, crate::Error>(child.wait().await?) };
        let ((), status, output, _diagnostic) = tokio::try_join!(
            send,
            wait,
            capture(stdout, MAX_DOCUMENT * 2),
            capture(stderr, 1024 * 1024)
        )?;
        if !status.success() {
            return reject("established signature verifier rejected the artifact or policy");
        }
        Ok(output)
    };
    tokio::time::timeout(Duration::from_secs(60), operation)
        .await
        .unwrap_or_else(|_| reject("signature verifier timed out"))
}

fn nonempty(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn check_statement(statement: &Value, release_digest: &str) -> Result<()> {
    if statement["_type"] != "https://in-toto.io/Statement/v1"
        || statement["predicateType"] != "https://slsa.dev/provenance/v1"
    {
        return reject("expected an in-toto SLSA v1 provenance statement");
    }
    let Some(subjects) = statement["subject"].as_array() else {
        return reject("attestation has no subjects");
    };
    if !subjects.iter().any(|subject| {
        subject["name"] == "release.json"
            && subject["digest"]["sha256"].as_str() == release_digest.strip_prefix("sha256:")
    }) {
        return reject("attestation does not bind this release manifest");
    }
    Ok(())
}

/// Verify release integrity and platform-approved signature identity.
///
/// `aps deploy` and `aps verify-release` both call this. A release carries no
/// test results; per-program behaviour is checked by `aps test` before signing.
///
/// # Errors
/// Rejects untrusted, incomplete, fixture-only, altered candidates and verifier failures.
pub async fn verify_candidate(
    root: &Path,
    policy: &TrustPolicy,
) -> Result<IntegrityVerifiedCandidate> {
    let (release, compiled, bytes) = manifest::read_release(root)?;
    if release.mode != ReleaseMode::Production || compiled.ir.test_only {
        return reject("fixture-substituted artifacts cannot be promoted to production");
    }
    if policy.schema_version != "apsides.dev/trust/v1"
        || release.source.repository != policy.repository
        || !policy
            .approved_compiler_digests
            .contains(&release.compiler_digest)
    {
        return reject("release source/compiler is not approved by platform trust policy");
    }
    if policy.approved_compiler_digests.is_empty()
        || policy.approved_compiler_digests.len() > 32
        || !policy
            .approved_compiler_digests
            .iter()
            .all(|digest| manifest::is_digest(digest))
    {
        return reject("invalid approved compiler policy");
    }
    if !policy.verifier.executable.is_absolute() || !manifest::is_digest(&policy.verifier.sha256) {
        return reject("verifier executable must be pinned by absolute path and SHA-256");
    }
    let tool = policy.verifier.executable.canonicalize()?;
    validate_tool_path(&tool)?;
    let tool_bytes = manifest::bounded_read(&tool, 256 * 1024 * 1024)?;
    if manifest::raw_digest(&tool_bytes) != policy.verifier.sha256 {
        return reject("signature verifier executable digest mismatch");
    }
    let scratch = tempfile::tempdir()?;
    let payload = scratch.path().join("release.json");
    std::fs::write(&payload, &bytes)?;
    let release_digest = manifest::raw_digest(&bytes);
    let provenance_digest = verify_signature(
        root,
        policy,
        &release,
        &release_digest,
        &tool,
        scratch.path(),
    )
    .await?;
    // Detect accidental mutation of the release while the external verifier ran.
    let (_, _, after) = manifest::read_release(root)?;
    if after != bytes {
        return reject("release changed during signature verification");
    }
    Ok(IntegrityVerifiedCandidate {
        manifest: release,
        compiled,
        release_digest,
        provenance_digest,
        trust_policy_digest: compiler_core::digest(&serde_json::to_value(policy)?)?,
    })
}

async fn verify_signature(
    root: &Path,
    policy: &TrustPolicy,
    release: &manifest::ReleaseManifest,
    release_digest: &str,
    tool: &Path,
    scratch: &Path,
) -> Result<String> {
    match &policy.signature {
        SignaturePolicy::OpenSsh {
            principal,
            allowed_signers,
        } => {
            verify_ssh(
                root,
                release,
                release_digest,
                tool,
                scratch,
                principal,
                allowed_signers,
            )
            .await
        }
        SignaturePolicy::GithubActions {
            repository,
            signer_workflow,
            signer_digest,
            certificate_identity,
            source_ref,
            trusted_root,
        } => {
            if format!("https://github.com/{repository}") != policy.repository
                || !nonempty(signer_workflow, 512)
                || !commit(signer_digest)
                || !nonempty(certificate_identity, 1024)
                || !source_ref.starts_with("refs/")
                || !nonempty(source_ref, 256)
                || trusted_root.len() as u64 > MAX_DOCUMENT
            {
                return reject("incomplete pinned GitHub workflow trust policy");
            }
            let bundle = manifest::bounded_read(
                &manifest::contained(root, "provenance.sigstore.json")?,
                MAX_DOCUMENT,
            )?;
            std::fs::write(scratch.join("bundle.jsonl"), &bundle)?;
            std::fs::write(scratch.join("trusted-root.jsonl"), trusted_root)?;
            let args = [
                "attestation",
                "verify",
                "release.json",
                "--bundle",
                "bundle.jsonl",
                "--custom-trusted-root",
                "trusted-root.jsonl",
                "--repo",
                repository,
                "--signer-workflow",
                signer_workflow,
                "--signer-digest",
                signer_digest,
                "--source-digest",
                &release.source.commit,
                "--source-ref",
                source_ref,
                "--cert-identity",
                certificate_identity,
                "--cert-oidc-issuer",
                "https://token.actions.githubusercontent.com",
                "--deny-self-hosted-runners",
                "--predicate-type",
                "https://slsa.dev/provenance/v1",
                "--format",
                "json",
            ]
            .map(str::to_owned);
            let output = run(tool, &args, &[], scratch).await?;
            let results: Value = serde_json::from_slice(&output)?;
            let Some(results) = results.as_array() else {
                return reject("GitHub verifier did not return verified attestations");
            };
            if !results.iter().any(|result| {
                check_statement(&result["verificationResult"]["statement"], release_digest).is_ok()
            }) {
                return reject("GitHub verifier returned no matching release statement");
            }
            Ok(manifest::raw_digest(&bundle))
        }
    }
}

async fn verify_ssh(
    root: &Path,
    release: &manifest::ReleaseManifest,
    release_digest: &str,
    tool: &Path,
    scratch: &Path,
    principal: &str,
    allowed_signers: &str,
) -> Result<String> {
    if !nonempty(principal, 256)
        || principal.contains(char::is_whitespace)
        || allowed_signers.len() > 64 * 1024
        || allowed_signers.is_empty()
    {
        return reject("invalid platform SSH signer policy");
    }
    let statement_bytes =
        manifest::bounded_read(&manifest::contained(root, "provenance.json")?, MAX_DOCUMENT)?;
    let signature =
        manifest::bounded_read(&manifest::contained(root, "provenance.sig")?, 64 * 1024)?;
    let statement = manifest::json(&statement_bytes)?;
    check_statement(&statement, release_digest)?;
    let external = &statement["predicate"]["buildDefinition"]["externalParameters"];
    if external["repository"] != release.source.repository
        || external["commit"] != release.source.commit
        || external["compilerDigest"] != release.compiler_digest
    {
        return reject("provenance does not match approved release build inputs");
    }
    std::fs::write(scratch.join("allowed-signers"), allowed_signers)?;
    std::fs::write(scratch.join("provenance.sig"), signature)?;
    let args = [
        "-Y",
        "verify",
        "-f",
        "allowed-signers",
        "-I",
        principal,
        "-n",
        "apsides.provenance.v1",
        "-s",
        "provenance.sig",
    ]
    .map(str::to_owned);
    run(tool, &args, &statement_bytes, scratch).await?;
    Ok(manifest::raw_digest(&statement_bytes))
}

fn validate_tool_path(tool: &Path) -> Result<()> {
    // Execute the OS-protected original: copying Apple's platform binaries causes
    // code-signing termination. Every resolved ancestor must remain root-owned.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        for path in tool.ancestors() {
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return reject("signature verifier must reside in an OS-protected root-owned path");
            }
        }
    }
    #[cfg(not(unix))]
    return reject("this platform has no qualified verifier executable protection");
    Ok(())
}
