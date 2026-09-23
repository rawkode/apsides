//! Two compilations of one program produce the same bytes.
//!
//! `aps compile` of `examples/web-application` into two fresh directories
//! must give identical trees: the IR, the compiled plan, the build inputs
//! the native build binds, the generated controller source and the manifest
//! of file digests. This is the compile half of spec §28's reproducibility
//! row; `cuenv task reproducibleBuild` checks the native half.
//!
//! The compiler runs under Deno with the repository's installed modules, as
//! every `aps compile` does (`cuenv task install`).
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Files a compilation must write, each of which must match byte for byte.
const BOUND: [&str; 5] = [
    "ir.json",
    "plan.json",
    "manifest.json",
    "source/build-inputs.json",
    "controller/Cargo.lock",
];

fn compile(source: &Path, output: &Path) -> Result {
    let result = Command::new(env!("CARGO_BIN_EXE_aps"))
        .arg("compile")
        .arg("--source")
        .arg(source)
        .arg("--output")
        .arg(output)
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "aps compile failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(())
}

/// Every regular file under `root`, by its path relative to `root`.
fn tree(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                files.insert(
                    entry.path().strip_prefix(root)?.to_path_buf(),
                    fs::read(entry.path())?,
                );
            } else {
                return Err(format!("{} is not a regular file", entry.path().display()).into());
            }
        }
    }
    Ok(files)
}

#[test]
fn compiling_a_program_twice_gives_identical_artifacts() -> Result {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/web-application/program.tsx")
        .canonicalize()?;
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("first");
    let second = directory.path().join("second-compilation");
    compile(&source, &first)?;
    compile(&source, &second)?;
    let (first, second) = (tree(&first)?, tree(&second)?);
    for bound in BOUND {
        assert!(
            first.contains_key(Path::new(bound)),
            "aps compile wrote no {bound}"
        );
    }
    let differing = first
        .keys()
        .chain(second.keys())
        .filter(|path| first.get(*path) != second.get(*path))
        .collect::<Vec<_>>();
    assert!(
        differing.is_empty(),
        "two compilations differ in {differing:?}"
    );
    Ok(())
}
