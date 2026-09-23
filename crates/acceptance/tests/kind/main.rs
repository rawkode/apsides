//! Acceptance scenarios on disposable kind clusters.
//!
//! `cuenv task acceptancePrepare` builds and signs the releases and writes
//! `work/acceptance/inputs.json`; `cuenv task acceptanceTest` runs these with
//! `APSIDES_ACCEPTANCE_INPUTS` naming it. Each scenario creates its own
//! cluster named `aps-acc-…`, never reads the caller's kubeconfig, deletes the
//! cluster when it ends, and writes a redacted report and logs under the
//! inputs' results directory. docs/reference/ACCEPTANCE.md maps each to the
//! specification's acceptance rows.

mod cleanup;
mod collision;
mod credentials;
mod phases;
mod support;
mod uncertain;
mod upgrade;
