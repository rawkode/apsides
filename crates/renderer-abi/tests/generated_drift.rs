use std::fs;
use std::path::Path;

use oxideloop_renderer_abi::generate::generated_artifacts;

#[test]
fn generated_schemas_and_types_are_current() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for artifact in generated_artifacts() {
        let checked_in = fs::read_to_string(root.join(artifact.relative_path))
            .unwrap_or_else(|error| panic!("{}: {error}", artifact.relative_path));
        assert_eq!(
            checked_in, artifact.contents,
            "{} drifted; run `cargo run -p oxideloop-renderer-abi --bin generate -- .`",
            artifact.relative_path
        );
    }
}
