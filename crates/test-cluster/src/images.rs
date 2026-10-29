//! Image archives and their import into the node's containerd.
//!
//! Every image the suite runs arrives as a local archive in OCI layout (the
//! form `aps build`, `docker image save` and
//! `podman image save --format oci-archive` write). The
//! runner reads the archive's single manifest and config, imports it with
//! `ctr images import --digests`, and then asks the node's CRI which image
//! that digest reference resolves to. Pods name images by that digest
//! reference and `imagePullPolicy: Never`, so nothing is fetched from a
//! registry during the run.

use crate::{
    Error, Result,
    engine::{self, Engine, EngineCommands as _},
    infrastructure,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, io::Read as _, path::Path, time::Duration};

const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;

const MANIFESTS: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];
/// Where an archive's index names its image: Docker writes containerd's
/// annotation, podman the OCI reference name.
const NAME_ANNOTATIONS: [&str; 2] = [
    "io.containerd.image.name",
    "org.opencontainers.image.ref.name",
];
const INDEXES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// The identity of the single image an archive holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveImage {
    /// Digest of what the archive's index selects: a single-platform manifest,
    /// or an image index when the archive came from a multi-platform store.
    /// `ctr images import --digests` names the image by this digest.
    pub manifest: String,
    /// Digest of the config of the manifest for the requested platform: the
    /// image ID the CRI reports.
    pub config: String,
    /// Names the archive declares for the image: containerd's name annotation
    /// (Docker) or the OCI reference name (podman).
    pub names: Vec<String>,
}

/// Read the identity of the `platform` image out of an OCI-layout archive
/// without unpacking it.
///
/// # Errors
/// Rejects archives that are oversized, not OCI layout, hold other than one
/// image for `platform`, or whose metadata does not match its digests.
pub fn inspect(path: &Path, platform: &str) -> Result<ArchiveImage> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_ARCHIVE_BYTES {
        return infrastructure(format!(
            "image archive {} is not a bounded regular file",
            path.display()
        ));
    }
    let index = entries(path, |name| name == "index.json")?
        .remove("index.json")
        .ok_or_else(|| {
            Error::Infrastructure(format!("{} is not an OCI-layout archive", path.display()))
        })?;
    let index: Value = serde_json::from_slice(&index)?;
    let [descriptor] = index["manifests"].as_array().map_or(&[][..], Vec::as_slice) else {
        return infrastructure(format!("{} must hold exactly one image", path.display()));
    };
    let target = digest_field(descriptor)?;
    let names = NAME_ANNOTATIONS
        .iter()
        .filter_map(|key| descriptor["annotations"][key].as_str().map(str::to_owned))
        .collect();
    let mut selected = blob(path, &target)?;
    if INDEXES.iter().any(|kind| selected["mediaType"] == *kind) {
        let (os, architecture) = platform.split_once('/').unwrap_or_default();
        let matching = selected["manifests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| {
                item["platform"]["os"] == os && item["platform"]["architecture"] == architecture
            })
            .collect::<Vec<_>>();
        let [manifest] = matching.as_slice() else {
            return infrastructure(format!(
                "{} must hold exactly one {platform} image",
                path.display()
            ));
        };
        selected = blob(path, &digest_field(manifest)?)?;
    }
    if !MANIFESTS.iter().any(|kind| selected["mediaType"] == *kind) {
        return infrastructure("image archive does not select an image manifest");
    }
    let config = digest_field(&selected["config"])?;
    blob(path, &config)?;
    Ok(ArchiveImage {
        manifest: target,
        config,
        names,
    })
}

/// A JSON blob from the archive, checked against its digest.
fn blob(path: &Path, digest: &str) -> Result<Value> {
    let wanted = blob_path(digest);
    let bytes = entries(path, |name| name == wanted)?
        .remove(&wanted)
        .ok_or_else(|| Error::Infrastructure(format!("image blob {digest} is missing")))?;
    verify_digest(&bytes, digest)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Import an archive into the node's `k8s.io` containerd namespace.
///
/// An image the archive leaves unnamed is recorded as
/// `{repository}@{manifest}`; a named one keeps the name the archive gives it.
/// Either way the caller confirms the reference it will use with
/// [`require_resolves`].
///
/// # Errors
/// Fails when the archive cannot be read or containerd rejects it.
pub async fn import(node: &str, archive: &Path, repository: &str, platform: &str) -> Result<()> {
    engine::current()?
        .command()
        .args([
            "exec", "-i", node, "ctr", "-n", "k8s.io", "images", "import",
        ])
        .args([
            "--digests",
            "--platform",
            platform,
            "--base-name",
            repository,
            "-",
        ])
        .stdin(archive.to_path_buf())
        .timeout(Duration::from_secs(600))
        .run()
        .await?;
    Ok(())
}

/// How long the CRI plugin may take to see an image `ctr` just wrote. `ctr`
/// writes containerd's image store directly and the CRI plugin learns of the
/// change from containerd's event stream, so its image cache can lag.
const CRI_SYNC: Duration = Duration::from_secs(60);

/// Fail unless the node's CRI resolves `reference` to the image `config`
/// within [`CRI_SYNC`].
///
/// # Errors
/// Fails when the reference stays unknown to the node or names another image.
pub async fn require_resolves(node: &str, reference: &str, config: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + CRI_SYNC;
    loop {
        let output = engine::current()?
            .command()
            .args(["exec", node, "crictl", "inspecti", "-o", "json", reference])
            .output()
            .await?;
        let resolved = serde_json::from_str::<Value>(&output.stdout)
            .ok()
            .and_then(|value| value["status"]["id"].as_str().map(str::to_owned));
        if output.success && resolved.as_deref() == Some(config) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let found = resolved.map_or_else(
                || format!("crictl: {}", output.stderr.trim()),
                |id| format!("it resolves to {id}"),
            );
            return infrastructure(format!(
                "the node does not resolve {reference} to the imported image {config} ({found})"
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The fully qualified repository containerd records for `reference`, which
/// may be a short Docker Hub name, carry a tag, or carry a digest.
///
/// # Errors
/// Rejects an empty reference or one containing whitespace.
pub fn repository(reference: &str) -> Result<String> {
    let name = reference.split('@').next().unwrap_or_default();
    let (prefix, last) = name
        .rsplit_once('/')
        .map_or(("", name), |(prefix, last)| (prefix, last));
    let last = last.split(':').next().unwrap_or_default();
    if last.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return infrastructure(format!("{reference} is not an image reference"));
    }
    let registry = prefix.split('/').next().unwrap_or_default();
    Ok(if prefix.is_empty() {
        format!("docker.io/library/{last}")
    } else if registry.contains('.') || registry.contains(':') || registry == "localhost" {
        format!("{prefix}/{last}")
    } else {
        format!("docker.io/{prefix}/{last}")
    })
}

/// An OCI-layout archive of a locally cached image, written into
/// `directory`. The image must already be present: nothing is pulled.
///
/// Docker saves `reference` as its store holds it. podman saves the image by
/// its ID: podman's archive holds only the single-platform manifest, and
/// saved under a digest reference it would still carry the pinned index
/// digest as its name, which then names other content in the node.
///
/// # Errors
/// Fails when the image is not local or the engine cannot save it.
pub async fn save(reference: &str, directory: &Path, file: &str) -> Result<std::path::PathBuf> {
    let engine = engine::current()?;
    let present = engine
        .command()
        .args(["image", "inspect", "--format", "{{.Id}}", reference])
        .output()
        .await?;
    let id = present.stdout.trim();
    if !present.success || id.is_empty() {
        return infrastructure(format!(
            "{reference} is not in the local {} image store; pull it before running the suite",
            engine.name()
        ));
    }
    let path = directory.join(file);
    let saved = match engine {
        Engine::Docker => engine
            .command()
            .args(["image", "save", "--output"])
            .arg(&path)
            .arg(reference),
        Engine::Podman => engine
            .command()
            .args(["image", "save", "--format", "oci-archive", "--output"])
            .arg(&path)
            .arg(id),
    };
    saved.timeout(Duration::from_secs(300)).run().await?;
    Ok(path)
}

/// Files in the archive whose names `wanted` accepts, each bounded.
fn entries(path: &Path, wanted: impl Fn(&str) -> bool) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut archive = tar::Archive::new(std::fs::File::open(path)?);
    let mut found = BTreeMap::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let name = entry
            .path()?
            .to_string_lossy()
            .trim_start_matches("./")
            .to_owned();
        if !entry.header().entry_type().is_file() || !wanted(&name) {
            continue;
        }
        if entry.size() > MAX_METADATA_BYTES {
            return infrastructure(format!("archive member {name} is oversized"));
        }
        let mut bytes = Vec::new();
        entry.take(MAX_METADATA_BYTES).read_to_end(&mut bytes)?;
        if found.insert(name.clone(), bytes).is_some() {
            return infrastructure(format!("archive member {name} is duplicated"));
        }
    }
    Ok(found)
}

fn digest_field(descriptor: &Value) -> Result<String> {
    let digest = descriptor["digest"].as_str().unwrap_or_default();
    let valid = digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    });
    if !valid {
        return infrastructure("image descriptor must use a SHA-256 digest");
    }
    Ok(digest.to_owned())
}

fn blob_path(digest: &str) -> String {
    format!("blobs/sha256/{}", digest.trim_start_matches("sha256:"))
}

fn verify_digest(bytes: &[u8], expected: &str) -> Result<()> {
    if format!("sha256:{:x}", Sha256::digest(bytes)) != expected {
        return infrastructure("image archive blob does not match its digest");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    fn archive(entries: &[(String, Vec<u8>)]) -> Result<tempfile::NamedTempFile> {
        let file = tempfile::NamedTempFile::new()?;
        let mut builder = tar::Builder::new(file.reopen()?);
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, bytes.as_slice())?;
        }
        builder.finish()?;
        Ok(file)
    }

    type Entries = Vec<(String, Vec<u8>)>;

    fn layout(annotations: &Value) -> Result<(Entries, String, String)> {
        let config = br#"{"architecture":"arm64","os":"linux"}"#.to_vec();
        let config_digest = digest(&config);
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": config_digest, "size": config.len()},
            "layers": [],
        }))?;
        let manifest_digest = digest(&manifest);
        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json",
                           "digest": manifest_digest, "size": manifest.len(),
                           "annotations": annotations}],
        }))?;
        let entries = vec![
            (
                "oci-layout".to_owned(),
                br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec(),
            ),
            ("index.json".to_owned(), index),
            (blob_path(&manifest_digest), manifest),
            (blob_path(&config_digest), config),
        ];
        Ok((entries, manifest_digest, config_digest))
    }

    #[test]
    fn inspection_reads_the_single_manifest_and_config() -> Result<()> {
        let (entries, manifest, config) = layout(
            &json!({"io.containerd.image.name": "docker.io/library/apsides-fixture:local"}),
        )?;
        let file = archive(&entries)?;
        let image = inspect(file.path(), "linux/arm64")?;
        assert_eq!(image.manifest, manifest);
        assert_eq!(image.config, config);
        assert_eq!(image.names, ["docker.io/library/apsides-fixture:local"]);
        Ok(())
    }

    #[test]
    fn inspection_reads_the_name_podman_writes() -> Result<()> {
        let (entries, _, _) = layout(
            &json!({"org.opencontainers.image.ref.name": "docker.io/library/apsides-fixture:local"}),
        )?;
        let file = archive(&entries)?;
        let image = inspect(file.path(), "linux/arm64")?;
        assert_eq!(image.names, ["docker.io/library/apsides-fixture:local"]);
        Ok(())
    }

    /// The containerd image store saves an index that points at a
    /// multi-platform index; the requested platform's config is the identity.
    #[test]
    fn inspection_descends_into_a_multi_platform_index() -> Result<()> {
        let (mut entries, manifest, config) = layout(&json!({}))?;
        let nested = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": manifest,
                 "size": 1, "platform": {"os": "linux", "architecture": "arm64"}},
                {"mediaType": "application/vnd.oci.image.manifest.v1+json",
                 "digest": format!("sha256:{}", "0".repeat(64)),
                 "size": 1, "platform": {"os": "unknown", "architecture": "unknown"}},
            ],
        }))?;
        let nested_digest = digest(&nested);
        entries[1].1 = serde_json::to_vec(&json!({"schemaVersion": 2, "manifests": [
            {"mediaType": "application/vnd.oci.image.index.v1+json", "digest": nested_digest,
             "size": nested.len()}]}))?;
        entries.push((blob_path(&nested_digest), nested));
        let file = archive(&entries)?;
        let image = inspect(file.path(), "linux/arm64")?;
        assert_eq!(image.manifest, nested_digest);
        assert_eq!(image.config, config);
        assert!(inspect(file.path(), "linux/amd64").is_err());
        Ok(())
    }

    #[test]
    fn inspection_rejects_tampered_blobs() -> Result<()> {
        let (mut entries, _, _) = layout(&json!({}))?;
        if let Some(config) = entries.last_mut() {
            config.1 = b"{}".to_vec();
        }
        let file = archive(&entries)?;
        assert!(inspect(file.path(), "linux/arm64").is_err());
        Ok(())
    }

    #[test]
    fn inspection_rejects_archives_without_an_oci_index() -> Result<()> {
        let file = archive(&[("manifest.json".to_owned(), b"[]".to_vec())])?;
        assert!(inspect(file.path(), "linux/arm64").is_err());
        Ok(())
    }

    #[test]
    fn repositories_are_normalised_the_way_containerd_names_them() -> Result<()> {
        assert_eq!(
            repository("apsides-fixture:local")?,
            "docker.io/library/apsides-fixture"
        );
        assert_eq!(
            repository(crate::PROXY_IMAGE)?,
            "docker.io/nginxinc/nginx-unprivileged"
        );
        assert_eq!(
            repository("ghcr.io/rawkode/controller@sha256:00")?,
            "ghcr.io/rawkode/controller"
        );
        assert_eq!(
            repository("localhost:5000/team/app:v1")?,
            "localhost:5000/team/app"
        );
        assert!(repository("").is_err());
        Ok(())
    }
}
