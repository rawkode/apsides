//! The runtime base: binding the engine's save of it to the registry index
//! and platform manifest committed under `scripts/runtime-base`.

use crate::{
    Error, Platform, Result, Stage,
    digest::{digest, is_digest},
    fs::read_regular,
    reference::pinned,
    saved::SavedImage,
};
use crate::{builder::inspect_image, command::Run, redact::TAIL_LINES};
use container::Engine;
use release::{ApprovedRuntime, BuilderImage};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// The pins directory under the assets root.
pub const PINS: &str = "scripts/runtime-base";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MAX_PIN_BYTES: u64 = 1024 * 1024;

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::RuntimeBase, detail)
}

/// The exact registry bytes pinned for `value` under the pins directory.
fn pinned_blob(
    assets: &Path,
    reference: &str,
    value: &Value,
    size: Option<&Value>,
) -> Result<Vec<u8>> {
    let pins = assets.join(PINS);
    let blob_digest = value
        .as_str()
        .filter(|value| is_digest(value))
        .ok_or_else(|| fail("runtime base pins require SHA-256 descriptors"))?;
    let path = pins.join("blobs/sha256").join(&blob_digest[7..]);
    if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(fail(format!(
            "the runtime base {reference} is not pinned under {}; run `cuenv task runtimeBasePin`",
            pins.display()
        )));
    }
    let bytes = read_regular(&path, MAX_PIN_BYTES).map_err(|error| {
        fail(format!(
            "cannot read runtime base pin {}: {error}",
            path.display()
        ))
    })?;
    let size_matches = size.is_none_or(|size| size.as_u64() == Some(bytes.len() as u64));
    if digest(&bytes) != blob_digest || !size_matches {
        return Err(fail(format!(
            "runtime base pin {} differs from its digest",
            path.display()
        )));
    }
    Ok(bytes)
}

fn parse(bytes: &[u8], what: &str) -> Result<Value> {
    serde_json::from_slice(bytes)
        .map_err(|error| fail(format!("pinned runtime base {what} is not JSON: {error}")))
}

fn text(bytes: Vec<u8>, what: &str) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| fail(format!("runtime base {what} is not UTF-8")))
}

/// The approved runtime base for `reference` on `platform`: the pinned index
/// and platform manifest, offline, and the saved image's config, which must
/// be the one that manifest names.
///
/// Every saved layer was already verified against the config's diff IDs and
/// is stored uncompressed in the controller image, so the config binding is
/// the whole chain: engine image IDs and compressed digests never matter.
///
/// # Errors
/// Fails when the reference is not pinned, a pin differs from its digest,
/// the index does not select exactly one OCI manifest for the platform, the
/// saved config or layer count differs from the pinned manifest, or release
/// validation rejects the result.
pub fn approved_runtime(
    assets: &Path,
    reference: &str,
    platform: Platform,
    saved: &SavedImage,
) -> Result<ApprovedRuntime> {
    let index_digest = pinned(reference).ok_or_else(|| {
        fail(format!(
            "runtime image {reference} is not a repository@sha256:<64 hex> reference"
        ))
    })?;
    let index_raw = pinned_blob(
        assets,
        reference,
        &Value::String(index_digest.to_owned()),
        None,
    )?;
    let index = parse(&index_raw, "index")?;
    if index["mediaType"] != OCI_INDEX {
        return Err(fail("pinned runtime base must be an OCI image index"));
    }
    let matches: Vec<&Value> = index["manifests"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|item| {
            item["platform"]["os"] == "linux"
                && item["platform"]["architecture"] == platform.architecture()
        })
        .collect();
    let [entry] = matches.as_slice() else {
        return Err(fail(format!(
            "pinned runtime base index must select exactly one {} manifest",
            platform.as_str()
        )));
    };
    let manifest_raw = pinned_blob(assets, reference, &entry["digest"], Some(&entry["size"]))?;
    let manifest = parse(&manifest_raw, "manifest")?;
    if manifest["mediaType"] != OCI_MANIFEST {
        return Err(fail(
            "pinned runtime base platform entry must be an OCI manifest",
        ));
    }
    let saved_digest = digest(&saved.config);
    let pinned_digest = manifest["config"]["digest"].as_str().unwrap_or_default();
    if saved_digest != pinned_digest
        || manifest["config"]["size"].as_u64() != Some(saved.config.len() as u64)
    {
        return Err(fail(format!(
            "the saved runtime base config differs from the pinned manifest (saved {saved_digest}, \
             pinned {pinned_digest}); the local image is not the pinned one"
        )));
    }
    let pinned_layers = manifest["layers"].as_array().map_or(0, Vec::len);
    if pinned_layers != saved.layers.len() {
        return Err(fail(format!(
            "saved runtime base layers differ from the pinned manifest ({} saved, {pinned_layers} \
             pinned)",
            saved.layers.len()
        )));
    }
    let approved = ApprovedRuntime {
        base_image: BuilderImage {
            reference: reference.to_owned(),
            platform: platform.as_str().to_owned(),
        },
        index_json: text(index_raw, "index")?,
        manifest_json: text(manifest_raw, "manifest")?,
        config_json: text(saved.config.clone(), "config")?,
    };
    release::validate_approved_runtime(&approved).map_err(|error| {
        fail(format!(
            "release validation rejected the approved runtime base: {error}"
        ))
    })?;
    Ok(approved)
}

/// The file [`save`] writes in its directory.
pub const SAVE_NAME: &str = "runtime-base.tar";
const SAVE_TIMEOUT: Duration = Duration::from_secs(600);

/// Saves the runtime image `reference` from `engine`'s store into
/// `directory` and returns the archive's path.
///
/// The image must be present, Linux, for `platform` and carry the pinned
/// digest; aps never pulls.
///
/// # Errors
/// Fails as [`crate::builder::inspect`] for the runtime image, when its
/// platform is not `platform`, and when the save fails.
pub fn save(
    engine: Engine,
    program: &Path,
    reference: &str,
    platform: Platform,
    directory: &Path,
) -> Result<PathBuf> {
    let image = inspect_image(engine, program, reference, "runtime", Stage::RuntimeBase)?;
    if image.platform != platform {
        return Err(fail(format!(
            "runtime image {reference} is {} but the build profile declares {}",
            image.platform.as_str(),
            platform.as_str()
        )));
    }
    let output = directory.join(SAVE_NAME);
    Run::new(program, container::save_args(engine, reference, &output))
        .timeout(SAVE_TIMEOUT)
        .capture()
        .map_err(|failure| {
            fail(failure.describe(
                &format!("{} image save of {reference}", program.display()),
                TAIL_LINES,
            ))
        })?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{
        TestResult, archive, base_layer, classic_members, gzip, image_config, second_layer, write,
        write_pins,
    };

    #[test]
    fn the_committed_pins_are_exact_registry_bytes() -> TestResult {
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let pins = assets.join(PINS);
        for entry in fs::read_dir(pins.join("blobs/sha256"))? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF-8 pin")?;
            assert_eq!(digest(&fs::read(entry.path())?), format!("sha256:{name}"));
        }
        let reference = fs::read_to_string(pins.join("reference"))?;
        let reference = reference.trim();
        let index_digest = pinned(reference).ok_or("unpinned reference")?;
        let index = parse(
            &pinned_blob(
                &assets,
                reference,
                &Value::String(index_digest.to_owned()),
                None,
            )?,
            "index",
        )?;
        for platform in [Platform::Amd64, Platform::Arm64] {
            let entries: Vec<&Value> = index["manifests"]
                .as_array()
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter(|item| {
                    item["platform"]["os"] == "linux"
                        && item["platform"]["architecture"] == platform.architecture()
                })
                .collect();
            let [entry] = entries.as_slice() else {
                return Err(format!("{} is not selected exactly once", platform.as_str()).into());
            };
            pinned_blob(&assets, reference, &entry["digest"], Some(&entry["size"]))?;
        }
        Ok(())
    }

    struct Fixture {
        assets: tempfile::TempDir,
        layers: Vec<Vec<u8>>,
        config: Vec<u8>,
        reference: String,
        index: Vec<u8>,
        manifest: Vec<u8>,
    }

    impl Fixture {
        fn new() -> TestResult<Self> {
            let assets = tempfile::tempdir()?;
            let layers = vec![base_layer()?, second_layer()?];
            let config = image_config(&layers, "arm64")?;
            let compressed = layers
                .iter()
                .map(|layer| gzip(layer))
                .collect::<std::io::Result<Vec<_>>>()?;
            let (reference, index, manifest) = write_pins(assets.path(), &config, &compressed)?;
            Ok(Self {
                assets,
                layers,
                config,
                reference,
                index,
                manifest,
            })
        }

        fn saved(&self, config: &[u8], layers: &[Vec<u8>]) -> TestResult<SavedImage> {
            let path = write(
                self.assets.path(),
                "save.tar",
                &archive(classic_members(config, layers)?)?,
            )?;
            Ok(SavedImage::open(&path, Platform::Arm64)?)
        }

        fn rejection(&self, reference: &str, saved: &SavedImage) -> TestResult<String> {
            let error = approved_runtime(self.assets.path(), reference, Platform::Arm64, saved)
                .err()
                .ok_or("the runtime base was approved")?;
            assert_eq!(error.stage, Stage::RuntimeBase);
            Ok(error.detail)
        }
    }

    #[test]
    fn the_pinned_chain_is_verified_offline() -> TestResult {
        let fixture = Fixture::new()?;
        let saved = fixture.saved(&fixture.config, &fixture.layers)?;
        let approved = approved_runtime(
            fixture.assets.path(),
            &fixture.reference,
            Platform::Arm64,
            &saved,
        )?;
        assert_eq!(approved.base_image.reference, fixture.reference);
        assert_eq!(approved.base_image.platform, "linux/arm64");
        assert_eq!(approved.index_json.as_bytes(), fixture.index);
        assert_eq!(approved.manifest_json.as_bytes(), fixture.manifest);
        assert_eq!(approved.config_json.as_bytes(), fixture.config);
        release::validate_approved_runtime(&approved)?;
        Ok(())
    }

    /// The image the producer writes over an approved base is the image the
    /// release verifier accepts for that base.
    #[test]
    fn a_packaged_image_passes_release_verification() -> TestResult {
        let fixture = Fixture::new()?;
        let saved = fixture.saved(&fixture.config, &fixture.layers)?;
        let approved = approved_runtime(
            fixture.assets.path(),
            &fixture.reference,
            Platform::Arm64,
            &saved,
        )?;
        let mut binary = vec![0_u8; 64];
        binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
        binary[18] = 183;
        let ir_digest = format!("sha256:{}", "1".repeat(64));
        let output = fixture.assets.path().join("controller.oci.tar");
        let image_digest = crate::image::package(
            &saved,
            &binary,
            &ir_digest,
            86_400,
            Platform::Arm64,
            &output,
        )?;
        let expected = release::ControllerImage {
            image_digest: &image_digest,
            platform: Platform::Arm64.as_str(),
            ir_digest: &ir_digest,
            binary_digest: &digest(&binary),
            approved_runtime: &approved,
        };
        let archive = fs::read(&output)?;
        release::verify_controller_image(&archive, &expected)?;
        let other_ir = format!("sha256:{}", "2".repeat(64));
        assert!(
            release::verify_controller_image(
                &archive,
                &release::ControllerImage {
                    ir_digest: &other_ir,
                    ..expected
                }
            )
            .is_err(),
            "an image for another IR was accepted"
        );
        Ok(())
    }

    #[test]
    fn unpinned_tampered_or_disconnected_bases_are_rejected() -> TestResult {
        let fixture = Fixture::new()?;
        let saved = fixture.saved(&fixture.config, &fixture.layers)?;
        let unpinned = format!("example.invalid/runtime@sha256:{}", "e".repeat(64));
        let detail = fixture.rejection(&unpinned, &saved)?;
        assert!(detail.contains("is not pinned under"), "{detail}");
        assert!(
            detail.ends_with("run `cuenv task runtimeBasePin`"),
            "{detail}"
        );

        let amd64 = approved_runtime(
            fixture.assets.path(),
            &fixture.reference,
            Platform::Amd64,
            &saved,
        )
        .err()
        .ok_or("amd64 was approved")?;
        assert!(amd64.detail.contains("not pinned"), "{}", amd64.detail);

        let other_config = image_config(&fixture.layers[..1], "arm64")?;
        let detail = fixture.rejection(
            &fixture.reference,
            &fixture.saved(&other_config, &fixture.layers[..1])?,
        )?;
        assert!(
            detail.starts_with(
                "the saved runtime base config differs from the pinned manifest (saved sha256:"
            ),
            "{detail}"
        );

        let index_path = fixture
            .assets
            .path()
            .join(PINS)
            .join("blobs/sha256")
            .join(&digest(&fixture.index)[7..]);
        let mut tampered = fixture.index.clone();
        tampered.push(b' ');
        fs::write(&index_path, tampered)?;
        let detail = fixture.rejection(&fixture.reference, &saved)?;
        assert!(detail.ends_with("differs from its digest"), "{detail}");
        Ok(())
    }

    #[test]
    fn the_layer_count_must_match_the_pinned_manifest() -> TestResult {
        let fixture = Fixture::new()?;
        let config = image_config(&fixture.layers[..1], "arm64")?;
        let compressed = vec![gzip(&fixture.layers[0])?, gzip(&fixture.layers[1])?];
        let (reference, _, _) = write_pins(fixture.assets.path(), &config, &compressed)?;
        let detail =
            fixture.rejection(&reference, &fixture.saved(&config, &fixture.layers[..1])?)?;
        assert!(
            detail.contains("layers differ from the pinned manifest (1 saved, 2 pinned)"),
            "{detail}"
        );
        Ok(())
    }

    #[test]
    fn save_checks_the_image_then_writes_the_archive() -> TestResult {
        let directory = tempfile::tempdir()?;
        let digest_value = format!("sha256:{}", "9".repeat(64));
        let reference = format!("gcr.io/distroless/cc-debian12@{digest_value}");
        let script = format!(
            "case \"$1 $2\" in\n\
             'image inspect') echo '[{{\"Id\":\"sha256:ab\",\"Os\":\"linux\",\"Architecture\":\"amd64\",\"RepoDigests\":[\"{reference}\"]}}]';;\n\
             'image save') [ \"$3 $4 $5\" = '--format docker-archive --output' ] && printf saved > \"$6\";;\n\
             esac"
        );
        let program = crate::tools::fake_tool(directory.path(), "podman", &script)?;
        let saved = save(
            Engine::Podman,
            &program,
            &reference,
            Platform::Amd64,
            directory.path(),
        )?;
        assert_eq!(saved, directory.path().join(SAVE_NAME));
        assert_eq!(fs::read(&saved)?, b"saved");
        let error = save(
            Engine::Podman,
            &program,
            &reference,
            Platform::Arm64,
            directory.path(),
        )
        .err()
        .ok_or("a foreign platform was saved")?;
        assert_eq!(
            error.detail,
            format!(
                "runtime image {reference} is linux/amd64 but the build profile declares \
                 linux/arm64"
            )
        );
        Ok(())
    }
}
