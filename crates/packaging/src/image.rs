//! Assembling the controller image: the verified runtime base layers,
//! uncompressed, plus one layer holding the controller, as a single-manifest
//! OCI layout tar.
//!
//! Its bytes depend only on the base, the binary, the IR digest and the
//! source date epoch: every entry, header, mode, owner and timestamp is fixed
//! here, so the image digest does not depend on the engine or image store.

use crate::{
    Error, Platform, Result, Stage,
    digest::{HashingReader, digest},
    error::io,
    saved::SavedImage,
    ustar,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self as stdio, BufWriter, Read},
    path::Path,
};

/// Where the controller lands in the image.
pub const TARGET: &str = "usr/local/bin/apsides-controller";
/// The controller's parent directories, each a directory entry of its layer.
pub const ANCESTORS: [&str; 3] = ["usr", "usr/local", "usr/local/bin"];
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
const OCI_LAYOUT: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::Image, detail)
}

/// `epoch` as `%Y-%m-%dT%H:%M:%SZ` in UTC.
#[must_use]
pub fn timestamp(epoch: u64) -> String {
    // Howard Hinnant's civil-from-days, over days since 1970-01-01, in
    // unsigned arithmetic: the epoch is never negative.
    let (days, seconds) = (epoch / 86_400, epoch % 86_400);
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

/// The final layer: the controller and its parent directories, nothing else,
/// every entry owned by root, mode 0755 and dated `epoch`.
///
/// # Errors
/// Fails when `epoch` or the binary's size does not fit a USTAR header.
pub fn controller_layer(binary: &[u8], epoch: u64) -> Result<Vec<u8>> {
    let build = || -> stdio::Result<Vec<u8>> {
        let mut layer = ustar::Writer::new(Vec::new());
        for ancestor in ANCESTORS {
            layer.directory(ancestor, 0o755, epoch, "root")?;
        }
        layer.file(
            TARGET,
            0o755,
            epoch,
            "root",
            binary.len() as u64,
            &mut &binary[..],
        )?;
        layer.finish()
    };
    build().map_err(|error| fail(format!("cannot write the controller layer: {error}")))
}

/// Whether any number in `value` is not an integer.
fn has_fraction(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.is_f64(),
        Value::Array(items) => items.iter().any(has_fraction),
        Value::Object(fields) => fields.values().any(has_fraction),
        _ => false,
    }
}

/// The controller image config: the runtime base config with the controller
/// layer appended, a history entry for it and the runtime settings release
/// verification requires.
///
/// # Errors
/// Fails when the base config is not ASCII JSON free of fractional numbers
/// (which would serialise differently from the bytes release verification
/// recomputes), or lacks a runtime configuration or layers.
pub fn image_config(
    base_config: &[u8],
    layer_diff_id: &str,
    ir_digest: &str,
    epoch: u64,
) -> Result<Vec<u8>> {
    if !base_config.is_ascii() {
        return Err(fail("runtime base config is not ASCII JSON"));
    }
    let mut config: Value = serde_json::from_slice(base_config)
        .map_err(|error| fail(format!("runtime base config is not JSON: {error}")))?;
    if has_fraction(&config) {
        return Err(fail("runtime base config contains a fractional number"));
    }
    let lacking = || fail("runtime base config lacks a runtime configuration or layers");
    if !config["config"].is_object() {
        return Err(lacking());
    }
    let mut diff_ids = config["rootfs"]["diff_ids"]
        .as_array()
        .cloned()
        .ok_or_else(lacking)?;
    diff_ids.push(Value::String(layer_diff_id.to_owned()));
    let mut history = match config.get("history") {
        None => Vec::new(),
        Some(Value::Array(history)) => history.clone(),
        Some(_) => return Err(fail("runtime base config history is not an array")),
    };
    let runtime = release::controller_runtime_config(&config, ir_digest)
        .map_err(|error| fail(error.to_string()))?;
    let created = timestamp(epoch);
    history.push(json!({
        "comment": "apsides.dev/oci-packaging",
        "created": created,
        "created_by": format!(
            "COPY controller /{TARGET}; USER 65532:65532; WORKDIR /; ENTRYPOINT [\"/{TARGET}\"]"
        ),
    }));
    let object = config.as_object_mut().ok_or_else(lacking)?;
    object.insert("config".into(), runtime);
    object.insert("created".into(), Value::String(created));
    object.insert("history".into(), Value::Array(history));
    object.insert(
        "rootfs".into(),
        json!({"diff_ids": diff_ids, "type": "layers"}),
    );
    canonical(&config)
}

fn canonical(value: &Value) -> Result<Vec<u8>> {
    compiler_core::canonical_bytes(value).map_err(|error| fail(error.to_string()))
}

/// One layer for [`write_layout`]: its diff ID, uncompressed size and a way
/// to open its uncompressed bytes.
pub struct LayerSource<'a> {
    pub digest: String,
    pub size: u64,
    pub open: Box<dyn FnMut() -> Result<Box<dyn Read + 'a>> + 'a>,
}

/// Writes a single-manifest OCI layout tar to `output`, which must not exist,
/// and returns the manifest digest.
///
/// Every layer is stored uncompressed, so its blob digest is its diff ID.
/// Each layer is re-hashed as it is written; on any error the partial file is
/// removed.
///
/// # Errors
/// Fails when `output` exists or cannot be written, and when a layer's bytes
/// differ from its digest or size.
pub fn write_layout(
    output: &Path,
    platform: Platform,
    config: &[u8],
    layers: Vec<LayerSource<'_>>,
) -> Result<String> {
    let manifest = canonical(&json!({
        "config": {"digest": digest(config), "mediaType": OCI_CONFIG, "size": config.len()},
        "layers": layers
            .iter()
            .map(|layer| json!({"digest": layer.digest, "mediaType": OCI_LAYER, "size": layer.size}))
            .collect::<Vec<_>>(),
        "mediaType": OCI_MANIFEST,
        "schemaVersion": 2,
    }))?;
    let image_digest = digest(&manifest);
    let index = canonical(&json!({
        "manifests": [{
            "digest": image_digest,
            "mediaType": OCI_MANIFEST,
            "platform": {"architecture": platform.architecture(), "os": "linux"},
            "size": manifest.len(),
        }],
        "mediaType": OCI_INDEX,
        "schemaVersion": 2,
    }))?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(io(Stage::Image, "create", output))?;
    let written = write_entries(file, output, &index, &manifest, config, layers);
    if written.is_err() {
        // The partial layout is useless and must not look like an output.
        let _ = fs::remove_file(output);
    }
    written.map(|()| image_digest)
}

fn write_entries(
    file: File,
    output: &Path,
    index: &[u8],
    manifest: &[u8],
    config: &[u8],
    layers: Vec<LayerSource<'_>>,
) -> Result<()> {
    let mut tar = ustar::Writer::new(BufWriter::new(file));
    let blob = |bytes: &[u8]| format!("blobs/sha256/{}", &digest(bytes)[7..]);
    let entries = [
        ("oci-layout".to_owned(), OCI_LAYOUT),
        ("index.json".to_owned(), index),
        (blob(manifest), manifest),
        (blob(config), config),
    ];
    let mut done = BTreeSet::from([digest(manifest), digest(config)]);
    for (name, bytes) in entries {
        tar.file(&name, 0o644, 0, "", bytes.len() as u64, &mut &bytes[..])
            .map_err(io(Stage::Image, "write", output))?;
    }
    for (position, mut layer) in layers.into_iter().enumerate() {
        if !done.insert(layer.digest.clone()) {
            continue;
        }
        let changed = || {
            fail(format!(
                "runtime base layer {position} changed while it was written"
            ))
        };
        let mut reader = HashingReader::new((layer.open)()?);
        let name = format!(
            "blobs/sha256/{}",
            layer.digest.trim_start_matches("sha256:")
        );
        match tar.file(&name, 0o644, 0, "", layer.size, &mut reader) {
            Ok(()) => {}
            Err(error) if error.kind() == stdio::ErrorKind::UnexpectedEof => return Err(changed()),
            Err(error) => {
                return Err(fail(format!(
                    "cannot write layer {position} into {}: {error}",
                    output.display()
                )));
            }
        }
        if reader.finish() != (layer.digest.clone(), layer.size) {
            return Err(changed());
        }
    }
    let file = tar
        .finish()
        .and_then(|buffer| {
            buffer
                .into_inner()
                .map_err(stdio::IntoInnerError::into_error)
        })
        .map_err(io(Stage::Image, "write", output))?;
    file.sync_all().map_err(io(Stage::Image, "sync", output))
}

/// Writes the controller image for `binary` on the saved runtime base to
/// `output` and returns its manifest digest.
///
/// # Errors
/// As [`controller_layer`], [`image_config`] and [`write_layout`], and when
/// the saved base cannot be reread.
pub fn package(
    saved: &SavedImage,
    binary: &[u8],
    ir_digest: &str,
    epoch: u64,
    platform: Platform,
    output: &Path,
) -> Result<String> {
    let layer = controller_layer(binary, epoch)?;
    let layer_digest = digest(&layer);
    let config = image_config(&saved.config, &layer_digest, ir_digest, epoch)?;
    let mut sources: Vec<LayerSource<'_>> = saved
        .layers
        .iter()
        .enumerate()
        .map(|(position, base)| LayerSource {
            digest: base.diff_id.clone(),
            size: base.diff_size,
            open: Box::new(move || saved.layer(position)),
        })
        .collect();
    let bytes = layer.as_slice();
    sources.push(LayerSource {
        digest: layer_digest,
        size: layer.len() as u64,
        open: Box::new(move || Ok(Box::new(bytes) as Box<dyn Read>)),
    });
    write_layout(output, platform, &config, sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{
        TestResult, archive, base_layer, classic_members, containerd_members, gzip,
        image_config as base_image_config, write,
    };
    use std::io::Cursor;

    const IR: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const LAYER_ABC_0: &str = "d7400f4971f8b7107ed399f220d7165dd52bfc74e1034e5db6aec57d3f819be0";
    const GOLDEN_BASE: &str = r#"{"architecture":"amd64","config":{"Cmd":["x"],"Env":["PATH=/usr/bin"],"Labels":{"a":"b"}},"os":"linux","rootfs":{"type":"layers","diff_ids":[]},"history":[]}"#;
    const GOLDEN_CONFIG: &str = r#"{"architecture":"amd64","config":{"Entrypoint":["/usr/local/bin/apsides-controller"],"Env":["PATH=/usr/bin"],"Labels":{"a":"b","dev.apsides.ir-digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","dev.apsides.release-status":"blocked","org.opencontainers.image.title":"Apsides generated controller"},"User":"65532:65532","WorkingDir":"/"},"created":"1970-01-01T00:00:00Z","history":[{"comment":"apsides.dev/oci-packaging","created":"1970-01-01T00:00:00Z","created_by":"COPY controller /usr/local/bin/apsides-controller; USER 65532:65532; WORKDIR /; ENTRYPOINT [\"/usr/local/bin/apsides-controller\"]"}],"os":"linux","rootfs":{"diff_ids":["sha256:d7400f4971f8b7107ed399f220d7165dd52bfc74e1034e5db6aec57d3f819be0"],"type":"layers"}}"#;
    const ELF: [u8; 64] = {
        let mut binary = [0_u8; 64];
        binary[0] = 0x7f;
        binary[1] = b'E';
        binary[2] = b'L';
        binary[3] = b'F';
        binary[4] = 2;
        binary[5] = 1;
        binary[18] = 183;
        binary
    };

    fn in_memory(bytes: &[u8]) -> LayerSource<'_> {
        LayerSource {
            digest: digest(bytes),
            size: bytes.len() as u64,
            open: Box::new(move || Ok(Box::new(bytes) as Box<dyn Read>)),
        }
    }

    #[test]
    fn the_controller_layer_matches_the_python_golden_vectors() -> TestResult {
        let layer = controller_layer(b"abc", 0)?;
        assert_eq!(layer.len(), 10240);
        assert_eq!(digest(&layer), format!("sha256:{LAYER_ABC_0}"));
        assert_eq!(&layer[..4], b"usr/");
        assert_eq!(&layer[100..108], b"0000755\0");
        assert_eq!(&layer[108..124], b"0000000\x000000000\0");
        assert_eq!(&layer[124..136], b"00000000000\0");
        assert_eq!(&layer[136..148], b"00000000000\0");
        assert_eq!(&layer[148..156], b"010366\0 ");
        assert_eq!(layer[156], b'5');
        assert_eq!(&layer[257..265], b"ustar\x0000");
        assert_eq!(&layer[265..270], b"root\0");
        assert_eq!(&layer[329..345], [0_u8; 16]);
        let fourth = &layer[1536..2048];
        assert_eq!(&fourth[..33], b"usr/local/bin/apsides-controller\0");
        assert_eq!(&fourth[124..136], b"00000000003\0");
        assert_eq!(&fourth[148..156], b"015760\0 ");
        assert_eq!(fourth[156], b'0');
        assert_eq!(
            digest(&controller_layer(b"abc", 86_400)?),
            "sha256:06866c12d4fbbaa16a6a869d724e4b105ec8db98443f4e772d5a4f607625a558"
        );
        Ok(())
    }

    #[test]
    fn the_controller_layer_holds_only_the_binary_and_its_ancestors() -> TestResult {
        let layer = controller_layer(&ELF, 1_700_000_000)?;
        assert_eq!(layer, controller_layer(&ELF, 1_700_000_000)?);
        let mut entries = Vec::new();
        for entry in tar::Archive::new(layer.as_slice()).entries()? {
            let mut entry = entry?;
            let header = entry.header().clone();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            entries.push((
                entry.path()?.to_string_lossy().into_owned(),
                header.entry_type().is_dir(),
                header.mtime()?,
                header.mode()?,
                header.uid()?,
                header.username()?.map(str::to_owned),
                bytes,
            ));
        }
        let directory = |name: &str| {
            (
                name.to_owned(),
                true,
                1_700_000_000,
                0o755,
                0,
                Some("root".to_owned()),
                Vec::new(),
            )
        };
        assert_eq!(
            entries,
            [
                directory("usr/"),
                directory("usr/local/"),
                directory("usr/local/bin/"),
                (
                    TARGET.to_owned(),
                    false,
                    1_700_000_000,
                    0o755,
                    0,
                    Some("root".to_owned()),
                    ELF.to_vec()
                ),
            ]
        );
        assert!(controller_layer(b"", 1 << 33).is_err());
        Ok(())
    }

    #[test]
    fn the_image_config_matches_the_python_golden_vector() -> TestResult {
        let config = image_config(
            GOLDEN_BASE.as_bytes(),
            &format!("sha256:{LAYER_ABC_0}"),
            IR,
            0,
        )?;
        assert_eq!(String::from_utf8(config.clone())?, GOLDEN_CONFIG);
        assert_eq!(
            digest(&config),
            "sha256:2d4045400d0a89a3b3ce00659a3fbba00ce9f8af6653df70866eddbee6b67d98"
        );
        Ok(())
    }

    #[test]
    fn the_image_config_extends_the_base_as_release_requires() -> TestResult {
        let base = br#"{"architecture":"arm64","os":"linux","variant":"v8","created":"2020-01-01T00:00:00Z",
            "config":{"Env":["PATH=/bin"],"User":"65532","WorkingDir":"/home/nonroot","Cmd":["sh"],"Labels":{"base":"kept"}},
            "history":[{"created_by":"base"}],
            "rootfs":{"type":"layers","diff_ids":["sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}}"#;
        let config: Value = serde_json::from_slice(&image_config(
            base,
            &format!("sha256:{LAYER_ABC_0}"),
            IR,
            86_400,
        )?)?;
        assert_eq!(
            config["config"],
            json!({
                "Env": ["PATH=/bin"], "User": "65532:65532", "WorkingDir": "/",
                "Entrypoint": ["/usr/local/bin/apsides-controller"],
                "Labels": {"base": "kept", "dev.apsides.ir-digest": IR, "dev.apsides.release-status": "blocked",
                           "org.opencontainers.image.title": "Apsides generated controller"},
            })
        );
        assert_eq!(
            config["rootfs"]["diff_ids"].as_array().map(Vec::len),
            Some(2)
        );
        assert_eq!(config["created"], "1970-01-02T00:00:00Z");
        assert_eq!(config["history"][0], json!({"created_by": "base"}));
        assert_eq!(config["history"][1]["created"], "1970-01-02T00:00:00Z");
        assert_eq!(
            (&config["os"], &config["architecture"], &config["variant"]),
            (&json!("linux"), &json!("arm64"), &json!("v8"))
        );
        Ok(())
    }

    #[test]
    fn the_image_config_rejects_bases_it_cannot_reproduce() {
        let layer = format!("sha256:{LAYER_ABC_0}");
        for (base, expected) in [
            (
                r#"{"config":{"Labels":{"a":"é"}},"rootfs":{"diff_ids":[]}}"#,
                "not ASCII",
            ),
            (
                r#"{"config":{"Healthcheck":{"Interval":1.5}},"rootfs":{"diff_ids":[]}}"#,
                "fractional",
            ),
            (
                r#"{"rootfs":{"diff_ids":[]}}"#,
                "lacks a runtime configuration",
            ),
            (r#"{"config":{}}"#, "lacks a runtime configuration"),
            (
                r#"{"config":{},"rootfs":{"diff_ids":[]},"history":{}}"#,
                "history",
            ),
        ] {
            let error = image_config(base.as_bytes(), &layer, IR, 0).err();
            assert!(
                error
                    .as_ref()
                    .is_some_and(|error| error.detail.contains(expected)),
                "{base}: {error:?}"
            );
        }
    }

    #[test]
    fn the_layout_matches_the_python_golden_vector() -> TestResult {
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("controller.oci.tar");
        let layer = controller_layer(b"abc", 0)?;
        let image_digest = write_layout(
            &output,
            Platform::Amd64,
            GOLDEN_CONFIG.as_bytes(),
            vec![in_memory(&layer)],
        )?;
        assert_eq!(
            image_digest,
            "sha256:50ac6a75bb6de4d9e5f14592c1754ba366cd4cedea4d6ba4dfdae7dc9437e922"
        );
        let bytes = fs::read(&output)?;
        assert_eq!(bytes.len(), 20480);
        assert_eq!(
            digest(&bytes),
            "sha256:311395c897d3db05d0b07cadaa901aedc2596a55276f09bfeac4021008ff201e"
        );
        assert_eq!(&bytes[..11], b"oci-layout\0");
        assert_eq!(&bytes[100..108], b"0000644\0");
        assert_eq!(&bytes[148..156], b"007754\0 ");
        assert_eq!(&bytes[265..297], [0_u8; 32]);
        assert_eq!(&bytes[512..542], OCI_LAYOUT);
        Ok(())
    }

    #[test]
    fn the_layout_is_create_exclusive_and_removed_when_a_layer_changes() -> TestResult {
        let directory = tempfile::tempdir()?;
        let base = base_layer()?;
        let config = base_image_config(std::slice::from_ref(&base), "arm64")?;
        let output = directory.path().join("changed.oci.tar");
        let mut altered = base.clone();
        if let Some(last) = altered.last_mut() {
            *last = b'x';
        }
        let changed = LayerSource {
            digest: digest(&base),
            size: base.len() as u64,
            open: Box::new(move || Ok(Box::new(Cursor::new(altered.clone())) as Box<dyn Read>)),
        };
        let error = write_layout(&output, Platform::Arm64, &config, vec![changed])
            .err()
            .ok_or("accepted")?;
        assert_eq!(
            error.detail,
            "runtime base layer 0 changed while it was written"
        );
        assert!(!output.exists());
        let short = base[..base.len() - 1].to_vec();
        let truncated = LayerSource {
            digest: digest(&base),
            size: base.len() as u64,
            open: Box::new(move || Ok(Box::new(Cursor::new(short.clone())) as Box<dyn Read>)),
        };
        let error = write_layout(&output, Platform::Arm64, &config, vec![truncated])
            .err()
            .ok_or("accepted")?;
        assert!(error.detail.contains("changed while it was written"));
        assert!(!output.exists());
        fs::write(&output, b"existing")?;
        let error = write_layout(&output, Platform::Arm64, &config, vec![in_memory(&base)])
            .err()
            .ok_or("overwrote")?;
        assert!(error.detail.starts_with("cannot create"));
        assert_eq!(fs::read(&output)?, b"existing");
        Ok(())
    }

    #[test]
    fn identical_inputs_give_identical_layouts_from_any_store() -> TestResult {
        let directory = tempfile::tempdir()?;
        let layers = vec![base_layer()?];
        let config = base_image_config(&layers, "arm64")?;
        let compressed = vec![gzip(&layers[0])?];
        let classic = SavedImage::open(
            &write(
                directory.path(),
                "classic.tar",
                &archive(classic_members(&config, &layers)?)?,
            )?,
            Platform::Arm64,
        )?;
        let containerd = SavedImage::open(
            &write(
                directory.path(),
                "containerd.tar",
                &archive(containerd_members(&config, &compressed)?)?,
            )?,
            Platform::Arm64,
        )?;
        let build = |saved: &SavedImage,
                     binary: &[u8],
                     epoch: u64,
                     name: &str|
         -> TestResult<(String, Vec<u8>)> {
            let output = directory.path().join(name);
            let image_digest = package(saved, binary, IR, epoch, Platform::Arm64, &output)?;
            Ok((image_digest, fs::read(&output)?))
        };
        let first = build(&classic, &ELF, 0, "first.tar")?;
        assert_eq!(first, build(&classic, &ELF, 0, "again.tar")?);
        assert_eq!(first, build(&containerd, &ELF, 0, "from-containerd.tar")?);
        let mut longer = ELF.to_vec();
        longer.push(0);
        let changed = build(&classic, &longer, 0, "changed.tar")?;
        let later = build(&classic, &ELF, 1, "later.tar")?;
        assert_eq!(BTreeSet::from([&first.0, &changed.0, &later.0]).len(), 3);

        let mut names = Vec::new();
        let mut manifest = Value::Null;
        for entry in tar::Archive::new(first.1.as_slice()).entries()? {
            let mut entry = entry?;
            let name = entry.path()?.to_string_lossy().into_owned();
            if name == format!("blobs/sha256/{}", &first.0[7..]) {
                manifest = serde_json::from_reader(&mut entry)?;
            }
            names.push(name);
        }
        assert_eq!(&names[..2], ["oci-layout", "index.json"]);
        assert_eq!(names.len(), 6);
        let layer_types: BTreeSet<&str> = manifest["layers"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(|layer| layer["mediaType"].as_str())
            .collect();
        assert_eq!(layer_types, BTreeSet::from([OCI_LAYER]));
        assert_eq!(manifest["layers"][0]["digest"], digest(&layers[0]));
        Ok(())
    }

    #[test]
    fn timestamps_are_utc_civil_dates() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(86_400), "1970-01-02T00:00:00Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(timestamp(1_709_164_799), "2024-02-28T23:59:59Z");
        assert_eq!(timestamp(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(timestamp(8_589_934_591), "2242-03-16T12:56:31Z");
        assert_eq!(timestamp(253_402_300_799), "9999-12-31T23:59:59Z");
    }
}
