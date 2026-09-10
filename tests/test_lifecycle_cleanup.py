import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("run_lifecycle.py")
SPEC = importlib.util.spec_from_file_location("run_lifecycle", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("lifecycle runner could not be imported")
LIFECYCLE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LIFECYCLE)


def completed(arguments, returncode=0, stdout=""):
    return subprocess.CompletedProcess(arguments, returncode, stdout=stdout, stderr="")


def cleanup_runner(root, created=True):
    runner = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
    runner.output = root
    runner.cluster = "apsides-lifecycle-owned"
    runner.created = created
    runner.control = "control"
    runner.workload = "workload"
    runner.results = []
    runner.admin = root / "admin.kubeconfig"
    runner.issuer = root / "issuer.kubeconfig"
    for path in [runner.admin, runner.issuer, root / "old-generation.kubeconfig"]:
        path.write_text("synthetic credential\n")
    return runner


class LifecycleCleanupTests(unittest.TestCase):
    def test_name_collision_refuses_creation_without_create_or_delete(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory), created=False)
            commands = []

            def command(arguments, **_):
                commands.append(arguments)
                return completed(arguments, stdout=runner.cluster + "\n")

            runner.command = command
            with self.assertRaisesRegex(RuntimeError, "name already exists"):
                runner.create()
            self.assertFalse(runner.created)
            self.assertEqual(commands, [["kind", "get", "clusters"]])
            self.assertEqual(runner.delete_owned_cluster(), "not-owned")
            self.assertFalse(any(command[:3] == ["kind", "create", "cluster"] for command in commands))
            self.assertFalse(any(command[:3] == ["kind", "delete", "cluster"] for command in commands))

    def test_failed_creation_remains_unowned_and_is_never_deleted(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory), created=False)
            commands = []

            def command(arguments, **_):
                commands.append(arguments)
                if arguments[:3] == ["kind", "create", "cluster"]:
                    return completed(arguments, returncode=1)
                return completed(arguments)

            runner.command = command
            with self.assertRaisesRegex(RuntimeError, "creation failed; ownership not established"):
                runner.create()
            self.assertFalse(runner.created)
            self.assertEqual(runner.delete_owned_cluster(), "not-owned")
            self.assertFalse(any(command[:3] == ["kind", "delete", "cluster"] for command in commands))

    def test_successful_create_marks_ownership_before_followup_setup(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory), created=False)
            commands = []

            def command(arguments, **_):
                commands.append(arguments)
                return completed(arguments)

            def stop_after_create(_):
                self.assertTrue(runner.created)
                raise RuntimeError("stop after owned creation")

            runner.command = command
            runner.apply = stop_after_create
            with self.assertRaisesRegex(RuntimeError, "stop after owned creation"):
                runner.create()
            self.assertTrue(runner.created)
            self.assertTrue(any(command[:3] == ["kind", "create", "cluster"] for command in commands))

    def test_success_requires_delete_success_and_confirmed_absence(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory))
            commands = []
            runner.collect_diagnostics = lambda: True

            def command(arguments, **_):
                commands.append(arguments)
                if arguments == ["kind", "get", "clusters"]:
                    return completed(arguments, stdout="unrelated-cluster\n")
                return completed(arguments)

            runner.command = command
            runner.finish(True)

            result = json.loads((runner.output / "result.json").read_text())
            self.assertTrue(result["passed"])
            self.assertEqual(result["clusterDisposition"], "deleted")
            self.assertTrue(result["credentialsRemoved"])
            cleanup = json.loads((runner.output / "cluster-cleanup.json").read_text())
            self.assertEqual(cleanup, {"stage": "complete", "deleteExitCode": 0, "absenceConfirmed": True})
            self.assertEqual(commands[-2:], [
                ["kind", "delete", "cluster", "--name", runner.cluster, "--kubeconfig", runner.admin],
                ["kind", "get", "clusters"],
            ])
            self.assertTrue(all(not path.exists() for path in [
                runner.issuer,
                runner.admin,
                runner.output / "old-generation.kubeconfig",
            ]))

    def test_delete_error_remaining_cluster_or_inventory_error_reports_failure(self):
        for failure in ["delete", "present", "inventory"]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                runner = cleanup_runner(Path(directory))
                commands = []
                runner.collect_diagnostics = lambda: True

                def command(arguments, **_):
                    commands.append(arguments)
                    if arguments[:3] == ["kind", "delete", "cluster"]:
                        return completed(arguments, returncode=1 if failure == "delete" else 0)
                    if arguments == ["kind", "get", "clusters"]:
                        if failure == "inventory":
                            return completed(arguments, returncode=1)
                        return completed(arguments, stdout=runner.cluster + "\n" if failure == "present" else "")
                    raise AssertionError(f"unexpected command: {arguments}")

                runner.command = command
                with self.assertRaisesRegex(RuntimeError, "cleanup did not establish"):
                    runner.finish(True)
                result = json.loads((runner.output / "result.json").read_text())
                self.assertFalse(result["passed"])
                self.assertEqual(result["clusterDisposition"], "cleanup-failed")
                self.assertTrue(result["credentialsRemoved"])
                cleanup = json.loads((runner.output / "cluster-cleanup.json").read_text())
                self.assertEqual(cleanup["stage"], "delete" if failure == "delete" else "verify-absence")
                self.assertFalse(cleanup["absenceConfirmed"])
                self.assertTrue(any(item[:3] == ["kind", "delete", "cluster"] for item in commands))
                self.assertTrue(all(not path.exists() for path in [
                    runner.issuer,
                    runner.admin,
                    runner.output / "old-generation.kubeconfig",
                ]))

    def test_delete_timeout_preserves_closed_diagnostic_and_removes_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory))
            runner.collect_diagnostics = lambda: True

            def command(arguments, **_):
                raise subprocess.TimeoutExpired(arguments, 180, output="private output")

            runner.command = command
            with self.assertRaisesRegex(RuntimeError, "cleanup did not establish"):
                runner.finish(True)
            cleanup = json.loads((runner.output / "cluster-cleanup.json").read_text())
            self.assertEqual(cleanup, {"stage": "delete", "deleteExitCode": None,
                                       "absenceConfirmed": False, "timedOut": True})
            result = json.loads((runner.output / "result.json").read_text())
            self.assertFalse(result["passed"])
            self.assertTrue(result["credentialsRemoved"])

    def test_diagnostic_failure_does_not_suppress_owned_cluster_deletion(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory))
            commands = []

            def diagnostics():
                raise RuntimeError("synthetic diagnostic failure")

            def command(arguments, **_):
                commands.append(arguments)
                return completed(arguments)

            runner.collect_diagnostics = diagnostics
            runner.command = command
            runner.finish(True)

            result = json.loads((runner.output / "result.json").read_text())
            self.assertTrue(result["passed"])
            self.assertFalse(result["diagnosticsComplete"])
            self.assertEqual(result["clusterDisposition"], "deleted")
            self.assertEqual(commands, [
                ["kind", "delete", "cluster", "--name", runner.cluster, "--kubeconfig", runner.admin],
                ["kind", "get", "clusters"],
            ])

    def test_credential_removal_continues_after_one_unlink_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = cleanup_runner(Path(directory), created=False)
            runner.issuer.unlink()
            runner.issuer.mkdir()

            with self.assertRaisesRegex(RuntimeError, "cleanup did not establish"):
                runner.finish(False)

            result = json.loads((runner.output / "result.json").read_text())
            self.assertFalse(result["passed"])
            self.assertFalse(result["credentialsRemoved"])
            self.assertTrue(runner.issuer.is_dir())
            self.assertFalse(runner.admin.exists())
            self.assertFalse((runner.output / "old-generation.kubeconfig").exists())


if __name__ == "__main__":
    unittest.main()
