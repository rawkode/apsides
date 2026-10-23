//! The native half of spec §28's reproducibility row.
//!
//! `acceptance-reproduce` runs `aps build` of one program twice, each into a
//! fresh directory with Cargo's cache disabled, from one build profile written
//! from the repository's pins, and requires the two outputs to be identical
//! file for file: the controller binary, the OCI archive and so the image
//! digest, the SBOM, the IR, the plan and the build inputs. It writes a report
//! of the digests to the output directory. The compile half, that two
//! `aps compile` runs agree, is a CLI test that runs on every change.
//!
//! With two engines the first build compiles with the first engine and the
//! second with the second, which checks that podman and Docker produce the
//! same controller. The build inputs then necessarily differ in the engine
//! they record as a host tool, and so in the compiler closure digest; that
//! one entry and the digest are the only differences allowed.

use crate::{
    Error, Result, ensure, infrastructure,
    prepare::{
        BUILD_TIMEOUT, absolute, envs_for_build_via, host_platform, remove_tree, write_profile,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};
use test_cluster::{
    engine::{Engine, OVERRIDE},
    process::Invocation,
};

/// The two builds, by the directory each is written to.
const BUILDS: [&str; 2] = ["first", "second"];
/// The build output that records the host tools, compared apart from the
/// others when the builds use different engines.
const BUILD_INPUTS: &str = "build-inputs.json";

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
    /// The engine each build compiles with, in build order; `None` leaves
    /// the choice to `aps build` for both.
    pub engines: Option<[Engine; 2]>,
}

/// The two engines `--engines` names, as `<first>,<second>`.
///
/// # Errors
/// Fails unless the value names two different engines, each `podman` or
/// `docker`.
pub fn parse_engines(value: &str) -> Result<[Engine; 2]> {
    let engines = value
        .split(',')
        .map(|name| Engine::select(Some(OsStr::new(name)), false))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| Error::Input(format!("--engines: {error}")))?;
    match engines.as_slice() {
        [first, second] if first != second => Ok([*first, *second]),
        _ => Err(Error::Input(format!(
            "--engines must name two different engines, such as docker,podman, not {value:?}"
        ))),
    }
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
    for (index, name) in BUILDS.into_iter().enumerate() {
        let directory = output.join(name);
        for leftover in [
            directory.clone(),
            output.join(format!("{name}.source")),
            output.join(format!("{name}.native-context")),
        ] {
            remove_tree(&leftover)?;
        }
        let invocation = Invocation::new(&aps)
            .arg("build")
            .arg("--source")
            .arg(&source)
            .arg("--output")
            .arg(&directory)
            .arg("--profile")
            .arg(&profile)
            .args(["--cache", "disabled"]);
        build_environment(
            invocation,
            options.engines.map(|engines| engines[index]),
            |name| std::env::var_os(name),
        )
        .timeout(BUILD_TIMEOUT)
        .run()
        .await?;
    }
    let comparison = compare(
        &output.join(BUILDS[0]),
        &output.join(BUILDS[1]),
        options.engines,
    )?;
    let result: Value =
        serde_json::from_slice(&std::fs::read(output.join("first/build-result.json"))?)?;
    let report = json!({
        "schemaVersion": "apsides.dev/reproducible-build/v1",
        "source": options.source,
        "platform": platform,
        "profileDigest": crate::digest(&std::fs::read(&profile)?),
        "cache": "disabled",
        "engines": comparison.engines.map(Engine::program),
        "binaryDigest": result["binaryDigest"],
        "imageDigest": result["imageDigest"],
        "irDigest": result["irDigest"],
        "sbomDigest": result["sbomDigest"],
        "files": comparison.files,
        "differing": comparison.differing,
    });
    std::fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    ensure(
        comparison.differing.is_empty(),
        format!(
            "two cache-disabled builds differ in {:?}",
            comparison.differing
        ),
    )?;
    Ok(report)
}

/// The variables one build runs with: the forwarded build environment and,
/// when the build names its engine, [`OVERRIDE`] set to that engine after
/// it, so a value inherited from the caller can never choose the engine
/// instead.
fn build_environment(
    invocation: Invocation,
    engine: Option<Engine>,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Invocation {
    match engine {
        None => envs_for_build_via(invocation, lookup),
        Some(engine) => envs_for_build_via(invocation, |name| {
            if name == OVERRIDE { None } else { lookup(name) }
        })
        .env(OVERRIDE, engine.program()),
    }
}

/// What comparing the two builds found.
struct Comparison {
    /// The first build's files and their digests.
    files: BTreeMap<String, String>,
    /// The files whose digests, or normalised build inputs, differ.
    differing: BTreeSet<String>,
    /// The engine each build recorded as its `engine` host tool.
    engines: [Engine; 2],
}

/// Compare the builds in `first` and `second` file for file.
///
/// With `requested` engines, `build-inputs.json` is compared with its
/// `engine` host tool and compiler closure digest set aside, and the builds
/// must have recorded the requested engines, two different engine
/// identities and otherwise equal host tools.
fn compare(first: &Path, second: &Path, requested: Option<[Engine; 2]>) -> Result<Comparison> {
    let first_files = digests(first)?;
    let second_files = digests(second)?;
    let mut differing = first_files
        .keys()
        .chain(second_files.keys())
        .filter(|path| requested.is_none() || path.as_str() != BUILD_INPUTS)
        .filter(|path| first_files.get(*path) != second_files.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>();
    let first_inputs = HostTools::read(&first.join(BUILD_INPUTS))?;
    let second_inputs = HostTools::read(&second.join(BUILD_INPUTS))?;
    let engines = [first_inputs.engine()?, second_inputs.engine()?];
    if let Some(requested) = requested {
        ensure(
            engines == requested,
            format!(
                "the builds were asked to use {} then {}, but recorded {} then {}",
                requested[0].program(),
                requested[1].program(),
                engines[0].program(),
                engines[1].program(),
            ),
        )?;
        ensure(
            first_inputs.engine_tool() != second_inputs.engine_tool(),
            "both builds recorded one engine identity",
        )?;
        ensure(
            first_inputs.others() == second_inputs.others(),
            format!(
                "the builds recorded different host tools besides the engine: {} and {}",
                Value::Array(first_inputs.others()),
                Value::Array(second_inputs.others()),
            ),
        )?;
        if first_inputs.normalised() != second_inputs.normalised() {
            differing.insert(BUILD_INPUTS.to_owned());
        }
    }
    Ok(Comparison {
        files: first_files,
        differing,
        engines,
    })
}

/// One build's `build-inputs.json`, with the position of its `engine` host
/// tool in `compilerClosure.nativeCompiler.hostTools`.
struct HostTools {
    document: Value,
    engine: usize,
}

impl HostTools {
    fn read(path: &Path) -> Result<Self> {
        let document: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let tools = Self::tools_of(&document, path)?;
        let engines = tools
            .iter()
            .enumerate()
            .filter(|(_, tool)| tool["role"] == "engine")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let [engine] = engines.as_slice() else {
            return infrastructure(format!(
                "{} records {} engine host tools, not one",
                path.display(),
                engines.len()
            ));
        };
        Ok(Self {
            engine: *engine,
            document,
        })
    }

    fn tools_of<'a>(document: &'a Value, path: &Path) -> Result<&'a Vec<Value>> {
        document["compilerClosure"]["nativeCompiler"]["hostTools"]
            .as_array()
            .ok_or_else(|| {
                Error::Infrastructure(format!(
                    "{} has no compilerClosure.nativeCompiler.hostTools",
                    path.display()
                ))
            })
    }

    fn tools(&self) -> &[Value] {
        self.document["compilerClosure"]["nativeCompiler"]["hostTools"]
            .as_array()
            .map_or(&[], Vec::as_slice)
    }

    fn engine_tool(&self) -> &Value {
        &self.tools()[self.engine]
    }

    /// The engine the `engine` host tool's version names: `podman version
    /// …` or `Docker version …`.
    fn engine(&self) -> Result<Engine> {
        let version = self.engine_tool()["version"].as_str().unwrap_or_default();
        match version
            .split_whitespace()
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("podman") => Ok(Engine::Podman),
            Some("docker") => Ok(Engine::Docker),
            _ => infrastructure(format!(
                "the engine host tool's version {version:?} names neither podman nor docker"
            )),
        }
    }

    fn others(&self) -> Vec<Value> {
        self.tools()
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self.engine)
            .map(|(_, tool)| tool.clone())
            .collect()
    }

    /// The document with the engine host tool replaced by a placeholder and
    /// the compiler closure digest, which covers it, removed.
    fn normalised(&self) -> Value {
        let mut document = self.document.clone();
        if let Some(object) = document.as_object_mut() {
            object.remove("compilerClosureDigest");
        }
        if let Some(tool) =
            document["compilerClosure"]["nativeCompiler"]["hostTools"].get_mut(self.engine)
        {
            *tool = json!({"role": "engine"});
        }
        document
    }
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
    use std::time::Duration;

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

    #[test]
    fn engines_are_two_different_named_engines() -> Result {
        assert_eq!(
            parse_engines("docker,podman")?,
            [Engine::Docker, Engine::Podman]
        );
        assert_eq!(
            parse_engines("podman,docker")?,
            [Engine::Podman, Engine::Docker]
        );
        for invalid in [
            "docker",
            "docker,docker",
            "docker,podman,docker",
            "docker,nerdctl",
            "",
        ] {
            assert!(
                matches!(parse_engines(invalid), Err(Error::Input(_))),
                "{invalid:?} was accepted"
            );
        }
        Ok(())
    }

    /// The engine a build names is set after the forwarded environment, so
    /// the caller's own `APSIDES_CONTAINER_ENGINE` (CI sets `docker`
    /// workflow-wide) cannot decide it, while every other forwarded
    /// variable still reaches the build.
    #[tokio::test]
    async fn a_named_engine_overrides_the_forwarded_one() -> Result {
        let lookup = |name: &str| match name {
            "APSIDES_CONTAINER_ENGINE" => Some(OsString::from("docker")),
            "APSIDES_BUILD_CACHE" => Some(OsString::from("/example/build-cache")),
            _ => None,
        };
        let script = || {
            Invocation::new("sh")
                .args([
                    "-c",
                    "printf '%s %s' \"$APSIDES_CONTAINER_ENGINE\" \"$APSIDES_BUILD_CACHE\"",
                ])
                .timeout(Duration::from_secs(10))
        };
        let named = build_environment(script(), Some(Engine::Podman), lookup)
            .output()
            .await?;
        assert!(named.success, "stderr: {}", named.stderr);
        assert_eq!(named.stdout, "podman /example/build-cache");
        let chosen = build_environment(script(), None, lookup).output().await?;
        assert!(chosen.success, "stderr: {}", chosen.stderr);
        assert_eq!(chosen.stdout, "docker /example/build-cache");
        Ok(())
    }

    fn build(directory: &Path, engine: (&str, &str), cargo: &str, binary: &[u8]) -> Result {
        std::fs::create_dir_all(directory.join("linux-binary"))?;
        std::fs::write(directory.join("linux-binary/apsides-controller"), binary)?;
        let inputs = json!({
            "compilerClosure": {"nativeCompiler": {"hostTools": [
                {"role": "engine", "sha256": engine.0, "version": engine.1},
                {"role": "cargo", "sha256": "sha256:cargo", "version": cargo},
            ]}},
            "compilerClosureDigest": crate::digest(engine.0.as_bytes()),
            "entry": "program.tsx",
        });
        std::fs::write(directory.join(BUILD_INPUTS), serde_json::to_vec(&inputs)?)?;
        Ok(())
    }

    const DOCKER: (&str, &str) = (
        "sha256:docker",
        "Docker version 28.0.4, build b8034c0; server 28.0.4",
    );
    const PODMAN: (&str, &str) = ("sha256:podman", "podman version 5.8.9; server 5.8.9");

    #[test]
    fn engine_parity_sets_aside_only_the_engine_and_the_closure_digest() -> Result {
        let directory = tempfile::tempdir()?;
        let (first, second) = (
            directory.path().join("first"),
            directory.path().join("second"),
        );
        build(&first, DOCKER, "cargo 1.98.0", b"controller")?;
        build(&second, PODMAN, "cargo 1.98.0", b"controller")?;
        let comparison = compare(&first, &second, Some([Engine::Docker, Engine::Podman]))?;
        assert!(
            comparison.differing.is_empty(),
            "{:?}",
            comparison.differing
        );
        assert_eq!(comparison.engines, [Engine::Docker, Engine::Podman]);

        // Without engines, the same build inputs are a difference.
        let comparison = compare(&first, &second, None)?;
        assert_eq!(
            comparison.differing.into_iter().collect::<Vec<_>>(),
            [BUILD_INPUTS]
        );

        // Anything else in the build inputs still is.
        let mut inputs: Value = serde_json::from_slice(&std::fs::read(second.join(BUILD_INPUTS))?)?;
        inputs["entry"] = json!("other.tsx");
        std::fs::write(second.join(BUILD_INPUTS), serde_json::to_vec(&inputs)?)?;
        let comparison = compare(&first, &second, Some([Engine::Docker, Engine::Podman]))?;
        assert_eq!(
            comparison.differing.into_iter().collect::<Vec<_>>(),
            [BUILD_INPUTS]
        );
        Ok(())
    }

    #[test]
    fn engine_parity_reports_differing_outputs() -> Result {
        let directory = tempfile::tempdir()?;
        let (first, second) = (
            directory.path().join("first"),
            directory.path().join("second"),
        );
        build(&first, DOCKER, "cargo 1.98.0", b"controller")?;
        build(&second, PODMAN, "cargo 1.98.0", b"other controller")?;
        let comparison = compare(&first, &second, Some([Engine::Docker, Engine::Podman]))?;
        assert_eq!(
            comparison.differing.into_iter().collect::<Vec<_>>(),
            ["linux-binary/apsides-controller"]
        );
        Ok(())
    }

    #[test]
    fn engine_parity_requires_the_requested_distinct_engines_and_equal_other_tools() -> Result {
        let directory = tempfile::tempdir()?;
        let (first, second) = (
            directory.path().join("first"),
            directory.path().join("second"),
        );
        let parity = Some([Engine::Docker, Engine::Podman]);

        // Both builds fell back to one engine.
        build(&first, PODMAN, "cargo 1.98.0", b"controller")?;
        build(&second, PODMAN, "cargo 1.98.0", b"controller")?;
        let error = compare(&first, &second, parity)
            .err()
            .map(|error| error.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("recorded podman then podman")),
            "{error:?}"
        );

        // Another host tool differs.
        build(&first, DOCKER, "cargo 1.98.0", b"controller")?;
        build(&second, PODMAN, "cargo 1.99.0", b"controller")?;
        let error = compare(&first, &second, parity)
            .err()
            .map(|error| error.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("different host tools besides the engine")),
            "{error:?}"
        );
        Ok(())
    }
}
