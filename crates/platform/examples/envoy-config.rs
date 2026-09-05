//! Emit the fixed profile for native Envoy schema validation, not deployment.
#[cfg(feature = "deployer")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("output directory required")?,
    );
    std::fs::create_dir(&directory)?;
    std::fs::write(
        directory.join("envoy.json"),
        compiler_core::canonical_bytes(&platform::envoy_configuration("validation-generation")?)?,
    )?;
    std::fs::write(
        directory.join("token-sds.json"),
        compiler_core::canonical_bytes(&platform::envoy_token_sds())?,
    )?;
    Ok(())
}
#[cfg(not(feature = "deployer"))]
fn main() {
    eprintln!("enable --features deployer to emit the Envoy profile");
}
