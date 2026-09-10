"""Protocol and admission checks for coordination PVC qualification."""
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from run_coordination_restart import CoordinationRunner, parse_coordination_event, parse_race_event


NONCE = "0123456789abcdef0123456789abcdef"


def output(record):
    return ("APS_COORDINATION_RESULT=" + json.dumps(record)
            + "\ntest pvc_coordination_phase ... ok\n"
            + "test result: ok. 1 passed; 0 failed; 0 ignored\n")


class CoordinationTests(unittest.TestCase):
    def test_barrier_follows_both_readiness_checks_and_reopen_must_match(self):
        def run(substitute=False):
            runner = CoordinationRunner.__new__(CoordinationRunner)
            runner.nonce = NONCE
            runner.events = []
            order = []
            runner.create_pod = lambda name, phase, **kw: order.append(("create", name, kw))
            runner.wait_race_ready = lambda phase: order.append(("ready", phase))
            runner.validate_volume = lambda phase: None
            runner.wait_pod = lambda name: name
            digests = {key: "sha256:" + char * 64 for key, char in zip(
                ["claimDigest", "leftReviewDigest", "rightReviewDigest", "winnerAuditDigest", "loserAuditDigest"],
                "abcde")}
            def parse(_output, _nonce, phase):
                record = dict(digests, phase=phase, outcome={"race-left": "published",
                              "race-right": "conflict", "verify-race": "verified"}[phase])
                if substitute and phase == "verify-race":
                    record["winnerAuditDigest"] = "sha256:" + "f" * 64
                return record
            with patch("run_coordination_restart.parse_race_event", side_effect=parse):
                runner.race()
            return order
        order = run()
        barrier = next(i for i, item in enumerate(order) if item[:2] == ("create", "race-barrier"))
        ready = [i for i, item in enumerate(order) if item[0] == "ready"]
        self.assertEqual(len(ready), 2)
        self.assertTrue(all(i < barrier for i in ready))
        self.assertTrue(next(item[2]["read_only"] for item in order
                             if item[:2] == ("create", "verify-race")))
        with self.assertRaisesRegex(RuntimeError, "post-race references differ"):
            run(substitute=True)

    def test_race_protocol_requires_distinct_candidates_and_exact_nonce(self):
        record = {"schemaVersion": "apsides.dev/pvc-coordination-race/v1", "nonce": NONCE,
                  "phase": "race-left", "outcome": "published", "claimDigest": "sha256:" + "a" * 64,
                  "leftReviewDigest": "sha256:" + "b" * 64, "rightReviewDigest": "sha256:" + "c" * 64,
                  "winnerAuditDigest": "sha256:" + "d" * 64, "loserAuditDigest": "sha256:" + "e" * 64}
        def encode(value):
            return ("APS_COORDINATION_RACE_RESULT=" + json.dumps(value)
                    + "\ntest pvc_coordination_phase ... ok\n"
                    + "test result: ok. 1 passed; 0 failed; 0 ignored\n")
        self.assertEqual(parse_race_event(encode(record), NONCE, "race-left"), record)
        for mutation in [{"nonce": "f" * 32}, {"outcome": "verified"},
                         {"loserAuditDigest": record["winnerAuditDigest"]}, {"extra": 1}]:
            with self.subTest(mutation=mutation), self.assertRaises(RuntimeError):
                parse_race_event(encode(dict(record, **mutation)), NONCE, "race-left")

    def test_exact_event_rejects_substitution_and_unexpected_fields(self):
        record = {"schemaVersion": "apsides.dev/pvc-coordination-probe/v1",
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
