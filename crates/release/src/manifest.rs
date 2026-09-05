use crate::{Result, reject};
use compiler_core::{CompiledPlan, canonical_bytes, digest, parse_and_validate, parse_strict};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path},
};

pub(crate) const MAX_DOCUMENT: u64 = 8 * 1024 * 1024;
const MAX_BINARY: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReleaseMode {
    Production,
    Fixture,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema_version: String,
    pub mode: ReleaseMode,
    pub ir_digest: String,
    pub image: Image,
    pub source: Source,
    pub compiler_digest: String,
    pub binary_digest: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Image {
    pub reference: String,
    pub platform: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub repository: String,
    pub commit: String,
    pub inputs_digest: String,
}

/// Constructed only after an established signature verifier approves a policy-bound
/// statement. Callers cannot manufacture this from serialized receipt strings.
#[derive(Debug)]
pub struct VerifiedRelease {
    pub(crate) manifest: ReleaseManifest,
    pub(crate) compiled: CompiledPlan,
    pub(crate) release_digest: String,
    pub(crate) provenance_digest: String,
    pub(crate) trust_policy_digest: String,
}

/// A signed, policy-approved production candidate whose release evidence has
/// not yet passed the promotion gate.
#[derive(Debug)]
pub struct IntegrityVerifiedCandidate {
    pub(crate) manifest: ReleaseManifest,
    pub(crate) compiled: CompiledPlan,
    pub(crate) release_digest: String,
    pub(crate) provenance_digest: String,
    pub(crate) trust_policy_digest: String,
}

impl IntegrityVerifiedCandidate {
    #[must_use]
    pub fn manifest(&self) -> &ReleaseManifest {
        &self.manifest
    }
    #[must_use]
    pub fn compiled(&self) -> &CompiledPlan {
        &self.compiled
    }
    #[must_use]
    pub fn release_digest(&self) -> &str {
        &self.release_digest
    }
    #[must_use]
    pub fn provenance_digest(&self) -> &str {
        &self.provenance_digest
    }
    #[must_use]
    pub fn trust_policy_digest(&self) -> &str {
        &self.trust_policy_digest
    }
    pub(crate) fn into_verified(self) -> VerifiedRelease {
        VerifiedRelease {
            manifest: self.manifest,
            compiled: self.compiled,
            release_digest: self.release_digest,
            provenance_digest: self.provenance_digest,
            trust_policy_digest: self.trust_policy_digest,
        }
    }

    /// Bind this unpromoted candidate to an explicit disposable test scope.
    ///
    /// # Errors
    /// Rejects a scope that differs from the candidate's signed IR target.
    #[cfg(feature = "test-support")]
    pub fn authorize_test_staging(
        self,
        scope: TestStagingScope,
    ) -> Result<TestStagingAuthorization> {
        let domain = &self.compiled.ir.domain;
        if scope.cluster_binding != domain.cluster_binding
            || scope.control_namespace != domain.control_namespace
            || scope.workload_namespace != domain.namespace
        {
            return reject("test staging scope differs from the signed candidate target");
        }
        if !valid_namespace(&scope.control_namespace) || !valid_namespace(&scope.workload_namespace)
        {
            return reject("test staging scope contains an invalid namespace");
        }
        Ok(TestStagingAuthorization {
            candidate: self,
            scope,
        })
    }
}

#[cfg(feature = "test-support")]
fn valid_namespace(value: &str) -> bool {
    value.len() <= 63
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestStagingScope {
    pub cluster_binding: String,
    pub control_namespace: String,
    pub workload_namespace: String,
}

#[cfg(feature = "test-support")]
#[derive(Debug)]
pub struct TestStagingAuthorization {
    candidate: IntegrityVerifiedCandidate,
    scope: TestStagingScope,
}

#[cfg(feature = "test-support")]
impl TestStagingAuthorization {
    #[must_use]
    pub fn candidate(&self) -> &IntegrityVerifiedCandidate {
        &self.candidate
    }
    #[must_use]
    pub fn scope(&self) -> &TestStagingScope {
        &self.scope
    }
}
impl VerifiedRelease {
    #[must_use]
    pub fn manifest(&self) -> &ReleaseManifest {
        &self.manifest
    }
    #[must_use]
    pub fn compiled(&self) -> &CompiledPlan {
        &self.compiled
    }
    #[must_use]
    pub fn release_digest(&self) -> &str {
        &self.release_digest
    }
    #[must_use]
    pub fn provenance_digest(&self) -> &str {
        &self.provenance_digest
    }
    #[must_use]
    pub fn trust_policy_digest(&self) -> &str {
        &self.trust_policy_digest
    }
}

pub(crate) fn raw_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
pub(crate) fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub(crate) fn bounded_read(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return reject("document exceeds the release size limit");
    }
    Ok(bytes)
}
pub(crate) fn json(bytes: &[u8]) -> Result<Value> {
    let input = std::str::from_utf8(bytes)
        .map_err(|_| crate::Error::Rejected("document is not UTF-8".into()))?;
    Ok(parse_strict(input)?)
}
pub(crate) fn contained(root: &Path, name: &str) -> Result<std::path::PathBuf> {
    if name.is_empty()
        || Path::new(name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return reject("release path is not a normal relative file path");
    }
    let path = root.join(name);
    // A promoted release is a regular immutable snapshot; do not follow links.
    let mut cursor = root.to_path_buf();
    for component in Path::new(name).components() {
        cursor.push(component.as_os_str());
        if std::fs::symlink_metadata(&cursor)?.file_type().is_symlink() {
            return reject("release contains a symlink");
        }
    }
    if !path.is_file() {
        return reject("release entry is not a regular file");
    }
    Ok(path)
}

/// Validate immutable payload integrity. This does not confer signature trust.
///
/// # Errors
/// Rejects malformed, missing, altered, or inconsistent release payloads.
pub fn read_release(root: &Path) -> Result<(ReleaseManifest, CompiledPlan, Vec<u8>)> {
    let bytes = bounded_read(&contained(root, "release.json")?, MAX_DOCUMENT)?;
    let value = json(&bytes)?;
    if canonical_bytes(&value)? != bytes {
        return reject("release.json is not canonical JSON");
    }
    let manifest: ReleaseManifest = serde_json::from_value(value)?;
    if manifest.schema_version != "reakt.dev/release/v1" {
        return reject("unsupported release schema");
    }
    if !is_digest(&manifest.ir_digest)
        || !is_digest(&manifest.compiler_digest)
        || !is_digest(&manifest.source.inputs_digest)
        || !is_digest(&manifest.binary_digest)
    {
        return reject("release contains an invalid digest");
    }
    let Some((repository, image_digest)) = manifest.image.reference.rsplit_once('@') else {
        return reject("controller image must use a repository@sha256 digest");
    };
    if repository.is_empty()
        || repository.contains('@')
        || repository
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || b"./:_-".contains(&b)))
        || !is_digest(image_digest)
    {
        return reject("invalid immutable controller image reference");
    }
    if !matches!(
        manifest.image.platform.as_str(),
        "linux/amd64" | "linux/arm64"
    ) {
        return reject("unsupported controller architecture");
    }
    if !manifest
        .source
        .repository
        .starts_with("https://github.com/")
        || manifest.source.repository.len() > 256
        || !matches!(manifest.source.commit.len(), 40 | 64)
        || !manifest
            .source
            .commit
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return reject("release source must identify a GitHub repository and exact commit");
    }
    for name in [
        "ir.json",
        "plan.json",
        "source-map.json",
        "build-inputs.json",
        "sbom.json",
        "compatibility.json",
        "graphs.json",
        "test-plan.json",
        "coverage.json",
        "capabilities.json",
        "ir-source-map.json",
        "reakt-controller",
        "controller.oci.tar",
    ] {
        if !manifest.files.contains_key(name) {
            return reject(format!("release missing required payload {name}"));
        }
    }
    if manifest.files.len() > 128 {
        return reject("release has too many payload files");
    }
    for (name, expected) in &manifest.files {
        if name == "release.json" || !is_digest(expected) {
            return reject("invalid or self-referential release file digest");
        }
        let actual = bounded_read(
            &contained(root, name)?,
            if name == "controller.oci.tar" {
                crate::oci::MAX_OCI_BYTES
            } else if name == "reakt-controller" {
                MAX_BINARY
            } else {
                MAX_DOCUMENT
            },
        )?;
        if raw_digest(&actual) != *expected {
            return reject(format!("release payload digest mismatch: {name}"));
        }
    }
    validate_binary(root, &manifest)?;
    crate::oci::verify(root, &manifest)?;
    let compiled = validate_compilation(root, &manifest)?;
    Ok((manifest, compiled, bytes))
}

fn validate_compilation(root: &Path, manifest: &ReleaseManifest) -> Result<CompiledPlan> {
    let ir = bounded_read(&contained(root, "ir.json")?, MAX_DOCUMENT)?;
    if manifest.mode == ReleaseMode::Production
        && json(&ir)?.get("testOnly") != Some(&Value::Bool(false))
    {
        return reject("production IR must explicitly contain testOnly:false");
    }
    let compiled = parse_and_validate(
        std::str::from_utf8(&ir).map_err(|_| crate::Error::Rejected("IR is not UTF-8".into()))?,
    )?;
    if compiled.ir_digest != manifest.ir_digest {
        return reject("release IR identity mismatch");
    }
    let plan = json(&bounded_read(&contained(root, "plan.json")?, MAX_DOCUMENT)?)?;
    if serde_json::to_value(&compiled)? != plan {
        return reject("release plan differs from trusted compiler output");
    }
    for (name, expected) in compiler_core::review_documents(&compiled) {
        let actual = json(&bounded_read(&contained(root, name)?, MAX_DOCUMENT)?)?;
        if actual != expected {
            return reject(format!(
                "release review document differs from trusted derivation: {name}"
            ));
        }
    }
    let test_plan = json(&bounded_read(
        &contained(root, "test-plan.json")?,
        MAX_DOCUMENT,
    )?)?;
    if test_plan != serde_json::to_value(&compiled.test_plan)? {
        return reject("release reactor test plan differs from trusted derivation");
    }
    let inputs = json(&bounded_read(
        &contained(root, "build-inputs.json")?,
        MAX_DOCUMENT,
    )?)?;
    if digest(&inputs)? != manifest.source.inputs_digest {
        return reject("declared build inputs digest mismatch");
    }
    crate::validate_compiler_closure(&inputs, &manifest.compiler_digest)?;
    Ok(compiled)
}

fn validate_binary(root: &Path, manifest: &ReleaseManifest) -> Result<()> {
    if manifest.files.get("reakt-controller") != Some(&manifest.binary_digest) {
        return reject("native controller identity differs from the release binary digest");
    }
    let binary = bounded_read(&contained(root, "reakt-controller")?, MAX_BINARY)?;
    let machine = match manifest.image.platform.as_str() {
        "linux/arm64" => 183_u16,
        _ => 62_u16,
    };
    if binary.len() < 64
        || !binary.starts_with(b"\x7fELF\x02\x01")
        || binary.get(18..20) != Some(machine.to_le_bytes().as_slice())
    {
        return reject(
            "native controller is not a Linux ELF binary for the declared image architecture",
        );
    }
    Ok(())
}
