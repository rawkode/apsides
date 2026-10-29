//! The whole native build against fake tools: a fake podman and Docker
//! that act out inspect, save, the environment probe and the compile, a fake
//! host cargo, a fake cargo-cyclonedx and a synthetic runtime base pinned in
//! a temporary assets tree.
//!
//! Two podman builds must write identical outputs, every output must pass
//! the release checks, and a Docker build must differ from them only in the
//! engine's host tool identity and so the compiler closure digest.
#![cfg(unix)]

use packaging::{
    Cache, Overrides, Request, Validator, build_with, controller_manifest, digest::digest,
    rusty_v8::Fetch, sources,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const IR_DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const ARCHIVE: &[u8] = b"rusty v8 archive bytes";
const BUILDER: &str = "example.invalid/builder@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922";
const BUILDER_ENV: &str = r#"["PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin","RUSTUP_HOME=/usr/local/rustup","CARGO_HOME=/usr/local/cargo","RUST_VERSION=1.98.0"]"#;
/// The outputs a build adds to the artifact directory.
const OUTPUTS: [&str; 5] = [
    "build-inputs.json",
    "build-result.json",
    "image/controller.oci.tar",
    "linux-binary/apsides-controller",
    "sbom/sbom.json",
];

/// Accepts the fixture artifact as `IR_DIGEST`.
struct FakeValidator;

impl Validator for FakeValidator {
    fn verify_artifact(&self, _: &Path) -> Result<String, String> {
        Ok(IR_DIGEST.to_owned())
    }

    fn ir_digest(&self, ir: &Path) -> Result<String, String> {
        fs::read(ir).map_err(|error| error.to_string())?;
        Ok(IR_DIGEST.to_owned())
    }
}

/// Serves the fixture archive.
struct FakeFetch;

impl Fetch for FakeFetch {
    fn fetch(&self, _: &str, sink: &mut dyn Write) -> Result<u64, String> {
        sink.write_all(ARCHIVE).map_err(|error| error.to_string())?;
        Ok(ARCHIVE.len() as u64)
    }
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) -> TestResult {
    fs::create_dir_all(path.parent().ok_or("no parent directory")?)?;
    fs::write(path, bytes)?;
    Ok(())
}

/// Writes an executable `/bin/sh` script.
fn tool(path: &Path, script: &str) -> TestResult<PathBuf> {
    write(path, format!("#!/bin/sh\n{script}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(path.to_path_buf())
}

fn canonical(value: &Value) -> TestResult<Vec<u8>> {
    Ok(compiler_core::canonical_bytes(value)?)
}

/// A one-file image layer, as an engine save stores it.
fn base_layer() -> TestResult<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    let contents = b"ID=fake\n";
    let mut header = tar::Header::new_ustar();
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_size(contents.len() as u64);
    header.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut header, "etc/os-release", &contents[..])?;
    Ok(builder.into_inner()?)
}

/// The runtime base: its pinned reference, and the podman docker-archive
/// save of it at `save`.
fn runtime_base(assets: &Path, save: &Path) -> TestResult<String> {
    let layer = base_layer()?;
    let config = canonical(&json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {"Env": ["PATH=/usr/bin"]},
        "rootfs": {"type": "layers", "diff_ids": [digest(&layer)]},
    }))?;
    let manifest = canonical(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest(&config), "size": config.len()},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": digest(&layer), "size": layer.len()}],
    }))?;
    let index = canonical(&json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest(&manifest),
            "size": manifest.len(),
            "platform": {"os": "linux", "architecture": "amd64"},
        }],
    }))?;
    let blobs = assets.join("scripts/runtime-base/blobs/sha256");
    for bytes in [&index, &manifest] {
        write(&blobs.join(&digest(bytes)[7..]), bytes)?;
    }
    let config_name = format!("{}.json", &digest(&config)[7..]);
    let layer_name = format!("{}.tar", &digest(&layer)[7..]);
    let members = [
        (
            "manifest.json".to_owned(),
            serde_json::to_vec(
                &json!([{"Config": config_name, "RepoTags": [], "Layers": [layer_name]}]),
            )?,
        ),
        (config_name, config),
        (layer_name, layer),
    ];
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in members {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_size(bytes.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, name, bytes.as_slice())?;
    }
    write(save, builder.into_inner()?)?;
    Ok(format!("example.invalid/runtime@{}", digest(&index)))
}

/// A fake engine CLI. `client`, `server` and `info` are what its version
/// and info commands print; `id` is the builder's image ID. `run` acts out
/// the environment probe, or the compile, which writes an ELF header and a
/// fixed tail into the target mount.
fn engine(
    path: &Path,
    client: &str,
    server: &str,
    info: &str,
    id: &str,
    runtime: &str,
    save: &Path,
) -> TestResult<PathBuf> {
    let script = r#"case "$1" in
  --version) echo '@CLIENT@'; exit 0;;
  version) echo '@SERVER@'; exit 0;;
  info) echo '@INFO@'; exit 0;;
  rm) exit 0;;
  image)
    case "$2 $3" in
      'inspect @BUILDER@') echo '[{"Id":"@ID@","Os":"linux","Architecture":"amd64","RepoDigests":["@BUILDER@"],"Config":{"Env":@ENV@}}]'; exit 0;;
      'inspect @RUNTIME@') echo '[{"Id":"@ID@0","Os":"linux","Architecture":"amd64","RepoDigests":["@RUNTIME@"],"Config":{"Env":["PATH=/usr/bin"]}}]'; exit 0;;
      inspect*) echo "Error: no such image: $3" >&2; exit 125;;
    esac
    prev=; out=
    for arg in "$@"; do
      [ "$prev" = --output ] && out=$arg
      prev=$arg
    done
    cp '@SAVE@' "$out"
    exit 0;;
esac
entry=; target=; cargo=; prev=; envs=
for arg in "$@"; do
  case "$prev" in
    --entrypoint) entry=$arg;;
    --env) envs="$envs$arg
";;
    --mount) case "$arg" in
      *target=/build/target) target=${arg#type=bind,source=}; target=${target%,target=/build/target};;
      *target=/build/.cargo,readonly) cargo=${arg#type=bind,source=}; cargo=${cargo%,target=/build/.cargo,readonly};;
    esac;;
  esac
  prev=$arg
done
if [ "$entry" = /usr/bin/cat ]; then
  printf '%s' "$envs" | while IFS= read -r line; do printf '%s\0' "$line"; done
  printf 'HOSTNAME=apsides-build\0'
  cat "$cargo/config.toml"
  exit 0
fi
mkdir -p "$target/release"
{ printf '\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\002\000\076\000'; head -c 44 /dev/zero; printf 'controller'; } > "$target/release/controller""#
        .replace("@CLIENT@", client)
        .replace("@SERVER@", server)
        .replace("@INFO@", info)
        .replace("@BUILDER@", BUILDER)
        .replace("@RUNTIME@", runtime)
        .replace("@ID@", id)
        .replace("@ENV@", BUILDER_ENV)
        .replace("@SAVE@", &save.display().to_string());
    tool(path, &script)
}

/// The assets tree: the packaging sources this test binary's packaging
/// crate was built from (so the closure's source check passes), a minimal
/// runtime crate, the `rusty_v8` pins and the pinned runtime base, whose
/// engine save is written to `save`. Returns the runtime base reference.
fn assets_tree(assets: &Path, save: &Path) -> TestResult<String> {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for (path, _) in sources::collect(&checkout)? {
        write(&assets.join(&path), fs::read(checkout.join(&path))?)?;
    }
    let runtime = assets.join("crates/runtime");
    write(
        &runtime.join("Cargo.toml"),
        "[package]\nname = \"runtime\"\n",
    )?;
    write(&runtime.join("src/lib.rs"), "pub fn start() {}\n")?;
    write(
        &assets.join("scripts/rusty-v8/pins.json"),
        serde_json::to_vec(&json!({
            "v8CrateVersion": "150.4.0",
            "bindingFile": {"required": false},
            "archives": {"x86_64-unknown-linux-gnu": {
                "url": "https://example.invalid/librusty_v8.a.gz",
                "sha256": &digest(ARCHIVE)[7..],
                "name": "librusty_v8",
                "version": "150.4.0",
            }},
        }))?,
    )?;
    runtime_base(assets, save)
}

/// The artifact `aps compile` would have written, by path.
fn artifact(runtime: &Path) -> TestResult<BTreeMap<String, Vec<u8>>> {
    let runtime_json = serde_json::to_string(runtime.to_str().ok_or("non-UTF-8 path")?)?;
    let mut artifact = BTreeMap::from([
        (
            "controller/Cargo.toml".to_owned(),
            controller_manifest(&runtime_json).into_bytes(),
        ),
        (
            "controller/Cargo.lock".to_owned(),
            b"version = 4\n\n[[package]]\nname = \"controller\"\nversion = \"0.1.0\"\n".to_vec(),
        ),
        (
            "controller/src/main.rs".to_owned(),
            b"fn main() {}\n".to_vec(),
        ),
        ("controller/src/ir.json".to_owned(), b"{}\n".to_vec()),
    ]);
    let files: BTreeMap<&String, String> = artifact
        .iter()
        .map(|(path, bytes)| (path, digest(bytes)))
        .collect();
    artifact.insert(
        "manifest.json".to_owned(),
        serde_json::to_vec(&json!({"files": files}))?,
    );
    Ok(artifact)
}

/// A fake host cargo: `cargo metadata` names the controller it was asked
/// about, and `cargo vendor` vendors one crate.
fn host_cargo(tools: &Path, runtime: &Path) -> TestResult<PathBuf> {
    let metadata = tools.join("metadata.json");
    write(
        &metadata,
        serde_json::to_vec(&json!({"packages": [
            {"name": "controller", "version": "0.1.0", "source": null,
             "manifest_path": "@CONTROLLER@/Cargo.toml",
             "targets": [{"kind": ["bin"], "src_path": "@CONTROLLER@/src/main.rs"}]},
            {"name": "runtime", "version": "0.1.0", "source": null,
             "manifest_path": runtime.join("Cargo.toml"),
             "targets": [{"kind": ["lib"], "src_path": runtime.join("src/lib.rs")}]},
            {"name": "v8", "version": "150.4.0",
             "source": "registry+https://github.com/rust-lang/crates.io-index",
             "manifest_path": "/registry/v8/Cargo.toml", "targets": []},
        ]}))?,
    )?;
    tool(
        &tools.join("cargo"),
        &format!(
            "prev=; manifest=\n\
             for arg in \"$@\"; do [ \"$prev\" = --manifest-path ] && manifest=$arg; prev=$arg; done\n\
             case \"$1\" in\n\
             --version) echo 'cargo 1.98.0 (fake)';;\n\
             metadata) sed \"s#@CONTROLLER@#$(cd \"$(dirname \"$manifest\")\" && pwd)#g\" '{}';;\n\
             vendor) mkdir -p vendor/itoa-1.0.0 && printf 'pub fn x() {{}}\\n' > vendor/itoa-1.0.0/lib.rs && printf '[source.crates-io]\\n';;\n\
             *) exit 64;;\n\
             esac",
            metadata.display()
        ),
    )
}

/// A fake cargo-cyclonedx writing a minimal SBOM that names its build path.
fn cyclonedx(tools: &Path) -> TestResult<PathBuf> {
    tool(
        &tools.join("cargo-cyclonedx"),
        r#"if [ "$1 $2" = "cyclonedx --version" ]; then echo 'cargo-cyclonedx 0.5.9'; exit 0; fi
prev=; manifest=
for arg in "$@"; do
  [ "$prev" = --manifest-path ] && manifest=$arg
  prev=$arg
done
controller=$(dirname "$manifest")
build=$(dirname "$controller")
printf '{"bomFormat":"CycloneDX","specVersion":"1.5","metadata":{"component":{"bom-ref":"path+file://%s/controller#0.1.0"}},"components":[{"type":"library","name":"itoa","version":"1.0.0","bom-ref":"itoa"}],"dependencies":[{"ref":"itoa","dependsOn":[]}]}' "$build" > "$controller/controller.cdx.json""#,
    )
}

/// The source compiler's `build-inputs.json`, with no native compiler yet.
fn receipt() -> TestResult<Vec<u8>> {
    let closure = json!({
        "schemaVersion": release::COMPILER_CLOSURE_SCHEMA,
        "authoringCompiler": {"sdk": {"identity": "@apsides/kubernetes"}, "toolchain": {"deno": "pinned"}},
        "nativeCompiler": null,
    });
    canonical(&json!({
        "sdk": {"identity": "@apsides/kubernetes"},
        "toolchain": {"deno": "pinned"},
        "compilerClosure": closure,
        "compilerClosureDigest": compiler_core::digest(&closure)?,
    }))
}

/// The assets tree, tools, profile, artifact and source receipt every build
/// starts from.
struct Fixture {
    directory: tempfile::TempDir,
    assets: PathBuf,
    profile: PathBuf,
    cargo: PathBuf,
    podman: PathBuf,
    docker: PathBuf,
    cache: PathBuf,
    artifact: BTreeMap<String, Vec<u8>>,
    receipt: Vec<u8>,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let assets = root.join("assets");
        let tools = root.join("tools");
        let save = tools.join("runtime-base.tar");
        let runtime_reference = assets_tree(&assets, &save)?;
        let runtime = assets.join("crates/runtime");
        let cargo = host_cargo(&tools, &runtime)?;
        let sbom = cyclonedx(&tools)?;
        let podman = engine(
            &tools.join("podman/podman"),
            "podman version 5.8.9",
            "5.8.9",
            "true",
            "a3535ab4",
            &runtime_reference,
            &save,
        )?;
        let docker = engine(
            &tools.join("docker/docker"),
            "Docker version 28.0.0, build fake",
            "28.0.0",
            r#"["name=seccomp,profile=builtin"]"#,
            "sha256:a3535ab4",
            &runtime_reference,
            &save,
        )?;
        let profile = root.join("apsides.build.json");
        write(
            &profile,
            serde_json::to_vec(&json!({
                "schemaVersion": "apsides.dev/build-profile/v1",
                "builderImage": BUILDER,
                "runtimeImage": runtime_reference,
                "platform": "linux/amd64",
                "sbomTool": sbom,
                "sbomToolSha256": digest(&fs::read(&sbom)?),
                "sourceDateEpoch": 0,
            }))?,
        )?;
        Ok(Self {
            cache: root.join("rusty-v8-cache"),
            directory,
            artifact: artifact(&runtime)?,
            receipt: receipt()?,
            assets,
            profile,
            cargo,
            podman,
            docker,
        })
    }

    /// Builds a fresh copy of the artifact in `<name>/build` with `engine`
    /// and returns the build outputs by path.
    fn build(
        &self,
        name: &str,
        engine: (container::Engine, &Path),
    ) -> TestResult<BTreeMap<String, Vec<u8>>> {
        let out = self.directory.path().canonicalize()?.join(name);
        let artifact = out.join("build");
        for (path, bytes) in &self.artifact {
            write(&artifact.join(path), bytes)?;
        }
        write(&out.join("build.source/build-inputs.json"), &self.receipt)?;
        let built = build_with(
            &Request {
                assets: &self.assets,
                artifact: &artifact,
                stage: &out.join("build.source"),
                profile: &self.profile,
                cache: Cache::Disabled,
                validator: &FakeValidator,
            },
            &Overrides {
                engine: Some(engine),
                cargo: Some(&self.cargo),
                rusty_v8_cache: Some(&self.cache),
                fetcher: Some(&FakeFetch),
            },
        );
        let context = out.join("build.native-context");
        if context.exists() {
            packaging::context::unseal(&context)?;
        }
        let built = built?;

        let mut leftovers: Vec<String> = fs::read_dir(&out)?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        leftovers.sort();
        assert_eq!(leftovers, ["build", "build.native-context", "build.source"]);
        let mut outputs = BTreeMap::new();
        collect(&artifact, "", &mut outputs)?;
        for path in self.artifact.keys() {
            outputs.remove(path);
        }
        assert_eq!(
            outputs.keys().map(String::as_str).collect::<Vec<_>>(),
            OUTPUTS
        );

        let result: Value = serde_json::from_slice(&outputs["build-result.json"])?;
        assert_eq!(result["schemaVersion"], "apsides.dev/build-result/v1");
        assert_eq!(result["irDigest"], IR_DIGEST);
        assert_eq!(result["platform"], "linux/amd64");
        assert_eq!(result["imageDigest"], built.image_digest.as_str());
        assert_eq!(
            result["binaryDigest"],
            digest(&outputs["linux-binary/apsides-controller"])
        );
        assert_eq!(result["sbomDigest"], digest(&outputs["sbom/sbom.json"]));
        assert_eq!(built.binary_digest, result["binaryDigest"]);

        let inputs: Value = serde_json::from_slice(&outputs["build-inputs.json"])?;
        let closure_digest = inputs["compilerClosureDigest"]
            .as_str()
            .ok_or("no closure digest")?;
        release::validate_compiler_closure(&inputs, closure_digest)?;
        let approved: release::ApprovedRuntime = serde_json::from_value(
            inputs["compilerClosure"]["nativeCompiler"]["approvedRuntime"].clone(),
        )?;
        release::verify_controller_image(
            &outputs["image/controller.oci.tar"],
            &release::ControllerImage {
                image_digest: &built.image_digest,
                platform: "linux/amd64",
                ir_digest: IR_DIGEST,
                binary_digest: &built.binary_digest,
                approved_runtime: &approved,
            },
        )?;
        let sbom = String::from_utf8(outputs["sbom/sbom.json"].clone())?;
        assert!(sbom.contains("path+file:///build/controller#0.1.0"));
        assert!(!sbom.contains(&out.display().to_string()));
        Ok(outputs)
    }
}

/// Every regular file under `directory`, by `/`-separated path below `root`.
fn collect(directory: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) -> TestResult {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF-8 name")?;
        let relative = format!("{prefix}{name}");
        if entry.file_type()?.is_dir() {
            collect(&entry.path(), &format!("{relative}/"), files)?;
        } else {
            files.insert(relative, fs::read(entry.path())?);
        }
    }
    Ok(())
}

#[test]
fn builds_are_reproducible_and_engine_independent_apart_from_the_engine_identity() -> TestResult {
    let fixture = Fixture::new()?;
    let first = fixture.build("first", (container::Engine::Podman, &fixture.podman))?;
    let second = fixture.build("second", (container::Engine::Podman, &fixture.podman))?;
    assert_eq!(first, second, "two podman builds differ");

    let docker = fixture.build("docker", (container::Engine::Docker, &fixture.docker))?;
    for path in OUTPUTS.iter().filter(|path| **path != "build-inputs.json") {
        assert_eq!(first[*path], docker[*path], "{path} depends on the engine");
    }
    let podman_inputs: Value = serde_json::from_slice(&first["build-inputs.json"])?;
    let mut docker_inputs: Value = serde_json::from_slice(&docker["build-inputs.json"])?;
    let engine =
        |inputs: &Value| inputs["compilerClosure"]["nativeCompiler"]["hostTools"][0].clone();
    assert_eq!(engine(&podman_inputs)["role"], "engine");
    assert_ne!(engine(&podman_inputs), engine(&docker_inputs));
    assert_ne!(
        podman_inputs["compilerClosureDigest"],
        docker_inputs["compilerClosureDigest"]
    );
    docker_inputs["compilerClosure"]["nativeCompiler"]["hostTools"][0] = engine(&podman_inputs);
    docker_inputs["compilerClosureDigest"] = podman_inputs["compilerClosureDigest"].clone();
    assert_eq!(podman_inputs, docker_inputs);
    Ok(())
}
