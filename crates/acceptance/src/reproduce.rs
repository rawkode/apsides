//! The native half of spec §28's reproducibility row.
//!
//! `acceptance-reproduce` runs `aps build` of one program twice, each into a
//! fresh directory with Cargo's cache disabled, from one build profile written
//! from the repository's pins, and requires the two outputs to be identical
//! file for file: the controller binary, the OCI archive and so the image
//! digest, the SBOM, the IR, the plan and the build inputs. It writes a report
//! of the digests to the output directory. The compile half, that two
//! `aps compile` runs agree, is a CLI test that runs on every change.

use crate::{
    Result, ensure,
    prepare::{BUILD_TIMEOUT, BuildEnvironment as _, absolute, host_platform, write_profile},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use test_cluster::process::Invocation;

/// The two builds, by the directory each is written to.
const BUILDS: [&str; 2] = ["first", "second"];

/// What to build and where.
pub struct Options {
    /// The repository checkout holding `scripts/` and the program.
    pub root: PathBuf,
    /// A directory the run owns; earlier builds in it are replaced.
    pub output: PathBuf,
    /// The `aps` executable to build with.
    pub aps: PathBuf,
    /// `cargo-cyclonedx`, which `aps build` records in the build profile.
    pub sbom_tool: PathBuf,
    /// The program's entry point.
    pub source: PathBuf,
}

/// Build the program twice and compare the outputs; returns the report
/// written to `report.json`.
///
/// # Errors
/// Fails when a tool is missing, either build fails, or the outputs differ.
pub async fn reproduce(options: &Options) -> Result<Value> {
    let root = options.root.canonicalize()?;
    let aps = absolute(&options.aps)?;
    let sbom_tool = absolute(&options.sbom_tool)?;
    let source = absolute(&root.join(&options.source))?;
    std::fs::create_dir_all(&options.output)?;
    let output = options.output.canonicalize()?;
    let platform = host_platform()?;
    let profile = write_profile(&root, &output, &sbom_tool, &platform)?;
    let mut trees = Vec::new();
    for name in BUILDS {
        let directory = output.join(name);
        for leftover in [
            directory.clone(),
            output.join(format!("{name}.source")),
            output.join(format!("{name}.oci-context")),
        ] {
            if leftover.exists() {
                std::fs::remove_dir_all(&leftover)?;
            }
        }
        Invocation::new(&aps)
            .arg("build")
            .arg("--source")
            .arg(&source)
            .arg("--output")
            .arg(&directory)
            .arg("--profile")
            .arg(&profile)
            .args(["--cache", "disabled"])
            .envs_for_build()
            .timeout(BUILD_TIMEOUT)
            .run()
            .await?;
        trees.push(digests(&directory)?);
    }
    let [first, second] = trees.as_slice() else {
        return Err(crate::Error::Infrastructure("expected two builds".into()));
    };
    let differing = first
        .keys()
        .chain(second.keys())
        .filter(|path| first.get(*path) != second.get(*path))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let result: Value =
        serde_json::from_slice(&std::fs::read(output.join("first/build-result.json"))?)?;
    let report = json!({
        "schemaVersion": "apsides.dev/reproducible-build/v1",
        "source": options.source,
        "platform": platform,
        "profileDigest": crate::digest(&std::fs::read(&profile)?),
        "cache": "disabled",
        "binaryDigest": result["binaryDigest"],
        "imageDigest": result["imageDigest"],
        "irDigest": result["irDigest"],
        "sbomDigest": result["sbomDigest"],
        "files": first,
        "differing": differing,
    });
    std::fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    ensure(
        differing.is_empty(),
        format!("two cache-disabled builds differ in {differing:?}"),
    )?;
    Ok(report)
}

/// The digest of every regular file under `root`, by its path relative to
/// `root`.
fn digests(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else {
                ensure(
                    kind.is_file(),
                    format!("{} is not a regular file", path.display()),
                )?;
                let relative = path
                    .strip_prefix(root)
                    .map_err(|error| crate::Error::Infrastructure(error.to_string()))?;
                files.insert(
                    relative.to_string_lossy().into_owned(),
                    crate::digest(&std::fs::read(&path)?),
                );
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_name_every_file_by_its_relative_path() -> Result {
        let directory = tempfile::tempdir()?;
        std::fs::create_dir(directory.path().join("image"))?;
        std::fs::write(directory.path().join("ir.json"), b"{}")?;
        std::fs::write(directory.path().join("image/controller.oci.tar"), b"oci")?;
        let files = digests(directory.path())?;
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["image/controller.oci.tar", "ir.json"]
        );
        assert_eq!(files.get("ir.json"), Some(&crate::digest(b"{}")));
        Ok(())
    }
}
