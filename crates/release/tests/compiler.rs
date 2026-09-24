use compiler_core::{canonical_bytes, digest};
use release::validate_compiler_closure;
use serde_json::{Value, json};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn valid_inputs() -> TestResult<(Value, String)> {
    inputs_with_padding(0)
}

/// [`valid_inputs`], with `extra_vendor_files` synthetic entries added to
/// `buildContextFiles` between the fixture's real `vendor/example/...` entry
/// and `workspace/Cargo.toml` -- short, sorted, digest-bound paths that stay
/// well under the closure's own 8 MiB byte budget even at real-vendor scale,
/// so a padded closure's rejection or acceptance is driven by the file-count
/// ceiling being tested, not incidentally by the byte budget.
fn inputs_with_padding(extra_vendor_files: usize) -> TestResult<(Value, String)> {
    let mut build_context_files = vec![
        json!({"path":".cargo/config.toml","sha256":format!("sha256:{}", "1".repeat(64))}),
        json!({"path":"Dockerfile","sha256":format!("sha256:{}", "2".repeat(64))}),
        json!({"path":"controller/Cargo.lock","sha256":format!("sha256:{}", "3".repeat(64))}),
        json!({"path":"controller/Cargo.toml","sha256":format!("sha256:{}", "4".repeat(64))}),
        json!({"path":"controller/src/ir.json","sha256":format!("sha256:{}", "5".repeat(64))}),
        json!({"path":"controller/src/main.rs","sha256":format!("sha256:{}", "6".repeat(64))}),
        json!({"path":"rusty-v8/librusty_v8.a.gz","sha256":format!("sha256:{}", "9".repeat(64))}),
        json!({"path":"vendor/example/src/lib.rs","sha256":format!("sha256:{}", "7".repeat(64))}),
    ];
    // Sorts after "vendor/example/..." ('p' > 'e') and before
    // "workspace/..." ('v' < 'w') for every index in range, keeping the
    // whole manifest in the sorted, unique order validate_files requires.
    for index in 0..extra_vendor_files {
        build_context_files.push(json!({
            "path": format!("vendor/pad-{index:06}"),
            "sha256": format!("sha256:{:064x}", index),
        }));
    }
    build_context_files
        .push(json!({"path":"workspace/Cargo.toml","sha256":format!("sha256:{}", "8".repeat(64))}));
    let build_context_files = Value::Array(build_context_files);
    let packaging_files = json!([
        {"path":"crates/cli/Cargo.toml","sha256":format!("sha256:{}", "1".repeat(64))},
        {"path":"crates/cli/src/main.rs","sha256":format!("sha256:{}", "1".repeat(64))},
        {"path":"scripts/Dockerfile","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/build.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/diagnostics.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/extract_binary.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/finalize_inputs.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/package_oci.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/rusty_v8.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/sbom.py","sha256":format!("sha256:{}", "2".repeat(64))},
        {"path":"scripts/toolchain_snapshot.py","sha256":format!("sha256:{}", "2".repeat(64))}
    ]);
    let rusty_v8 = json!({
        "archive": {
            "name": "librusty_v8", "version": "150.4.0", "target": "aarch64-unknown-linux-gnu",
            "url": "https://github.com/denoland/rusty_v8/releases/download/v150.4.0/librusty_v8_simdutf_release_aarch64-unknown-linux-gnu.a.gz",
            "sha256": format!("sha256:{}", "9".repeat(64)), "path": "rusty-v8/librusty_v8.a.gz",
        },
        "bindingFile": Value::Null,
    });
    let cli: Vec<Value> = packaging_files
        .as_array()
        .ok_or("packaging file list is not an array")?
        .iter()
        .filter(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("crates/cli/"))
        })
        .cloned()
        .collect();
    let config = canonical_bytes(&json!({
        "architecture":"arm64","os":"linux","config":{},
        "rootfs":{"type":"layers","diff_ids":[]}
    }))?;
    let manifest = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":digest_bytes(&config),"size":config.len()},
        "layers":[]
    }))?;
    let index = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
        "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":digest_bytes(&manifest),"size":manifest.len(),"platform":{"os":"linux","architecture":"arm64"}}]
    }))?;
    let approved_runtime = json!({
        "baseImage":{"reference":format!("example.invalid/runtime@{}",digest_bytes(&index)),"platform":"linux/arm64"},
        "indexJson":String::from_utf8(index)?,
        "manifestJson":String::from_utf8(manifest)?,
        "configJson":String::from_utf8(config)?
    });
    let host_tools: Vec<_> = ["buildx", "cargo", "docker", "python", "apsides", "sbom"]
        .into_iter()
        .map(|role| {
            json!({"role":role,"sha256":format!("sha256:{}", "6".repeat(64)),"version":"1.0"})
        })
        .collect();
    let closure = json!({
        "schemaVersion":"apsides.dev/compiler-closure/v3",
        "authoringCompiler":{"sdk":{"identity":"@apsides/kubernetes","sha256":"sdk"},"toolchain":{"deno":"pinned"}},
        "nativeCompiler":{
            "builderImage":{"reference":format!("example.invalid/builder@sha256:{}", "3".repeat(64)),"platform":"linux/arm64"},
            "cargoLockSha256":format!("sha256:{}", "4".repeat(64)),
            "buildContextFiles":build_context_files,
            "packagingFiles":packaging_files,
            "buildProfileSha256":format!("sha256:{}", "5".repeat(64)),
            "hostTools":host_tools,
            "trustedCliCodeSha256":digest(&cli)?,
            "approvedRuntime":approved_runtime,
            "rustyV8":rusty_v8
        }
    });
    let identity = digest(&closure)?;
    Ok((
        json!({"sdk":closure["authoringCompiler"]["sdk"],"toolchain":closure["authoringCompiler"]["toolchain"],"compilerClosure":closure,"compilerClosureDigest":identity}),
        identity,
    ))
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[test]
fn recomputes_complete_compiler_closure() -> TestResult {
    let (inputs, identity) = valid_inputs()?;
    assert_eq!(validate_compiler_closure(&inputs, &identity)?, identity);
    Ok(())
}

#[test]
fn rejects_labels_disconnected_from_actual_closure() -> TestResult {
    let (mut inputs, identity) = valid_inputs()?;
    assert!(validate_compiler_closure(&inputs, &format!("sha256:{}", "f".repeat(64))).is_err());
    inputs["toolchain"]["deno"] = json!("other");
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    Ok(())
}

#[test]
fn rejects_incomplete_or_forged_native_identity() -> TestResult {
    let (mut inputs, identity) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"] = Value::Null;
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    let (mut inputs, identity) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["trustedCliCodeSha256"] =
        json!(format!("sha256:{}", "9".repeat(64)));
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    Ok(())
}

#[test]
fn rejects_ambiguous_native_file_manifests() -> TestResult {
    let (mut inputs, identity) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["buildContextFiles"]
        .as_array_mut()
        .ok_or("build context file list is not an array")?
        .reverse();
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    Ok(())
}

#[test]
fn rejects_missing_packaging_sources_and_host_tools() -> TestResult {
    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["packagingFiles"]
        .as_array_mut()
        .ok_or("expected a JSON array")?
        .pop();
    let identity = digest(&inputs["compilerClosure"])?;
    inputs["compilerClosureDigest"] = json!(identity);
    assert!(validate_compiler_closure(&inputs, &identity).is_err());

    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["hostTools"][0]["role"] = json!("docker");
    let identity = digest(&inputs["compilerClosure"])?;
    inputs["compilerClosureDigest"] = json!(identity);
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    Ok(())
}

#[test]
fn rejects_runtime_documents_disconnected_from_the_approved_base() -> TestResult {
    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["approvedRuntime"]["manifestJson"] = json!("{}");
    let identity = digest(&inputs["compilerClosure"])?;
    inputs["compilerClosureDigest"] = json!(identity);
    assert!(
        validate_compiler_closure(
            &inputs,
            inputs["compilerClosureDigest"]
                .as_str()
                .ok_or("compiler closure digest is not a string")?
        )
        .is_err()
    );
    Ok(())
}

/// Re-seal the closure after a change, so only the changed field can reject it.
fn resealed(mut inputs: Value) -> TestResult<(Value, String)> {
    let identity = digest(&inputs["compilerClosure"])?;
    inputs["compilerClosureDigest"] = json!(identity);
    Ok((inputs, identity))
}

#[test]
fn rejects_runtimes_without_a_pinned_reference_or_with_unknown_fields() -> TestResult {
    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["approvedRuntime"]["baseImage"]["reference"] =
        json!("example.invalid/runtime:latest");
    let (inputs, identity) = resealed(inputs)?;
    assert!(validate_compiler_closure(&inputs, &identity).is_err());

    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["approvedRuntime"]["registryToken"] = json!("{}");
    let (inputs, identity) = resealed(inputs)?;
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    Ok(())
}

#[test]
fn rejects_a_rusty_v8_record_disconnected_from_its_staged_file() -> TestResult {
    // An unrecognised target triple.
    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["rustyV8"]["archive"]["target"] =
        json!("x86_64-pc-windows-msvc");
    let (inputs, identity) = resealed(inputs)?;
    assert!(validate_compiler_closure(&inputs, &identity).is_err());

    // A digest that does not match the same-path entry in buildContextFiles.
    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["rustyV8"]["archive"]["sha256"] =
        json!(format!("sha256:{}", "a".repeat(64)));
    let (inputs, identity) = resealed(inputs)?;
    assert!(validate_compiler_closure(&inputs, &identity).is_err());

    // A path naming no buildContextFiles entry at all.
    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["nativeCompiler"]["rustyV8"]["archive"]["path"] =
        json!("rusty-v8/other.a.gz");
    let (inputs, identity) = resealed(inputs)?;
    assert!(validate_compiler_closure(&inputs, &identity).is_err());
    Ok(())
}

/// A real `cargo vendor --offline --versioned-dirs` of the actual
/// `controller/Cargo.lock` (342 locked packages, once `crates/engine`'s
/// `deno_core`/`v8` are in the graph) stages 37,408 files under `vendor/`
/// alone -- comfortably over the file-count ceiling's previous value of
/// 32,768, which rejected every such real, valid closure with "native
/// compiler closure is incomplete or invalid" (measured directly: a real
/// vendor of this workspace's actual locked dependencies, not an estimate).
/// 40,000 padding entries reproduces that real scale directly rather than
/// only asserting the ceiling moved.
#[test]
fn accepts_a_real_scale_vendored_dependency_closure_over_the_previous_file_count_ceiling()
-> TestResult {
    let (inputs, identity) = inputs_with_padding(40_000)?;
    assert_eq!(validate_compiler_closure(&inputs, &identity)?, identity);
    Ok(())
}

#[test]
fn rejects_a_build_context_file_manifest_over_its_own_file_count_ceiling() -> TestResult {
    let (inputs, identity) = inputs_with_padding(65_600)?;
    let error = validate_compiler_closure(&inputs, &identity)
        .err()
        .ok_or("expected a build context file manifest over the ceiling to be rejected")?
        .to_string();
    assert!(error.contains("incomplete or invalid"), "{error}");
    Ok(())
}
