use crate::{Result, cli, report};
use std::{
    fs,
    io::Read as _,
    path::{Component, Path},
};

/// The largest fragment list the CLI will read.
const MAX_BYTES: usize = 1024 * 1024;

/// # Errors
/// Returns an error before the candidate is opened if the fragment list is
/// missing, oversized, empty, over-long, or names a path that is absolute,
/// traverses upwards, or crosses a symlink.
pub fn command(arguments: &cli::MergeEvidence) -> Result<()> {
    let fragments = arguments.fragments.as_path();
    let metadata = fragments.symlink_metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
        return Err("evidence fragment list must be a bounded regular file".into());
    }
    let mut contents = String::new();
    fs::File::open(fragments)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut contents)?;
    if contents.len() > MAX_BYTES {
        return Err("evidence fragment list exceeds its size limit".into());
    }
    let values: Vec<String> = serde_json::from_value(compiler_core::parse_strict(&contents)?)?;
    if values.is_empty() || values.len() > 128 || values.iter().any(String::is_empty) {
        return Err("evidence fragment list must contain 1 to 128 receipt paths".into());
    }
    if values.iter().any(|value| {
        !Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    }) {
        return Err("receipt paths must be relative to the fragment list without traversal".into());
    }
    let parent = match fragments.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
    .canonicalize()?;
    let mut receipts = Vec::new();
    for value in &values {
        let mut receipt = parent.clone();
        for part in Path::new(value).components() {
            receipt.push(part);
            if receipt.symlink_metadata()?.file_type().is_symlink() {
                return Err("receipt paths must not traverse symlinks".into());
            }
        }
        receipts.push(receipt);
    }
    report::emit(&release::merge_evidence(
        &arguments.artifact,
        &receipts,
        &arguments.output,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_fragment_lists_before_reading_the_candidate() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let fragments = directory.path().join("fragments.json");
        let output = directory.path().join("merged");
        let arguments = cli::MergeEvidence {
            artifact: directory.path().join("absent"),
            fragments: fragments.clone(),
            output: output.clone(),
        };
        for contents in [
            "[]",
            "[\"../receipt.json\"]",
            "[\"/receipt.json\"]",
            "[\"\"]",
            "[42]",
        ] {
            fs::write(&fragments, contents)?;
            let error = command(&arguments)
                .err()
                .ok_or("invalid fragment list was accepted")?;
            if error.downcast_ref::<std::io::Error>().is_some() || output.exists() {
                return Err("invalid list reached candidate access or wrote output".into());
            }
        }
        fs::write(&fragments, " ".repeat(1024 * 1024 + 1))?;
        let error = command(&arguments)
            .err()
            .ok_or("oversized fragment list was accepted")?;
        if error.to_string() != "evidence fragment list must be a bounded regular file" {
            return Err("oversized fragment list returned an unexpected error".into());
        }
        Ok(())
    }
}
