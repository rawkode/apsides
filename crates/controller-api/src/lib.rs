//! Strict, namespaced API contracts for the `OxideLoop` controller.
//!
//! [`OxideLoopState`] is an internal controller journal. `KeyedRootCoordinator`
//! is its sole writer; renderer and action processes never write this resource.

#![forbid(unsafe_code)]

mod crd;
mod ids;
mod types;

pub use crd::{CrdError, crd_documents, write_or_check_crds};
pub use ids::{IdError, automation_run_name, bootstrap_id, state_name};
pub use types::*;
