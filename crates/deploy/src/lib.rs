#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]
#![forbid(unsafe_code)]
//! Pure provider-neutral deployment planning. Kubernetes execution and evidence
//! verification are deliberately outside this model's authority.
mod allocation;
pub mod allocation_counter;
pub use allocation::{AllocationNames, allocation_names, validate_allocation};
mod inventory;
pub use inventory::{
    generation_inventory, inventory_metadata_digest, predecessor_metadata,
    validate_inventory_checkpoint, validate_transition_inventory,
};
mod model;
mod ownership;
mod planner;
mod reducer;
pub use model::*;
pub use ownership::{
    FrozenOwnershipIntent, OWNERSHIP_INTENT_SCHEMA, OwnershipAction, OwnershipApproval,
    OwnershipIntent, freeze_ownership_intents, restore_ownership_intents,
    validate_frozen_ownership_intents, validate_ownership_intent,
};
pub use planner::{cleanup_decision, plan_inventory_cleanup, plan_transition, resource_identity};
pub use reducer::{advance, begin, require_event_capacity, service_account_name, verify_record};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("deployment plan rejected: {0}")]
    Plan(String),
    #[error("deployment transition blocked: {0}")]
    Transition(String),
    #[error(transparent)]
    Core(#[from] compiler_core::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
