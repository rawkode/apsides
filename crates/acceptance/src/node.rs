//! Images on the kind node.
//!
//! Release controller images arrive as the OCI archives the releases carry
//! and are imported by digest. Third-party images the platform or the example
//! programs pin by digest (the Envoy proxy and its admin helper, the
//! `CloudNativePG` operator, `PostgreSQL` and `PostgREST`) are pulled into the
//! node's containerd by that digest before anything uses them. A host-side
//! image save cannot stand in for them: a classic (overlay2) Docker image
//! store and podman both save a single-platform manifest rather than the
//! pinned index, so the node would hold the image under a different digest
//! than the pin.

use crate::{Result, digest, ensure, infrastructure, objects::Objects};
use serde_json::Value;
use std::time::Duration;
use test_cluster::{
    engine::{self, EngineCommands as _},
    images,
};

/// The node's `os/architecture`, as release manifests record platforms.
///
/// # Errors
/// Fails when the cluster does not have exactly one node.
pub async fn platform(objects: &Objects) -> Result<String> {
    let nodes = objects.list("v1", "Node", None, None).await?;
    let [node] = nodes.as_slice() else {
        return infrastructure("the acceptance cluster must have exactly one node");
    };
    let info = &node["status"]["nodeInfo"];
    Ok(format!(
        "{}/{}",
        info["operatingSystem"].as_str().unwrap_or_default(),
        info["architecture"].as_str().unwrap_or_default()
    ))
}

/// Pull `reference`, which must be pinned by digest, into the node for
/// `platform` and require the node's CRI to resolve it.
///
/// # Errors
/// Fails when the reference is not digest-pinned, the pull fails or the CRI
/// does not resolve the reference within [`CRI_SYNC`] afterwards.
pub async fn pull(node: &str, reference: &str, platform: &str) -> Result {
    let pinned = reference.rsplit_once('@').map_or("", |(_, pinned)| pinned);
    ensure(
        crate::valid_digest(pinned),
        format!("{reference} is not pinned by a SHA-256 digest"),
    )?;
    let canonical = format!("{}@{pinned}", images::repository(reference)?);
    engine::current()?
        .command()
        .args(["exec", node, "ctr", "-n", "k8s.io", "images", "pull"])
        .args(["--platform", platform, &canonical])
        .timeout(Duration::from_secs(600))
        .run()
        .await?;
    // `ctr` writes containerd's image store directly; the CRI plugin learns
    // of the image from containerd's event stream, so it can lag the pull.
    crate::wait::until(
        &format!("the node to resolve {reference} after pulling it"),
        CRI_SYNC,
        async || {
            let inspected = engine::current()?
                .command()
                .args(["exec", node, "crictl", "inspecti", "-o", "json", reference])
                .output()
                .await?;
            if inspected.success {
                Ok(Some(()))
            } else {
                infrastructure(format!("crictl: {}", inspected.stderr.trim()))
            }
        },
    )
    .await
}

/// How long the CRI plugin may take to see an image `ctr` just pulled.
const CRI_SYNC: Duration = Duration::from_secs(60);

/// Import a release's controller image archive and require the node to
/// resolve the release's image reference to it.
///
/// # Errors
/// Fails when the archive does not hold the release's image, the platform
/// differs, or the import does not resolve.
pub async fn import_release(
    node: &str,
    candidate: &crate::pair::Candidate,
    platform: &str,
) -> Result {
    let image = &candidate.manifest.image;
    ensure(
        image.platform == platform,
        "candidate image platform differs from the acceptance node",
    )?;
    let archive = candidate.root.join("controller.oci.tar");
    let inspected = images::inspect(&archive, platform)?;
    let pinned = image.reference.rsplit_once('@').map(|(_, pinned)| pinned);
    ensure(
        pinned == Some(inspected.manifest.as_str()),
        "the release's controller archive does not hold its pinned image",
    )?;
    let repository = images::repository(&image.reference)?;
    images::import(node, &archive, &repository, platform).await?;
    images::require_resolves(
        node,
        &format!("{repository}@{}", inspected.manifest),
        &inspected.config,
    )
    .await?;
    Ok(())
}

/// The child manifest digests of the image index `index` as the node stores
/// it, after checking the stored bytes hash to `index`.
///
/// # Errors
/// Fails when the node lacks the content or it does not match its digest.
pub async fn index_children(node: &str, index: &str) -> Result<Vec<String>> {
    let raw = engine::current()?
        .command()
        .args(["exec", node, "ctr", "-n", "k8s.io", "content", "get", index])
        .run()
        .await?;
    ensure(
        digest(raw.as_bytes()) == index,
        "containerd image content differs from the signed successor",
    )?;
    let manifest: Value = serde_json::from_str(&raw)?;
    Ok(manifest["manifests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["digest"].as_str().map(str::to_owned))
        .collect())
}
