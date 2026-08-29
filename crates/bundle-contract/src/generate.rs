use std::fmt::Write as _;

use schemars::schema_for;
use serde_json::Value;
use ts_rs::TS;

use crate::model::{
    BundleInstallRecordV0, BundleIntegrityManifestV0, ExecutionPhaseV0, FileIntegrityV0,
    ModuleIntegrityV0, PhaseModuleV0, ToolIdentityV0,
};

pub struct GeneratedArtifact {
    pub relative_path: &'static str,
    pub contents: String,
}

#[must_use]
pub fn generated_artifacts() -> Vec<GeneratedArtifact> {
    vec![
        schema_artifact::<BundleIntegrityManifestV0>(
            "abi/bundle/v0/integrity-manifest.schema.json",
        ),
        schema_artifact::<BundleInstallRecordV0>("abi/bundle/v0/install-record.schema.json"),
        GeneratedArtifact {
            relative_path: "abi/bundle/v0/bundle-v0.generated.ts",
            contents: typescript_declarations(),
        },
    ]
}

fn schema_artifact<T>(relative_path: &'static str) -> GeneratedArtifact
where
    T: schemars::JsonSchema,
{
    let value = serde_json::to_value(schema_for!(T)).expect("schema serializes");
    let mut contents = serde_json::to_string_pretty(&sort_json(value)).expect("schema JSON");
    contents.push('\n');
    GeneratedArtifact {
        relative_path,
        contents,
    }
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sort_json(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json).collect()),
        value => value,
    }
}

fn typescript_declarations() -> String {
    let mut output =
        String::from("// Generated from oxideloop-bundle-contract Rust types. Do not edit.\n\n");
    macro_rules! declaration {
        ($type:ty) => {{
            let declaration = <$type>::decl();
            let declaration = declaration
                .strip_prefix("type ")
                .map_or(declaration.clone(), |body| format!("export type {body}"));
            writeln!(&mut output, "{declaration}\n").expect("write String");
        }};
    }
    declaration!(ToolIdentityV0);
    declaration!(FileIntegrityV0);
    declaration!(ModuleIntegrityV0);
    declaration!(ExecutionPhaseV0);
    declaration!(PhaseModuleV0);
    declaration!(BundleIntegrityManifestV0);
    declaration!(BundleInstallRecordV0);
    let _ = output.pop();
    output
}
