#![cfg(feature = "test-support")]
//! Exact-candidate protected-claim collision qualification on one owned cluster.

mod driver {
    use platform as platform_api;
    include!("support/ownership_collision_driver.rs");
}

#[tokio::test]
#[ignore = "requires two exact signed production candidates and a runner-owned kind cluster"]
async fn ownership_collision_step() -> driver::Result<()> {
    driver::run().await
}
