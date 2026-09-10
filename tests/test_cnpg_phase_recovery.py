"""Offline checks of phase checkpoint ownership, interruption, and output boundaries."""
import json
import signal
import subprocess
import sys
import unittest
from unittest.mock import Mock

import run_cnpg_phase_recovery as recovery


class PhaseRecoveryTests(unittest.TestCase):
    def fixture(self):
        prepared = {"transition": "transition", "generation": "generation", "registryUid": "registry"}
        stored = {"record": {"transitionId": "transition", "phase": "Staged", "executorId": "candidate-pair-qualification"},
                  "executorBinding": {"transitionId": "transition", "executorId": "candidate-pair-qualification", "serviceAccountUid": "sa", "domainId": "database-demo", "sequence": "2",
                                      "scope": {"registryUid": "registry", **{key: recovery.demo.EXPECTED_SCOPE[key] for key in ["clusterBinding", "controlNamespace", "workloadNamespace"]}}},
                  "inputs": {"allocation": {"transitionId": "transition", "generationId": "generation", "registryUid": "registry", "sequence": "2"}, "previous": {"allocation": {"generationId": "old"}}}}
        value = {"metadata": {"uid": "registry", "resourceVersion": "42"},
                 "data": {"state.json": json.dumps({"transitions": {"transition": stored}, "histories": {"database-demo": {"activeAllocation": {"generationId": "old"}}}})}}
        marker = {"registryUid": "registry", "registryResourceVersion": "42", "transition": "transition", "phase": "Staged"}
        return prepared, stored, value, marker

    def test_checkpoint_exact_api_binding(self):
        prepared, _, value, marker = self.fixture()
        result = recovery.binding(value, prepared, "Staged", marker)
        self.assertEqual(result["generation"], "generation")
        self.assertEqual(result["executorId"], "candidate-pair-qualification")
        self.assertNotIn("state.json", json.dumps(result))
        self.assertTrue(recovery.shared.valid_digest(result["executorBindingDigest"]))

    def test_rejects_generation_owner_and_phase_substitution(self):
        for path, key in [("record", "executorId"), ("record", "phase"),
                          ("executorBinding", "transitionId"), ("executorBinding", "serviceAccountUid")]:
            prepared, stored, value, marker = self.fixture()
            stored[path][key] = "" if key == "serviceAccountUid" else "other"
            value["data"]["state.json"] = json.dumps({"transitions": {"transition": stored}, "histories": {"database-demo": {"activeAllocation": {"generationId": "old"}}}})
            with self.subTest(key=key), self.assertRaises(recovery.shared.QualificationError):
                recovery.binding(value, prepared, "Staged", marker)
        prepared, stored, value, marker = self.fixture()
        stored["inputs"]["allocation"]["generationId"] = "other"
        value["data"]["state.json"] = json.dumps({"transitions": {"transition": stored}, "histories": {"database-demo": {"activeAllocation": {"generationId": "old"}}}})
        with self.assertRaises(recovery.shared.QualificationError):
            recovery.binding(value, prepared, "Staged", marker)

    def test_rejects_stale_or_open_checkpoint(self):
        prepared, _, value, marker = self.fixture()
        for changed in [{**marker, "registryResourceVersion": "41"}, {**marker, "secret": "private"}]:
            with self.assertRaises(recovery.shared.QualificationError):
                recovery.binding(value, prepared, "Staged", changed)

    def test_rejects_scope_and_active_allocation_changes(self):
        for alteration in ["scope", "active", "registry"]:
            prepared, stored, value, marker = self.fixture()
            state = json.loads(value["data"]["state.json"])
            if alteration == "scope":
                state["transitions"]["transition"]["executorBinding"]["scope"]["workloadNamespace"] = "other"
            elif alteration == "active":
                state["histories"]["database-demo"]["activeAllocation"] = {"generationId": "new"}
            else:
                state["transitions"]["transition"]["inputs"]["allocation"]["registryUid"] = "replacement"
            value["data"]["state.json"] = json.dumps(state)
            with self.subTest(alteration=alteration), self.assertRaises(recovery.shared.QualificationError):
                recovery.binding(value, prepared, "Staged", marker)

    def test_requires_owned_kill_and_reap(self):
        process = Mock(pid=123)
        process.poll.return_value = None
        process.wait.return_value = -signal.SIGKILL
        self.assertEqual(recovery.kill_and_reap(process), {"pid": 123, "signal": "SIGKILL", "returnCode": -9, "reaped": True})
        process.kill.assert_called_once_with()
        process.wait.assert_called_once_with(timeout=10)
        for code in [0, 1, -signal.SIGTERM]:
            process.wait.return_value = code
            with self.assertRaises(recovery.shared.QualificationError):
                recovery.kill_and_reap(process)
        process.poll.return_value = 0
        with self.assertRaises(recovery.shared.QualificationError):
            recovery.kill_and_reap(process)

    def test_stream_discards_unrelated_output(self):
        marker = recovery.PREFIX + '{"phase":"Staged"}'
        child = subprocess.Popen([sys.executable, "-c", "import time; print('private body', flush=True); print(" + repr(marker) + ", flush=True); time.sleep(5)"],
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        try:
            self.assertEqual(recovery.await_record(child, 2), (recovery.PREFIX, marker))
            recovery.kill_and_reap(child)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
            child.stdout.close()

    def test_all_six_phase_actions_are_exact(self):
        self.assertEqual(recovery.PHASES, {"Planned": "reserve-successor", "Staged": "stage-successor",
            "Quiesced": "quiesce-upgrade", "Activated": "activate-upgrade", "Cleaning": "activate-upgrade", "Committed": "commit-upgrade"})

    def test_diagnostics_do_not_admit_dynamic_values(self):
        self.assertEqual(recovery.static_reasons('require(False, "safe")\nrequire(False, f"private {x}")\nrequire(False, value)'), {"safe"})


if __name__ == "__main__":
    unittest.main()
