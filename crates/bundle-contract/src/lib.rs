#![forbid(unsafe_code)]

pub mod generate;
pub mod model;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use oxideloop_renderer_abi::{canonical_bytes, parse_strict_value, sha256_hex};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub use model::{
    BUNDLE_MANIFEST_VERSION_V0, BundleDescriptorV0, BundleInstallRecordV0,
    BundleIntegrityManifestV0, DESCRIPTOR_VERSION_V0, DescriptorModuleV0, ExecutionPhaseV0,
    FileIntegrityV0, ModuleIntegrityV0, PhaseModuleV0, ToolIdentityV0,
};

const INSTALL_RECORD_NAME: &str = "install-record.json";
pub const MAX_DECLARED_FILES: usize = 64;
pub const MAX_PATH_DEPTH: usize = 32;
pub const MAX_DISCOVERED_NODES: usize = MAX_DECLARED_FILES * (MAX_PATH_DEPTH + 1) + 1;
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_BUNDLE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INSTALL_RECORD_BYTES: usize = 256 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_CANONICAL_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_DESCRIPTOR_MODULES: usize = 32;
const MAX_PATH_BYTES: usize = 512;
static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Runtime evaluators must consume [`VerifiedArtifact::bytes`] and mount the
/// digest-addressed store read-only. The installer seals files and directories
/// before publication, but the mount is the deployment-time enforcement layer.
pub const RUNTIME_READ_ONLY_MOUNT_REQUIREMENT: &str =
    "mount the digest-addressed bundle store read-only for renderer processes";

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum BundleError {
    #[error("bundle manifest JSON is invalid")]
    InvalidJson,
    #[error("bundle manifest shape is invalid")]
    InvalidManifest,
    #[error("bundle manifest contract is invalid")]
    InvalidContract,
    #[error("bundle path is invalid")]
    InvalidPath,
    #[error("bundle contains duplicate declarations")]
    DuplicateDeclaration,
    #[error("bundle file set does not match the manifest")]
    FileSetMismatch,
    #[error("bundle file length does not match the manifest")]
    LengthMismatch,
    #[error("bundle file digest does not match the manifest")]
    DigestMismatch,
    #[error("bundle file is a symbolic link")]
    Symlink,
    #[error("bundle module is missing")]
    MissingModule,
    #[error("bundle installation already exists with a different record")]
    ExistingInstallMismatch,
    #[error("bundle I/O failed")]
    Io,
    #[error("bundle exceeds a fixed resource bound")]
    Limit,
}

impl From<io::Error> for BundleError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedManifestV0 {
    manifest: BundleIntegrityManifestV0,
    canonical_bytes: Vec<u8>,
    bundle_digest: String,
}

impl ValidatedManifestV0 {
    #[must_use]
    pub fn manifest(&self) -> &BundleIntegrityManifestV0 {
        &self.manifest
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
    #[must_use]
    pub fn bundle_digest(&self) -> &str {
        &self.bundle_digest
    }
}

/// Parse and validate a manifest using the shared strict JSON and canonical rules.
///
/// # Errors
///
/// Returns [`BundleError`] when the JSON, schema, or integrity contract fails.
pub fn parse_validate_manifest(input: &str) -> Result<ValidatedManifestV0, BundleError> {
    if input.len() > MAX_MANIFEST_BYTES {
        return Err(BundleError::Limit);
    }
    let value = parse_strict_value(input).map_err(|_| BundleError::InvalidJson)?;
    let manifest = serde_json::from_value::<BundleIntegrityManifestV0>(value.clone())
        .map_err(|_| BundleError::InvalidManifest)?;
    validate_manifest(&manifest)?;
    let canonical_bytes = canonical_manifest_bytes(&manifest)?;
    let bundle_digest = sha256_hex(&canonical_bytes);
    Ok(ValidatedManifestV0 {
        manifest,
        canonical_bytes,
        bundle_digest,
    })
}

/// # Errors
/// Returns [`BundleError`] when the descriptor is malformed or disagrees with the manifest.
pub fn parse_validate_descriptor(
    input: &str,
    manifest: &BundleIntegrityManifestV0,
) -> Result<BundleDescriptorV0, BundleError> {
    if input.len() > MAX_MANIFEST_BYTES {
        return Err(BundleError::Limit);
    }
    let value = parse_strict_value(input).map_err(|_| BundleError::InvalidJson)?;
    let descriptor = serde_json::from_value::<BundleDescriptorV0>(value)
        .map_err(|_| BundleError::InvalidManifest)?;
    validate_descriptor(&descriptor, manifest)?;
    Ok(descriptor)
}

/// # Errors
/// Returns [`BundleError`] when any descriptor field or route is invalid.
pub fn validate_descriptor(
    descriptor: &BundleDescriptorV0,
    manifest: &BundleIntegrityManifestV0,
) -> Result<(), BundleError> {
    if descriptor.descriptor_version != DESCRIPTOR_VERSION_V0
        || descriptor.renderer_abi_version != manifest.renderer_abi_version
        || descriptor.renderer_abi_version != oxideloop_renderer_abi::model::ABI_VERSION_V0
        || descriptor.action_modules.is_empty()
        || descriptor.action_modules.len() > MAX_DESCRIPTOR_MODULES
    {
        return Err(BundleError::InvalidContract);
    }
    let valid = |module: &DescriptorModuleV0, phase: ExecutionPhaseV0| {
        module.identity.len() <= MAX_IDENTITY_BYTES
            && !blank(&module.identity)
            && valid_digest(&module.digest)
            && manifest
                .phase_modules
                .iter()
                .any(|m| m.phase == phase && m.module_identity == module.identity)
            && match phase {
                ExecutionPhaseV0::Render => manifest
                    .render_modules
                    .iter()
                    .any(|m| m.identity == module.identity && m.file.sha256 == module.digest),
                ExecutionPhaseV0::Action => manifest
                    .action_modules
                    .iter()
                    .any(|m| m.identity == module.identity && m.file.sha256 == module.digest),
                ExecutionPhaseV0::Descriptor => false,
            }
    };
    if !valid(&descriptor.render_module, ExecutionPhaseV0::Render) {
        return Err(BundleError::InvalidContract);
    }
    let mut identities = HashSet::new();
    identities.insert(descriptor.render_module.identity.as_str());
    for module in &descriptor.action_modules {
        if !identities.insert(module.identity.as_str()) || !valid(module, ExecutionPhaseV0::Action)
        {
            return Err(BundleError::InvalidContract);
        }
    }
    Ok(())
}

/// Validate a typed manifest before it is serialized or installed.
///
/// # Errors
///
/// Returns [`BundleError`] for missing identities, invalid paths, duplicate
/// declarations, digest format failures, or invalid phase mappings.
pub fn validate_manifest(manifest: &BundleIntegrityManifestV0) -> Result<(), BundleError> {
    if manifest.manifest_version != BUNDLE_MANIFEST_VERSION_V0
        || manifest.renderer_abi_version != oxideloop_renderer_abi::model::ABI_VERSION_V0
        || blank(&manifest.compiler.identity)
        || blank(&manifest.compiler.version)
        || blank(&manifest.bundler.identity)
        || blank(&manifest.bundler.version)
    {
        return Err(BundleError::InvalidContract);
    }
    for value in [
        &manifest.manifest_version,
        &manifest.compiler.identity,
        &manifest.compiler.version,
        &manifest.bundler.identity,
        &manifest.bundler.version,
        &manifest.renderer_abi_version,
    ] {
        if value.len() > MAX_IDENTITY_BYTES {
            return Err(BundleError::Limit);
        }
    }

    let declared_count = 1
        + manifest.render_modules.len()
        + manifest.action_modules.len()
        + manifest.source_maps.len();
    if declared_count > MAX_DECLARED_FILES {
        return Err(BundleError::Limit);
    }
    let mut paths = HashSet::new();
    validate_file(&manifest.descriptor, &mut paths)?;
    let mut identities = HashSet::new();
    for module in manifest
        .render_modules
        .iter()
        .chain(&manifest.action_modules)
    {
        if module.identity.len() > MAX_IDENTITY_BYTES {
            return Err(BundleError::Limit);
        }
        if blank(&module.identity) || !identities.insert(module.identity.as_str()) {
            return Err(BundleError::DuplicateDeclaration);
        }
        validate_file(&module.file, &mut paths)?;
    }
    for source_map in &manifest.source_maps {
        validate_file(source_map, &mut paths)?;
    }
    for path in &paths {
        let prefix = format!("{path}/");
        if paths.iter().any(|candidate| candidate.starts_with(&prefix)) {
            return Err(BundleError::InvalidPath);
        }
    }
    let total = declared_files(manifest)
        .try_fold(0_u64, |total, file| total.checked_add(file.length))
        .ok_or(BundleError::Limit)?;
    if total > MAX_BUNDLE_BYTES {
        return Err(BundleError::Limit);
    }
    if manifest.render_modules.is_empty() {
        return Err(BundleError::MissingModule);
    }
    let render_modules: HashSet<_> = manifest
        .render_modules
        .iter()
        .map(|module| module.identity.as_str())
        .collect();
    let action_modules: HashSet<_> = manifest
        .action_modules
        .iter()
        .map(|module| module.identity.as_str())
        .collect();
    let mut mapped = HashSet::new();
    for phase in &manifest.phase_modules {
        if phase.module_identity.len() > MAX_IDENTITY_BYTES {
            return Err(BundleError::Limit);
        }
        let valid_type = match phase.phase {
            ExecutionPhaseV0::Render => render_modules.contains(phase.module_identity.as_str()),
            ExecutionPhaseV0::Action => action_modules.contains(phase.module_identity.as_str()),
            ExecutionPhaseV0::Descriptor => false,
        };
        if !valid_type || !mapped.insert((phase.phase, phase.module_identity.as_str())) {
            return Err(BundleError::InvalidContract);
        }
    }
    if mapped.len() != render_modules.len() + action_modules.len()
        || manifest.render_modules.is_empty()
    {
        return Err(BundleError::MissingModule);
    }
    if declared_nodes(manifest).len() > MAX_DISCOVERED_NODES {
        return Err(BundleError::Limit);
    }
    canonical_manifest_bytes(manifest)?;
    Ok(())
}

fn validate_file(file: &FileIntegrityV0, paths: &mut HashSet<String>) -> Result<(), BundleError> {
    if !valid_relative_path(&file.path) {
        return Err(BundleError::InvalidPath);
    }
    if file.path == INSTALL_RECORD_NAME || file.path.starts_with(&format!("{INSTALL_RECORD_NAME}/"))
    {
        return Err(BundleError::InvalidPath);
    }
    if file.path.len() > MAX_PATH_BYTES {
        return Err(BundleError::Limit);
    }
    if file.path.split('/').count() > MAX_PATH_DEPTH {
        return Err(BundleError::Limit);
    }
    if !paths.insert(file.path.clone()) {
        return Err(BundleError::DuplicateDeclaration);
    }
    if !valid_digest(&file.sha256) {
        return Err(BundleError::InvalidContract);
    }
    if file.length > MAX_FILE_BYTES {
        return Err(BundleError::Limit);
    }
    Ok(())
}

fn blank(value: &str) -> bool {
    value.trim().is_empty()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !value.contains('\0')
        && !value.contains('\\')
        && !value.contains("//")
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !path.is_absolute()
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Digest-addressed bundle storage. Installation copies verified bytes into a
/// temporary sibling directory before the final atomic rename.
#[derive(Debug)]
pub struct BundleStore {
    root: Dir,
}

impl BundleStore {
    /// Open a caller-selected ambient directory as the store capability.
    ///
    /// The caller is responsible for selecting a trusted, stable root. Every
    /// component below this root is subsequently resolved capability-relative
    /// with no symlink following.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the root cannot be created or opened.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, BundleError> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: open_ambient_directory(root.as_ref())?,
        })
    }

    /// Construct a store from authority the caller has already opened.
    #[must_use]
    pub fn from_open_directory(root: Dir) -> Self {
        Self { root }
    }

    /// Reopen a published bundle through this store capability and rescan its
    /// complete installed file set before returning any executable bytes.
    ///
    /// The digest is both the caller's address and the integrity root of the
    /// install record.  This deliberately does not trust a previous
    /// [`InstalledBundle`] handle: an evaluator process must independently
    /// verify the record, manifest, directory shape, and every declared file.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the digest is malformed, the installation
    /// is absent, or any part of the published directory no longer matches its
    /// canonical install record.
    pub fn open_installed(&self, digest: &str) -> Result<InstalledBundle, BundleError> {
        if !valid_digest(digest) {
            return Err(BundleError::InvalidContract);
        }
        let directory = self
            .root
            .open_dir_nofollow(digest)
            .map_err(|error| map_open_error(&error))?;
        let record = load_record(&directory)?;
        if record.bundle_digest != digest {
            return Err(BundleError::DigestMismatch);
        }
        validate_directory_exact(&directory, &record.manifest, true)?;
        Ok(InstalledBundle::new(directory, record))
    }

    /// Install one directory after validating its complete, exact file set.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the source differs from the manifest or an
    /// atomic installation cannot be completed.
    pub fn install_directory(
        &self,
        source: &Path,
        manifest: &ValidatedManifestV0,
    ) -> Result<InstalledBundle, BundleError> {
        validate_validated_manifest(manifest)?;
        let source = open_ambient_directory(source)?;
        validate_directory_exact(&source, manifest.manifest(), false)?;
        self.publish_verified(manifest, |temporary| {
            for file in declared_files(manifest.manifest()) {
                copy_verified(&source, temporary, file)?;
            }
            Ok(())
        })
    }

    /// Install an exact set of verified in-memory bundle files.
    ///
    /// Every supplied path must be a manifest-declared, normalized relative
    /// path and every declared file must appear exactly once. Lengths and
    /// SHA-256 values are checked before an existing installation is reused or
    /// any store directory is created. Publication is capability-relative to
    /// this store's already-open root and follows the same atomic path as
    /// [`Self::install_directory`].
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when the manifest, supplied set, or bytes fail
    /// validation, or when the atomic installation cannot be completed.
    pub fn install_bytes<'a, I>(
        &self,
        manifest: &ValidatedManifestV0,
        files: I,
    ) -> Result<InstalledBundle, BundleError>
    where
        I: IntoIterator<Item = (&'a str, &'a [u8])>,
    {
        validate_validated_manifest(manifest)?;
        let files = validate_exact_bytes(manifest.manifest(), files)?;
        self.publish_verified(manifest, |temporary| {
            for file in declared_files(manifest.manifest()) {
                let bytes = files
                    .get(file.path.as_str())
                    .copied()
                    .ok_or(BundleError::FileSetMismatch)?;
                write_new_file(temporary, &file.path, bytes)?;
            }
            Ok(())
        })
    }

    fn publish_verified<F>(
        &self,
        manifest: &ValidatedManifestV0,
        populate: F,
    ) -> Result<InstalledBundle, BundleError>
    where
        F: FnOnce(&Dir) -> Result<(), BundleError>,
    {
        let store = self.root.try_clone()?;
        let record = BundleInstallRecordV0 {
            bundle_digest: manifest.bundle_digest().to_owned(),
            manifest: manifest.manifest().clone(),
        };
        let record_bytes = canonical_install_record_bytes(&record)?;
        if let Ok(destination) = store.open_dir_nofollow(manifest.bundle_digest()) {
            let existing = load_record(&destination)?;
            if existing != record {
                return Err(BundleError::ExistingInstallMismatch);
            }
            validate_directory_exact(&destination, &record.manifest, true)?;
            return Ok(InstalledBundle::new(destination, record));
        }
        let temporary_name = create_temporary_directory(&store, manifest.bundle_digest())?;
        let temporary = store.open_dir_nofollow(&temporary_name)?;
        let outcome = (|| {
            populate(&temporary)?;
            write_new_file(&temporary, INSTALL_RECORD_NAME, &record_bytes)?;
            seal_directory(&temporary)?;
            sync_directory(&temporary)?;
            let destination = if store
                .rename(&temporary_name, &store, manifest.bundle_digest())
                .is_ok()
            {
                sync_directory(&store)?;
                store.open_dir_nofollow(manifest.bundle_digest())?
            } else {
                let existing = store.open_dir_nofollow(manifest.bundle_digest())?;
                if load_record(&existing)? != record {
                    return Err(BundleError::ExistingInstallMismatch);
                }
                validate_directory_exact(&existing, &record.manifest, true)?;
                existing
            };
            Ok(InstalledBundle::new(destination, record))
        })();
        if store.open_dir_nofollow(&temporary_name).is_ok() {
            if let Ok(temporary) = store.open_dir_nofollow(&temporary_name) {
                let _ = unseal_directory(&temporary);
            }
            let _ = store.remove_dir_all(&temporary_name);
        }
        outcome
    }
}

fn validate_validated_manifest(manifest: &ValidatedManifestV0) -> Result<(), BundleError> {
    validate_manifest(manifest.manifest())?;
    let canonical = canonical_manifest_bytes(manifest.manifest())?;
    if canonical != manifest.canonical_bytes || sha256_hex(&canonical) != manifest.bundle_digest {
        return Err(BundleError::InvalidContract);
    }
    Ok(())
}

fn validate_exact_bytes<'a, I>(
    manifest: &BundleIntegrityManifestV0,
    files: I,
) -> Result<BTreeMap<&'a str, &'a [u8]>, BundleError>
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let declared: BTreeMap<&str, &FileIntegrityV0> = declared_files(manifest)
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut supplied = BTreeMap::new();
    let mut count = 0_usize;
    for (path, bytes) in files {
        count = count.checked_add(1).ok_or(BundleError::Limit)?;
        if count > MAX_DECLARED_FILES {
            return Err(BundleError::Limit);
        }
        if !valid_relative_path(path)
            || path == INSTALL_RECORD_NAME
            || path.starts_with(&format!("{INSTALL_RECORD_NAME}/"))
        {
            return Err(BundleError::InvalidPath);
        }
        let expected = declared.get(path).ok_or(BundleError::FileSetMismatch)?;
        if supplied.insert(path, bytes).is_some() {
            return Err(BundleError::DuplicateDeclaration);
        }
        if u64::try_from(bytes.len()).map_err(|_| BundleError::Limit)? != expected.length {
            return Err(BundleError::LengthMismatch);
        }
        if sha256_hex(bytes) != expected.sha256 {
            return Err(BundleError::DigestMismatch);
        }
    }
    if supplied.len() != declared.len() {
        return Err(BundleError::FileSetMismatch);
    }
    Ok(supplied)
}

fn declared_files(manifest: &BundleIntegrityManifestV0) -> impl Iterator<Item = &FileIntegrityV0> {
    std::iter::once(&manifest.descriptor)
        .chain(manifest.render_modules.iter().map(|module| &module.file))
        .chain(manifest.action_modules.iter().map(|module| &module.file))
        .chain(manifest.source_maps.iter())
}

fn open_ambient_directory(path: &Path) -> Result<Dir, BundleError> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(BundleError::Symlink);
    }
    // The caller-supplied root is the capability boundary. Every subsequent
    // source, destination, and record component is opened relative to it with
    // no-follow resolution, avoiding racy ambient path reopening.
    Dir::open_ambient_dir(path, ambient_authority()).map_err(Into::into)
}

fn create_temporary_directory(store: &Dir, digest: &str) -> Result<String, BundleError> {
    for _ in 0..16 {
        let number = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{digest}.installing.{}.{}", std::process::id(), number);
        match store.create_dir(&name) {
            Ok(()) => return Ok(name),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(BundleError::Io),
        }
    }
    Err(BundleError::Io)
}

fn validate_directory_exact(
    directory: &Dir,
    manifest: &BundleIntegrityManifestV0,
    installed: bool,
) -> Result<(), BundleError> {
    let mut found = BTreeSet::new();
    collect_files(directory, "", 0, &mut found)?;
    let mut expected = declared_nodes(manifest);
    if installed {
        expected.insert(INSTALL_RECORD_NAME.into());
    }
    if found != expected {
        return Err(BundleError::FileSetMismatch);
    }
    for file in declared_files(manifest) {
        let _ = read_verified_bytes(directory, file)?;
    }
    Ok(())
}

fn declared_nodes(manifest: &BundleIntegrityManifestV0) -> BTreeSet<String> {
    let mut nodes = BTreeSet::new();
    for file in declared_files(manifest) {
        nodes.insert(file.path.clone());
        let mut parent = Path::new(&file.path).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            let text = path.to_str().expect("validated manifest paths are UTF-8");
            nodes.insert(format!("{text}/"));
            parent = path.parent();
        }
    }
    nodes
}

fn collect_files(
    directory: &Dir,
    prefix: &str,
    depth: usize,
    found: &mut BTreeSet<String>,
) -> Result<(), BundleError> {
    if depth > MAX_PATH_DEPTH {
        return Err(BundleError::Limit);
    }
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| BundleError::InvalidPath)?;
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(BundleError::Symlink);
        }
        if found.len() >= MAX_DISCOVERED_NODES {
            return Err(BundleError::Limit);
        }
        if file_type.is_dir() {
            if depth >= MAX_PATH_DEPTH {
                return Err(BundleError::Limit);
            }
            if !found.insert(format!("{relative}/")) {
                return Err(BundleError::FileSetMismatch);
            }
            let child = directory
                .open_dir_nofollow(&name)
                .map_err(|error| map_open_error(&error))?;
            collect_files(&child, &relative, depth + 1, found)?;
        } else if file_type.is_file() {
            if !found.insert(relative) {
                return Err(BundleError::FileSetMismatch);
            }
        } else {
            return Err(BundleError::FileSetMismatch);
        }
    }
    Ok(())
}

fn open_file_nofollow(directory: &Dir, relative: &str) -> Result<cap_std::fs::File, BundleError> {
    if !valid_relative_path(relative) {
        return Err(BundleError::InvalidPath);
    }
    let mut components = relative.split('/').peekable();
    let mut parent = directory.try_clone()?;
    while let Some(component) = components.next() {
        if components.peek().is_some() {
            parent = parent
                .open_dir_nofollow(component)
                .map_err(|error| map_open_error(&error))?;
        } else {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            return parent
                .open_with(component, &options)
                .map_err(|error| map_open_error(&error));
        }
    }
    Err(BundleError::InvalidPath)
}

fn map_open_error(error: &io::Error) -> BundleError {
    if matches!(
        error.kind(),
        io::ErrorKind::TooManyLinks | io::ErrorKind::NotADirectory
    ) {
        BundleError::Symlink
    } else {
        BundleError::Io
    }
}

fn read_verified_bytes(
    directory: &Dir,
    expected: &FileIntegrityV0,
) -> Result<Vec<u8>, BundleError> {
    let file = open_file_nofollow(directory, &expected.path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(BundleError::FileSetMismatch);
    }
    if metadata.len() != expected.length {
        return Err(BundleError::LengthMismatch);
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(expected.length).map_err(|_| BundleError::Limit)?);
    let mut hasher = Sha256::new();
    let mut limited = file.take(expected.length.saturating_add(1));
    let mut buffer = [0_u8; 8192];
    loop {
        let count = limited.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        hasher.update(&buffer[..count]);
    }
    if u64::try_from(bytes.len()).map_err(|_| BundleError::Limit)? != expected.length {
        return Err(BundleError::LengthMismatch);
    }
    if format!("{:x}", hasher.finalize()) != expected.sha256 {
        return Err(BundleError::DigestMismatch);
    }
    Ok(bytes)
}

fn ensure_parent_directories(directory: &Dir, relative: &str) -> Result<Dir, BundleError> {
    let parent = Path::new(relative).parent().unwrap_or(Path::new(""));
    let mut current = directory.try_clone()?;
    for component in parent.components() {
        let Component::Normal(component) = component else {
            return Err(BundleError::InvalidPath);
        };
        let name = component.to_str().ok_or(BundleError::InvalidPath)?;
        if let Err(error) = current.create_dir(name)
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(BundleError::Io);
        }
        current = current
            .open_dir_nofollow(name)
            .map_err(|error| map_open_error(&error))?;
    }
    Ok(current)
}

fn write_new_file(directory: &Dir, relative: &str, bytes: &[u8]) -> Result<(), BundleError> {
    let leaf = Path::new(relative)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(BundleError::InvalidPath)?;
    let parent = ensure_parent_directories(directory, relative)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut output = parent
        .open_with(leaf, &options)
        .map_err(|error| map_open_error(&error))?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

fn copy_verified(
    source: &Dir,
    destination: &Dir,
    expected: &FileIntegrityV0,
) -> Result<(), BundleError> {
    let bytes = read_verified_bytes(source, expected)?;
    write_new_file(destination, &expected.path, &bytes)
}

fn load_record(directory: &Dir) -> Result<BundleInstallRecordV0, BundleError> {
    let file = open_file_nofollow(directory, INSTALL_RECORD_NAME)?;
    let metadata = file.metadata()?;
    if metadata.len() > u64::try_from(MAX_INSTALL_RECORD_BYTES).map_err(|_| BundleError::Limit)? {
        return Err(BundleError::Limit);
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| BundleError::Limit)?
        .min(MAX_INSTALL_RECORD_BYTES);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(u64::try_from(MAX_INSTALL_RECORD_BYTES + 1).map_err(|_| BundleError::Limit)?)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INSTALL_RECORD_BYTES {
        return Err(BundleError::Limit);
    }
    let value: Value = oxideloop_renderer_abi::parse_strict_value(
        std::str::from_utf8(&bytes).map_err(|_| BundleError::InvalidManifest)?,
    )
    .map_err(|_| BundleError::InvalidManifest)?;
    let canonical = canonical_bytes(&value).map_err(|_| BundleError::InvalidManifest)?;
    if bytes != canonical {
        return Err(BundleError::InvalidManifest);
    }
    let record: BundleInstallRecordV0 =
        serde_json::from_value(value).map_err(|_| BundleError::InvalidManifest)?;
    validate_manifest(&record.manifest)?;
    if canonical_install_record_bytes(&record)? != bytes {
        return Err(BundleError::InvalidManifest);
    }
    let canonical = canonical_manifest_bytes(&record.manifest)?;
    if sha256_hex(&canonical) != record.bundle_digest {
        return Err(BundleError::DigestMismatch);
    }
    Ok(record)
}

fn canonical_manifest_bytes(manifest: &BundleIntegrityManifestV0) -> Result<Vec<u8>, BundleError> {
    let value = serde_json::to_value(manifest).map_err(|_| BundleError::InvalidManifest)?;
    let bytes = canonical_bytes(&value).map_err(|_| BundleError::InvalidManifest)?;
    if bytes.len() > MAX_CANONICAL_MANIFEST_BYTES {
        return Err(BundleError::Limit);
    }
    Ok(bytes)
}

fn canonical_install_record_bytes(record: &BundleInstallRecordV0) -> Result<Vec<u8>, BundleError> {
    let value = serde_json::to_value(record).map_err(|_| BundleError::InvalidManifest)?;
    let bytes = canonical_bytes(&value).map_err(|_| BundleError::InvalidManifest)?;
    if bytes.len() > MAX_INSTALL_RECORD_BYTES {
        return Err(BundleError::Limit);
    }
    Ok(bytes)
}

fn seal_directory(directory: &Dir) -> Result<(), BundleError> {
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(BundleError::Symlink);
        }
        if file_type.is_dir() {
            seal_directory(
                &directory
                    .open_dir_nofollow(&name)
                    .map_err(|error| map_open_error(&error))?,
            )?;
        }
        let mut permissions = directory.symlink_metadata(&name)?.permissions();
        permissions.set_readonly(true);
        directory.set_permissions(&name, permissions)?;
    }
    let mut permissions = directory.dir_metadata()?.permissions();
    permissions.set_readonly(true);
    directory.set_permissions(".", permissions)?;
    Ok(())
}

fn unseal_directory(directory: &Dir) -> Result<(), BundleError> {
    let mut root_permissions = directory.dir_metadata()?.permissions();
    make_writable(&mut root_permissions, true);
    directory.set_permissions(".", root_permissions)?;
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        let mut permissions = directory.symlink_metadata(&name)?.permissions();
        make_writable(&mut permissions, file_type.is_dir());
        directory.set_permissions(&name, permissions)?;
        if file_type.is_dir() {
            unseal_directory(
                &directory
                    .open_dir_nofollow(&name)
                    .map_err(|error| map_open_error(&error))?,
            )?;
        }
    }
    Ok(())
}

fn make_writable(permissions: &mut cap_std::fs::Permissions, directory: bool) {
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        permissions.set_mode(if directory { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
}

fn sync_directory(directory: &Dir) -> Result<(), BundleError> {
    directory
        .try_clone()?
        .into_std_file()
        .sync_all()
        .map_err(Into::into)
}

#[derive(Debug)]
pub struct InstalledBundle {
    directory: Dir,
    record: BundleInstallRecordV0,
}

impl InstalledBundle {
    fn new(directory: Dir, record: BundleInstallRecordV0) -> Self {
        Self { directory, record }
    }
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.record.bundle_digest
    }
    #[must_use]
    pub fn manifest(&self) -> &BundleIntegrityManifestV0 {
        &self.record.manifest
    }

    /// # Errors
    /// Returns [`BundleError`] when the installed descriptor fails integrity or contract checks.
    pub fn descriptor(&self) -> Result<BundleDescriptorV0, BundleError> {
        let artifact = self.open_declared(&self.record.manifest.descriptor.path)?;
        let text = std::str::from_utf8(artifact.bytes()).map_err(|_| BundleError::InvalidJson)?;
        parse_validate_descriptor(text, &self.record.manifest)
    }

    /// Read the exact verified bytes that a renderer must consume.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] if the declaration is absent or its stored bytes
    /// no longer match the immutable manifest. The returned bytes are an
    /// independent immutable snapshot, so later filesystem mutation cannot
    /// affect evaluator input.
    pub fn open_declared(&self, path: &str) -> Result<VerifiedArtifact, BundleError> {
        let expected = declared_files(&self.record.manifest)
            .find(|file| file.path == path)
            .ok_or(BundleError::FileSetMismatch)?;
        let bytes = read_verified_bytes(&self.directory, expected)?;
        Ok(VerifiedArtifact {
            bytes,
            length: expected.length,
            sha256: expected.sha256.clone(),
        })
    }

    /// Open a declared module only through its exact phase mapping.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::MissingModule`] for unknown identities and all
    /// verification errors from [`Self::open_declared`].
    pub fn open_phase_module(
        &self,
        phase: ExecutionPhaseV0,
        identity: &str,
    ) -> Result<VerifiedArtifact, BundleError> {
        if phase == ExecutionPhaseV0::Descriptor
            || !self
                .record
                .manifest
                .phase_modules
                .iter()
                .any(|mapping| mapping.phase == phase && mapping.module_identity == identity)
        {
            return Err(BundleError::MissingModule);
        }
        let modules = match phase {
            ExecutionPhaseV0::Render => &self.record.manifest.render_modules,
            ExecutionPhaseV0::Action => &self.record.manifest.action_modules,
            ExecutionPhaseV0::Descriptor => return Err(BundleError::MissingModule),
        };
        let module = modules
            .iter()
            .find(|module| module.identity == identity)
            .ok_or(BundleError::MissingModule)?;
        self.open_declared(&module.file.path)
    }
}

#[derive(Debug)]
pub struct VerifiedArtifact {
    bytes: Vec<u8>,
    length: u64,
    sha256: String,
}

impl VerifiedArtifact {
    #[must_use]
    pub fn length(&self) -> u64 {
        self.length
    }
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
