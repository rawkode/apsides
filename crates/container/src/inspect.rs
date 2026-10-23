//! `image inspect`: what the local store holds under a reference.

use crate::{Engine, Error};
use serde::Deserialize;
use std::ffi::OsString;

/// The fields of one inspected image aps relies on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    /// The image ID as the engine reports it: bare hex from podman,
    /// `sha256:`-prefixed from Docker. Both are the config digest.
    pub id: String,
    pub os: String,
    pub architecture: String,
    pub repo_digests: Vec<String>,
    /// `Config.Env`, empty when the image sets none.
    pub env: Vec<String>,
}

/// `image inspect <reference>`.
#[must_use]
pub fn inspect_args(reference: &str) -> Vec<OsString> {
    ["image", "inspect", reference]
        .into_iter()
        .map(OsString::from)
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Raw {
    id: String,
    os: String,
    architecture: String,
    #[serde(default)]
    repo_digests: Option<Vec<String>>,
    #[serde(default)]
    config: Option<RawConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawConfig {
    #[serde(default)]
    env: Option<Vec<String>>,
}

/// The images in `engine`'s `image inspect` output, a JSON array in the
/// same shape for both engines.
///
/// # Errors
/// Fails when the output is not that array or an image lacks its ID, OS or
/// architecture.
pub fn parse_inspection(engine: Engine, stdout: &str) -> Result<Vec<Inspection>, Error> {
    let raw: Vec<Raw> = serde_json::from_str(stdout).map_err(|error| Error::Unreadable {
        engine: engine.name(),
        what: "image inspect",
        detail: error.to_string(),
    })?;
    Ok(raw
        .into_iter()
        .map(|image| Inspection {
            id: image.id,
            os: image.os,
            architecture: image.architecture,
            repo_digests: image.repo_digests.unwrap_or_default(),
            env: image
                .config
                .and_then(|config| config.env)
                .unwrap_or_default(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PODMAN: &str = r#"[
  {
    "Id": "a3535ab4d4c5b0e9b5a1ef2c0c3c1e0b9e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39",
    "Digest": "sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922",
    "RepoTags": [],
    "RepoDigests": [
      "docker.io/library/rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922",
      "docker.io/library/rust@sha256:1111111111111111111111111111111111111111111111111111111111111111"
    ],
    "Architecture": "amd64",
    "Os": "linux",
    "Variant": null,
    "Config": {
      "Env": [
        "PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "RUSTUP_HOME=/usr/local/rustup",
        "CARGO_HOME=/usr/local/cargo",
        "RUST_VERSION=1.98.0"
      ],
      "Cmd": ["bash"]
    }
  }
]"#;

    const DOCKER: &str = r#"[
  {
    "Id": "sha256:a3535ab4d4c5b0e9b5a1ef2c0c3c1e0b9e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39",
    "RepoTags": [],
    "RepoDigests": ["rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922"],
    "Architecture": "arm64",
    "Variant": "v8",
    "Os": "linux",
    "Config": {"Env": null, "Cmd": ["bash"]}
  }
]"#;

    #[test]
    fn podman_ids_are_bare_hex_and_env_is_kept_in_order() -> Result<(), Error> {
        let images = parse_inspection(Engine::Podman, PODMAN)?;
        assert_eq!(images.len(), 1);
        let image = &images[0];
        assert_eq!(
            image.id,
            "a3535ab4d4c5b0e9b5a1ef2c0c3c1e0b9e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39"
        );
        assert_eq!(
            (image.os.as_str(), image.architecture.as_str()),
            ("linux", "amd64")
        );
        assert_eq!(image.repo_digests.len(), 2);
        assert_eq!(image.env.len(), 4);
        assert_eq!(image.env[2], "CARGO_HOME=/usr/local/cargo");
        Ok(())
    }

    #[test]
    fn docker_ids_keep_their_prefix_and_null_env_is_empty() -> Result<(), Error> {
        let images = parse_inspection(Engine::Docker, DOCKER)?;
        assert_eq!(
            images,
            vec![Inspection {
                id: "sha256:a3535ab4d4c5b0e9b5a1ef2c0c3c1e0b9e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39"
                    .to_owned(),
                os: "linux".to_owned(),
                architecture: "arm64".to_owned(),
                repo_digests: vec![
                    "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922"
                        .to_owned()
                ],
                env: Vec::new(),
            }]
        );
        Ok(())
    }

    #[test]
    fn missing_fields_and_non_arrays_are_unreadable() {
        for stdout in ["", "{}", r#"[{"Id": "x", "Os": "linux"}]"#, "[]x"] {
            assert!(
                matches!(
                    parse_inspection(Engine::Podman, stdout),
                    Err(Error::Unreadable {
                        engine: "Podman",
                        what: "image inspect",
                        ..
                    })
                ),
                "{stdout:?} was accepted"
            );
        }
        assert_eq!(parse_inspection(Engine::Docker, "[]"), Ok(Vec::new()));
    }

    #[test]
    fn inspect_names_the_reference() {
        assert_eq!(
            inspect_args("rust@sha256:00"),
            ["image", "inspect", "rust@sha256:00"]
        );
    }
}
