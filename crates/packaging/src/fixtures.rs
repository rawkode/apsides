//! Engine saves, layers and pins built in code for the unit tests.

use crate::{digest::digest, ustar};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
pub const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
pub const DOCKER_LAYER_GZIP: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";

/// What one member of a test archive is.
pub enum Item {
    File(Vec<u8>),
    Directory,
    Symlink(String),
    Fifo,
    /// A PAX global header with these records.
    GlobalPax(Vec<u8>),
    /// A regular file preceded by a PAX header whose `path` record is the
    /// member name; the USTAR header itself carries a placeholder name.
    PaxNamed(Vec<u8>),
}

/// A tar of `(name, item)` members, written by the `tar` crate as an engine
/// would.
pub fn archive(members: Vec<(String, Item)>) -> io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, item) in members {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_mtime(0);
        match item {
            Item::File(bytes) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(bytes.len() as u64);
                builder.append_data(&mut header, &name, bytes.as_slice())?;
            }
            Item::Directory => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                builder.append_data(&mut header, &name, io::empty())?;
            }
            Item::Symlink(target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                builder.append_link(&mut header, &name, target)?;
            }
            Item::Fifo => {
                header.set_entry_type(tar::EntryType::Fifo);
                header.set_size(0);
                builder.append_data(&mut header, &name, io::empty())?;
            }
            Item::GlobalPax(records) => {
                header.set_entry_type(tar::EntryType::XGlobalHeader);
                header.set_size(records.len() as u64);
                builder.append_data(&mut header, &name, records.as_slice())?;
            }
            Item::PaxNamed(bytes) => {
                builder.append_pax_extensions([("path", name.as_bytes())])?;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(bytes.len() as u64);
                builder.append_data(&mut header, "placeholder", bytes.as_slice())?;
            }
        }
    }
    builder.into_inner()
}

pub fn file(name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> (String, Item) {
    (name.into(), Item::File(bytes.into()))
}

pub fn blob_name(bytes: &[u8]) -> String {
    format!("blobs/sha256/{}", &digest(bytes)[7..])
}

/// An image layer: a tar of directories (`None`) and files.
pub fn layer(entries: &[(&str, Option<&[u8]>)]) -> io::Result<Vec<u8>> {
    let mut writer = ustar::Writer::new(Vec::new());
    for (name, bytes) in entries {
        match bytes {
            None => writer.directory(name, 0o755, 0, "")?,
            Some(bytes) => {
                writer.file(name, 0o755, 0, "", bytes.len() as u64, &mut &bytes[..])?;
            }
        }
    }
    writer.finish()
}

pub fn base_layer() -> io::Result<Vec<u8>> {
    layer(&[("etc", None), ("etc/os-release", Some(b"ID=test\n"))])
}

pub fn second_layer() -> io::Result<Vec<u8>> {
    layer(&[
        ("usr", None),
        ("usr/lib", None),
        ("usr/lib/libtest.so", Some(b"\x7fELF")),
    ])
}

pub fn gzip(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    encoder.finish()
}

pub fn canonical(value: &Value) -> TestResult<Vec<u8>> {
    Ok(compiler_core::canonical_bytes(value)?)
}

/// An image config naming `layers` as its diff IDs.
pub fn image_config(layers: &[Vec<u8>], architecture: &str) -> TestResult<Vec<u8>> {
    canonical(&json!({
        "architecture": architecture,
        "os": "linux",
        "config": {"Env": ["PATH=/usr/bin"]},
        "rootfs": {"type": "layers", "diff_ids": layers.iter().map(|layer| digest(layer)).collect::<Vec<_>>()},
    }))
}

pub fn descriptor(media_type: &str, bytes: &[u8]) -> Value {
    json!({"mediaType": media_type, "digest": digest(bytes), "size": bytes.len()})
}

pub fn manifest(
    media_type: &str,
    config_type: &str,
    config: &[u8],
    layer_type: &str,
    blobs: &[Vec<u8>],
) -> TestResult<Vec<u8>> {
    canonical(&json!({
        "schemaVersion": 2,
        "mediaType": media_type,
        "config": descriptor(config_type, config),
        "layers": blobs.iter().map(|blob| descriptor(layer_type, blob)).collect::<Vec<_>>(),
    }))
}

pub fn index(manifests: &[Value]) -> TestResult<Vec<u8>> {
    canonical(&json!({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests}))
}

pub fn with_platform(mut descriptor: Value, architecture: &str) -> Value {
    descriptor["platform"] = json!({"os": "linux", "architecture": architecture});
    descriptor
}

/// Writes `bytes` to `<directory>/<name>` and returns the path.
pub fn write(directory: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let path = directory.join(name);
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Docker 25+ with the classic store: an OCI layout of uncompressed layers
/// plus `manifest.json`.
pub fn classic_members(config: &[u8], layers: &[Vec<u8>]) -> TestResult<Vec<(String, Item)>> {
    let manifest = manifest(OCI_MANIFEST, OCI_CONFIG, config, OCI_LAYER, layers)?;
    let index = index(&[descriptor(OCI_MANIFEST, &manifest)])?;
    let legacy = serde_json::to_vec(&json!([{
        "Config": blob_name(config),
        "RepoTags": null,
        "Layers": layers.iter().map(|layer| blob_name(layer)).collect::<Vec<_>>(),
    }]))?;
    let mut members = vec![
        file("oci-layout", br#"{"imageLayoutVersion": "1.0.0"}"#.to_vec()),
        file("index.json", index),
        file("manifest.json", legacy),
        file(blob_name(&manifest), manifest),
        file(blob_name(config), config.to_vec()),
    ];
    for layer in layers {
        members.push(file(blob_name(layer), layer.clone()));
    }
    Ok(members)
}

/// The containerd store: `index.json` names the pulled multi-platform index;
/// only the arm64 manifest and its gzip layers are present.
pub fn containerd_members(
    config: &[u8],
    compressed: &[Vec<u8>],
) -> TestResult<Vec<(String, Item)>> {
    let manifest = manifest(
        DOCKER_MANIFEST,
        DOCKER_CONFIG,
        config,
        DOCKER_LAYER_GZIP,
        compressed,
    )?;
    let absent = |byte: char, architecture: &str| {
        json!({
            "mediaType": OCI_MANIFEST,
            "digest": format!("sha256:{}", byte.to_string().repeat(64)),
            "size": 1,
            "platform": {"os": "linux", "architecture": architecture},
        })
    };
    let pulled = index(&[
        with_platform(descriptor(OCI_MANIFEST, &manifest), "arm64"),
        absent('0', "amd64"),
        absent('1', "s390x"),
    ])?;
    let top = index(&[descriptor(OCI_INDEX, &pulled)])?;
    let mut members = vec![
        file("oci-layout", br#"{"imageLayoutVersion": "1.0.0"}"#.to_vec()),
        file("index.json", top),
        file(blob_name(&pulled), pulled),
        file(blob_name(&manifest), manifest),
        file(blob_name(config), config.to_vec()),
    ];
    for layer in compressed {
        members.push(file(blob_name(layer), layer.clone()));
    }
    Ok(members)
}

/// Docker before 25: `manifest.json` only, a `<id>.json` config and
/// `<id>/layer.tar` layers, a shared layer stored once behind a symlink.
pub fn legacy_members(config: &[u8], layers: &[Vec<u8>; 2]) -> TestResult<Vec<(String, Item)>> {
    let config_name = format!("{}.json", &digest(config)[7..]);
    let manifest = serde_json::to_vec(&json!([{
        "Config": config_name,
        "RepoTags": null,
        "Layers": ["base/layer.tar", "./top/layer.tar"],
    }]))?;
    Ok(vec![
        file("manifest.json", manifest),
        file(config_name, config.to_vec()),
        ("shared/".into(), Item::Directory),
        file("shared/layer.tar", layers[0].clone()),
        (
            "base/layer.tar".into(),
            Item::Symlink("../shared/layer.tar".into()),
        ),
        file("top/layer.tar", layers[1].clone()),
        file("repositories", b"{}".to_vec()),
    ])
}

/// `podman image save --format docker-archive`: top-level `<diff id>.tar`
/// layers and `<id>.json` config, with `<diff id>/layer.tar` symlinks.
pub fn podman_docker_members(config: &[u8], layers: &[Vec<u8>]) -> TestResult<Vec<(String, Item)>> {
    let config_name = format!("{}.json", &digest(config)[7..]);
    let names: Vec<String> = layers
        .iter()
        .map(|layer| format!("{}.tar", &digest(layer)[7..]))
        .collect();
    let manifest = serde_json::to_vec(&json!([{
        "Config": config_name,
        "RepoTags": [],
        "Layers": names,
    }]))?;
    let mut members = vec![
        file("manifest.json", manifest),
        file(config_name, config.to_vec()),
    ];
    for (layer, name) in layers.iter().zip(&names) {
        let hex = &digest(layer)[7..];
        members.push(file(name.clone(), layer.clone()));
        members.push((format!("{hex}/"), Item::Directory));
        members.push((
            format!("{hex}/layer.tar"),
            Item::Symlink(format!("../{name}")),
        ));
    }
    Ok(members)
}

/// `podman image save --format oci-archive`: directory entries for
/// `blobs/`, gzip layers recompressed by podman, PAX headers.
pub fn podman_oci_members(
    config: &[u8],
    compressed: &[Vec<u8>],
) -> TestResult<Vec<(String, Item)>> {
    let manifest = manifest(OCI_MANIFEST, OCI_CONFIG, config, OCI_LAYER_GZIP, compressed)?;
    let mut entry = descriptor(OCI_MANIFEST, &manifest);
    entry["annotations"] = json!({"org.opencontainers.image.ref.name": "latest"});
    let index = index(&[entry])?;
    let mut members = vec![
        (
            "pax_global_header".into(),
            Item::GlobalPax(b"20 comment=podman\n".to_vec()),
        ),
        ("blobs/".into(), Item::Directory),
        ("blobs/sha256/".into(), Item::Directory),
        (blob_name(&manifest), Item::PaxNamed(manifest.clone())),
        file(blob_name(config), config.to_vec()),
    ];
    for layer in compressed {
        members.push(file(blob_name(layer), layer.clone()));
    }
    members.push(file("index.json", index));
    members.push(file(
        "oci-layout",
        br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
    ));
    Ok(members)
}

/// Registry pins for an image under `<directory>/scripts/runtime-base`: an
/// OCI index and its arm64 manifest of `compressed` layers. Returns the
/// reference, index and manifest.
pub fn write_pins(
    assets: &Path,
    config: &[u8],
    compressed: &[Vec<u8>],
) -> TestResult<(String, Vec<u8>, Vec<u8>)> {
    let manifest = manifest(OCI_MANIFEST, OCI_CONFIG, config, OCI_LAYER_GZIP, compressed)?;
    let index = index(&[
        with_platform(descriptor(OCI_MANIFEST, &manifest), "arm64"),
        json!({
            "mediaType": OCI_MANIFEST,
            "digest": format!("sha256:{}", "0".repeat(64)),
            "size": 1,
            "platform": {"os": "linux", "architecture": "amd64"},
        }),
    ])?;
    let blobs = assets.join("scripts/runtime-base/blobs/sha256");
    std::fs::create_dir_all(&blobs)?;
    for bytes in [&index, &manifest] {
        std::fs::write(blobs.join(&digest(bytes)[7..]), bytes)?;
    }
    Ok((
        format!("example.invalid/runtime@{}", digest(&index)),
        index,
        manifest,
    ))
}

/// The approved arm64 runtime base whose layers are `layers`, stored
/// uncompressed.
pub fn approved_runtime(layers: &[Vec<u8>]) -> TestResult<release::ApprovedRuntime> {
    let config = image_config(layers, "arm64")?;
    let manifest = manifest(OCI_MANIFEST, OCI_CONFIG, &config, OCI_LAYER, layers)?;
    let index = index(&[with_platform(descriptor(OCI_MANIFEST, &manifest), "arm64")])?;
    Ok(release::ApprovedRuntime {
        base_image: release::BuilderImage {
            reference: format!("example.invalid/runtime@{}", digest(&index)),
            platform: "linux/arm64".into(),
        },
        index_json: String::from_utf8(index)?,
        manifest_json: String::from_utf8(manifest)?,
        config_json: String::from_utf8(config)?,
    })
}
