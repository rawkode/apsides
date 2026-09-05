//! Restricted compiler-image verification using OCI, tar and gzip OSS parsers.
//! No archive entry is extracted to the host filesystem.
use crate::{
    Result,
    manifest::{MAX_DOCUMENT, ReleaseManifest, bounded_read, contained, json, raw_digest},
    reject,
};
use flate2::read::GzDecoder;
use oci_spec::image::{ImageConfiguration, ImageIndex, ImageManifest};
use serde_json::Value;
use std::{collections::BTreeMap, io::Read, path::Path};

pub(crate) const MAX_OCI_BYTES: u64 = 512 * 1024 * 1024;
const TARGET: &str = "usr/local/bin/reakt-controller";
const ANCESTORS: &[&str] = &["usr", "usr/local", "usr/local/bin"];

fn approved_runtime(
    root: &Path,
    release: &ReleaseManifest,
) -> Result<crate::compiler::ValidatedRuntime> {
    let build_inputs = json(&bounded_read(
        &contained(root, "build-inputs.json")?,
        MAX_DOCUMENT,
    )?)?;
    let (_, runtime) =
        crate::compiler::validated_compiler_closure(&build_inputs, &release.compiler_digest)?;
    Ok(runtime)
}

pub(crate) fn verify(root: &Path, release: &ReleaseManifest) -> Result<()> {
    let runtime = approved_runtime(root, release)?;
    let bytes = bounded_read(&contained(root, "controller.oci.tar")?, MAX_OCI_BYTES)?;
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut files = BTreeMap::new();
    let mut total = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return reject("OCI archive contains a non-regular entry");
        }
        let name = entry.path()?.to_string_lossy().into_owned();
        if !(name == "index.json"
            || name == "oci-layout"
            || name
                .strip_prefix("blobs/sha256/")
                .is_some_and(|hex| crate::manifest::is_digest(&format!("sha256:{hex}"))))
        {
            return reject("OCI archive entry is outside the content-addressed layout");
        }
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| crate::Error::Rejected("OCI size overflow".into()))?;
        if total > MAX_OCI_BYTES || files.len() >= 4096 {
            return reject("OCI archive exceeds its entry or byte budget");
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if files.insert(name, data).is_some() {
            return reject("duplicate OCI archive path");
        }
    }
    if json(
        files
            .get("oci-layout")
            .ok_or_else(|| crate::Error::Rejected("missing OCI layout".into()))?,
    )?["imageLayoutVersion"]
        != "1.0.0"
    {
        return reject("unsupported OCI layout version");
    }
    let image_digest = release
        .image
        .reference
        .rsplit_once('@')
        .ok_or_else(|| crate::Error::Rejected("image reference lacks a digest".into()))?
        .1;
    let raw = blob(&files, image_digest, None)?;
    let index = json(
        files
            .get("index.json")
            .ok_or_else(|| crate::Error::Rejected("missing OCI index".into()))?,
    )?;
    let _: ImageIndex = serde_json::from_value(index.clone())?;
    let entries = index["manifests"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("OCI index has no manifest".into()))?;
    if !matches!(entries.as_slice(), [entry]
        if entry["digest"] == image_digest
        && entry["size"].as_u64() == Some(raw.len() as u64))
    {
        return reject("OCI archive index does not select the released manifest");
    }

    let value = json(raw)?;
    let _: ImageManifest = serde_json::from_value(value.clone())?;
    if value["schemaVersion"] != 2
        || value["mediaType"] != "application/vnd.oci.image.manifest.v1+json"
    {
        return reject("release image must name a single-platform OCI manifest");
    }
    let config_bytes = descriptor_blob(&files, &value["config"])?;
    let config = json(config_bytes)?;
    let _: ImageConfiguration = serde_json::from_value(config.clone())?;
    if format!(
        "{}/{}",
        config["os"].as_str().unwrap_or(""),
        config["architecture"].as_str().unwrap_or("")
    ) != release.image.platform
        || config["rootfs"]["type"] != "layers"
        || config["config"] != expected_config(&runtime.config, &release.ir_digest)?
    {
        return reject(
            "OCI image runtime command, user or platform differs from the native controller contract",
        );
    }
    verify_layers(
        &files,
        &value,
        &config,
        &runtime.manifest,
        &runtime.config,
        &release.binary_digest,
    )
}

fn expected_config(base: &Value, ir_digest: &str) -> Result<Value> {
    let mut config = base["config"]
        .as_object()
        .ok_or_else(|| crate::Error::Rejected("approved base runtime config missing".into()))?
        .clone();
    config.insert("User".into(), Value::String("65532:65532".into()));
    config.insert("WorkingDir".into(), Value::String("/".into()));
    config.insert(
        "Entrypoint".into(),
        serde_json::json!(["/usr/local/bin/reakt-controller"]),
    );
    config.remove("Cmd");
    let mut labels = base["config"]["Labels"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (key, value) in [
        ("dev.reakt.ir-digest", ir_digest),
        ("dev.reakt.release-status", "blocked"),
        (
            "org.opencontainers.image.title",
            "Reakt generated controller",
        ),
    ] {
        labels.insert(key.into(), Value::String(value.into()));
    }
    config.insert("Labels".into(), Value::Object(labels));
    Ok(Value::Object(config))
}

fn verify_layers(
    files: &BTreeMap<String, Vec<u8>>,
    value: &Value,
    config: &Value,
    base_manifest: &Value,
    base_config: &Value,
    binary_digest: &str,
) -> Result<()> {
    let layers = value["layers"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("OCI layers missing".into()))?;
    let diff_ids = config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("OCI diff IDs missing".into()))?;
    let base_layers = base_manifest["layers"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("approved base layers missing".into()))?;
    let base_diff_ids = base_config["rootfs"]["diff_ids"]
        .as_array()
        .ok_or_else(|| crate::Error::Rejected("approved base diff IDs missing".into()))?;
    if layers.len() != base_layers.len() + 1
        || diff_ids.len() != base_diff_ids.len() + 1
        || layers.get(..base_layers.len()) != Some(base_layers.as_slice())
        || diff_ids.get(..base_diff_ids.len()) != Some(base_diff_ids.as_slice())
        || layers.len() > 64
    {
        return reject("compiler image does not exactly extend the approved runtime layer chain");
    }
    let mut expanded_total = 0;
    for (index, (layer, diff_id)) in layers.iter().zip(diff_ids).enumerate() {
        let compressed = descriptor_blob(files, layer)?;
        let mut data = Vec::new();
        let reader: Box<dyn Read> = match layer["mediaType"].as_str() {
            Some("application/vnd.oci.image.layer.v1.tar+gzip") => {
                Box::new(GzDecoder::new(compressed))
            }
            Some("application/vnd.oci.image.layer.v1.tar") => Box::new(compressed),
            _ => return reject("unsupported compiler-image compression"),
        };
        reader
            .take(MAX_OCI_BYTES - expanded_total + 1)
            .read_to_end(&mut data)?;
        expanded_total += data.len() as u64;
        if expanded_total > MAX_OCI_BYTES {
            return reject("OCI expanded layers exceed budget");
        }
        if diff_id.as_str() != Some(raw_digest(&data).as_str()) {
            return reject("OCI uncompressed layer digest mismatch");
        }
        verify_layer(&data, index + 1 == layers.len(), binary_digest)?;
    }
    Ok(())
}

fn blob<'a>(
    files: &'a BTreeMap<String, Vec<u8>>,
    digest: &str,
    size: Option<u64>,
) -> Result<&'a [u8]> {
    if !crate::manifest::is_digest(digest) {
        return reject("OCI descriptor must use SHA-256");
    }
    let data = files
        .get(&format!("blobs/sha256/{}", &digest[7..]))
        .ok_or_else(|| crate::Error::Rejected("missing OCI blob".into()))?;
    if raw_digest(data) != digest || size.is_some_and(|size| size != data.len() as u64) {
        return reject("OCI descriptor digest/size mismatch");
    }
    Ok(data)
}
fn descriptor_blob<'a>(
    files: &'a BTreeMap<String, Vec<u8>>,
    descriptor: &Value,
) -> Result<&'a [u8]> {
    let digest = descriptor["digest"]
        .as_str()
        .ok_or_else(|| crate::Error::Rejected("missing OCI descriptor digest".into()))?;
    let size = descriptor["size"]
        .as_u64()
        .ok_or_else(|| crate::Error::Rejected("missing OCI descriptor size".into()))?;
    blob(files, digest, Some(size))
}
fn verify_layer(bytes: &[u8], last: bool, binary_digest: &str) -> Result<()> {
    let mut archive = tar::Archive::new(bytes);
    let mut found = false;
    let mut directories = std::collections::BTreeSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw_path = entry.path()?;
        let mut parts = Vec::new();
        for component in raw_path.components() {
            match component {
                std::path::Component::Normal(part) => parts.push(
                    part.to_str()
                        .ok_or_else(|| crate::Error::Rejected("non-UTF8 OCI path".into()))?,
                ),
                std::path::Component::CurDir => {}
                _ => return reject("OCI layer path escapes its root"),
            }
        }
        let path = parts.join("/");
        if path == "etc/ld.so.preload" {
            return reject("compiler image runtime may not install a dynamic-loader preload file");
        }
        if ANCESTORS.contains(&path.as_str()) {
            directories.insert(path.clone());
        }
        if ANCESTORS.contains(&path.as_str()) && !entry.header().entry_type().is_dir() {
            return reject("OCI binary ancestor is not a directory");
        }
        if last {
            if path == TARGET {
                if found
                    || !entry.header().entry_type().is_file()
                    || entry.header().mode()? & 0o001 == 0
                    || entry.header().mode()? & 0o6000 != 0
                {
                    return reject(
                        "OCI controller is duplicated, non-regular, privileged or not executable by its runtime user",
                    );
                }
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                if raw_digest(&data) != binary_digest {
                    return reject("OCI controller bytes differ from the signed native binary");
                }
                found = true;
            } else if !ANCESTORS.contains(&path.as_str()) {
                return reject("final compiler image layer contains unexpected entries");
            }
        }
    }
    if last && (!found || directories.len() != ANCESTORS.len()) {
        return reject("final compiler image layer does not contain the controller");
    }
    Ok(())
}
