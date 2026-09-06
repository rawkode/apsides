"""Producer orchestration tests; mocked compiler outcomes are not live isolation evidence."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import run_input_isolation as producer


class Fixture:
    def __init__(self, root, mutation=None):
        root = root.resolve()
        self.root, self.mutation = root, mutation
        self.source = root / "source" / "program.tsx"
        self.source.parent.mkdir()
        self.source.write_text("export default <Controller />;\n")
        self.cli = root / "reakt"
        self.cli.write_text("mock CLI")
        self.helper = root / "isolation_probe.ts"
        self.helper.write_text("// mock compiler probe helper\n")
        self.output = root / "work" / "isolation"
        self.artifact = root / "artifact"
        self.artifact.mkdir()
        self.author = {"sdk": {"sha256": "fixed"}, "toolchain": {"ses": {"version": "2.3.0"}}}
        self.inputs = {
            "compilerClosure": {
                "authoringCompiler": self.author,
                "nativeCompiler": {"buildProfileSha256": "fixed-profile"},
            },
            "evaluation": {
                "sandbox": {"engine": "SES", "moduleLoader": False},
                "permissions": {name: "denied" for name in ["read", "write", "net", "env", "run", "ffi", "sys", "import"]},
            },
        }
        self.ir = {"testOnly": False, "resources": []}
        self.write(self.artifact / "ir.json", self.ir)
        self.write(self.artifact / "build-inputs.json", self.inputs)
        self.write(self.source.parent / "reakt.lock.json", {
            "schemaVersion": "reakt.dev/source-lock/v1", "entry": self.source.name,
            "inputs": [{"path": self.source.name, "sha256": producer.shared.digest_file(self.source)[7:]}],
        })
        self.identity = {
            "irDigest": "sha256:" + "1" * 64,
            "compilerDigest": "sha256:" + "2" * 64,
            "binaryDigest": "sha256:" + "3" * 64,
            "sourceInputsDigest": "sha256:" + "4" * 64,
            "payloadFiles": {name: "sha256:" + "5" * 64 for name in producer.shared.PAYLOAD_PATHS},
            "platform": "linux/arm64", "imageDigest": "sha256:" + "6" * 64,
        }
        self.receipt = {key: copy.deepcopy(self.identity[key]) for key in [
            "irDigest", "compilerDigest", "binaryDigest", "sourceInputsDigest", "payloadFiles",
        ]}
        self.receipt["image"] = {
            "reference": "example.invalid/controller@" + self.identity["imageDigest"],
            "platform": self.identity["platform"],
        }
        self.previous = root / "reproducibility.json"
        self.write(self.previous, self.receipt)
        self.build_reads = 0
        self.probes = []
        self.baselines = []

    @staticmethod
    def write(path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))

    def build_identity(self, *_):
        self.build_reads += 1
        result = copy.deepcopy(self.identity)
        if self.mutation == "subject-change" and self.build_reads > 1:
            result["binaryDigest"] = "sha256:" + "9" * 64
        return result

    def definitions(self):
        dependency = {"name": "aps-isolation-dependency.ts", "source": "export const declared = true;\n"}
        probes = {name: {"prefix": "// controlled probe"} for name in producer.PROBES}
        for name in ["declared", "undeclared"]:
            probes[name]["dependency"] = dependency
        return {"schemaVersion": "reakt.dev/isolation-probes/v1", "probes": probes}

    def run(self, command, log, repository):
        if repository != self.root:
            raise AssertionError("producer invoked compiler outside the expected repository")
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_text("mock successful invocation\n")
        if command[1] == "verify":
            return
        if command[1] == "compile":
            destination = Path(command[command.index("--output") + 1])
            self.baselines.append(destination.name)
            ir = {"testOnly": False, "resources": [{"id": "changed"}]} if self.mutation == "baseline-ir" else self.ir
            self.write(destination / "ir.json", ir)
            inputs = copy.deepcopy(self.inputs)
            if self.mutation == "authoring":
                inputs["compilerClosure"]["authoringCompiler"] = {"different": True}
            self.write(destination / "source" / "build-inputs.json", inputs)
            return
        if command[-1] == "--describe":
            self.write(log, self.definitions())
            return
        probe = command[-1]
        self.probes.append(probe)
        compiled = Path(command[-2])
        result = {"probe": probe, "outcome": "compiled" if probe == "declared" else "rejected"}
        if probe != "declared":
            result["reason"] = {
                "undeclared": "undeclared-source-module", "clock": "clock-unavailable",
                "random": "random-unavailable", "env": "environment-unavailable",
                "read": "filesystem-unavailable", "net": "network-unavailable",
            }[probe]
        if self.mutation == "unexpected-reason" and probe == "clock":
            result["reason"] = "unrelated-syntax-error"
        if self.mutation == "unexpected-outcome" and probe == "clock":
            result["outcome"] = "compiled"
        if probe == "declared":
            self.write(compiled / "build-inputs.json", self.inputs)
        elif self.mutation == "negative-artifact" and probe == "clock":
            self.write(compiled / "ir.json", self.ir)
        if self.mutation == "source-change" and probe == "undeclared":
            self.source.write_text("export default <Changed />;\n")
        self.write(log, result)

    def qualify(self):
        with patch.object(producer, "HELPER", self.helper), \
             patch.object(producer, "run", self.run), \
             patch.object(producer.shared, "source_state", return_value=(self.root, "a" * 40)), \
             patch.object(producer.shared, "require_ignored"), \
             patch.object(producer.shared, "build_identity", self.build_identity):
            return producer.qualify(self.source, self.artifact, self.cli, self.previous, self.output)


class InputIsolationTests(unittest.TestCase):
    def test_receipt_carries_only_measured_check_and_assembly_relative_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            path = fixture.qualify()
            receipt = json.loads(path.read_text())
            self.assertEqual(set(receipt["checks"]), {"declared-input-isolation"})
            for key, value in fixture.receipt.items():
                self.assertEqual(receipt[key], value)
            self.assertEqual(receipt["runnerSourceFile"], "evidence/run_input_isolation.py")
            record = receipt["checks"]["declared-input-isolation"]
            self.assertIn("evidence/isolation_probe.ts", record["evidenceFiles"])
            self.assertIn("evidence/run_reproducibility.py", record["evidenceFiles"])
            for name in record["evidenceFiles"]:
                self.assertTrue(name.startswith("evidence/"))
                self.assertTrue((fixture.output / name).is_file())
            self.assertEqual(receipt["runnerSourceDigest"], producer.shared.digest_file(fixture.output / receipt["runnerSourceFile"]))
            self.assertEqual(set(fixture.probes), producer.PROBES)
            self.assertEqual(fixture.probes[0], "declared")
            self.assertEqual(fixture.baselines, ["baseline", "baseline-final"])
            self.assertEqual(fixture.build_reads, 2)

    def test_baseline_or_authoring_mismatch_produces_no_receipt(self):
        for mutation in ["baseline-ir", "authoring"]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                fixture = Fixture(Path(directory), mutation)
                expected = "baseline differs" if mutation == "baseline-ir" else "authoring compiler differs"
                with self.assertRaisesRegex(producer.shared.QualificationError, expected):
                    fixture.qualify()
                self.assertFalse(fixture.probes)
                self.assertFalse((fixture.output / "evidence/isolation-evidence.json").exists())

    def test_bad_probe_outcome_or_emitted_negative_artifact_produces_no_receipt(self):
        for mutation in ["unexpected-outcome", "unexpected-reason", "negative-artifact"]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                fixture = Fixture(Path(directory), mutation)
                with self.assertRaises(producer.shared.QualificationError):
                    fixture.qualify()
                self.assertIn("clock", fixture.probes)
                self.assertFalse((fixture.output / "evidence/isolation-evidence.json").exists())

    def test_final_subject_or_source_change_produces_no_receipt(self):
        for mutation in ["subject-change", "source-change"]:
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as directory:
                fixture = Fixture(Path(directory), mutation)
                with self.assertRaises(producer.shared.QualificationError):
                    fixture.qualify()
                self.assertEqual(set(fixture.probes), producer.PROBES)
                self.assertFalse((fixture.output / "evidence/isolation-evidence.json").exists())

    def test_undeclared_probe_keeps_existing_dependency_outside_lock(self):
        with tempfile.TemporaryDirectory() as directory:
            fixture = Fixture(Path(directory))
            snapshot = producer.shared.source_snapshot(fixture.source)
            definition = fixture.definitions()["probes"]["undeclared"]
            locks = {}
            for probe in ["declared", "undeclared"]:
                destination = fixture.root / probe
                producer.stage(fixture.source, snapshot, destination, probe, definition)
                dependency = destination / "aps-isolation-dependency.ts"
                self.assertTrue(dependency.is_file())
                locks[probe] = json.loads((destination / "reakt.lock.json").read_text())
                self.assertEqual(
                    (destination / "reakt.lock.json").read_text(),
                    json.dumps(locks[probe], sort_keys=True, indent=2, ensure_ascii=False) + "\n",
                )
                for item in locks[probe]["inputs"]:
                    self.assertEqual(item["sha256"], producer.shared.digest_file(destination / item["path"])[7:])
            self.assertIn("aps-isolation-dependency.ts", [item["path"] for item in locks["declared"]["inputs"]])
            self.assertNotIn("aps-isolation-dependency.ts", [item["path"] for item in locks["undeclared"]["inputs"]])


if __name__ == "__main__":
    unittest.main()
