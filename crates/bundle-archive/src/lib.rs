#![forbid(unsafe_code)]

//! A deliberately small, uncompressed transport format for an installable v0
//! bundle. The wire format is exactly:
//!
//! ```text
//! 4 bytes  magic "OLBA"
//! 1 byte   version (1)
//! 1 byte   entry count (5)
//! repeated entry count times, in bytewise lexicographic path order:
//!   1 byte   UTF-8 path byte length
//!   n bytes  path
//!   4 bytes  big-endian payload length
//!   64 bytes lowercase ASCII SHA-256 hex of the payload
//! contiguous payloads in the same order, followed by exact EOF
//! ```
//!
//! There is no compression, archive kind field, link metadata, or implicit
//! entries. This makes links, devices, FIFOs, and tar-like format confusion
//! unrepresentable at the wire boundary.

use bundle_contract::{
    BundleError, BundleInstallRecordV0, BundleStore, InstalledBundle, ValidatedManifestV0,
    parse_validate_manifest,
};
use renderer_abi::{canonical_bytes, parse_strict_value, sha256_hex};
use serde_json::Value;
use std::collections::BTreeSet;
use thiserror::Error;

const MAGIC: &[u8; 4] = b"OLBA";
const VERSION: u8 = 1;
const ENTRY_COUNT: usize = 5;
const MAX_ARCHIVE_BYTES: usize = 128 * 1024;
const MAX_ENTRY_BYTES: usize = 128 * 1024;
const MAX_PATH_DEPTH: usize = 2;
const EXPECTED_PATHS: [&str; ENTRY_COUNT] = [
    "action.callable.js",
    "bundle-install.json",
    "descriptor.json",
    "integrity-manifest.json",
    "render.callable.js",
];
const STAGED_PATHS: [&str; 3] = [
    "action.callable.js",
    "descriptor.json",
    "render.callable.js",
];

/// Archive validation failures are deliberately distinct from filesystem
/// installation failures, allowing callers to reject untrusted transport
/// bytes before opening a staging directory.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArchiveError {
    #[error("archive framing is invalid")]
    InvalidEnvelope,
    #[error("archive exceeds a fixed resource bound")]
    Limit,
    #[error("archive metadata is invalid")]
    InvalidMetadata,
    #[error("archive metadata does not match its digest")]
    DigestMismatch,
    #[error("archive installation failed")]
    Install,
}

impl From<BundleError> for ArchiveError {
    fn from(_: BundleError) -> Self {
        Self::Install
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArchiveEntry {
    path: &'static str,
    bytes: Vec<u8>,
}

/// A fully validated in-memory archive. Its private entry bytes are bounded by
/// the archive limits and have already been checked against both metadata
/// roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedArchive {
    manifest: ValidatedManifestV0,
    entries: Vec<ArchiveEntry>,
}

impl ValidatedArchive {
    #[must_use]
    pub fn manifest(&self) -> &ValidatedManifestV0 {
        &self.manifest
    }

    fn entry(&self, path: &str) -> Result<&[u8], ArchiveError> {
        self.entries
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.bytes.as_slice())
            .ok_or(ArchiveError::InvalidEnvelope)
    }
}

/// Parse an archive and validate all transport and bundle integrity roots.
/// `expected_digest` is the caller's digest-addressed authorization boundary.
/// No filesystem access occurs here.
///
/// # Errors
///
/// Returns [`ArchiveError`] for any non-canonical framing, metadata mismatch,
/// digest mismatch, truncation, trailing bytes, or resource-limit breach.
pub fn parse_validate_archive(
    bytes: &[u8],
    expected_digest: &str,
) -> Result<ValidatedArchive, ArchiveError> {
    let entries = parse_envelope(bytes)?;
    let manifest = validate_metadata(&entries, expected_digest)?;
    Ok(ValidatedArchive { manifest, entries })
}

fn parse_envelope(bytes: &[u8]) -> Result<Vec<ArchiveEntry>, ArchiveError> {
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(ArchiveError::Limit);
    }
    let mut cursor = 0;
    if take(bytes, &mut cursor, MAGIC.len())? != MAGIC {
        return Err(ArchiveError::InvalidEnvelope);
    }
    if take_byte(bytes, &mut cursor)? != VERSION
        || usize::from(take_byte(bytes, &mut cursor)?) != ENTRY_COUNT
    {
        return Err(ArchiveError::InvalidEnvelope);
    }

    let mut header = Vec::with_capacity(ENTRY_COUNT);
    for (index, expected_path) in EXPECTED_PATHS.iter().enumerate() {
        let path_length = usize::from(take_byte(bytes, &mut cursor)?);
        let path = std::str::from_utf8(take(bytes, &mut cursor, path_length)?)
            .map_err(|_| ArchiveError::InvalidEnvelope)?;
        if path != *expected_path || !valid_transport_path(path) {
            return Err(ArchiveError::InvalidEnvelope);
        }
        let length = usize::try_from(u32::from_be_bytes(
            take(bytes, &mut cursor, 4)?
                .try_into()
                .map_err(|_| ArchiveError::InvalidEnvelope)?,
        ))
        .map_err(|_| ArchiveError::Limit)?;
        if length > MAX_ENTRY_BYTES {
            return Err(ArchiveError::Limit);
        }
        let digest = take(bytes, &mut cursor, 64)?;
        if !valid_lower_hex_digest(digest) {
            return Err(ArchiveError::InvalidEnvelope);
        }
        // The path equality check above and fixed count make duplicate, missing,
        // unordered, unknown, and non-canonical headers impossible.
        header.push((EXPECTED_PATHS[index], length, digest));
    }

    let payload_total = header.iter().try_fold(0_usize, |sum, (_, length, _)| {
        sum.checked_add(*length).ok_or(ArchiveError::Limit)
    })?;
    if cursor
        .checked_add(payload_total)
        .as_ref()
        .is_none_or(|end| *end != bytes.len())
    {
        return Err(ArchiveError::InvalidEnvelope);
    }

    let mut entries = Vec::with_capacity(ENTRY_COUNT);
    for (path, length, digest) in header {
        let payload = take(bytes, &mut cursor, length)?;
        if sha256_hex(payload).as_bytes() != digest {
            return Err(ArchiveError::DigestMismatch);
        }
        entries.push(ArchiveEntry {
            path,
            bytes: payload.to_vec(),
        });
    }
    if cursor != bytes.len() {
        return Err(ArchiveError::InvalidEnvelope);
    }

    Ok(entries)
}

fn validate_metadata(
    entries: &[ArchiveEntry],
    expected_digest: &str,
) -> Result<ValidatedManifestV0, ArchiveError> {
    let manifest_raw = entry_bytes(entries, "integrity-manifest.json")?;
    let manifest = parse_canonical_manifest(manifest_raw)?;
    if manifest.bundle_digest() != expected_digest {
        return Err(ArchiveError::DigestMismatch);
    }
    let install_raw = entry_bytes(entries, "bundle-install.json")?;
    let install = parse_canonical_install_record(install_raw)?;
    if install.bundle_digest != manifest.bundle_digest() || install.manifest != *manifest.manifest()
    {
        return Err(ArchiveError::DigestMismatch);
    }
    if !has_exact_v0_roles(manifest.manifest()) {
        return Err(ArchiveError::InvalidMetadata);
    }

    let manifest_paths: BTreeSet<_> = std::iter::once(&manifest.manifest().descriptor.path)
        .chain(
            manifest
                .manifest()
                .render_modules
                .iter()
                .map(|module| &module.file.path),
        )
        .chain(
            manifest
                .manifest()
                .action_modules
                .iter()
                .map(|module| &module.file.path),
        )
        .chain(
            manifest
                .manifest()
                .source_maps
                .iter()
                .map(|file| &file.path),
        )
        .map(String::as_str)
        .collect();
    let staged_paths: BTreeSet<_> = STAGED_PATHS.into_iter().collect();
    if manifest_paths != staged_paths {
        return Err(ArchiveError::InvalidMetadata);
    }
    for file in std::iter::once(&manifest.manifest().descriptor)
        .chain(
            manifest
                .manifest()
                .render_modules
                .iter()
                .map(|module| &module.file),
        )
        .chain(
            manifest
                .manifest()
                .action_modules
                .iter()
                .map(|module| &module.file),
        )
    {
        let payload = entry_bytes(entries, &file.path)?;
        if u64::try_from(payload.len()).map_err(|_| ArchiveError::Limit)? != file.length
            || sha256_hex(payload) != file.sha256
        {
            return Err(ArchiveError::DigestMismatch);
        }
    }

    Ok(manifest)
}

fn has_exact_v0_roles(manifest: &bundle_contract::BundleIntegrityManifestV0) -> bool {
    manifest.descriptor.path == "descriptor.json"
        && matches!(manifest.render_modules.as_slice(), [module]
            if module.file.path == "render.callable.js")
        && matches!(manifest.action_modules.as_slice(), [module]
            if module.file.path == "action.callable.js")
        && manifest.source_maps.is_empty()
        && matches!(manifest.phase_modules.as_slice(), [
            render,
            action,
        ] if matches!(manifest.render_modules.as_slice(), [render_module]
                if render.phase == bundle_contract::ExecutionPhaseV0::Render
                    && render.module_identity == render_module.identity)
            && matches!(manifest.action_modules.as_slice(), [action_module]
                if action.phase == bundle_contract::ExecutionPhaseV0::Action
                    && action.module_identity == action_module.identity))
}

/// Validate and install an archive directly into the store capability.
/// Only manifest-declared runtime artifacts are supplied; transport metadata
/// is never passed to [`BundleStore::install_bytes`].
///
/// # Errors
///
/// Returns [`ArchiveError`] if parsing, validation, or the underlying atomic
/// `BundleStore` installation fails.
pub fn install_archive(
    store: &BundleStore,
    bytes: &[u8],
    expected_digest: &str,
) -> Result<InstalledBundle, ArchiveError> {
    let archive = parse_validate_archive(bytes, expected_digest)?;
    store
        .install_bytes(
            archive.manifest(),
            [
                (STAGED_PATHS[0], archive.entry(STAGED_PATHS[0])?),
                (STAGED_PATHS[1], archive.entry(STAGED_PATHS[1])?),
                (STAGED_PATHS[2], archive.entry(STAGED_PATHS[2])?),
            ],
        )
        .map_err(ArchiveError::from)
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, count: usize) -> Result<&'a [u8], ArchiveError> {
    let end = cursor.checked_add(count).ok_or(ArchiveError::Limit)?;
    let slice = bytes
        .get(*cursor..end)
        .ok_or(ArchiveError::InvalidEnvelope)?;
    *cursor = end;
    Ok(slice)
}

fn take_byte(bytes: &[u8], cursor: &mut usize) -> Result<u8, ArchiveError> {
    Ok(*take(bytes, cursor, 1)?
        .first()
        .ok_or(ArchiveError::InvalidEnvelope)?)
}

fn valid_lower_hex_digest(bytes: &[u8]) -> bool {
    bytes.len() == 64
        && bytes.iter().all(|byte| {
            byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
        })
}

fn valid_transport_path(path: &str) -> bool {
    !path.is_empty()
        && path.is_ascii()
        && !path.contains(['\\', '/'])
        && path != "."
        && path != ".."
        && path.split('/').count() <= MAX_PATH_DEPTH
}

fn entry_bytes<'a>(entries: &'a [ArchiveEntry], path: &str) -> Result<&'a [u8], ArchiveError> {
    entries
        .iter()
        .find(|entry| entry.path == path)
        .map(|entry| entry.bytes.as_slice())
        .ok_or(ArchiveError::InvalidEnvelope)
}

fn parse_canonical_manifest(bytes: &[u8]) -> Result<ValidatedManifestV0, ArchiveError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ArchiveError::InvalidMetadata)?;
    let value = parse_strict_value(text).map_err(|_| ArchiveError::InvalidMetadata)?;
    if canonical_bytes(&value).map_err(|_| ArchiveError::InvalidMetadata)? != bytes {
        return Err(ArchiveError::InvalidMetadata);
    }
    parse_validate_manifest(text).map_err(|_| ArchiveError::InvalidMetadata)
}

fn parse_canonical_install_record(bytes: &[u8]) -> Result<BundleInstallRecordV0, ArchiveError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ArchiveError::InvalidMetadata)?;
    let value: Value = parse_strict_value(text).map_err(|_| ArchiveError::InvalidMetadata)?;
    if canonical_bytes(&value).map_err(|_| ArchiveError::InvalidMetadata)? != bytes {
        return Err(ArchiveError::InvalidMetadata);
    }
    let record: BundleInstallRecordV0 =
        serde_json::from_value(value).map_err(|_| ArchiveError::InvalidMetadata)?;
    let manifest_value =
        serde_json::to_value(&record.manifest).map_err(|_| ArchiveError::InvalidMetadata)?;
    let manifest_bytes =
        canonical_bytes(&manifest_value).map_err(|_| ArchiveError::InvalidMetadata)?;
    if sha256_hex(&manifest_bytes) != record.bundle_digest {
        return Err(ArchiveError::DigestMismatch);
    }
    parse_validate_manifest(
        std::str::from_utf8(&manifest_bytes).map_err(|_| ArchiveError::InvalidMetadata)?,
    )
    .map_err(|_| ArchiveError::InvalidMetadata)?;
    Ok(record)
}
