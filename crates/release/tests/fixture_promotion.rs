use release::{ReleaseMode, TrustPolicy, validate_fixture_projection, verify_candidate};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env,
    fmt::{Display, Formatter},
    fs::{self, File},
    io::{self, Read as _, Write as _},
    path::{Component, Path, PathBuf},
};

const PRODUCTION_ARTIFACT: &str = "APS_FIXTURE_PRODUCTION_ARTIFACT";
const FIXTURE_ARTIFACT: &str = "APS_FIXTURE_ARTIFACT";
const TRUST_POLICY: &str = "APS_FIXTURE_TRUST_POLICY";
const RESULT_PREFIX: &str = "APS_FIXTURE_PROMOTION_RESULT=";
const RESULT_SCHEMA: &str = "reakt.dev/fixture-promotion-result/v1";
const MAX_DOCUMENT: u64 = 8 * 1024 * 1024;
const MAX_POLICY: u64 = 1024 * 1024;

#[derive(Debug)]
struct DriverError(&'static str);

impl Display for DriverError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for DriverError {}

type Result<T> = std::result::Result<T, DriverError>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PromotionResult {
    schema_version: &'static str,
    passed: bool,
    production_release_digest: String,
    production_ir_digest: String,
    production_binary_digest: String,
    production_image_digest: String,
    fixture_release_digest: String,
    fixture_ir_digest: String,
    fixture_binary_digest: String,
    fixture_image_digest: String,
}

struct FixtureSnapshot {
    release_bytes: Vec<u8>,
    ir: Value,
    fixture_ir: Value,
    overlay: Value,
    test_plan: Value,
    release_digest: String,
    ir_digest: String,
    binary_digest: String,
    image_digest: String,
}

#[tokio::test]
#[ignore = "requires an exact signed production candidate and a compiler-built fixture release"]
async fn fixture_promotion_step() -> Result<()> {
    let production_root = artifact_path(PRODUCTION_ARTIFACT)?;
    let fixture_root = artifact_path(FIXTURE_ARTIFACT)?;
    if production_root == fixture_root {
        return Err(DriverError(
            "production and fixture artifacts must be distinct",
        ));
    }
    let policy = trust_policy(&file_path(TRUST_POLICY)?)?;

    let production = verify_candidate(&production_root, &policy)
        .await
        .map_err(|_| DriverError("production candidate verification failed"))?;
    let production_ir = read_bound_json(&production_root, production.manifest(), "ir.json")?;
    if compiler_core::digest(&production_ir)
        .map_err(|_| DriverError("production IR digest failed"))?
        != production.compiled().ir_digest
    {
        return Err(DriverError(
            "production candidate IR differs from its packaged identity",
        ));
    }
    let production_test_plan =
        read_bound_json(&production_root, production.manifest(), "test-plan.json")?;
    let fixture = fixture_snapshot(&fixture_root)?;
    verify_projection(&production_ir, &production_test_plan, &fixture)?;

    require_fixture_rejection(&fixture_root, &policy).await?;

    let production_after = verify_candidate(&production_root, &policy)
        .await
        .map_err(|_| DriverError("production candidate changed after fixture rejection"))?;
    if production_after.release_digest() != production.release_digest()
        || production_after.compiled().ir_digest != production.compiled().ir_digest
        || production_after.manifest().binary_digest != production.manifest().binary_digest
        || production_after.manifest().image.reference != production.manifest().image.reference
    {
        return Err(DriverError(
            "production candidate identity changed after fixture rejection",
        ));
    }
    let fixture_after = fixture_snapshot(&fixture_root)?;
    if fixture_after.release_bytes != fixture.release_bytes
        || fixture_after.ir != fixture.ir
        || fixture_after.fixture_ir != fixture.fixture_ir
        || fixture_after.overlay != fixture.overlay
        || fixture_after.test_plan != fixture.test_plan
    {
        return Err(DriverError(
            "fixture candidate identity changed after fixture rejection",
        ));
    }
    verify_projection(&production_ir, &production_test_plan, &fixture_after)?;
    require_fixture_rejection(&fixture_root, &policy).await?;

    emit(&PromotionResult {
        schema_version: RESULT_SCHEMA,
        passed: true,
        production_release_digest: production.release_digest().to_owned(),
        production_ir_digest: production.compiled().ir_digest.clone(),
        production_binary_digest: production.manifest().binary_digest.clone(),
        production_image_digest: image_digest(&production.manifest().image.reference)?.to_owned(),
        fixture_release_digest: fixture.release_digest,
        fixture_ir_digest: fixture.ir_digest,
        fixture_binary_digest: fixture.binary_digest,
        fixture_image_digest: fixture.image_digest,
    })
}

async fn require_fixture_rejection(root: &Path, policy: &TrustPolicy) -> Result<()> {
    match verify_candidate(root, policy).await {
        Err(release::Error::Rejected(message))
            if message == "fixture-substituted artifacts cannot be promoted to production" =>
        {
            Ok(())
        }
        Err(_) => Err(DriverError(
            "fixture candidate returned an unexpected rejection",
        )),
        Ok(_) => Err(DriverError("fixture candidate was accepted for production")),
    }
}

fn fixture_snapshot(root: &Path) -> Result<FixtureSnapshot> {
    let (manifest, compiled, release_bytes) = release::read_release(root)
        .map_err(|_| DriverError("fixture release integrity validation failed"))?;
    if manifest.mode != ReleaseMode::Fixture || !compiled.ir.test_only {
        return Err(DriverError("fixture release lacks its test-only mode"));
    }
    for name in ["fixture-ir.json", "fixture-overlay.json"] {
        if !manifest.files.contains_key(name) {
            return Err(DriverError("fixture release lacks its compiler projection"));
        }
    }
    let ir = read_bound_json(root, &manifest, "ir.json")?;
    if compiler_core::digest(&ir).map_err(|_| DriverError("fixture IR digest failed"))?
        != manifest.ir_digest
    {
        return Err(DriverError("fixture IR differs from its packaged identity"));
    }
    let fixture_ir = read_bound_json(root, &manifest, "fixture-ir.json")?;
    if ir != fixture_ir {
        return Err(DriverError(
            "fixture release IR differs from its packaged compiler fixture",
        ));
    }
    let overlay = read_bound_json(root, &manifest, "fixture-overlay.json")?;
    let test_plan = read_bound_json(root, &manifest, "test-plan.json")?;
    Ok(FixtureSnapshot {
        release_digest: raw_digest(&release_bytes),
        ir_digest: manifest.ir_digest,
        binary_digest: manifest.binary_digest,
        image_digest: image_digest(&manifest.image.reference)?.to_owned(),
        release_bytes,
        ir,
        fixture_ir,
        overlay,
        test_plan,
    })
}

fn verify_projection(
    production_ir: &Value,
    production_test_plan: &Value,
    fixture: &FixtureSnapshot,
) -> Result<()> {
    if production_test_plan != &fixture.test_plan {
        return Err(DriverError(
            "production and fixture reactor contracts differ",
        ));
    }
    validate_fixture_projection(production_ir, &fixture.ir, &fixture.overlay)
        .map_err(|_| DriverError("fixture is not the exact compiler projection"))
}

fn artifact_path(name: &'static str) -> Result<PathBuf> {
    let path = environment_path(name)?;
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| DriverError("release artifact path is invalid"))?;
    if !metadata.file_type().is_dir() {
        return Err(DriverError("release artifact path is invalid"));
    }
    path.canonicalize()
        .map_err(|_| DriverError("release artifact path is invalid"))
}

fn file_path(name: &'static str) -> Result<PathBuf> {
    let path = environment_path(name)?;
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| DriverError("trust policy path is invalid"))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > MAX_POLICY {
        return Err(DriverError("trust policy path is invalid"));
    }
    path.canonicalize()
        .map_err(|_| DriverError("trust policy path is invalid"))
}

fn environment_path(name: &'static str) -> Result<PathBuf> {
    let value = env::var_os(name).ok_or(DriverError("required path environment is absent"))?;
    let paths = env::split_paths(&value).collect::<Vec<_>>();
    let [path] = paths.as_slice() else {
        return Err(DriverError("required path environment is invalid"));
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(DriverError("required path environment is invalid"));
    }
    Ok(path.clone())
}

fn trust_policy(path: &Path) -> Result<TrustPolicy> {
    let value = read_json(path, MAX_POLICY)?;
    serde_json::from_value(value).map_err(|_| DriverError("trust policy is invalid"))
}

fn read_bound_json(root: &Path, manifest: &release::ReleaseManifest, name: &str) -> Result<Value> {
    let bytes = read_bytes(&root.join(name), MAX_DOCUMENT)?;
    if manifest.files.get(name) != Some(&raw_digest(&bytes)) {
        return Err(DriverError(
            "release JSON differs from its verified payload",
        ));
    }
    parse_json(&bytes)
}

fn read_json(path: &Path, maximum: u64) -> Result<Value> {
    parse_json(&read_bytes(path, maximum)?)
}

fn read_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|_| DriverError("JSON input is invalid"))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > maximum {
        return Err(DriverError("JSON input is invalid"));
    }
    let limit = maximum
        .checked_add(1)
        .ok_or(DriverError("JSON input is invalid"))?;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| DriverError("JSON input is invalid"))?
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|_| DriverError("JSON input is invalid"))?;
    if bytes.is_empty()
        || u64::try_from(bytes.len()).map_err(|_| DriverError("JSON input is invalid"))? > maximum
    {
        return Err(DriverError("JSON input is invalid"));
    }
    Ok(bytes)
}

fn parse_json(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).map_err(|_| DriverError("JSON input is invalid"))?;
    compiler_core::parse_strict(text).map_err(|_| DriverError("JSON input is invalid"))
}

fn image_digest(reference: &str) -> Result<&str> {
    let digest = reference
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .ok_or(DriverError("release image identity is invalid"))?;
    if !valid_digest(digest) {
        return Err(DriverError("release image identity is invalid"));
    }
    Ok(digest)
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn raw_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn emit(result: &PromotionResult) -> Result<()> {
    let encoded = serde_json::to_string(result)
        .map_err(|_| DriverError("fixture promotion result serialization failed"))?;
    if encoded.len() > 16 * 1024
        || encoded
            .chars()
            .any(|character| matches!(character, '\n' | '\r'))
    {
        return Err(DriverError("fixture promotion result exceeds its bound"));
    }
    let mut output = io::stdout().lock();
    writeln!(output, "{RESULT_PREFIX}{encoded}")
        .map_err(|_| DriverError("fixture promotion result write failed"))
}
