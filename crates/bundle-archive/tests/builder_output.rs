use std::{env, fs};

use oxideloop_bundle_archive::install_archive;
use oxideloop_bundle_contract::{BundleStore, ExecutionPhaseV0};
use oxideloop_renderer_abi::sha256_hex;
use tempfile::TempDir;

#[test]
#[ignore = "the TypeScript builder test supplies the required archive fixture"]
fn typescript_archive_installs_and_opens_by_its_raw_manifest_digest() {
    let archive = fs::read(
        env::var("OXIDELOOP_BUILT_ARCHIVE")
            .expect("bundle-builder test must provide OXIDELOOP_BUILT_ARCHIVE"),
    )
    .expect("read TypeScript archive");
    let expected = env::var("OXIDELOOP_EXPECTED_BUNDLE_DIGEST")
        .expect("bundle-builder test must provide OXIDELOOP_EXPECTED_BUNDLE_DIGEST");
    let root = TempDir::new().expect("create store");
    let store = BundleStore::new(root.path()).expect("open store");
    let installed =
        install_archive(&store, &archive, &expected).expect("install TypeScript archive");
    assert_eq!(installed.digest(), expected);
    let render = installed
        .open_phase_module(ExecutionPhaseV0::Render, "demo.web-application.render")
        .expect("open render callable");
    assert_eq!(
        sha256_hex(render.bytes()),
        installed.manifest().render_modules[0].file.sha256
    );
}
