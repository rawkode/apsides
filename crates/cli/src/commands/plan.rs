//! `aps plan` — validate compiled IR against a supplied permission ceiling.

use crate::{Result, cli, ir, report};
use compiler_core::{PermissionCeiling, validate_ceiling};
use std::fs;

/// # Errors
/// Returns an error if either input is unreadable, or if the plan requires
/// permissions the ceiling does not grant.
pub fn command(arguments: &cli::Plan) -> Result<()> {
    let plan = ir::read_plan(&arguments.ir)?;
    let ceiling: PermissionCeiling = serde_json::from_value(compiler_core::parse_strict(
        &fs::read_to_string(&arguments.ceiling)?,
    )?)?;
    validate_ceiling(&plan, &ceiling)?;
    report::emit(&plan)
}
