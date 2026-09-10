import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("run_cluster.py")
SPEC = importlib.util.spec_from_file_location("run_cluster", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("reactor runner could not be imported")
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class Fixture:
    def __init__(self, root):
        self.output = root
        self.receipt = {
            "schemaVersion": "apsides.dev/reactor-evidence/v1",
            "productionIrDigest": "sha256:" + "1" * 64,
            "fixtureIrDigest": "sha256:" + "2" * 64,
            "testedArtifactDigest": "sha256:" + "3" * 64,
            "artifactManifestFile": "tested-artifact-manifest.json",
            "overlayDigest": "sha256:" + "4" * 64,
            "runnerIdentity": {
                "sourceSha256": RUNNER.file_digest(Path(RUNNER.__file__)),
                "controllerManifestDigest": "sha256:" + "6" * 64,
                "nodeImage": "kindest/node@sha256:" + "7" * 64,
            },
            "cases": {
                "case-digest": {
                    "status": "passed",
                    "assertions": [{"checkId": "case-result", "expected": 1, "actual": 1, "passed": True}],
                    "evidenceFiles": ["semantic-results.json"],
                },
            },
            "fixtureCases": {
                "fixture-case-digest": {
                    "status": "passed",
                    "assertions": [{"checkId": "fixture-result", "expected": True, "actual": True, "passed": True}],
                    "evidenceFiles": ["cluster-transcript.json"],
                },
            },
            "checks": {
                "secret-canary-scan": {
                    "status": "passed",
                    "assertions": [{"checkId": "canary-occurrences", "expected": 0, "actual": 0, "passed": True}],
                    "evidenceFiles": ["canary-scan.json"],
                },
            },
        }
        self.coverage = {
            "schemaVersion": "apsides.dev/reactor-coverage-fragment/v1",
            "irDigest": self.receipt["fixtureIrDigest"],
            "cases": [{
                "caseDigest": "case-digest",
                "status": "passed",
                "evidenceFiles": ["reactor-evidence.json"],
            }],
            "checks": {
                "secret-canary-scan": {
                    "status": "passed",
                    "evidenceFiles": ["reactor-evidence.json"],
                },
            },
        }
        self.result = {"status": "passed"}
        for name in [
            "tested-artifact-manifest.json",
            "cluster-transcript.json",
            "semantic-results.json",
            "canary-scan.json",
        ]:
            (self.output / name).write_text(f"synthetic {name}\n")
        (self.output / "fixture-driver.kubeconfig").write_text("private fixture credential\n")
        (self.output / "diagnostic-production-pods.json").write_text("unbounded diagnostic\n")
        (self.output / "controller.log").write_text("unbounded raw controller output\n")

    def stage(self):
        RUNNER.stage_release_evidence(self.output, self.receipt, self.coverage, self.result)


class ReactorPackagingTests(unittest.TestCase):
    def fixture(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        return Fixture(Path(temporary.name))

    def assert_not_published(self, fixture):
        self.assertFalse((fixture.output / "evidence").exists())
        self.assertFalse((fixture.output / "release-coverage-fragment.json").exists())

    def test_success_preserves_records_and_publishes_only_flat_references(self):
        fixture = self.fixture()
        original_receipt = copy.deepcopy(fixture.receipt)
        original_coverage = copy.deepcopy(fixture.coverage)
        fixture.stage()

        evidence = fixture.output / "evidence"
        packaged = json.loads((evidence / "reactor-evidence.json").read_text())
        coverage = json.loads((fixture.output / "release-coverage-fragment.json").read_text())
        self.assertEqual(fixture.receipt, original_receipt)
        self.assertEqual(fixture.coverage, original_coverage)
        self.assertEqual(packaged["runnerIdentity"], original_receipt["runnerIdentity"])
        for catalogue in ["cases", "fixtureCases", "checks"]:
            for key, record in packaged[catalogue].items():
                self.assertEqual(record["assertions"], original_receipt[catalogue][key]["assertions"])
                for reference in record["evidenceFiles"]:
                    path = fixture.output / reference
                    self.assertEqual(path.parent, evidence)
                    self.assertTrue(path.is_file())
        manifest = fixture.output / packaged["artifactManifestFile"]
        self.assertEqual(manifest.parent, evidence)
        self.assertTrue(manifest.is_file())
        for record in [*coverage["cases"], *coverage["checks"].values()]:
            self.assertEqual(record["evidenceFiles"], ["evidence/reactor-evidence.json"])
        self.assertEqual(coverage["irDigest"], original_receipt["productionIrDigest"])
        self.assertNotEqual(coverage["irDigest"], original_receipt["fixtureIrDigest"])
        self.assertEqual(
            {path.name for path in evidence.iterdir()},
            {
                "tested-artifact-manifest.json",
                "cluster-transcript.json",
                "semantic-results.json",
                "canary-scan.json",
                "run_cluster.py",
                "reactor-evidence.json",
            },
        )
        self.assertNotIn("fixture-driver.kubeconfig", {path.name for path in evidence.iterdir()})
        self.assertNotIn("diagnostic-production-pods.json", {path.name for path in evidence.iterdir()})
        self.assertNotIn("controller.log", {path.name for path in evidence.iterdir()})

    def test_failed_result_publishes_nothing(self):
        fixture = self.fixture()
        fixture.result["status"] = "failed"
        fixture.stage()
        self.assert_not_published(fixture)

    def test_failed_receipt_records_are_rejected_before_publication(self):
        for catalogue in ["cases", "fixtureCases", "checks"]:
            with self.subTest(catalogue=catalogue):
                fixture = self.fixture()
                next(iter(fixture.receipt[catalogue].values()))["status"] = "failed"
                with self.assertRaisesRegex(RuntimeError, "failed reactor record"):
                    fixture.stage()
                self.assert_not_published(fixture)

    def test_unknown_and_traversing_references_are_rejected(self):
        for reference in ["debug.json", "../cluster-transcript.json"]:
            with self.subTest(reference=reference):
                fixture = self.fixture()
                fixture.receipt["cases"]["case-digest"]["evidenceFiles"] = [reference]
                with self.assertRaisesRegex(RuntimeError, "unapproved diagnostic"):
                    fixture.stage()
                self.assert_not_published(fixture)

    def test_missing_symlinked_and_oversized_diagnostics_are_rejected(self):
        for mutation in ["missing", "symlink", "oversized"]:
            with self.subTest(mutation=mutation):
                fixture = self.fixture()
                diagnostic = fixture.output / "semantic-results.json"
                if mutation == "missing":
                    diagnostic.unlink()
                elif mutation == "symlink":
                    diagnostic.unlink()
                    diagnostic.symlink_to(fixture.output / "controller.log")
                else:
                    with diagnostic.open("wb") as stream:
                        stream.truncate(4 * 1024 * 1024 + 1)
                with self.assertRaisesRegex(RuntimeError, "missing, nonregular or oversized"):
                    fixture.stage()
                self.assert_not_published(fixture)

    def test_coverage_must_reference_the_packaged_reactor_receipt(self):
        fixture = self.fixture()
        fixture.coverage["cases"][0]["evidenceFiles"] = ["cluster-transcript.json"]
        with self.assertRaisesRegex(RuntimeError, "unexpected evidence reference"):
            fixture.stage()
        self.assert_not_published(fixture)

    def test_changed_runner_source_claim_is_rejected(self):
        fixture = self.fixture()
        fixture.receipt["runnerIdentity"]["sourceSha256"] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(RuntimeError, "runner changed during measurement"):
            fixture.stage()
        self.assert_not_published(fixture)

    def test_reaction_removal_mutation_uses_packaged_transcript_evidence(self):
        runner = RUNNER.ReactorRunner.__new__(RUNNER.ReactorRunner)
        reaction = {"id": "reaction-a"}
        case = {"reactionId": reaction["id"], "scenario": "reaction-contribution-removed"}
        runner.ir = {"reactions": [reaction]}
        runner.fixture_cases = [case]
        runner.fixture_results = {}
        runner.target_revision = lambda _: "revision-before"
        runner.expected_revision = (
            lambda _, omitted_reaction=None:
                "revision-before" if omitted_reaction is None else "revision-after-removal"
        )

        runner.bind_fixture_coverage()
        row = runner.fixture_results[(reaction["id"], case["scenario"])]
        self.assertEqual(row["status"], "passed")
        self.assertEqual(row["evidenceFiles"], ["cluster-transcript.json"])
        self.assertNotIn("reactor-evidence.json", row["evidenceFiles"])

        fixture = self.fixture()
        fixture.receipt["fixtureCases"] = {
            row["caseDigest"]: {
                "status": row["status"],
                "assertions": row["assertions"],
                "evidenceFiles": row["evidenceFiles"],
            },
        }
        fixture.stage()
        packaged = json.loads((fixture.output / "evidence/reactor-evidence.json").read_text())
        packaged_row = packaged["fixtureCases"][row["caseDigest"]]
        self.assertEqual(packaged_row["assertions"], row["assertions"])
        self.assertEqual(packaged_row["evidenceFiles"], ["evidence/cluster-transcript.json"])
        self.assertTrue((fixture.output / packaged_row["evidenceFiles"][0]).is_file())


if __name__ == "__main__":
    unittest.main()
