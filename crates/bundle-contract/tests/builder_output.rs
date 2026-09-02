use std::{env, fs, path::Path};

use oxideloop_bundle_contract::{BundleStore, ExecutionPhaseV0, parse_validate_manifest};
use oxideloop_renderer_abi::sha256_hex;
use renderer_host::{Invocation, Limits, RendererHost};
use serde_json::Value;
use tempfile::TempDir;

#[test]
#[ignore = "the TypeScript builder test supplies the required built artifact fixture"]
#[allow(clippy::too_many_lines)]
fn builder_output_installs_and_opens_by_its_raw_manifest_digest() {
    let bundle_dir = env::var("OXIDELOOP_BUILT_BUNDLE_DIR")
        .expect("bundle-builder test must provide OXIDELOOP_BUILT_BUNDLE_DIR");
    let metadata_dir = env::var("OXIDELOOP_BUILT_METADATA_DIR")
        .expect("bundle-builder test must provide OXIDELOOP_BUILT_METADATA_DIR");
    let manifest_text = fs::read_to_string(format!("{metadata_dir}/integrity-manifest.json"))
        .expect("read builder manifest");
    let validated = parse_validate_manifest(&manifest_text).expect("validate builder manifest");
    assert_eq!(
        manifest_text.trim_end(),
        std::str::from_utf8(validated.canonical_bytes()).expect("canonical manifest UTF-8"),
        "builder canonicalization must match the Rust ABI exactly"
    );
    assert!(
        validated
            .bundle_digest()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );

    let install_record: Value = serde_json::from_slice(
        &fs::read(format!("{metadata_dir}/bundle-install.json")).expect("read install metadata"),
    )
    .expect("parse install metadata");
    assert_eq!(
        install_record["bundleDigest"].as_str(),
        Some(validated.bundle_digest()),
        "metadata must use the exact BundleStore address"
    );

    let store_root = TempDir::new().expect("create store directory");
    let store = BundleStore::new(store_root.path()).expect("open store");
    let installed = store
        .install_directory(Path::new(&bundle_dir), &validated)
        .expect("install exact builder file set");
    assert_eq!(installed.digest(), validated.bundle_digest());

    let render = installed
        .open_phase_module(ExecutionPhaseV0::Render, "controller.render")
        .expect("open builder render callable");
    let action = installed
        .open_phase_module(ExecutionPhaseV0::Action, "configuration-changed")
        .expect("open builder action callable");
    assert_eq!(
        sha256_hex(render.bytes()),
        validated.manifest().render_modules[0].file.sha256
    );
    assert_eq!(
        sha256_hex(action.bytes()),
        validated.manifest().action_modules[0].file.sha256
    );

    let bundle_digest = validated.bundle_digest();
    let render_input = serde_json::json!({
        "abiVersion": "renderer.oxideloop.dev/v0",
        "root": {"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","namespace":"demo","name":"portal","uid":"root-1","generation":1,"spec":{"expose":true}},
        "bundleDigest": bundle_digest,
        "cacheEpoch": "0",
        "observationToken": "snapshot-1",
        "observations": [{"apiVersion":"v1","kind":"ConfigMap","namespace":"demo","name":"demo-input","uid":"input-1","resourceVersion":"2","value":{"data":{"flavor":"vanilla"}}}]
    });
    let host = RendererHost::new(Limits::default());
    let Invocation::Completed(rendered) = host.render(
        std::str::from_utf8(render.bytes()).expect("render bytes UTF-8"),
        &render_input.to_string(),
    ) else {
        panic!("installed render callable must execute through RendererHost");
    };
    let automation = rendered
        .document
        .automations
        .first()
        .expect("render emits automation");
    assert_eq!(automation.bundle_digest, bundle_digest);
    assert_eq!(
        automation.action_module_digest,
        validated.manifest().action_modules[0].file.sha256
    );
    assert_eq!(
        automation.source_dependency,
        oxideloop_renderer_abi::model::Dependency::Object {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: "demo".into(),
            name: "demo-input".into(),
            uid: Some("input-1".into()),
            resource_version: Some("2".into()),
            observation_token: "snapshot-1".into(),
        }
    );
    let action_input = serde_json::json!({
        "abiVersion":"renderer.oxideloop.dev/v0", "bundleDigest": bundle_digest,
        "automationIdentity":"configuration-changed", "automationVersion":"v1",
        "actionModuleIdentity":"configuration-changed", "actionModuleDigest": automation.action_module_digest,
        "occurrenceId":"occ-1", "sourceTransitionId":"transition-1", "sourceUid":"input-1", "sourceResourceVersion":"2",
        "eventMetadata":{"source":{"namespace":"demo","name":"demo-input"},"note":"changed"}
    });
    let Invocation::Completed(action_output) = host.action(
        std::str::from_utf8(action.bytes()).expect("action bytes UTF-8"),
        &action_input.to_string(),
    ) else {
        panic!("installed action callable must execute through RendererHost");
    };
    assert_eq!(action_output.document.actions.len(), 1);
    assert_eq!(action_output.document.actions[0].kind, "Event");
}
