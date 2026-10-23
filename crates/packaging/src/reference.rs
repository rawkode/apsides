//! Pinned image references: `repository@sha256:<digest>` and nothing looser.

use crate::digest::is_digest;

/// The digest of a `[a-z0-9][a-z0-9./:_-]*@sha256:<64 hex>` reference, or
/// `None` for a tag, a short digest or any other form.
#[must_use]
pub fn pinned(reference: &str) -> Option<&str> {
    let (repository, digest) = reference.split_once('@')?;
    let mut bytes = repository.bytes();
    let first = bytes.next()?;
    let valid = (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"./:_-".contains(&byte)
        })
        && is_digest(digest);
    valid.then_some(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922";

    #[test]
    fn only_digest_references_are_pinned() {
        assert_eq!(pinned(&format!("rust@{DIGEST}")), Some(DIGEST));
        assert_eq!(
            pinned(&format!("docker.io/library/rust@{DIGEST}")),
            Some(DIGEST)
        );
        for reference in [
            "rust:latest".to_owned(),
            "rust".to_owned(),
            format!("Rust@{DIGEST}"),
            format!("-rust@{DIGEST}"),
            format!("rust:1.98@{DIGEST}@{DIGEST}"),
            format!("@{DIGEST}"),
            "rust@sha256:82150a52".to_owned(),
            format!("rust@{}", DIGEST.to_uppercase()),
        ] {
            assert_eq!(pinned(&reference), None, "{reference} was accepted");
        }
    }
}
