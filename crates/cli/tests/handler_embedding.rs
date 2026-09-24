//! `aps compile` embeds each declared handler's bundle into the generated
//! controller so `include_str!` compiles it into the image, exactly as
//! Phase 4 of the event plane requires — without executing it (that is a
//! later phase, in a separate crate).
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
const HANDLER_MODULE: &str = "export async function syncFlags(event) {\n  return event;\n}\n";

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_source(root: &Path) -> Result {
    fs::create_dir_all(root.join("handlers"))?;
    fs::write(root.join("program.tsx"), PROGRAM)?;
    fs::write(root.join("handlers/sync-flags.ts"), HANDLER_MODULE)?;
    let inputs = serde_json::json!([
        {"path": "handlers/sync-flags.ts", "sha256": digest(HANDLER_MODULE.as_bytes())},
        {"path": "program.tsx", "sha256": digest(PROGRAM.as_bytes())},
    ]);
    let lock = serde_json::json!({
        "schemaVersion": "apsides.dev/source-lock/v1",
        "entry": "program.tsx",
        "inputs": inputs,
    });
    fs::write(
        root.join("apsides.lock.json"),
        format!("{}\n", serde_json::to_string_pretty(&lock)?),
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
        main_rs.contains("runtime::run(include_str!(\"ir.json\"), HANDLERS)"),
        "main.rs passes the embedded handlers to runtime::run: {main_rs}"
    );

    let manifest: Value = serde_json::from_str(&fs::read_to_string(output.join("manifest.json"))?)?;
    assert!(
        manifest["files"][format!("controller/src/handlers/{handler_id}.js")].is_string(),
        "manifest records the embedded handler bundle's digest"
    );
    Ok(())
}
