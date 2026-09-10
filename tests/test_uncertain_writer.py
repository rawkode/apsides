"""Offline safety checks for lost predecessor Pod evidence qualification."""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock

import run_uncertain_writer as writer


class UncertainWriterChecks(unittest.TestCase):
    def test_only_static_require_messages_are_reportable(self):
        source = '''
require(False, "safe static reason")
require(False, dynamic_message)
require(False, f"private {value}")
other(False, "not an approved reason")
'''
        reasons = writer.static_failure_reasons(source)
        self.assertEqual(reasons, {"safe static reason"})
        runner = Mock(failure_reasons=reasons)
        self.assertEqual(writer.demo.base.failure_reason(runner, writer.shared.QualificationError("safe static reason")),
                         "safe static reason")
        self.assertEqual(writer.demo.base.failure_reason(runner, writer.shared.QualificationError("private secret")),
                         "QualificationError")

    def test_checkpoint_records_only_fixed_stage_and_survives_cleanup_phase(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = object.__new__(writer.UncertainWriterRunner)
            runner.output = Path(directory)
            runner.checkpoint("fault-removal")
            self.assertEqual(runner.phase, "uncertain-writer-fault-removal")
            runner.phase = "cleanup"
            self.assertEqual(json.loads((runner.output / "progress.json").read_bytes()), {"stage": "fault-removal"})
            with self.assertRaises(writer.shared.QualificationError):
                runner.checkpoint("arbitrary private value")
            self.assertEqual(json.loads((runner.output / "progress.json").read_bytes()), {"stage": "fault-removal"})

    def pod(self):
        return {"metadata": {"name": "runtime-pod", "namespace": "control", "uid": "pod-uid",
                             "resourceVersion": "42", "ownerReferences": [{"kind": "ReplicaSet", "controller": True, "uid": "rs-uid"}], "finalizers": [writer.FINALIZER, "other/hold"]},
                "spec": {"nodeName": "node", "serviceAccountName": "runtime-process"}, "status": {"phase": "Failed", "containerStatuses": [
                    {"name": "runtime", "state": {"terminated": {"exitCode": 0}}}]}}

    def expected(self):
        return {"name": "runtime-pod", "uid": "pod-uid", "nodeName": "node", "serviceAccountName": "runtime-process", "replicaSetUid": "rs-uid"}

    def test_fault_patch_pins_uid_and_revision_preserves_other_finalizers(self):
        patch = writer.removal_patch(self.pod(), self.expected(), "control")
        self.assertEqual(patch, [
            {"op": "test", "path": "/metadata/uid", "value": "pod-uid"},
            {"op": "test", "path": "/metadata/resourceVersion", "value": "42"},
            {"op": "replace", "path": "/metadata/finalizers", "value": ["other/hold"]}])

    def test_fault_rejects_replacement_live_or_unprotected_pod(self):
        for section, key, value in [("metadata", "uid", "replacement"), ("metadata", "namespace", "other"),
                ("metadata", "resourceVersion", ""), ("metadata", "ownerReferences", []), ("metadata", "finalizers", []),
                ("spec", "nodeName", "other"), ("spec", "serviceAccountName", "other"), ("status", "phase", "Running"),
                ("status", "containerStatuses", [{"state": {"running": {}}}])]:
            pod = self.pod()
            pod[section][key] = value
            with self.subTest(key=key), self.assertRaises(writer.shared.QualificationError):
                writer.removal_patch(pod, self.expected(), "control")

    def history(self):
        initial = {"name": "runtime", "uid": "deployment-uid", "processServiceAccountName": "runtime-process", "pods": [self.expected()]}
        registry = {"transitions": {"transition": {"record": {"phase": "Quiesced"}, "upgradeDrain": {
            "runtimeWriters": {"deploymentName": "runtime", "deploymentUid": "deployment-uid",
                               "serviceAccountName": "runtime-process", "pods": [self.expected()]}}}}}
        return registry, initial

    def test_history_requires_same_protected_pod_and_initial_allocation(self):
        registry, initial = self.history()
        self.assertEqual(writer.pinned_history(registry, "transition", initial), self.expected())
        for change in ["pod", "deployment", "phase", "base-account", "extra-pod"]:
            changed = copy.deepcopy(registry)
            stored = changed["transitions"]["transition"]
            if change == "pod":
                stored["upgradeDrain"]["runtimeWriters"]["pods"][0]["uid"] = "other"
            elif change == "deployment":
                stored["upgradeDrain"]["runtimeWriters"]["deploymentUid"] = "other"
            elif change == "phase":
                stored["record"]["phase"] = "Staged"
            elif change == "base-account":
                stored["upgradeDrain"]["runtimeWriters"]["serviceAccountName"] = "runtime"
            else:
                stored["upgradeDrain"]["runtimeWriters"]["pods"].append(self.expected())
            with self.subTest(change=change), self.assertRaises(writer.shared.QualificationError):
                writer.pinned_history(changed, "transition", initial)

    def test_closed_refusal_parser_requires_quiesced_expected_outcome(self):
        value = {"schemaVersion": writer.demo.SCHEMA, "outcome": writer.OUTCOME,
                 "candidates": {key: "sha256:" + "a" * 64 for key in writer.demo.PAIR_DIGESTS},
                 "transition": "transition", "generation": "generation", "phase": "Quiesced",
                 "registryUid": "registry", "registryResourceVersion": "42", "requestDigest": ""}
        line = lambda v: writer.demo.PREFIX + json.dumps(v)
        self.assertEqual(writer.result_record(line(value), writer.OUTCOME), value)
        for changed in [dict(value, phase="Activated"), dict(value, unexpected=True), dict(value, outcome="upgrade-activated")]:
            with self.assertRaises(writer.shared.QualificationError):
                writer.result_record(line(changed), writer.OUTCOME)
        with self.assertRaises(writer.shared.QualificationError):
            writer.result_record(line(value), "upgrade-quiesced")
        with self.assertRaises(writer.shared.QualificationError):
            writer.result_record(line(value) + "\n" + line(value), writer.OUTCOME)

    def successor_fixture(self):
        deployment = {"metadata": {"name": "successor", "uid": "deployment-uid"},
                      "spec": {"replicas": 0, "template": {"spec": {"serviceAccountName": "successor-process"}}}}
        proxy = {"deploymentName": "proxy", "deploymentUid": "proxy-deployment-uid",
                 "podName": "proxy-pod", "podUid": "proxy-pod-uid"}
        proxy_deployment = {"metadata": {"name": "proxy", "uid": "proxy-deployment-uid"}}
        pods = [{"metadata": {"name": "proxy-pod", "uid": "proxy-pod-uid"}, "spec": {"serviceAccountName": "successor"}}]
        return [deployment, pods, "successor", "deployment-uid", proxy, proxy_deployment]

    def test_successor_allows_only_recorded_staged_proxy(self):
        args = self.successor_fixture()
        result = writer.stopped_successor(*args)
        self.assertEqual(result["replicas"], 0)
        self.assertEqual(result["runtimePodUids"], [])
        self.assertEqual(result["proxyPodUid"], "proxy-pod-uid")
        for mutation in ["missing", "replacement", "extra", "renamed", "proxy-deployment", "runtime-deployment"]:
            args = self.successor_fixture()
            if mutation == "missing":
                args[1].clear()
            elif mutation == "replacement":
                args[1][0]["metadata"]["uid"] = "other"
            elif mutation == "extra":
                args[1].append({"metadata": {"name": "unknown", "uid": "unknown"}, "spec": {"serviceAccountName": "successor"}})
            elif mutation == "renamed":
                args[1][0]["metadata"]["name"] = "other"
            elif mutation == "proxy-deployment":
                args[5]["metadata"]["uid"] = "other"
            else:
                args[0]["metadata"]["uid"] = "other"
            with self.subTest(mutation=mutation), self.assertRaises(writer.shared.QualificationError):
                writer.stopped_successor(*args)

    def test_successor_zero_requires_no_runtime_pod_even_if_terminal(self):
        args = self.successor_fixture()
        args[1].append({"spec": {"serviceAccountName": "successor-process"}, "status": {"phase": "Failed"}})
        with self.assertRaises(writer.shared.QualificationError):
            writer.stopped_successor(*args)
        args = self.successor_fixture()
        args[0]["spec"]["replicas"] = 1
        with self.assertRaises(writer.shared.QualificationError):
            writer.stopped_successor(*args)
        args = self.successor_fixture()
        args[0]["spec"]["template"]["spec"]["serviceAccountName"] = "successor"
        with self.assertRaises(writer.shared.QualificationError):
            writer.stopped_successor(*args)

    def test_activation_must_remain_bound_to_draining_predecessor(self):
        binding = {"domainId": "domain", "generationId": "old", "serviceAccountName": "runtime"}
        registry = {"transitions": {"next": {"previousWriterCheckpoint": {"runtimeBinding": binding}}}}
        initial = {"generation": "old", "serviceAccountName": "runtime"}
        successor = {"generation": "new", "transition": "next"}
        activation = {"status": "draining", "binding": binding}
        writer.draining_activation(activation, registry, initial, successor, "domain")
        for changed in [dict(activation, status="active"), dict(activation, status="inactive"),
                        dict(activation, binding=dict(binding, generationId="new"))]:
            with self.assertRaises(writer.shared.QualificationError):
                writer.draining_activation(changed, registry, initial, successor, "domain")

    def test_protected_snapshot_rejects_absent_activation_or_inventory(self):
        for missing_index in range(3):
            runner = object.__new__(writer.UncertainWriterRunner)
            runner.control, runner.workload, runner.domain = "control", "workload", "domain"
            value = json.dumps({"data": {"state.json": "{}"}})
            runner.k = Mock(side_effect=[value] * missing_index + [""])
            with self.subTest(missing_index=missing_index), self.assertRaises(writer.shared.QualificationError):
                runner.protected_state()

    def test_fault_does_not_touch_a_cluster_with_changed_container_identity(self):
        runner = object.__new__(writer.UncertainWriterRunner)
        runner.cluster = "owned"
        runner.container_id = "pinned"
        runner.verify_docker = Mock()
        runner.command = Mock(return_value=json.dumps([{"Id": "replacement"}]))
        runner.get_pod = Mock()
        with self.assertRaises(writer.shared.QualificationError):
            runner.lose_pod_record(self.expected())
        runner.get_pod.assert_not_called()

    def test_cleanup_is_required_before_publication(self):
        runner = object.__new__(writer.UncertainWriterRunner)
        runner.disposition = "retained"
        runner.verify_docker = Mock()
        with self.assertRaises(writer.shared.QualificationError):
            runner.publish()
        runner.verify_docker.assert_not_called()

    def test_receipt_covers_only_uncertain_writer_and_binds_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = object.__new__(writer.UncertainWriterRunner)
            runner.output = root
            runner.final_check = Mock()
            runner.require_inputs_unchanged = Mock()
            digest = "sha256:" + "a" * 64
            source = Path(writer.__file__).resolve()
            runner.source_digests = {source: writer.shared.digest_file(source)}
            runner.source_archive = root / "source.tar"
            runner.source_archive.write_bytes(b"synthetic test source archive")
            (root / "cleanup.json").write_text('{"clusterDeleted":true}')
            runner.operator_bytes = b"synthetic test operator"
            runner.successor_manifest = {key: digest for key in ["irDigest", "compilerDigest", "binaryDigest", "image"]}
            runner.successor_manifest.update({"source": {"inputsDigest": digest}, "files": {
                name: digest for name in ["build-inputs.json", "controller.oci.tar", "apsides-controller"]}})
            runner.commit = "a" * 40
            runner.pair = {"candidates": {"successorReleaseDigest": digest}}
            runner.snapshot, runner.steps, runner.observation = {}, [], {}
            receipt = json.loads(runner.publish().read_bytes())
            self.assertEqual(set(receipt["checks"]), {"uncertain-writer-blocks"})
            self.assertEqual(receipt["runnerSourceDigest"], runner.source_digests[source])
            for filename in receipt["checks"]["uncertain-writer-blocks"]["evidenceFiles"]:
                self.assertTrue((root / filename).is_file())
            with self.assertRaises(FileExistsError):
                runner.publish()


if __name__ == "__main__":
    unittest.main()
