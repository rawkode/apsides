#![cfg(feature = "test-support")]
//! Exact signed-candidate permission-expansion lifecycle on one owned cluster.

mod driver {
    use platform as platform_api;
    include!("support/permission_upgrade_driver.rs");
}

#[tokio::test]
#[ignore = "requires two exact signed production candidates and a runner-owned kind cluster"]
async fn permission_upgrade_step() -> driver::Result<()> {
    driver::run().await
}
