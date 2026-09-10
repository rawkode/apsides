use compiler_core::{canonical_bytes, digest};
use release::validate_compiler_closure;
use serde_json::{Value, json};

fn valid_inputs() -> (Value, String) {
    let build_context_files = json!([
        {"path":".cargo/config.toml","sha256":format!("sha256:{}", "1".repeat(64))},
        {"path":"Dockerfile","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"controller/Cargo.lock","sha256":format!("sha256:{}", "3".repeat(64))},
        {"path":"controller/Cargo.toml","sha256":format!("sha256:{}", "4".repeat(64))},
        {"path":"controller/src/ir.json","sha256":format!("sha256:{}", "5".repeat(64))},
        {"path":"controller/src/main.rs","sha256":format!("sha256:{}", "6".repeat(64))},
        {"path":"vendor/example/src/lib.rs","sha256":format!("sha256:{}", "7".repeat(64))},
        {"path":"workspace/Cargo.toml","sha256":format!("sha256:{}", "8".repeat(64))}
    ]);
    let packaging_files = json!([
        {"path":"crates/cli/Cargo.toml","sha256":format!("sha256:{}", "1".repeat(64))},
        {"path":"crates/cli/src/main.rs","sha256":format!("sha256:{}", "1".repeat(64))},
        {"path":"scripts/Dockerfile","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/build.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/extract_binary.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/finalize_inputs.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/package_oci.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/sbom.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/toolchain_snapshot.py","sha256":format!("sha256:{}", "2".repeat(64))}
    ]);
    let cli: Vec<Value> = packaging_files
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["path"].as_str().unwrap().starts_with("crates/cli/"))
        .cloned()
        .collect();
    let config = canonical_bytes(&json!({
        "architecture":"arm64","os":"linux","config":{},
        "rootfs":{"type":"layers","diff_ids":[]}
    }))
    .unwrap();
    let manifest = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":digest_bytes(&config),"size":config.len()},
        "layers":[]
    }))
    .unwrap();
    let index = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
        "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":digest_bytes(&manifest),"size":manifest.len(),"platform":{"os":"linux","architecture":"arm64"}}]
    }))
    .unwrap();
    let approved_runtime = json!({
        "baseImage":{"reference":format!("example.invalid/runtime@{}",digest_bytes(&index)),"platform":"linux/arm64"},
        "indexJson":String::from_utf8(index).unwrap(),
        "manifestJson":String::from_utf8(manifest).unwrap(),
        "configJson":String::from_utf8(config).unwrap()
    });
    let host_tools: Vec<_> = ["buildx", "cargo", "docker", "python", "apsides", "sbom"]
        .into_iter()
        .map(|role| {
            json!({"role":role,"sha256":format!("sha256:{}", "6".repeat(64)),"version":"1.0"})
        })
        .collect();
    let closure = json!({
        "schemaVersion":"apsides.dev/compiler-closure/v2",
        "authoringCompiler":{"sdk":{"identity":"@apsides/kubernetes","sha256":"sdk"},"toolchain":{"deno":"pinned"}},
        "nativeCompiler":{
            "builderImage":{"reference":format!("example.invalid/builder@sha256:{}", "3".repeat(64)),"platform":"linux/arm64"},
            "cargoLockSha256":format!("sha256:{}", "4".repeat(64)),
            "buildContextFiles":build_context_files,
            "packagingFiles":packaging_files,
            "buildProfileSha256":format!("sha256:{}", "5".repeat(64)),
            "hostTools":host_tools,
            "trustedCliCodeSha256":digest(&cli).unwrap(),
            "approvedRuntime":approved_runtime
        }
    });
    let identity = digest(&closure).unwrap();
    (
        json!({"sdk":closure["authoringCompiler"]["sdk"],"toolchain":closure["authoringCompiler"]["toolchain"],"compilerClosure":closure,"compilerClosureDigest":identity}),
        identity,
    )
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[test]
fn recomputes_complete_compiler_closure() {
    let (inputs, identity) = valid_inputs();
    assert_eq!(
        validate_compiler_closure(&inputs, &identity).unwrap(),
        identity
    );
}

#[test]
fn rejects_labels_disconnected_from_actual_closure() {
    let (mut inputs, identity) = valid_inputs();
    assert!(validate_compiler_closure(&inputs, &format!("sha256:{}", "f".repeat(64))).is_err());
    inputs["toolchain"]["deno"] = json!("other");
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
}

#[test]
fn rejects_incomplete_or_forged_native_identity() {
    let (mut inputs, identity) = valid_inputs();
    inputs["compilerClosure"]["nativeCompiler"] = Value::Null;
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    let (mut inputs, identity) = valid_inputs();
    inputs["compilerClosure"]["nativeCompiler"]["trustedCliCodeSha256"] =
        json!(format!("sha256:{}", "9".repeat(64)));
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
}

#[test]
fn rejects_ambiguous_native_file_manifests() {
    let (mut inputs, identity) = valid_inputs();
    inputs["compilerClosure"]["nativeCompiler"]["buildContextFiles"]
        .as_array_mut()
        .unwrap()
        .reverse();
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
}

#[test]
fn rejects_missing_packaging_sources_and_host_tools() {
    let (mut inputs, _) = valid_inputs();
    inputs["compilerClosure"]["nativeCompiler"]["packagingFiles"]
        .as_array_mut()
        .unwrap()
        .pop();
    let identity = digest(&inputs["compilerClosure"]).unwrap();
    inputs["compilerClosureDigest"] = json!(identity.clone());
    assert!(validate_compiler_closure(&inputs, &identity).is_err());

    let (mut inputs, _) = valid_inputs();
    inputs["compilerClosure"]["nativeCompiler"]["hostTools"][0]["role"] = json!("docker");
    let identity = digest(&inputs["compilerClosure"]).unwrap();
    inputs["compilerClosureDigest"] = json!(identity.clone());
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
}

#[test]
fn rejects_runtime_documents_disconnected_from_the_approved_base() {
    let (mut inputs, _) = valid_inputs();
    inputs["compilerClosure"]["nativeCompiler"]["approvedRuntime"]["manifestJson"] = json!("{}");
    let identity = digest(&inputs["compilerClosure"]).unwrap();
    inputs["compilerClosureDigest"] = json!(identity);
    assert!(
        validate_compiler_closure(&inputs, inputs["compilerClosureDigest"].as_str().unwrap())
            .is_err()
    );
}
