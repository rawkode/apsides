"""Offline regressions for the live CNPG demo's polling and image assertions."""
import hashlib
import json
import unittest
from unittest.mock import Mock, patch

import run_cnpg_demo as demo


class DemoChecks(unittest.TestCase):
    def test_missing_capability_preserves_compiler_order(self):
        first = {"namespace": "control", "apiGroup": "", "resource": "secrets"}
        second = {"namespace": "workload", "apiGroup": "", "resource": "configmaps"}
        self.assertEqual(demo.missing_capability(
            {"capabilities": []}, {"capabilities": [first, second]},
        ), first)

    def runner(self):
        runner = object.__new__(demo.CnpgDemoRunner)
        runner.cluster = "owned-test"
        return runner

    def test_poll_waits_for_absent_resource(self):
        runner = self.runner()
        runner.k = Mock(side_effect=["", '{"metadata":{"uid":"created"}}'])
        with patch.object(demo.time, "sleep"):
            result = runner.wait_object("deployment", "api", "default", 10)
        self.assertEqual(result["metadata"]["uid"], "created")
        self.assertEqual(runner.k.call_count, 2)
        self.assertIn("--ignore-not-found", runner.k.call_args.args)

    def test_poll_times_out_on_absence(self):
        runner = self.runner()
        runner.k = Mock(return_value="")
        with self.assertRaises(demo.shared.QualificationError):
            runner.wait_object("deployment", "api", "default", 0)

    def test_image_identity_requires_verified_index_membership(self):
        runner = self.runner()
        child = "sha256:" + "a" * 64
        raw = json.dumps({"manifests": [{"digest": child}]})
        digest = "sha256:" + hashlib.sha256(raw.encode()).hexdigest()
        reference = "example.invalid/api@" + digest
        runner.command = Mock(return_value=raw)
        self.assertEqual(runner.selected_child_digest(reference, child, "owned-test-control-plane"), child)
        self.assertEqual(runner.selected_child_digest(reference, digest, "owned-test-control-plane"), digest)
        with self.assertRaises(demo.shared.QualificationError):
            runner.selected_child_digest(reference, "sha256:" + "b" * 64, "owned-test-control-plane")
        runner.command = Mock(return_value=raw + " ")
        with self.assertRaises(demo.shared.QualificationError):
            runner.selected_child_digest(reference, child, "owned-test-control-plane")


if __name__ == "__main__":
    unittest.main()
