//! `aps check` — validate compiled IR without producing an artifact.

use crate::{Result, cli, ir, report};
use serde_json::json;

/// # Errors
/// Returns an error if the IR is unreadable or does not validate.
pub fn command(arguments: &cli::Check) -> Result<()> {
    let plan = ir::read_plan(&arguments.ir)?;
    report::emit(&json!({
        "status": "valid",
        "irDigest": plan.ir_digest,
        "productionReady": false,
    }))
}
