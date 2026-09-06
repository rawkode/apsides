//! Opt-in consumer check for real assembled payloads; no signature or promotion claim.
use std::path::PathBuf;

#[test]
#[ignore = "requires APS_PRODUCTION_EVIDENCE_RELEASE selecting an assembled release"]
fn assembled_reproducibility_evidence_matches_production_payload() -> crate::Result<()> {
    validate("evidence/production-evidence.json", "reproducible-payload")
}

#[test]
#[ignore = "requires APS_PRODUCTION_EVIDENCE_RELEASE selecting an assembled release"]
fn assembled_input_isolation_evidence_matches_production_payload() -> crate::Result<()> {
    validate(
        "evidence/isolation-evidence.json",
        "declared-input-isolation",
    )
}

fn validate(file: &str, check: &str) -> crate::Result<()> {
    let root = PathBuf::from(
        std::env::var_os("APS_PRODUCTION_EVIDENCE_RELEASE").ok_or_else(|| {
            crate::Error::Rejected("assembled evidence check requires an explicit path".into())
        })?,
    );
    if !root.is_absolute() || !root.is_dir() {
        return crate::reject("assembled evidence check requires an existing absolute directory");
    }
    // This validates manifest-bound files, OCI/native payload correspondence and
    // canonical compilation. It deliberately does not confer signature trust.
    let (manifest, _, _) = crate::read_release(&root)?;
    crate::production_evidence::validate_record(&root, &manifest, file, check)
}
