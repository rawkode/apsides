use crate::{
    Result,
    manifest::{self, IntegrityVerifiedCandidate, MAX_DOCUMENT, ReleaseMode},
    reject,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sigstore_verify::{
    VerificationPolicy,
    trust_root::TrustedRoot,
    types::{Bundle, Sha256Hash, SignatureContent},
};
use std::path::Path;

/// The release file holding the Sigstore bundle that attests `release.json`.
pub const PROVENANCE_BUNDLE: &str = "provenance.sigstore.json";
/// The DSSE payload type of an in-toto statement.
const IN_TOTO_PAYLOAD: &str = "application/vnd.in-toto+json";

/// This policy is loaded from platform-owned state, never from a release bundle.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustPolicy {
    pub schema_version: String,
    pub repository: String,
    pub approved_compiler_digests: Vec<String>,
    pub signature: SignaturePolicy,
}

/// The Sigstore instance and CI workload whose signatures the platform accepts.
///
/// Only keyless signatures are accepted: a short-lived certificate issued by a
/// certificate authority in `trusted_root` to the workload these fields name.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignaturePolicy {
    /// The instance's Sigstore trusted root, kept as the platform stored it so
    /// the policy digest recorded at staging is the digest of that document.
    pub trusted_root: Value,
    /// Require a verified transparency log inclusion proof. Public-good
    /// Sigstore logs every signature; GitHub's instance for private
    /// repositories does not, and its bundles carry RFC 3161 timestamps.
    pub transparency_log: bool,
    /// Require a verified signed certificate timestamp, which public-good
    /// Fulcio certificates carry and GitHub's instance's do not.
    pub certificate_transparency: bool,
    /// The certificate's exact subject alternative name, for GitHub Actions
    /// the workflow and ref that signed: `https://github.com/OWNER/REPO/.github/workflows/FILE@REF`.
    pub certificate_identity: String,
    /// The OIDC issuer the certificate authority accepted the identity from.
    pub certificate_issuer: String,
    /// The commit of the signing workflow.
    pub signer_digest: String,
    /// The ref the release was built from, such as `refs/heads/main`.
    pub source_ref: String,
}

fn nonempty(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn listed(root: &Value, field: &str) -> bool {
    root[field]
        .as_array()
        .is_some_and(|entries| !entries.is_empty())
}

impl SignaturePolicy {
    fn validate(&self) -> Result<()> {
        if !nonempty(&self.certificate_identity, 1024)
            || !nonempty(&self.certificate_issuer, 512)
            || !self.certificate_issuer.starts_with("https://")
            || !commit(&self.signer_digest)
            || !self.source_ref.starts_with("refs/")
            || !nonempty(&self.source_ref, 256)
        {
            return reject("incomplete pinned signer trust policy");
        }
        // Each requirement needs the authority that satisfies it, and the
        // log being off needs the timestamp authorities that replace it.
        let root = &self.trusted_root;
        if !listed(root, "certificateAuthorities")
            || (self.transparency_log && !listed(root, "tlogs"))
            || (!self.transparency_log && !listed(root, "timestampAuthorities"))
            || (self.certificate_transparency && !listed(root, "ctlogs"))
        {
            return reject("trusted root cannot satisfy the signer trust policy");
        }
        Ok(())
    }
}

fn check_statement(statement: &Value, subject: &str, digest: &str) -> Result<()> {
    if statement["_type"] != "https://in-toto.io/Statement/v1"
        || statement["predicateType"] != "https://slsa.dev/provenance/v1"
    {
        return reject("expected an in-toto SLSA v1 provenance statement");
    }
    let Some(subjects) = statement["subject"].as_array() else {
        return reject("attestation has no subjects");
    };
    if !subjects.iter().any(|entry| {
        entry["name"] == subject
            && entry["digest"]["sha256"].as_str() == digest.strip_prefix("sha256:")
    }) {
        return reject("attestation does not bind this release manifest");
    }
    Ok(())
}

/// Verify `bundle` as a signature over the artifact named `subject` with
/// SHA-256 `digest`, by the workload `policy` approves, built from `commit`
/// of `repository`.
///
/// # Errors
/// Rejects a malformed bundle, a signature Sigstore verification refuses, any
/// certificate claim that is absent or differs from the policy, and a
/// statement that is not SLSA provenance for this subject.
pub(crate) fn verify_bundle(
    bundle: &[u8],
    subject: &str,
    digest: &str,
    policy: &SignaturePolicy,
    repository: &str,
    commit: &str,
) -> Result<()> {
    crate::crypto::install()?;
    let bundle = Bundle::from_json(std::str::from_utf8(bundle).map_err(|_| invalid_bundle())?)
        .map_err(|_| invalid_bundle())?;
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return reject("provenance bundle must carry a DSSE attestation");
    };
    if envelope.payload_type != IN_TOTO_PAYLOAD {
        return reject("provenance bundle must carry an in-toto statement");
    }
    let artifact = digest
        .strip_prefix("sha256:")
        .and_then(|hex| Sha256Hash::from_hex(hex).ok())
        .ok_or_else(|| crate::Error::Rejected("invalid release digest".into()))?;
    let root = TrustedRoot::from_json(&policy.trusted_root.to_string())
        .map_err(|_| crate::Error::Rejected("invalid Sigstore trusted root".into()))?;
    let mut requirements = VerificationPolicy::any_identity()
        .require_identity(policy.certificate_identity.as_str())
        .require_issuer(policy.certificate_issuer.as_str());
    if !policy.transparency_log {
        // Signing time then comes only from timestamps the root's
        // authorities issued; Sigstore verification still requires one.
        requirements = requirements.skip_tlog_unsafe();
    }
    if !policy.certificate_transparency {
        requirements = requirements.skip_sct();
    }
    let verified =
        sigstore_verify::verify(artifact, &bundle, &requirements, &root).map_err(|_| {
            crate::Error::Rejected("Sigstore verification rejected the provenance".into())
        })?;

    let Some(claims) = verified
        .certificate()
        .map(|certificate| &certificate.ci_claims)
    else {
        return reject("provenance certificate carries no workload claims");
    };
    // Every claim must be present: a certificate issued to a person, or by a
    // provider that omits one, matches nothing.
    let expected = [
        (
            &claims.build_signer_uri,
            policy.certificate_identity.as_str(),
        ),
        (&claims.build_signer_digest, &policy.signer_digest),
        (&claims.source_repository_uri, repository),
        (&claims.source_repository_digest, commit),
        (&claims.source_repository_ref, &policy.source_ref),
    ];
    if !expected
        .iter()
        .all(|(claim, value)| claim.as_deref() == Some(*value))
    {
        return reject("provenance certificate does not name the approved workload and source");
    }
    if !matches!(claims.runner_environment.as_deref(), Some(runner) if runner != "self-hosted") {
        return reject("provenance was not signed on a provider-hosted runner");
    }
    // The statement is read from the envelope Sigstore just verified.
    let statement = manifest::json(envelope.payload.as_bytes(), "provenance statement")?;
    check_statement(&statement, subject, digest)
}

fn invalid_bundle() -> crate::Error {
    crate::Error::Rejected("provenance bundle is not a valid Sigstore bundle".into())
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
    if policy.schema_version != "apsides.dev/trust/v2"
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
    policy.signature.validate()?;
    if !commit(&release.source.commit) {
        return reject("release source commit must be a full commit digest");
    }
    let bundle =
        manifest::bounded_read(&manifest::contained(root, PROVENANCE_BUNDLE)?, MAX_DOCUMENT)?;
    let signature = policy.signature.clone();
    let repository = policy.repository.clone();
    let source_commit = release.source.commit.clone();
    let release_digest = manifest::raw_digest(&bytes);
    let checked = release_digest.clone();
    let provenance = bundle.clone();
    tokio::task::spawn_blocking(move || {
        verify_bundle(
            &provenance,
            "release.json",
            &checked,
            &signature,
            &repository,
            &source_commit,
        )
    })
    .await
    .map_err(|_| crate::Error::Rejected("signature verification did not complete".into()))??;
    Ok(IntegrityVerifiedCandidate {
        release_digest,
        manifest: release,
        compiled,
        provenance_digest: manifest::raw_digest(&bundle),
        trust_policy_digest: compiler_core::digest(&serde_json::to_value(policy)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real attestation GitHub Actions signed through public-good Sigstore,
    /// logged in Rekor with an embedded SCT, from sigstore-rust's test data
    /// (Apache-2.0), and the public-good trusted root it verifies under.
    const BUNDLE: &[u8] =
        include_bytes!("../tests/data/sigstore/public-good-attestation.sigstore.json");
    const ROOT: &str = include_str!("../tests/data/sigstore/public-good-trusted-root.json");
    const SUBJECT: &str = "signed-package-2.1.0-hb0f4dca_0.conda";
    const DIGEST: &str = "sha256:54303491a8418fbed24344b513546182c29b43bf282ceb433af65e2299f9271f";
    const REPOSITORY: &str = "https://github.com/prefix-dev/sigstore-example";
    const COMMIT: &str = "193b5bd7d3985809503963ae400594ea16df31cf";
    /// Its statement is a conda publish attestation rather than SLSA
    /// provenance, so this is the rejection once every other check passes.
    const NOT_PROVENANCE: &str =
        "release rejected: expected an in-toto SLSA v1 provenance statement";

    fn policy() -> Result<SignaturePolicy> {
        Ok(SignaturePolicy {
            trusted_root: serde_json::from_str(ROOT)?,
            transparency_log: true,
            certificate_transparency: true,
            certificate_identity: format!(
                "{REPOSITORY}/.github/workflows/action.yaml@refs/heads/main"
            ),
            certificate_issuer: "https://token.actions.githubusercontent.com".into(),
            signer_digest: COMMIT.into(),
            source_ref: "refs/heads/main".into(),
        })
    }

    fn rejection(policy: &SignaturePolicy, repository: &str, commit: &str) -> Result<String> {
        verify_bundle(BUNDLE, SUBJECT, DIGEST, policy, repository, commit)
            .err()
            .map(|error| error.to_string())
            .ok_or_else(|| crate::Error::Rejected("expected verification to fail".into()))
    }

    #[test]
    fn a_real_attestation_passes_the_log_certificate_transparency_and_claim_checks() -> Result<()> {
        let policy = policy()?;
        policy.validate()?;
        assert_eq!(rejection(&policy, REPOSITORY, COMMIT)?, NOT_PROVENANCE);
        Ok(())
    }

    #[test]
    fn a_real_attestation_is_refused_for_any_other_workload_or_source() -> Result<()> {
        let edits: [fn(&mut SignaturePolicy); 6] = [
            |changed| changed.certificate_identity.push('x'),
            |changed| changed.certificate_issuer = "https://accounts.google.com".into(),
            |changed| changed.signer_digest = "0".repeat(40),
            |changed| changed.source_ref = "refs/heads/release".into(),
            |changed| changed.trusted_root["certificateAuthorities"] = serde_json::json!([]),
            |changed| changed.trusted_root["ctlogs"] = serde_json::json!([]),
        ];
        for edit in edits {
            let mut changed = policy()?;
            edit(&mut changed);
            assert_ne!(rejection(&changed, REPOSITORY, COMMIT)?, NOT_PROVENANCE);
        }
        let policy = policy()?;
        assert_ne!(
            rejection(&policy, "https://github.com/prefix-dev/fork", COMMIT)?,
            NOT_PROVENANCE
        );
        assert_ne!(
            rejection(&policy, REPOSITORY, &"0".repeat(40))?,
            NOT_PROVENANCE
        );
        let other = format!("sha256:{}", "0".repeat(64));
        assert!(verify_bundle(BUNDLE, SUBJECT, &other, &policy, REPOSITORY, COMMIT).is_err());
        Ok(())
    }
}
