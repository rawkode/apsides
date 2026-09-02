use std::{env, fs, path::Path, process::Command, sync::Arc, thread};

use oxideloop_bundle_archive::{ArchiveError, install_archive, parse_validate_archive};
use oxideloop_bundle_contract::{
    BUNDLE_MANIFEST_VERSION_V0, BundleInstallRecordV0, BundleIntegrityManifestV0, BundleStore,
    ExecutionPhaseV0, FileIntegrityV0, ModuleIntegrityV0, PhaseModuleV0, ToolIdentityV0,
};
use oxideloop_renderer_abi::{canonical_bytes, sha256_hex};
use tempfile::TempDir;

const PATHS: [&str; 5] = [
    "action.callable.js",
    "bundle-install.json",
    "descriptor.json",
    "integrity-manifest.json",
    "render.callable.js",
];

fn file(path: &str, bytes: &[u8]) -> FileIntegrityV0 {
    FileIntegrityV0 {
        path: path.into(),
        length: bytes.len() as u64,
        sha256: sha256_hex(bytes),
    }
}

fn canonical(value: &impl serde::Serialize) -> Vec<u8> {
    canonical_bytes(&serde_json::to_value(value).unwrap()).unwrap()
}

fn archive() -> (Vec<u8>, String) {
    archive_with_action(b"action".to_vec())
}

fn archive_with_action(action: Vec<u8>) -> (Vec<u8>, String) {
    let descriptor = b"descriptor".to_vec();
    let render = b"render".to_vec();
    let manifest = BundleIntegrityManifestV0 {
        manifest_version: BUNDLE_MANIFEST_VERSION_V0.into(),
        compiler: ToolIdentityV0 {
            identity: "tsx".into(),
            version: "1".into(),
        },
        bundler: ToolIdentityV0 {
            identity: "rolldown".into(),
            version: "1".into(),
        },
        renderer_abi_version: "renderer.oxideloop.dev/v0".into(),
        descriptor: file("descriptor.json", &descriptor),
        render_modules: vec![ModuleIntegrityV0 {
            identity: "controller.render".into(),
            file: file("render.callable.js", &render),
        }],
        action_modules: vec![ModuleIntegrityV0 {
            identity: "configuration-changed".into(),
            file: file("action.callable.js", &action),
        }],
        source_maps: vec![],
        phase_modules: vec![
            PhaseModuleV0 {
                phase: ExecutionPhaseV0::Render,
                module_identity: "controller.render".into(),
            },
            PhaseModuleV0 {
                phase: ExecutionPhaseV0::Action,
                module_identity: "configuration-changed".into(),
            },
        ],
    };
    let manifest_bytes = canonical(&manifest);
    let digest = sha256_hex(&manifest_bytes);
    let install_bytes = canonical(&BundleInstallRecordV0 {
        bundle_digest: digest.clone(),
        manifest,
    });
    let entries = [
        (PATHS[0], action),
        (PATHS[1], install_bytes),
        (PATHS[2], descriptor),
        (PATHS[3], manifest_bytes),
        (PATHS[4], render),
    ];
    let mut result = b"OLBA\x01\x05".to_vec();
    for (path, bytes) in &entries {
        result.push(u8::try_from(path.len()).expect("test path fits envelope framing"));
        result.extend_from_slice(path.as_bytes());
        result.extend_from_slice(
            &u32::try_from(bytes.len())
                .expect("test payload fits envelope framing")
                .to_be_bytes(),
        );
        result.extend_from_slice(sha256_hex(bytes).as_bytes());
    }
    for (_, bytes) in entries {
        result.extend_from_slice(&bytes);
    }
    (result, digest)
}

fn archive_at_limit() -> (Vec<u8>, String) {
    let mut action_length = 128 * 1024;
    for _ in 0..8 {
        let result = archive_with_action(vec![b'a'; action_length]);
        if result.0.len() == 128 * 1024 {
            return result;
        }
        action_length = action_length
            .checked_sub_signed(isize::try_from(result.0.len()).unwrap() - 128 * 1024)
            .unwrap();
    }
    panic!("test archive must converge on the encoded limit");
}

#[test]
fn validates_and_installs_then_reinstalls_after_store_wipe() {
    let (bytes, digest) = archive();
    let parsed = parse_validate_archive(&bytes, &digest).unwrap();
    assert_eq!(parsed.manifest().bundle_digest(), digest);
    let root = TempDir::new().unwrap();
    let store = BundleStore::new(root.path()).unwrap();
    let installed = install_archive(&store, &bytes, &digest).unwrap();
    assert_eq!(
        installed.open_declared("descriptor.json").unwrap().bytes(),
        b"descriptor"
    );
    let published = root.path().join(&digest);
    make_writable(&published, 0o700);
    fs::remove_dir_all(&published).unwrap();
    assert_eq!(
        install_archive(&store, &bytes, &digest).unwrap().digest(),
        digest
    );
}

#[test]
fn archive_install_uses_only_the_store_capability_for_staging() {
    const CHILD: &str = "OXIDELOOP_ARCHIVE_AMBIENT_TEMP_CHILD";
    const STORE: &str = "OXIDELOOP_ARCHIVE_AMBIENT_TEMP_STORE";
    const TEMP_FIXTURE: &str = "OXIDELOOP_ARCHIVE_AMBIENT_TEMP_FIXTURE";
    const SENTINEL: &str = "direct-bytes-store-only";
    match env::var(CHILD) {
        Ok(value) if value == SENTINEL => {
            let temporary = env::var("TMPDIR").expect("child receives TMPDIR fixture");
            assert_eq!(
                temporary,
                env::var(TEMP_FIXTURE).expect("child receives exact TMPDIR fixture path")
            );
            assert!(
                Path::new(&temporary).is_file(),
                "TMPDIR fixture must be a regular file so ambient TempDir creation fails even as root"
            );
            let (bytes, digest) = archive();
            let root = env::var(STORE).expect("child receives writable store path");
            let store = BundleStore::new(root).expect("open child store");
            assert_eq!(
                install_archive(&store, &bytes, &digest).unwrap().digest(),
                digest
            );
            return;
        }
        Ok(value) => panic!("unexpected archive child sentinel: {value}"),
        Err(env::VarError::NotPresent) => {}
        Err(error) => panic!("invalid archive child sentinel: {error}"),
    }

    let root = TempDir::new().unwrap();
    let temporary = root.path().join("not-a-directory");
    fs::write(&temporary, b"ambient temporary directory fixture").unwrap();
    let status = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "archive_install_uses_only_the_store_capability_for_staging",
        ])
        .env(CHILD, SENTINEL)
        .env(STORE, root.path())
        .env(TEMP_FIXTURE, &temporary)
        .env("TMPDIR", &temporary)
        .status()
        .expect("start isolated archive-install child");
    assert!(status.success());
}

#[test]
fn concurrent_same_digest_install_rechecks_bundle_store() {
    let (bytes, digest) = archive();
    let root = TempDir::new().unwrap();
    let store = Arc::new(BundleStore::new(root.path()).unwrap());
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let store = Arc::clone(&store);
            let bytes = bytes.clone();
            let digest = digest.clone();
            thread::spawn(move || {
                install_archive(&store, &bytes, &digest).map(|bundle| bundle.digest().to_owned())
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap().unwrap(), digest);
    }
}

#[test]
fn rejects_transport_framing_and_metadata_tampering_without_installation() {
    let (bytes, digest) = archive();
    let root = TempDir::new().unwrap();
    let store = BundleStore::new(root.path()).unwrap();
    let mut cases = Vec::new();
    cases.push(Vec::new()); // truncated magic
    let mut wrong_version = bytes.clone();
    wrong_version[4] = 2;
    cases.push(wrong_version);
    let mut wrong_count = bytes.clone();
    wrong_count[5] = 4;
    cases.push(wrong_count);
    let mut reordered = bytes.clone();
    reordered[7..7 + PATHS[0].len()].copy_from_slice(PATHS[4].as_bytes());
    let final_path = header_path_offset(&bytes, 4);
    reordered[final_path..final_path + PATHS[4].len()].copy_from_slice(PATHS[0].as_bytes());
    cases.push(reordered);
    let mut wrong_endian = bytes.clone();
    let first_length = 6 + 1 + PATHS[0].len();
    wrong_endian[first_length..first_length + 4].reverse();
    cases.push(wrong_endian);
    let mut unknown_path = bytes.clone();
    unknown_path[7] = b'z';
    cases.push(unknown_path);
    let mut header_hash = bytes.clone();
    let hash_start = 6 + 1 + PATHS[0].len() + 4;
    header_hash[hash_start] = b'g';
    cases.push(header_hash);
    let mut payload_hash = bytes.clone();
    *payload_hash.last_mut().unwrap() ^= 1;
    cases.push(payload_hash);
    let mut trailing = bytes.clone();
    trailing.push(0);
    cases.push(trailing);
    let mut over_limit = bytes.clone();
    over_limit.resize(128 * 1024 + 1, 0);
    cases.push(over_limit);
    for length in [4, 5, 6, 7, bytes.len() - 1] {
        cases.push(bytes[..length].to_vec());
    }
    for invalid in cases {
        assert!(parse_validate_archive(&invalid, &digest).is_err());
        assert!(install_archive(&store, &invalid, &digest).is_err());
    }
    assert!(
        fs::read_dir(root.path()).unwrap().next().is_none(),
        "failed validation must not publish or leak staging into the store"
    );
}

#[test]
fn rejects_wrong_expected_digest_and_noncanonical_metadata() {
    let (bytes, digest) = archive();
    assert_eq!(
        parse_validate_archive(&bytes, "0").unwrap_err(),
        ArchiveError::DigestMismatch
    );
    let mut noncanonical = bytes.clone();
    let manifest_payload = payload_offset(&bytes, 3);
    let replacement = b"configuration-changer";
    let original = b"configuration-changed";
    let position = noncanonical[manifest_payload..]
        .windows(original.len())
        .position(|window| window == original)
        .unwrap();
    noncanonical[manifest_payload + position..manifest_payload + position + original.len()]
        .copy_from_slice(replacement);
    rehash_entry(&mut noncanonical, 3);
    assert!(parse_validate_archive(&noncanonical, &digest).is_err());
}

#[test]
fn rejects_self_consistent_swapped_render_and_action_roles() {
    let (bytes, _) = archive();
    let mut entries = decode_entries(&bytes);
    let mut manifest: BundleIntegrityManifestV0 = serde_json::from_slice(&entries[3].1).unwrap();
    manifest.render_modules[0].file.path = "action.callable.js".into();
    manifest.action_modules[0].file.path = "render.callable.js".into();
    let manifest_bytes = canonical(&manifest);
    let digest = sha256_hex(&manifest_bytes);
    entries[1].1 = canonical(&BundleInstallRecordV0 {
        bundle_digest: digest.clone(),
        manifest,
    });
    entries[3].1 = manifest_bytes;
    assert_eq!(
        parse_validate_archive(&encode_entries(&entries), &digest),
        Err(ArchiveError::InvalidMetadata)
    );
}

#[test]
fn rejects_self_consistent_phase_role_swaps() {
    let (bytes, _) = archive();
    let mut entries = decode_entries(&bytes);
    let mut manifest: BundleIntegrityManifestV0 = serde_json::from_slice(&entries[3].1).unwrap();
    manifest.phase_modules.swap(0, 1);
    let manifest_bytes = canonical(&manifest);
    let digest = sha256_hex(&manifest_bytes);
    entries[1].1 = canonical(&BundleInstallRecordV0 {
        bundle_digest: digest.clone(),
        manifest,
    });
    entries[3].1 = manifest_bytes;
    assert_eq!(
        parse_validate_archive(&encode_entries(&entries), &digest),
        Err(ArchiveError::InvalidMetadata)
    );
}

#[test]
fn rejects_phase_module_identity_that_does_not_match_its_fixed_payload_role() {
    let (bytes, _) = archive();
    let mut entries = decode_entries(&bytes);
    let mut manifest: BundleIntegrityManifestV0 = serde_json::from_slice(&entries[3].1).unwrap();
    manifest.phase_modules[1].module_identity = "controller.render".into();
    let manifest_bytes = canonical(&manifest);
    let digest = sha256_hex(&manifest_bytes);
    entries[1].1 = canonical(&BundleInstallRecordV0 {
        bundle_digest: digest.clone(),
        manifest,
    });
    entries[3].1 = manifest_bytes;
    assert_eq!(
        parse_validate_archive(&encode_entries(&entries), &digest),
        Err(ArchiveError::InvalidMetadata)
    );
}

#[test]
fn rejects_unknown_manifest_metadata_fields() {
    let (bytes, _) = archive();
    let mut entries = decode_entries(&bytes);
    let mut manifest: serde_json::Value = serde_json::from_slice(&entries[3].1).unwrap();
    manifest["unknown"] = serde_json::json!(true);
    let manifest_bytes = canonical(&manifest);
    let digest = sha256_hex(&manifest_bytes);
    let mut record: serde_json::Value = serde_json::from_slice(&entries[1].1).unwrap();
    record["bundleDigest"] = serde_json::json!(digest.clone());
    record["manifest"] = manifest;
    entries[1].1 = canonical(&record);
    entries[3].1 = manifest_bytes;
    assert_eq!(
        parse_validate_archive(&encode_entries(&entries), &digest),
        Err(ArchiveError::InvalidMetadata)
    );
}

#[test]
fn rejects_unknown_install_record_metadata_fields() {
    let (bytes, digest) = archive();
    let mut entries = decode_entries(&bytes);
    let mut record: serde_json::Value = serde_json::from_slice(&entries[1].1).unwrap();
    record["unknown"] = serde_json::json!(true);
    entries[1].1 = canonical(&record);
    assert_eq!(
        parse_validate_archive(&encode_entries(&entries), &digest),
        Err(ArchiveError::InvalidMetadata)
    );
}

#[test]
fn accepts_an_archive_at_the_total_limit_and_rejects_one_byte_over() {
    let (at_limit, digest) = archive_at_limit();
    assert_eq!(at_limit.len(), 128 * 1024);
    assert!(parse_validate_archive(&at_limit, &digest).is_ok());
    let mut over = at_limit;
    over.push(0);
    assert_eq!(
        parse_validate_archive(&over, &digest),
        Err(ArchiveError::Limit)
    );
}

fn payload_offset(bytes: &[u8], entry: usize) -> usize {
    let mut offset = 6;
    let mut payload_start = 6;
    let mut lengths = Vec::new();
    for _ in 0..5 {
        let path_length = usize::from(bytes[offset]);
        let length_start = offset + 1 + path_length;
        lengths.push(
            u32::from_be_bytes(bytes[length_start..length_start + 4].try_into().unwrap()) as usize,
        );
        offset = length_start + 4 + 64;
        payload_start = offset;
    }
    payload_start + lengths[..entry].iter().sum::<usize>()
}

fn header_path_offset(bytes: &[u8], entry: usize) -> usize {
    let mut offset = 6;
    for _ in 0..entry {
        offset += 1 + usize::from(bytes[offset]) + 4 + 64;
    }
    offset + 1
}

fn rehash_entry(bytes: &mut [u8], entry: usize) {
    let mut offset = 6;
    let mut digest_offset = 0;
    let mut length = 0;
    for index in 0..5 {
        let path_length = usize::from(bytes[offset]);
        let length_start = offset + 1 + path_length;
        length =
            u32::from_be_bytes(bytes[length_start..length_start + 4].try_into().unwrap()) as usize;
        digest_offset = length_start + 4;
        if index == entry {
            break;
        }
        offset = digest_offset + 64;
    }
    let payload = payload_offset(bytes, entry);
    let digest = sha256_hex(&bytes[payload..payload + length]);
    bytes[digest_offset..digest_offset + 64].copy_from_slice(digest.as_bytes());
}

fn decode_entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut header = 6;
    let mut entries = Vec::new();
    for _ in 0..5 {
        let path_length = usize::from(bytes[header]);
        let path = std::str::from_utf8(&bytes[header + 1..header + 1 + path_length])
            .unwrap()
            .to_owned();
        let length_start = header + 1 + path_length;
        let length =
            u32::from_be_bytes(bytes[length_start..length_start + 4].try_into().unwrap()) as usize;
        entries.push((path, length));
        header = length_start + 4 + 64;
    }
    let mut payload = header;
    entries
        .into_iter()
        .map(|(path, length)| {
            let result = (path, bytes[payload..payload + length].to_vec());
            payload += length;
            result
        })
        .collect()
}

fn encode_entries(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut result = b"OLBA\x01\x05".to_vec();
    for (path, bytes) in entries {
        result.push(u8::try_from(path.len()).unwrap());
        result.extend_from_slice(path.as_bytes());
        result.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
        result.extend_from_slice(sha256_hex(bytes).as_bytes());
    }
    for (_, bytes) in entries {
        result.extend_from_slice(bytes);
    }
    result
}

fn make_writable(path: &std::path::Path, mode: u32) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(mode);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).unwrap();
}
