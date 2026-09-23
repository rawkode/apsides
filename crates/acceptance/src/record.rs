//! What a scenario records: its checks, the output it collected, and the
//! secret-canary scan over that output.
//!
//! Documents are kept in memory and written only when the scenario ends, with
//! every canary known by then redacted, so a leak the scan catches is not
//! re-published in the uploaded logs.

use crate::Result;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Canaries shorter than this are not searched for: they would match by
/// chance.
pub const MIN_CANARY_BYTES: usize = 8;

const REDACTED: &[u8] = b"[REDACTED]";

/// A scenario's checks and collected output.
pub struct Recorder {
    directory: PathBuf,
    documents: Vec<(String, Vec<u8>)>,
    canaries: BTreeSet<Vec<u8>>,
    checks: Vec<Value>,
}

impl Recorder {
    /// A recorder that will write into `directory`, which must not exist yet.
    #[must_use]
    pub const fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            documents: Vec::new(),
            canaries: BTreeSet::new(),
            checks: Vec::new(),
        }
    }

    /// Keep `bytes` to scan and, redacted, to write as `name`.
    pub fn document(&mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.documents.push((name.into(), bytes.into()));
    }

    /// Search every document for `secret`, as its raw bytes and as the base64
    /// Kubernetes stores Secret data in. Values too short to search for are
    /// counted in the report instead.
    pub fn canary(&mut self, secret: &[u8]) {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(secret)
            .into_bytes();
        for form in [secret.to_vec(), encoded] {
            self.canaries.insert(form);
        }
    }

    /// Record a passed check with its evidence.
    pub fn check(&mut self, id: &str, evidence: &Value) {
        self.checks
            .push(json!({"check": id, "passed": true, "evidence": evidence}));
    }

    /// Names of the documents, and of `extra` (scanned but never written,
    /// such as the inventory database), that contain a canary.
    #[must_use]
    pub fn leaks(&self, extra: &[(String, Vec<u8>)]) -> Vec<String> {
        let canaries = self.searchable();
        self.documents
            .iter()
            .chain(extra)
            .filter(|(_, bytes)| canaries.iter().any(|canary| contains(bytes, canary)))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// How many canaries are searched for, and how many were too short.
    #[must_use]
    pub fn canary_counts(&self) -> (usize, usize) {
        let searchable = self.searchable().len();
        (searchable, self.canaries.len() - searchable)
    }

    fn searchable(&self) -> Vec<&[u8]> {
        self.canaries
            .iter()
            .filter(|canary| canary.len() >= MIN_CANARY_BYTES)
            .map(Vec::as_slice)
            .collect()
    }

    /// Write the documents, redacted, and `report.json` holding the checks and
    /// `outcome`.
    ///
    /// # Errors
    /// Fails when the directory exists or a file cannot be written.
    pub fn write(&self, outcome: &Value) -> Result {
        std::fs::create_dir_all(self.directory.parent().unwrap_or_else(|| Path::new(".")))?;
        std::fs::create_dir(&self.directory)?;
        let canaries = self.searchable();
        for (name, bytes) in &self.documents {
            let file = self.directory.join(sanitise(name));
            std::fs::write(file, redact(bytes, &canaries))?;
        }
        let (searched, too_short) = self.canary_counts();
        let report = json!({
            "schemaVersion": "apsides.dev/acceptance-report/v1",
            "outcome": outcome,
            "checks": self.checks,
            "canaries": {"searched": searched, "tooShort": too_short},
            "documents": self.documents.iter().map(|(name, _)| sanitise(name)).collect::<Vec<_>>(),
        });
        let report = redact(&serde_json::to_vec_pretty(&report)?, &canaries);
        std::fs::write(self.directory.join("report.json"), report)?;
        Ok(())
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// `bytes` with every canary replaced, longest first.
fn redact(bytes: &[u8], canaries: &[&[u8]]) -> Vec<u8> {
    let mut ordered = canaries.to_vec();
    ordered.sort_by_key(|canary| std::cmp::Reverse(canary.len()));
    ordered.iter().fold(bytes.to_vec(), |text, canary| {
        let mut result = Vec::with_capacity(text.len());
        let mut index = 0;
        while index < text.len() {
            if text[index..].starts_with(canary) {
                result.extend_from_slice(REDACTED);
                index += canary.len();
            } else {
                result.push(text[index]);
                index += 1;
            }
        }
        result
    })
}

/// A document name as a single safe file name.
fn sanitise(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canaries_are_found_raw_and_base64_encoded_and_redacted_on_write() -> Result {
        let directory = tempfile::tempdir()?;
        let mut recorder = Recorder::new(directory.path().join("p0"));
        recorder.canary(b"postgres-password-value");
        recorder.canary(b"short");
        recorder.document("aps/clean.log", b"nothing to see".to_vec());
        recorder.document("aps/raw.log", b"uri=postgres-password-value;".to_vec());
        recorder.document(
            "aps/encoded.log",
            b"cG9zdGdyZXMtcGFzc3dvcmQtdmFsdWU=".to_vec(),
        );
        assert_eq!(recorder.leaks(&[]), ["aps/raw.log", "aps/encoded.log"]);
        let extra = [(
            "inventory.db".to_owned(),
            b"xxpostgres-password-valuexx".to_vec(),
        )];
        assert_eq!(recorder.leaks(&extra).len(), 3);
        assert_eq!(recorder.canary_counts(), (3, 1));
        recorder.check("row-survives", &json!({"rows": 1}));
        recorder.write(&json!({"passed": false}))?;
        let written = std::fs::read(directory.path().join("p0/aps-raw.log"))?;
        assert_eq!(written, b"uri=[REDACTED];");
        let report: Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("p0/report.json"))?)?;
        assert_eq!(report["checks"][0]["check"], "row-survives");
        assert!(recorder.write(&json!({})).is_err(), "never overwrites");
        Ok(())
    }
}
