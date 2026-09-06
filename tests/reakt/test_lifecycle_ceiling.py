import copy
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


def plan(capabilities, gaps, risks):
    return {
        "ir": {
            "domain": {
                "policyRef": "platform/team-api",
                "id": "team-api",
                "clusterBinding": "reakt-disposable-fixture",
                "namespace": "workload",
                "controlNamespace": "control",
            },
        },
        "capabilities": capabilities,
        "enforcementGaps": gaps,
        "indirectRisks": risks,
    }


def protected_snapshot():
    return {
        "registry": {"uid": "registry-uid", "resourceVersion": "41", "data": {"state.json": "fixed"}},
        "claims": [{"identity": {"name": "retained"}, "uid": "resource-uid"}],
        "objects": {("workload", "configmaps"): ["retained resource-uid"]},
        "permissions": {
            ("workload", "roles"): [{
                "name": "runtime",
                "uid": "role-uid",
                "resourceVersion": "8",
                "rules": [{"apiGroups": [""], "resources": ["configmaps"], "verbs": ["get"]}],
            }],
        },
    }


class LifecycleCeilingTests(unittest.TestCase):
    def test_runtime_ceiling_uses_only_the_selected_plan(self):
        first_capability = {"namespace": "workload", "resource": "configmaps", "verb": "get"}
        second_capability = {"namespace": "workload", "resource": "services", "verb": "create"}
        first = plan([first_capability], ["gap-z", "gap-a"], ["risk-z", "risk-a"])
        second = plan([first_capability, second_capability], ["gap-b"], ["risk-b"])

        first_ceiling = LIFECYCLE.runtime_ceiling(first)
        second_ceiling = LIFECYCLE.runtime_ceiling(second)
        self.assertEqual(first_ceiling["capabilities"], [first_capability])
        self.assertNotIn(second_capability, first_ceiling["capabilities"])
        self.assertEqual(first_ceiling["acceptedEnforcementGaps"], ["gap-a", "gap-z"])
        self.assertEqual(first_ceiling["acceptedIndirectRisks"], ["risk-a", "risk-z"])
        self.assertEqual(second_ceiling["capabilities"], [first_capability, second_capability])

    def test_handoff_ceiling_deterministically_retains_both_generations(self):
        retained_capability = {"namespace": "workload", "resource": "configmaps", "verb": "get"}
        successor_capability = {"namespace": "workload", "resource": "services", "verb": "create"}
        first = plan([retained_capability], ["gap-z", "gap-shared"], ["risk-z"])
        second = plan([successor_capability], ["gap-a", "gap-shared"], ["risk-a"])

        ceiling = LIFECYCLE.handoff_ceiling([first, second])

        self.assertEqual(ceiling["capabilities"], [retained_capability, successor_capability])
        self.assertEqual(ceiling["acceptedEnforcementGaps"], ["gap-a", "gap-shared", "gap-z"])
        self.assertEqual(ceiling["acceptedIndirectRisks"], ["risk-a", "risk-z"])
        self.assertEqual(ceiling, LIFECYCLE.handoff_ceiling([first, second]))
        self.assertNotIn(successor_capability, LIFECYCLE.runtime_ceiling(first)["capabilities"])

    def test_unchanged_unapproved_upgrade_records_typed_preflight_denial(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
            runner.output = Path(directory)
            runner.candidates = [Path("first"), Path("second")]
            runner.results = []
            snapshots = iter([protected_snapshot(), protected_snapshot()])
            events = []

            def snapshot():
                events.append("snapshot")
                return next(snapshots)

            runner.protected_snapshot = snapshot
            runner.allocation = lambda candidate, alias: events.append(("allocation", candidate, alias))
            calls = []

            def reject(candidate, transition, action, **options):
                events.append("denial")
                calls.append((candidate, transition, action, options))

            runner.step = reject
            runner.reject_unapproved_upgrade()

            self.assertEqual(events, [
                ("allocation", runner.candidates[1], "upgrade"),
                "snapshot",
                "denial",
                "snapshot",
            ])
            self.assertEqual(calls, [(runner.candidates[1], "upgrade", "reserve", {"expected_kind": "permission-ceiling"})])
            self.assertEqual(runner.results, [{
                "check": "unapproved-successor-rejected-before-reservation",
                "passed": True,
                "rejectionKind": "permission-ceiling",
                "protectedRegistryUnchanged": True,
                "claimsUnchanged": True,
                "objectIdentitiesUnchanged": True,
                "permissionsUnchanged": True,
            }])
            self.assertEqual(json.loads((runner.output / "steps.json").read_text()), runner.results)

    def test_unapproved_upgrade_rejects_each_protected_state_change(self):
        mutations = {
            "registry": lambda value: value["registry"].update(resourceVersion="42"),
            "claims": lambda value: value["claims"].append({"identity": {"name": "new"}, "uid": "new-uid"}),
            "objects": lambda value: value["objects"][("workload", "configmaps")].append("new new-uid"),
            "permissions": lambda value: value["permissions"][("workload", "roles")][0]["rules"][0]["verbs"].append("update"),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                before = protected_snapshot()
                after = copy.deepcopy(before)
                mutate(after)
                runner = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
                runner.output = Path(directory)
                runner.candidates = [Path("first"), Path("second")]
                runner.results = []
                snapshots = iter([before, after])
                events = []

                def snapshot():
                    events.append("snapshot")
                    return next(snapshots)

                runner.protected_snapshot = snapshot
                runner.allocation = lambda candidate, alias: events.append(("allocation", candidate, alias))
                calls = []

                def reject(candidate, transition, action, **options):
                    events.append("denial")
                    calls.append((candidate, transition, action, options))

                runner.step = reject

                with self.assertRaisesRegex(RuntimeError, "changed protected state"):
                    runner.reject_unapproved_upgrade()
                self.assertEqual(events, [
                    ("allocation", runner.candidates[1], "upgrade"),
                    "snapshot",
                    "denial",
                    "snapshot",
                ])
                self.assertEqual(calls, [(
                    runner.candidates[1],
                    "upgrade",
                    "reserve",
                    {"expected_kind": "permission-ceiling"},
                )])
                self.assertEqual(runner.results, [])
                self.assertFalse((runner.output / "steps.json").exists())

    def test_runtime_approval_preserves_the_independent_cleanup_ceiling(self):
        runner = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
        runner.domain = "team-api"
        runner.runtime_ceilings = [{"capabilities": ["first"]}, {"capabilities": ["second"]}]
        runner.cleanup_ceiling = {"capabilities": ["cleanup"], "acceptedEnforcementGaps": ["cleanup-gap"]}
        writes = []
        runner.state = lambda name, value: writes.append((name, copy.deepcopy(value)))

        runner.approve_runtime(1)

        self.assertEqual(writes, [("reakt-policy-team-api", {
            "schemaVersion": "reakt.dev/platform/v1",
            "runtimeCeiling": runner.runtime_ceilings[1],
            "cleanupCeiling": runner.cleanup_ceiling,
        })])
        self.assertIsNot(writes[0][1]["runtimeCeiling"], writes[0][1]["cleanupCeiling"])

    def test_expected_kind_requires_one_exact_nonpending_failed_record(self):
        cases = {
            "exact": (1, [{"pending": False, "kind": "permission-ceiling"}], True),
            "substring": (1, [{"pending": False, "kind": "permission-ceiling-other"}], False),
            "pending": (1, [{"pending": True, "kind": "permission-ceiling"}], False),
            "success": (0, [{"pending": False, "kind": "permission-ceiling"}], False),
            "duplicate": (1, [
                {"pending": False, "kind": "permission-ceiling"},
                {"pending": False, "kind": "permission-ceiling"},
            ], False),
        }
        for name, (returncode, records, accepted) in cases.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                runner = LIFECYCLE.LifecycleRunner.__new__(LIFECYCLE.LifecycleRunner)
                runner.output = Path(directory)
                runner.results = []
                runner.allocations = {}
                runner.target = {"markerUid": "marker"}
                runner.adopted_uid = None
                runner.issuer = Path(directory) / "issuer.kubeconfig"
                runner.allocation = lambda *_: {"transition": "canonical-transition"}
                runner.driver_environment = lambda *_: {}
                stdout = "\n".join("REAKT_TEST_ERROR=" + json.dumps(record) for record in records)
                runner.cargo = lambda *_, **__: subprocess.CompletedProcess([], returncode, stdout=stdout, stderr="")

                if accepted:
                    runner.step(Path("candidate"), "upgrade", "reserve", expected_kind="permission-ceiling")
                else:
                    with self.assertRaisesRegex(RuntimeError, "required typed rejection"):
                        runner.step(Path("candidate"), "upgrade", "reserve", expected_kind="permission-ceiling")


if __name__ == "__main__":
    unittest.main()
