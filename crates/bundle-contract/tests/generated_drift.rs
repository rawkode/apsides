use std::fs;
use std::path::Path;

use bundle_contract::generate::generated_artifacts;

#[test]
fn checked_in_bundle_contract_artifacts_are_current() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for artifact in generated_artifacts() {
        assert_eq!(
            fs::read_to_string(root.join(artifact.relative_path)).unwrap(),
            artifact.contents,
            "{} drifted; run `cargo run -p bundle-contract --bin generate-bundle-contract -- .`",
            artifact.relative_path
        );
    }
}
