#![forbid(unsafe_code)]
//! The native build: packaging an authored program's verified artifact into
//! the release inputs `aps release` consumes.
//!
//! Everything but the cargo compile runs in this process. The compile runs as
//! one network-less `podman run` or `docker run` of the pinned builder image;
//! the image layout, SBOM and compiler closure around it are written here,
//! byte for byte, so they follow from the inputs alone.

pub mod builder;
pub mod cargo;
pub mod closure;
pub mod command;
pub mod compile;
pub mod context;
pub mod digest;
pub mod error;
#[cfg(test)]
mod fixtures;
pub mod fs;
pub mod image;
pub mod outputs;
pub mod platform;
pub mod profile;
pub mod redact;
pub mod reference;
pub mod result;
pub mod runtime;
pub mod rusty_v8;
pub mod saved;
pub mod sbom;
pub mod sources;
pub mod tools;
pub mod ustar;

pub use cargo::HostCargo;
pub use context::controller_manifest;
pub use error::{Error, Stage};
pub use platform::Platform;

use container::Engine;
use release::{BuilderImage, CompilerFile, ControllerImage, NativeCompiler};
use rusty_v8::{Fetch, HttpsFetch};
use saved::SavedImage;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

/// Whether the compile reuses a persistent cargo target directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cache {
    /// Reuse the target directory cached for this recipe, builder, platform,
    /// epoch, lock and trusted sources.
    Enabled,
    /// A fresh target directory, removed afterwards: the reproducibility
    /// mode.
    Disabled,
}

/// The artifact checks the native build borrows from `aps` itself.
pub trait Validator {
    /// Verifies the compiled artifact at `artifact` and returns its
    /// `irDigest`.
    ///
    /// # Errors
    /// Describes why the artifact is not valid.
    fn verify_artifact(&self, artifact: &Path) -> std::result::Result<String, String>;

    /// Validates the IR document at `ir_json` and returns its digest.
    ///
    /// # Errors
    /// Describes why the IR is not valid.
    fn ir_digest(&self, ir_json: &Path) -> std::result::Result<String, String>;
}

/// The identities one native build produced, as `build-result.json` records
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildResult {
    pub ir_digest: String,
    pub platform: Platform,
    pub image_digest: String,
    pub binary_digest: String,
    pub sbom_digest: String,
}

/// One native build: the verified artifact `aps compile` wrote, the source
/// compiler's staging directory and the build profile.
pub struct Request<'a> {
    /// The assets root: the checkout, or a distribution's share directory.
    /// Nothing is written under it.
    pub assets: &'a Path,
    /// The artifact directory, which also receives the build outputs.
    pub artifact: &'a Path,
    /// The source compiler's staging directory, holding `build-inputs.json`.
    pub stage: &'a Path,
    pub profile: &'a Path,
    pub cache: Cache,
    pub validator: &'a dyn Validator,
}

/// Replacements for the tools a build would otherwise discover, so the
/// whole pipeline can run against fakes. [`build`] uses none.
#[doc(hidden)]
#[derive(Default)]
pub struct Overrides<'a> {
    /// The engine and its CLI, instead of [`Engine::current`] and `PATH`.
    pub engine: Option<(Engine, &'a Path)>,
    /// Host cargo, run with no toolchain override, instead of `PATH`.
    pub cargo: Option<&'a Path>,
    /// The `rusty_v8` archive cache, instead of the environment's.
    pub rusty_v8_cache: Option<&'a Path>,
    /// How the `rusty_v8` archive is downloaded, instead of HTTPS.
    pub fetcher: Option<&'a dyn Fetch>,
}

/// Builds the controller image, binary, SBOM and completed compiler closure
/// for `request.artifact`, and writes `build-result.json`.
///
/// Only the cargo compile leaves this process, as one network-less run of
/// the pinned builder image in the local podman or Docker. Nothing is
/// written to stdout; the compile's output streams to stderr.
///
/// # Errors
/// Fails, naming the stage, when any input is missing or invalid, any tool
/// or check fails, or any input changes during the build. A failed build
/// removes the outputs it wrote and keeps the build context.
pub fn build(request: &Request<'_>) -> Result<BuildResult> {
    build_with(request, &Overrides::default())
}

/// [`build`] with tool discovery replaced by `overrides`.
///
/// # Errors
/// As [`build`].
#[doc(hidden)]
pub fn build_with(request: &Request<'_>, overrides: &Overrides<'_>) -> Result<BuildResult> {
    let (profile, profile_bytes) = profile::read(request.profile)?;
    let host = Host::identify(request.assets, request.profile, &profile, overrides)?;
    // A mismatch between this aps and the sources it would record fails
    // before anything is built.
    closure::packaging_files(request.assets)?;

    let reservation = outputs::reserve(request.artifact)?;
    let artifact = request.artifact.canonicalize().map_err(error::io(
        Stage::Outputs,
        "resolve",
        request.artifact,
    ))?;
    let scratch = artifact.parent().map(Path::to_path_buf).ok_or_else(|| {
        Error::new(
            Stage::Outputs,
            format!("{} has no parent directory", artifact.display()),
        )
    })?;
    let builder = host.builder(&profile)?;
    let rusty_v8_cache = match overrides.rusty_v8_cache {
        Some(cache) => cache.to_path_buf(),
        None => rusty_v8::cache_dir(|name| std::env::var_os(name))?,
    };
    let context = context::prepare(&context::ContextInputs {
        assets: request.assets,
        artifact: &artifact,
        platform: profile.platform,
        cargo: &host.cargo,
        validator: request.validator,
        rusty_v8_cache: &rusty_v8_cache,
        fetcher: overrides.fetcher.unwrap_or(&HttpsFetch),
    })?;
    let (save_dir, saved_path, saved, approved) =
        host.runtime_base(request.assets, &profile, &scratch)?;
    let binary = compile::compile(&compile::CompileInputs {
        engine: host.engine,
        engine_program: &host.engine_program,
        builder: &builder,
        context: &context,
        cache: request.cache,
        assets: request.assets,
        epoch: profile.source_date_epoch,
        platform: profile.platform,
        scratch: &scratch,
    })?;
    let (image_digest, sbom_digest) = assemble(&Assembly {
        artifact: &artifact,
        profile: &profile,
        host: &host,
        context: &context,
        saved_path: &saved_path,
        saved: &saved,
        approved: &approved,
        binary: &binary,
    })?;
    closure::finalize(&closure::FinalizeInputs {
        stage_inputs: &request.stage.join("build-inputs.json"),
        output: &artifact.join("build-inputs.json"),
        native: &NativeCompiler {
            builder_image: BuilderImage {
                reference: builder.reference.clone(),
                platform: builder.platform.as_str().to_owned(),
            },
            cargo_lock_sha256: context.controller_lock_sha256.clone(),
            build_context_files: context
                .files
                .iter()
                .map(|(path, sha256)| CompilerFile {
                    path: path.clone(),
                    sha256: sha256.clone(),
                })
                .collect(),
            packaging_files: closure::packaging_files(request.assets)?,
            build_profile_sha256: digest::digest(&profile_bytes),
            host_tools: host.tools.public(),
            approved_runtime: approved,
            rusty_v8: context.rusty_v8.clone(),
        },
    })?;

    host.verify_unchanged(&profile)?;
    if std::fs::read(request.profile).ok().as_deref() != Some(profile_bytes.as_slice()) {
        return Err(Error::new(
            Stage::Outputs,
            format!(
                "the build profile {} changed during the build",
                request.profile.display()
            ),
        ));
    }
    context.verify_unchanged(Stage::Outputs, "the build")?;
    let result = BuildResult {
        ir_digest: context.ir_digest,
        platform: profile.platform,
        image_digest,
        binary_digest: digest::digest(&binary),
        sbom_digest,
    };
    result::write(&artifact.join("build-result.json"), &result)?;
    reservation.commit();
    if let Err(error) = save_dir.close() {
        eprintln!("aps: warning: cannot remove the saved runtime base: {error}");
    }
    Ok(result)
}

/// The host tools one build runs and records.
struct Host {
    engine: Engine,
    /// The engine CLI, as identified among the host tools.
    engine_program: PathBuf,
    cargo: HostCargo,
    tools: tools::Snapshot,
}

impl Host {
    /// Finds and identifies the engine, host cargo, this `aps` and the
    /// profile's SBOM tool, whose bytes must be the ones the profile pins.
    fn identify(
        assets: &Path,
        profile_path: &Path,
        profile: &profile::Profile,
        overrides: &Overrides<'_>,
    ) -> Result<Self> {
        let (engine, engine_program) = if let Some((engine, program)) = overrides.engine {
            (engine, program.to_path_buf())
        } else {
            let engine = Engine::current()
                .map_err(|error| Error::new(Stage::HostTools, error.to_string()))?;
            let program = locate_engine(
                engine,
                container::find_on_path(engine.program()),
                std::env::var_os(container::OVERRIDE).is_some(),
            )?;
            (engine, program)
        };
        let cargo = match overrides.cargo {
            Some(program) => HostCargo::with_program(program.to_path_buf(), None),
            None => HostCargo::locate(assets)?,
        };
        locate_sbom_tool(&profile.sbom_tool, profile_path)?;
        let tools = tools::capture(engine, &engine_program, &cargo, &profile.sbom_tool)?;
        if tools.sbom_sha256() != profile.sbom_tool_sha256 {
            return Err(Error::new(
                Stage::HostTools,
                format!(
                    "build profile sbomToolSha256 {} differs from the bytes of {} ({}); update \
                     the profile or reinstall cargo-cyclonedx 0.5.9",
                    profile.sbom_tool_sha256,
                    profile.sbom_tool.display(),
                    tools.sbom_sha256()
                ),
            ));
        }
        Ok(Self {
            engine,
            engine_program,
            cargo,
            tools,
        })
    }

    /// The builder image the profile names, which must be for the
    /// profile's platform.
    fn builder(&self, profile: &profile::Profile) -> Result<builder::Builder> {
        let builder = builder::inspect(self.engine, &self.engine_program, &profile.builder_image)?;
        if builder.platform != profile.platform {
            return Err(Error::new(
                Stage::Builder,
                format!(
                    "builder image {} is {} but the build profile declares {}",
                    profile.builder_image,
                    builder.platform.as_str(),
                    profile.platform.as_str()
                ),
            ));
        }
        Ok(builder)
    }

    /// Saves the profile's runtime base into a fresh directory in `scratch`
    /// and binds the save to its pins; returns the directory, the save, the
    /// image read from it and the approved runtime.
    fn runtime_base(
        &self,
        assets: &Path,
        profile: &profile::Profile,
        scratch: &Path,
    ) -> Result<(
        tempfile::TempDir,
        PathBuf,
        SavedImage,
        release::ApprovedRuntime,
    )> {
        let directory = tempfile::Builder::new()
            .prefix(".apsides-runtime-base-")
            .tempdir_in(scratch)
            .map_err(error::io(
                Stage::RuntimeBase,
                "create a directory for the runtime base in",
                scratch,
            ))?;
        let path = runtime::save(
            self.engine,
            &self.engine_program,
            &profile.runtime_image,
            profile.platform,
            directory.path(),
        )?;
        let saved = SavedImage::open(&path, profile.platform)?;
        let approved =
            runtime::approved_runtime(assets, &profile.runtime_image, profile.platform, &saved)?;
        Ok((directory, path, saved, approved))
    }

    /// Fails unless every tool is still the one identified.
    fn verify_unchanged(&self, profile: &profile::Profile) -> Result<()> {
        self.tools.verify_unchanged(
            self.engine,
            &self.engine_program,
            &self.cargo,
            &profile.sbom_tool,
        )
    }
}

/// What [`assemble`] writes the outputs from.
struct Assembly<'a> {
    artifact: &'a Path,
    profile: &'a profile::Profile,
    host: &'a Host,
    context: &'a context::Context,
    saved_path: &'a Path,
    saved: &'a SavedImage,
    approved: &'a release::ApprovedRuntime,
    binary: &'a [u8],
}

/// Writes the controller image, checked as `aps release` checks it, the
/// binary and the SBOM; returns the image and SBOM digests.
fn assemble(assembly: &Assembly<'_>) -> Result<(String, String)> {
    let platform = assembly.profile.platform;
    let epoch = assembly.profile.source_date_epoch;
    let binary_digest = digest::digest(assembly.binary);
    let image_path = assembly.artifact.join("image/controller.oci.tar");
    let image_digest = image::package(
        assembly.saved,
        assembly.binary,
        &assembly.context.ir_digest,
        epoch,
        platform,
        &image_path,
    )?;
    check_image(
        &image_path,
        &ControllerImage {
            image_digest: &image_digest,
            platform: platform.as_str(),
            ir_digest: &assembly.context.ir_digest,
            binary_digest: &binary_digest,
            approved_runtime: assembly.approved,
        },
    )?;
    if SavedImage::open(assembly.saved_path, platform)?.config
        != assembly.approved.config_json.as_bytes()
    {
        return Err(Error::new(
            Stage::Image,
            "the saved runtime base changed while the image was assembled",
        ));
    }
    outputs::write_binary(
        &assembly.artifact.join("linux-binary/apsides-controller"),
        assembly.binary,
    )?;
    let sbom_digest = sbom::generate(&sbom::SbomInputs {
        context: assembly.context,
        tool: assembly.host.tools.sbom_path(),
        tool_sha256: &assembly.profile.sbom_tool_sha256,
        target: platform.rust_target(),
        epoch,
        cargo: &assembly.host.cargo,
        output: &assembly.artifact.join("sbom"),
    })?;
    assembly
        .context
        .verify_unchanged(Stage::Sbom, "SBOM generation")?;
    assembly.host.verify_unchanged(assembly.profile)?;
    Ok((image_digest, sbom_digest))
}

/// The engine CLI on `PATH` (`found`), or why there is none.
fn locate_engine(engine: Engine, found: Option<PathBuf>, overridden: bool) -> Result<PathBuf> {
    found.ok_or_else(|| {
        Error::new(
            Stage::HostTools,
            if overridden {
                format!(
                    "{} is not on PATH; install it, or set {} to the engine you have",
                    engine.program(),
                    container::OVERRIDE
                )
            } else {
                format!(
                    "neither podman nor docker is on PATH; install one, or set {} to the one you \
                     have",
                    container::OVERRIDE
                )
            },
        )
    })
}

/// Fails, naming the build profile, when the SBOM tool it names at `sbom`
/// cannot be resolved.
fn locate_sbom_tool(sbom: &Path, profile_path: &Path) -> Result<()> {
    sbom.canonicalize().map(drop).map_err(|error| {
        Error::new(
            Stage::HostTools,
            format!(
                "cannot resolve the SBOM tool {} that sbomTool in build profile {} names: \
                 {error}; install cargo-cyclonedx 0.5.9 at that path, or set sbomTool to where \
                 it is installed",
                sbom.display(),
                profile_path.display()
            ),
        )
    })
}

/// The release verifier's check of the image just written: producer and
/// consumer share one definition of a controller image.
fn check_image(path: &Path, expected: &ControllerImage<'_>) -> Result<()> {
    let archive = std::fs::read(path).map_err(error::io(Stage::Image, "read", path))?;
    release::verify_controller_image(&archive, expected).map_err(|error| {
        Error::new(
            Stage::Image,
            format!("release verification rejected the assembled image: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_sbom_tool_names_the_profile_and_a_remedy() {
        let error = locate_sbom_tool(
            Path::new("/nonexistent/cargo-cyclonedx"),
            Path::new("apsides.build.json"),
        )
        .err()
        .map(|error| error.detail)
        .unwrap_or_default();
        assert!(
            error.starts_with(
                "cannot resolve the SBOM tool /nonexistent/cargo-cyclonedx that sbomTool in build \
                 profile apsides.build.json names: "
            ),
            "{error}"
        );
        assert!(
            error.ends_with(
                "; install cargo-cyclonedx 0.5.9 at that path, or set sbomTool to where it is \
                 installed"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_missing_engine_says_how_to_get_one() {
        let error = locate_engine(Engine::Docker, None, false)
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            error.as_deref(),
            Some(
                "host tool identification: neither podman nor docker is on PATH; install one, or \
                 set APSIDES_CONTAINER_ENGINE to the one you have"
            )
        );
        let error = locate_engine(Engine::Podman, None, true)
            .err()
            .map(|error| error.detail);
        assert_eq!(
            error.as_deref(),
            Some(
                "podman is not on PATH; install it, or set APSIDES_CONTAINER_ENGINE to the engine \
                 you have"
            )
        );
        assert_eq!(
            locate_engine(Engine::Podman, Some(PathBuf::from("/bin/podman")), true).ok(),
            Some(PathBuf::from("/bin/podman"))
        );
    }
}
