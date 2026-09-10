use crate::{Result, reject};
use compiler_core::{digest, parse_strict};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

pub const COMPILER_CLOSURE_SCHEMA: &str = "apsides.dev/compiler-closure/v2";
const MAX_COMPILER_CLOSURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPILER_FILES: usize = 32_768;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompilerClosure {
    pub schema_version: String,
    pub authoring_compiler: AuthoringCompiler,
    pub native_compiler: NativeCompiler,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoringCompiler {
    pub sdk: Value,
    pub toolchain: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeCompiler {
    pub builder_image: BuilderImage,
    pub cargo_lock_sha256: String,
    pub build_context_files: Vec<CompilerFile>,
    pub packaging_files: Vec<CompilerFile>,
    pub build_profile_sha256: String,
    pub host_tools: Vec<HostTool>,
    pub trusted_cli_code_sha256: String,
    pub approved_runtime: ApprovedRuntime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuilderImage {
    pub reference: String,
    pub platform: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompilerFile {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostTool {
    pub role: String,
    pub sha256: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovedRuntime {
    pub base_image: BuilderImage,
    pub index_json: String,
    pub manifest_json: String,
    pub config_json: String,
}

pub(crate) struct ValidatedRuntime {
    pub manifest: Value,
    pub config: Value,
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_image(value: &str) -> bool {
    let Some((repository, digest)) = value.rsplit_once('@') else {
        return false;
    };
    !repository.is_empty()
        && !repository.contains('@')
        && repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"./:_-".contains(&byte))
        && valid_digest(digest)
}

fn valid_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn platform(value: &str) -> Option<(&str, &str)> {
    value
        .split_once('/')
        .filter(|(os, architecture)| *os == "linux" && matches!(*architecture, "amd64" | "arm64"))
}

fn descriptor_matches(descriptor: &Value, raw: &str, media_type: &str) -> bool {
    descriptor["mediaType"] == media_type
        && descriptor["digest"].as_str()
            == Some(crate::manifest::raw_digest(raw.as_bytes()).as_str())
        && descriptor["size"].as_u64() == Some(raw.len() as u64)
}

fn runtime_documents(runtime: &ApprovedRuntime) -> Result<(Value, Value)> {
    const MAX_RUNTIME_DOCUMENT: usize = 1024 * 1024;
    if !valid_image(&runtime.base_image.reference)
        || !matches!(
            runtime.base_image.platform.as_str(),
            "linux/amd64" | "linux/arm64"
        )
        || [
            &runtime.index_json,
            &runtime.manifest_json,
            &runtime.config_json,
        ]
        .iter()
        .any(|document| document.is_empty() || document.len() > MAX_RUNTIME_DOCUMENT)
    {
        return reject("approved runtime identity is incomplete or oversized");
    }
    let base_digest = runtime
        .base_image
        .reference
        .rsplit_once('@')
        .ok_or_else(|| crate::Error::Rejected("runtime image reference lacks a digest".into()))?
        .1;
    if crate::manifest::raw_digest(runtime.index_json.as_bytes()) != base_digest {
        return reject("approved runtime index differs from its immutable image reference");
    }
    let index = parse_strict(&runtime.index_json)?;
    if index["schemaVersion"] != 2
        || index["mediaType"] != "application/vnd.oci.image.index.v1+json"
    {
        return reject("approved runtime must identify an OCI image index");
    }
    let (os, architecture) = platform(&runtime.base_image.platform)
        .ok_or_else(|| crate::Error::Rejected("unsupported runtime platform".into()))?;
    let selected: Vec<&Value> = index["manifests"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("approved runtime index has no manifests".into()))?
        .iter()
        .filter(|descriptor| {
            descriptor["platform"]["os"] == os
                && descriptor["platform"]["architecture"] == architecture
        })
        .collect();
    if !matches!(selected.as_slice(), [descriptor] if descriptor_matches(
        descriptor,
        &runtime.manifest_json,
        "application/vnd.oci.image.manifest.v1+json",
    )) {
        return reject("approved runtime index does not select the recorded platform manifest");
    }
    let manifest = parse_strict(&runtime.manifest_json)?;
    if manifest["schemaVersion"] != 2
        || manifest["mediaType"] != "application/vnd.oci.image.manifest.v1+json"
        || !descriptor_matches(
            &manifest["config"],
            &runtime.config_json,
            "application/vnd.oci.image.config.v1+json",
        )
    {
        return reject("approved runtime manifest does not select the recorded image config");
    }
    Ok((manifest, parse_strict(&runtime.config_json)?))
}

fn validate_runtime(runtime: &ApprovedRuntime) -> Result<ValidatedRuntime> {
    let (manifest, config) = runtime_documents(runtime)?;
    let (os, architecture) = platform(&runtime.base_image.platform)
        .ok_or_else(|| crate::Error::Rejected("unsupported runtime platform".into()))?;
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("approved runtime layers missing".into()))?;
    if layers.len() > 63
        || layers.iter().any(|layer| {
            !matches!(
                layer["mediaType"].as_str(),
                Some(
                    "application/vnd.oci.image.layer.v1.tar+gzip"
                        | "application/vnd.oci.image.layer.v1.tar"
                )
            ) || !layer["digest"].as_str().is_some_and(valid_digest)
                || layer["size"].as_u64().is_none()
        })
    {
        return reject("approved runtime has invalid layer descriptors");
    }
    if !config["config"].is_object() {
        return reject("approved runtime config lacks an OCI runtime configuration");
    }
    let diff_ids = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("approved runtime diff IDs missing".into()))?;
    if config["rootfs"]["type"] != "layers"
        || config["os"] != os
        || config["architecture"] != architecture
        || diff_ids.len() != layers.len()
        || diff_ids
            .iter()
            .any(|value| !value.as_str().is_some_and(valid_digest))
    {
        return reject("approved runtime config is inconsistent with its platform and layers");
    }
    let env = config["config"]["Env"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let mut names = BTreeSet::new();
    if env.len() > 64
        || env.iter().any(|value| {
            let Some(value) = value.as_str() else {
                return true;
            };
            let Some((name, _)) = value.split_once('=') else {
                return true;
            };
            value.len() > 4096
                || name.is_empty()
                || name.starts_with("LD_")
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || !names.insert(name)
        })
    {
        return reject("approved runtime contains an unsafe or ambiguous environment");
    }
    Ok(ValidatedRuntime { manifest, config })
}

pub(crate) fn validated_compiler_closure(
    build_inputs: &Value,
    expected_digest: &str,
) -> Result<(String, ValidatedRuntime)> {
    let object = build_inputs
        .as_object()
        .ok_or_else(|| crate::Error::Rejected("build inputs must be an object".into()))?;
    let closure_value = object
        .get("compilerClosure")
        .ok_or_else(|| crate::Error::Rejected("build inputs lack compiler closure".into()))?;
    let recorded_digest = object
        .get("compilerClosureDigest")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::Error::Rejected("build inputs lack compiler closure digest".into())
        })?;
    let closure: CompilerClosure = serde_json::from_value(closure_value.clone())?;
    if compiler_core::canonical_bytes(&closure)?.len() > MAX_COMPILER_CLOSURE_BYTES {
        return reject("compiler closure exceeds the 8 MiB identity budget");
    }
    if closure.schema_version != COMPILER_CLOSURE_SCHEMA {
        return reject("unsupported compiler closure schema");
    }
    let expected_authoring = json!({
        "sdk": object.get("sdk").ok_or_else(|| crate::Error::Rejected("build inputs lack SDK identity".into()))?,
        "toolchain": object.get("toolchain").ok_or_else(|| crate::Error::Rejected("build inputs lack toolchain identity".into()))?,
    });
    if serde_json::to_value(&closure.authoring_compiler)? != expected_authoring {
        return reject("compiler closure is disconnected from the authoring toolchain receipt");
    }
    let runtime = validate_native_compiler(&closure.native_compiler)?;
    let actual = digest(&closure)?;
    if !valid_digest(recorded_digest) || recorded_digest != actual || expected_digest != actual {
        return reject("release compiler identity does not match the compiler closure");
    }
    Ok((actual, runtime))
}

fn validate_native_compiler(native: &NativeCompiler) -> Result<ValidatedRuntime> {
    if !valid_image(&native.builder_image.reference)
        || !matches!(
            native.builder_image.platform.as_str(),
            "linux/amd64" | "linux/arm64"
        )
        || !valid_digest(&native.cargo_lock_sha256)
        || !valid_digest(&native.build_profile_sha256)
        || native.build_context_files.is_empty()
        || native.build_context_files.len() > MAX_COMPILER_FILES
        || native.packaging_files.is_empty()
        || native.packaging_files.len() > 4096
    {
        return reject("native compiler closure is incomplete or invalid");
    }
    let runtime = validate_runtime(&native.approved_runtime)?;
    validate_files(&native.build_context_files)?;
    validate_files(&native.packaging_files)?;
    for required in [
        ".cargo/config.toml",
        "Dockerfile",
        "controller/Cargo.lock",
        "controller/Cargo.toml",
        "controller/src/ir.json",
        "controller/src/main.rs",
        "workspace/Cargo.toml",
    ] {
        if !native
            .build_context_files
            .iter()
            .any(|file| file.path == required)
        {
            return reject("native compiler context omits a required build input");
        }
    }
    if !native
        .build_context_files
        .iter()
        .any(|file| file.path.starts_with("vendor/"))
    {
        return reject("native compiler context omits vendored dependency sources");
    }
    for required in [
        "scripts/Dockerfile",
        "scripts/build.py",
        "scripts/extract_binary.py",
        "scripts/finalize_inputs.py",
        "scripts/package_oci.py",
        "scripts/sbom.py",
        "scripts/toolchain_snapshot.py",
        "crates/cli/Cargo.toml",
    ] {
        if !native
            .packaging_files
            .iter()
            .any(|file| file.path == required)
        {
            return reject("native compiler closure omits trusted packaging code");
        }
    }
    if native
        .packaging_files
        .iter()
        .any(|file| !file.path.starts_with("scripts/") && !file.path.starts_with("crates/cli/"))
    {
        return reject("native compiler packaging manifest contains an unsupported source");
    }
    let cli_files: Vec<&CompilerFile> = native
        .packaging_files
        .iter()
        .filter(|file| file.path.starts_with("crates/cli/"))
        .collect();
    if cli_files.is_empty() || digest(&cli_files)? != native.trusted_cli_code_sha256 {
        return reject("trusted CLI code identity does not match the native compiler manifest");
    }
    let required_roles = ["buildx", "cargo", "docker", "python", "apsides", "sbom"];
    if native.host_tools.len() != required_roles.len()
        || native
            .host_tools
            .iter()
            .zip(required_roles)
            .any(|(tool, role)| {
                tool.role != role
                    || !valid_digest(&tool.sha256)
                    || tool.version.is_empty()
                    || tool.version.len() > 4096
                    || tool.version.chars().any(char::is_control)
            })
    {
        return reject("native compiler host tool identities are incomplete or ambiguous");
    }
    Ok(runtime)
}

fn validate_files(files: &[CompilerFile]) -> Result<()> {
    let mut previous: Option<&str> = None;
    let mut paths = BTreeSet::new();
    for file in files {
        if !valid_path(&file.path)
            || !valid_digest(&file.sha256)
            || previous.is_some_and(|value| value >= file.path.as_str())
            || !paths.insert(file.path.as_str())
        {
            return reject(
                "native compiler file manifest must be sorted, unique, and digest-bound",
            );
        }
        previous = Some(&file.path);
    }
    Ok(())
}

/// Validate and recompute the compiler closure embedded in `build-inputs.json`.
///
/// # Errors
/// Rejects incomplete, disconnected, malformed, or incorrectly identified closures.
pub fn validate_compiler_closure(build_inputs: &Value, expected_digest: &str) -> Result<String> {
    validated_compiler_closure(build_inputs, expected_digest).map(|(digest, _)| digest)
}
