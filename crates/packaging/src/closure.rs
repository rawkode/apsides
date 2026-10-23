//! The compiler closure: the record of the packaging code, and the
//! `build-inputs.json` the source compiler wrote, completed with the native
//! compiler that ran.

use crate::{
    Error, Result, Stage,
    fs::{read_regular, write_new},
    sources,
};
use compiler_core::MAX_BUILD_INPUTS_BYTES;
use release::{COMPILER_CLOSURE_SCHEMA, CompilerFile, NativeCompiler};
use serde_json::Value;
use std::{fs, path::Path};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/packaging_files.rs"));
}

/// The packaging sources this `aps` was built from, as their digests under
/// `assets` confirm.
///
/// The digests are embedded at compile time; reading `assets` proves the
/// sources there are the ones the running code came from, so the closure
/// names the code that actually ran.
///
/// # Errors
/// Fails when the sources under `assets` cannot be read or differ from the
/// embedded ones.
pub fn packaging_files(assets: &Path) -> Result<Vec<CompilerFile>> {
    let current = sources::collect(assets).map_err(|error| {
        Error::new(
            Stage::Closure,
            format!(
                "cannot read the packaging sources under {}: {error}",
                assets.display()
            ),
        )
    })?;
    compare(&current, embedded::EMBEDDED, assets)?;
    Ok(current
        .into_iter()
        .map(|(path, sha256)| CompilerFile { path, sha256 })
        .collect())
}

/// What [`finalize`] needs.
pub struct FinalizeInputs<'a> {
    /// The source compiler's `build-inputs.json`, whose closure has no
    /// native compiler yet.
    pub stage_inputs: &'a Path,
    /// `<out>/build-inputs.json`, which must not exist.
    pub output: &'a Path,
    pub native: &'a NativeCompiler,
}

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::Closure, detail)
}

/// Completes the source compiler's closure with `native`, checks the result
/// as `aps release` will, and writes it to `output` as canonical bytes.
/// Returns the compiler closure digest.
///
/// # Errors
/// Fails when the output exists, the source receipt is unreadable, carries
/// another closure schema or already names a native compiler, release
/// validation rejects the completed closure, or the output cannot be
/// written.
pub fn finalize(inputs: &FinalizeInputs<'_>) -> Result<String> {
    if fs::symlink_metadata(inputs.output).is_ok() {
        return Err(fail(format!(
            "{} already exists; choose a fresh --output (aps build never overwrites)",
            inputs.output.display()
        )));
    }
    let source =
        read_regular(inputs.stage_inputs, MAX_BUILD_INPUTS_BYTES as u64).map_err(|error| {
            fail(format!(
                "cannot read {}: {error}",
                inputs.stage_inputs.display()
            ))
        })?;
    let source = String::from_utf8(source)
        .map_err(|_| fail(format!("{} is not UTF-8", inputs.stage_inputs.display())))?;
    let mut document =
        compiler_core::parse_bounded(&source, MAX_BUILD_INPUTS_BYTES, "build-inputs.json")
            .map_err(|error| {
                fail(format!(
                    "{} is not a usable source receipt: {error}",
                    inputs.stage_inputs.display()
                ))
            })?;
    let closure = document
        .get_mut("compilerClosure")
        .filter(|closure| closure.is_object())
        .ok_or_else(|| {
            fail(format!(
                "{} has no compiler closure",
                inputs.stage_inputs.display()
            ))
        })?;
    let schema = closure["schemaVersion"].as_str().unwrap_or_default();
    if schema != COMPILER_CLOSURE_SCHEMA {
        return Err(fail(format!(
            "the source compiler wrote compiler closure schema {schema:?}; this aps expects \
             {COMPILER_CLOSURE_SCHEMA}. The Deno compiler and aps are out of step."
        )));
    }
    if !closure["nativeCompiler"].is_null() {
        return Err(fail("source receipt already has a native compiler closure"));
    }
    closure["nativeCompiler"] = serde_json::to_value(inputs.native)
        .map_err(|error| fail(format!("cannot serialise the native compiler: {error}")))?;
    let digest = compiler_core::digest(closure)
        .map_err(|error| fail(format!("cannot digest the compiler closure: {error}")))?;
    document["compilerClosureDigest"] = Value::String(digest.clone());
    release::validate_compiler_closure(&document, &digest).map_err(|error| {
        fail(format!(
            "release validation rejected the compiler closure: {error}"
        ))
    })?;
    let bytes = compiler_core::canonical_bytes(&document)
        .map_err(|error| fail(format!("cannot serialise build-inputs.json: {error}")))?;
    if bytes.len() > MAX_BUILD_INPUTS_BYTES {
        return Err(fail(format!(
            "the completed build-inputs.json is {} bytes, over the {MAX_BUILD_INPUTS_BYTES}-byte \
             limit aps release accepts",
            bytes.len()
        )));
    }
    write_new(inputs.output, &bytes, 0o644)
        .map_err(|error| fail(format!("cannot write {}: {error}", inputs.output.display())))?;
    Ok(digest)
}

fn compare(current: &[(String, String)], built: &[(&str, &str)], assets: &Path) -> Result<()> {
    let first_difference = current
        .iter()
        .map(|(path, sha)| (path.as_str(), sha.as_str()))
        .zip(built.iter().copied())
        .find(|(now, then)| now != then)
        .map(|(now, then)| now.0.min(then.0).to_owned())
        .or_else(|| match current.len().cmp(&built.len()) {
            std::cmp::Ordering::Greater => current.get(built.len()).map(|(path, _)| path.clone()),
            std::cmp::Ordering::Less => {
                built.get(current.len()).map(|(path, _)| (*path).to_owned())
            }
            std::cmp::Ordering::Equal => None,
        });
    first_difference.map_or(Ok(()), |path| {
        Err(Error::new(
            Stage::Closure,
            format!(
                "this aps was built from different packaging sources than {} (first difference: \
                 {path}); rebuild aps",
                assets.display()
            ),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn checkout() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn sha(digit: char) -> String {
        format!("sha256:{}", digit.to_string().repeat(64))
    }

    /// A native compiler release validation accepts.
    fn native() -> std::result::Result<NativeCompiler, Box<dyn std::error::Error>> {
        let file = |path: &str, sha256: String| CompilerFile {
            path: path.to_owned(),
            sha256,
        };
        Ok(NativeCompiler {
            builder_image: release::BuilderImage {
                reference: format!("example.invalid/builder@{}", sha('3')),
                platform: "linux/arm64".into(),
            },
            cargo_lock_sha256: sha('4'),
            build_context_files: vec![
                file(".cargo/config.toml", sha('1')),
                file("controller/Cargo.lock", sha('4')),
                file("controller/Cargo.toml", sha('5')),
                file("controller/src/ir.json", sha('6')),
                file("controller/src/main.rs", sha('7')),
                file("rusty-v8/librusty_v8.a.gz", sha('9')),
                file("vendor/itoa-1.0.0/src/lib.rs", sha('8')),
                file("workspace/Cargo.toml", sha('a')),
            ],
            packaging_files: packaging_files(&checkout())?,
            build_profile_sha256: sha('b'),
            host_tools: crate::tools::ROLES
                .iter()
                .map(|role| release::HostTool {
                    role: (*role).to_owned(),
                    sha256: sha('c'),
                    version: format!("{role} 1.0"),
                })
                .collect(),
            approved_runtime: crate::fixtures::approved_runtime(&[crate::fixtures::base_layer()?])?,
            rusty_v8: release::RustyV8 {
                archive: release::RustyV8Component {
                    name: "librusty_v8".into(),
                    version: "150.4.0".into(),
                    target: "aarch64-unknown-linux-gnu".into(),
                    url: "https://example.invalid/librusty_v8.a.gz".into(),
                    sha256: sha('9'),
                    path: crate::rusty_v8::CONTEXT_PATH.into(),
                },
                binding_file: None,
            },
        })
    }

    /// A source receipt as the Deno compiler writes it, with `schema` and
    /// `native` as its closure's schema and native compiler.
    fn receipt(directory: &Path, schema: &str, native: &serde_json::Value) -> TestResult {
        let closure = serde_json::json!({
            "schemaVersion": schema,
            "authoringCompiler": {"sdk": {"identity": "@apsides/kubernetes"}, "toolchain": {"deno": "pinned"}},
            "nativeCompiler": native,
        });
        let document = serde_json::json!({
            "sdk": {"identity": "@apsides/kubernetes"},
            "toolchain": {"deno": "pinned"},
            "compilerClosure": closure,
            "compilerClosureDigest": compiler_core::digest(&closure)?,
        });
        fs::write(
            directory.join("build-inputs.json"),
            compiler_core::canonical_bytes(&document)?,
        )?;
        Ok(())
    }

    #[test]
    fn a_completed_closure_is_canonical_and_release_valid() -> TestResult {
        let directory = tempfile::tempdir()?;
        receipt(directory.path(), COMPILER_CLOSURE_SCHEMA, &Value::Null)?;
        let native = native()?;
        let output = directory.path().join("out.json");
        let digest = finalize(&FinalizeInputs {
            stage_inputs: &directory.path().join("build-inputs.json"),
            output: &output,
            native: &native,
        })?;
        let bytes = fs::read(&output)?;
        assert_ne!(bytes.last(), Some(&b'\n'));
        let document: Value = serde_json::from_slice(&bytes)?;
        assert_eq!(bytes, compiler_core::canonical_bytes(&document)?);
        assert_eq!(document["compilerClosureDigest"], digest.as_str());
        assert_eq!(compiler_core::digest(&document["compilerClosure"])?, digest);
        assert_eq!(
            document["compilerClosure"]["nativeCompiler"],
            serde_json::to_value(&native)?
        );
        assert_eq!(
            release::validate_compiler_closure(&document, &digest)?,
            digest
        );
        let error = finalize(&FinalizeInputs {
            stage_inputs: &directory.path().join("build-inputs.json"),
            output: &output,
            native: &native,
        })
        .err()
        .ok_or("an existing output was overwritten")?;
        assert!(error.detail.ends_with("(aps build never overwrites)"));
        Ok(())
    }

    #[test]
    fn a_receipt_out_of_step_or_already_complete_is_rejected() -> TestResult {
        let native = native()?;
        let directory = tempfile::tempdir()?;
        receipt(
            directory.path(),
            "apsides.dev/compiler-closure/v3",
            &Value::Null,
        )?;
        let output = directory.path().join("out.json");
        let inputs = FinalizeInputs {
            stage_inputs: &directory.path().join("build-inputs.json"),
            output: &output,
            native: &native,
        };
        let error = finalize(&inputs).err().ok_or("a v3 receipt was accepted")?;
        assert_eq!(error.stage, Stage::Closure);
        assert_eq!(
            error.detail,
            "the source compiler wrote compiler closure schema \"apsides.dev/compiler-closure/v3\"; \
             this aps expects apsides.dev/compiler-closure/v4. The Deno compiler and aps are out \
             of step."
        );
        receipt(
            directory.path(),
            COMPILER_CLOSURE_SCHEMA,
            &serde_json::to_value(&native)?,
        )?;
        let error = finalize(&inputs)
            .err()
            .ok_or("a completed receipt was accepted")?;
        assert_eq!(
            error.detail,
            "source receipt already has a native compiler closure"
        );
        receipt(directory.path(), COMPILER_CLOSURE_SCHEMA, &Value::Null)?;
        let mut invalid = native.clone();
        invalid.cargo_lock_sha256 = sha('e');
        let error = finalize(&FinalizeInputs {
            native: &invalid,
            ..inputs
        })
        .err()
        .ok_or("an invalid closure was written")?;
        assert_eq!(
            error.detail,
            "release validation rejected the compiler closure: release rejected: native compiler \
             lock identity is not the staged controller/Cargo.lock"
        );
        assert!(!output.exists());
        Ok(())
    }

    #[test]
    fn the_checkout_matches_the_embedded_sources() -> TestResult {
        let files = packaging_files(&checkout())?;
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(paths, sorted);
        for required in [
            "Cargo.lock",
            "Cargo.toml",
            "crates/cli/Cargo.toml",
            "crates/compiler-core/Cargo.toml",
            "crates/container/Cargo.toml",
            "crates/container/src/lib.rs",
            "crates/packaging/Cargo.toml",
            "crates/packaging/src/lib.rs",
            "crates/packaging/src/sources.rs",
            "crates/release/Cargo.toml",
        ] {
            assert!(paths.contains(&required), "{required} is missing");
        }
        assert!(paths.iter().all(|path| {
            ["Cargo.lock", "Cargo.toml"].contains(path)
                || sources::CRATES.iter().any(|name| {
                    path.strip_prefix(&format!("crates/{name}/"))
                        .is_some_and(|rest| rest == "Cargo.toml" || rest.starts_with("src/"))
                })
        }));
        Ok(())
    }

    fn tree() -> std::io::Result<tempfile::TempDir> {
        let root = tempfile::tempdir()?;
        for name in sources::ROOT_FILES {
            fs::write(root.path().join(name), name)?;
        }
        for name in sources::CRATES {
            let crate_root = root.path().join("crates").join(name);
            fs::create_dir_all(crate_root.join("src/nested"))?;
            fs::write(crate_root.join("Cargo.toml"), name)?;
            fs::write(crate_root.join("src/lib.rs"), "")?;
            fs::write(crate_root.join("src/nested/template.txt"), "x")?;
        }
        fs::create_dir_all(root.path().join("crates/cli/tests"))?;
        fs::write(root.path().join("crates/cli/tests/ignored.rs"), "")?;
        Ok(root)
    }

    #[test]
    fn every_file_under_src_is_collected_in_order() -> TestResult {
        let root = tree()?;
        let files = sources::collect(root.path())?;
        assert_eq!(files.len(), 2 + 3 * sources::CRATES.len());
        assert_eq!(files[0].0, "Cargo.lock");
        assert!(
            files
                .iter()
                .any(|(path, _)| path == "crates/packaging/src/nested/template.txt")
        );
        assert!(!files.iter().any(|(path, _)| path.contains("tests/")));
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
        Ok(())
    }

    #[test]
    fn symlinked_sources_are_rejected() -> TestResult {
        let root = tree()?;
        symlink("lib.rs", root.path().join("crates/release/src/linked.rs"))?;
        let error = sources::collect(root.path()).err().ok_or("accepted")?;
        assert_eq!(
            error.to_string(),
            "packaging source crates/release/src/linked.rs is a symlink"
        );
        let root = tree()?;
        fs::remove_dir_all(root.path().join("crates/container/src"))?;
        symlink("../cli/src", root.path().join("crates/container/src"))?;
        assert!(sources::collect(root.path()).is_err());
        Ok(())
    }

    #[test]
    fn different_sources_say_to_rebuild() -> TestResult {
        let root = tree()?;
        let current = sources::collect(root.path())?;
        let built: Vec<(&str, &str)> = current
            .iter()
            .map(|(path, sha)| (path.as_str(), sha.as_str()))
            .collect();
        compare(&current, &built, root.path())?;
        let mut changed = built.clone();
        changed[3].1 = "sha256:0";
        let error = compare(&current, &changed, root.path())
            .err()
            .ok_or("accepted")?;
        assert_eq!(
            error.detail,
            format!(
                "this aps was built from different packaging sources than {} (first difference: \
                 {}); rebuild aps",
                root.path().display(),
                current[3].0
            )
        );
        assert!(compare(&current, &built[..built.len() - 1], root.path()).is_err());
        assert!(packaging_files(root.path()).is_err());
        Ok(())
    }
}
