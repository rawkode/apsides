//! Phase 5a spike: the single `#[op2]` op proving Rust<->V8 op dispatch
//! works through this pinned `deno_core`. It carries no product semantics;
//! Phase 5b defines the real `op_apsides_*` ops (fetch/output/log).

use deno_core::op2;

/// Returns a fixed string to the calling JS module. No arguments, no
/// fallible state, so this cannot itself violate the "no panicking path"
/// workspace lint.
#[op2]
#[string]
pub fn op_apsides_spike_ping() -> String {
    "pong-from-rust".to_owned()
}

deno_core::extension!(apsides_spike, ops = [op_apsides_spike_ping]);
