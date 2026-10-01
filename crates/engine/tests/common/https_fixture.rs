//! Thin re-export of `engine::test_support`'s local, in-process HTTPS test
//! server (`test-util`-gated) -- kept here so this crate's own `tests/*.rs`
//! binaries can keep writing `common::https_fixture::HttpsFixtureServer`
//! unchanged. The real implementation lives in `src/test_support.rs` so
//! `runtime`'s own tests (see the Apsides v0.3 plan, Phase 5b review item
//! 4) can reuse the exact same fixture server instead of re-implementing
//! it.

#[allow(
    unused_imports,
    reason = "not every tests/*.rs binary that includes this module actually uses the fixture server"
)]
pub use engine::test_support::{FIXTURE_HOST, HttpsFixtureServer};
