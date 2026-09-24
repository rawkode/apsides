use compiler_core::{canonical_bytes, digest, parse_and_validate};
use release::{SignaturePolicy, TrustPolicy, VerifierTool, verify_candidate};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

/// The one rejection a fixture release gets, whichever marker exposes it.
const FIXTURE_REJECTED: &str =
    "release rejected: fixture-substituted artifacts cannot be promoted to production";
/// The rejection for a production-mode release whose IR is not explicitly
/// `testOnly: false`; payload validation reaches it before the mode check.
const MARKER_REQUIRED: &str =
    "release rejected: production IR must explicitly contain testOnly:false";

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
async fn rejection(root: &Path, policy: &TrustPolicy) -> TestResult<String> {
    Ok(verify_candidate(root, policy)
        .await
        .err()
        .ok_or("expected verification to fail")?
        .to_string())
}
fn write_json(root: &Path, name: &str, value: &Value) -> TestResult<Vec<u8>> {
    let bytes = canonical_bytes(value)?;
    fs::write(root.join(name), &bytes)?;
    Ok(bytes)
}

fn tar(files: Vec<(&str, &[u8], bool)>) -> TestResult<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::new());
    for (path, bytes, directory) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(if directory {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        header.set_cksum();
        archive.append_data(&mut header, path, bytes)?;
    }
    Ok(archive.into_inner()?)
}

fn approved_runtime() -> TestResult<Value> {
    let layer = tar(vec![])?;
    let config = canonical_bytes(&json!({
        "architecture":"arm64","os":"linux","config":{},
        "rootfs":{"type":"layers","diff_ids":[hash(&layer)]}
    }))?;
    let runtime_manifest = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&layer),"size":layer.len()}]
    }))?;
    let index = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
        "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":hash(&runtime_manifest),"size":runtime_manifest.len(),"platform":{"os":"linux","architecture":"arm64"}}]
    }))?;
    Ok(json!({
        "baseImage":{"reference":format!("example.invalid/runtime@{}",hash(&index)),"platform":"linux/arm64"},
        "indexJson":String::from_utf8(index)?,
        "manifestJson":String::from_utf8(runtime_manifest)?,
        "configJson":String::from_utf8(config)?
    }))
}
fn command(args: &[&str]) -> TestResult {
    let output = Command::new("/usr/bin/ssh-keygen").args(args).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
/// The compiled documents a release is assembled from, and their digests.
struct Documents {
    plan: compiler_core::CompiledPlan,
    inputs: Value,
    compiler: String,
    files: BTreeMap<String, String>,
}

/// `padding_entries` inflates `build-inputs.json` with that many extra
/// synthetic `buildContextFiles` entries (see `compiler_inputs`); `0` for
/// the ordinary fixture.
fn write_documents_with_padding(root: &Path, padding_entries: usize) -> TestResult<Documents> {
    let ir = json!({
        "schemaVersion":"apsides.dev/ir/v2","testOnly":false,
        "domain":{"id":"example","namespace":"workload","controlNamespace":"apsides-system","clusterBinding":"test-cluster","policyRef":"platform/example","enforcement":"api-bound","minKubernetesVersion":"1.34","budgets":{"maxResources":64,"maxInputs":64,"maxReactions":64,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"config","namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload"},"data":{"hello":"world"}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"controller.ts","line":1,"column":1}}],
        "inputs":[],"reactions":[],
        "handlers":[],"outputs":[],"provenance":{"compiler":"apsides/0.1","inputs":[{"path":"controller.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    });
    let plan = parse_and_validate(&ir.to_string())?;
    let (inputs, compiler) = compiler_inputs(padding_entries)?;
    // Verifier test payloads only; nothing here is compiled or executed.
    let mut files = BTreeMap::new();
    for (name, value) in [
        ("ir.json", ir),
        ("plan.json", serde_json::to_value(&plan)?),
        ("build-inputs.json", inputs.clone()),
        ("source-map.json", json!({})),
        ("sbom.json", json!({})),
        ("compatibility.json", json!({})),
        ("graphs.json", json!({})),
        ("test-plan.json", json!([])),
    ] {
        files.insert(name.to_owned(), hash(&write_json(root, name, &value)?));
    }
    for (name, value) in compiler_core::review_documents(&plan) {
        files.insert(name.into(), hash(&write_json(root, name, &value)?));
    }
    files.insert(
        "test-plan.json".into(),
        hash(&write_json(
            root,
            "test-plan.json",
            &serde_json::to_value(&plan.test_plan)?,
        )?),
    );
    Ok(Documents {
        plan,
        inputs,
        compiler,
        files,
    })
}

fn release(root: &Path) -> TestResult<TrustPolicy> {
    release_with_padding(root, 0)
}

/// `padding_entries` inflates `build-inputs.json` well past
/// `compiler_core::MAX_IR_BYTES` (4 MiB), the way a real one now does once
/// `crates/engine`'s `deno_core` is in the vendored dependency closure, but
/// stays a fully valid, digest-consistent, signed release: every digest
/// here is computed from the actual (inflated) document, not hardcoded.
fn release_with_padding(root: &Path, padding_entries: usize) -> TestResult<TrustPolicy> {
    let Documents {
        plan,
        inputs,
        compiler,
        mut files,
    } = write_documents_with_padding(root, padding_entries)?;
    // Minimal architecture header exercises metadata verification, not execution.
    let mut binary = vec![0_u8; 64];
    binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
    binary[18] = 183;
    fs::write(root.join("apsides-controller"), &binary)?;
    files.insert("apsides-controller".into(), hash(&binary));
    let image_digest = write_oci(root, &binary, &plan.ir_digest)?;
    files.insert(
        "controller.oci.tar".into(),
        hash(&fs::read(root.join("controller.oci.tar"))?),
    );
    let manifest = json!({"schemaVersion":"apsides.dev/release/v2","mode":"production","irDigest":plan.ir_digest,"image":{"reference":format!("example.invalid/controller@{}",image_digest),"platform":"linux/arm64"},"source":{"repository":"https://github.com/example/apsides","commit":"a".repeat(40),"inputsDigest":digest(&inputs)?},"compilerDigest":compiler,"binaryDigest":hash(&binary),"files":files});
    let bytes = write_json(root, "release.json", &manifest)?;
    write_json(
        root,
        "provenance.json",
        &json!({"_type":"https://in-toto.io/Statement/v1","predicateType":"https://slsa.dev/provenance/v1","subject":[{"name":"release.json","digest":{"sha256":hash(&bytes).trim_start_matches("sha256:")}}],"predicate":{"buildDefinition":{"externalParameters":{"repository":manifest["source"]["repository"],"commit":manifest["source"]["commit"],"compilerDigest":compiler}}}}),
    )?;
    let key = root.join("key");
    command(&[
        "-q",
        "-t",
        "ed25519",
        "-N",
        "",
        "-f",
        key.to_str().ok_or("path is not valid UTF-8")?,
    ])?;
    command(&[
        "-Y",
        "sign",
        "-f",
        key.to_str().ok_or("path is not valid UTF-8")?,
        "-n",
        "apsides.provenance.v1",
        root.join("provenance.json")
            .to_str()
            .ok_or("path is not valid UTF-8")?,
    ])?;
    fs::rename(
        root.join("provenance.json.sig"),
        root.join("provenance.sig"),
    )?;
    Ok(TrustPolicy {
        schema_version: "apsides.dev/trust/v1".into(),
        repository: "https://github.com/example/apsides".into(),
        approved_compiler_digests: vec![compiler],
        verifier: VerifierTool {
            executable: "/usr/bin/ssh-keygen".into(),
            sha256: hash(&fs::read("/usr/bin/ssh-keygen")?),
        },
        signature: SignaturePolicy::OpenSsh {
            principal: "release@example.invalid".into(),
            allowed_signers: format!(
                "release@example.invalid {}",
                fs::read_to_string(root.join("key.pub"))?
            ),
        },
    })
}

#[tokio::test]
async fn actual_openssh_signature_is_required_and_policy_bound() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let verified = verify_candidate(root.path(), &policy).await?;
    assert_eq!(verified.manifest().source.repository, policy.repository);
    let mut wrong = policy.clone();
    if let SignaturePolicy::OpenSsh { principal, .. } = &mut wrong.signature {
        *principal = "intruder@example.invalid".into();
    }
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.approved_compiler_digests = vec![hash(b"different compiler")];
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.verifier.sha256 = hash(b"different tool");
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    fs::write(root.path().join("provenance.sig"), b"not a signature")?;
    assert!(verify_candidate(root.path(), &policy).await.is_err());
    Ok(())
}

/// A `build-inputs.json` over `compiler_core::MAX_IR_BYTES` (4 MiB) -- as a
/// real one now is, once `crates/engine`'s `deno_core` dependency inflates
/// the vendored source closure `compilerClosure.nativeCompiler.
/// buildContextFiles` records into the thousands of files -- must still
/// verify: it has its own, larger budget (`MAX_BUILD_INPUTS_BYTES`), not
/// IR's.
#[tokio::test]
async fn an_oversized_but_valid_build_inputs_document_still_verifies() -> TestResult {
    let root = tempfile::tempdir()?;
    let padding_entries = 30_000;
    let policy = release_with_padding(root.path(), padding_entries)?;
    let build_inputs_bytes = fs::metadata(root.path().join("build-inputs.json"))?.len();
    assert!(
        build_inputs_bytes > compiler_core::MAX_IR_BYTES as u64,
        "fixture must actually exceed the IR budget to exercise the fix; got {build_inputs_bytes} bytes"
    );
    assert!(
        build_inputs_bytes < compiler_core::MAX_BUILD_INPUTS_BYTES as u64,
        "fixture must still fit build-inputs.json's own budget; got {build_inputs_bytes} bytes"
    );
    let verified = verify_candidate(root.path(), &policy).await?;
    assert_eq!(verified.manifest().source.repository, policy.repository);
    Ok(())
}

/// A `build-inputs.json` over even its own, larger budget
/// (`MAX_BUILD_INPUTS_BYTES`, matching this crate's own `MAX_DOCUMENT`) is
/// still rejected here -- at the raw byte-read cap the release verifier
/// already applied to every document before this change, not a newly
/// misleading "IR exceeds 4 MiB" for a document that was never IR.
/// `crates/cli/src/commands/release.rs`'s own `read_build_inputs` (unit
/// tests in that file) is the parse-time bound named specifically after
/// `build-inputs.json`, reached because that reader has no separate raw
/// byte-read cap ahead of it the way this crate's `bounded_read` does.
#[tokio::test]
async fn a_build_inputs_document_over_its_own_larger_budget_is_still_rejected() -> TestResult {
    let root = tempfile::tempdir()?;
    // ~152 bytes/padding entry; comfortably over MAX_BUILD_INPUTS_BYTES (8 MiB).
    let policy = release_with_padding(root.path(), 60_000)?;
    let build_inputs_bytes = fs::metadata(root.path().join("build-inputs.json"))?.len();
    assert!(
        build_inputs_bytes > compiler_core::MAX_BUILD_INPUTS_BYTES as u64,
        "fixture must actually exceed build-inputs.json's own budget; got {build_inputs_bytes} bytes"
    );
    let error = verify_candidate(root.path(), &policy)
        .await
        .err()
        .ok_or("expected an oversized build-inputs.json to be rejected")?
        .to_string();
    assert!(error.contains("exceeds"), "{error}");
    assert!(!error.contains("IR exceeds"), "{error}");
    Ok(())
}

#[tokio::test]
async fn altered_payload_and_unsigned_manifest_cannot_be_promoted() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    fs::write(root.path().join("sbom.json"), b"{\"changed\":true}")?;
    assert!(verify_candidate(root.path(), &policy).await.is_err());
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    manifest["mode"] = json!("fixture");
    write_json(root.path(), "release.json", &manifest)?;
    assert_eq!(rejection(root.path(), &policy).await?, FIXTURE_REJECTED);
    Ok(())
}

fn resign(root: &Path) -> TestResult {
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("release.json"))?)?;
    let mut statement: Value = serde_json::from_slice(&fs::read(root.join("provenance.json"))?)?;
    statement["subject"][0]["digest"]["sha256"] =
        json!(hash(&fs::read(root.join("release.json"))?).trim_start_matches("sha256:"));
    statement["predicate"]["buildDefinition"]["externalParameters"]["compilerDigest"] =
        manifest["compilerDigest"].clone();
    write_json(root, "provenance.json", &statement)?;
    command(&[
        "-Y",
        "sign",
        "-f",
        root.join("key").to_str().ok_or("path is not valid UTF-8")?,
        "-n",
        "apsides.provenance.v1",
        root.join("provenance.json")
            .to_str()
            .ok_or("path is not valid UTF-8")?,
    ])?;
    fs::rename(
        root.join("provenance.json.sig"),
        root.join("provenance.sig"),
    )?;
    Ok(())
}

#[tokio::test]
async fn embedded_fixture_marker_survives_manifest_mode_changes() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let mut ir: Value = serde_json::from_slice(&fs::read(root.path().join("ir.json"))?)?;
    ir["testOnly"] = json!(true);
    let compiled = parse_and_validate(&ir.to_string())?;
    let ir_bytes = write_json(root.path(), "ir.json", &ir)?;
    let plan_bytes = write_json(root.path(), "plan.json", &serde_json::to_value(&compiled)?)?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    manifest["files"]["ir.json"] = json!(hash(&ir_bytes));
    manifest["files"]["plan.json"] = json!(hash(&plan_bytes));
    manifest["irDigest"] = json!(compiled.ir_digest);
    let binary = fs::read(root.path().join("apsides-controller"))?;
    let image = write_oci(root.path(), &binary, &compiled.ir_digest)?;
    manifest["image"]["reference"] = json!(format!("example.invalid/controller@{image}"));
    manifest["files"]["controller.oci.tar"] =
        json!(hash(&fs::read(root.path().join("controller.oci.tar"))?));

    for (name, value) in compiler_core::review_documents(&compiled) {
        let bytes = write_json(root.path(), name, &value)?;
        manifest["files"][name] = json!(hash(&bytes));
    }

    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    assert_eq!(rejection(root.path(), &policy).await?, MARKER_REQUIRED);
    // Declaring the release a fixture passes payload validation, and the
    // signed fixture release is still refused.
    manifest["mode"] = json!("fixture");
    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    assert_eq!(rejection(root.path(), &policy).await?, FIXTURE_REJECTED);
    Ok(())
}

/// A `surface.json` edited to claim a risk the compiler never derived is
/// rejected even when the tamperer keeps `release.json`'s recorded digest
/// and the outer signature self-consistent: `read_release` recomputes the
/// surface from the trusted IR rather than trusting whatever is on disk.
#[tokio::test]
async fn tampered_surface_document_is_rejected_even_when_resigned() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let mut surface: Value = serde_json::from_slice(&fs::read(root.path().join("surface.json"))?)?;
    surface["risks"] = json!(["fabricated-risk-not-derived-by-the-compiler"]);
    let bytes = write_json(root.path(), "surface.json", &surface)?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    manifest["files"]["surface.json"] = json!(hash(&bytes));
    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    assert_eq!(
        rejection(root.path(), &policy).await?,
        "release rejected: release review document differs from trusted derivation: surface.json"
    );
    Ok(())
}

fn write_oci(root: &Path, binary: &[u8], ir_digest: &str) -> TestResult<String> {
    write_oci_with_base(root, binary, ir_digest, Base::Approved)
}

/// How a test image carries the approved runtime base.
#[derive(Clone, Copy, PartialEq)]
enum Base {
    /// The base layer exactly as the approved config's diff ID names it.
    Approved,
    /// The same base content, stored gzip-compressed as a registry would.
    Compressed,
    /// An unapproved layer beneath the approved base.
    ExtraLayer,
}

fn gzip(bytes: &[u8]) -> TestResult<Vec<u8>> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

fn write_oci_with_base(
    root: &Path,
    binary: &[u8],
    ir_digest: &str,
    variant: Base,
) -> TestResult<String> {
    let extra_base = variant == Base::ExtraLayer;
    let uncompressed = tar(vec![])?;
    let (base, base_type) = if variant == Base::Compressed {
        (
            gzip(&uncompressed)?,
            "application/vnd.oci.image.layer.v1.tar+gzip",
        )
    } else {
        (
            uncompressed.clone(),
            "application/vnd.oci.image.layer.v1.tar",
        )
    };
    let foreign = tar(vec![("etc/foreign-runtime", b"unapproved", false)])?;
    let layer = tar(vec![
        ("usr", b"", true),
        ("usr/local", b"", true),
        ("usr/local/bin", b"", true),
        ("usr/local/bin/apsides-controller", binary, false),
    ])?;
    let mut diff_ids = vec![hash(&uncompressed)];
    let mut layers = vec![json!({"mediaType":base_type,"digest":hash(&base),"size":base.len()})];
    if extra_base {
        diff_ids.insert(0, hash(&foreign));
        layers.insert(0, json!({"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&foreign),"size":foreign.len()}));
    }
    diff_ids.push(hash(&layer));
    layers.push(json!({"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&layer),"size":layer.len()}));
    let config = canonical_bytes(
        &json!({"architecture":"arm64","os":"linux","config":{"Entrypoint":["/usr/local/bin/apsides-controller"],"User":"65532:65532","WorkingDir":"/","Labels":{"dev.apsides.ir-digest":ir_digest,"dev.apsides.release-status":"blocked","org.opencontainers.image.title":"Apsides generated controller"}},"rootfs":{"type":"layers","diff_ids":diff_ids}}),
    )?;
    let manifest = canonical_bytes(
        &json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},"layers":layers}),
    )?;
    let index = canonical_bytes(
        &json!({"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":hash(&manifest),"size":manifest.len()}]}),
    )?;
    let layer_path = format!("blobs/sha256/{}", &hash(&layer)[7..]);
    let base_path = format!("blobs/sha256/{}", &hash(&base)[7..]);
    let foreign_path = format!("blobs/sha256/{}", &hash(&foreign)[7..]);
    let config_path = format!("blobs/sha256/{}", &hash(&config)[7..]);
    let manifest_path = format!("blobs/sha256/{}", &hash(&manifest)[7..]);
    let mut entries = vec![
        (
            "oci-layout",
            b"{\"imageLayoutVersion\":\"1.0.0\"}".as_slice(),
            false,
        ),
        ("index.json", index.as_slice(), false),
        (base_path.as_str(), base.as_slice(), false),
    ];
    if extra_base {
        entries.push((foreign_path.as_str(), foreign.as_slice(), false));
    }
    entries.extend([
        (layer_path.as_str(), layer.as_slice(), false),
        (config_path.as_str(), config.as_slice(), false),
        (manifest_path.as_str(), manifest.as_slice(), false),
    ]);
    fs::write(root.join("controller.oci.tar"), tar(entries)?)?;
    Ok(hash(&manifest))
}

/// `padding_entries` extra, unique, correctly sorted `vendor/` file entries,
/// so a test can build a `build-inputs.json` document as large as a real
/// one gets once `crates/engine`'s `deno_core` inflates the vendored
/// dependency closure into the thousands of files -- without needing a real
/// `cargo vendor` run.
fn compiler_inputs(padding_entries: usize) -> TestResult<(Value, String)> {
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
    // Sorts after "vendor/example/..." ('e' < 's') and before
    // "workspace/..." ('v' < 'w'), keeping buildContextFiles sorted as
    // validate_files requires.
    for index in 0..padding_entries {
        build_context_files.push(json!({
            "path": format!("vendor/synthetic-package-{index:06}/src/generated/module.rs"),
            "sha256": format!("sha256:{index:064x}"),
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
    let cli: Vec<Value> = packaging_files
        .as_array()
        .ok_or("expected a JSON array")?
        .iter()
        .filter(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("crates/cli/"))
        })
        .cloned()
        .collect();
    let approved_runtime = approved_runtime()?;
    let host_tools: Vec<_> = ["buildx", "cargo", "docker", "python", "apsides", "sbom"]
        .into_iter()
        .map(|role| {
            json!({"role":role,"sha256":format!("sha256:{}", "6".repeat(64)),"version":"1.0"})
        })
        .collect();
    let rusty_v8 = json!({
        "archive": {
            "name": "librusty_v8", "version": "150.4.0", "target": "aarch64-unknown-linux-gnu",
            "url": "https://github.com/denoland/rusty_v8/releases/download/v150.4.0/librusty_v8_simdutf_release_aarch64-unknown-linux-gnu.a.gz",
            "sha256": format!("sha256:{}", "9".repeat(64)), "path": "rusty-v8/librusty_v8.a.gz",
        },
        "bindingFile": Value::Null,
    });
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

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn signed_binary_must_be_the_image_entrypoint_payload() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let mut binary = fs::read(root.path().join("apsides-controller"))?;
    binary[32] = 1;
    fs::write(root.path().join("apsides-controller"), &binary)?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    manifest["binaryDigest"] = json!(hash(&binary));
    manifest["files"]["apsides-controller"] = json!(hash(&binary));
    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    assert!(
        verify_candidate(root.path(), &policy)
            .await
            .err()
            .ok_or("expected this call to fail")?
            .to_string()
            .contains("OCI controller bytes")
    );
    Ok(())
}

#[tokio::test]
async fn signed_image_cannot_add_an_unapproved_runtime_base_layer() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let binary = fs::read(root.path().join("apsides-controller"))?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    let image = write_oci_with_base(
        root.path(),
        &binary,
        manifest["irDigest"]
            .as_str()
            .ok_or("expected a JSON string")?,
        Base::ExtraLayer,
    )?;
    manifest["image"]["reference"] = json!(format!("example.invalid/controller@{image}"));
    manifest["files"]["controller.oci.tar"] =
        json!(hash(&fs::read(root.path().join("controller.oci.tar"))?));
    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    let error = verify_candidate(root.path(), &policy)
        .await
        .err()
        .ok_or("expected this call to fail")?
        .to_string();
    assert!(error.contains("approved runtime layer chain"), "{error}");
    Ok(())
}

/// Base layers are identified by diff ID, so an image whose base layer is
/// stored compressed (the containerd store) verifies like one whose base is
/// stored uncompressed (the classic store).
#[tokio::test]
async fn approved_base_may_be_stored_compressed() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let binary = fs::read(root.path().join("apsides-controller"))?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    let image = write_oci_with_base(
        root.path(),
        &binary,
        manifest["irDigest"]
            .as_str()
            .ok_or("expected a JSON string")?,
        Base::Compressed,
    )?;
    manifest["image"]["reference"] = json!(format!("example.invalid/controller@{image}"));
    manifest["files"]["controller.oci.tar"] =
        json!(hash(&fs::read(root.path().join("controller.oci.tar"))?));
    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    verify_candidate(root.path(), &policy).await?;
    Ok(())
}

#[tokio::test]
async fn production_requires_explicit_marker_even_with_valid_signature() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let mut ir: Value = serde_json::from_slice(&fs::read(root.path().join("ir.json"))?)?;
    ir.as_object_mut()
        .ok_or("expected a JSON object")?
        .remove("testOnly");
    let bytes = write_json(root.path(), "ir.json", &ir)?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    manifest["files"]["ir.json"] = json!(hash(&bytes));
    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    assert_eq!(rejection(root.path(), &policy).await?, MARKER_REQUIRED);
    Ok(())
}

#[tokio::test]
#[ignore = "requires real ORAS and a disposable TLS registry; verifier fixture only"]
async fn signed_oci_release_resolves_after_source_workspace_is_removed()
-> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let oras = std::path::PathBuf::from(std::env::var("APSIDES_TEST_ORAS")?);
    let ca = std::path::PathBuf::from(std::env::var("APSIDES_TEST_REGISTRY_CA")?);
    let repository = std::env::var("APSIDES_TEST_OCI_REPOSITORY")?;
    // Validate the destination syntax using an immutable dummy reference before
    // publishing the test fixture to the explicitly configured disposable registry.
    release::ReleaseReference {
        oci: format!("{repository}@{}", hash(b"reference syntax")),
        release_digest: hash(b"release syntax"),
    }
    .validate()?;
    let source = tempfile::tempdir()?;
    let policy = release(source.path())?;
    let manifest_bytes = fs::read(source.path().join("release.json"))?;
    let (manifest, _, _) = release::read_release(source.path())?;
    let mut files: Vec<String> = manifest.files.keys().cloned().collect();
    files.extend([
        "release.json".into(),
        "provenance.json".into(),
        "provenance.sig".into(),
    ]);
    assert!(!files.iter().any(|name| name == "key" || name == "key.pub"));
    let output = Command::new(&oras)
        .current_dir(source.path())
        .args([
            "push",
            "--plain-http=false",
            "--no-tty",
            "--format",
            "json",
            "--ca-file",
        ])
        .arg(&ca)
        .arg(format!("{repository}:signed-verifier-fixture"))
        .args(files)
        .output()?;
    if !output.status.success() {
        return Err("isolated fixture publication failed".into());
    }
    let pushed: Value = serde_json::from_slice(&output.stdout)?;
    let reference = release::ReleaseReference {
        oci: pushed
            .get("reference")
            .and_then(Value::as_str)
            .ok_or("ORAS did not return an immutable reference")?
            .into(),
        release_digest: hash(&manifest_bytes),
    };
    let original = source.path().to_owned();
    source.close()?;
    assert!(!original.exists());
    let tool = release::OrasTool {
        sha256: hash(&fs::read(&oras)?),
        executable: oras,
        ca_file: Some(ca),
    };
    let fetched = release::fetch_release(&reference, &tool).await?;
    let verified = release::verify_candidate(fetched.path(), &policy).await?;
    assert_eq!(verified.release_digest(), reference.release_digest);
    assert!(!fetched.path().join("key").exists());
    assert!(!fetched.path().join("key.pub").exists());
    let mut wrong_policy = policy;
    if let SignaturePolicy::OpenSsh { principal, .. } = &mut wrong_policy.signature {
        *principal = "unapproved@example.invalid".into();
    }
    assert!(
        release::verify_candidate(fetched.path(), &wrong_policy)
            .await
            .is_err()
    );
    let download = fetched.path().to_owned();
    drop(fetched);
    assert!(!download.exists());
    Ok(())
}
