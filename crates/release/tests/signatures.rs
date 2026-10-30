use compiler_core::{canonical_bytes, digest, parse_and_validate};
use release::{PROVENANCE_BUNDLE, TrustPolicy, verify_candidate};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};
use test_sigstore::{Authority, Workload};

#[path = "support/closure.rs"]
mod closure;

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
const REPOSITORY: &str = "https://github.com/example/apsides";

/// The GitHub Actions run that signs every test release.
fn workload() -> Workload {
    Workload::github(REPOSITORY, &"a".repeat(40))
}

/// The trust policy approving `workload`'s signatures under `authority`, as
/// the platform would store it.
fn policy_document(authority: &Authority, compiler: &str) -> TestResult<Value> {
    Ok(json!({
        "schemaVersion": "apsides.dev/trust/v2",
        "repository": REPOSITORY,
        "approvedCompilerDigests": [compiler],
        "signature": workload().signature_policy(&authority.trusted_root()?),
    }))
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
    let manifest = json!({"schemaVersion":"apsides.dev/release/v2","mode":"production","irDigest":plan.ir_digest,"image":{"reference":format!("example.invalid/controller@{}",image_digest),"platform":"linux/arm64"},"source":{"repository":REPOSITORY,"commit":"a".repeat(40),"inputsDigest":digest(&inputs)?},"compilerDigest":compiler,"binaryDigest":hash(&binary),"files":files});
    write_json(root, "release.json", &manifest)?;
    let authority = Authority::shared()?;
    authority.sign_release(&workload(), root)?;
    Ok(serde_json::from_value(policy_document(
        authority, &compiler,
    )?)?)
}

#[tokio::test]
async fn sigstore_signature_is_required_and_policy_bound() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let verified = verify_candidate(root.path(), &policy).await?;
    assert_eq!(verified.manifest().source.repository, policy.repository);
    assert_eq!(
        verified.provenance_digest(),
        hash(&fs::read(root.path().join(PROVENANCE_BUNDLE))?)
    );
    // Activation compares the staged digest with the stored document's.
    let stored = policy_document(Authority::shared()?, &policy.approved_compiler_digests[0])?;
    assert_eq!(verified.trust_policy_digest(), digest(&stored)?);

    let mut wrong = policy.clone();
    wrong.signature.certificate_identity =
        format!("{REPOSITORY}/.github/workflows/other.yml@refs/heads/main");
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.signature.certificate_issuer = "https://accounts.example.invalid".into();
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.signature.signer_digest = "b".repeat(40);
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.signature.source_ref = "refs/heads/release".into();
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.approved_compiler_digests = vec![hash(b"different compiler")];
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    // The authority issues no signed certificate timestamps.
    wrong = policy.clone();
    wrong.signature.certificate_transparency = true;
    wrong.signature.trusted_root["ctlogs"] = wrong.signature.trusted_root["tlogs"].clone();
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    // A policy whose root lacks the authority a requirement needs.
    wrong = policy.clone();
    wrong.signature.certificate_transparency = true;
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.signature.transparency_log = false;
    assert!(verify_candidate(root.path(), &wrong).await.is_err());
    // Another instance's root does not chain this signature.
    wrong = policy.clone();
    wrong.signature.trusted_root = Authority::new()?.trusted_root()?;
    assert!(verify_candidate(root.path(), &wrong).await.is_err());

    let bundle: Value = serde_json::from_slice(&fs::read(root.path().join(PROVENANCE_BUNDLE))?)?;
    let mut unlogged = bundle.clone();
    unlogged["verificationMaterial"]["tlogEntries"] = json!([]);
    fs::write(
        root.path().join(PROVENANCE_BUNDLE),
        serde_json::to_vec(&unlogged)?,
    )?;
    assert!(verify_candidate(root.path(), &policy).await.is_err());
    fs::write(root.path().join(PROVENANCE_BUNDLE), b"not a bundle")?;
    assert!(verify_candidate(root.path(), &policy).await.is_err());
    fs::remove_file(root.path().join(PROVENANCE_BUNDLE))?;
    assert!(verify_candidate(root.path(), &policy).await.is_err());
    Ok(())
}

#[tokio::test]
async fn every_workload_claim_must_match() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let authority = Authority::shared()?;
    for change in [
        |w: &mut Workload| w.runner_environment = "self-hosted".into(),
        |w: &mut Workload| w.repository = "https://github.com/example/fork".into(),
        |w: &mut Workload| w.commit = "b".repeat(40),
        |w: &mut Workload| w.signer_digest = "b".repeat(40),
        |w: &mut Workload| w.source_ref = "refs/heads/feature".into(),
        |w: &mut Workload| w.issuer = "https://accounts.example.invalid".into(),
    ] {
        let mut signer = workload();
        change(&mut signer);
        authority.sign_release(&signer, root.path())?;
        assert!(verify_candidate(root.path(), &policy).await.is_err());
    }
    authority.sign_release(&workload(), root.path())?;
    verify_candidate(root.path(), &policy).await?;
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
    Authority::shared()?.sign_release(&workload(), root)
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

/// A `surface.json` that is semantically identical to what the compiler
/// derives -- same parsed value, just pretty-printed rather than canonical
/// -- is still rejected. The value-equality check above alone would accept
/// it (it compares parsed JSON), so this exercises the separate,
/// canonical-bytes digest check `aps deploy`'s independent surface
/// recomputation relies on: `manifest.files["surface.json"]` must equal
/// `compiler_core::digest` of the derived surface, not merely a digest of
/// *some* byte encoding of an equal value.
#[tokio::test]
async fn noncanonical_surface_document_is_rejected_even_when_semantically_equal() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path())?;
    let surface: Value = serde_json::from_slice(&fs::read(root.path().join("surface.json"))?)?;
    let bytes = serde_json::to_vec_pretty(&surface)?;
    assert_ne!(
        bytes,
        canonical_bytes(&surface)?,
        "pretty-printed bytes must actually differ from canonical bytes"
    );
    fs::write(root.path().join("surface.json"), &bytes)?;
    let mut manifest: Value = serde_json::from_slice(&fs::read(root.path().join("release.json"))?)?;
    manifest["files"]["surface.json"] = json!(hash(&bytes));
    write_json(root.path(), "release.json", &manifest)?;
    resign(root.path())?;
    assert_eq!(
        rejection(root.path(), &policy).await?,
        "release rejected: release surface.json is not canonical JSON"
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
    // Sorts after "vendor/example/..." ('e' < 's') and before
    // "workspace/..." ('v' < 'w'), keeping buildContextFiles sorted as
    // validate_files requires.
    let padding = (0..padding_entries)
        .map(|index| {
            json!({
                "path": format!("vendor/synthetic-package-{index:06}/src/generated/module.rs"),
                "sha256": format!("sha256:{index:064x}"),
            })
        })
        .collect();
    closure::inputs(padding, &approved_runtime()?)
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
