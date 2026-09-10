use crate::{
    CaseResult, CoverageReport, EvidenceResult, ProductionEvidence, REQUIRED_CHECKS,
    ReactorEvidence, ReleaseManifest, ReleaseMode, Result,
    manifest::{MAX_DOCUMENT, bounded_read, contained, json, raw_digest},
    reject,
};
use compiler_core::{CompiledPlan, canonical_bytes, digest};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

const MAX_FILES: usize = 128;
const REACTOR_RUNNER: &str = "evidence/run_cluster.py";

/// Honest progress from merging explicitly selected evidence. This operation
/// does not verify signatures or promote a release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeSummary {
    pub release_verified: bool,
    pub missing_checks: Vec<String>,
    pub missing_cases: Vec<String>,
}

enum Receipt {
    Production {
        file: String,
        evidence: ProductionEvidence,
    },
    Reactor {
        file: String,
        evidence: ReactorEvidence,
    },
}

#[derive(Default)]
struct CollectedFiles {
    files: BTreeMap<String, Vec<u8>>,
}

impl CollectedFiles {
    fn insert(&mut self, source: &Path, output_name: String) -> Result<()> {
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return reject("evidence closure contains a nonregular file");
        }
        let bytes = bounded_read(source, MAX_DOCUMENT)?;
        match self.files.get(&output_name) {
            Some(existing) if existing != &bytes => {
                return reject("evidence filename collision has differing bytes");
            }
            Some(_) => return Ok(()),
            None => {}
        }
        if self.files.len() >= MAX_FILES {
            return reject("evidence closure has too many files");
        }
        self.files.insert(output_name, bytes);
        Ok(())
    }

    fn referenced(&mut self, parent: &Path, reference: &str) -> Result<()> {
        let name = flat_evidence_name(reference)?;
        self.insert(&parent.join(name), reference.to_owned())
    }
}

fn flat_evidence_name(reference: &str) -> Result<&str> {
    let mut components = Path::new(reference).components();
    let valid = matches!(components.next(), Some(Component::Normal(value)) if value == "evidence");
    let name = match components.next() {
        Some(Component::Normal(value)) => value
            .to_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| crate::Error::Rejected("evidence filename is not UTF-8".into()))?,
        _ => return reject("evidence reference is not a flat evidence path"),
    };
    if !valid || components.next().is_some() {
        return reject("evidence reference is not a flat evidence path");
    }
    Ok(name)
}

fn receipt_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| crate::Error::Rejected("evidence receipt filename is invalid".into()))?;
    if Path::new(name).components().count() != 1 {
        return reject("evidence receipt filename is invalid");
    }
    Ok(format!("evidence/{name}"))
}

fn collect_record_files(
    collected: &mut CollectedFiles,
    parent: &Path,
    records: &BTreeMap<String, crate::EvidenceRecord>,
) -> Result<()> {
    for record in records.values() {
        for reference in &record.evidence_files {
            collected.referenced(parent, reference)?;
        }
    }
    Ok(())
}

fn parent_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn read_receipts(paths: &[PathBuf]) -> Result<(Vec<Receipt>, CollectedFiles)> {
    if paths.len() > MAX_FILES {
        return reject("too many evidence receipts");
    }
    let mut selected_receipts = BTreeSet::new();
    let mut receipts = Vec::new();
    let mut collected = CollectedFiles::default();
    for path in paths {
        let parent = parent_directory(path);
        let parent_metadata = fs::symlink_metadata(parent)?;
        if !parent_metadata.file_type().is_dir() || parent_metadata.file_type().is_symlink() {
            return reject("evidence receipt parent is not a regular directory");
        }
        let file = receipt_name(path)?;
        if !selected_receipts.insert(fs::canonicalize(path)?) {
            return reject("duplicate evidence receipt selection");
        }
        collected.insert(path, file.clone())?;
        let bytes = collected
            .files
            .get(&file)
            .ok_or_else(|| crate::Error::Rejected("evidence receipt snapshot is missing".into()))?;
        let value = json(bytes)?;
        match value
            .get("schemaVersion")
            .and_then(serde_json::Value::as_str)
        {
            Some("apsides.dev/production-evidence/v1") => {
                let evidence: ProductionEvidence = serde_json::from_value(value)?;
                collected.referenced(parent, &evidence.runner_source_file)?;
                collect_record_files(&mut collected, parent, &evidence.checks)?;
                receipts.push(Receipt::Production { file, evidence });
            }
            Some("apsides.dev/reactor-evidence/v1") => {
                let evidence: ReactorEvidence = serde_json::from_value(value)?;
                collected.referenced(parent, REACTOR_RUNNER)?;
                collected.referenced(parent, &evidence.artifact_manifest_file)?;
                collect_record_files(&mut collected, parent, &evidence.cases)?;
                collect_record_files(&mut collected, parent, &evidence.checks)?;
                collect_record_files(&mut collected, parent, &evidence.fixture_cases)?;
                receipts.push(Receipt::Reactor { file, evidence });
            }
            _ => return reject("unsupported evidence receipt schema"),
        }
    }
    Ok((receipts, collected))
}

fn copy_manifest_files(
    candidate: &Path,
    manifest: &ReleaseManifest,
    destination: &Path,
    retain: impl Fn(&str) -> bool,
) -> Result<()> {
    fs::create_dir(destination)?;
    for name in manifest.files.keys().filter(|name| retain(name)) {
        let source = contained(candidate, name)?;
        let target = destination.join(name);
        let parent = target
            .parent()
            .ok_or_else(|| crate::Error::Rejected("candidate payload path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        fs::copy(source, target)?;
    }
    Ok(())
}

fn snapshot_candidate(
    candidate: &Path,
    manifest: &ReleaseManifest,
    release_bytes: &[u8],
    destination: &Path,
) -> Result<()> {
    copy_manifest_files(candidate, manifest, destination, |_| true)?;
    write_new(&destination.join("release.json"), release_bytes)?;
    crate::read_release(destination)?;
    Ok(())
}

fn write_collected(root: &Path, collected: &CollectedFiles) -> Result<()> {
    if !collected.files.is_empty() {
        fs::create_dir(root.join("evidence"))?;
    }
    for (name, bytes) in &collected.files {
        let path = root.join(name);
        write_new(&path, bytes)?;
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn validation_manifest(
    mut manifest: ReleaseManifest,
    collected: &CollectedFiles,
) -> Result<ReleaseManifest> {
    manifest
        .files
        .retain(|name, _| name != "coverage.json" && !name.starts_with("evidence/"));
    for (name, bytes) in &collected.files {
        manifest.files.insert(name.clone(), raw_digest(bytes));
    }
    if manifest.files.len() + 1 > MAX_FILES {
        return reject("merged release manifest would have too many files");
    }
    Ok(manifest)
}

fn required_cases(plan: &CompiledPlan) -> Result<BTreeMap<String, String>> {
    let mut required = BTreeMap::new();
    for case in plan.test_plan.iter().filter(|case| case.required) {
        let key = digest(&serde_json::to_value(case)?)?;
        if required.insert(key, case.scenario.clone()).is_some() {
            return reject("compiler produced duplicate required reactor cases");
        }
    }
    Ok(required)
}

fn evidence_result(file: &str, status: &str) -> EvidenceResult {
    EvidenceResult {
        status: status.to_owned(),
        evidence_files: vec![file.to_owned()],
    }
}

fn validate_and_select(
    root: &Path,
    manifest: &ReleaseManifest,
    plan: &CompiledPlan,
    receipts: &[Receipt],
) -> Result<(CoverageReport, MergeSummary)> {
    let required_cases = required_cases(plan)?;
    let mut checks = BTreeMap::new();
    let mut cases = BTreeMap::new();
    for receipt in receipts {
        match receipt {
            Receipt::Production { file, evidence } => {
                if evidence.checks.is_empty() {
                    return reject("production evidence receipt selects no checks");
                }
                for (name, record) in &evidence.checks {
                    if checks.contains_key(name) {
                        return reject("duplicate evidence check selection");
                    }
                    crate::production_evidence::validate_record(root, manifest, file, name)?;
                    checks.insert(name.clone(), evidence_result(file, &record.status));
                }
            }
            Receipt::Reactor { file, evidence } => {
                if evidence.cases.is_empty() && evidence.checks.is_empty() {
                    return reject("reactor evidence receipt selects no coverage");
                }
                for (key, record) in &evidence.cases {
                    let scenario = required_cases.get(key).ok_or_else(|| {
                        crate::Error::Rejected(
                            "reactor evidence selects an unknown or unrequired case".into(),
                        )
                    })?;
                    if cases.contains_key(key) {
                        return reject("duplicate reactor case selection");
                    }
                    crate::runner_evidence::validate_record(
                        root,
                        manifest,
                        file,
                        key,
                        true,
                        Some(scenario),
                    )?;
                    cases.insert(
                        key.clone(),
                        CaseResult {
                            case_digest: key.clone(),
                            result: evidence_result(file, &record.status),
                        },
                    );
                }
                for (name, record) in &evidence.checks {
                    if !crate::production_evidence::fixture_check(name)
                        || !REQUIRED_CHECKS.contains(&name.as_str())
                    {
                        return reject("reactor evidence selects an unsupported check");
                    }
                    if checks.contains_key(name) {
                        return reject("duplicate evidence check selection");
                    }
                    crate::runner_evidence::validate_record(
                        root, manifest, file, name, false, None,
                    )?;
                    checks.insert(name.clone(), evidence_result(file, &record.status));
                }
                for record in evidence.fixture_cases.values() {
                    crate::runner_evidence::record(record, manifest)?;
                }
            }
        }
    }
    let missing_checks = REQUIRED_CHECKS
        .iter()
        .filter(|name| !checks.contains_key(**name))
        .map(|name| (*name).to_owned())
        .collect();
    let missing_cases = required_cases
        .keys()
        .filter(|key| !cases.contains_key(*key))
        .cloned()
        .collect();
    let summary = MergeSummary {
        release_verified: false,
        missing_checks,
        missing_cases,
    };
    Ok((
        CoverageReport {
            schema_version: "apsides.dev/release-coverage/v1".to_owned(),
            ir_digest: manifest.ir_digest.clone(),
            cases: cases.into_values().collect(),
            checks,
        },
        summary,
    ))
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => reject("evidence merge output must be absent"),
    }
}

fn publish_output(
    output: &Path,
    collected: &CollectedFiles,
    coverage_bytes: &[u8],
    summary_bytes: &[u8],
) -> Result<()> {
    require_absent(output)?;
    fs::create_dir(output)?;
    let result = (|| {
        write_collected(output, collected)?;
        write_new(&output.join("coverage.json"), coverage_bytes)?;
        // The summary is the completion marker and is published last.
        write_new(&output.join("summary.json"), summary_bytes)
    })();
    if result.is_err() {
        for name in collected.files.keys() {
            let _ = fs::remove_file(output.join(name));
        }
        let _ = fs::remove_file(output.join("coverage.json"));
        let _ = fs::remove_file(output.join("summary.json"));
        let _ = fs::remove_dir(output.join("evidence"));
        let _ = fs::remove_dir(output);
    }
    result
}

/// Merge bounded, typed evidence receipts into a validated, no-clobber release
/// fragment. Validation finishes before the output directory is created; the
/// directory is visible while its files are published and `summary.json` marks
/// completion.
///
/// # Errors
/// Rejects invalid candidates, receipts, subjects, assertions, collisions,
/// duplicate selections, unsafe paths, or an existing output path.
pub fn merge_evidence(
    candidate: &Path,
    receipt_paths: &[PathBuf],
    output: &Path,
) -> Result<MergeSummary> {
    require_absent(output)?;
    if receipt_paths.is_empty() {
        return reject("evidence merge requires at least one receipt");
    }
    let output_parent = parent_directory(output);
    let parent_metadata = fs::symlink_metadata(output_parent)?;
    if !parent_metadata.file_type().is_dir() || parent_metadata.file_type().is_symlink() {
        return reject("evidence merge output parent is not a regular directory");
    }
    let (manifest, plan, release_bytes) = crate::read_release(candidate)?;
    if manifest.mode != ReleaseMode::Production {
        return reject("evidence can merge only into a production candidate");
    }
    let (receipts, collected) = read_receipts(receipt_paths)?;
    let scratch = tempfile::Builder::new()
        .prefix(".apsides-evidence-merge-")
        .tempdir_in(output_parent)?;
    let candidate_snapshot = scratch.path().join("candidate");
    snapshot_candidate(candidate, &manifest, &release_bytes, &candidate_snapshot)?;
    let validation_root = scratch.path().join("validation");
    copy_manifest_files(&candidate_snapshot, &manifest, &validation_root, |name| {
        name != "coverage.json" && !name.starts_with("evidence/")
    })?;
    write_collected(&validation_root, &collected)?;
    let mut validation_manifest = validation_manifest(manifest, &collected)?;
    let (coverage, summary) =
        validate_and_select(&validation_root, &validation_manifest, &plan, &receipts)?;
    let coverage_bytes = canonical_bytes(&coverage)?;
    if u64::try_from(coverage_bytes.len())
        .map_err(|_| crate::Error::Rejected("merged coverage size overflow".into()))?
        > MAX_DOCUMENT
    {
        return reject("merged coverage exceeds the document size limit");
    }
    fs::write(validation_root.join("coverage.json"), &coverage_bytes)?;
    validation_manifest
        .files
        .insert("coverage.json".to_owned(), raw_digest(&coverage_bytes));
    if summary.missing_checks.is_empty() && summary.missing_cases.is_empty() {
        crate::evidence::validate(&validation_root, &validation_manifest, &plan)?;
    }
    let summary_bytes = canonical_bytes(&summary)?;
    if u64::try_from(summary_bytes.len())
        .map_err(|_| crate::Error::Rejected("evidence merge summary size overflow".into()))?
        > MAX_DOCUMENT
    {
        return reject("evidence merge summary exceeds the document size limit");
    }
    publish_output(output, &collected, &coverage_bytes, &summary_bytes)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn single_component_paths_use_the_current_directory() -> TestResult {
        for name in ["receipt.json", "merged"] {
            if parent_directory(Path::new(name)) != Path::new(".") {
                return Err("single-component path did not resolve its parent".into());
            }
        }
        Ok(())
    }

    #[test]
    fn flat_references_exclude_traversal_and_nested_files() -> TestResult {
        for invalid in [
            "receipt.json",
            "../evidence/receipt.json",
            "evidence/sub/receipt.json",
            "evidence/../receipt.json",
            "evidence/",
        ] {
            assert!(flat_evidence_name(invalid).is_err());
        }
        if flat_evidence_name("evidence/receipt.json")? != "receipt.json" {
            return Err("flat evidence reference changed".into());
        }
        Ok(())
    }

    #[test]
    fn empty_receipts_reject_before_candidate_access() -> TestResult {
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("merged");
        let error = merge_evidence(&directory.path().join("absent"), &[], &output)
            .err()
            .ok_or("empty receipt selection was accepted")?;
        if !matches!(error, crate::Error::Rejected(_)) || output.exists() {
            return Err("empty receipt selection reached candidate access or output".into());
        }
        Ok(())
    }

    #[test]
    fn same_parent_receipts_collect_only_their_closed_files() -> TestResult {
        let directory = tempfile::tempdir()?;
        let runner = directory.path().join("runner.py");
        fs::write(&runner, b"runner")?;
        fs::write(directory.path().join("private.kubeconfig"), b"private")?;
        let receipt = json!({
            "schemaVersion": "apsides.dev/production-evidence/v1",
            "irDigest": format!("sha256:{}", "1".repeat(64)),
            "compilerDigest": format!("sha256:{}", "2".repeat(64)),
            "binaryDigest": format!("sha256:{}", "3".repeat(64)),
            "image": {
                "reference": format!("example.invalid/controller@sha256:{}", "4".repeat(64)),
                "platform": "linux/arm64"
            },
            "sourceInputsDigest": format!("sha256:{}", "5".repeat(64)),
            "payloadFiles": {},
            "runnerSourceFile": "evidence/runner.py",
            "runnerSourceDigest": raw_digest(b"runner"),
            "checks": {}
        });
        let first = directory.path().join("first.json");
        let second = directory.path().join("second.json");
        fs::write(&first, canonical_bytes(&receipt)?)?;
        fs::write(&second, canonical_bytes(&receipt)?)?;
        let (receipts, collected) = read_receipts(&[first, second])?;
        let expected = BTreeSet::from([
            "evidence/first.json".to_owned(),
            "evidence/runner.py".to_owned(),
            "evidence/second.json".to_owned(),
        ]);
        if receipts.len() != 2
            || collected.files.keys().cloned().collect::<BTreeSet<_>>() != expected
        {
            return Err("receipt collection included unrelated sibling files".into());
        }
        Ok(())
    }

    #[test]
    fn identical_filename_collisions_deduplicate_and_differing_bytes_reject() -> TestResult {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        fs::write(first.path().join("report.json"), b"same")?;
        fs::write(second.path().join("report.json"), b"same")?;
        let mut files = CollectedFiles::default();
        files.referenced(first.path(), "evidence/report.json")?;
        files.referenced(second.path(), "evidence/report.json")?;
        if files.files.len() != 1 {
            return Err("identical collision was not deduplicated".into());
        }
        fs::write(second.path().join("report.json"), b"different")?;
        if files
            .referenced(second.path(), "evidence/report.json")
            .is_ok()
        {
            return Err("differing collision was accepted".into());
        }
        Ok(())
    }

    #[test]
    fn symlinked_closure_file_is_rejected() -> TestResult {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let directory = tempfile::tempdir()?;
            fs::write(directory.path().join("target.json"), b"target")?;
            symlink(
                directory.path().join("target.json"),
                directory.path().join("linked.json"),
            )?;
            let mut files = CollectedFiles::default();
            if files
                .referenced(directory.path(), "evidence/linked.json")
                .is_ok()
            {
                return Err("symlinked closure file was accepted".into());
            }
        }
        Ok(())
    }

    #[test]
    fn existing_output_is_preserved_by_exclusive_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("merged");
        fs::create_dir(&output)?;
        fs::write(output.join("canary"), b"preserved")?;
        let result = publish_output(&output, &CollectedFiles::default(), b"{}", b"{}");
        if result.is_ok() || fs::read(output.join("canary"))? != b"preserved" {
            return Err("no-clobber publish changed an existing output".into());
        }
        Ok(())
    }
}
