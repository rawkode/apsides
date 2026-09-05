use std::fs;

use bundle_contract::{
    BUNDLE_MANIFEST_VERSION_V0, BundleError, BundleInstallRecordV0, BundleIntegrityManifestV0,
    BundleStore, ExecutionPhaseV0, FileIntegrityV0, MAX_FILE_BYTES, MAX_MANIFEST_BYTES,
    ModuleIntegrityV0, PhaseModuleV0, ToolIdentityV0, parse_validate_manifest,
};
use renderer_abi::{canonical_bytes, sha256_hex};
use tempfile::TempDir;

fn digest(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}
fn file(path: &str, contents: &[u8]) -> FileIntegrityV0 {
    FileIntegrityV0 {
        path: path.into(),
        length: contents.len() as u64,
        sha256: digest(contents),
    }
}
fn manifest() -> BundleIntegrityManifestV0 {
    let descriptor = file("descriptor.json", b"descriptor");
    let render = ModuleIntegrityV0 {
        identity: "render.main".into(),
        file: file("render.js", b"render"),
    };
    let action = ModuleIntegrityV0 {
        identity: "action.smoke".into(),
        file: file("action.js", b"action"),
    };
    BundleIntegrityManifestV0 {
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
        descriptor,
        render_modules: vec![render],
        action_modules: vec![action],
        source_maps: vec![file("render.js.map", b"map")],
        phase_modules: vec![
            PhaseModuleV0 {
                phase: ExecutionPhaseV0::Render,
                module_identity: "render.main".into(),
            },
            PhaseModuleV0 {
                phase: ExecutionPhaseV0::Action,
                module_identity: "action.smoke".into(),
            },
        ],
    }
}
fn write_bundle(directory: &TempDir, manifest: &BundleIntegrityManifestV0) {
    for (path, contents) in [
        ("descriptor.json", b"descriptor".as_slice()),
        ("render.js", b"render"),
        ("action.js", b"action"),
        ("render.js.map", b"map"),
    ] {
        fs::write(directory.path().join(path), contents).unwrap();
    }
    assert_eq!(manifest.render_modules[0].file.path, "render.js");
}

fn bundle_bytes() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("descriptor.json", b"descriptor"),
        ("render.js", b"render"),
        ("action.js", b"action"),
        ("render.js.map", b"map"),
    ]
}

fn store_entries(path: &std::path::Path) -> Vec<String> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}
fn validated(manifest: &BundleIntegrityManifestV0) -> bundle_contract::ValidatedManifestV0 {
    parse_validate_manifest(&serde_json::to_string(manifest).unwrap()).unwrap()
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

#[test]
fn descriptor_is_strict_and_cross_validated() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let mut manifest = manifest();
    let render = &manifest.render_modules[0];
    let action = &manifest.action_modules[0];
    let descriptor = format!(
        r#"{{"descriptorVersion":"oxideloop.dev/descriptor/v0","rendererAbiVersion":"renderer.oxideloop.dev/v0","renderModule":{{"identity":"{}","digest":"{}"}},"actionModules":[{{"identity":"{}","digest":"{}"}}]}}"#,
        render.identity, render.file.sha256, action.identity, action.file.sha256
    );
    manifest.descriptor = file("descriptor.json", descriptor.as_bytes());
    fs::write(source.path().join("descriptor.json"), descriptor).unwrap();
    fs::write(source.path().join("render.js"), b"render").unwrap();
    fs::write(source.path().join("action.js"), b"action").unwrap();
    fs::write(source.path().join("render.js.map"), b"map").unwrap();
    let validated = validated(&manifest);
    let store = BundleStore::new(storage.path()).unwrap();
    let installed = store.install_directory(source.path(), &validated).unwrap();
    assert_eq!(installed.descriptor().unwrap().action_modules.len(), 1);
    let unknown = r#"{"descriptorVersion":"oxideloop.dev/descriptor/v0","rendererAbiVersion":"renderer.oxideloop.dev/v0","renderModule":{"identity":"render.main","digest":"0000000000000000000000000000000000000000000000000000000000000000"},"actionModules":[],"extra":1}"#;
    assert!(bundle_contract::parse_validate_descriptor(unknown, &manifest).is_err());
}

#[test]
fn install_uses_canonical_digest_and_verified_handles() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    let validated = validated(&manifest);
    let store = BundleStore::new(storage.path()).unwrap();
    let installed = store.install_directory(source.path(), &validated).unwrap();
    assert_eq!(installed.digest(), validated.bundle_digest());
    let artifact = installed
        .open_phase_module(ExecutionPhaseV0::Render, "render.main")
        .unwrap();
    assert_eq!(artifact.bytes(), b"render");
    assert_eq!(
        installed
            .open_phase_module(ExecutionPhaseV0::Action, "render.main")
            .unwrap_err(),
        BundleError::MissingModule
    );
    assert_eq!(
        installed
            .open_phase_module(ExecutionPhaseV0::Render, "action.smoke")
            .unwrap_err(),
        BundleError::MissingModule
    );
    let path = storage.path().join(installed.digest()).join("render.js");
    make_writable(&path, 0o600);
    fs::write(&path, b"other!").unwrap();
    assert_eq!(artifact.bytes(), b"render");
    assert_eq!(
        installed
            .open_phase_module(ExecutionPhaseV0::Render, "render.main")
            .unwrap_err(),
        BundleError::DigestMismatch
    );
}

#[test]
fn direct_byte_install_opens_and_reuses_the_identical_bundle() {
    let storage = TempDir::new().unwrap();
    let validated = validated(&manifest());
    let store = BundleStore::new(storage.path()).unwrap();

    let installed = store.install_bytes(&validated, bundle_bytes()).unwrap();
    assert_eq!(installed.digest(), validated.bundle_digest());
    assert_eq!(
        installed
            .open_phase_module(ExecutionPhaseV0::Render, "render.main")
            .unwrap()
            .bytes(),
        b"render"
    );
    assert_eq!(
        store
            .install_bytes(&validated, bundle_bytes())
            .unwrap()
            .digest(),
        validated.bundle_digest()
    );
    assert_eq!(
        store_entries(storage.path()),
        vec![validated.bundle_digest()]
    );
}

#[test]
fn direct_byte_install_creates_nested_declared_paths_without_ambient_staging() {
    let storage = TempDir::new().unwrap();
    let mut input = manifest();
    input.descriptor.path = "metadata/descriptor.json".into();
    input.render_modules[0].file.path = "modules/render.js".into();
    input.action_modules[0].file.path = "modules/action.js".into();
    input.source_maps[0].path = "maps/render.js.map".into();
    let validated = validated(&input);
    let store = BundleStore::new(storage.path()).unwrap();

    let installed = store
        .install_bytes(
            &validated,
            [
                ("metadata/descriptor.json", b"descriptor".as_slice()),
                ("modules/render.js", b"render".as_slice()),
                ("modules/action.js", b"action".as_slice()),
                ("maps/render.js.map", b"map".as_slice()),
            ],
        )
        .unwrap();
    assert_eq!(
        installed
            .open_declared("metadata/descriptor.json")
            .unwrap()
            .bytes(),
        b"descriptor"
    );
    assert_eq!(
        installed
            .open_phase_module(ExecutionPhaseV0::Action, "action.smoke")
            .unwrap()
            .bytes(),
        b"action"
    );
}

#[test]
fn direct_byte_install_rejects_nonexact_or_unverified_sets_without_residue() {
    let storage = TempDir::new().unwrap();
    let validated = validated(&manifest());
    let store = BundleStore::new(storage.path()).unwrap();
    let missing = vec![
        ("descriptor.json", b"descriptor".as_slice()),
        ("render.js", b"render".as_slice()),
        ("action.js", b"action".as_slice()),
    ];
    let extra = vec![
        ("descriptor.json", b"descriptor".as_slice()),
        ("render.js", b"render".as_slice()),
        ("action.js", b"action".as_slice()),
        ("render.js.map", b"map".as_slice()),
        ("extra.js", b"extra".as_slice()),
    ];
    let duplicate = vec![
        ("descriptor.json", b"descriptor".as_slice()),
        ("descriptor.json", b"descriptor".as_slice()),
    ];
    let confused_path = vec![
        ("nested/../descriptor.json", b"descriptor".as_slice()),
        ("render.js", b"render".as_slice()),
        ("action.js", b"action".as_slice()),
        ("render.js.map", b"map".as_slice()),
    ];
    let wrong_length = vec![
        ("descriptor.json", b"short".as_slice()),
        ("render.js", b"render".as_slice()),
        ("action.js", b"action".as_slice()),
        ("render.js.map", b"map".as_slice()),
    ];
    let wrong_digest = vec![
        ("descriptor.json", b"XXXXXXXXXX".as_slice()),
        ("render.js", b"render".as_slice()),
        ("action.js", b"action".as_slice()),
        ("render.js.map", b"map".as_slice()),
    ];

    for (files, expected) in [
        (missing, BundleError::FileSetMismatch),
        (extra, BundleError::FileSetMismatch),
        (duplicate, BundleError::DuplicateDeclaration),
        (confused_path, BundleError::InvalidPath),
        (wrong_length, BundleError::LengthMismatch),
        (wrong_digest, BundleError::DigestMismatch),
    ] {
        assert_eq!(
            store.install_bytes(&validated, files).unwrap_err(),
            expected
        );
        assert!(
            store_entries(storage.path()).is_empty(),
            "ordinary validation failures must not publish or retain temporary directories"
        );
    }
}

#[test]
fn direct_byte_install_bounds_the_input_iterator_before_publishing() {
    let storage = TempDir::new().unwrap();
    let mut input = manifest();
    input.source_maps = (0..61)
        .map(|index| file(&format!("source-{index}.map"), b""))
        .collect();
    let validated = validated(&input);
    let store = BundleStore::new(storage.path()).unwrap();

    let mut files = vec![
        ("descriptor.json".to_owned(), b"descriptor".to_vec()),
        ("render.js".to_owned(), b"render".to_vec()),
        ("action.js".to_owned(), b"action".to_vec()),
    ];
    files.extend((0..61).map(|index| (format!("source-{index}.map"), Vec::new())));
    files.push(("not-reached.map".to_owned(), Vec::new()));

    assert_eq!(
        store
            .install_bytes(
                &validated,
                files
                    .iter()
                    .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
            )
            .unwrap_err(),
        BundleError::Limit
    );
    assert!(store_entries(storage.path()).is_empty());
}

#[test]
fn direct_byte_install_validates_new_input_before_reusing_an_existing_bundle() {
    let storage = TempDir::new().unwrap();
    let validated = validated(&manifest());
    let store = BundleStore::new(storage.path()).unwrap();
    store.install_bytes(&validated, bundle_bytes()).unwrap();

    let malformed = vec![
        ("descriptor.json", b"descriptor".as_slice()),
        ("render.js", b"render".as_slice()),
        ("action.js", b"action".as_slice()),
    ];
    assert_eq!(
        store.install_bytes(&validated, malformed).unwrap_err(),
        BundleError::FileSetMismatch
    );
    assert_eq!(
        store_entries(storage.path()),
        vec![validated.bundle_digest()]
    );
}

#[test]
fn direct_byte_install_rejects_a_different_existing_record() {
    let storage = TempDir::new().unwrap();
    let expected_manifest = manifest();
    let expected = validated(&expected_manifest);
    let store = BundleStore::new(storage.path()).unwrap();
    store.install_bytes(&expected, bundle_bytes()).unwrap();

    let mut different_manifest = expected_manifest;
    different_manifest.compiler.version = "different".into();
    let different = validated(&different_manifest);
    let record = BundleInstallRecordV0 {
        bundle_digest: different.bundle_digest().to_owned(),
        manifest: different_manifest,
    };
    let record_bytes = canonical_bytes(&serde_json::to_value(record).unwrap()).unwrap();
    let directory = storage.path().join(expected.bundle_digest());
    let record_path = directory.join("install-record.json");
    make_writable(&directory, 0o700);
    make_writable(&record_path, 0o600);
    fs::write(record_path, record_bytes).unwrap();

    assert_eq!(
        store.install_bytes(&expected, bundle_bytes()).unwrap_err(),
        BundleError::ExistingInstallMismatch
    );
}

#[test]
fn reopening_an_install_rescans_the_record_and_exact_file_set() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    let validated = validated(&manifest);
    let store = BundleStore::new(storage.path()).unwrap();
    let installed = store.install_directory(source.path(), &validated).unwrap();

    let reopened = store.open_installed(installed.digest()).unwrap();
    assert_eq!(reopened.digest(), installed.digest());
    assert_eq!(
        reopened
            .open_phase_module(ExecutionPhaseV0::Render, "render.main")
            .unwrap()
            .bytes(),
        b"render"
    );

    let directory = storage.path().join(installed.digest());
    make_writable(&directory, 0o700);
    fs::write(directory.join("unexpected"), b"no").unwrap();
    assert_eq!(
        store.open_installed(installed.digest()).unwrap_err(),
        BundleError::FileSetMismatch
    );
    assert_eq!(
        store.open_installed(&"not-a-digest".repeat(8)).unwrap_err(),
        BundleError::InvalidContract
    );
}

#[test]
fn existing_install_is_revalidated_before_reuse() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    let validated = validated(&manifest);
    let store = BundleStore::new(storage.path()).unwrap();
    let installed = store.install_directory(source.path(), &validated).unwrap();
    let directory = storage.path().join(installed.digest());
    make_writable(&directory, 0o700);
    fs::write(directory.join("extra"), b"extra").unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated)
            .unwrap_err(),
        BundleError::FileSetMismatch
    );
}

#[test]
fn existing_install_record_is_bounded_by_actual_bytes_read() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    let validated = validated(&manifest);
    let store = BundleStore::new(storage.path()).unwrap();
    let installed = store.install_directory(source.path(), &validated).unwrap();
    let directory = storage.path().join(installed.digest());
    let record = directory.join("install-record.json");
    make_writable(&directory, 0o700);
    make_writable(&record, 0o600);
    fs::write(&record, vec![b' '; MAX_MANIFEST_BYTES + 1]).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated)
            .unwrap_err(),
        BundleError::Limit
    );
}

#[test]
fn installation_rejects_tampering_stale_and_undeclared_files() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    fs::write(source.path().join("descriptor.json"), b"stale!!!!").unwrap();
    let store = BundleStore::new(storage.path()).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::LengthMismatch
    );
    fs::write(source.path().join("descriptor.json"), b"descriptor").unwrap();
    fs::write(source.path().join("extra"), b"no").unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::FileSetMismatch
    );
    fs::remove_file(source.path().join("extra")).unwrap();
    fs::create_dir(source.path().join("extra-dir")).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::FileSetMismatch
    );
}

#[test]
fn installation_rejects_action_and_source_map_mismatches() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    fs::write(source.path().join("action.js"), b"wrong!").unwrap();
    let store = BundleStore::new(storage.path()).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::DigestMismatch
    );
    fs::write(source.path().join("action.js"), b"action").unwrap();
    fs::remove_file(source.path().join("render.js.map")).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::FileSetMismatch
    );
}

#[test]
fn manifest_rejects_paths_duplicates_digests_and_missing_modules() {
    let mut input = manifest();
    input.descriptor.path = "../escape".into();
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::InvalidPath)
    );
    let mut input = manifest();
    input.source_maps.push(input.descriptor.clone());
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::DuplicateDeclaration)
    );
    let mut input = manifest();
    input.action_modules[0].file.sha256 = "0".repeat(63);
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::InvalidContract)
    );
    let mut input = manifest();
    input.render_modules.clear();
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::MissingModule)
    );
    let mut input = manifest();
    input.phase_modules[1].phase = ExecutionPhaseV0::Render;
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::InvalidContract)
    );
    let mut input = manifest();
    input.phase_modules.push(input.phase_modules[0].clone());
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::InvalidContract)
    );
    let mut input = manifest();
    input.action_modules[0].file.length = MAX_FILE_BYTES + 1;
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::Limit)
    );
    let mut input = manifest();
    input.compiler.identity = "x".repeat(257);
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::Limit)
    );
    let mut raw = serde_json::to_string(&manifest()).unwrap();
    raw.push_str(&" ".repeat(MAX_MANIFEST_BYTES));
    assert_eq!(parse_validate_manifest(&raw), Err(BundleError::Limit));
    let mut input = manifest();
    input.descriptor.path = "foo/./descriptor.json".into();
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::InvalidPath)
    );
    let mut input = manifest();
    input.descriptor.path = format!("{}descriptor.json", "a/".repeat(32));
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::Limit)
    );
    let mut input = manifest();
    input.descriptor.path = "install-record.json".into();
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::InvalidPath)
    );
    let mut input = manifest();
    input.descriptor.path = "foo".into();
    input.render_modules[0].file.path = "foo/render.js".into();
    assert_eq!(
        bundle_contract::validate_manifest(&input),
        Err(BundleError::InvalidPath)
    );
}

#[test]
fn hostile_source_depth_is_rejected_before_unbounded_descent() {
    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    let mut nested = source.path().to_path_buf();
    for _ in 0..33 {
        nested.push("a");
        fs::create_dir(&nested).unwrap();
    }
    let store = BundleStore::new(storage.path()).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::Limit
    );
}

#[cfg(unix)]
#[test]
fn installed_bundle_keeps_its_original_directory_capability() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    let store = BundleStore::new(storage.path()).unwrap();
    let installed = store
        .install_directory(source.path(), &validated(&manifest))
        .unwrap();
    let published = storage.path().join(installed.digest());
    let detached = storage.path().join("detached-original");
    fs::rename(&published, &detached).unwrap();
    let attacker = TempDir::new().unwrap();
    fs::write(attacker.path().join("render.js"), b"attack").unwrap();
    symlink(attacker.path(), &published).unwrap();

    let artifact = installed
        .open_phase_module(ExecutionPhaseV0::Render, "render.main")
        .unwrap();
    assert_eq!(artifact.bytes(), b"render");
}

#[cfg(unix)]
#[test]
fn installation_rejects_symbolic_links() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let manifest = manifest();
    write_bundle(&source, &manifest);
    fs::remove_file(source.path().join("render.js")).unwrap();
    symlink(
        source.path().join("action.js"),
        source.path().join("render.js"),
    )
    .unwrap();
    let store = BundleStore::new(storage.path()).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::Symlink
    );
}

#[cfg(unix)]
#[test]
fn installation_rejects_parent_symbolic_links() {
    use std::os::unix::fs::symlink;

    let source = TempDir::new().unwrap();
    let storage = TempDir::new().unwrap();
    let mut manifest = manifest();
    manifest.render_modules[0].file.path = "nested/render.js".into();
    fs::write(source.path().join("descriptor.json"), b"descriptor").unwrap();
    fs::write(source.path().join("action.js"), b"action").unwrap();
    fs::write(source.path().join("render.js.map"), b"map").unwrap();
    symlink(source.path(), source.path().join("nested")).unwrap();
    let store = BundleStore::new(storage.path()).unwrap();
    assert_eq!(
        store
            .install_directory(source.path(), &validated(&manifest))
            .unwrap_err(),
        BundleError::Symlink
    );
}
