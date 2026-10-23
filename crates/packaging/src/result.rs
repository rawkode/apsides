//! `build-result.json`: the identities a native build produced.

use crate::{BuildResult, Result, Stage, error::io, fs::write_new};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub const SCHEMA: &str = "apsides.dev/build-result/v1";

/// The `build-result.json` bytes: sorted keys, two-space indentation and a
/// trailing newline.
///
/// # Errors
/// Fails when the document cannot be serialised.
pub fn render(result: &BuildResult) -> Result<Vec<u8>> {
    let fields: BTreeMap<&str, Value> = BTreeMap::from([
        ("schemaVersion", json!(SCHEMA)),
        ("irDigest", json!(result.ir_digest)),
        ("platform", json!(result.platform.as_str())),
        ("imageDigest", json!(result.image_digest)),
        ("binaryDigest", json!(result.binary_digest)),
        ("sbomDigest", json!(result.sbom_digest)),
        ("image", json!("image/controller.oci.tar")),
        ("binary", json!("linux-binary/apsides-controller")),
        ("sbom", json!("sbom/sbom.json")),
        ("buildInputs", json!("build-inputs.json")),
        ("releaseVerified", json!(false)),
        (
            "remainingReleaseGates",
            json!(["approved-provenance", "trusted-release-verification"]),
        ),
    ]);
    let mut bytes = serde_json::to_vec_pretty(&fields).map_err(|error| {
        crate::Error::new(
            Stage::Outputs,
            format!("cannot serialise build-result.json: {error}"),
        )
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes `build-result.json` to `path`, which must not exist.
///
/// # Errors
/// Fails when `path` exists or cannot be written.
pub fn write(path: &Path, result: &BuildResult) -> Result<()> {
    write_new(path, &render(result)?, 0o644).map_err(io(Stage::Outputs, "write", path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Platform;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn result() -> BuildResult {
        BuildResult {
            ir_digest: format!("sha256:{}", "1".repeat(64)),
            platform: Platform::Amd64,
            image_digest: format!("sha256:{}", "2".repeat(64)),
            binary_digest: format!("sha256:{}", "3".repeat(64)),
            sbom_digest: format!("sha256:{}", "4".repeat(64)),
        }
    }

    #[test]
    fn the_document_has_exactly_the_v1_fields() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("build-result.json");
        write(&path, &result())?;
        let text = std::fs::read_to_string(&path)?;
        assert!(text.starts_with("{\n  \"binary\": \"linux-binary/apsides-controller\",\n"));
        assert!(text.ends_with("}\n"));
        let document: Value = serde_json::from_str(&text)?;
        let keys: Vec<&str> = document
            .as_object()
            .ok_or("not an object")?
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "binary",
                "binaryDigest",
                "buildInputs",
                "image",
                "imageDigest",
                "irDigest",
                "platform",
                "releaseVerified",
                "remainingReleaseGates",
                "sbom",
                "sbomDigest",
                "schemaVersion",
            ]
        );
        assert_eq!(document["platform"], "linux/amd64");
        assert_eq!(document["releaseVerified"], false);
        assert!(
            write(&path, &result()).is_err(),
            "an existing result was overwritten"
        );
        Ok(())
    }
}
