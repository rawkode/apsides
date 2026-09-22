#![cfg(feature = "test-support")]
//! Explicitly invoked by the CNPG demo harness to record the pinned proxy profile.
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[test]
#[ignore = "metadata for explicit disposable-cluster bootstrap"]
fn proxy_profile() -> Result<()> {
    println!(
        "APSIDES_TEST_PROFILE={}",
        json!({"envoyImage":platform::ENVOY_IMAGE,"adminImage":platform::ADMIN_IMAGE,"configurationTemplateDigest":compiler_core::digest(&platform::envoy_configuration("qualified-template")?)?,"maxLedgerBytes":platform::MAX_PROXY_LEDGER_BYTES})
    );
    Ok(())
}
