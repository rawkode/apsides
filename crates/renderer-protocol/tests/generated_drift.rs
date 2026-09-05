use std::fs;
use std::path::Path;

#[test]
fn checked_in_renderer_protocol_artifacts_are_current() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    renderer_protocol::generate::generate(&root, true).unwrap();
    let typescript =
        fs::read_to_string(root.join("abi/renderer-protocol/v0/renderer-protocol-v0.generated.ts"))
            .unwrap();
    assert!(typescript.contains("RequestIdV0 = { bytes: number[]"));
    let schema: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("abi/renderer-protocol/v0/request.schema.json")).unwrap(),
    )
    .unwrap();
    let bytes = &schema["$defs"]["RequestIdV0"]["properties"]["bytes"];
    assert_eq!(bytes["minItems"], 16);
    assert_eq!(bytes["maxItems"], 16);
}

#[test]
fn stale_short_artifacts_return_an_error_instead_of_panicking() {
    let root = tempfile::TempDir::new().unwrap();
    let directory = root.path().join("abi/renderer-protocol/v0");
    fs::create_dir_all(&directory).unwrap();
    for name in [
        "request.schema.json",
        "response.schema.json",
        "renderer-protocol-v0.generated.ts",
    ] {
        fs::write(directory.join(name), "x").unwrap();
    }
    assert!(renderer_protocol::generate::generate(root.path(), true).is_err());
}
