//! What a fixture release promises the runner, checked before any cluster
//! exists.
//!
//! `release::read_release` establishes payload integrity, the controller
//! image's OCI identity and that the plan and test plan are the trusted
//! derivation of the IR. This module adds the fixture contract on top: the
//! release must be test-only, every workload must run the local fixture image,
//! and the compiler's fixture coverage must cover exactly the generated test
//! plan plus one mutation check per reaction, bound to these fixtures and this
//! IR.

use crate::{FIXTURE_IMAGE, Result, input};
use compiler_core::{CompiledPlan, InputMode, TestCase, digest};
use release::{ReleaseManifest, ReleaseMode};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// The scenario the compiler adds per reaction to prove the suite notices a
/// reaction's contribution going missing.
pub const MUTATION_SCENARIO: &str = "reaction-contribution-removed";

/// Identifies the synthetic rollout key the fixture controller signs
/// sensitive revisions with. Never a production key.
pub const ROLLOUT_KEY_ID: &str = "fixture-v1";
pub const ROLLOUT_KEY: &str = "synthetic-fixture-key-never-production-0123456789";

/// The largest fixture document the runner reads.
const MAX_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;

/// A fixture release that has passed every offline check.
#[derive(Debug)]
pub struct FixtureRelease {
    pub root: PathBuf,
    pub manifest: ReleaseManifest,
    pub plan: CompiledPlan,
    /// Digest of `release.json`: the tested artifact's identity.
    pub release_digest: String,
    pub fixtures: Value,
    /// Digest of the canonical fixtures document.
    pub fixture_digest: String,
    /// Digest of the canonical overlay document.
    pub overlay_digest: String,
    pub production_ir_digest: String,
    /// The compiler's required fixture cases, in coverage order.
    pub coverage: Vec<CoverageCase>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CoverageCase {
    pub reaction_id: String,
    pub scenario: String,
    pub status: String,
    pub fixture_sha256: String,
    pub overlay_identity: String,
    #[serde(default)]
    pub applicability: Applicability,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Applicability {
    #[serde(default)]
    pub input_ids: Vec<String>,
    #[serde(default)]
    pub input_modes: Vec<String>,
    #[serde(default)]
    pub managed_resource_ids: Vec<String>,
}

impl FixtureRelease {
    /// Read and check a fixture release directory.
    ///
    /// # Errors
    /// Returns an error if the release fails integrity checks, is not a
    /// fixture release, or its fixture contract is inconsistent.
    pub fn load(root: &Path) -> Result<Self> {
        let (manifest, plan, bytes) = release::read_release(root)?;
        if manifest.mode != ReleaseMode::Fixture || !plan.ir.test_only {
            return input(
                "cluster tests run only a fixture release; build it with `aps build --mode fixture`",
            );
        }
        let read = |name: &str| {
            manifest.files.get(name).map_or_else(
                || input(format!("fixture release is missing {name}")),
                |expected| verified_document(root, name, expected),
            )
        };
        let fixtures = read("fixtures.json")?;
        let overlay = read("fixture-overlay.json")?;
        let coverage = read("fixture-coverage.json")?;
        require_fixture_workloads(&plan)?;
        let checked = validate_coverage(&plan, &fixtures, &overlay, &coverage)?;
        Ok(Self {
            root: root.to_path_buf(),
            release_digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
            fixture_digest: digest(&fixtures)?,
            overlay_digest: digest(&overlay)?,
            production_ir_digest: format!(
                "sha256:{}",
                coverage["productionIrSha256"].as_str().unwrap_or_default()
            ),
            manifest,
            plan,
            fixtures,
            coverage: checked,
        })
    }

    /// The generated input fixture for `input_id`.
    #[must_use]
    pub fn fixture(&self, input_id: &str) -> Option<&Value> {
        self.fixtures["inputs"]
            .as_array()?
            .iter()
            .find(|item| item["inputId"] == input_id)
    }

    /// The reproducible seed the suite starts from: which inputs exist and
    /// which keys they carry, never their values.
    #[must_use]
    pub fn seed(&self) -> Value {
        let inputs = self.fixtures["inputs"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let id = item["inputId"].as_str().unwrap_or_default();
                        let kind = self
                            .plan
                            .ir
                            .inputs
                            .iter()
                            .find(|input| input.id == id)
                            .map(|input| input.kind.clone());
                        let keys = item["data"]
                            .as_object()
                            .map(|data| data.keys().cloned().collect::<BTreeSet<_>>())
                            .unwrap_or_default();
                        json!({"id": id, "kind": kind, "state": item["state"], "dataKeys": keys})
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        json!({
            "schemaVersion": "apsides.dev/reactor-seed/v1",
            "fixtureIrDigest": self.plan.ir_digest,
            "rolloutKeyId": ROLLOUT_KEY_ID,
            "inputs": inputs,
        })
    }
}

/// A fixture document, parsed from the same bytes whose digest is checked
/// against the release manifest, so a file swapped after `read_release`
/// verified it is refused rather than used.
fn verified_document(root: &Path, name: &str, expected: &str) -> Result<Value> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(root.join(name))?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return input(format!("fixture release document {name} is oversized"));
    }
    if format!("sha256:{:x}", Sha256::digest(&bytes)) != expected {
        return input(format!(
            "fixture release document {name} differs from its manifest digest"
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| crate::Error::Input(format!("{name} is not UTF-8")))?;
    Ok(compiler_core::parse_strict(text)?)
}

/// Every container of every Deployment must run the local fixture image and
/// never pull.
fn require_fixture_workloads(plan: &CompiledPlan) -> Result<()> {
    for resource in plan
        .ir
        .resources
        .iter()
        .filter(|item| item.kind == "Deployment")
    {
        let spec = &resource.desired["spec"]["template"]["spec"];
        let containers = ["containers", "initContainers"]
            .iter()
            .flat_map(|field| spec[field].as_array().cloned().unwrap_or_default());
        for container in containers {
            if container["image"] != FIXTURE_IMAGE || container["imagePullPolicy"] != "Never" {
                return input(format!(
                    "{} does not run the local fixture image {FIXTURE_IMAGE} with imagePullPolicy Never",
                    resource.id
                ));
            }
        }
    }
    Ok(())
}

/// The compiler's fixture coverage must be bound to these fixtures and this
/// IR, and must require exactly the generated test plan plus one mutation
/// check per reaction.
fn validate_coverage(
    plan: &CompiledPlan,
    fixtures: &Value,
    overlay: &Value,
    coverage: &Value,
) -> Result<Vec<CoverageCase>> {
    let fixture_hex = hex(&digest(fixtures)?);
    let ir_hex = hex(&plan.ir_digest);
    if coverage["schemaVersion"] != "apsides.dev/fixture-coverage/v1"
        || coverage["testOnly"] != true
        || coverage["productionReady"] != false
    {
        return input("fixture coverage has an invalid safety envelope");
    }
    if coverage["fixtureSha256"] != fixture_hex || coverage["fixtureIrSha256"] != ir_hex {
        return input("fixture coverage is not bound to the release's fixtures and IR");
    }
    if overlay["fixtureIrSha256"] != ir_hex
        || overlay["productionIrSha256"] != coverage["productionIrSha256"]
        || !coverage["productionIrSha256"].is_string()
    {
        return input("fixture overlay and coverage identities differ");
    }
    let planned: BTreeMap<(&str, &str), &TestCase> = plan
        .test_plan
        .iter()
        .map(|case| ((case.reaction_id.as_str(), case.scenario.as_str()), case))
        .collect();
    let reactions: BTreeSet<&str> = plan
        .ir
        .reactions
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    let mut cases = Vec::new();
    for entry in coverage["reactions"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
    {
        let id = entry["id"].as_str().unwrap_or_default();
        let entry_cases: Vec<CoverageCase> =
            serde_json::from_value(entry["cases"].clone()).unwrap_or_default();
        let mut expected: BTreeSet<&str> = planned
            .keys()
            .filter(|(reaction, _)| *reaction == id)
            .map(|(_, scenario)| *scenario)
            .collect();
        expected.insert(MUTATION_SCENARIO);
        let covered: BTreeSet<&str> = entry_cases
            .iter()
            .map(|case| case.scenario.as_str())
            .collect();
        if !reactions.contains(id) || covered != expected {
            return input(format!(
                "fixture coverage for {id} does not cover exactly its generated scenarios"
            ));
        }
        for case in entry_cases {
            if case.reaction_id != id
                || !seen.insert((case.reaction_id.clone(), case.scenario.clone()))
                || case.status != "required"
                || case.fixture_sha256 != fixture_hex
                || case.overlay_identity != ir_hex
            {
                return input(format!(
                    "fixture case {id}:{} is duplicated, optional or not bound to this release",
                    case.scenario
                ));
            }
            if case.scenario != MUTATION_SCENARIO {
                let key = (case.reaction_id.as_str(), case.scenario.as_str());
                let matches = planned
                    .get(&key)
                    .is_some_and(|planned| case.applicability == applicability(plan, planned));
                if !matches {
                    return input(format!(
                        "fixture applicability for {id}:{} differs from the test plan",
                        case.scenario
                    ));
                }
            }
            cases.push(case);
        }
    }
    let covered: BTreeSet<&str> = seen.iter().map(|(reaction, _)| reaction.as_str()).collect();
    if covered != reactions || seen.len() != planned.len() + reactions.len() {
        return input("fixture coverage does not cover every reaction");
    }
    Ok(cases)
}

/// The stimulus set a planned case applies to, as the compiler records it.
fn applicability(plan: &CompiledPlan, case: &TestCase) -> Applicability {
    let inputs = plan
        .ir
        .inputs
        .iter()
        .filter(|input| case.input_ids.contains(&input.id))
        .collect::<Vec<_>>();
    // Records each input's own mode string, never collapsing Output into
    // Observed: the fixture-coverage document the TypeScript compiler emits
    // (`fixtures.ts`'s `fixtureCoverage`) already records the raw mode, so
    // this must match it exactly for `validate_coverage`'s applicability
    // comparison to agree case for case.
    let modes = inputs
        .iter()
        .map(|input| match input.mode {
            InputMode::Observed => "observed".to_owned(),
            InputMode::Output => "output".to_owned(),
            InputMode::Managed => "managed".to_owned(),
        })
        .collect::<BTreeSet<_>>();
    let managed = inputs
        .iter()
        .filter_map(|input| input.managed_resource_id.clone())
        .collect::<BTreeSet<_>>();
    Applicability {
        input_ids: case.input_ids.clone(),
        input_modes: modes.into_iter().collect(),
        managed_resource_ids: managed.into_iter().collect(),
    }
}

fn hex(digest: &str) -> String {
    digest.trim_start_matches("sha256:").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn generated_coverage_is_accepted() -> Result<()> {
        let plan = testing::plan()?;
        let (fixtures, overlay, coverage) = testing::fixture_documents(&plan)?;
        let cases = validate_coverage(&plan, &fixtures, &overlay, &coverage)?;
        assert_eq!(cases.len(), plan.test_plan.len() + plan.ir.reactions.len());
        assert_eq!(
            cases
                .iter()
                .filter(|case| case.scenario == MUTATION_SCENARIO)
                .count(),
            plan.ir.reactions.len()
        );
        require_fixture_workloads(&plan)?;
        Ok(())
    }

    #[test]
    fn coverage_must_match_the_plan_and_identities() -> Result<()> {
        let plan = testing::plan()?;
        let (fixtures, overlay, coverage) = testing::fixture_documents(&plan)?;

        let mut dropped = coverage.clone();
        if let Some(cases) = dropped["reactions"][0]["cases"].as_array_mut() {
            cases.pop();
        }
        assert!(validate_coverage(&plan, &fixtures, &overlay, &dropped).is_err());

        let mut optional = coverage.clone();
        optional["reactions"][0]["cases"][0]["status"] = json!("optional");
        assert!(validate_coverage(&plan, &fixtures, &overlay, &optional).is_err());

        let mut rebound = coverage.clone();
        rebound["reactions"][0]["cases"][0]["applicability"]["inputIds"] = json!([]);
        assert!(validate_coverage(&plan, &fixtures, &overlay, &rebound).is_err());

        let mut other = fixtures.clone();
        other["inputs"][0]["data"]["value"] = json!("changed");
        assert!(validate_coverage(&plan, &other, &overlay, &coverage).is_err());

        let mut unsafe_envelope = coverage.clone();
        unsafe_envelope["productionReady"] = json!(true);
        assert!(validate_coverage(&plan, &fixtures, &overlay, &unsafe_envelope).is_err());

        let mut overlay_mismatch = overlay;
        overlay_mismatch["productionIrSha256"] = json!("0".repeat(64));
        assert!(validate_coverage(&plan, &fixtures, &overlay_mismatch, &coverage).is_err());
        Ok(())
    }

    #[test]
    fn fixture_documents_are_parsed_from_the_bytes_that_were_verified() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("fixtures.json");
        std::fs::write(&path, br#"{"inputs":[]}"#)?;
        let digest = format!("sha256:{:x}", Sha256::digest(br#"{"inputs":[]}"#));
        assert_eq!(
            verified_document(directory.path(), "fixtures.json", &digest)?,
            json!({"inputs": []})
        );
        std::fs::write(&path, br#"{"inputs":[1]}"#)?;
        assert!(verified_document(directory.path(), "fixtures.json", &digest).is_err());
        Ok(())
    }

    #[test]
    fn production_workload_images_are_refused() -> Result<()> {
        let mut ir = testing::ir();
        ir["resources"][1]["desired"]["spec"]["template"]["spec"]["containers"][0]["image"] =
            json!("registry.example/api@sha256:abc");
        let plan = compiler_core::parse_and_validate(&ir.to_string())?;
        assert!(require_fixture_workloads(&plan).is_err());
        Ok(())
    }

    /// A production IR with an output-only reaction (`output-only`, watching
    /// only the handler-output's `output`-mode input) and a mixed reaction
    /// (`rollout-mixed`, watching both an `observed` input and the same
    /// `output`-mode one) -- the two shapes B3's review residual reported.
    /// The SDK's own `<Deployment rolloutOn>` can only ever declare a
    /// single-input reaction (each array entry becomes its own reaction via
    /// `Domain[declareRollout]`), so a genuinely mixed *single* reaction can
    /// only be authored as raw IR, exactly as here.
    fn cross_language_ir() -> Value {
        let source = json!({"file": "program.tsx", "line": 1, "column": 1});
        let deployment = |id: &str, name: &str| {
            json!({
                "id": id, "apiVersion": "apps/v1", "kind": "Deployment", "name": name, "namespace": "workload",
                "desired": {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "namespace": "workload"}, "spec": {"replicas": 1, "selector": {"matchLabels": {"app": name}}, "template": {"metadata": {"labels": {"app": name}}, "spec": {"serviceAccountName": "api-workload", "containers": [{"name": "api", "image": format!("example.invalid/api@sha256:{}", "0".repeat(64))}]}}}},
                "lifecycle": {"deletionPolicy": "orphan"}, "source": source,
            })
        };
        json!({
            "schemaVersion": "apsides.dev/ir/v2", "testOnly": false,
            "domain": {
                "id": "coverage-check", "namespace": "workload", "controlNamespace": "apsides-system",
                "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
                "minKubernetesVersion": "1.34",
                "budgets": {"maxResources": 16, "maxInputs": 16, "maxReactions": 16, "maxObjectBytes": 262_144, "maxQueueDepth": 16, "maxConcurrentRequests": 4},
            },
            "resources": [
                deployment("output-only", "output-only"),
                deployment("mixed", "mixed"),
                {
                    "id": "output-flags", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
                    "desired": {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "flags", "namespace": "workload", "labels": {"apsides.dev/handler-output": "output-flags"}}},
                    "lifecycle": {"deletionPolicy": "orphan"}, "source": source, "handlerOutput": true,
                },
            ],
            "inputs": [
                {"id": "settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "namespace": "workload", "mode": "observed", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source},
                {"id": "output-flags-input", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload", "mode": "output", "managedResourceId": "output-flags", "projection": ["data"], "sensitive": false, "missing": "block-dependent-mutations", "source": source},
            ],
            "reactions": [
                {"id": "output-only", "op": "rollout", "inputs": ["output-flags-input"], "target": "output-only", "source": source},
                {"id": "rollout-mixed", "op": "rollout", "inputs": ["settings", "output-flags-input"], "target": "mixed", "source": source},
            ],
            "handlers": [{
                "id": "sync-flags", "module": "handlers/sync-flags.ts", "export": "syncFlags",
                "trigger": {"kind": "input-changed", "input": "settings"},
                "inputs": ["settings"], "egress": [{"host": "api.example.com", "port": 443}],
                "outputs": ["output-flags"], "timeoutMillis": 30000, "maxHeapBytes": 67_108_864,
                "bundleSha256": "1".repeat(64), "source": source,
            }],
            "outputs": [{"id": "output-flags", "name": "flags", "namespace": "workload"}],
            "provenance": {"compiler": "apsides/0.1", "inputs": [
                {"path": "program.tsx", "sha256": "0".repeat(64)},
                {"path": "handlers/sync-flags.ts", "sha256": "1".repeat(64)},
            ]},
        })
    }

    /// Runs `fixtures.ts`'s own `buildFixtureOutputs` (the function B3's
    /// review residual found out of sync with Rust) through a real Deno
    /// process on [`cross_language_ir`], and writes its four documents to
    /// `destination` under the names `contract.rs::load` reads. Bypasses the
    /// JSX renderer entirely (`compiler.ts`'s full pipeline), since the SDK
    /// cannot author the mixed-input reaction this needs; this still
    /// exercises the exact TypeScript function the review flagged, with a
    /// real Deno evaluation, not a hand-maintained duplicate of its output.
    fn run_fixtures_ts(ir: &Value, destination: &std::path::Path) -> Result<()> {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let fixtures_ts = repo_root.join("packages/compiler/src/fixtures.ts");
        let harness = destination.join("harness.ts");
        std::fs::write(
            &harness,
            format!(
                r"import {{ buildFixtureOutputs }} from {fixtures_ts:?};
const ir = {ir};
const destination = Deno.args[0];
const outputs = buildFixtureOutputs(ir);
await Deno.writeTextFile(`${{destination}}/fixture-ir.json`, JSON.stringify(outputs.fixtureIr));
await Deno.writeTextFile(`${{destination}}/fixtures.json`, JSON.stringify(outputs.fixtures));
await Deno.writeTextFile(`${{destination}}/fixture-overlay.json`, JSON.stringify(outputs.overlay));
await Deno.writeTextFile(`${{destination}}/fixture-coverage.json`, JSON.stringify(outputs.coverage));
",
                fixtures_ts = fixtures_ts.display(),
                ir = ir
            ),
        )?;
        let status = std::process::Command::new("deno")
            .args(["run", "--allow-read", "--allow-write"])
            .arg(&harness)
            .arg(destination)
            .status()
            .map_err(|error| crate::Error::Infrastructure(format!("deno spawn failed: {error}")))?;
        if !status.success() {
            return Err(crate::Error::Infrastructure(
                "deno fixtures.ts harness failed".into(),
            ));
        }
        Ok(())
    }

    fn read_json(path: &std::path::Path) -> Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    #[test]
    fn typescript_fixture_coverage_for_output_only_and_mixed_reactions_loads_through_rust()
    -> Result<()> {
        let production = cross_language_ir();
        let destination = tempfile::tempdir()?;
        run_fixtures_ts(&production, destination.path())?;

        let fixture_ir = read_json(&destination.path().join("fixture-ir.json"))?;
        let fixtures = read_json(&destination.path().join("fixtures.json"))?;
        let overlay = read_json(&destination.path().join("fixture-overlay.json"))?;
        let coverage = read_json(&destination.path().join("fixture-coverage.json"))?;

        let plan = compiler_core::parse_and_validate(&fixture_ir.to_string())?;
        let cases = validate_coverage(&plan, &fixtures, &overlay, &coverage)?;
        assert_eq!(cases.len(), plan.test_plan.len() + plan.ir.reactions.len());

        let output_only: BTreeSet<&str> = cases
            .iter()
            .filter(|case| case.reaction_id == "output-only")
            .map(|case| case.scenario.as_str())
            .collect();
        assert!(output_only.contains("projected-change"));
        assert!(output_only.contains("restart-and-relist"));
        for excluded in ["missing-input", "input-returned", "coalesced-inputs"] {
            assert!(
                !output_only.contains(excluded),
                "did not expect {excluded} for an output-only reaction"
            );
        }

        let mixed: Vec<_> = cases
            .iter()
            .filter(|case| case.reaction_id == "rollout-mixed")
            .collect();
        let mixed_scenarios: Vec<&str> = mixed.iter().map(|case| case.scenario.as_str()).collect();
        let unique: BTreeSet<&str> = mixed_scenarios.iter().copied().collect();
        assert_eq!(
            mixed_scenarios.len(),
            unique.len(),
            "duplicate scenario rows for the mixed reaction: {mixed_scenarios:?}"
        );
        let projected = mixed
            .iter()
            .find(|case| case.scenario == "projected-change")
            .ok_or_else(|| crate::Error::Case("expected one projected-change row".into()))?;
        assert_eq!(
            projected.applicability.input_ids,
            ["output-flags-input", "settings"]
        );
        assert_eq!(projected.applicability.input_modes, ["observed", "output"]);
        Ok(())
    }
}
