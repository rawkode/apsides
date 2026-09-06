//! Audit commands must not bootstrap or repair the metadata database.
use serde_json::json;
use std::{fs, process::Command};

#[test]
fn audit_does_not_create_a_missing_database() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let kubeconfig = directory.path().join("kubeconfig.json");
    let database = directory.path().join("missing.db");
    let token = "audit-credential-canary-01";
    fs::write(
        &kubeconfig,
        serde_json::to_vec(&json!({
            "apiVersion":"v1","kind":"Config",
            "clusters":[{"name":"test","cluster":{"server":"https://127.0.0.1:9"}}],
            "users":[{"name":"test","user":{"token":token}}],
            "contexts":[{"name":"test","context":{"cluster":"test","user":"test","namespace":"control"}}],
            "current-context":"test"
        }))?,
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_aps"))
        .args([
            "audit",
            "--domain",
            "example",
            "--transition",
            "tceirceircfardairceirceirce-1",
            "--cluster-binding",
            "cluster-a",
            "--namespace",
            "workload",
            "--control-namespace",
            "control",
        ])
        .env("KUBECONFIG", &kubeconfig)
        .env("REAKT_INVENTORY_DATABASE", &database)
        .env_remove("REAKT_INVENTORY_TOKEN")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    let error = std::str::from_utf8(&output.stderr)?;
    assert!(
        error.starts_with("aps: coordination backend operation failed:"),
        "{error}"
    );
    assert!(!error.contains(token));
    assert!(!database.exists());
    assert!(output.stdout.is_empty());
    Ok(())
}
