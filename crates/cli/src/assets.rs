use crate::Result;
use std::path::PathBuf;

#[cfg(not(feature = "distribution"))]
pub fn root() -> Result<PathBuf> {
    Ok(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?)
}

#[cfg(feature = "distribution")]
pub fn root() -> Result<PathBuf> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let prefix = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or("distribution executable has no installation prefix")?;
    let expected = option_env!("APS_ASSET_MANIFEST_SHA256")
        .ok_or("distribution binary was built without its asset manifest")?;
    verified::root(prefix, expected)
}

#[cfg(any(feature = "distribution", test))]
mod verified {
    use crate::Result;
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeMap,
        fs,
        io::{BufReader, Read},
        path::{Path, PathBuf},
    };

    pub(super) fn root(prefix: &Path, expected: &str) -> Result<PathBuf> {
        let manifest = prefix.join("asset-manifest.json");
        if !fs::symlink_metadata(&manifest)?.is_file() {
            return Err("distribution asset manifest must be a regular file".into());
        }
        let mut bytes = Vec::new();
        fs::File::open(manifest)?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 8 * 1024 * 1024 || format!("{:x}", Sha256::digest(&bytes)) != expected {
            return Err("distribution asset manifest does not match this binary".into());
        }
        let declared: BTreeMap<String, String> =
            serde_json::from_value(compiler_core::parse_strict(std::str::from_utf8(&bytes)?)?)?;
        if declared.is_empty() {
            return Err("distribution asset manifest is empty".into());
        }
        let root = prefix.join("share/apsides");
        if fs::symlink_metadata(prefix.join("share"))?
            .file_type()
            .is_symlink()
            || fs::symlink_metadata(&root)?.file_type().is_symlink()
        {
            return Err("distribution asset root must not be a symlink".into());
        }
        let root = root.canonicalize()?;
        let mut observed = BTreeMap::new();
        visit(&root, &root, &mut observed)?;
        if observed != declared {
            return Err("distribution assets do not match this binary".into());
        }
        Ok(root)
    }

    const MAX_ENTRIES: usize = 20_000;
    const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
    const MAX_TREE_BYTES: u64 = 512 * 1024 * 1024;
    const MAX_DEPTH: usize = 64;

    #[derive(Default)]
    struct Budget {
        entries: usize,
        bytes: u64,
    }

    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        walk(root, directory, files, &mut Budget::default(), 0)
    }

    fn walk(
        root: &Path,
        directory: &Path,
        files: &mut BTreeMap<String, String>,
        budget: &mut Budget,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err("distribution asset tree is too deep".into());
        }
        for entry in fs::read_dir(directory)? {
            if budget.entries >= MAX_ENTRIES {
                return Err("distribution contains too many asset entries".into());
            }
            budget.entries += 1;
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .ok_or("distribution asset path is not UTF-8")?
                .to_owned();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                let target = fs::read_link(&path)?;
                if target.is_absolute() || !path.canonicalize()?.starts_with(root) {
                    return Err("distribution asset symlink escapes its installation".into());
                }
                files.insert(
                    relative,
                    format!(
                        "symlink:{}",
                        target
                            .to_str()
                            .ok_or("distribution symlink target is not UTF-8")?
                    ),
                );
            } else if kind.is_dir() {
                walk(root, &path, files, budget, depth + 1)?;
            } else if kind.is_file() {
                let length = entry.metadata()?.len();
                if length > MAX_FILE_BYTES || length > MAX_TREE_BYTES - budget.bytes {
                    return Err("distribution asset byte limit exceeded".into());
                }
                let mut hash = Sha256::new();
                let copied = std::io::copy(
                    &mut BufReader::new(fs::File::open(path)?).take(length + 1),
                    &mut hash,
                )?;
                if copied != length {
                    return Err("distribution asset size changed during verification".into());
                }
                budget.bytes += length;
                files.insert(relative, format!("sha256:{:x}", hash.finalize()));
            } else {
                return Err("distribution contains an unsupported asset type".into());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rejects_oversized_and_exhausted_asset_trees() -> Result<()> {
            let directory = tempfile::tempdir()?;
            let root = directory.path().canonicalize()?;
            let file = fs::File::create(root.join("asset"))?;
            file.set_len(MAX_FILE_BYTES + 1)?;
            assert!(visit(&root, &root, &mut BTreeMap::new()).is_err());
            file.set_len(1)?;
            for (entries, bytes, depth) in [
                (MAX_ENTRIES, 0, 0),
                (0, MAX_TREE_BYTES, 0),
                (0, 0, MAX_DEPTH + 1),
            ] {
                assert!(
                    walk(
                        &root,
                        &root,
                        &mut BTreeMap::new(),
                        &mut Budget { entries, bytes },
                        depth
                    )
                    .is_err()
                );
            }
            Ok(())
        }

        #[test]
        fn verifies_relocated_assets_and_rejects_mutation() -> Result<()> {
            let prefix = tempfile::tempdir()?;
            let assets = prefix.path().join("share/apsides");
            fs::create_dir_all(&assets)?;
            fs::write(assets.join("compiler.ts"), "compiler")?;
            let mut files = BTreeMap::new();
            visit(&assets, &assets, &mut files)?;
            let manifest = serde_json::to_vec(&files)?;
            let digest = format!("{:x}", Sha256::digest(&manifest));
            fs::write(prefix.path().join("asset-manifest.json"), &manifest)?;
            assert_eq!(root(prefix.path(), &digest)?, assets.canonicalize()?);
            fs::write(assets.join("extra.ts"), "injected")?;
            assert!(root(prefix.path(), &digest).is_err());
            fs::remove_file(assets.join("extra.ts"))?;
            fs::write(assets.join("compiler.ts"), "changed")?;
            assert!(root(prefix.path(), &digest).is_err());
            fs::write(prefix.path().join("asset-manifest.json"), "{}")?;
            assert!(root(prefix.path(), &digest).is_err());
            Ok(())
        }

        /// Mirrors a packaged installation: compiler, SDK, lockfile, a
        /// dependency reached through an internal link, runtime crate and
        /// runtime base pin. Each file is removed or extended in turn; the
        /// tree must fail verification and pass again once restored.
        #[cfg(unix)]
        #[test]
        fn rejects_each_removed_or_extended_asset_until_restored() -> Result<()> {
            let prefix = tempfile::tempdir()?;
            let assets = prefix.path().join("share/apsides");
            let targets = [
                ("packages/compiler/src/compiler.ts", false),
                ("packages/kubernetes/src/model.ts", true),
                ("deno.lock", false),
                (
                    "node_modules/.deno/ses@2.3.0/node_modules/ses/package.json",
                    true,
                ),
                ("crates/runtime/src/lib.rs", false),
                ("scripts/runtime-base/reference", true),
            ];
            for (path, _) in targets {
                let file = assets.join(path);
                fs::create_dir_all(file.parent().ok_or("asset path has no parent")?)?;
                fs::write(file, format!("{path}\n"))?;
            }
            std::os::unix::fs::symlink(
                ".deno/ses@2.3.0/node_modules/ses",
                assets.join("node_modules/ses"),
            )?;
            let assets = assets.canonicalize()?;
            let mut files = BTreeMap::new();
            visit(&assets, &assets, &mut files)?;
            let manifest = serde_json::to_vec(&files)?;
            let digest = format!("{:x}", Sha256::digest(&manifest));
            fs::write(prefix.path().join("asset-manifest.json"), manifest)?;
            root(prefix.path(), &digest)?;
            for (path, remove) in targets {
                let file = assets.join(path);
                let original = fs::read(&file)?;
                if remove {
                    fs::remove_file(&file)?;
                } else {
                    let mut extended = original.clone();
                    extended.push(b'\n');
                    fs::write(&file, extended)?;
                }
                if root(prefix.path(), &digest).is_ok() {
                    return Err(format!("damaged asset {path} passed verification").into());
                }
                fs::write(&file, original)?;
                root(prefix.path(), &digest)?;
            }
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn permits_internal_links_and_rejects_external_targets() -> Result<()> {
            let prefix = tempfile::tempdir()?;
            let assets = prefix.path().join("share/apsides");
            fs::create_dir_all(assets.join("store"))?;
            let assets = assets.canonicalize()?;
            fs::write(assets.join("store/module.ts"), "module")?;
            std::os::unix::fs::symlink("store", assets.join("dependency"))?;
            let mut files = BTreeMap::new();
            visit(&assets, &assets, &mut files)?;
            let manifest = serde_json::to_vec(&files)?;
            let digest = format!("{:x}", Sha256::digest(&manifest));
            fs::write(prefix.path().join("asset-manifest.json"), manifest)?;
            assert!(root(prefix.path(), &digest).is_ok());
            fs::remove_file(assets.join("dependency"))?;
            std::os::unix::fs::symlink("../..", assets.join("dependency"))?;
            assert!(root(prefix.path(), &digest).is_err());
            Ok(())
        }
    }
}
