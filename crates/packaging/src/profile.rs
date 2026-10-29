//! The build profile: which builder and runtime images, platform, SBOM tool
//! and source date epoch a build uses.

use crate::{
    Error, Platform, Result, Stage, digest::is_digest, error::io, fs::read_regular,
    reference::pinned,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

pub const SCHEMA: &str = "apsides.dev/build-profile/v1";
/// The largest source date epoch a USTAR header's 11 octal digits hold.
pub const MAX_SOURCE_DATE_EPOCH: u64 = 0o77_777_777_777;
const MAX_PROFILE_BYTES: u64 = 64 * 1024;
const KEYS: [&str; 7] = [
    "builderImage",
    "platform",
    "runtimeImage",
    "sbomTool",
    "sbomToolSha256",
    "schemaVersion",
    "sourceDateEpoch",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub builder_image: String,
    pub runtime_image: String,
    pub platform: Platform,
    /// Always absolute: a relative `sbomTool` is resolved against the
    /// profile's own directory, never looked up on `PATH`.
    pub sbom_tool: PathBuf,
    pub sbom_tool_sha256: String,
    pub source_date_epoch: u64,
}

/// The profile at `path` and its exact bytes, which the compiler closure
/// hashes.
///
/// # Errors
/// Fails when the file is unreadable, oversized or not strict JSON, when its
/// keys differ from the seven profile fields, and when a field is invalid.
pub fn read(path: &Path) -> Result<(Profile, Vec<u8>)> {
    let bytes = read_regular(path, MAX_PROFILE_BYTES).map_err(io(Stage::Profile, "read", path))?;
    let problem = |detail: String| {
        Error::new(
            Stage::Profile,
            format!("build profile {}: {detail}", path.display()),
        )
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| problem("is not UTF-8".into()))?;
    let document = compiler_core::parse_bounded(
        text,
        usize::try_from(MAX_PROFILE_BYTES).unwrap_or(usize::MAX),
        "build profile",
    )
    .map_err(|error| problem(error.to_string()))?;
    let object = document
        .as_object()
        .ok_or_else(|| problem("is not a JSON object".into()))?;
    let present: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let expected = BTreeSet::from(KEYS);
    let unknown: Vec<&str> = present.difference(&expected).copied().collect();
    let missing: Vec<&str> = expected.difference(&present).copied().collect();
    if !unknown.is_empty() || !missing.is_empty() {
        let mut parts = Vec::new();
        if !unknown.is_empty() {
            parts.push(format!("unknown fields {}", unknown.join(", ")));
        }
        if !missing.is_empty() {
            parts.push(format!("missing fields {}", missing.join(", ")));
        }
        return Err(problem(parts.join("; ")));
    }
    let field = |name: &str, problem_text: &str| problem(format!("field {name} {problem_text}"));
    let string = |name: &str| {
        object[name]
            .as_str()
            .ok_or_else(|| field(name, "must be a string"))
    };
    if string("schemaVersion")? != SCHEMA {
        return Err(field("schemaVersion", &format!("must be {SCHEMA}")));
    }
    let platform = Platform::parse(string("platform")?)
        .ok_or_else(|| field("platform", "must be linux/amd64 or linux/arm64"))?;
    let source_date_epoch = match &object["sourceDateEpoch"] {
        Value::Number(number) => number
            .as_u64()
            .filter(|epoch| *epoch <= MAX_SOURCE_DATE_EPOCH),
        _ => None,
    }
    .ok_or_else(|| {
        field(
            "sourceDateEpoch",
            &format!("must be an integer from 0 to {MAX_SOURCE_DATE_EPOCH}"),
        )
    })?;
    let image = |name: &str| {
        let reference = string(name)?;
        pinned(reference)
            .map(|_| reference.to_owned())
            .ok_or_else(|| field(name, "must be a repository@sha256:<64 hex> reference"))
    };
    let builder_image = image("builderImage")?;
    let runtime_image = image("runtimeImage")?;
    let sbom_tool_sha256 = string("sbomToolSha256")?;
    if !is_digest(sbom_tool_sha256) {
        return Err(field("sbomToolSha256", "must be sha256:<64 lowercase hex>"));
    }
    let tool = string("sbomTool")?;
    if tool.is_empty() {
        return Err(field("sbomTool", "must not be empty"));
    }
    let sbom_tool = if Path::new(tool).is_absolute() {
        PathBuf::from(tool)
    } else {
        let directory = path
            .canonicalize()
            .map_err(io(Stage::Profile, "resolve", path))?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| problem("has no parent directory".into()))?;
        directory.join(tool)
    };
    Ok((
        Profile {
            builder_image,
            runtime_image,
            platform,
            sbom_tool,
            sbom_tool_sha256: sbom_tool_sha256.to_owned(),
            source_date_epoch,
        },
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    const BUILDER: &str =
        "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922";
    const RUNTIME: &str = "gcr.io/distroless/cc-debian12@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f";
    const TOOL_SHA: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn valid() -> Value {
        json!({
            "schemaVersion": SCHEMA,
            "builderImage": BUILDER,
            "runtimeImage": RUNTIME,
            "platform": "linux/amd64",
            "sbomTool": "tools/cargo-cyclonedx",
            "sbomToolSha256": TOOL_SHA,
            "sourceDateEpoch": 0,
        })
    }

    fn write(directory: &Path, text: &str) -> std::io::Result<PathBuf> {
        let path = directory.join("profile.json");
        std::fs::write(&path, text)?;
        Ok(path)
    }

    fn rejection(
        directory: &Path,
        text: &str,
    ) -> std::result::Result<String, Box<dyn std::error::Error>> {
        let path = write(directory, text)?;
        let error = read(&path).err().ok_or("the profile was accepted")?;
        assert_eq!(error.stage, Stage::Profile);
        Ok(error.detail)
    }

    #[test]
    fn a_valid_profile_resolves_a_relative_tool_against_its_directory() -> TestResult {
        let directory = tempfile::tempdir()?;
        let text = valid().to_string();
        let path = write(directory.path(), &text)?;
        let (profile, bytes) = read(&path)?;
        assert_eq!(bytes, text.as_bytes());
        assert_eq!(
            profile,
            Profile {
                builder_image: BUILDER.into(),
                runtime_image: RUNTIME.into(),
                platform: Platform::Amd64,
                sbom_tool: directory
                    .path()
                    .canonicalize()?
                    .join("tools/cargo-cyclonedx"),
                sbom_tool_sha256: TOOL_SHA.into(),
                source_date_epoch: 0,
            }
        );
        let mut absolute = valid();
        absolute["sbomTool"] = json!("/opt/bin/cargo-cyclonedx");
        absolute["sourceDateEpoch"] = json!(MAX_SOURCE_DATE_EPOCH);
        let (profile, _) = read(&write(directory.path(), &absolute.to_string())?)?;
        assert_eq!(profile.sbom_tool, Path::new("/opt/bin/cargo-cyclonedx"));
        assert_eq!(profile.source_date_epoch, 8_589_934_591);
        Ok(())
    }

    #[test]
    fn the_key_set_must_be_exact() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut extra = valid();
        extra["engine"] = json!("podman");
        let detail = rejection(directory.path(), &extra.to_string())?;
        assert!(detail.ends_with("unknown fields engine"), "{detail}");
        let mut missing = valid();
        missing
            .as_object_mut()
            .ok_or("not an object")?
            .remove("platform");
        let detail = rejection(directory.path(), &missing.to_string())?;
        assert!(detail.ends_with("missing fields platform"), "{detail}");
        let duplicate = valid()
            .to_string()
            .replacen('{', r#"{"platform":"linux/arm64","#, 1);
        rejection(directory.path(), &duplicate)?;
        Ok(())
    }

    #[test]
    fn fields_are_validated() -> TestResult {
        let directory = tempfile::tempdir()?;
        let cases = [
            (
                "schemaVersion",
                json!("apsides.dev/build-profile/v2"),
                "field schemaVersion",
            ),
            ("platform", json!("linux/riscv64"), "field platform"),
            ("sourceDateEpoch", json!(true), "field sourceDateEpoch"),
            ("sourceDateEpoch", json!(-1), "field sourceDateEpoch"),
            (
                "sourceDateEpoch",
                json!(MAX_SOURCE_DATE_EPOCH + 1),
                "field sourceDateEpoch",
            ),
            ("sourceDateEpoch", json!("0"), "field sourceDateEpoch"),
            ("builderImage", json!("rust:latest"), "field builderImage"),
            (
                "runtimeImage",
                json!("gcr.io/distroless/cc-debian12"),
                "field runtimeImage",
            ),
            (
                "sbomToolSha256",
                json!("sha256:abc"),
                "field sbomToolSha256",
            ),
            ("sbomTool", json!(""), "field sbomTool"),
        ];
        for (name, value, expected) in cases {
            let mut profile = valid();
            profile[name] = value;
            let detail = rejection(directory.path(), &profile.to_string())?;
            assert!(detail.contains(expected), "{name}: {detail}");
        }
        let fractional = valid()
            .to_string()
            .replace(r#""sourceDateEpoch":0"#, r#""sourceDateEpoch":0.5"#);
        assert!(fractional.contains("0.5"));
        rejection(directory.path(), &fractional)?;
        Ok(())
    }

    #[test]
    fn a_missing_profile_names_its_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("absent.json");
        let error = read(&path).err().ok_or("accepted")?;
        assert!(
            error
                .to_string()
                .starts_with(&format!("build profile: cannot read {}", path.display()))
        );
        Ok(())
    }
}
