use compiler_core::{canonical_bytes, digest, parse_and_validate};
use release::{SignaturePolicy, TrustPolicy, VerifierTool, verify_candidate, verify_release};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
fn assertion(id: &str, expected: Value, actual: Value) -> Value {
    let mut value = serde_json::Map::new();
    value.insert("checkId".into(), Value::String(id.into()));
    value.insert("expected".into(), expected);
    value.insert("actual".into(), actual);
    value.insert("passed".into(), Value::Bool(true));
    Value::Object(value)
}
fn case_assertions(scenario: &str) -> Vec<Value> {
    let boolean = |id: &str, value: bool| assertion(id, json!(value), json!(value));
    let zero = |id: &str| assertion(id, json!(0), json!(0));
    match scenario {
        "initial-evaluation" => vec![
            boolean("revision-present", true),
            boolean("target-converged", true),
        ],
        "projected-change" => vec![
            boolean("revision-changed", true),
            boolean("target-converged", true),
        ],
        "irrelevant-change" | "same-value-replay" | "accepted-write-restart" => vec![
            boolean("revision-unchanged", true),
            zero("template-mutations"),
        ],
        "coalesced-inputs" | "restart-and-relist" => vec![
            boolean("latest-input-set-used", true),
            boolean("target-converged", true),
        ],
        "missing-input" => vec![
            assertion("block-reason", json!("missing"), json!("missing")),
            zero("target-mutations"),
            boolean("target-preserved", true),
        ],
        "input-returned" => vec![
            boolean("dependency-recovered", true),
            boolean("target-converged", true),
        ],
        "unauthorized-input" => vec![
            assertion("block-reason", json!("unauthorized"), json!("unauthorized")),
            zero("target-mutations"),
        ],
        "credential-read-denial" => vec![
            assertion("http-status", json!(403), json!(403)),
            boolean("permission-expanded", false),
            zero("target-mutations"),
        ],
        "unavailable-input" => vec![
            assertion("block-reason", json!("unavailable"), json!("unavailable")),
            zero("target-mutations"),
        ],
        "invalid-input" => vec![
            assertion("block-reason", json!("invalid"), json!("invalid")),
            boolean("sensitive-data-exposed", false),
            zero("target-mutations"),
        ],
        "field-conflict" => vec![
            boolean("conflict-reported", true),
            boolean("force-applied", false),
            boolean("foreign-field-preserved", true),
        ],
        "out-of-scope-change" => vec![
            assertion("http-status", json!(403), json!(403)),
            zero("out-of-scope-mutations"),
        ],
        "rollout-readiness" => vec![
            assertion("ready-pods", json!(">=1"), json!(1)),
            boolean("new-template-ready", true),
        ],
        other => panic!("missing test assertion fixture for {other}"),
    }
}
fn check_assertions(check: &str) -> Vec<Value> {
    let boolean = |id: &str, value: bool| assertion(id, json!(value), json!(value));
    let zero = |id: &str| assertion(id, json!(0), json!(0));
    match check {
        "reproducible-payload" => vec![
            assertion("independent-builds", json!(2), json!(2)),
            boolean("payload-digests-equal", true),
        ],
        "declared-input-isolation" => vec![
            assertion("sandbox-engine", json!("SES"), json!("SES")),
            boolean("undeclared-input-rejected", true),
        ],
        "offline-reactor-suite" => vec![
            boolean("external-network-disabled", true),
            zero("required-cases-failed"),
            zero("required-cases-unsupported"),
        ],
        "secret-canary-scan" => vec![
            zero("canary-occurrences"),
            assertion("files-scanned", json!(">=1"), json!(1)),
        ],
        "reaction-mutation-check" => vec![boolean("removed-reaction-contribution-detected", true)],
        "scope-matched-benchmark" => vec![
            boolean("measurements-present", true),
            boolean("scope-matched", true),
        ],
        "deployment-ceiling-rejection" => vec![
            boolean("above-ceiling-rejected", true),
            boolean("next-writer-started", false),
        ],
        "ownership-collision" => vec![
            boolean("claim-collision-rejected", true),
            boolean("next-writer-started", false),
        ],
        "permission-changing-upgrade" => vec![
            boolean("permission-change-reviewed", true),
            boolean("unapproved-writer-started", false),
        ],
        "phase-interruption-recovery" => vec![
            boolean("planned-interruption-recovered", true),
            boolean("staged-interruption-recovered", true),
            boolean("quiesced-interruption-recovered", true),
            boolean("activated-interruption-recovered", true),
            boolean("cleaning-interruption-recovered", true),
            boolean("committed-interruption-recovered", true),
            boolean("unsafe-activation", false),
        ],
        "last-api-type-cleanup" => vec![
            boolean("last-api-type-cleanup-complete", true),
            boolean("wrong-object-deleted", false),
        ],
        "uid-replacement-rejection" => vec![
            boolean("replacement-deleted", false),
            boolean("uid-mismatch-rejected", true),
        ],
        "finalizer-preservation" => vec![
            boolean("finalizer-block-reported", true),
            boolean("finalizers-forced", false),
        ],
        "uncertain-writer-blocks" => vec![
            boolean("activation-blocked", true),
            boolean("unsafe-activation", false),
        ],
        "fixture-promotion-rejection" => vec![boolean("fixture-release-rejected", true)],
        "api-bound-gap-conformance" => vec![
            boolean("actual-credential-tests-passed", true),
            boolean("undisclosed-gap", false),
        ],
        other => panic!("missing release-check assertion fixture for {other}"),
    }
}
fn write_json(root: &Path, name: &str, value: &Value) -> Vec<u8> {
    let bytes = canonical_bytes(value).unwrap();
    fs::write(root.join(name), &bytes).unwrap();
    bytes
}

fn tar(files: Vec<(&str, &[u8], bool)>) -> Vec<u8> {
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
        archive.append_data(&mut header, path, bytes).unwrap();
    }
    archive.into_inner().unwrap()
}

fn approved_runtime() -> Value {
    let layer = tar(vec![]);
    let config = canonical_bytes(&json!({
        "architecture":"arm64","os":"linux","config":{},
        "rootfs":{"type":"layers","diff_ids":[hash(&layer)]}
    }))
    .unwrap();
    let runtime_manifest = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&layer),"size":layer.len()}]
    }))
    .unwrap();
    let index = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
        "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":hash(&runtime_manifest),"size":runtime_manifest.len(),"platform":{"os":"linux","architecture":"arm64"}}]
    }))
    .unwrap();
    json!({
        "baseImage":{"reference":format!("example.invalid/runtime@{}",hash(&index)),"platform":"linux/arm64"},
        "indexJson":String::from_utf8(index).unwrap(),
        "manifestJson":String::from_utf8(runtime_manifest).unwrap(),
        "configJson":String::from_utf8(config).unwrap()
    })
}
fn command(args: &[&str]) {
    let output = Command::new("/usr/bin/ssh-keygen")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn release(root: &Path) -> TrustPolicy {
    assert!(
        fs::create_dir(root.join("evidence")).is_ok(),
        "test evidence directory creation failed"
    );
    let ir = json!({
        "schemaVersion":"apsides.dev/ir/v1","testOnly":false,
        "domain":{"id":"example","namespace":"workload","controlNamespace":"apsides-system","clusterBinding":"test-cluster","policyRef":"platform/example","enforcement":"api-bound","budgets":{"maxResources":64,"maxInputs":64,"maxReactions":64,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"config","namespace":"workload","desired":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"config","namespace":"workload"},"data":{"hello":"world"}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"controller.ts","line":1,"column":1}}],
        "inputs":[],"reactions":[],
        "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"controller.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    });
    let plan = parse_and_validate(&ir.to_string()).unwrap();
    let (inputs, compiler) = compiler_inputs();
    // These are verifier test payloads, not reactor-suite acceptance evidence.
    let mut files = BTreeMap::new();
    for (name, value) in [
        ("ir.json", ir),
        ("plan.json", serde_json::to_value(&plan).unwrap()),
        ("build-inputs.json", inputs.clone()),
        ("source-map.json", json!({})),
        ("sbom.json", json!({})),
        ("compatibility.json", json!({})),
        ("graphs.json", json!({})),
        ("test-plan.json", json!([])),
        (
            "test-evidence.json",
            json!({"purpose":"signature verifier test only"}),
        ),
    ] {
        files.insert(name.to_owned(), hash(&write_json(root, name, &value)));
    }
    for (name, value) in compiler_core::review_documents(&plan) {
        files.insert(name.into(), hash(&write_json(root, name, &value)));
    }
    files.insert(
        "test-plan.json".into(),
        hash(&write_json(
            root,
            "test-plan.json",
            &serde_json::to_value(&plan.test_plan).unwrap(),
        )),
    );
    // Minimal architecture header exercises metadata verification, not execution.
    let mut binary = vec![0_u8; 64];
    binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
    binary[18] = 183;
    fs::write(root.join("apsides-controller"), &binary).unwrap();
    files.insert("apsides-controller".into(), hash(&binary));
    let image_digest = write_oci(root, &binary, &plan.ir_digest);
    files.insert(
        "controller.oci.tar".into(),
        hash(&fs::read(root.join("controller.oci.tar")).unwrap()),
    );
    add_evidence(root, &plan, &mut files);
    let mut manifest = json!({"schemaVersion":"apsides.dev/release/v1","mode":"production","irDigest":plan.ir_digest,"image":{"reference":format!("example.invalid/controller@{}",image_digest),"platform":"linux/arm64"},"source":{"repository":"https://github.com/example/apsides","commit":"a".repeat(40),"inputsDigest":digest(&inputs).unwrap()},"compilerDigest":compiler,"binaryDigest":hash(&binary),"files":files});
    add_production_evidence(root, &mut manifest);
    let bytes = write_json(root, "release.json", &manifest);
    write_json(
        root,
        "provenance.json",
        &json!({"_type":"https://in-toto.io/Statement/v1","predicateType":"https://slsa.dev/provenance/v1","subject":[{"name":"release.json","digest":{"sha256":hash(&bytes).trim_start_matches("sha256:")}}],"predicate":{"buildDefinition":{"externalParameters":{"repository":manifest["source"]["repository"],"commit":manifest["source"]["commit"],"compilerDigest":compiler}}}}),
    );
    let key = root.join("key");
    command(&["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()]);
    command(&[
        "-Y",
        "sign",
        "-f",
        key.to_str().unwrap(),
        "-n",
        "apsides.provenance.v1",
        root.join("provenance.json").to_str().unwrap(),
    ]);
    fs::rename(
        root.join("provenance.json.sig"),
        root.join("provenance.sig"),
    )
    .unwrap();
    TrustPolicy {
        schema_version: "apsides.dev/trust/v1".into(),
        repository: "https://github.com/example/apsides".into(),
        approved_compiler_digests: vec![compiler],
        verifier: VerifierTool {
            executable: "/usr/bin/ssh-keygen".into(),
            sha256: hash(&fs::read("/usr/bin/ssh-keygen").unwrap()),
        },
        signature: SignaturePolicy::OpenSsh {
            principal: "release@example.invalid".into(),
            allowed_signers: format!(
                "release@example.invalid {}",
                fs::read_to_string(root.join("key.pub")).unwrap()
            ),
        },
    }
}

#[tokio::test]
async fn actual_openssh_signature_is_required_and_policy_bound() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let verified = verify_release(root.path(), &policy).await.unwrap();
    assert_eq!(verified.manifest().source.repository, policy.repository);
    let mut wrong = policy.clone();
    if let SignaturePolicy::OpenSsh { principal, .. } = &mut wrong.signature {
        *principal = "intruder@example.invalid".into();
    }
    assert!(verify_release(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.approved_compiler_digests = vec![hash(b"different compiler")];
    assert!(verify_release(root.path(), &wrong).await.is_err());
    wrong = policy.clone();
    wrong.verifier.sha256 = hash(b"different tool");
    assert!(verify_release(root.path(), &wrong).await.is_err());
    fs::write(root.path().join("provenance.sig"), b"not a signature").unwrap();
    assert!(verify_release(root.path(), &policy).await.is_err());
}

#[tokio::test]
async fn altered_payload_and_unsigned_manifest_cannot_be_promoted() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    fs::write(root.path().join("sbom.json"), b"{\"changed\":true}").unwrap();
    assert!(verify_release(root.path(), &policy).await.is_err());
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("release.json")).unwrap()).unwrap();
    manifest["mode"] = json!("fixture");
    write_json(root.path(), "release.json", &manifest);
    assert!(verify_release(root.path(), &policy).await.is_err());
}

fn resign(root: &Path) {
    let manifest: Value =
        serde_json::from_slice(&fs::read(root.join("release.json")).unwrap()).unwrap();
    let mut statement: Value =
        serde_json::from_slice(&fs::read(root.join("provenance.json")).unwrap()).unwrap();
    statement["subject"][0]["digest"]["sha256"] =
        json!(hash(&fs::read(root.join("release.json")).unwrap()).trim_start_matches("sha256:"));
    statement["predicate"]["buildDefinition"]["externalParameters"]["compilerDigest"] =
        manifest["compilerDigest"].clone();
    write_json(root, "provenance.json", &statement);
    command(&[
        "-Y",
        "sign",
        "-f",
        root.join("key").to_str().unwrap(),
        "-n",
        "apsides.provenance.v1",
        root.join("provenance.json").to_str().unwrap(),
    ]);
    fs::rename(
        root.join("provenance.json.sig"),
        root.join("provenance.sig"),
    )
    .unwrap();
}

#[tokio::test]
async fn approved_signer_cannot_bypass_required_coverage() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let mut coverage: Value =
        serde_json::from_slice(&fs::read(root.path().join("coverage.json")).unwrap()).unwrap();
    coverage["checks"]["offline-reactor-suite"]["status"] = json!("unsupported");
    let bytes = write_json(root.path(), "coverage.json", &coverage);
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("release.json")).unwrap()).unwrap();
    manifest["files"]["coverage.json"] = json!(hash(&bytes));
    write_json(root.path(), "release.json", &manifest);
    resign(root.path());
    let candidate = verify_candidate(root.path(), &policy).await.unwrap();
    assert_eq!(candidate.manifest().ir_digest, manifest["irDigest"]);
    #[cfg(feature = "test-support")]
    candidate
        .authorize_test_staging(release::TestStagingScope {
            cluster_binding: "test-cluster".into(),
            control_namespace: "apsides-system".into(),
            workload_namespace: "workload".into(),
        })
        .unwrap();
    let error = verify_release(root.path(), &policy)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("required release evidence"), "{error}");
}

#[tokio::test]
async fn embedded_fixture_marker_survives_manifest_mode_changes() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let mut ir: Value =
        serde_json::from_slice(&fs::read(root.path().join("ir.json")).unwrap()).unwrap();
    ir["testOnly"] = json!(true);
    let compiled = parse_and_validate(&ir.to_string()).unwrap();
    let ir_bytes = write_json(root.path(), "ir.json", &ir);
    let plan_bytes = write_json(
        root.path(),
        "plan.json",
        &serde_json::to_value(&compiled).unwrap(),
    );
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("release.json")).unwrap()).unwrap();
    manifest["files"]["ir.json"] = json!(hash(&ir_bytes));
    manifest["files"]["plan.json"] = json!(hash(&plan_bytes));
    manifest["irDigest"] = json!(compiled.ir_digest);
    let binary = fs::read(root.path().join("apsides-controller")).unwrap();
    let image = write_oci(root.path(), &binary, &compiled.ir_digest);
    manifest["image"]["reference"] = json!(format!("example.invalid/controller@{image}"));
    manifest["files"]["controller.oci.tar"] = json!(hash(
        &fs::read(root.path().join("controller.oci.tar")).unwrap()
    ));

    for (name, value) in compiler_core::review_documents(&compiled) {
        let bytes = write_json(root.path(), name, &value);
        manifest["files"][name] = json!(hash(&bytes));
    }

    write_json(root.path(), "release.json", &manifest);
    resign(root.path());
    let error = verify_release(root.path(), &policy)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("testOnly:false"), "{error}");
}

fn write_oci(root: &Path, binary: &[u8], ir_digest: &str) -> String {
    write_oci_with_extra_base(root, binary, ir_digest, false)
}

fn write_oci_with_extra_base(
    root: &Path,
    binary: &[u8],
    ir_digest: &str,
    extra_base: bool,
) -> String {
    let base = tar(vec![]);
    let foreign = tar(vec![("etc/foreign-runtime", b"unapproved", false)]);
    let layer = tar(vec![
        ("usr", b"", true),
        ("usr/local", b"", true),
        ("usr/local/bin", b"", true),
        ("usr/local/bin/apsides-controller", binary, false),
    ]);
    let mut diff_ids = vec![hash(&base)];
    let mut layers = vec![
        json!({"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&base),"size":base.len()}),
    ];
    if extra_base {
        diff_ids.insert(0, hash(&foreign));
        layers.insert(0, json!({"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&foreign),"size":foreign.len()}));
    }
    diff_ids.push(hash(&layer));
    layers.push(json!({"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&layer),"size":layer.len()}));
    let config=canonical_bytes(&json!({"architecture":"arm64","os":"linux","config":{"Entrypoint":["/usr/local/bin/apsides-controller"],"User":"65532:65532","WorkingDir":"/","Labels":{"dev.apsides.ir-digest":ir_digest,"dev.apsides.release-status":"blocked","org.opencontainers.image.title":"Apsides generated controller"}},"rootfs":{"type":"layers","diff_ids":diff_ids}})).unwrap();
    let manifest=canonical_bytes(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},"layers":layers})).unwrap();
    let index=canonical_bytes(&json!({"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":hash(&manifest),"size":manifest.len()}]})).unwrap();
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
    fs::write(root.join("controller.oci.tar"), tar(entries)).unwrap();
    hash(&manifest)
}

fn compiler_inputs() -> (Value, String) {
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
    let approved_runtime = approved_runtime();
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

fn add_evidence(
    root: &Path,
    plan: &compiler_core::CompiledPlan,
    files: &mut BTreeMap<String, String>,
) {
    let cases: Vec<_> = plan.test_plan.iter().filter(|c|c.required).map(|case| json!({"caseDigest":digest(&serde_json::to_value(case).unwrap()).unwrap(),"result":{"status":"passed","evidenceFiles":["test-evidence.json"]}})).collect();
    let checks: BTreeMap<_, _> = release::REQUIRED_CHECKS
        .iter()
        .map(|name| {
            (
                *name,
                json!({"status":"passed","evidenceFiles":[if fixture_check(name) { "test-evidence.json" } else { "production-evidence.json" }]}),
            )
        })
        .collect();
    let coverage = json!({"schemaVersion":"apsides.dev/release-coverage/v1","irDigest":plan.ir_digest,"cases":cases,"checks":checks});
    files.insert(
        "coverage.json".into(),
        hash(&write_json(root, "coverage.json", &coverage)),
    );
    let mut fixture = serde_json::to_value(&plan.ir).unwrap();
    fixture["testOnly"] = json!(true);
    let fixture_digest = digest(&fixture).unwrap();
    let overlay = json!({"schemaVersion":"apsides.dev/fixture-overlay/v1","testOnly":true,"productionReady":false,"productionIrSha256":plan.ir_digest.strip_prefix("sha256:"),"fixtureIrSha256":fixture_digest.strip_prefix("sha256:")});
    let artifact = json!({"schemaVersion":"apsides.dev/local-artifact/v1","irDigest":fixture_digest});
    let artifact_bytes = write_json(root, "tested-artifact-manifest.json", &artifact);
    for (name, value) in [
        ("fixture-ir.json", fixture),
        ("fixture-overlay.json", overlay.clone()),
    ] {
        files.insert(name.into(), hash(&write_json(root, name, &value)));
    }
    files.insert(
        "tested-artifact-manifest.json".into(),
        hash(&artifact_bytes),
    );
    let records: BTreeMap<_, _> = plan.test_plan
        .iter()
        .filter(|case| case.required)
        .map(|case| (digest(&serde_json::to_value(case).unwrap()).unwrap(), json!({"status":"passed","assertions":case_assertions(&case.scenario),"evidenceFiles":[]})))
        .collect();
    let check_records: BTreeMap<_, _> = release::REQUIRED_CHECKS
        .iter()
        .filter(|name| fixture_check(name))
        .map(|c| {
            (
                *c,
                json!({"status":"passed","assertions":check_assertions(c),"evidenceFiles":[]}),
            )
        })
        .collect();
    let runner_bytes = write_json(
        root,
        "evidence/run_cluster.py",
        &json!("synthetic reactor verifier fixture; not executed acceptance evidence"),
    );
    files.insert("evidence/run_cluster.py".into(), hash(&runner_bytes));
    let evidence = json!({"schemaVersion":"apsides.dev/reactor-evidence/v1","productionIrDigest":plan.ir_digest,
        "fixtureIrDigest":fixture_digest,"testedArtifactDigest":hash(&artifact_bytes),"artifactManifestFile":"tested-artifact-manifest.json",
        "overlayDigest":digest(&overlay).unwrap(),"runnerIdentity":{"sourceSha256":hash(&runner_bytes),"controllerManifestDigest":hash(b"test controller"),"controllerArchiveDigest":hash(b"test archive"),"fixtureArchiveDigest":hash(b"test fixture"),"nodeImage":format!("example.invalid/node@{}",hash(b"node"))},
        "cases":records,"checks":check_records});
    files.insert(
        "test-evidence.json".into(),
        hash(&write_json(root, "test-evidence.json", &evidence)),
    );
}

fn fixture_check(name: &str) -> bool {
    matches!(
        name,
        "offline-reactor-suite" | "secret-canary-scan" | "reaction-mutation-check"
    )
}

fn add_production_evidence(root: &Path, manifest: &mut Value) {
    // Synthetic verifier fixtures establish validation behavior, not execution coverage.
    let source = write_json(
        root,
        "runner-source.py",
        &json!("verifier fixture source only"),
    );
    manifest["files"]["runner-source.py"] = json!(hash(&source));
    let checks: BTreeMap<_, _> = release::REQUIRED_CHECKS
        .iter()
        .filter(|name| !fixture_check(name))
        .map(|name| {
            (
                *name,
                json!({"status":"passed", "assertions":check_assertions(name),
            "evidenceFiles":["runner-source.py"]}),
            )
        })
        .collect();
    let payload_files: BTreeMap<_, _> = [
        "build-inputs.json",
        "controller.oci.tar",
        "apsides-controller",
    ]
    .into_iter()
    .map(|name| (name, manifest["files"][name].clone()))
    .collect();
    let evidence = json!({
        "schemaVersion":"apsides.dev/production-evidence/v1",
        "irDigest":manifest["irDigest"], "compilerDigest":manifest["compilerDigest"],
        "binaryDigest":manifest["binaryDigest"], "image":manifest["image"],
        "sourceInputsDigest":manifest["source"]["inputsDigest"], "payloadFiles":payload_files,
        "runnerSourceFile":"runner-source.py", "runnerSourceDigest":hash(&source), "checks":checks
    });
    let bytes = write_json(root, "production-evidence.json", &evidence);
    manifest["files"]["production-evidence.json"] = json!(hash(&bytes));
}

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn read_fixture(root: &Path, name: &str) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&fs::read(root.join(name))?)?)
}

fn replace_signed_document(root: &Path, name: &str, document: &Value) -> TestResult {
    let bytes = canonical_bytes(document)?;
    fs::write(root.join(name), &bytes)?;
    let mut manifest = read_fixture(root, "release.json")?;
    manifest["files"][name] = json!(hash(&bytes));
    fs::write(root.join("release.json"), canonical_bytes(&manifest)?)?;
    resign(root);
    Ok(())
}

async fn require_evidence_rejection(
    root: &Path,
    policy: &TrustPolicy,
    expected: &str,
) -> TestResult {
    // The envelope and real SSH signature remain valid: failure must come from coverage.
    verify_candidate(root, policy).await?;
    match verify_release(root, policy).await {
        Ok(_) => return Err("signed invalid production evidence was accepted".into()),
        Err(error) => assert!(
            error.to_string().contains(expected),
            "unexpected evidence rejection branch"
        ),
    }
    Ok(())
}

#[tokio::test]
async fn signed_production_evidence_must_match_every_payload_subject() -> TestResult {
    for (pointer, replacement) in [
        ("/irDigest", json!(hash(b"another IR"))),
        ("/compilerDigest", json!(hash(b"another compiler"))),
        ("/binaryDigest", json!(hash(b"another binary"))),
        ("/sourceInputsDigest", json!(hash(b"another source"))),
        (
            "/image/reference",
            json!(format!("example.invalid/other@{}", hash(b"another image"))),
        ),
        ("/image/platform", json!("linux/amd64")),
        (
            "/payloadFiles/build-inputs.json",
            json!(hash(b"other build inputs")),
        ),
        (
            "/payloadFiles/controller.oci.tar",
            json!(hash(b"other OCI bytes")),
        ),
        (
            "/payloadFiles/apsides-controller",
            json!(hash(b"other controller bytes")),
        ),
    ] {
        let root = tempfile::tempdir()?;
        let policy = release(root.path());
        verify_release(root.path(), &policy).await?;
        let mut evidence = read_fixture(root.path(), "production-evidence.json")?;
        *evidence
            .pointer_mut(pointer)
            .ok_or("subject fixture path missing")? = replacement;
        replace_signed_document(root.path(), "production-evidence.json", &evidence)?;
        let expected = if pointer.starts_with("/payloadFiles/") {
            "production evidence payload files differ"
        } else {
            "production evidence refers to another production payload"
        };
        require_evidence_rejection(root.path(), &policy, expected).await?;
    }
    Ok(())
}

#[tokio::test]
async fn signed_fixture_evidence_cannot_satisfy_a_production_check() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path());
    verify_release(root.path(), &policy).await?;
    let mut fixture = read_fixture(root.path(), "test-evidence.json")?;
    fixture["checks"]["reproducible-payload"] = json!({"status":"passed",
        "assertions":check_assertions("reproducible-payload"), "evidenceFiles":[]});
    replace_signed_document(root.path(), "test-evidence.json", &fixture)?;
    let mut coverage = read_fixture(root.path(), "coverage.json")?;
    coverage["checks"]["reproducible-payload"]["evidenceFiles"] = json!(["test-evidence.json"]);
    replace_signed_document(root.path(), "coverage.json", &coverage)?;
    require_evidence_rejection(root.path(), &policy, "unknown field").await
}

#[tokio::test]
async fn signed_production_runner_source_must_be_bound_and_match() -> TestResult {
    for mutation in ["unbound", "digest", "changed-source"] {
        let root = tempfile::tempdir()?;
        let policy = release(root.path());
        verify_release(root.path(), &policy).await?;
        let mut evidence = read_fixture(root.path(), "production-evidence.json")?;
        match mutation {
            "unbound" => {
                fs::write(root.path().join("unsigned-runner.py"), b"unsigned runner")?;
                evidence["runnerSourceFile"] = json!("unsigned-runner.py");
                evidence["runnerSourceDigest"] = json!(hash(b"unsigned runner"));
            }
            "digest" => evidence["runnerSourceDigest"] = json!(hash(b"different runner")),
            _ => {
                replace_signed_document(root.path(), "runner-source.py", &json!("changed runner"))?;
            }
        }
        replace_signed_document(root.path(), "production-evidence.json", &evidence)?;
        require_evidence_rejection(
            root.path(),
            &policy,
            "production evidence runner source is not authenticated",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn signed_production_evidence_rejects_open_schema_and_wrong_check_kinds() -> TestResult {
    for (field, value, expected) in [
        ("unexpected", json!(true), "unknown field"),
        ("cases", json!({}), "unknown field"),
        (
            "unknown-check",
            json!({"status":"passed", "assertions":check_assertions("reproducible-payload"),
            "evidenceFiles":[]}),
            "production evidence contains an unsupported check",
        ),
        (
            "offline-reactor-suite",
            json!({"status":"passed", "assertions":check_assertions("offline-reactor-suite"),
            "evidenceFiles":[]}),
            "production evidence contains an unsupported check",
        ),
    ] {
        let root = tempfile::tempdir()?;
        let policy = release(root.path());
        verify_release(root.path(), &policy).await?;
        let mut evidence = read_fixture(root.path(), "production-evidence.json")?;
        if matches!(field, "unexpected" | "cases") {
            evidence[field] = value;
        } else {
            evidence["checks"][field] = value;
        }
        replace_signed_document(root.path(), "production-evidence.json", &evidence)?;
        require_evidence_rejection(root.path(), &policy, expected).await?;
    }
    Ok(())
}

#[tokio::test]
async fn signed_coverage_cannot_add_an_unknown_release_check() -> TestResult {
    let root = tempfile::tempdir()?;
    let policy = release(root.path());
    verify_release(root.path(), &policy).await?;
    let mut coverage = read_fixture(root.path(), "coverage.json")?;
    coverage["checks"]["unknown-check"] =
        json!({"status":"passed", "evidenceFiles":["production-evidence.json"]});
    replace_signed_document(root.path(), "coverage.json", &coverage)?;
    require_evidence_rejection(
        root.path(),
        &policy,
        "coverage contains an unsupported release check",
    )
    .await
}

#[tokio::test]
async fn signed_production_assertions_must_satisfy_the_exact_contract() -> TestResult {
    for (mutation, expected) in [
        (
            "missing",
            "evidence does not contain the exact required assertion set",
        ),
        ("wrong-id", "evidence is missing a required assertion"),
        (
            "wrong-actual",
            "evidence assertion does not satisfy its independent expected contract",
        ),
    ] {
        let root = tempfile::tempdir()?;
        let policy = release(root.path());
        verify_release(root.path(), &policy).await?;
        let mut evidence = read_fixture(root.path(), "production-evidence.json")?;
        let assertions = evidence["checks"]["reproducible-payload"]["assertions"]
            .as_array_mut()
            .ok_or("production assertion fixture missing")?;
        match mutation {
            "missing" => {
                assertions
                    .pop()
                    .ok_or("production assertion fixture empty")?;
            }
            "wrong-id" => {
                assertions
                    .first_mut()
                    .ok_or("production assertion fixture empty")?["checkId"] =
                    json!("invented-build-count");
            }
            _ => {
                assertions
                    .first_mut()
                    .ok_or("production assertion fixture empty")?["actual"] = json!(1);
            }
        }
        replace_signed_document(root.path(), "production-evidence.json", &evidence)?;
        require_evidence_rejection(root.path(), &policy, expected).await?;
    }
    Ok(())
}

#[tokio::test]
async fn signed_reactor_evidence_requires_authenticated_fixed_runner_source() -> TestResult {
    for mutation in ["missing-entry", "swapped-claim", "changed-signed-source"] {
        let root = tempfile::tempdir()?;
        let policy = release(root.path());
        verify_release(root.path(), &policy).await?;
        match mutation {
            "missing-entry" => {
                let mut manifest = read_fixture(root.path(), "release.json")?;
                manifest["files"]
                    .as_object_mut()
                    .ok_or("manifest files missing")?
                    .remove("evidence/run_cluster.py")
                    .ok_or("runner source fixture missing")?;
                fs::write(
                    root.path().join("release.json"),
                    canonical_bytes(&manifest)?,
                )?;
                resign(root.path());
            }
            "swapped-claim" => {
                let mut evidence = read_fixture(root.path(), "test-evidence.json")?;
                evidence["runnerIdentity"]["sourceSha256"] =
                    json!(hash(b"unrelated runner source"));
                replace_signed_document(root.path(), "test-evidence.json", &evidence)?;
            }
            _ => {
                replace_signed_document(
                    root.path(),
                    "evidence/run_cluster.py",
                    &json!("changed signed runner source"),
                )?;
            }
        }
        require_evidence_rejection(
            root.path(),
            &policy,
            "reactor runner source is not authenticated by the release",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn signed_binary_must_be_the_image_entrypoint_payload() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let mut binary = fs::read(root.path().join("apsides-controller")).unwrap();
    binary[32] = 1;
    fs::write(root.path().join("apsides-controller"), &binary).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("release.json")).unwrap()).unwrap();
    manifest["binaryDigest"] = json!(hash(&binary));
    manifest["files"]["apsides-controller"] = json!(hash(&binary));
    write_json(root.path(), "release.json", &manifest);
    resign(root.path());
    assert!(
        verify_release(root.path(), &policy)
            .await
            .unwrap_err()
            .to_string()
            .contains("OCI controller bytes")
    );
}

#[tokio::test]
async fn signed_image_cannot_add_an_unapproved_runtime_base_layer() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let binary = fs::read(root.path().join("apsides-controller")).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("release.json")).unwrap()).unwrap();
    let image = write_oci_with_extra_base(
        root.path(),
        &binary,
        manifest["irDigest"].as_str().unwrap(),
        true,
    );
    manifest["image"]["reference"] = json!(format!("example.invalid/controller@{image}"));
    manifest["files"]["controller.oci.tar"] = json!(hash(
        &fs::read(root.path().join("controller.oci.tar")).unwrap()
    ));
    write_json(root.path(), "release.json", &manifest);
    resign(root.path());
    let error = verify_release(root.path(), &policy)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("approved runtime layer chain"), "{error}");
}

#[tokio::test]
async fn signed_status_strings_and_unrelated_files_are_not_test_evidence() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let bytes = write_json(
        root.path(),
        "test-evidence.json",
        &json!({"purpose":"unrelated file"}),
    );
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("release.json")).unwrap()).unwrap();
    manifest["files"]["test-evidence.json"] = json!(hash(&bytes));
    write_json(root.path(), "release.json", &manifest);
    resign(root.path());
    assert!(verify_release(root.path(), &policy).await.is_err());
}

#[tokio::test]
async fn production_requires_explicit_marker_even_with_valid_signature() {
    let root = tempfile::tempdir().unwrap();
    let policy = release(root.path());
    let mut ir: Value =
        serde_json::from_slice(&fs::read(root.path().join("ir.json")).unwrap()).unwrap();
    ir.as_object_mut().unwrap().remove("testOnly");
    let bytes = write_json(root.path(), "ir.json", &ir);
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.path().join("release.json")).unwrap()).unwrap();
    manifest["files"]["ir.json"] = json!(hash(&bytes));
    write_json(root.path(), "release.json", &manifest);
    resign(root.path());
    assert!(
        verify_release(root.path(), &policy)
            .await
            .unwrap_err()
            .to_string()
            .contains("testOnly:false")
    );
}

#[tokio::test]
#[ignore = "requires real ORAS and a disposable TLS registry; verifier fixture only"]
async fn signed_oci_release_resolves_after_source_workspace_is_removed()
-> Result<(), Box<dyn std::error::Error>> {
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
    let policy = release(source.path());
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
    let resolved = release::resolve_release(&reference, &tool, &policy).await?;
    assert_eq!(
        resolved.verified().release_digest(),
        reference.release_digest
    );
    assert!(!resolved.path().join("key").exists());
    assert!(!resolved.path().join("key.pub").exists());
    let download = resolved.path().to_owned();
    drop(resolved);
    assert!(!download.exists());
    let mut wrong_policy = policy;
    if let SignaturePolicy::OpenSsh { principal, .. } = &mut wrong_policy.signature {
        *principal = "unapproved@example.invalid".into();
    }
    assert!(
        release::resolve_release(&reference, &tool, &wrong_policy)
            .await
            .is_err()
    );
    Ok(())
}
