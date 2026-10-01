//! `aps compile` embeds each declared handler's bundle into the generated
//! controller so `include_str!` compiles it into the image, without
//! executing it at compile time: only the
//! compiled controller runs it, through the `engine` crate, in production
//! mode.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const PROGRAM: &str = r#"import { Controller, Deployment, On, useConfigMap, useHandlerOutput } from "@apsides/kubernetes";
const HandlerApp = () => {
  const settings = useConfigMap({ name: "settings" });
  const flags = useHandlerOutput({ name: "flags" });
  return <>
    <On trigger={settings.changed} handler="./handlers/sync-flags.ts#syncFlags" inputs={[settings]} egress={["api.example.com"]} outputs={[flags]} />
    <Deployment metadata={{name:"api",namespace:"workload"}} rolloutOn={[flags.data]} spec={{selector:{matchLabels:{app:"api"}},template:{metadata:{labels:{app:"api"}},spec:{containers:[{name:"api",image:"example.invalid/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}}}} />
  </>;
};
export default (
  <Controller id="handlers-check" namespace="workload" controlNamespace="apsides-system" clusterBinding="fixture" policyRef="platform/test" enforcement="api-bound" budgets={{maxResources:8,maxInputs:8,maxReactions:8,maxObjectBytes:32768,maxQueueDepth:8,maxConcurrentRequests:2}}>
    <HandlerApp />
  </Controller>
);
"#;
const HANDLER_MODULE: &str =
    "export async function syncFlags(event: { readonly id: string }) {\n  return event.id;\n}\n";

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_source(root: &Path) -> Result {
    fs::create_dir_all(root.join("handlers"))?;
    fs::write(root.join("program.tsx"), PROGRAM)?;
    fs::write(root.join("handlers/sync-flags.ts"), HANDLER_MODULE)?;
    // `packages/compiler/src/compiler.ts`'s `parseCanonicalLock` requires the
    // lock's own on-disk bytes to already be its canonical (alphabetically
    // key-sorted) pretty-printed form. `serde_json::json!`/`Value::Object`
    // only produces that ordering when compiled without the `preserve_order`
    // feature; this workspace's `deno_core` dependency (the Apsides v0.3
    // plan's Phase 5b engine) enables `serde_json/preserve_order`, and Cargo
    // unifies that feature across every crate using this `serde_json`
    // version in the same build -- so this fixture, unlike production's own
    // `source-lock.ts` generator (which always sorts explicitly), must build
    // its object with keys inserted in already-sorted order to stay
    // byte-canonical regardless of which `Map` backend is active. Compare
    // `compiler_core::json::canonical_bytes`'s own explicit sort, guarded by
    // the identical comment.
    let sorted_input = |path: &str, bytes: &[u8]| -> serde_json::Value {
        let mut object = serde_json::Map::new();
        object.insert("path".into(), serde_json::Value::String(path.into()));
        object.insert("sha256".into(), serde_json::Value::String(digest(bytes)));
        serde_json::Value::Object(object)
    };
    let inputs = serde_json::Value::Array(vec![
        sorted_input("handlers/sync-flags.ts", HANDLER_MODULE.as_bytes()),
        sorted_input("program.tsx", PROGRAM.as_bytes()),
    ]);
    let mut lock = serde_json::Map::new();
    lock.insert(
        "entry".into(),
        serde_json::Value::String("program.tsx".into()),
    );
    lock.insert("inputs".into(), inputs);
    lock.insert(
        "schemaVersion".into(),
        serde_json::Value::String("apsides.dev/source-lock/v1".into()),
    );
    fs::write(
        root.join("apsides.lock.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::Value::Object(lock))?
        ),
    )?;
    Ok(())
}

#[test]
fn compiled_handler_bundle_is_embedded_and_referenced_by_main_rs() -> Result {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("src");
    fs::create_dir_all(&source)?;
    write_source(&source)?;
    let output = directory.path().join("out");

    let result = Command::new(env!("CARGO_BIN_EXE_aps"))
        .arg("compile")
        .arg("--source")
        .arg(source.join("program.tsx"))
        .arg("--output")
        .arg(&output)
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "aps compile failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }

    let ir: Value = serde_json::from_str(&fs::read_to_string(output.join("ir.json"))?)?;
    let handlers = ir["handlers"].as_array().ok_or("ir.handlers missing")?;
    assert_eq!(handlers.len(), 1, "one handler declared");
    let handler = &handlers[0];
    let handler_id = handler["id"].as_str().ok_or("handler id missing")?;
    let bundle_sha256 = handler["bundleSha256"]
        .as_str()
        .ok_or("bundleSha256 missing")?;
    assert_eq!(
        bundle_sha256.len(),
        64,
        "bundleSha256 is a sha256 hex digest"
    );

    let outputs = ir["outputs"].as_array().ok_or("ir.outputs missing")?;
    assert_eq!(outputs.len(), 1, "one output declared");
    let output_id = outputs[0]["id"].as_str().ok_or("output id missing")?;
    let resources = ir["resources"].as_array().ok_or("ir.resources missing")?;
    let output_resource = resources
        .iter()
        .find(|resource| resource["id"] == output_id)
        .ok_or("output resource missing")?;
    assert_eq!(output_resource["handlerOutput"], true);
    assert_eq!(
        output_resource["desired"]["metadata"]["labels"]["apsides.dev/handler-output"],
        output_id
    );
    assert!(output_resource["desired"]["data"].is_null());

    let bundle_path = output
        .join("controller/src/handlers")
        .join(format!("{handler_id}.js"));
    let bundle = fs::read(&bundle_path)?;
    assert_eq!(
        digest(&bundle),
        bundle_sha256,
        "embedded handler bundle matches ir.json's bundleSha256"
    );
    assert!(
        String::from_utf8_lossy(&bundle).contains("syncFlags"),
        "embedded bundle contains the handler's exported function"
    );

    let main_rs = fs::read_to_string(output.join("controller/src/main.rs"))?;
    assert!(
        main_rs.contains(&format!("include_str!(\"handlers/{handler_id}.js\")")),
        "main.rs embeds the handler bundle with include_str!: {main_rs}"
    );
    assert!(
        main_rs.contains("runtime::start(include_str!(\"ir.json\"), HANDLERS)"),
        "main.rs passes the embedded handlers to runtime::start: {main_rs}"
    );
    assert!(
        !main_rs.contains("async fn main") && !main_rs.contains("#[tokio::main]"),
        "main.rs is synchronous and must not use #[tokio::main]: {main_rs}"
    );
    assert!(
        main_rs.contains("fn main() {"),
        "main.rs declares a plain, synchronous fn main: {main_rs}"
    );

    let manifest: Value = serde_json::from_str(&fs::read_to_string(output.join("manifest.json"))?)?;
    assert!(
        manifest["files"][format!("controller/src/handlers/{handler_id}.js")].is_string(),
        "manifest records the embedded handler bundle's digest"
    );
    Ok(())
}
