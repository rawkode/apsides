#![cfg(feature = "test-support")]
//! Explicit reservation-only qualification for one runner-owned kind cluster.

mod driver {
    use platform as platform_api;
    include!("support/deployment_ceiling_driver.rs");
}

#[tokio::test]
#[ignore = "requires an exact signed production candidate and runner-owned kind cluster"]
async fn deployment_ceiling_step() -> driver::Result<()> {
    driver::run().await
}
