use compiler_core::{canonical_bytes, digest};
use release::validate_compiler_closure;
use serde_json::{Value, json};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[path = "support/closure.rs"]
mod closure;

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
    // Sorts after "vendor/example/..." ('p' > 'e') and before
    // "workspace/..." ('v' < 'w') for every index in range, keeping the
    // whole manifest in the sorted, unique order validate_files requires.
    let padding = (0..extra_vendor_files)
        .map(|index| {
            json!({
                "path": format!("vendor/pad-{index:06}"),
                "sha256": format!("sha256:{index:064x}"),
            })
        })
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
    closure::inputs(padding, &approved_runtime)
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
    inputs["compilerClosure"]["nativeCompiler"]["cargoLockSha256"] =
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

/// The rejection a resealed closure gets once `change` has been applied to
/// its native compiler.
fn native_rejection(change: impl FnOnce(&mut Value)) -> TestResult<String> {
    let (mut inputs, _) = valid_inputs()?;
    change(&mut inputs["compilerClosure"]["nativeCompiler"]);
    let (inputs, identity) = resealed(inputs)?;
    Ok(validate_compiler_closure(&inputs, &identity)
        .err()
        .ok_or("the changed closure was accepted")?
        .to_string())
}

/// Removes the file at `path` from the list at `files`.
fn without(files: &mut Value, path: &str) -> TestResult {
    files
        .as_array_mut()
        .ok_or("expected a JSON array")?
        .retain(|file| file["path"] != path);
    Ok(())
}

#[test]
fn a_v3_closure_is_rejected() -> TestResult {
    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["schemaVersion"] = json!("apsides.dev/compiler-closure/v3");
    inputs["compilerClosure"]["nativeCompiler"]["trustedCliCodeSha256"] =
        json!(format!("sha256:{}", "1".repeat(64)));
    let (inputs, identity) = resealed(inputs)?;
    let error = validate_compiler_closure(&inputs, &identity)
        .err()
        .ok_or("a v3 closure was accepted")?;
    assert!(
        error.to_string().starts_with("release JSON: unknown field"),
        "{error}"
    );

    let (mut inputs, _) = valid_inputs()?;
    inputs["compilerClosure"]["schemaVersion"] = json!("apsides.dev/compiler-closure/v3");
    let (inputs, identity) = resealed(inputs)?;
    assert_eq!(
        validate_compiler_closure(&inputs, &identity)
            .err()
            .ok_or("a v3 schema was accepted")?
            .to_string(),
        "release rejected: unsupported compiler closure schema"
    );
    Ok(())
}

#[test]
fn context_files_outside_the_mounted_directories_are_rejected() -> TestResult {
    for path in ["Dockerfile", "context.json", "target/release/controller"] {
        let error = native_rejection(|native| {
            if let Some(files) = native["buildContextFiles"].as_array_mut() {
                files.push(json!({"path":path,"sha256":format!("sha256:{}", "2".repeat(64))}));
                files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            }
        })?;
        assert_eq!(
            error,
            "release rejected: native compiler context contains a file outside the mounted build \
             directories",
            "{path}"
        );
    }
    let error = native_rejection(|native| {
        let _ = without(&mut native["buildContextFiles"], ".cargo/config.toml");
    })?;
    assert_eq!(
        error,
        "release rejected: native compiler context omits a required build input"
    );
    Ok(())
}

#[test]
fn the_lock_platform_and_rusty_v8_target_must_agree() -> TestResult {
    let error = native_rejection(|native| {
        native["cargoLockSha256"] = json!(format!("sha256:{}", "a".repeat(64)));
    })?;
    assert_eq!(
        error,
        "release rejected: native compiler lock identity is not the staged controller/Cargo.lock"
    );
    let error = native_rejection(|native| {
        native["builderImage"]["platform"] = json!("linux/amd64");
    })?;
    assert_eq!(
        error,
        "release rejected: builder and runtime base platforms differ"
    );
    let error = native_rejection(|native| {
        native["rustyV8"]["archive"]["target"] = json!("x86_64-unknown-linux-gnu");
    })?;
    assert_eq!(
        error,
        "release rejected: rusty_v8 component target differs from the build platform"
    );
    Ok(())
}

#[test]
fn rusty_v8_must_be_the_sole_staged_archive_at_its_fixed_path() -> TestResult {
    let error = native_rejection(|native| {
        if let Some(files) = native["buildContextFiles"].as_array_mut() {
            for file in files.iter_mut() {
                if file["path"] == "rusty-v8/librusty_v8.a.gz" {
                    file["path"] = json!("rusty-v8/other.a.gz");
                }
            }
        }
        native["rustyV8"]["archive"]["path"] = json!("rusty-v8/other.a.gz");
    })?;
    assert_eq!(
        error,
        "release rejected: rusty_v8 archive is not staged at rusty-v8/librusty_v8.a.gz"
    );
    let error = native_rejection(|native| {
        if let Some(files) = native["buildContextFiles"].as_array_mut() {
            files.push(
                json!({"path":"rusty-v8/extra.bin","sha256":format!("sha256:{}", "2".repeat(64))}),
            );
            files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        }
    })?;
    assert_eq!(
        error,
        "release rejected: native compiler context stages rusty-v8 files the closure does not \
         record"
    );
    Ok(())
}

#[test]
fn packaging_sources_must_be_cargo_manifests_or_src_of_the_packaging_crates() -> TestResult {
    for path in [
        "crates/cli/tests/errors.rs",
        "crates/runtime/src/lib.rs",
        "crates/packaging/build.rs",
        "scripts/build.py",
    ] {
        let error = native_rejection(|native| {
            if let Some(files) = native["packagingFiles"].as_array_mut() {
                files.push(json!({"path":path,"sha256":format!("sha256:{}", "2".repeat(64))}));
                files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            }
        })?;
        assert_eq!(
            error,
            "release rejected: native compiler packaging manifest contains an unsupported source",
            "{path}"
        );
    }
    Ok(())
}

#[test]
fn every_required_packaging_source_must_be_present() -> TestResult {
    for path in [
        "Cargo.lock",
        "Cargo.toml",
        "crates/cli/Cargo.toml",
        "crates/compiler-core/Cargo.toml",
        "crates/container/Cargo.toml",
        "crates/container/src/lib.rs",
        "crates/packaging/Cargo.toml",
        "crates/packaging/src/lib.rs",
        "crates/release/Cargo.toml",
    ] {
        let error = native_rejection(|native| {
            let _ = without(&mut native["packagingFiles"], path);
        })?;
        assert_eq!(
            error, "release rejected: native compiler closure omits trusted packaging code",
            "{path}"
        );
    }
    Ok(())
}

#[test]
fn host_tools_must_be_the_four_roles_in_order() -> TestResult {
    let error = native_rejection(|native| {
        if let Some(tools) = native["hostTools"].as_array_mut() {
            tools.swap(0, 1);
        }
    })?;
    assert_eq!(
        error,
        "release rejected: native compiler host tool identities are incomplete or ambiguous"
    );
    let error = native_rejection(|native| {
        if let Some(tools) = native["hostTools"].as_array_mut() {
            tools.pop();
        }
    })?;
    assert_eq!(
        error,
        "release rejected: native compiler host tool identities are incomplete or ambiguous"
    );
    Ok(())
}
