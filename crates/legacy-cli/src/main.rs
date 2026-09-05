#![allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    clippy::ignored_unit_patterns,
    clippy::useless_borrows_in_formatting
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug)]
struct Error(String);
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, PartialEq, Eq)]
struct BuildArgs {
    source: PathBuf,
    image: String,
    output: PathBuf,
}

#[derive(Debug)]
struct DeployArgs {
    build: PathBuf,
    cluster: String,
    namespace: String,
    root: String,
    workload: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Receipt {
    schema: String,
    runtime_revision: String,
    runtime_dirty: bool,
    platform: String,
    source: SourceReceipt,
    archive: HashReceipt,
    bundle: BundleReceipt,
    compiler: Value,
    bundler: Value,
    renderer_abi: String,
    manifest_sha256: String,
    image: ImageReceipt,
}
#[derive(Serialize, Deserialize, Debug)]
struct SourceReceipt {
    path: String,
    sha256: String,
}
#[derive(Serialize, Deserialize, Debug)]
struct HashReceipt {
    path: String,
    sha256: String,
    byte_length: u64,
}
#[derive(Serialize, Deserialize, Debug)]
struct BundleReceipt {
    digest: String,
    metadata_sha256: String,
}
#[derive(Serialize, Deserialize, Debug)]
struct ImageReceipt {
    repository: String,
    tag: String,
    reference: String,
    manifest_digest: String,
    config_digest: String,
    archive: HashReceipt,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("oxideloop: {}", error.0);
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref().and_then(|x| x.to_str()) {
        Some("build") => build(parse_build(args.collect())?),
        Some("deploy") => deploy(parse_deploy(args.collect())?),
        _ => Err(Error("usage: oxideloop build --source PATH --image REPOSITORY --output DIR | oxideloop deploy --build RECEIPT --kind-cluster NAME --namespace NAME --root-name NAME --workload-image IMAGE".into())),
    }
}

fn value(args: &[OsString], name: &str) -> Result<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].to_string_lossy().into_owned())
        .ok_or_else(|| Error(format!("missing {name}")))
}
fn parse_build(args: Vec<OsString>) -> Result<BuildArgs> {
    if args.iter().any(|x| x == "--help") {
        return Err(Error(
            "usage: oxideloop build --source PATH --image REPOSITORY --output DIR".into(),
        ));
    }
    Ok(BuildArgs {
        source: PathBuf::from(value(&args, "--source")?),
        image: value(&args, "--image")?,
        output: PathBuf::from(value(&args, "--output")?),
    })
}
fn parse_deploy(args: Vec<OsString>) -> Result<DeployArgs> {
    Ok(DeployArgs {
        build: PathBuf::from(value(&args, "--build")?),
        cluster: value(&args, "--kind-cluster")?,
        namespace: value(&args, "--namespace")?,
        root: value(&args, "--root-name")?,
        workload: value(&args, "--workload-image")?,
    })
}
fn err<T: std::fmt::Display>(x: T) -> Error {
    Error(x.to_string())
}
fn run_cmd(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    if program == "pnpm" {
        command.env("CI", "1");
    }
    if let Some(path) = cwd {
        command.current_dir(path);
    }
    let output = command.output().map_err(err)?;
    if !output.status.success() {
        return Err(Error(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
fn run_cmd_input(program: &str, args: &[&str], input: &[u8], cwd: Option<&Path>) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(path) = cwd {
        command.current_dir(path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(err)?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error("unable to open command stdin".into()))?
        .write_all(input)
        .map_err(err)?;
    let output = child.wait_with_output().map_err(err)?;
    if !output.status.success() {
        return Err(Error(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
fn sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}
fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn saved_image_config_digest(image_archive: &Path, reference: &str) -> Result<String> {
    let manifest = run_cmd(
        "tar",
        &["-xOf", image_archive.to_str().unwrap(), "manifest.json"],
        None,
    )?;
    let entries: Vec<Value> = serde_json::from_str(&manifest).map_err(err)?;
    if entries.len() != 1 || entries[0]["RepoTags"] != serde_json::json!([reference]) {
        return Err(Error(
            "saved image archive must contain exactly the content-derived tag".into(),
        ));
    }
    let config = entries[0]["Config"]
        .as_str()
        .ok_or_else(|| Error("saved image manifest omitted its config identity".into()))?;
    let raw = config
        .strip_prefix("blobs/sha256/")
        .or_else(|| config.strip_suffix(".json"))
        .ok_or_else(|| Error("saved image config identity is not SHA-256".into()))?;
    let digest = format!("sha256:{raw}");
    if !is_sha256_digest(&digest) {
        return Err(Error("saved image config identity is not SHA-256".into()));
    }
    Ok(digest)
}
fn file_hash(path: &Path) -> Result<HashReceipt> {
    let bytes = fs::read(path).map_err(err)?;
    if bytes.len() > 512 * 1024 * 1024 {
        return Err(Error(format!(
            "artifact exceeds bounded size: {}",
            path.display()
        )));
    }
    Ok(HashReceipt {
        path: path.display().to_string(),
        sha256: sha256(&bytes),
        byte_length: bytes.len() as u64,
    })
}
fn validate_name(value: &str, what: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 253
        || value.starts_with('-')
        || value.ends_with('-')
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        return Err(Error(format!("invalid DNS {what}: {value}")));
    }
    Ok(())
}
fn repository_root() -> Result<PathBuf> {
    fs::canonicalize(env::current_dir().map_err(err)?).map_err(err)
}

fn build(args: BuildArgs) -> Result<()> {
    if args.source.file_name().and_then(|x| x.to_str()) != Some("src") {
        return Err(Error(
            "--source must name an authored controller source directory".into(),
        ));
    }
    let source = fs::canonicalize(&args.source).map_err(err)?;
    let root = repository_root()?;
    let expected_source =
        fs::canonicalize(root.join("packages/example-bundle/src")).map_err(err)?;
    if source != expected_source {
        return Err(Error("--source must be packages/example-bundle/src".into()));
    }
    let entry = source.join("controller.tsx");
    if !entry.is_file() {
        return Err(Error("approved source is missing controller.tsx".into()));
    }
    fs::create_dir_all(&args.output).map_err(err)?;
    let output = fs::canonicalize(&args.output).map_err(err)?;
    if output == root || !output.starts_with(&root) {
        return Err(Error(
            "--output must be a directory inside the repository".into(),
        ));
    }
    let receipt_path = output.join("receipt.json");
    if receipt_path.exists() {
        fs::remove_file(&receipt_path).map_err(err)?;
    }
    let staging = output.join(format!(".oxideloop-build-{}", std::process::id()));
    fs::create_dir(&staging).map_err(err)?;
    let archive = staging.join("bundle.archive");
    let metadata = staging.join("bundle.metadata.json");
    let result = run_cmd(
        "pnpm",
        &[
            "--filter",
            "@oxideloop/bundle-builder",
            "bundle:e2e",
            "--",
            "--out",
            archive.to_str().unwrap(),
            "--metadata",
            metadata.to_str().unwrap(),
            "--source",
            source.to_str().unwrap(),
        ],
        Some(&root),
    );
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    let metadata_bytes = fs::read(&metadata).map_err(err)?;
    let metadata_json: Value = serde_json::from_slice(&metadata_bytes).map_err(err)?;
    let build_meta = metadata_json
        .get("buildBundle")
        .ok_or_else(|| Error("metadata missing buildBundle".into()))?;
    let authored = build_meta
        .get("authoredSource")
        .ok_or_else(|| Error("metadata missing authoredSource".into()))?;
    let expected = serde_json::json!({"location":"packages/example-bundle/src","controller":"packages/example-bundle/src/controller.tsx"});
    if authored != &expected {
        return Err(Error(
            "metadata authoredSource is not the approved source identity".into(),
        ));
    }
    let archive_hash = file_hash(&archive)?;
    if build_meta["archive"]["sha256"] != archive_hash.sha256 {
        return Err(Error("bundle archive metadata hash mismatch".into()));
    }
    let digest = build_meta["bundleDigest"]
        .as_str()
        .ok_or_else(|| Error("metadata missing bundleDigest".into()))?;
    if digest.len() != 64 {
        return Err(Error("invalid bundle digest".into()));
    }
    let source_hash = build_meta["sourceSha256"]
        .as_str()
        .ok_or_else(|| Error("metadata missing builder sourceSha256".into()))?;
    if source_hash.len() != 64 {
        return Err(Error("invalid source SHA-256".into()));
    }
    let digest_file = staging.join("bundle.digest");
    fs::write(&digest_file, format!("{digest}\n")).map_err(err)?;
    let archive_context = archive
        .strip_prefix(&root)
        .map_err(|_| Error("bundle archive escaped the Docker build context".into()))?
        .to_str()
        .ok_or_else(|| Error("bundle archive path is not UTF-8".into()))?;
    let digest_context = digest_file
        .strip_prefix(&root)
        .map_err(|_| Error("bundle digest escaped the Docker build context".into()))?
        .to_str()
        .ok_or_else(|| Error("bundle digest path is not UTF-8".into()))?;
    let temporary_reference = format!("{}:oxideloop-build-{}", args.image, &digest[..16]);
    let docker_meta = staging.join("buildx-metadata.json");
    let docker_args = [
        "buildx",
        "build",
        "--provenance=false",
        "--target",
        "production",
        "--load",
        "--tag",
        &temporary_reference,
        "--file",
        "Dockerfile",
        "--build-arg",
        &format!("OXIDELOOP_BUNDLE_ARCHIVE={archive_context}"),
        "--build-arg",
        &format!("OXIDELOOP_BUNDLE_DIGEST_FILE={digest_context}"),
        "--metadata-file",
        docker_meta.to_str().unwrap(),
        ".",
    ];
    run_cmd("docker", &docker_args, Some(&root))?;
    let buildx_json: Value =
        serde_json::from_slice(&fs::read(&docker_meta).map_err(err)?).map_err(err)?;
    let image_digest = buildx_json["containerimage.digest"]
        .as_str()
        .unwrap_or_default();
    if image_digest.len() < 16 {
        return Err(Error(
            "buildx metadata missing containerimage.digest".into(),
        ));
    }
    if !is_sha256_digest(image_digest) {
        return Err(Error("buildx image digest is not sha256".into()));
    }
    let tag = format!(
        "oxideloop-bundle-{}",
        image_digest[7..].chars().take(16).collect::<String>()
    );
    let reference = format!("{}:{tag}", args.image);
    run_cmd(
        "docker",
        &["tag", &temporary_reference, &reference],
        Some(&root),
    )?;
    let image_archive = staging.join("image.tar");
    run_cmd(
        "docker",
        &[
            "image",
            "save",
            "--output",
            image_archive.to_str().unwrap(),
            &reference,
        ],
        Some(&root),
    )?;
    let config_digest = saved_image_config_digest(&image_archive, &reference)?;
    let inspect = run_cmd(
        "docker",
        &[
            "image",
            "inspect",
            "--format",
            "{{.Id}}|{{.Os}}/{{.Architecture}}",
            &reference,
        ],
        Some(&root),
    )?;
    let mut inspect_parts = inspect.trim().split('|');
    let engine_id = inspect_parts.next().unwrap_or_default().to_string();
    let platform = inspect_parts.next().unwrap_or_default().to_string();
    if !is_sha256_digest(&engine_id) || platform.is_empty() {
        return Err(Error(
            "docker image inspect omitted identity/platform".into(),
        ));
    }
    let final_archive = output.join("bundle.archive");
    let final_metadata = output.join("bundle.metadata.json");
    let final_image = output.join("image.tar");
    fs::rename(&archive, &final_archive).map_err(err)?;
    fs::rename(&metadata, &final_metadata).map_err(err)?;
    fs::rename(&image_archive, &final_image).map_err(err)?;
    let receipt = Receipt {
        schema: "oxideloop.dev/build-receipt/v0".into(),
        runtime_revision: run_cmd("git", &["rev-parse", "HEAD"], Some(&root))
            .unwrap_or_else(|_| "unknown".into())
            .trim()
            .into(),
        runtime_dirty: !run_cmd("git", &["status", "--porcelain"], Some(&root))
            .unwrap_or_default()
            .trim()
            .is_empty(),
        platform,
        source: SourceReceipt {
            path: "packages/example-bundle/src/controller.tsx".into(),
            sha256: source_hash.into(),
        },
        archive: file_hash(&final_archive)?,
        bundle: BundleReceipt {
            digest: digest.into(),
            metadata_sha256: sha256(&metadata_bytes),
        },
        compiler: build_meta["compiler"].clone(),
        bundler: build_meta["bundler"].clone(),
        renderer_abi: build_meta["rendererAbiVersion"]
            .as_str()
            .unwrap_or_default()
            .into(),
        manifest_sha256: digest.into(),
        image: ImageReceipt {
            repository: args.image,
            tag,
            reference,
            manifest_digest: image_digest.into(),
            config_digest,
            archive: file_hash(&final_image)?,
        },
    };
    let tmp = staging.join("receipt.json");
    fs::write(&tmp, serde_json::to_vec_pretty(&receipt).map_err(err)?).map_err(err)?;
    fs::rename(&tmp, &receipt_path).map_err(err)?;
    fs::remove_dir_all(staging).map_err(err)?;
    println!("{}", receipt_path.display());
    Ok(())
}

fn kubectl(cluster: &str, namespace: Option<&str>, args: &[&str]) -> Result<String> {
    let context = format!("kind-{cluster}");
    let mut all = vec!["--context", context.as_str()];
    if let Some(ns) = namespace {
        all.extend(["--namespace", ns]);
    }
    all.extend(args);
    run_cmd("kubectl", &all, None)
}
fn image_id_matches(value: &str, receipt: &Receipt) -> bool {
    value.trim_end().ends_with(&receipt.image.config_digest)
        || value.trim_end().ends_with(&receipt.image.manifest_digest)
}
fn json_values(bytes: &[u8]) -> Result<Vec<Value>> {
    let stream = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let mut values = Vec::new();
    for value in stream {
        values.push(value.map_err(err)?);
    }
    Ok(values)
}
fn objects(value: Value) -> Vec<Value> {
    if value.get("kind").and_then(Value::as_str) == Some("List") {
        value
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        vec![value]
    }
}
fn stamp(mut object: Value, receipt: &Receipt, namespace: &str, root: &str) -> Value {
    let is_root = object.get("kind").and_then(Value::as_str) == Some("WebApplication");
    let metadata = object
        .as_object_mut()
        .unwrap()
        .entry("metadata")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();
    metadata.insert("namespace".into(), Value::String(namespace.into()));
    let annotations = metadata
        .entry("annotations")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();
    annotations.insert(
        "oxideloop.dev/receipt-schema".into(),
        Value::String(receipt.schema.clone()),
    );
    annotations.insert(
        "oxideloop.dev/bundle-digest".into(),
        Value::String(receipt.bundle.digest.clone()),
    );
    annotations.insert(
        "oxideloop.dev/image-config".into(),
        Value::String(receipt.image.config_digest.clone()),
    );
    annotations.insert(
        "oxideloop.dev/image-manifest".into(),
        Value::String(receipt.image.manifest_digest.clone()),
    );
    annotations.insert(
        "oxideloop.dev/build-receipt".into(),
        Value::String(sha256(
            &serde_json::to_vec(receipt)
                .expect("serializing a validated build receipt cannot fail"),
        )),
    );
    annotations.insert(
        "oxideloop.dev/namespace".into(),
        Value::String(namespace.into()),
    );
    if is_root {
        annotations.insert("oxideloop.dev/root-name".into(), Value::String(root.into()));
    }
    object
}
fn stamp_crd(mut object: Value) -> Value {
    let metadata = object
        .as_object_mut()
        .unwrap()
        .entry("metadata")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();
    let annotations = metadata
        .entry("annotations")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();
    annotations.insert(
        "oxideloop.dev/install-version".into(),
        Value::String("oxideloop-crds-v0".into()),
    );
    object
}
fn projection(value: &Value) -> Value {
    let mut output = serde_json::Map::new();
    for key in [
        "apiVersion",
        "kind",
        "data",
        "type",
        "rules",
        "subjects",
        "roleRef",
        "automountServiceAccountToken",
        "spec",
    ] {
        if let Some(v) = value.get(key) {
            output.insert(key.into(), v.clone());
        }
    }
    if let Some(metadata) = value.get("metadata") {
        let mut m = serde_json::Map::new();
        for key in ["name", "namespace"] {
            if let Some(v) = metadata.get(key) {
                m.insert(key.into(), v.clone());
            }
        }
        if let Some(annotations) = metadata.get("annotations").and_then(Value::as_object) {
            let owned = annotations
                .iter()
                .filter(|(key, _)| key.starts_with("oxideloop.dev/"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            m.insert("annotations".into(), Value::Object(owned));
        }
        output.insert("metadata".into(), Value::Object(m));
    }
    Value::Object(output)
}
fn server_dry_run(
    cluster: &str,
    namespace: Option<&str>,
    verb: &str,
    desired: &Value,
) -> Result<Value> {
    let bytes = serde_json::to_vec(desired).map_err(err)?;
    let context = format!("kind-{cluster}");
    let mut args = vec!["--context", context.as_str()];
    if let Some(ns) = namespace {
        args.extend(["--namespace", ns]);
    }
    args.extend([verb, "--dry-run=server", "-o", "json", "-f", "-"]);
    let normalized = run_cmd_input("kubectl", &args, &bytes, None)?;
    serde_json::from_str(&normalized).map_err(err)
}
fn exact_resume(
    cluster: &str,
    namespace: Option<&str>,
    resource: &str,
    kind: &str,
    name: &str,
    desired: &Value,
) -> Result<()> {
    let raw = kubectl(cluster, namespace, &["get", resource, name, "-o", "json"])?;
    let live: Value = serde_json::from_str(&raw).map_err(err)?;
    let mut replacement = desired.clone();
    replacement["metadata"]["resourceVersion"] = live["metadata"]["resourceVersion"].clone();
    let normalized = server_dry_run(cluster, namespace, "replace", &replacement)?;
    if projection(&live) != projection(&normalized) {
        return Err(Error(format!(
            "existing {kind}/{name} does not exactly resume the build receipt"
        )));
    }
    Ok(())
}
fn ensure_object(cluster: &str, namespace: Option<&str>, desired: Value) -> Result<()> {
    let kind = desired["kind"]
        .as_str()
        .ok_or_else(|| Error("desired object missing kind".into()))?;
    let name = desired["metadata"]["name"]
        .as_str()
        .ok_or_else(|| Error("desired object missing name".into()))?;
    let resource = match kind {
        "CustomResourceDefinition" => "crd",
        "ServiceAccount" => "sa",
        "Role" => "role",
        "RoleBinding" => "rolebinding",
        "ConfigMap" => "configmap",
        "Deployment" => "deployment",
        "WebApplication" => "webapplication",
        _ => return Err(Error(format!("unsupported create-only object kind {kind}"))),
    };
    let found = kubectl(cluster, namespace, &["get", resource, name, "-o", "json"]);
    match found {
        Ok(_) => exact_resume(cluster, namespace, resource, kind, name, &desired),
        Err(error) if error.0.contains("NotFound") || error.0.contains("not found") => {
            server_dry_run(cluster, namespace, "create", &desired)?;
            let bytes = serde_json::to_vec(&desired).map_err(err)?;
            let context = format!("kind-{cluster}");
            let mut args = vec!["--context", context.as_str()];
            if let Some(ns) = namespace {
                args.extend(["--namespace", ns]);
            }
            args.extend(["create", "-f", "-"]);
            match run_cmd_input("kubectl", &args, &bytes, None) {
                Ok(_) => Ok(()),
                Err(error) if error.0.contains("AlreadyExists") => {
                    exact_resume(cluster, namespace, resource, kind, name, &desired)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}
fn deploy(args: DeployArgs) -> Result<()> {
    validate_name(&args.cluster, "cluster")
        .and_then(|_| validate_name(&args.namespace, "namespace"))
        .and_then(|_| validate_name(&args.root, "root"))?;
    if !args.cluster.starts_with("oxideloop-") {
        return Err(Error(
            "--kind-cluster must be a uniquely named OxideLoop kind cluster".into(),
        ));
    }
    let bytes = fs::read(&args.build).map_err(err)?;
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(err)?;
    let archive = PathBuf::from(&receipt.archive.path);
    if file_hash(&archive)?.sha256 != receipt.archive.sha256 {
        return Err(Error("bundle archive hash mismatch".into()));
    }
    let image_archive = PathBuf::from(&receipt.image.archive.path);
    if file_hash(&image_archive)?.sha256 != receipt.image.archive.sha256 {
        return Err(Error("image archive hash mismatch".into()));
    }
    let metadata_path = args
        .build
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bundle.metadata.json");
    if sha256(&fs::read(&metadata_path).map_err(err)?) != receipt.bundle.metadata_sha256 {
        return Err(Error("bundle metadata hash mismatch".into()));
    }
    let clusters = run_cmd("kind", &["get", "clusters"], None)?;
    if !clusters.lines().any(|name| name == args.cluster) {
        run_cmd(
            "kind",
            &[
                "create",
                "cluster",
                "--name",
                &args.cluster,
                "--config",
                "deploy/e2e/kind-config.yaml",
            ],
            None,
        )?;
    }
    run_cmd(
        "kind",
        &[
            "load",
            "image-archive",
            image_archive.to_str().unwrap(),
            "--name",
            &args.cluster,
        ],
        None,
    )?;
    kubectl(
        &args.cluster,
        None,
        &["create", "namespace", &args.namespace],
    )
    .or_else(|e| {
        if e.0.contains("AlreadyExists") {
            Ok(String::new())
        } else {
            Err(e)
        }
    })?;
    let crd_source = fs::read("deploy/crds/oxideloop-crds.yaml").map_err(err)?;
    let crd_context = format!("kind-{}", args.cluster);
    let crd_json = run_cmd_input(
        "kubectl",
        &[
            "--context",
            &crd_context,
            "create",
            "--dry-run=client",
            "-o",
            "json",
            "-f",
            "-",
        ],
        &crd_source,
        None,
    )?;
    for object in json_values(crd_json.as_bytes())?
        .into_iter()
        .flat_map(objects)
    {
        let object = stamp_crd(object);
        let name = object["metadata"]["name"]
            .as_str()
            .ok_or_else(|| Error("CRD missing name".into()))?
            .to_string();
        ensure_object(&args.cluster, None, object)?;
        kubectl(
            &args.cluster,
            None,
            &[
                "wait",
                "--for=condition=Established",
                &format!("crd/{name}"),
                "--timeout=60s",
            ],
        )?;
    }
    let rendered = run_cmd("kubectl", &["kustomize", "deploy/controller"], None)?
        .replace(
            "ghcr.io/oxideloop/oxideloop:production",
            &receipt.image.reference,
        )
        .replace("imagePullPolicy: IfNotPresent", "imagePullPolicy: Never")
        .replace("rootName: root", &format!("rootName: {}", args.root));
    let controller_json = run_cmd_input(
        "kubectl",
        &[
            "--context",
            &format!("kind-{}", args.cluster),
            "--namespace",
            &args.namespace,
            "create",
            "--dry-run=client",
            "-o",
            "json",
            "-f",
            "-",
        ],
        rendered.as_bytes(),
        None,
    )?;
    let controller_objects: Vec<Value> = json_values(controller_json.as_bytes())?
        .into_iter()
        .flat_map(objects)
        .map(|object| stamp(object, &receipt, &args.namespace, &args.root))
        .collect();
    for object in &controller_objects {
        ensure_object(&args.cluster, Some(&args.namespace), object.clone())?;
    }
    kubectl(
        &args.cluster,
        Some(&args.namespace),
        &[
            "rollout",
            "status",
            "deployment/oxideloop-controller",
            "--timeout=180s",
        ],
    )?;
    let image_ids = kubectl(
        &args.cluster,
        Some(&args.namespace),
        &[
            "get",
            "pods",
            "-l",
            "app.kubernetes.io/component=controller",
            "-o",
            "jsonpath={range .items[0].status.containerStatuses[*]}{.imageID}{\"\\n\"}{end}",
        ],
    )?;
    if image_ids
        .lines()
        .filter(|id| image_id_matches(id, &receipt))
        .count()
        < 2
    {
        return Err(Error(
            "controller and renderer pod image IDs do not match the build receipt".into(),
        ));
    }
    for object in &controller_objects {
        ensure_object(&args.cluster, Some(&args.namespace), object.clone())?;
    }
    let root = stamp(
        serde_json::json!({"apiVersion":"demo.oxideloop.dev/v1alpha1","kind":"WebApplication","metadata":{"name":args.root},"spec":{"image":args.workload,"replicas":1,"expose":true,"program":{"digest":receipt.bundle.digest}}}),
        &receipt,
        &args.namespace,
        &args.root,
    );
    ensure_object(&args.cluster, Some(&args.namespace), root)?;
    let digest_wait = format!(
        "--for=jsonpath={{.status.verifiedBundleDigest}}={}",
        receipt.bundle.digest
    );
    kubectl(
        &args.cluster,
        Some(&args.namespace),
        &[
            &digest_wait,
            &format!("webapplication/{}", args.root),
            "--timeout=180s",
        ],
    )?;
    kubectl(
        &args.cluster,
        Some(&args.namespace),
        &[
            "wait",
            "--for=condition=Available",
            &format!("deployment/{}", args.root),
            "--timeout=180s",
        ],
    )?;
    kubectl(
        &args.cluster,
        Some(&args.namespace),
        &[
            "wait",
            "--for=condition=Ready",
            "pod",
            "-l",
            &format!("app={}", args.root),
            "--timeout=180s",
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_dns_validated() {
        assert!(validate_name("demo-1", "x").is_ok());
        assert!(validate_name("Demo", "x").is_err());
    }
    #[test]
    fn hash_is_sha256() {
        assert_eq!(
            sha256(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
