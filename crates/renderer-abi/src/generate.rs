use std::fmt::Write as _;

use schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::Value;
use ts_rs::TS;

use crate::model::{
    ActionDeclaration, ActionInputV0, ActionOutputV0, AutomationDeclaration, Dependency,
    Diagnostic, DiagnosticSeverity, Observation, ReadinessDeclaration, RenderDisposition,
    RenderInputV0, RenderOutputV0, ResourceDeclaration, RootProjection,
};

pub struct GeneratedArtifact {
    pub relative_path: &'static str,
    pub contents: String,
}

/// Generate all checked-in schemas and TypeScript declarations.
#[must_use]
pub fn generated_artifacts() -> Vec<GeneratedArtifact> {
    let typescript = typescript_declarations();
    vec![
        schema_artifact::<RenderInputV0>("abi/renderer/v0/render-input.schema.json"),
        schema_artifact::<RenderOutputV0>("abi/renderer/v0/render-output.schema.json"),
        schema_artifact::<ActionInputV0>("abi/renderer/v0/action-input.schema.json"),
        schema_artifact::<ActionOutputV0>("abi/renderer/v0/action-output.schema.json"),
        GeneratedArtifact {
            relative_path: "abi/renderer/v0/renderer-v0.generated.ts",
            contents: typescript.clone(),
        },
        GeneratedArtifact {
            relative_path: "packages/sdk/src/generated/renderer-v0.ts",
            contents: typescript,
        },
    ]
}

fn schema_artifact<T>(relative_path: &'static str) -> GeneratedArtifact
where
    T: JsonSchema,
{
    let schema = serde_json::to_value(schema_for!(T))
        .expect("serializing a generated JSON Schema cannot fail");
    let mut contents = serde_json::to_string_pretty(&sort_json(schema))
        .expect("serializing a generated JSON Schema cannot fail");
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
        scalar => scalar,
    }
}

fn typescript_declarations() -> String {
    let mut output = String::from(
        "// Generated from oxideloop-renderer-abi Rust types. Do not edit.\n\
         export type JsonPrimitive = null | boolean | number | string;\n\
         export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };\n\n",
    );

    macro_rules! declaration {
        ($type:ty) => {{
            let declaration = <$type>::decl();
            let declaration = declaration
                .strip_prefix("type ")
                .map_or(declaration.clone(), |body| format!("export type {body}"));
            writeln!(&mut output, "{declaration}\n").expect("writing to String cannot fail");
        }};
    }

    declaration!(RootProjection);
    declaration!(Observation);
    declaration!(RenderInputV0);
    declaration!(Dependency);
    declaration!(ReadinessDeclaration);
    declaration!(ResourceDeclaration);
    declaration!(AutomationDeclaration);
    declaration!(DiagnosticSeverity);
    declaration!(Diagnostic);
    declaration!(RenderDisposition);
    declaration!(RenderOutputV0);
    declaration!(ActionInputV0);
    declaration!(ActionDeclaration);
    declaration!(ActionOutputV0);
    let _ = output.pop();
    output
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[allow(dead_code)]
fn _assert_serializable<T: Serialize>() {}
