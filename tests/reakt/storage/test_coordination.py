"""Protocol and admission checks for coordination PVC qualification."""
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from run_coordination_restart import CoordinationRunner, parse_coordination_event


NONCE = "0123456789abcdef0123456789abcdef"


def output(record):
    return ("APS_COORDINATION_RESULT=" + json.dumps(record)
            + "\ntest pvc_coordination_phase ... ok\n"
            + "test result: ok. 1 passed; 0 failed; 0 ignored\n")


class CoordinationTests(unittest.TestCase):
    def test_exact_event_rejects_substitution_and_unexpected_fields(self):
        record = {"schemaVersion": "reakt.dev/pvc-coordination-probe/v1",
                  "nonce": NONCE, "phase": "verify", "outcome": "verified",
                  "claimDigest": "sha256:" + "a" * 64,
                  "reviewDigest": "sha256:" + "b" * 64,
                  "auditDigest": "sha256:" + "c" * 64,
                  "claimCount": 1, "reviewResourceCount": 0, "auditEventCount": 5}
        self.assertEqual(parse_coordination_event(output(record), NONCE, "verify"), record)
        seed = dict(record, phase="seed", outcome="published")
        self.assertEqual(parse_coordination_event(output(seed), NONCE, "seed"), seed)
        with self.assertRaises(RuntimeError):
            parse_coordination_event(output(dict(seed, outcome="verified")), NONCE, "seed")
        for mutation in [{"nonce": "f" * 32}, {"phase": "seed"},
                         {"extra": "payload"}, {"claimCount": True},
                         {"auditDigest": "invalid"}]:
            with self.subTest(mutation=mutation), self.assertRaises(RuntimeError):
                parse_coordination_event(output(dict(record, **mutation)), NONCE, "verify")
        with self.assertRaises(RuntimeError):
            parse_coordination_event(output(record) * 2, NONCE, "verify")

    def test_read_only_pod_uses_coordination_target_and_path(self):
        runner = CoordinationRunner.__new__(CoordinationRunner)
        runner.image = {"reference": "localhost/test@sha256:" + "a" * 64}
        runner.node = "owned-control-plane"
        runner.nonce = NONCE
        pod = runner.pod_object("verify", "verify", read_only=True)
        container = pod["spec"]["containers"][0]
        self.assertEqual(container["args"][1], "pvc_coordination_phase")
        env = {item["name"]: item["value"] for item in container["env"]}
        self.assertEqual(env["APS_COORDINATION_TEST_PATH"], "/store/coordination.db")
        self.assertTrue(container["volumeMounts"][0]["readOnly"])
        self.assertFalse(pod["spec"]["automountServiceAccountToken"])
        with self.assertRaises(RuntimeError):
            runner.pod_object("crash", "verify", crash=True)


if __name__ == "__main__":
    unittest.main()
