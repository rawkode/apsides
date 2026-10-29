//! The controller SBOM: cargo-cyclonedx's `CycloneDX` 1.5 inventory with the
//! host build path mapped to `/build` and the `rusty_v8` archive, which cargo
//! never sees, declared by hand.

use crate::{
    Error, Result, Stage,
    cargo::HostCargo,
    command::{Env, Run},
    context::Context,
    digest::{HashingReader, digest},
    fs::{link_tree, read_regular, write_new},
    redact::{TAIL_LINES, diagnostic},
};
use release::{RustyV8, RustyV8Component};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

/// The Rust targets an SBOM can be generated for.
pub const TARGETS: [&str; 2] = ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"];
const GENERATE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RAW_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LOCK_BYTES: u64 = 16 * 1024 * 1024;

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::Sbom, detail)
}

/// Whether every byte of `path` is one of `A-Z a-z 0-9 / . _ -`.
fn safe_characters(path: &str) -> bool {
    !path.is_empty()
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
}

/// `path` as the build root of SBOM generation, if it is usable.
///
/// It must be absolute and made only of `A-Z a-z 0-9 / . _ -`: then neither
/// JSON escaping nor URL encoding can disguise it, and [`normalise`] can map
/// and scrub it exactly.
#[must_use]
pub fn safe_build_path(path: &Path) -> Option<&str> {
    path.to_str()
        .filter(|path| path.starts_with('/') && path.len() > 1 && safe_characters(path))
}

/// `value` percent-encoded as Python's `quote(value, safe="")`: ASCII letters,
/// digits and `_.-~` are kept and every other byte becomes `%XX` in uppercase
/// hex.
#[must_use]
pub fn percent_encode_all(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

/// The `CycloneDX` component declaring a `rusty_v8` archive or binding file.
#[must_use]
pub fn manual_component(archive: &RustyV8Component) -> Value {
    let checksum = archive.sha256.trim_start_matches("sha256:");
    let purl = format!(
        "pkg:generic/{}@{}?download_url={}&checksum=sha256:{checksum}",
        archive.name,
        archive.version,
        percent_encode_all(&archive.url)
    );
    json!({
        "type": "library",
        "bom-ref": purl,
        "name": archive.name,
        "version": archive.version,
        "description": format!(
            "Prebuilt rusty_v8 (v8 crate) static archive for {}, fetched and SHA-256-verified \
             outside the --network=none controller build (scripts/rusty-v8/pins.json).",
            archive.target
        ),
        "purl": purl,
        "hashes": [{"alg": "SHA-256", "content": checksum}],
        "externalReferences": [{"type": "distribution", "url": archive.url}],
    })
}

/// The canonical SBOM bytes for cargo-cyclonedx's `raw` output generated
/// under `build_root`.
///
/// Cargo package IDs carry absolute paths, so every occurrence of
/// `build_root` becomes `/build`, the compile container's root; every
/// component and dependency edge is kept. The `rusty_v8` components are
/// appended, and the result is compact, key-sorted, raw UTF-8 JSON.
///
/// # Errors
/// Fails when `build_root` is not a [`safe_build_path`], when `raw` is not a
/// `CycloneDX` 1.5 document with components and dependencies, and when the
/// result still holds `build_root`, plainly or percent-encoded.
pub fn normalise(raw: &str, build_root: &str, rusty_v8: &RustyV8) -> Result<Vec<u8>> {
    if safe_build_path(Path::new(build_root)) != Some(build_root) {
        return Err(fail(format!(
            "SBOM generation needs an absolute build path made only of A-Z a-z 0-9 / . _ -, not \
             {build_root:?}"
        )));
    }
    let mut document: Value = serde_json::from_str(&raw.replace(build_root, "/build"))
        .map_err(|error| fail(format!("cargo-cyclonedx output is not JSON: {error}")))?;
    if document["bomFormat"] != "CycloneDX" || document["specVersion"] != "1.5" {
        return Err(fail("cargo-cyclonedx output is not CycloneDX 1.5"));
    }
    let inventory = |key: &str| {
        document[key]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    };
    if !inventory("components") || !inventory("dependencies") {
        return Err(fail(
            "cargo-cyclonedx output omitted components or dependencies",
        ));
    }
    if let Some(components) = document["components"].as_array_mut() {
        components.push(manual_component(&rusty_v8.archive));
        if let Some(binding) = &rusty_v8.binding_file {
            components.push(manual_component(binding));
        }
    }
    let bytes = compiler_core::canonical_bytes(&document)
        .map_err(|error| fail(format!("cannot serialise the SBOM: {error}")))?;
    let encoded = percent_encode_all(build_root).to_ascii_lowercase();
    if contains(&bytes, build_root.as_bytes())
        || contains(&bytes.to_ascii_lowercase(), encoded.as_bytes())
    {
        return Err(fail(format!(
            "SBOM retains the host build path {build_root}"
        )));
    }
    Ok(bytes)
}

/// What [`generate`] needs.
pub struct SbomInputs<'a> {
    pub context: &'a Context,
    /// cargo-cyclonedx, as identified among the host tools.
    pub tool: &'a Path,
    /// The digest the build profile pins for `tool`.
    pub tool_sha256: &'a str,
    /// The Rust target triple, one of [`TARGETS`].
    pub target: &'a str,
    pub epoch: u64,
    pub cargo: &'a HostCargo,
    /// `<out>/sbom`, which must not exist.
    pub output: &'a Path,
}

/// Generates `<output>/sbom.json` for the context's controller and returns
/// its digest.
///
/// cargo-cyclonedx writes beside the manifest it reads, so it runs on a
/// hard-linked copy of the context, in a work directory whose path is made
/// only of characters [`normalise`] can map and scrub exactly. A write
/// through a link is caught by the caller's recheck of the context.
///
/// # Errors
/// Fails when the output exists, the tool's bytes differ from the profile,
/// the target is unsupported, no usable work directory exists, the tool
/// fails or changes the lock, or its output is not a usable SBOM.
pub fn generate(inputs: &SbomInputs<'_>) -> Result<String> {
    if fs::symlink_metadata(inputs.output).is_ok() {
        return Err(fail(format!(
            "{} already exists; choose a fresh --output (aps build never overwrites)",
            inputs.output.display()
        )));
    }
    let actual = file_digest(inputs.tool)?;
    if actual != inputs.tool_sha256 {
        return Err(fail(format!(
            "cargo-cyclonedx at {} is {actual}, not the build profile's sbomToolSha256 {}",
            inputs.tool.display(),
            inputs.tool_sha256
        )));
    }
    if !TARGETS.contains(&inputs.target) {
        return Err(fail(format!(
            "SBOM target {} is not one of {}",
            inputs.target,
            TARGETS.join(", ")
        )));
    }
    let work = work_directory(&inputs.context.root)?;
    let build = work.path().join("build");
    let build_root = safe_build_path(&build)
        .ok_or_else(|| {
            fail(format!(
                "SBOM generation needs a build path made only of A-Z a-z 0-9 / . _ -: {}",
                build.display()
            ))
        })?
        .to_owned();
    link_tree(&inputs.context.root, &build).map_err(|error| {
        fail(format!(
            "cannot link the build context into {}: {error}",
            build.display()
        ))
    })?;
    run_tool(inputs, &build)?;
    let lock = build.join("controller/Cargo.lock");
    let lock = read_regular(&lock, MAX_LOCK_BYTES)
        .map_err(|error| fail(format!("cannot read {}: {error}", lock.display())))?;
    if digest(&lock) != inputs.context.controller_lock_sha256 {
        return Err(fail("SBOM generation modified the dependency lock"));
    }
    let raw_path = build.join("controller/controller.cdx.json");
    let raw = read_regular(&raw_path, MAX_RAW_BYTES)
        .map_err(|error| fail(format!("cannot read {}: {error}", raw_path.display())))?;
    let raw = String::from_utf8(raw).map_err(|_| fail("cargo-cyclonedx output is not UTF-8"))?;
    let bytes = normalise(&raw, &build_root, &inputs.context.rusty_v8)?;
    fs::create_dir(inputs.output).map_err(|error| {
        fail(format!(
            "cannot create {}: {error}",
            inputs.output.display()
        ))
    })?;
    let sbom = inputs.output.join("sbom.json");
    write_new(&sbom, &bytes, 0o644)
        .map_err(|error| fail(format!("cannot write {}: {error}", sbom.display())))?;
    work.close()
        .map_err(|error| fail(format!("cannot remove the SBOM work directory: {error}")))?;
    Ok(digest(&bytes))
}

/// Runs cargo-cyclonedx on the linked context at `build`, offline, with a
/// fixed environment.
fn run_tool(inputs: &SbomInputs<'_>, build: &Path) -> Result<()> {
    let mut environment = vec![
        ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
        ("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()),
        ("SOURCE_DATE_EPOCH".to_owned(), inputs.epoch.to_string()),
        ("LANG".to_owned(), "C".to_owned()),
        ("TZ".to_owned(), "UTC".to_owned()),
    ];
    environment.extend(inputs.cargo.environment());
    let manifest = build.join("controller/Cargo.toml");
    Run::new(
        inputs.tool,
        [
            "cyclonedx".into(),
            "--manifest-path".into(),
            manifest.into_os_string(),
            "--format".into(),
            "json".into(),
            "--spec-version".into(),
            "1.5".into(),
            "--all".into(),
            "--target".into(),
            inputs.target.into(),
        ],
    )
    .cwd(build)
    .env(Env::Clear(environment))
    .timeout(GENERATE_TIMEOUT)
    .capture()
    .map_err(|failure| {
        let mut detail = failure.code().map_or_else(
            || failure.describe("cargo-cyclonedx", TAIL_LINES),
            |code| format!("cargo-cyclonedx failed (exit {code})"),
        );
        if failure.code().is_some() && !failure.tail.trim().is_empty() {
            detail.push_str(": ");
            detail.push_str(&diagnostic(&failure.tail));
        }
        fail(detail)
    })?;
    Ok(())
}

/// A fresh work directory beside the build context, or else in the system
/// temporary directory, whose canonical path [`safe_build_path`] accepts.
fn work_directory(context_root: &Path) -> Result<tempfile::TempDir> {
    let candidates: Vec<PathBuf> = context_root
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .chain([std::env::temp_dir()])
        .collect();
    for candidate in &candidates {
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if safe_build_path(&canonical.join("build")).is_none() {
            continue;
        }
        if let Ok(work) = tempfile::Builder::new()
            .prefix(".apsides-sbom-")
            .tempdir_in(&canonical)
        {
            return Ok(work);
        }
    }
    Err(fail(format!(
        "SBOM generation needs a writable work directory whose path holds only A-Z a-z 0-9 / . _ \
         -; neither of {} qualifies (set TMPDIR to such a directory)",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

fn file_digest(path: &Path) -> Result<String> {
    let unreadable = |error: io::Error| fail(format!("cannot read {}: {error}", path.display()));
    let mut reader = HashingReader::new(File::open(path).map_err(unreadable)?);
    io::copy(&mut reader, &mut io::sink()).map_err(unreadable)?;
    Ok(reader.finish().0)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    const ROOT: &str = "/tmp/apsides-sbom-x1/build";
    const HEX: &str = "f48762ca10d1f1fc605a441c5ae430ec8ce1e9e80f14d78fbc42cb878c30b476";

    fn rusty_v8() -> RustyV8 {
        RustyV8 {
            archive: RustyV8Component {
                name: "librusty_v8".into(),
                version: "150.4.0".into(),
                target: "x86_64-unknown-linux-gnu".into(),
                url: "https://github.com/denoland/rusty_v8/releases/download/v150.4.0/librusty_v8_simdutf_release_x86_64-unknown-linux-gnu.a.gz".into(),
                sha256: format!("sha256:{HEX}"),
                path: "rusty-v8/librusty_v8.a.gz".into(),
            },
            binding_file: None,
        }
    }

    fn raw(root: &str) -> String {
        json!({
            "bomFormat": "CycloneDX",
            "specVersion": "1.5",
            "version": 1,
            "metadata": {"component": {"bom-ref": format!("path+file://{root}/controller#0.1.0"), "name": "controller"}},
            "components": [
                {"type": "library", "bom-ref": format!("path+file://{root}/workspace/crates/runtime#0.1.0"), "name": "runtime", "version": "0.1.0"},
                {"type": "library", "bom-ref": "registry+https://github.com/rust-lang/crates.io-index#tokio@1.0.0", "name": "tökio", "version": "1.0.0"},
            ],
            "dependencies": [{"ref": format!("path+file://{root}/controller#0.1.0"), "dependsOn": []}],
        })
        .to_string()
    }

    #[test]
    fn the_build_root_is_mapped_and_the_archive_declared() -> TestResult {
        let bytes = normalise(&raw(ROOT), ROOT, &rusty_v8())?;
        let text = String::from_utf8(bytes.clone())?;
        assert!(!text.contains("/tmp/"));
        assert!(text.contains(r#""bom-ref":"path+file:///build/workspace/crates/runtime#0.1.0""#));
        assert!(text.contains("tökio"), "non-ASCII text stays raw UTF-8");
        let document: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(compiler_core::canonical_bytes(&document)?, bytes);
        let components = document["components"].as_array().ok_or("no components")?;
        assert_eq!(components.len(), 3);
        let purl = format!(
            "pkg:generic/librusty_v8@150.4.0?download_url=https%3A%2F%2Fgithub.com%2Fdenoland%2Frusty_v8%2Freleases%2Fdownload%2Fv150.4.0%2Flibrusty_v8_simdutf_release_x86_64-unknown-linux-gnu.a.gz&checksum=sha256:{HEX}"
        );
        assert_eq!(
            components[2],
            json!({
                "type": "library",
                "bom-ref": purl,
                "name": "librusty_v8",
                "version": "150.4.0",
                "description": "Prebuilt rusty_v8 (v8 crate) static archive for x86_64-unknown-linux-gnu, fetched and SHA-256-verified outside the --network=none controller build (scripts/rusty-v8/pins.json).",
                "purl": purl,
                "hashes": [{"alg": "SHA-256", "content": HEX}],
                "externalReferences": [{"type": "distribution", "url": rusty_v8().archive.url}],
            })
        );
        Ok(())
    }

    #[test]
    fn a_binding_file_is_declared_too() -> TestResult {
        let mut record = rusty_v8();
        let mut binding = record.archive.clone();
        binding.name = "src_binding".into();
        record.binding_file = Some(binding);
        let document: Value = serde_json::from_slice(&normalise(&raw(ROOT), ROOT, &record)?)?;
        assert_eq!(document["components"][3]["name"], "src_binding");
        Ok(())
    }

    #[test]
    fn a_retained_host_path_is_rejected() -> TestResult {
        let encoded = raw(ROOT).replace(
            r#""version":1"#,
            &format!(
                r#""version":1,"x":"file%3A%2F%2F{}""#,
                percent_encode_all(ROOT).trim_start_matches("%2F")
            ),
        );
        let error = normalise(&encoded, ROOT, &rusty_v8())
            .err()
            .ok_or("accepted")?;
        assert_eq!(
            error.detail,
            format!("SBOM retains the host build path {ROOT}")
        );
        let lowercase = raw(ROOT).replace(
            r#""version":1"#,
            r#""version":1,"x":"%2ftmp%2fapsides-sbom-x1%2fbuild""#,
        );
        assert!(normalise(&lowercase, ROOT, &rusty_v8()).is_err());
        Ok(())
    }

    #[test]
    fn unusable_build_paths_are_rejected() {
        for root in [
            "/tmp/with space/build",
            "relative/build",
            "/tmp/quote\"/build",
            "/",
            "/tmp/ü/build",
        ] {
            assert_eq!(safe_build_path(Path::new(root)), None, "{root}");
            let error = normalise(&raw(root), root, &rusty_v8()).err();
            assert!(
                error.is_some_and(|error| error
                    .detail
                    .starts_with("SBOM generation needs an absolute build path")),
                "{root}"
            );
        }
        assert_eq!(safe_build_path(Path::new(ROOT)), Some(ROOT));
    }

    #[test]
    fn documents_other_than_cyclonedx_1_5_with_an_inventory_are_rejected() -> TestResult {
        let base: Value = serde_json::from_str(&raw(ROOT))?;
        let cases = [
            ("bomFormat", json!("SPDX"), "not CycloneDX 1.5"),
            ("specVersion", json!("1.4"), "not CycloneDX 1.5"),
            (
                "components",
                json!([]),
                "omitted components or dependencies",
            ),
            (
                "dependencies",
                Value::Null,
                "omitted components or dependencies",
            ),
        ];
        for (key, value, expected) in cases {
            let mut document = base.clone();
            document[key] = value;
            let error = normalise(&document.to_string(), ROOT, &rusty_v8()).err();
            assert!(
                error.is_some_and(|error| error.detail.contains(expected)),
                "{key}"
            );
        }
        assert!(normalise("not json", ROOT, &rusty_v8()).is_err());
        Ok(())
    }

    #[test]
    fn percent_encoding_matches_python_quote_with_nothing_safe() {
        assert_eq!(
            percent_encode_all("https://a.b/c_d-e~f?g=h&i"),
            "https%3A%2F%2Fa.b%2Fc_d-e~f%3Fg%3Dh%26i"
        );
        assert_eq!(percent_encode_all("é "), "%C3%A9%20");
    }

    mod generation {
        use super::super::*;
        use crate::{Stage, context::tests::Fixture, tools::fake_tool};
        use std::path::PathBuf;

        type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

        /// A fake cargo-cyclonedx writing a minimal SBOM that names its
        /// build root, after running `extra`.
        fn tool(directory: &Path, extra: &str) -> TestResult<(PathBuf, String)> {
            let script = format!(
                "{extra}\n\
                 printf '{{\"bomFormat\":\"CycloneDX\",\"specVersion\":\"1.5\",\"components\":[{{\"bom-ref\":\"path+file://%s/workspace/crates/runtime#0.1.0\"}}],\"dependencies\":[{{\"ref\":\"x\"}}],\"env\":\"%s\"}}' \"$PWD\" \"$CARGO_NET_OFFLINE $SOURCE_DATE_EPOCH $LANG $TZ ${{HOME:-nohome}}\" > controller/controller.cdx.json"
            );
            let path = fake_tool(directory, "cargo-cyclonedx", &script)?;
            let sha = digest(&fs::read(&path)?);
            Ok((path, sha))
        }

        fn run(fixture: &Fixture, context: &Context, extra: &str) -> TestResult<Result<String>> {
            let (tool, sha) = tool(fixture.directory.path(), extra)?;
            Ok(generate(&SbomInputs {
                context,
                tool: &tool,
                tool_sha256: &sha,
                target: "x86_64-unknown-linux-gnu",
                epoch: 86400,
                cargo: &fixture.cargo,
                output: &fixture.artifact.join("sbom"),
            }))
        }

        #[test]
        fn the_sbom_is_written_with_the_build_root_mapped() -> TestResult {
            let fixture = Fixture::new()?;
            let context = fixture.prepare()?;
            let sbom_digest = run(&fixture, &context, "")??;
            let bytes = fs::read(fixture.artifact.join("sbom/sbom.json"))?;
            assert_eq!(sbom_digest, digest(&bytes));
            let text = String::from_utf8(bytes)?;
            assert!(
                text.contains("path+file:///build/workspace/crates/runtime#0.1.0"),
                "{text}"
            );
            assert!(
                text.contains("\"env\":\"true 86400 C UTC nohome\""),
                "{text}"
            );
            assert!(text.contains("librusty_v8"));
            context.verify_unchanged(Stage::Sbom, "SBOM generation")?;
            assert!(
                fixture
                    .leftovers()?
                    .iter()
                    .all(|name| !name.starts_with(".apsides-sbom-"))
            );
            let again = run(&fixture, &context, "")?
                .err()
                .ok_or("overwrote the SBOM")?;
            assert!(
                again.detail.ends_with(
                    "already exists; choose a fresh --output (aps build never overwrites)"
                )
            );
            Ok(())
        }

        #[test]
        fn a_lock_written_through_the_link_is_detected() -> TestResult {
            let fixture = Fixture::new()?;
            let context = fixture.prepare()?;
            let error = run(
                &fixture,
                &context,
                "chmod u+w controller/Cargo.lock && echo '# x' >> controller/Cargo.lock",
            )?
            .err()
            .ok_or("accepted")?;
            assert_eq!(error.detail, "SBOM generation modified the dependency lock");
            assert!(
                context
                    .verify_unchanged(Stage::Sbom, "SBOM generation")
                    .is_err()
            );
            Ok(())
        }

        #[test]
        fn a_context_written_through_the_link_is_detected() -> TestResult {
            let fixture = Fixture::new()?;
            let context = fixture.prepare()?;
            run(
                &fixture,
                &context,
                "chmod u+w controller/src/ir.json && echo '{\"x\":1}' > controller/src/ir.json",
            )??;
            let error = context
                .verify_unchanged(Stage::Sbom, "SBOM generation")
                .err()
                .ok_or("accepted")?;
            assert!(
                error.detail.contains("changed controller/src/ir.json"),
                "{}",
                error.detail
            );
            Ok(())
        }

        #[test]
        fn a_failing_tool_or_foreign_bytes_are_rejected() -> TestResult {
            let fixture = Fixture::new()?;
            let context = fixture.prepare()?;
            let error = run(&fixture, &context, "echo 'token=abcdef broken' >&2; exit 3")?
                .err()
                .ok_or("accepted")?;
            assert!(
                error
                    .detail
                    .starts_with("cargo-cyclonedx failed (exit 3): "),
                "{}",
                error.detail
            );
            assert!(error.detail.contains("token=[REDACTED]"));
            let (tool, _) = tool(fixture.directory.path(), "")?;
            let error = generate(&SbomInputs {
                context: &context,
                tool: &tool,
                tool_sha256: &digest(b"other"),
                target: "x86_64-unknown-linux-gnu",
                epoch: 0,
                cargo: &fixture.cargo,
                output: &fixture.artifact.join("sbom"),
            })
            .err()
            .ok_or("accepted")?;
            assert!(
                error
                    .detail
                    .contains("not the build profile's sbomToolSha256")
            );
            Ok(())
        }

        /// A context beside a path with a space falls back to the system
        /// temporary directory, and the space never reaches the SBOM.
        #[test]
        fn a_build_path_with_a_space_falls_back_to_the_temporary_directory() -> TestResult {
            let fixture = Fixture::build("", "with space")?;
            let context = fixture.prepare()?;
            run(&fixture, &context, "")??;
            let text = fs::read_to_string(fixture.artifact.join("sbom/sbom.json"))?;
            assert!(text.contains("path+file:///build/workspace"), "{text}");
            assert!(!text.contains("with space"));
            assert!(
                fixture
                    .leftovers()?
                    .iter()
                    .all(|name| !name.starts_with(".apsides-sbom-"))
            );
            Ok(())
        }
    }
}
