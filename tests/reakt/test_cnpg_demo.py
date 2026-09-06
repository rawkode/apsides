"""Offline regressions for the live CNPG demo's polling and image assertions."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import run_cnpg_demo as demo


class DemoChecks(unittest.TestCase):
    @staticmethod
    def digest(character="a"):
        return "sha256:" + character * 64

    def candidates(self):
        return {name: self.digest() for name in demo.PAIR_DIGESTS}

    def prepared(self, creation, managed):
        return {
            "schemaVersion": demo.SCHEMA,
            "outcome": "rejection-prepared",
            "candidates": self.candidates(),
            "generation": "generation",
            "transition": "transition",
            "registryUid": "registry-uid",
            "registryResourceVersion": "10",
            "requestDigest": self.digest("b"),
            "policyRole": "initial",
            "serviceAccountName": "r-generation-2",
            "creationTargetsSource": "authorized-candidate-policy-projection",
            "creationTargets": creation,
            "managedTargets": managed,
        }

    def test_prepared_result_requires_rust_ordered_closed_targets(self):
        targets = [
            {"namespace": "control", "apiGroup": "", "resource": "serviceaccounts",
             "name": "r-generation-2"},
            {"namespace": "workload", "apiGroup": "apps", "resource": "deployments",
             "name": "api"},
        ]
        value = self.prepared(targets, targets)
        line = demo.PREFIX + json.dumps(value)
        self.assertEqual(demo.result_record(line, "rejection-prepared"), value)
        value["creationTargets"] = list(reversed(targets))
        with self.assertRaises(demo.shared.QualificationError):
            demo.result_record(demo.PREFIX + json.dumps(value), "rejection-prepared")

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

    @staticmethod
    def resource(api_version, kind, name, api_resource=None):
        value = {
            "apiVersion": api_version, "kind": kind, "name": name, "namespace": "workload",
        }
        if api_resource is not None:
            value["apiResource"] = api_resource
        return value

    def test_denied_targets_are_generation_creates_plus_signed_managed_delta(self):
        runner = self.runner()
        runner.control = "control"
        runner.workload = "workload"
        cluster = self.resource(
            "postgresql.cnpg.io/v1", "Cluster", "database",
            {"plural": "clusters", "scope": "Namespaced", "schemaSha256": self.digest()},
        )
        config = self.resource("v1", "ConfigMap", "api-config")
        deployment = self.resource("apps/v1", "Deployment", "database-api")
        service = self.resource("v1", "Service", "database-api")
        runner.initial_plan = {"ir": {"resources": [cluster, config]}}
        runner.successor_plan = {"ir": {"resources": [cluster, config, deployment, service]}}
        initial = sorted([demo.managed_target(cluster), demo.managed_target(config)], key=demo.target_order)
        successor = sorted(
            [*initial, demo.managed_target(deployment), demo.managed_target(service)],
            key=demo.target_order,
        )
        creates = sorted([
            {"namespace": "control", "apiGroup": "", "resource": "serviceaccounts",
             "name": "r-generation-2"},
            {"namespace": "control", "apiGroup": "apps", "resource": "deployments",
             "name": "r-generation-2"},
        ], key=demo.target_order)
        prepared = self.prepared(creates, successor)
        observed = runner.denied_targets(initial, prepared)
        self.assertEqual(
            {demo.target_key(target) for target in observed},
            {demo.target_key(target) for target in [*creates, demo.managed_target(deployment),
                                                     demo.managed_target(service)]},
        )

    def test_writer_identity_binds_deployment_replicaset_pod_and_container_uids(self):
        runner = self.runner()
        runner.control = "control"
        runner.workload = "workload"
        runner.initial_plan = {"ir": {"resources": [self.resource("v1", "ConfigMap", "config")]}}
        account = "r-generation-1"
        proxy = "proxy-generation-1"
        creation = sorted([
            {"namespace": "control", "apiGroup": "", "resource": "serviceaccounts", "name": account},
            {"namespace": "control", "apiGroup": "apps", "resource": "deployments", "name": account},
            {"namespace": "control", "apiGroup": "apps", "resource": "deployments", "name": proxy},
        ], key=demo.target_order)
        managed = [demo.managed_target(runner.initial_plan["ir"]["resources"][0])]
        prepared = self.prepared(creation, managed)
        prepared["serviceAccountName"] = account
        def deployment(name, uid):
            return {"metadata": {"name": name, "uid": uid}, "spec": {"replicas": 1},
                    "status": {"readyReplicas": 1}}
        deployments = [deployment(account, "deployment-a"), deployment(proxy, "deployment-b")]
        replicasets = [
            {"metadata": {"uid": "set-a", "ownerReferences": [
                {"kind": "Deployment", "uid": "deployment-a", "controller": True}]}},
            {"metadata": {"uid": "set-b", "ownerReferences": [
                {"kind": "Deployment", "uid": "deployment-b", "controller": True}]}},
        ]
        def pod(name, uid, owner, container):
            return {"metadata": {"name": name, "uid": uid, "ownerReferences": [
                        {"kind": "ReplicaSet", "uid": owner, "controller": True}]},
                    "spec": {"nodeName": "node"},
                    "status": {"phase": "Running", "conditions": [
                        {"type": "Ready", "status": "True"}], "containerStatuses": [
                        {"name": "writer", "ready": True, "containerID": container,
                         "restartCount": 0}]}}
        pods = [pod("pod-a", "pod-uid-a", "set-a", "containerd://a"),
                pod("pod-b", "pod-uid-b", "set-b", "containerd://b")]
        runner.k = Mock(side_effect=[json.dumps({"items": deployments}),
                                     json.dumps({"items": replicasets}),
                                     json.dumps({"items": pods})])
        identity = runner.writer_identity(prepared)
        self.assertEqual({item["uid"] for item in identity}, {"deployment-a", "deployment-b"})
        containers = {item["uid"]: item["pods"][0]["containers"] for item in identity}
        self.assertEqual(containers["deployment-a"], [{
            "name": "writer", "containerId": "containerd://a", "restartCount": 0,
        }])

    def test_retained_cluster_cannot_publish_production_evidence(self):
        runner = self.runner()
        runner.disposition = "retained"
        runner.cleanup_confirmed = False
        with tempfile.TemporaryDirectory() as directory:
            runner.output = Path(directory)
            with self.assertRaises(demo.shared.QualificationError):
                runner.publish()
            self.assertFalse((runner.output / "evidence").exists())

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
