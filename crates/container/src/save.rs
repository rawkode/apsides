//! `image save`: a local image as an archive file.

use crate::Engine;
use std::{ffi::OsString, path::Path};

/// `image save` writing `reference` to `output`.
///
/// podman is asked for a docker-archive, which keeps the image's config and
/// layers byte for byte. Docker writes its own archive format and takes no
/// `--format`.
#[must_use]
pub fn save_args(engine: Engine, reference: &str, output: &Path) -> Vec<OsString> {
    let format: &[&str] = match engine {
        Engine::Podman => &["--format", "docker-archive"],
        Engine::Docker => &[],
    };
    ["image", "save"]
        .iter()
        .chain(format)
        .chain(&["--output"])
        .map(OsString::from)
        .chain([output.as_os_str().to_owned(), reference.into()])
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn podman_saves_a_docker_archive_and_docker_its_default() {
        let output = Path::new("/tmp/out/base.tar");
        assert_eq!(
            save_args(Engine::Podman, "base@sha256:00", output),
            [
                "image",
                "save",
                "--format",
                "docker-archive",
                "--output",
                "/tmp/out/base.tar",
                "base@sha256:00"
            ]
        );
        assert_eq!(
            save_args(Engine::Docker, "base@sha256:00", output),
            [
                "image",
                "save",
                "--output",
                "/tmp/out/base.tar",
                "base@sha256:00"
            ]
        );
    }
}
