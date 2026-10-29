//! The v4 compiler closure the release tests build on: a complete,
//! consistent `build-inputs.json` around a caller-supplied approved runtime.
use compiler_core::digest;
use serde_json::{Value, json};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn sha(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

/// `build-inputs.json` and its compiler closure digest, for an arm64 build
/// over `approved_runtime`.
///
/// `padding` holds extra `buildContextFiles` entries, each sorting after
/// `vendor/example/...` and before `workspace/...`, so a test can size the
/// document like a real vendored dependency closure.
pub fn inputs(padding: Vec<Value>, approved_runtime: &Value) -> TestResult<(Value, String)> {
    let mut build_context_files = vec![
        json!({"path":".cargo/config.toml","sha256":sha('1')}),
        json!({"path":"controller/Cargo.lock","sha256":sha('3')}),
        json!({"path":"controller/Cargo.toml","sha256":sha('4')}),
        json!({"path":"controller/src/ir.json","sha256":sha('5')}),
        json!({"path":"controller/src/main.rs","sha256":sha('6')}),
        json!({"path":"rusty-v8/librusty_v8.a.gz","sha256":sha('9')}),
        json!({"path":"vendor/example/src/lib.rs","sha256":sha('7')}),
    ];
    build_context_files.extend(padding);
    build_context_files.push(json!({"path":"workspace/Cargo.toml","sha256":sha('8')}));
    let mut packaging_paths = vec![
        "Cargo.lock",
        "Cargo.toml",
        "crates/cli/Cargo.toml",
        "crates/cli/src/main.rs",
        "crates/compiler-core/Cargo.toml",
        "crates/container/Cargo.toml",
        "crates/container/src/lib.rs",
        "crates/packaging/Cargo.toml",
        "crates/packaging/src/lib.rs",
        "crates/release/Cargo.toml",
    ];
    packaging_paths.sort_unstable();
    let packaging_files: Vec<Value> = packaging_paths
        .into_iter()
        .map(|path| json!({"path":path,"sha256":sha('2')}))
        .collect();
    let host_tools: Vec<Value> = ["engine", "cargo", "apsides", "sbom"]
        .into_iter()
        .map(|role| json!({"role":role,"sha256":sha('6'),"version":"1.0"}))
        .collect();
    let rusty_v8 = json!({
        "archive": {
            "name": "librusty_v8", "version": "150.4.0", "target": "aarch64-unknown-linux-gnu",
            "url": "https://github.com/denoland/rusty_v8/releases/download/v150.4.0/librusty_v8_simdutf_release_aarch64-unknown-linux-gnu.a.gz",
            "sha256": sha('9'), "path": "rusty-v8/librusty_v8.a.gz",
        },
        "bindingFile": Value::Null,
    });
    let closure = json!({
        "schemaVersion":"apsides.dev/compiler-closure/v4",
        "authoringCompiler":{"sdk":{"identity":"@apsides/kubernetes","sha256":"sdk"},"toolchain":{"deno":"pinned"}},
        "nativeCompiler":{
            "builderImage":{"reference":format!("example.invalid/builder@{}", sha('3')),"platform":"linux/arm64"},
            "cargoLockSha256":sha('3'),
            "buildContextFiles":build_context_files,
            "packagingFiles":packaging_files,
            "buildProfileSha256":sha('5'),
            "hostTools":host_tools,
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
