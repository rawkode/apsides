//! The prepared inputs every scenario reads.
//!
//! `acceptance-prepare` writes one `inputs.json`; the scenarios find it
//! through [`ENVIRONMENT`]. A missing or malformed file fails the scenario:
//! an acceptance suite that silently skips has not accepted anything.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The variable naming the inputs file.
pub const ENVIRONMENT: &str = "APSIDES_ACCEPTANCE_INPUTS";

/// The inputs file's schema.
pub const SCHEMA: &str = "apsides.dev/acceptance-inputs/v1";

const MAX_BYTES: u64 = 64 * 1024;

/// Everything `acceptance-prepare` produced, by absolute path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inputs {
    pub schema_version: String,
    /// The `aps` executable the releases were built with and the scenarios
    /// deploy with.
    pub aps: PathBuf,
    /// Signed release of `examples/cnpg/initial` (the database only).
    pub initial_release: PathBuf,
    /// Signed release of `examples/cnpg` (the database plus its API).
    pub successor_release: PathBuf,
    /// The trust policy the releases verify against: the ephemeral signer's
    /// public key, the approved compiler digests and the pinned verifier.
    pub trust_policy: PathBuf,
    /// The `CloudNativePG` operator manifest, byte-for-byte as released.
    pub operator_manifest: PathBuf,
    /// Where scenarios write their reports and logs.
    pub results: PathBuf,
}

impl Inputs {
    /// Read the inputs file [`ENVIRONMENT`] names.
    ///
    /// # Errors
    /// Fails when the variable is unset, or the file is missing, oversized,
    /// malformed, of another schema, or names a path that is not absolute or
    /// does not exist.
    pub fn load() -> Result<Self> {
        let path = std::env::var_os(ENVIRONMENT).ok_or_else(|| {
            Error::Input(format!(
                "{ENVIRONMENT} is unset; run `cuenv task acceptancePrepare` and point it at the inputs.json it writes"
            ))
        })?;
        Self::read(Path::new(&path))
    }

    /// Read and check an inputs file.
    ///
    /// # Errors
    /// As [`Inputs::load`], apart from the variable.
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| Error::Input(format!("{}: {error}", path.display())))?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(Error::Input(format!(
                "{} is not a bounded regular file",
                path.display()
            )));
        }
        let inputs: Self = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|error| Error::Input(format!("{}: {error}", path.display())))?;
        inputs.validate()?;
        Ok(inputs)
    }

    fn validate(&self) -> Result {
        if self.schema_version != SCHEMA {
            return Err(Error::Input(format!(
                "inputs schema is {}, expected {SCHEMA}",
                self.schema_version
            )));
        }
        for path in [
            &self.aps,
            &self.initial_release,
            &self.successor_release,
            &self.trust_policy,
            &self.operator_manifest,
        ] {
            if !path.is_absolute() || !path.exists() {
                return Err(Error::Input(format!(
                    "{} must be an absolute path that exists; rerun acceptancePrepare",
                    path.display()
                )));
            }
        }
        if !self.results.is_absolute() {
            return Err(Error::Input(
                "the results directory must be absolute".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(directory: &Path, inputs: &Inputs) -> Result<PathBuf> {
        let path = directory.join("inputs.json");
        std::fs::write(&path, serde_json::to_vec(inputs)?)?;
        Ok(path)
    }

    fn sample(directory: &Path) -> Inputs {
        let file = directory.join("present");
        Inputs {
            schema_version: SCHEMA.into(),
            aps: file.clone(),
            initial_release: file.clone(),
            successor_release: file.clone(),
            trust_policy: file.clone(),
            operator_manifest: file,
            results: directory.join("results"),
        }
    }

    #[test]
    fn inputs_round_trip_and_name_existing_absolute_paths() -> Result {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("present"), b"x")?;
        let inputs = sample(directory.path());
        assert_eq!(Inputs::read(&written(directory.path(), &inputs)?)?, inputs);
        Ok(())
    }

    #[test]
    fn missing_paths_other_schemas_and_unknown_fields_are_rejected() -> Result {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("present"), b"x")?;
        let mut missing = sample(directory.path());
        missing.successor_release = directory.path().join("absent");
        assert!(Inputs::read(&written(directory.path(), &missing)?).is_err());
        let mut relative = sample(directory.path());
        relative.aps = PathBuf::from("target/debug/aps");
        assert!(Inputs::read(&written(directory.path(), &relative)?).is_err());
        let mut schema = sample(directory.path());
        schema.schema_version = "apsides.dev/acceptance-inputs/v0".into();
        assert!(Inputs::read(&written(directory.path(), &schema)?).is_err());
        let path = directory.path().join("extra.json");
        let mut value = serde_json::to_value(sample(directory.path()))?;
        value["unexpected"] = serde_json::json!(true);
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        assert!(Inputs::read(&path).is_err());
        Ok(())
    }
}
