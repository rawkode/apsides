//! Reading the runtime base image an engine saved.
//!
//! Engines save an image in several shapes: podman's docker-archive and
//! oci-archive, Docker's legacy docker-archive (before Docker 25) and the OCI
//! layout Docker 25 and later write, filled differently by its classic and
//! containerd stores. Every blob read is checked against its digest and every
//! layer, decompressed, against the config's diff IDs. Nothing outside the
//! archive is followed and nothing is extracted.

use crate::{
    Error, Platform, Result, Stage,
    digest::{HashingReader, digest, is_digest, is_hex64},
    error::io,
};
use flate2::read::MultiGzDecoder;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self as stdio, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// The most layers a base may have: the controller layer makes 64, release
/// verification's limit.
pub const MAX_LAYERS: usize = 63;
const MAX_METADATA: u64 = 1024 * 1024;
/// How many documents an `index.json` chain may lead through before it must
/// reach a single-platform manifest.
const MAX_INDEX_HOPS: usize = 3;
const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
const MANIFEST_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// How a saved layer is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
}

/// The stored form of each layer media type a save may use.
fn layer_compression(media_type: &str) -> Option<Compression> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar" => Some(Compression::None),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Some(Compression::Gzip),
        _ => None,
    }
}

/// One verified layer of a saved image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedLayer {
    /// The archive member holding it.
    pub member: String,
    offset: u64,
    /// Its stored size.
    pub size: u64,
    pub compression: Compression,
    /// The digest of its uncompressed bytes, as the config names it.
    pub diff_id: String,
    /// Its uncompressed size.
    pub diff_size: u64,
}

/// The one image of an engine save, read and verified for a platform.
#[derive(Debug)]
pub struct SavedImage {
    path: PathBuf,
    /// The exact image config bytes.
    pub config: Vec<u8>,
    pub layers: Vec<SavedLayer>,
}

#[derive(Clone, Copy, Debug)]
struct Member {
    offset: u64,
    size: u64,
}

struct Pending {
    member: String,
    location: Member,
    compression: Compression,
    /// The descriptor digest the stored blob must have, when the save
    /// records one.
    blob_digest: Option<String>,
}

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::RuntimeBase, detail)
}

impl SavedImage {
    /// Reads the `platform` image of the engine save at `path`.
    ///
    /// # Errors
    /// Fails on an unreadable or malformed archive, on any entry other than
    /// a directory, a regular file or a symlink to an archived file, on a
    /// blob or layer that differs from its digest, and when the image is not
    /// exactly one `platform` image of at most [`MAX_LAYERS`] layers.
    pub fn open(path: &Path, platform: Platform) -> Result<Self> {
        let archive = Archive::index(path)?;
        let (config, pending) = if archive.members.contains_key("index.json") {
            archive.oci(platform)?
        } else if archive.members.contains_key("manifest.json") {
            archive.legacy()?
        } else {
            return Err(fail(format!(
                "unrecognised image save archive {} (neither index.json nor manifest.json)",
                path.display()
            )));
        };
        let details: Value = serde_json::from_slice(&config)
            .map_err(|error| fail(format!("saved image config is not JSON: {error}")))?;
        if details["os"] != "linux" || details["architecture"] != platform.architecture() {
            return Err(fail(format!(
                "saved image platform {}/{} differs from the requested platform {}",
                details["os"].as_str().unwrap_or("?"),
                details["architecture"].as_str().unwrap_or("?"),
                platform.as_str()
            )));
        }
        let diff_ids: Vec<&str> = details["rootfs"]["diff_ids"]
            .as_array()
            .filter(|_| details["rootfs"]["type"] == "layers")
            .and_then(|ids| {
                ids.iter()
                    .map(|id| id.as_str().filter(|id| is_digest(id)))
                    .collect::<Option<Vec<&str>>>()
            })
            .filter(|ids| ids.len() == pending.len())
            .ok_or_else(|| fail("saved image config does not describe its layers"))?;
        if pending.len() > MAX_LAYERS {
            return Err(fail(format!(
                "saved image has {} layers; at most {MAX_LAYERS} fit beside the controller layer",
                pending.len()
            )));
        }
        let mut layers = Vec::with_capacity(pending.len());
        for (index, (layer, diff_id)) in pending.into_iter().zip(diff_ids).enumerate() {
            let (blob, uncompressed, diff_size) = archive.layer_digests(index, &layer)?;
            if uncompressed != diff_id || layer.blob_digest.is_some_and(|recorded| recorded != blob)
            {
                return Err(fail(format!(
                    "saved image layer {index} ({}) differs from its recorded digest",
                    layer.member
                )));
            }
            layers.push(SavedLayer {
                member: layer.member,
                offset: layer.location.offset,
                size: layer.location.size,
                compression: layer.compression,
                diff_id: uncompressed,
                diff_size,
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            config,
            layers,
        })
    }

    /// Layer `index`'s uncompressed bytes, read afresh from the archive. The
    /// caller re-hashes them: the archive is not trusted to stay unchanged.
    ///
    /// # Errors
    /// Fails when there is no such layer or the archive cannot be reopened.
    pub fn layer(&self, index: usize) -> Result<Box<dyn Read>> {
        let layer = self
            .layers
            .get(index)
            .ok_or_else(|| fail(format!("saved image has no layer {index}")))?;
        let mut file =
            File::open(&self.path).map_err(io(Stage::RuntimeBase, "open", &self.path))?;
        file.seek(SeekFrom::Start(layer.offset)).map_err(io(
            Stage::RuntimeBase,
            "read",
            &self.path,
        ))?;
        let stored = file.take(layer.size);
        Ok(match layer.compression {
            Compression::None => Box::new(stored),
            Compression::Gzip => Box::new(MultiGzDecoder::new(stored)),
        })
    }
}

struct Archive {
    path: PathBuf,
    file: File,
    members: BTreeMap<String, Member>,
}

/// `name` with a leading `./`, `.` components and `..` components resolved,
/// or `None` when it is absolute or escapes the archive root.
fn normalise(name: &str) -> Option<String> {
    if name.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in name.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

impl Archive {
    /// One pass over the archive recording where each regular file's data
    /// lies, with in-archive symlinks resolved one level to regular files.
    fn index(path: &Path) -> Result<Self> {
        let unreadable = |error: stdio::Error| {
            fail(format!(
                "cannot read image save archive {}: {error}",
                path.display()
            ))
        };
        let file = File::open(path).map_err(io(Stage::RuntimeBase, "open", path))?;
        let mut files = BTreeMap::new();
        let mut links = BTreeMap::new();
        let mut archive = tar::Archive::new(&file);
        for entry in archive.entries().map_err(unreadable)? {
            let entry = entry.map_err(unreadable)?;
            let kind = entry.header().entry_type();
            // The tar crate applies PAX and GNU long-name records to the entry
            // they describe; a global PAX header reaches here on its own and
            // describes nothing aps reads.
            if kind.is_pax_global_extensions()
                || kind.is_pax_local_extensions()
                || kind.is_gnu_longname()
                || kind.is_gnu_longlink()
            {
                continue;
            }
            let raw = entry.path().map_err(unreadable)?;
            let raw = raw
                .to_str()
                .ok_or_else(|| fail("image archive member name is not UTF-8"))?
                .to_owned();
            let name = normalise(&raw)
                .filter(|name| !name.is_empty() || kind.is_dir())
                .ok_or_else(|| fail(format!("image archive member {raw:?} escapes the archive")))?;
            if kind.is_dir() {
                continue;
            }
            if files.contains_key(&name) || links.contains_key(&name) {
                return Err(fail(format!("duplicate image archive member {name}")));
            }
            if kind.is_file() {
                let member = Member {
                    offset: entry.raw_file_position(),
                    size: entry.size(),
                };
                files.insert(name, member);
            } else if kind.is_symlink() {
                let target = entry
                    .link_name()
                    .map_err(unreadable)?
                    .and_then(|target| target.to_str().map(str::to_owned))
                    .ok_or_else(|| {
                        fail(format!("image archive symlink {name} has no UTF-8 target"))
                    })?;
                let joined = match name.rsplit_once('/') {
                    Some((parent, _)) => format!("{parent}/{target}"),
                    None => target.clone(),
                };
                let resolved = (!target.starts_with('/'))
                    .then(|| normalise(&joined))
                    .flatten()
                    .ok_or_else(|| {
                        fail(format!(
                            "image archive symlink {name} escapes the archive ({target})"
                        ))
                    })?;
                links.insert(name, resolved);
            } else {
                return Err(fail(format!(
                    "image archive contains a non-regular entry {name}"
                )));
            }
        }
        let mut resolved = Vec::with_capacity(links.len());
        for (name, target) in links {
            let member = files.get(&target).copied().ok_or_else(|| {
                fail(format!(
                    "image archive symlink {name} does not name an archived file"
                ))
            })?;
            resolved.push((name, member));
        }
        files.extend(resolved);
        Ok(Self {
            path: path.to_path_buf(),
            file,
            members: files,
        })
    }

    fn stream(&self, member: Member) -> Result<impl Read + '_> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(member.offset)).map_err(io(
            Stage::RuntimeBase,
            "read",
            &self.path,
        ))?;
        Ok(file.take(member.size))
    }

    /// A metadata member's bytes, at most 1 MiB.
    fn read(&self, name: &str) -> Result<Vec<u8>> {
        let member = *self
            .members
            .get(name)
            .ok_or_else(|| fail(format!("image archive lacks {name}")))?;
        if member.size > MAX_METADATA {
            return Err(fail(format!("image metadata {name} is oversized")));
        }
        let mut bytes = Vec::new();
        self.stream(member)?.read_to_end(&mut bytes).map_err(io(
            Stage::RuntimeBase,
            "read",
            &self.path,
        ))?;
        if bytes.len() as u64 != member.size {
            return Err(fail(format!("image save archive ends inside {name}")));
        }
        Ok(bytes)
    }

    fn json(&self, name: &str) -> Result<Value> {
        serde_json::from_slice(&self.read(name)?)
            .map_err(|error| fail(format!("image archive member {name} is not JSON: {error}")))
    }

    /// The blob a descriptor names, checked against its digest and, when
    /// given, its size.
    fn blob(&self, descriptor: &Value) -> Result<Vec<u8>> {
        let digest_value = descriptor["digest"]
            .as_str()
            .filter(|value| is_digest(value))
            .ok_or_else(|| fail("image archive requires SHA-256 descriptors"))?;
        let name = format!("blobs/sha256/{}", &digest_value[7..]);
        if !self.members.contains_key(&name) {
            return Err(fail(format!(
                "image archive lacks a referenced blob {digest_value}"
            )));
        }
        let bytes = self.read(&name)?;
        let size_matches = descriptor
            .get("size")
            .is_none_or(|size| size.as_u64() == Some(bytes.len() as u64));
        if digest(&bytes) != digest_value || !size_matches {
            return Err(fail(format!(
                "image archive blob {digest_value} digest mismatch"
            )));
        }
        Ok(bytes)
    }

    /// The OCI layout branch: `index.json`, through at most
    /// [`MAX_INDEX_HOPS`] documents, to the platform's manifest.
    fn oci(&self, platform: Platform) -> Result<(Vec<u8>, Vec<Pending>)> {
        let index = self.json("index.json")?;
        let entries = index["manifests"].as_array().map_or(&[][..], Vec::as_slice);
        let [entry] = entries else {
            return Err(fail("image archive must hold exactly one image"));
        };
        let mut descriptor = entry.clone();
        let mut document = Value::Null;
        let mut kind = String::new();
        for _ in 0..MAX_INDEX_HOPS {
            document = serde_json::from_slice(&self.blob(&descriptor)?)
                .map_err(|error| fail(format!("image archive document is not JSON: {error}")))?;
            document["mediaType"]
                .as_str()
                .or_else(|| descriptor["mediaType"].as_str())
                .unwrap_or_default()
                .clone_into(&mut kind);
            if !INDEX_TYPES.contains(&kind.as_str()) {
                break;
            }
            let matches: Vec<&Value> = document["manifests"]
                .as_array()
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter(|item| {
                    item["platform"]["os"] == "linux"
                        && item["platform"]["architecture"] == platform.architecture()
                })
                .collect();
            let [selected] = matches.as_slice() else {
                return Err(fail(
                    "saved image index must select exactly one platform manifest",
                ));
            };
            descriptor = (*selected).clone();
        }
        if !MANIFEST_TYPES.contains(&kind.as_str()) {
            return Err(fail(
                "saved image does not resolve to a single-platform manifest",
            ));
        }
        if document["config"]["size"].as_u64().is_none() {
            return Err(fail("saved image manifest config lacks its size"));
        }
        let config = self.blob(&document["config"])?;
        let descriptors = document["layers"]
            .as_array()
            .ok_or_else(|| fail("saved image manifest lacks its layers"))?;
        let mut pending = Vec::with_capacity(descriptors.len());
        for layer in descriptors {
            let media_type = layer["mediaType"].as_str().unwrap_or_default();
            let compression = layer_compression(media_type).ok_or_else(|| {
                fail(format!(
                    "saved image uses an unsupported layer media type {media_type:?}"
                ))
            })?;
            let blob_digest = layer["digest"]
                .as_str()
                .filter(|value| is_digest(value))
                .ok_or_else(|| fail("image archive requires SHA-256 descriptors"))?;
            let name = format!("blobs/sha256/{}", &blob_digest[7..]);
            let location = self
                .members
                .get(&name)
                .copied()
                .filter(|member| layer["size"].as_u64() == Some(member.size))
                .ok_or_else(|| fail(format!("image archive lacks a layer blob {blob_digest}")))?;
            pending.push(Pending {
                member: name,
                location,
                compression,
                blob_digest: Some(blob_digest.to_owned()),
            });
        }
        Ok((config, pending))
    }

    /// The legacy docker-archive branch: `manifest.json` names a config
    /// member and uncompressed layer members.
    fn legacy(&self) -> Result<(Vec<u8>, Vec<Pending>)> {
        let manifest = self.json("manifest.json")?;
        let entries = manifest.as_array().map_or(&[][..], Vec::as_slice);
        let [entry] = entries else {
            return Err(fail("image archive must hold exactly one image"));
        };
        let config_name = entry["Config"]
            .as_str()
            .and_then(normalise)
            .ok_or_else(|| fail("legacy image archive lacks its config"))?;
        let config_hex = config_name
            .strip_suffix(".json")
            .filter(|hex| is_hex64(hex))
            .or_else(|| {
                config_name
                    .strip_prefix("blobs/sha256/")
                    .filter(|hex| is_hex64(hex))
            })
            .ok_or_else(|| {
                fail(format!(
                    "legacy image archive config {config_name} is not named by its digest"
                ))
            })?;
        if !self.members.contains_key(&config_name) {
            return Err(fail("legacy image archive lacks its config"));
        }
        let config = self.read(&config_name)?;
        if digest(&config)[7..] != *config_hex {
            return Err(fail(format!(
                "legacy image archive config {config_name} does not match its digest"
            )));
        }
        let names = entry["Layers"]
            .as_array()
            .ok_or_else(|| fail("legacy image archive lacks its layers"))?;
        let mut pending = Vec::with_capacity(names.len());
        for name in names {
            let name = name
                .as_str()
                .and_then(normalise)
                .ok_or_else(|| fail("legacy image archive names a layer outside the archive"))?;
            let location = self
                .members
                .get(&name)
                .copied()
                .ok_or_else(|| fail(format!("legacy image archive lacks a layer {name}")))?;
            pending.push(Pending {
                member: name,
                location,
                compression: Compression::None,
                blob_digest: None,
            });
        }
        Ok((config, pending))
    }

    /// A layer's stored digest, uncompressed digest and uncompressed size.
    fn layer_digests(&self, index: usize, layer: &Pending) -> Result<(String, String, u64)> {
        let broken = |error: stdio::Error| {
            fail(format!(
                "cannot read saved image layer {index} ({}): {error}",
                layer.member
            ))
        };
        let mut stored = HashingReader::new(self.stream(layer.location)?);
        let (uncompressed, diff_size) = match layer.compression {
            Compression::None => {
                stdio::copy(&mut stored, &mut stdio::sink()).map_err(broken)?;
                (None, 0)
            }
            Compression::Gzip => {
                let mut decoded = HashingReader::new(MultiGzDecoder::new(&mut stored));
                stdio::copy(&mut decoded, &mut stdio::sink()).map_err(broken)?;
                let (uncompressed, size) = decoded.finish();
                stdio::copy(&mut stored, &mut stdio::sink()).map_err(broken)?;
                (Some(uncompressed), size)
            }
        };
        let (blob, read) = stored.finish();
        if read != layer.location.size {
            return Err(fail(format!(
                "image save archive ends inside layer {index} ({})",
                layer.member
            )));
        }
        Ok(match uncompressed {
            None => (blob.clone(), blob, read),
            Some(uncompressed) => (blob, uncompressed, diff_size),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{
        Item, OCI_CONFIG, OCI_INDEX, OCI_LAYER, OCI_MANIFEST, TestResult, archive, base_layer,
        blob_name, classic_members, containerd_members, descriptor, file, gzip, image_config,
        index, layer, legacy_members, manifest, podman_docker_members, podman_oci_members,
        second_layer, with_platform, write,
    };
    use serde_json::json;

    struct Fixture {
        root: tempfile::TempDir,
        layers: Vec<Vec<u8>>,
        config: Vec<u8>,
    }

    impl Fixture {
        fn new() -> TestResult<Self> {
            let layers = vec![base_layer()?, second_layer()?];
            let config = image_config(&layers, "arm64")?;
            Ok(Self {
                root: tempfile::tempdir()?,
                layers,
                config,
            })
        }

        fn save(&self, name: &str, members: Vec<(String, Item)>) -> TestResult<PathBuf> {
            Ok(write(self.root.path(), name, &archive(members)?)?)
        }

        fn compressed(&self) -> TestResult<Vec<Vec<u8>>> {
            Ok(self
                .layers
                .iter()
                .map(|layer| gzip(layer))
                .collect::<std::io::Result<_>>()?)
        }

        fn rejection(&self, name: &str, members: Vec<(String, Item)>) -> TestResult<String> {
            let path = self.save(name, members)?;
            let error = SavedImage::open(&path, Platform::Arm64)
                .err()
                .ok_or_else(|| format!("{name} was accepted"))?;
            assert_eq!(error.stage, Stage::RuntimeBase);
            Ok(error.detail)
        }
    }

    fn assert_contains(detail: &str, expected: &str) {
        assert!(detail.contains(expected), "{detail:?} lacks {expected:?}");
    }

    #[test]
    fn every_save_format_yields_the_same_config_and_layers() -> TestResult {
        let fixture = Fixture::new()?;
        let compressed = fixture.compressed()?;
        let saves = [
            (
                "classic.tar",
                classic_members(&fixture.config, &fixture.layers)?,
                Compression::None,
            ),
            (
                "containerd.tar",
                containerd_members(&fixture.config, &compressed)?,
                Compression::Gzip,
            ),
            (
                "legacy.tar",
                legacy_members(
                    &fixture.config,
                    &[fixture.layers[0].clone(), fixture.layers[1].clone()],
                )?,
                Compression::None,
            ),
            (
                "podman-docker.tar",
                podman_docker_members(&fixture.config, &fixture.layers)?,
                Compression::None,
            ),
            (
                "podman-oci.tar",
                podman_oci_members(&fixture.config, &compressed)?,
                Compression::Gzip,
            ),
        ];
        for (name, members, compression) in saves {
            let image = SavedImage::open(&fixture.save(name, members)?, Platform::Arm64)?;
            assert_eq!(image.config, fixture.config, "{name}");
            assert_eq!(
                image
                    .layers
                    .iter()
                    .map(|layer| (layer.diff_id.clone(), layer.diff_size))
                    .collect::<Vec<_>>(),
                fixture
                    .layers
                    .iter()
                    .map(|layer| (digest(layer), layer.len() as u64))
                    .collect::<Vec<_>>(),
                "{name}"
            );
            assert!(
                image
                    .layers
                    .iter()
                    .all(|layer| layer.compression == compression),
                "{name}"
            );
            for (index, expected) in fixture.layers.iter().enumerate() {
                let mut bytes = Vec::new();
                image.layer(index)?.read_to_end(&mut bytes)?;
                assert_eq!(&bytes, expected, "{name} layer {index}");
            }
        }
        Ok(())
    }

    #[test]
    fn a_multi_member_gzip_layer_is_accepted() -> TestResult {
        let fixture = Fixture::new()?;
        let first = &fixture.layers[0];
        let mut split = gzip(&first[..1024])?;
        split.extend(gzip(&first[1024..])?);
        let compressed = vec![split, gzip(&fixture.layers[1])?];
        let image = SavedImage::open(
            &fixture.save(
                "multi.tar",
                containerd_members(&fixture.config, &compressed)?,
            )?,
            Platform::Arm64,
        )?;
        assert_eq!(image.layers[0].diff_id, digest(first));
        assert_eq!(image.layers[0].diff_size, first.len() as u64);
        Ok(())
    }

    #[test]
    fn another_platform_or_a_substituted_layer_is_rejected() -> TestResult {
        let fixture = Fixture::new()?;
        let classic = fixture.save(
            "classic.tar",
            classic_members(&fixture.config, &fixture.layers)?,
        )?;
        let error = SavedImage::open(&classic, Platform::Amd64)
            .err()
            .ok_or("amd64 accepted")?;
        assert_contains(
            &error.detail,
            "differs from the requested platform linux/amd64",
        );
        let containerd = fixture.save(
            "containerd.tar",
            containerd_members(&fixture.config, &fixture.compressed()?)?,
        )?;
        assert!(SavedImage::open(&containerd, Platform::Amd64).is_err());
        let substituted = vec![fixture.layers[0].clone(), layer(&[("usr", None)])?];
        let detail = fixture.rejection(
            "substituted.tar",
            classic_members(&fixture.config, &substituted)?,
        )?;
        assert_contains(&detail, "layer 1");
        assert_contains(&detail, "differs from its recorded digest");
        Ok(())
    }

    #[test]
    fn archive_structure_is_checked() -> TestResult {
        let fixture = Fixture::new()?;
        let detail = fixture.rejection(
            "escape.tar",
            vec![
                file("manifest.json", b"[]".to_vec()),
                ("link".into(), Item::Symlink("../../outside".into())),
            ],
        )?;
        assert_contains(&detail, "symlink link escapes the archive");
        let detail = fixture.rejection(
            "dangling.tar",
            vec![
                file("manifest.json", b"[]".to_vec()),
                ("link".into(), Item::Symlink("absent".into())),
            ],
        )?;
        assert_contains(&detail, "does not name an archived file");
        let detail = fixture.rejection(
            "absolute.tar",
            vec![
                file("manifest.json", b"[]".to_vec()),
                ("link".into(), Item::Symlink("/etc/passwd".into())),
            ],
        )?;
        assert_contains(&detail, "escapes the archive");
        let mut duplicated = classic_members(&fixture.config, &fixture.layers)?;
        duplicated.push(file("./index.json", b"{}".to_vec()));
        assert_contains(
            &fixture.rejection("duplicate.tar", duplicated)?,
            "duplicate image archive member index.json",
        );
        let mut fifo = classic_members(&fixture.config, &fixture.layers)?;
        fifo.push(("pipe".into(), Item::Fifo));
        assert_contains(
            &fixture.rejection("fifo.tar", fifo)?,
            "non-regular entry pipe",
        );
        let detail =
            fixture.rejection("neither.tar", vec![file("repositories", b"{}".to_vec())])?;
        assert_contains(&detail, "neither index.json nor manifest.json");
        let detail = fixture.rejection(
            "oversized.tar",
            vec![file("index.json", vec![b' '; 1024 * 1024 + 1])],
        )?;
        assert_contains(&detail, "image metadata index.json is oversized");
        Ok(())
    }

    #[test]
    fn blobs_must_match_their_descriptors() -> TestResult {
        let fixture = Fixture::new()?;
        let mut tampered = classic_members(&fixture.config, &fixture.layers)?;
        let config_name = blob_name(&fixture.config);
        for member in &mut tampered {
            if member.0 == config_name {
                member.1 = Item::File(image_config(&fixture.layers, "amd64")?);
            }
        }
        assert_contains(
            &fixture.rejection("mismatch.tar", tampered)?,
            "digest mismatch",
        );
        let missing: Vec<_> = classic_members(&fixture.config, &fixture.layers)?
            .into_iter()
            .filter(|(name, _)| *name != config_name)
            .collect();
        assert_contains(
            &fixture.rejection("missing.tar", missing)?,
            "lacks a referenced blob",
        );
        Ok(())
    }

    /// An OCI save whose `index.json` names `top`, with `extra` blobs.
    fn oci_save(
        fixture: &Fixture,
        top: &[serde_json::Value],
        extra: Vec<Vec<u8>>,
        layers: &[Vec<u8>],
    ) -> TestResult<Vec<(String, Item)>> {
        let mut members = vec![
            file("index.json", index(top)?),
            file(blob_name(&fixture.config), fixture.config.clone()),
        ];
        for blob in extra {
            members.push(file(blob_name(&blob), blob));
        }
        for layer in layers {
            members.push(file(blob_name(layer), layer.clone()));
        }
        Ok(members)
    }

    #[test]
    fn manifests_must_resolve_to_one_supported_image() -> TestResult {
        let fixture = Fixture::new()?;
        let plain = manifest(
            OCI_MANIFEST,
            OCI_CONFIG,
            &fixture.config,
            OCI_LAYER,
            &fixture.layers,
        )?;
        let entry = descriptor(OCI_MANIFEST, &plain);
        let detail = fixture.rejection(
            "two.tar",
            oci_save(
                &fixture,
                &[entry.clone(), entry],
                vec![plain.clone()],
                &fixture.layers,
            )?,
        )?;
        assert_contains(&detail, "exactly one image");

        let nest = |inner: &[u8]| index(&[with_platform(descriptor(OCI_INDEX, inner), "arm64")]);
        let first = nest(&plain)?;
        let second = nest(&first)?;
        let accepted = oci_save(
            &fixture,
            &[descriptor(OCI_INDEX, &second)],
            vec![plain.clone(), first.clone(), second.clone()],
            &fixture.layers,
        )?;
        SavedImage::open(&fixture.save("two-hops.tar", accepted)?, Platform::Arm64)?;
        let third = nest(&second)?;
        let detail = fixture.rejection(
            "three-hops.tar",
            oci_save(
                &fixture,
                &[descriptor(OCI_INDEX, &third)],
                vec![plain, first, second, third],
                &fixture.layers,
            )?,
        )?;
        assert_contains(&detail, "does not resolve to a single-platform manifest");

        let zstd = manifest(
            OCI_MANIFEST,
            OCI_CONFIG,
            &fixture.config,
            "application/vnd.oci.image.layer.v1.tar+zstd",
            &fixture.layers,
        )?;
        let detail = fixture.rejection(
            "zstd.tar",
            oci_save(
                &fixture,
                &[descriptor(OCI_MANIFEST, &zstd)],
                vec![zstd.clone()],
                &fixture.layers,
            )?,
        )?;
        assert_contains(&detail, "unsupported layer media type");

        let mut resized: serde_json::Value = serde_json::from_slice(&manifest(
            OCI_MANIFEST,
            OCI_CONFIG,
            &fixture.config,
            OCI_LAYER,
            &fixture.layers,
        )?)?;
        resized["layers"][0]["size"] = json!(fixture.layers[0].len() + 1);
        let resized = serde_json::to_vec(&resized)?;
        let detail = fixture.rejection(
            "resized.tar",
            oci_save(
                &fixture,
                &[descriptor(OCI_MANIFEST, &resized)],
                vec![resized.clone()],
                &fixture.layers,
            )?,
        )?;
        assert_contains(&detail, "lacks a layer blob");

        let mut unsized_config: serde_json::Value = serde_json::from_slice(&manifest(
            OCI_MANIFEST,
            OCI_CONFIG,
            &fixture.config,
            OCI_LAYER,
            &fixture.layers,
        )?)?;
        unsized_config["config"]
            .as_object_mut()
            .ok_or("expected a config descriptor")?
            .remove("size");
        let unsized_config = serde_json::to_vec(&unsized_config)?;
        let detail = fixture.rejection(
            "unsized-config.tar",
            oci_save(
                &fixture,
                &[descriptor(OCI_MANIFEST, &unsized_config)],
                vec![unsized_config.clone()],
                &fixture.layers,
            )?,
        )?;
        assert_contains(&detail, "config lacks its size");
        Ok(())
    }

    #[test]
    fn a_legacy_config_must_be_named_by_its_digest() -> TestResult {
        let fixture = Fixture::new()?;
        let wrong = format!("{}.json", "a".repeat(64));
        let manifest = serde_json::to_vec(&json!([{"Config": wrong, "Layers": ["layer.tar"]}]))?;
        let detail = fixture.rejection(
            "renamed.tar",
            vec![
                file("manifest.json", manifest),
                file(wrong, fixture.config.clone()),
                file("layer.tar", fixture.layers[0].clone()),
            ],
        )?;
        assert_contains(&detail, "does not match its digest");
        let manifest = serde_json::to_vec(&json!([{"Config": "config.json", "Layers": []}]))?;
        let detail = fixture.rejection(
            "unnamed.tar",
            vec![
                file("manifest.json", manifest),
                file("config.json", fixture.config.clone()),
            ],
        )?;
        assert_contains(&detail, "is not named by its digest");
        Ok(())
    }

    #[test]
    fn at_most_63_base_layers_are_accepted() -> TestResult {
        let fixture = Fixture::new()?;
        let layers = (0..64)
            .map(|index| layer(&[("data", Some(format!("{index}").as_bytes()))]))
            .collect::<std::io::Result<Vec<_>>>()?;
        let config = image_config(&layers, "arm64")?;
        let detail = fixture.rejection("deep.tar", classic_members(&config, &layers)?)?;
        assert_contains(&detail, "at most 63");
        let config = image_config(&layers[..63], "arm64")?;
        let image = SavedImage::open(
            &fixture.save("deepest.tar", classic_members(&config, &layers[..63])?)?,
            Platform::Arm64,
        )?;
        assert_eq!(image.layers.len(), 63);
        Ok(())
    }

    #[test]
    fn names_are_normalised_inside_the_root() {
        assert_eq!(
            normalise("./blobs/sha256/../sha256/x").as_deref(),
            Some("blobs/sha256/x")
        );
        assert_eq!(normalise("a//b/./c").as_deref(), Some("a/b/c"));
        assert_eq!(normalise("../x"), None);
        assert_eq!(normalise("/etc/passwd"), None);
    }
}
