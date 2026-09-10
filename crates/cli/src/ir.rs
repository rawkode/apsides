//! Reading compiled IR from disk under an explicit input bound.

use crate::Result;
use compiler_core::{CompiledPlan, parse_and_validate};
use std::{fs, path::Path};

/// The largest IR document the CLI will read.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Read and validate one compiled IR document.
///
/// # Errors
/// Returns an error if the file is unreadable, exceeds [`MAX_BYTES`], or does
/// not validate.
pub fn read_plan(path: &Path) -> Result<CompiledPlan> {
    if fs::metadata(path)?.len() > MAX_BYTES {
        return Err("IR exceeds 8 MiB input limit".into());
    }
    Ok(parse_and_validate(&fs::read_to_string(path)?)?)
}
