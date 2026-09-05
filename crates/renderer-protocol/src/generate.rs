use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use schemars::schema_for;
use serde_json::{Map, Value};
use ts_rs::TS;

use crate::{ErrorCategoryV0, PhaseV0, RequestIdV0, RequestV0, ResponseStatusV0, ResponseV0};

/// Generate or verify the checked-in renderer protocol schemas and TypeScript types.
///
/// # Errors
///
/// Returns an error when generation fails or checked-in artifacts are stale.
pub fn generate(root: &Path, check: bool) -> Result<(), Box<dyn std::error::Error>> {
    let request_schema =
        serde_json::to_string_pretty(&sort_json(serde_json::to_value(schema_for!(RequestV0))?))?
            + "\n";
    let response_schema =
        serde_json::to_string_pretty(&sort_json(serde_json::to_value(schema_for!(ResponseV0))?))?
            + "\n";
    let mut typescript =
        String::from("// Generated from renderer-protocol Rust types. Do not edit.\n\n");
    macro_rules! emit {
        ($type:ty) => {{
            let item = <$type>::decl();
            let item = item
                .strip_prefix("type ")
                .map_or(item.clone(), |value| format!("export type {value}"));
            writeln!(&mut typescript, "{item}\n")?;
        }};
    }
    emit!(PhaseV0);
    emit!(ResponseStatusV0);
    emit!(ErrorCategoryV0);
    emit!(RequestIdV0);
    emit!(RequestV0);
    emit!(ResponseV0);
    let _ = typescript.pop();
    let artifacts = [
        (
            "abi/renderer-protocol/v0/request.schema.json",
            request_schema,
        ),
        (
            "abi/renderer-protocol/v0/response.schema.json",
            response_schema,
        ),
        (
            "abi/renderer-protocol/v0/renderer-protocol-v0.generated.ts",
            typescript,
        ),
    ];
    let mut stale = Vec::new();
    for (relative, contents) in artifacts {
        let path = root.join(relative);
        if check {
            let actual = fs::read_to_string(&path).ok();
            if actual.as_deref() != Some(contents.as_str()) {
                let first_difference = actual.as_ref().and_then(|actual| {
                    actual
                        .bytes()
                        .zip(contents.bytes())
                        .position(|(actual, expected)| actual != expected)
                });
                stale.push(format!(
                    "{relative} (expected {} bytes, found {}, first difference {:?})",
                    contents.len(),
                    actual.as_ref().map_or(0, String::len),
                    first_difference
                ));
            }
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, contents)?;
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "generated renderer protocol artifacts are stale: {}",
            stale.join(", ")
        )
        .into())
    }
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, sort_json(value));
            }
            Value::Object(sorted)
        }
        scalar => scalar,
    }
}
