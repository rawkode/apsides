import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_ownership_collision as collision

DIGEST_A = "sha256:" + "a" * 64
DIGEST_B = "sha256:" + "b" * 64


def pair_result():
    return {"schemaVersion": collision.SCHEMA, "outcome": "verified-pair",
            "ownerReleaseDigest": DIGEST_A, "ownerIrDigest": DIGEST_A,
            "contenderReleaseDigest": DIGEST_B, "contenderIrDigest": DIGEST_B,
            "ownerDomainId": "owner", "contenderDomainId": "contender",
            "clusterBinding": "cluster", "controlNamespace": "control", "workloadNamespace": "workload",
            "overlapIdentityDigest": DIGEST_A, "overlapCount": 1}


def marker_runner():
    runner = collision.CollisionRunner.__new__(collision.CollisionRunner)
    runner.cluster = "qualification"
    runner.pair = pair_result()
    runner.target = {"markerUid": "owner-uid", "markerResourceVersion": "42"}
    marker = {"apiVersion": "v1", "kind": "ConfigMap", "immutable": True,
              "metadata": {"name": collision.base.MARKER, "namespace": runner.cluster, "uid": "owner-uid", "resourceVersion": "42"},
              "data": {"state.json": json.dumps({"domainId": "owner", "releaseDigest": DIGEST_A,
                                                  "nodeUid": "node-uid", "workloadNamespaceUid": "workload-uid"})}}
    calls = []

    def command(*args, data=None):
        calls.append((args, data))
        if args[0] == "delete":
            return "{}"
        if "--ignore-not-found" in args:
            return ""
        return json.dumps(marker)

    def state(name, value, namespace, immutable=False):
        return {**marker, "immutable": immutable, "metadata": {**marker["metadata"], "uid": "contender-uid", "resourceVersion": "44"},
                "data": {"state.json": json.dumps(value)}}

    runner.k = command
    runner.state = state
    return runner, marker, calls


class CollisionContractTests(unittest.TestCase):
    def test_verified_pair_is_closed_and_distinct(self):
        value = pair_result()
        output = "APS_COLLISION_RESULT=" + json.dumps(value)
        self.assertEqual(collision.result_record(output, "verified-pair"), value)
        for key, replacement in [("overlapCount", 2), ("overlapCount", True), ("ownerReleaseDigest", DIGEST_B),
                                  ("ownerDomainId", "contender"), ("ownerIrDigest", "invalid"), ("secret", "synthetic-canary")]:
            with self.subTest(key=key, replacement=replacement):
                changed = {**value, key: replacement}
                with self.assertRaises(collision.shared.QualificationError):
                    collision.result_record("APS_COLLISION_RESULT=" + json.dumps(changed), "verified-pair")

    def test_step_release_must_match_its_pair_role(self):
        for outcome, wrong_release in [("owner-prepared", DIGEST_B), ("contender-prepared", DIGEST_A), ("collision-rejected", DIGEST_A)]:
            value = {key: "identity" for key in collision.FIELDS[outcome]}
            value.update({key: pair_result()[key] for key in collision.PAIR})
            value.update(schemaVersion=collision.SCHEMA, outcome=outcome, releaseDigest=wrong_release, requestDigest=DIGEST_A)
            if "creationTargets" in value:
                value["creationTargets"] = [{"namespace": "workload", "apiGroup": "", "resource": "configmaps", "name": "target"}]
            with self.subTest(outcome=outcome), self.assertRaisesRegex(collision.shared.QualificationError, "candidate role"):
                collision.result_record("APS_COLLISION_RESULT=" + json.dumps(value), outcome)

    def test_sources_include_subclass_and_shared_rust_closure(self):
        with tempfile.TemporaryDirectory(dir=ROOT / "work") as directory:
            args = SimpleNamespace(artifact=Path(directory) / "candidate", trust_policy=Path(directory) / "trust",
                                   owner_artifact=Path(directory) / "owner", owner_trust_policy=Path(directory) / "owner-trust", output=Path(directory) / "run")
            with mock.patch.object(collision.shared, "source_state", return_value=("repository", "commit")), mock.patch.object(collision.shared, "require_ignored"):
                runner = collision.CollisionRunner(args)
            self.assertTrue({"run_ownership_collision.py", "run_deployment_ceiling.py", "qualification_driver.rs", "ownership_collision_driver.rs"}.issubset(runner.source_digests))
            self.assertEqual(len(runner.source_digests), len(runner.sources))
            self.assertEqual(len(set(runner.archives.values())), len(runner.sources))
            self.assertTrue(all(name.startswith("ownership-collision-") and len(name) <= 128 for name in runner.archives.values()))
            self.assertTrue(set(runner.source_digests).isdisjoint(runner.archives.values()))

    def test_invalid_owner_inputs_do_not_create_output(self):
        with tempfile.TemporaryDirectory() as directory:
            args = SimpleNamespace(owner_artifact=Path("relative"), owner_trust_policy=Path(directory) / "trust", output=Path(directory) / "run")
            with self.assertRaises(collision.shared.QualificationError):
                collision.CollisionRunner(args)
            self.assertFalse(args.output.exists())


class MarkerTests(unittest.TestCase):
    def test_replacement_uses_exact_preconditions_and_preserves_host_binding(self):
        runner, _, calls = marker_runner()
        result = runner.rotate_marker()
        deletes = [(args, data) for args, data in calls if args[0] == "delete"]
        self.assertEqual(len(deletes), 1)
        self.assertEqual(json.loads(deletes[0][1])["preconditions"], {"uid": "owner-uid", "resourceVersion": "42"})
        self.assertIn("--raw", deletes[0][0])
        self.assertEqual(result["contender"]["uid"], "contender-uid")
        self.assertEqual(runner.target["markerResourceVersion"], "44")
        value = json.loads(runner.rotated_marker["data"]["state.json"])
        self.assertEqual(value["nodeUid"], "node-uid")
        self.assertEqual(value["workloadNamespaceUid"], "workload-uid")
        self.assertEqual(value["domainId"], "contender")
        self.assertEqual(value["releaseDigest"], DIGEST_B)
        self.assertNotIn("state.json", json.dumps(result))

    def test_changed_owner_marker_stops_before_deletion(self):
        for field in ["uid", "resourceVersion"]:
            runner, marker, calls = marker_runner()
            marker["metadata"][field] = "changed"
            with self.assertRaises(collision.shared.QualificationError):
                runner.rotate_marker()
            self.assertFalse(any(args[0] == "delete" for args, _ in calls))

    def test_existing_marker_after_delete_stops_before_creation(self):
        runner, _, _ = marker_runner()
        original = runner.k
        runner.k = lambda *args, **kwargs: "configmap/still-present" if "--ignore-not-found" in args else original(*args, **kwargs)
        runner.state = mock.Mock()
        with self.assertRaises(collision.shared.QualificationError):
            runner.rotate_marker()
        runner.state.assert_not_called()

    def test_replacement_with_same_uid_is_rejected(self):
        runner, marker, _ = marker_runner()
        runner.state = lambda *args, **kwargs: marker
        with self.assertRaises(collision.shared.QualificationError):
            runner.rotate_marker()


class AbsenceTests(unittest.TestCase):
    def test_owner_and_contender_projections_are_separate(self):
        runner = collision.CollisionRunner.__new__(collision.CollisionRunner)
        runner.owner_plan = {"role": "owner"}
        runner.contender_plan = {"role": "contender"}
        runner.plan = runner.owner_plan
        calls = []
        runner.assert_targets_absent = lambda prepared: calls.append((runner.plan, prepared))
        owner = {"creationTargetsSource": "capability-complete-projection", "creationTargets": ["owner-target"]}
        contender = {"creationTargetsSource": "claims-empty-capability-complete-projection", "creationTargets": ["contender-target"]}
        runner.assert_all_absent(owner)
        self.assertEqual(calls, [(runner.owner_plan, owner), (runner.contender_plan, None)])
        calls.clear()
        runner.assert_all_absent(owner, contender)
        self.assertEqual(calls[1][1]["creationTargets"], ["contender-target"])
        self.assertEqual(contender["creationTargetsSource"], "claims-empty-capability-complete-projection")
        self.assertIs(runner.plan, runner.owner_plan)

    def test_authoritative_contender_projection_is_rejected(self):
        runner = collision.CollisionRunner.__new__(collision.CollisionRunner)
        runner.owner_plan, runner.contender_plan = {}, {"contender": True}
        runner.plan = runner.owner_plan
        runner.assert_targets_absent = mock.Mock()
        prepared = {"creationTargetsSource": "capability-complete-projection", "creationTargets": []}
        with self.assertRaises(collision.shared.QualificationError):
            runner.assert_all_absent(prepared, prepared)


if __name__ == "__main__":
    unittest.main()
