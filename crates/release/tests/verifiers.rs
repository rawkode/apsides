//! The public producer self-checks: each verifier on its own, without a
//! release directory around it.
use compiler_core::canonical_bytes;
use release::{
    ApprovedRuntime, BuilderImage, ControllerImage, controller_runtime_config,
    validate_approved_runtime, validate_controller_binary, verify_controller_image,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const IR_DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn tar(files: Vec<(&str, &[u8], bool)>) -> TestResult<Vec<u8>> {
    moded_tar(
        files
            .into_iter()
            .map(|(path, bytes, directory)| (path, bytes, directory, 0o755))
            .collect(),
    )
}

/// A tar whose member names are written verbatim, so a test can build a
/// path the `tar` builder itself would refuse.
fn moded_tar(files: Vec<(&str, &[u8], bool, u32)>) -> TestResult<Vec<u8>> {
    let mut archive = tar::Builder::new(Vec::new());
    for (path, bytes, directory, mode) in files {
        let mut header = tar::Header::new_gnu();
        let name = &mut header.as_gnu_mut().ok_or("expected a GNU header")?.name;
        let raw = path.as_bytes();
        if raw.len() > name.len() {
            return Err("tar member name too long".into());
        }
        name[..raw.len()].copy_from_slice(raw);
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(if directory {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        header.set_cksum();
        archive.append(&header, bytes)?;
    }
    Ok(archive.into_inner()?)
}

fn controller_entries(binary: &[u8]) -> Vec<(&str, &[u8], bool, u32)> {
    vec![
        ("usr", b"", true, 0o755),
        ("usr/local", b"", true, 0o755),
        ("usr/local/bin", b"", true, 0o755),
        ("usr/local/bin/apsides-controller", binary, false, 0o755),
    ]
}

fn elf(machine: u8) -> Vec<u8> {
    let mut binary = vec![0_u8; 64];
    binary[..6].copy_from_slice(b"\x7fELF\x02\x01");
    binary[18] = machine;
    binary
}

fn base_config() -> TestResult<Vec<u8>> {
    let layer = tar(vec![])?;
    Ok(canonical_bytes(&json!({
        "architecture":"arm64","os":"linux",
        "config":{"Cmd":["/bin/sh"],"Env":["PATH=/usr/bin"],"Labels":{"vendor":"example"}},
        "rootfs":{"type":"layers","diff_ids":[hash(&layer)]}
    }))?)
}

fn approved_runtime() -> TestResult<ApprovedRuntime> {
    let layer = tar(vec![])?;
    let config = base_config()?;
    let manifest = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&layer),"size":layer.len()}]
    }))?;
    let index = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
        "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":hash(&manifest),"size":manifest.len(),"platform":{"os":"linux","architecture":"arm64"}}]
    }))?;
    Ok(ApprovedRuntime {
        base_image: BuilderImage {
            reference: format!("example.invalid/runtime@{}", hash(&index)),
            platform: "linux/arm64".into(),
        },
        index_json: String::from_utf8(index)?,
        manifest_json: String::from_utf8(manifest)?,
        config_json: String::from_utf8(config)?,
    })
}

/// A controller layout over [`approved_runtime`]'s base, returned with its
/// manifest digest.
fn controller_layout(binary: &[u8], runtime_config: &Value) -> TestResult<(Vec<u8>, String)> {
    layout_with_final_layer(&moded_tar(controller_entries(binary))?, runtime_config)
}

/// A layout over [`approved_runtime`]'s base whose final layer is `layer`,
/// returned with its manifest digest.
fn layout_with_final_layer(layer: &[u8], runtime_config: &Value) -> TestResult<(Vec<u8>, String)> {
    let base = tar(vec![])?;
    let layer = layer.to_vec();
    let config = canonical_bytes(&json!({
        "architecture":"arm64","os":"linux","config":runtime_config,
        "rootfs":{"type":"layers","diff_ids":[hash(&base),hash(&layer)]}
    }))?;
    let manifest = canonical_bytes(&json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":hash(&config),"size":config.len()},
        "layers":[
            {"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&base),"size":base.len()},
            {"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":hash(&layer),"size":layer.len()}
        ]
    }))?;
    let index = canonical_bytes(&json!({
        "schemaVersion":2,
        "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":hash(&manifest),"size":manifest.len()}]
    }))?;
    let blob = |bytes: &[u8]| format!("blobs/sha256/{}", &hash(bytes)[7..]);
    let (base_path, layer_path, config_path, manifest_path) =
        (blob(&base), blob(&layer), blob(&config), blob(&manifest));
    let archive = tar(vec![
        ("oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#, false),
        ("index.json", &index, false),
        (&base_path, &base, false),
        (&layer_path, &layer, false),
        (&config_path, &config, false),
        (&manifest_path, &manifest, false),
    ])?;
    Ok((archive, hash(&manifest)))
}

fn expected_runtime_config() -> TestResult<Value> {
    let base: Value = serde_json::from_slice(&base_config()?)?;
    Ok(controller_runtime_config(&base, IR_DIGEST)?)
}

#[test]
fn approved_runtime_yields_its_base_config() -> TestResult {
    let config = validate_approved_runtime(&approved_runtime()?)?;
    assert_eq!(config, serde_json::from_slice::<Value>(&base_config()?)?);
    Ok(())
}

#[test]
fn approved_runtime_disconnected_from_its_reference_is_rejected() -> TestResult {
    let mut runtime = approved_runtime()?;
    runtime.base_image.reference = format!("example.invalid/runtime@{}", hash(b"other"));
    let error = validate_approved_runtime(&runtime)
        .err()
        .ok_or("expected a rejection")?;
    assert_eq!(
        error.to_string(),
        "release rejected: approved runtime index differs from its immutable image reference"
    );
    Ok(())
}

#[test]
fn runtime_config_applies_the_controller_contract() -> TestResult {
    assert_eq!(
        expected_runtime_config()?,
        json!({
            "Entrypoint":["/usr/local/bin/apsides-controller"],
            "Env":["PATH=/usr/bin"],
            "Labels":{
                "dev.apsides.ir-digest":IR_DIGEST,
                "dev.apsides.release-status":"blocked",
                "org.opencontainers.image.title":"Apsides generated controller",
                "vendor":"example"
            },
            "User":"65532:65532",
            "WorkingDir":"/"
        })
    );
    let error = controller_runtime_config(&json!({}), IR_DIGEST)
        .err()
        .ok_or("expected a rejection")?;
    assert_eq!(
        error.to_string(),
        "release rejected: approved base runtime config missing"
    );
    Ok(())
}

#[test]
fn controller_image_matching_its_identity_verifies() -> TestResult {
    let runtime = approved_runtime()?;
    let binary = elf(183);
    let (archive, image_digest) = controller_layout(&binary, &expected_runtime_config()?)?;
    let binary_digest = hash(&binary);
    let expected = ControllerImage {
        image_digest: &image_digest,
        platform: "linux/arm64",
        ir_digest: IR_DIGEST,
        binary_digest: &binary_digest,
        approved_runtime: &runtime,
    };
    verify_controller_image(&archive, &expected)?;

    let other_binary = hash(b"other");
    let error = verify_controller_image(
        &archive,
        &ControllerImage {
            binary_digest: &other_binary,
            ..expected
        },
    )
    .err()
    .ok_or("expected a rejection")?;
    assert_eq!(
        error.to_string(),
        "release rejected: OCI controller bytes differ from the signed native binary"
    );

    let error = verify_controller_image(
        &archive,
        &ControllerImage {
            platform: "linux/amd64",
            ..expected
        },
    )
    .err()
    .ok_or("expected a rejection")?;
    assert_eq!(
        error.to_string(),
        "release rejected: OCI image runtime command, user or platform differs from the native controller contract"
    );
    Ok(())
}

#[test]
fn controller_image_with_a_foreign_runtime_config_is_rejected() -> TestResult {
    let runtime = approved_runtime()?;
    let binary = elf(183);
    let mut config = expected_runtime_config()?;
    config["User"] = json!("0:0");
    let (archive, image_digest) = controller_layout(&binary, &config)?;
    let binary_digest = hash(&binary);
    let error = verify_controller_image(
        &archive,
        &ControllerImage {
            image_digest: &image_digest,
            platform: "linux/arm64",
            ir_digest: IR_DIGEST,
            binary_digest: &binary_digest,
            approved_runtime: &runtime,
        },
    )
    .err()
    .ok_or("expected a rejection")?;
    assert_eq!(
        error.to_string(),
        "release rejected: OCI image runtime command, user or platform differs from the native controller contract"
    );
    Ok(())
}

#[test]
fn controller_image_with_a_tampered_final_layer_is_rejected() -> TestResult {
    let runtime = approved_runtime()?;
    let binary = elf(183);
    let binary_digest = hash(&binary);
    let config = expected_runtime_config()?;

    let mut extra = controller_entries(&binary);
    extra.push(("etc", b"", true, 0o755));
    extra.push(("etc/passwd", b"root:x:0:0::/:/bin/sh\n", false, 0o644));
    let mut preload = controller_entries(&binary);
    preload.push(("etc/ld.so.preload", b"/tmp/evil.so\n", false, 0o644));
    let mut setuid = controller_entries(&binary);
    setuid[3].3 = 0o4755;
    let mut setgid = controller_entries(&binary);
    setgid[3].3 = 0o2755;
    let mut escape = controller_entries(&binary);
    escape.push(("usr/local/bin/../../../../outside", b"x", false, 0o644));

    for (entries, message) in [
        (
            extra,
            "release rejected: final compiler image layer contains unexpected entries",
        ),
        (
            preload,
            "release rejected: compiler image runtime may not install a dynamic-loader preload file",
        ),
        (
            setuid,
            "release rejected: OCI controller is duplicated, non-regular, privileged or not executable by its runtime user",
        ),
        (
            setgid,
            "release rejected: OCI controller is duplicated, non-regular, privileged or not executable by its runtime user",
        ),
        (escape, "release rejected: OCI layer path escapes its root"),
    ] {
        let (archive, image_digest) = layout_with_final_layer(&moded_tar(entries)?, &config)?;
        let error = verify_controller_image(
            &archive,
            &ControllerImage {
                image_digest: &image_digest,
                platform: "linux/arm64",
                ir_digest: IR_DIGEST,
                binary_digest: &binary_digest,
                approved_runtime: &runtime,
            },
        )
        .err()
        .ok_or("expected a rejection")?;
        assert_eq!(error.to_string(), message);
    }
    Ok(())
}

#[test]
fn controller_binary_must_match_its_platform() -> TestResult {
    validate_controller_binary(&elf(62), "linux/amd64")?;
    validate_controller_binary(&elf(183), "linux/arm64")?;
    for (binary, platform, message) in [
        (
            elf(183),
            "linux/amd64",
            "release rejected: native controller is not a Linux ELF binary for the declared image architecture",
        ),
        (
            elf(62)[..63].to_vec(),
            "linux/amd64",
            "release rejected: native controller is not a Linux ELF binary for the declared image architecture",
        ),
        (
            elf(62),
            "linux/riscv64",
            "release rejected: unsupported controller architecture",
        ),
    ] {
        let error = validate_controller_binary(&binary, platform)
            .err()
            .ok_or("expected a rejection")?;
        assert_eq!(error.to_string(), message);
    }
    Ok(())
}

#[test]
fn controller_image_over_an_invalid_approved_runtime_is_rejected() -> TestResult {
    let mut runtime = approved_runtime()?;
    let binary = elf(183);
    let (archive, image_digest) = controller_layout(&binary, &expected_runtime_config()?)?;
    let binary_digest = hash(&binary);
    runtime.base_image.reference = format!("example.invalid/runtime@{}", hash(b"other"));
    let error = verify_controller_image(
        &archive,
        &ControllerImage {
            image_digest: &image_digest,
            platform: "linux/arm64",
            ir_digest: IR_DIGEST,
            binary_digest: &binary_digest,
            approved_runtime: &runtime,
        },
    )
    .err()
    .ok_or("expected a rejection")?;
    assert_eq!(
        error.to_string(),
        "release rejected: approved runtime index differs from its immutable image reference"
    );
    Ok(())
}
