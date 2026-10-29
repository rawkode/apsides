//! Embeds the digests of the packaging sources (see `src/sources.rs`) into
//! the crate, so `aps` can state which packaging code it was built from.

#[path = "src/sources.rs"]
mod sources;

use std::{env, error::Error, fmt::Write as _, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is unset")?;
    let root = PathBuf::from(manifest).join("../..");
    let files = sources::collect(&root)?;
    for name in sources::ROOT_FILES {
        println!("cargo::rerun-if-changed={}", root.join(name).display());
    }
    for name in sources::CRATES {
        let crate_root = root.join("crates").join(name);
        println!(
            "cargo::rerun-if-changed={}",
            crate_root.join("Cargo.toml").display()
        );
        println!(
            "cargo::rerun-if-changed={}",
            crate_root.join("src").display()
        );
    }
    let mut source = String::from(
        "/// The packaging sources this crate was built from, as `(path, digest)`\n\
         /// sorted by path.\n\
         pub const EMBEDDED: &[(&str, &str)] = &[\n",
    );
    for (path, digest) in &files {
        println!("cargo::rerun-if-changed={}", root.join(path).display());
        writeln!(source, "    ({path:?}, {digest:?}),")?;
    }
    source.push_str("];\n");
    let out = env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?;
    fs::write(PathBuf::from(out).join("packaging_files.rs"), source)?;
    Ok(())
}
