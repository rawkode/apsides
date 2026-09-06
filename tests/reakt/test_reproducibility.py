import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("run_reproducibility.py")
SPEC = importlib.util.spec_from_file_location("run_reproducibility", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("reproducibility producer could not be imported")
PRODUCER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PRODUCER)


class Fixture:
    def __init__(self, root, *, mismatch=None, fail_second=False, statuses=None, heads=None):
        self.root = root
        self.mismatch = mismatch
        self.fail_second = fail_second
        self.statuses = iter(statuses or ["", ""])
        self.heads = iter(heads or ["a" * 40, "a" * 40])
        self.last_status = ""
        self.last_head = "a" * 40
        self.builds = []
        self.source = root / "examples" / "program.tsx"
        self.profile = root / "profile.json"
        self.cli = root / "bin" / "reakt"
        self.output = root / "work" / "reproducibility"
        self.prepare_inputs()

    def prepare_inputs(self):
        self.source.parent.mkdir(parents=True)
        self.source.write_text("export default <ConfigMap name='settings' />;\n")
        helper = self.source.parent / "api.tsx"
        helper.write_text("export const value = 'fixed';\n")
        inputs = []
        for path in [helper, self.source]:
            inputs.append(
                {
                    "path": path.name,
                    "sha256": PRODUCER.digest_file(path)[7:],
                }
            )
        (self.source.parent / "reakt.lock.json").write_text(
            json.dumps(
                {
                    "schemaVersion": "reakt.dev/source-lock/v1",
                    "entry": self.source.name,
                    "inputs": inputs,
                }
            )
        )
        self.locked_inputs = inputs
        self.profile.write_text('{"schemaVersion":"reakt.dev/build-profile/v1"}\n')
        self.cli.parent.mkdir()
        self.cli.write_bytes(b"test CLI")

    def run(self, arguments, **options):
        command = [str(argument) for argument in arguments]
        if command and command[0] == "git":
            return self.git(command)
        if command and command[0] == str(self.cli.resolve()):
            index = len(self.builds) + 1
            self.builds.append(command)
            options["stdout"].write(f"independent build {index}\n")
            if self.fail_second and index == 2:
                return subprocess.CompletedProcess(command, 1)
            output = Path(command[command.index("--output") + 1])
            self.write_build(output, index)
            return subprocess.CompletedProcess(command, 0)
        raise AssertionError(f"unexpected subprocess: {command}")

    def git(self, command):
        arguments = command[3:]
        if arguments == ["rev-parse", "--show-toplevel"]:
            return subprocess.CompletedProcess(command, 0, stdout=str(self.root) + "\n", stderr="")
        if arguments == ["ls-files", "--error-unmatch", "--", "examples/program.tsx"]:
            return subprocess.CompletedProcess(command, 0, stdout="examples/program.tsx\n", stderr="")
        if arguments == ["rev-parse", "--verify", "HEAD"]:
            try:
                self.last_head = next(self.heads)
            except StopIteration:
                pass
            return subprocess.CompletedProcess(command, 0, stdout=self.last_head + "\n", stderr="")
        if arguments == ["status", "--porcelain=v1", "--untracked-files=all"]:
            try:
                self.last_status = next(self.statuses)
            except StopIteration:
                pass
            return subprocess.CompletedProcess(command, 0, stdout=self.last_status, stderr="")
        if arguments[:3] == ["check-ignore", "--quiet", "--no-index"]:
            return subprocess.CompletedProcess(command, 0, stdout="", stderr="")
        raise AssertionError(f"unexpected git command: {command}")

    def write_build(self, output, index):
        output.mkdir(parents=True)
        (output / "image").mkdir()
        (output / "linux-binary").mkdir()
        changed = index == 2
        ir = {
            "testOnly": False,
            "schemaVersion": "reakt.dev/ir/v1",
            "resources": [],
            "provenance": {"compiler": "reakt/0.1", "inputs": self.locked_inputs},
        }
        if changed and self.mismatch == "ir":
            ir["resources"] = [{"id": "changed"}]
        ir_digest = PRODUCER.digest_bytes(PRODUCER.canonical(ir))
        plan = {"irDigest": ir_digest, "ir": ir, "effects": []}
        if changed and self.mismatch == "compiled":
            plan["effects"] = [{"target": "changed"}]
        closure = {
            "schemaVersion": "reakt.dev/compiler-closure/v2",
            "tool": "fixed",
            "nativeCompiler": {
                "buildProfileSha256": PRODUCER.digest_file(self.profile)
            },
        }
        if changed and self.mismatch == "compiler":
            closure["tool"] = "changed"
        inputs = {
            "schemaVersion": "reakt.dev/build-inputs/v1",
            "compilerClosure": closure,
            "compilerClosureDigest": PRODUCER.digest_bytes(PRODUCER.canonical(closure)),
            "entry": self.source.name,
            "inputs": self.locked_inputs,
            "lock": {
                "path": "reakt.lock.json",
                "schemaVersion": "reakt.dev/source-lock/v1",
                "sha256": PRODUCER.digest_file(self.source.parent / "reakt.lock.json")[7:],
            },
        }
        if changed and self.mismatch == "inputs":
            inputs["inputs"] = [{"path": "program.tsx", "sha256": "0" * 64}]
        binary = b"controller-one"
        if changed and self.mismatch == "binary":
            binary = b"controller-two"
        archive = b"oci-archive-one"
        if changed and self.mismatch == "archive":
            archive = b"oci-archive-two"
        image_digest = "sha256:" + "1" * 64
        if changed and self.mismatch == "image":
            image_digest = "sha256:" + "2" * 64
        binary_digest = PRODUCER.digest_bytes(binary)
        result = {
            "schemaVersion": "reakt.dev/build-result/v1",
            "releaseVerified": False,
            "binary": "linux-binary/reakt-controller",
            "image": "image/controller.oci.tar",
            "buildInputs": "build-inputs.json",
            "binaryDigest": binary_digest,
            "irDigest": ir_digest,
            "platform": "linux/arm64",
        }
        (output / "ir.json").write_text(json.dumps(ir))
        (output / "plan.json").write_text(json.dumps(plan))
        (output / "build-result.json").write_text(json.dumps(result))
        # Different formatting proves the payload identity follows release assembly's canonical JSON.
        (output / "build-inputs.json").write_text(
            json.dumps(inputs, indent=2 if index == 1 else None)
        )
        (output / "image/buildkit-metadata.json").write_text(
            json.dumps({"containerimage.digest": image_digest})
        )
        (output / "image/controller.oci.tar").write_bytes(archive)
        (output / "linux-binary/reakt-controller").write_bytes(binary)

    def qualify(self):
        with patch.object(PRODUCER.subprocess, "run", side_effect=self.run):
            return PRODUCER.qualify(
                self.source,
                self.profile,
                self.cli,
                self.output,
                "registry.example/reakt/controller",
            )


class ReproducibilityTests(unittest.TestCase):
    def fixture(self, **options):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        return Fixture(Path(temporary.name), **options)

    def test_two_independent_builds_emit_only_measured_payload_evidence(self):
        fixture = self.fixture()
        receipt = fixture.qualify()
        self.assertEqual(len(fixture.builds), 2)
        first_output = fixture.builds[0][fixture.builds[0].index("--output") + 1]
        second_output = fixture.builds[1][fixture.builds[1].index("--output") + 1]
        self.assertNotEqual(first_output, second_output)
        for command in fixture.builds:
            self.assertEqual(command[-2:], ["--cache", "disabled"])
        self.assertEqual(receipt["schemaVersion"], PRODUCER.SCHEMA)
        self.assertNotIn("releaseVerified", receipt)
        self.assertNotIn("passed", receipt)
        self.assertEqual(
            receipt["payloadFiles"]["build-inputs.json"], receipt["sourceInputsDigest"]
        )
        raw_pretty = PRODUCER.digest_file(fixture.output / "build-1/build-inputs.json")
        self.assertNotEqual(receipt["sourceInputsDigest"], raw_pretty)
        check = receipt["checks"]["reproducible-payload"]
        self.assertEqual(
            [assertion["checkId"] for assertion in check["assertions"]],
            ["independent-builds", "payload-digests-equal"],
        )
        self.assertEqual(check["evidenceFiles"], ["build-1.log", "build-2.log"])
        self.assertEqual(
            receipt["runnerSourceDigest"],
            PRODUCER.digest_file(fixture.output / receipt["runnerSourceFile"]),
        )

    def test_every_required_identity_mismatch_rejects_without_a_receipt(self):
        for mismatch in ["binary", "image", "archive", "ir", "compiled", "compiler", "inputs"]:
            with self.subTest(mismatch=mismatch):
                fixture = self.fixture(mismatch=mismatch)
                with self.assertRaises(PRODUCER.QualificationError):
                    fixture.qualify()
                self.assertEqual(len(fixture.builds), 2)
                self.assertFalse((fixture.output / "production-evidence.json").exists())

    def test_failed_second_build_never_emits_reproducibility_evidence(self):
        fixture = self.fixture(fail_second=True)
        with self.assertRaises(PRODUCER.QualificationError):
            fixture.qualify()
        self.assertEqual(len(fixture.builds), 2)
        self.assertTrue((fixture.output / "build-2.log").is_file())
        self.assertFalse((fixture.output / "production-evidence.json").exists())

    def test_oversized_build_transcript_never_emits_evidence(self):
        fixture = self.fixture()
        original = fixture.run

        def oversized(arguments, **options):
            result = original(arguments, **options)
            if str(arguments[0]) == str(fixture.cli.resolve()):
                options["stdout"].write("x" * (PRODUCER.MAX_EVIDENCE_BYTES + 1))
            return result

        with patch.object(PRODUCER.subprocess, "run", side_effect=oversized):
            with self.assertRaisesRegex(PRODUCER.QualificationError, "transcript is oversized"):
                PRODUCER.qualify(fixture.source, fixture.profile, fixture.cli,
                                 fixture.output, "registry.example/controller")
        self.assertEqual(len(fixture.builds), 1)
        self.assertFalse((fixture.output / "production-evidence.json").exists())

    def test_dirty_source_before_or_after_builds_is_rejected(self):
        for statuses, expected_builds in [([" M program.tsx\n"], 0), (["", "?? changed\n"], 2)]:
            with self.subTest(statuses=statuses):
                fixture = self.fixture(statuses=statuses)
                with self.assertRaises(PRODUCER.QualificationError):
                    fixture.qualify()
                self.assertEqual(len(fixture.builds), expected_builds)
                self.assertFalse((fixture.output / "production-evidence.json").exists())

    def test_source_head_change_after_builds_is_rejected(self):
        fixture = self.fixture(heads=["a" * 40, "b" * 40])
        with self.assertRaises(PRODUCER.QualificationError):
            fixture.qualify()
        self.assertEqual(len(fixture.builds), 2)
        self.assertFalse((fixture.output / "production-evidence.json").exists())


if __name__ == "__main__":
    unittest.main()
