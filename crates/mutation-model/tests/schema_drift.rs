use std::fs;
use std::path::Path;

use oxideloop_mutation_model::generated_probe_schema;

#[test]
fn checked_probe_schema_is_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../probes/conditional-mutation/v0/probe.schema.json");
    assert_eq!(fs::read_to_string(path).unwrap(), generated_probe_schema());
}
