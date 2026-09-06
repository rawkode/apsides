import importlib.util
import io
import json
from contextlib import redirect_stdout
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("run_preview_qualification.py")
SPEC = importlib.util.spec_from_file_location("run_preview_qualification", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("preview qualification wrapper could not be imported")
WRAPPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WRAPPER)

CLUSTER = "aps-preview-0123456789abcdef"
PASSING_TEST = """running 1 test
test preview_issued_live::issued_credentials_enforce_conditional_ssa_and_rbac ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out
"""


class PreviewQualificationTests(unittest.TestCase):
    def invoke(
        self,
        *,
        existing="",
        create="ok",
        test_output=PASSING_TEST,
        cleanup="ok",
    ):
        calls = []
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def execute(arguments, **options):
                command = [str(argument) for argument in arguments]
                calls.append(command)
                if command == ["kind", "get", "clusters"]:
                    return subprocess.CompletedProcess(command, 0, stdout=existing, stderr="")
                if command[:4] == ["kind", "create", "cluster", "--name"]:
                    Path(options["env"]["KUBECONFIG"]).write_text("disposable credential")
                    if create == "timeout":
                        raise subprocess.TimeoutExpired(command, options["timeout"])
                    return subprocess.CompletedProcess(command, 1 if create == "fail" else 0)
                if command == ["cuenv", "task", "previewIssuedLive"]:
                    options["stdout"].write(test_output)
                    return subprocess.CompletedProcess(command, 0)
                if command[:4] == ["kind", "delete", "cluster", "--name"]:
                    return subprocess.CompletedProcess(command, 1 if cleanup == "fail" else 0)
                raise AssertionError(f"unexpected subprocess: {command}")

            with (
                patch.object(WRAPPER, "ROOT", root),
                patch.object(WRAPPER.uuid, "uuid4", return_value=SimpleNamespace(hex="0123456789abcdef0123456789abcdef")),
                patch.object(WRAPPER.os, "umask"),
                patch.object(WRAPPER.subprocess, "run", side_effect=execute),
                redirect_stdout(io.StringIO()),
            ):
                status = WRAPPER.main()

            output = root / "work" / CLUSTER
            result = json.loads((output / "result.json").read_text())
            kubeconfig_exists = (output / "admin.kubeconfig").exists()
            return status, result, calls, kubeconfig_exists

    def test_create_failure_or_timeout_cleans_the_owned_cluster_and_credentials(self):
        for outcome in ["fail", "timeout"]:
            with self.subTest(outcome=outcome):
                status, result, calls, kubeconfig_exists = self.invoke(create=outcome)
                create = next(call for call in calls if call[:3] == ["kind", "create", "cluster"])
                delete = next(call for call in calls if call[:3] == ["kind", "delete", "cluster"])
                self.assertEqual(create[4], CLUSTER)
                self.assertEqual(delete[4], CLUSTER)
                self.assertEqual(status, 1)
                self.assertFalse(result["passed"])
                self.assertTrue(result["clusterRemoved"])
                self.assertFalse(kubeconfig_exists)

    def test_existing_cluster_name_is_never_created_or_deleted(self):
        status, result, calls, kubeconfig_exists = self.invoke(existing=f"{CLUSTER}\n")
        self.assertEqual(calls, [["kind", "get", "clusters"]])
        self.assertEqual(status, 1)
        self.assertFalse(result["passed"])
        self.assertNotIn("clusterRemoved", result)
        self.assertFalse(kubeconfig_exists)

    def test_successful_cargo_with_no_matching_test_is_rejected(self):
        status, result, calls, kubeconfig_exists = self.invoke(
            test_output="test result: ok. 0 passed; 0 failed; 1 filtered out\n"
        )
        self.assertEqual(status, 1)
        self.assertFalse(result["passed"])
        self.assertIn("expected live qualification test did not execute", result["error"])
        self.assertTrue(result["clusterRemoved"])
        self.assertFalse(kubeconfig_exists)
        self.assertTrue(any(call == ["cuenv", "task", "previewIssuedLive"] for call in calls))

    def test_exact_live_test_success_is_accepted_and_cleaned_up(self):
        status, result, calls, kubeconfig_exists = self.invoke()
        self.assertEqual(status, 0)
        self.assertTrue(result["passed"])
        self.assertTrue(result["clusterRemoved"])
        self.assertFalse(kubeconfig_exists)
        self.assertTrue(any(call[:3] == ["kind", "delete", "cluster"] for call in calls))

    def test_cleanup_failure_overrides_a_successful_live_test(self):
        status, result, _, kubeconfig_exists = self.invoke(cleanup="fail")
        self.assertEqual(status, 1)
        self.assertFalse(result["passed"])
        self.assertFalse(result["clusterRemoved"])
        self.assertIn("cleanup.log failed", result["cleanupError"])
        self.assertFalse(kubeconfig_exists)


if __name__ == "__main__":
    unittest.main()
