"""Bounded protocol and cleanup tests for the live PVC qualification wrapper."""
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


SCRIPT = Path(__file__).resolve().parents[1] / "run_storage_restart.py"
SPEC = importlib.util.spec_from_file_location("run_storage_restart", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("storage qualification wrapper could not be imported")
WRAPPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WRAPPER)

NONCE = "0123456789abcdef0123456789abcdef"
DIGEST_A = "sha256:" + "a" * 64
DIGEST_B = "sha256:" + "b" * 64


def event(phase, outcome, revision="2", snapshot=DIGEST_B, journal=None):
    return {
        "nonce": NONCE, "schemaVersion": "apsides.dev/pvc-inventory-probe/v1",
        "phase": phase, "outcome": outcome, "revision": revision,
        "snapshotId": snapshot, "firstSnapshotId": DIGEST_A,
        "secondSnapshotId": DIGEST_B, "largeSnapshotBytes": 2 * 1024 * 1024,
        "journalBytes": journal,
    }


def line(prefix, value):
    return prefix + json.dumps(value, separators=(",", ":")) + "\n"


def completed(arguments, returncode=0, stdout=""):
    return subprocess.CompletedProcess(arguments, returncode, stdout=stdout, stderr="")


class StorageRestartTests(unittest.TestCase):
    def test_storage_contract_rejects_mount_redirection_and_extra_sources(self):
        pod = {"spec": {"volumes": [{"name": "store", "persistentVolumeClaim": {
            "claimName": "fixed"}}], "containers": [{"volumeMounts": [
                {"name": "store", "mountPath": "/store"}]}]}}
        for field in ["subPath", "subPathExpr", "mountPropagation"]:
            mutated = json.loads(json.dumps(pod))
            mutated["spec"]["containers"][0]["volumeMounts"][0][field] = "redirect"
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                WRAPPER.pod_storage_contract(mutated)
        pod["spec"]["volumes"][0]["hostPath"] = {"path": "/alternate"}
        with self.assertRaises(RuntimeError):
            WRAPPER.pod_storage_contract(pod)

    def test_event_parser_is_closed_nonce_bound_and_private_safe(self):
        value = event("seed", "published")
        parsed = WRAPPER.parse_event(line("APS_STORAGE_RESULT=", value),
                                     "APS_STORAGE_RESULT=", NONCE, "seed", {"published"})
        self.assertEqual(parsed, value)
        crash = event("crash-ready", "uncommitted", "3", journal=4096)
        self.assertEqual(WRAPPER.parse_event(line("APS_STORAGE_CRASH_READY=", crash),
                                             "APS_STORAGE_CRASH_READY=", NONCE,
                                             "crash-ready", {"uncommitted"}), crash)
        malformed = dict(value, extra="field")
        wrong_nonce = dict(value, nonce="f" * 32)
        no_journal = dict(crash, journalBytes=None)
        for output, prefix, phase, outcomes in [
            (line("APS_STORAGE_RESULT=", malformed), "APS_STORAGE_RESULT=", "seed", {"published"}),
            (line("APS_STORAGE_RESULT=", wrong_nonce), "APS_STORAGE_RESULT=", "seed", {"published"}),
            (line("APS_STORAGE_CRASH_READY=", no_journal), "APS_STORAGE_CRASH_READY=",
             "crash-ready", {"uncommitted"}),
            (line("APS_STORAGE_RESULT=", value) * 2, "APS_STORAGE_RESULT=", "seed", {"published"}),
            (line("APS_STORAGE_RESULT=", value) + "private-desired-canary\n",
             "APS_STORAGE_RESULT=", "seed", {"published"}),
        ]:
            with self.subTest(output=output[:40]), self.assertRaises(RuntimeError):
                WRAPPER.parse_event(output, prefix, NONCE, phase, outcomes)

    def test_build_receipt_and_archive_are_digest_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            image = root / "image"
            image.mkdir()
            archive = image / "inventory-pvc.oci.tar"
            archive.write_bytes(b"test OCI archive")
            archive_digest = WRAPPER.file_digest(archive)
            receipt = {
                "schemaVersion": "apsides.dev/inventory-pvc-test-image/v1",
                "builder": {"reference": WRAPPER.BUILDER_IMAGE, "platform": "linux/arm64",
                            "localImageId": DIGEST_A}, "platform": "linux/arm64",
                "target": {"package": "inventory", "test": "pvc_restart"},
                "contextDigest": DIGEST_A, "imageDigest": DIGEST_B,
                "platformManifestDigest": DIGEST_B, "configDigest": DIGEST_A,
                "archive": {"path": "inventory-pvc.oci.tar", "sha256": archive_digest},
                "qualification": "test-only; not a production release artifact",
            }
            (image / "build.json").write_text(json.dumps(receipt))
            runner = WRAPPER.StorageRunner(SimpleNamespace(output=root / "out",
                                                            image_directory=image))
            self.assertEqual(runner.image["reference"],
                             "localhost/apsides-inventory-pvc@" + DIGEST_B)
            receipt["archive"]["sha256"] = DIGEST_A
            (image / "build.json").write_text(json.dumps(receipt))
            with self.assertRaisesRegex(RuntimeError, "archive digest differs"):
                WRAPPER.StorageRunner(SimpleNamespace(output=root / "other",
                                                       image_directory=image))

    def test_pod_contract_is_node_pinned_tokenless_and_read_only_when_requested(self):
        runner = WRAPPER.StorageRunner.__new__(WRAPPER.StorageRunner)
        runner.image = {"reference": "localhost/probe@" + DIGEST_A,
                        "platformManifestDigest": DIGEST_A}
        runner.node = "owned-control-plane"
        runner.nonce = NONCE
        runner.expected_pods = {}
        runner.mounts = []
        runner.pod_runs = []
        runner.runtime_image_id = None
        pod = runner.pod_object("readonly", "unavailable", read_only=True)
        spec = pod["spec"]
        container = spec["containers"][0]
        self.assertFalse(spec["automountServiceAccountToken"])
        self.assertEqual(spec["nodeName"], runner.node)
        self.assertEqual(container["imagePullPolicy"], "Never")
        self.assertEqual(container["args"], WRAPPER.PROBE_TEST)
        self.assertTrue(container["volumeMounts"][0]["readOnly"])
        self.assertTrue(spec["volumes"][0]["persistentVolumeClaim"]["readOnly"])
        self.assertEqual(container["securityContext"]["capabilities"], {"drop": ["ALL"]})
        crash = runner.pod_object("crash", "crash-ready", crash=True)
        self.assertEqual(crash["spec"]["containers"][0]["command"],
                         ["/usr/local/bin/storage-crash"])
        pod["metadata"]["uid"] = "pod-uid"
        runner.expected_pods["readonly"] = {
            "uid": "pod-uid", "command": None,
            "args": WRAPPER.PROBE_TEST,
            "env": {item["name"]: item["value"] for item in container["env"]},
            "storage": WRAPPER.pod_storage_contract(pod),
        }
        pod["status"] = {"containerStatuses": [{
            "name": "probe", "containerID": "containerd://runtime-id",
            "imageID": "docker-pullable://probe@" + DIGEST_A,
            "state": {"terminated": {"exitCode": 0, "signal": 0,
                                      "startedAt": "2026-01-01T00:00:00Z",
                                      "finishedAt": "2026-01-01T00:00:01Z"}},
        }]}
        runner.validate_pod(pod, "readonly", True)
        mutations = []
        extra = json.loads(json.dumps(pod))
        extra["spec"]["volumes"].append({"name": "extra", "persistentVolumeClaim": {
            "claimName": "extra", "readOnly": True}})
        extra["spec"]["containers"][0]["volumeMounts"].append({
            "name": "extra", "mountPath": "/extra", "readOnly": True})
        mutations.append(extra)
        wrong_claim = json.loads(json.dumps(pod))
        wrong_claim["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"] = "wrong"
        mutations.append(wrong_claim)
        wrong_mount = json.loads(json.dumps(pod))
        wrong_mount["spec"]["containers"][0]["volumeMounts"][0]["mountPath"] = "/wrong"
        mutations.append(wrong_mount)
        wrong_read_only = json.loads(json.dumps(pod))
        wrong_read_only["spec"]["containers"][0]["volumeMounts"][0]["readOnly"] = False
        mutations.append(wrong_read_only)
        for mutated in mutations:
            with self.subTest(storage=mutated["spec"]), self.assertRaisesRegex(
                    RuntimeError, "admitted contract changed"):
                runner.validate_pod(mutated, "readonly", False)

    def test_create_pod_captures_exact_two_volume_contract(self):
        runner = WRAPPER.StorageRunner.__new__(WRAPPER.StorageRunner)
        runner.image = {"reference": "localhost/probe@" + DIGEST_A,
                        "platformManifestDigest": DIGEST_A}
        runner.node = "owned-control-plane"
        runner.nonce = NONCE
        runner.expected_pods = {}
        original = runner.pod_object

        def two_volume_pod(name, phase, **options):
            value = original(name, phase, **options)
            value["spec"]["volumes"].append({
                "name": "backup", "persistentVolumeClaim": {"claimName": "backup-pvc"}})
            value["spec"]["containers"][0]["volumeMounts"].append({
                "name": "backup", "mountPath": "/backup"})
            return value

        runner.pod_object = two_volume_pod
        runner.k = lambda *_args, data=None, **_kwargs: completed(
            [], stdout=json.dumps(dict(json.loads(data), metadata={
                "name": "backup", "namespace": WRAPPER.NAMESPACE, "uid": "pod-uid"})))
        created = runner.create_pod("backup", "verify")
        expected = {
            "volumes": [
                {"name": "backup", "claimName": "backup-pvc", "readOnly": False},
                {"name": "store", "claimName": WRAPPER.PVC_NAME, "readOnly": False},
            ],
            "mounts": [
                {"name": "backup", "mountPath": "/backup", "readOnly": False},
                {"name": "store", "mountPath": WRAPPER.MOUNT_PATH, "readOnly": False},
            ],
        }
        self.assertEqual(runner.expected_pods["backup"]["storage"], expected)
        self.assertEqual(WRAPPER.pod_storage_contract(created), expected)

    def test_pv_omitted_empty_storage_class_is_canonical_and_diagnostics_name_only_fields(self):
        runner = WRAPPER.StorageRunner.__new__(WRAPPER.StorageRunner)
        runner.node = "owned-control-plane"
        runner.pvc_identity = {"uid": "pvc-uid"}
        runner.pv_identity = {"uid": "pv-uid"}
        runner.node_identity = {"uid": "node-uid"}
        runner.volume_checks = []
        pvc = {
            "metadata": {"uid": "pvc-uid"},
            "spec": {"volumeName": WRAPPER.PV_NAME, "accessModes": ["ReadWriteOnce"],
                     "storageClassName": ""},
            "status": {"phase": "Bound"},
        }
        pv = {
            "metadata": {"uid": "pv-uid"},
            "spec": {"claimRef": {"uid": "pvc-uid"},
                     "local": {"path": WRAPPER.LOCAL_PATH},
                     "accessModes": ["ReadWriteOnce"],
                     "persistentVolumeReclaimPolicy": "Retain",
                     "nodeAffinity": {"required": {"nodeSelectorTerms": [{
                         "matchExpressions": [{"key": "kubernetes.io/hostname",
                                               "operator": "In", "values": [runner.node]}],
                     }]}},
            },
        }
        node = {"metadata": {"uid": "node-uid",
                             "labels": {"kubernetes.io/hostname": runner.node}}}
        values = {"pvc": pvc, "pv": pv, "node": node}
        runner.get = lambda kind, *_: values[kind]
        runner.validate_volume()
        self.assertEqual(len(runner.volume_checks), 1)
        pv["spec"]["storageClassName"] = "private-value-must-not-appear"
        with self.assertRaisesRegex(RuntimeError, r"^PVC/PV/node validation failed: pv-storage-class$") as failure:
            runner.validate_volume()
        self.assertNotIn("private-value", str(failure.exception))

    def test_race_opens_barrier_only_after_both_ready_and_requires_exact_split(self):
        runner = WRAPPER.StorageRunner.__new__(WRAPPER.StorageRunner)
        runner.nonce = NONCE
        runner.events = []
        runner.fixture_identity = None
        runner.winner_snapshot_id = None
        order = []
        outputs = {
            "race-left": line("APS_STORAGE_RESULT=",
                              event("race-left", "published", "3", DIGEST_A))
                         + "test pvc_inventory_phase ... ok\n"
                         + "test result: ok. 1 passed; 0 failed; 0 ignored\n",
            "race-right": line("APS_STORAGE_RESULT=",
                               event("race-right", "conflict", "3", DIGEST_A))
                          + "test pvc_inventory_phase ... ok\n"
                          + "test result: ok. 1 passed; 0 failed; 0 ignored\n",
            "race-barrier": "",
        }
        runner.validate_volume = lambda *args, **kwargs: order.append(("validate", args))

        def create(name, phase, **options):
            order.append(("create", name, phase, options))

        def ready(name, phase):
            order.append(("ready", name, phase))
            return event(phase, "ready")

        runner.create_pod = create
        runner.wait_ready = ready
        runner.wait_pod = lambda name: outputs[name]
        runner.race()
        barrier_index = next(index for index, item in enumerate(order)
                             if item[:2] == ("create", "race-barrier"))
        ready_indices = [index for index, item in enumerate(order) if item[0] == "ready"]
        self.assertEqual(len(ready_indices), 2)
        self.assertTrue(all(index < barrier_index for index in ready_indices))
        self.assertEqual(sorted(item["outcome"] for item in runner.events),
                         ["conflict", "published"])

    def test_collision_never_deletes_and_owned_cleanup_uses_isolated_kubeconfig(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = WRAPPER.StorageRunner.__new__(WRAPPER.StorageRunner)
            runner.output = root
            runner.cluster = "aps-storage-owned"
            runner.node = runner.cluster + "-control-plane"
            runner.kubeconfig = root / "admin.kubeconfig"
            runner.kubeconfig.write_text("disposable credential")
            runner.created = False
            runner.image = {"reference": "localhost/probe@" + DIGEST_A,
                            "digest": DIGEST_A, "platformManifestDigest": DIGEST_A,
                            "configDigest": DIGEST_B, "archiveSha256": DIGEST_B}
            runner.events = []
            runner.mounts = []
            runner.pvc_identity = None
            runner.pv_identity = None
            runner.node_identity = None
            runner.crash_evidence = None
            runner.fixture_identity = None
            runner.winner_snapshot_id = None
            runner.pod_runs = []
            runner.runtime_image_id = None
            runner.volume_checks = []
            runner.cluster_inventory = lambda: {runner.cluster}
            calls = []
            runner.command = lambda arguments, **kwargs: (calls.append(arguments)
                                                           or completed(arguments))
            with self.assertRaisesRegex(RuntimeError, "already exists"):
                runner.create_cluster()
            self.assertFalse(runner.created)
            self.assertFalse(any(call[:3] == ["kind", "delete", "cluster"] for call in calls))

            runner.created = True
            inventories = iter([set()])
            runner.cluster_inventory = lambda: next(inventories)
            status = runner.finish(True)
            self.assertEqual(status, 0)
            delete = next(call for call in calls if call[:3] == ["kind", "delete", "cluster"])
            self.assertEqual(delete[-2:], ["--kubeconfig", runner.kubeconfig])
            self.assertFalse(runner.kubeconfig.exists())
            result = json.loads((root / "result.json").read_text())
            self.assertTrue(result["passed"])
            self.assertEqual(result["clusterDisposition"], "deleted")

    def test_cleanup_timeout_is_closed_and_still_removes_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = WRAPPER.StorageRunner.__new__(WRAPPER.StorageRunner)
            runner.output = root
            runner.cluster = "aps-storage-owned"
            runner.kubeconfig = root / "admin.kubeconfig"
            runner.kubeconfig.write_text("disposable credential")
            runner.created = True
            runner.image = {"reference": "localhost/probe@" + DIGEST_A,
                            "digest": DIGEST_A, "platformManifestDigest": DIGEST_A,
                            "configDigest": DIGEST_B, "archiveSha256": DIGEST_B}
            runner.events = []
            runner.mounts = []
            runner.pvc_identity = None
            runner.pv_identity = None
            runner.node_identity = None
            runner.crash_evidence = None
            runner.pod_runs = []
            runner.volume_checks = []

            def timeout(arguments, **_):
                raise subprocess.TimeoutExpired(arguments, 180, output="private output")

            runner.command = timeout
            status = runner.finish(True)
            result = json.loads((root / "result.json").read_text())
            self.assertEqual(status, 1)
            self.assertFalse(result["passed"])
            self.assertTrue(result["credentialsRemoved"])
            self.assertEqual(result["cleanup"], {"stage": "delete", "deleteExitCode": None,
                                                 "absenceConfirmed": False,
                                                 "boundedFailure": True})
            self.assertNotIn("private output", (root / "result.json").read_text())

    def test_keyboard_interrupt_finalizes_owned_cluster_and_credentials_once(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = WRAPPER.StorageRunner.__new__(WRAPPER.StorageRunner)
            runner.output = root / "output"
            runner.output.mkdir()
            runner.image_directory = root / "image"
            runner.cluster = "aps-storage-interrupted"
            runner.kubeconfig = runner.output / "admin.kubeconfig"
            runner.kubeconfig.write_text("disposable credential")
            runner.created = True
            runner.image = {"reference": "localhost/probe@" + DIGEST_A,
                            "digest": DIGEST_A, "platformManifestDigest": DIGEST_A,
                            "configDigest": DIGEST_B, "archiveSha256": DIGEST_B}
            runner.events = []
            runner.mounts = []
            runner.pod_runs = []
            runner.volume_checks = []
            runner.pvc_identity = None
            runner.pv_identity = None
            runner.node_identity = None
            runner.crash_evidence = None
            deletes = []

            def interrupt():
                raise KeyboardInterrupt

            def command(arguments, **_):
                deletes.append(arguments)
                return completed(arguments)

            runner.execute = interrupt
            runner.command = command
            runner.cluster_inventory = lambda: set()
            with (patch.object(WRAPPER, "StorageRunner", return_value=runner),
                  patch.object(WRAPPER.os, "umask"), redirect_stdout(io.StringIO())):
                status = WRAPPER.main(["--image-directory", str(runner.image_directory),
                                       "--output", str(runner.output)])
            self.assertEqual(status, 1)
            self.assertFalse(runner.kubeconfig.exists())
            self.assertEqual(sum(call[:3] == ["kind", "delete", "cluster"] for call in deletes), 1)
            result = json.loads((runner.output / "result.json").read_text())
            self.assertEqual(result["error"], "storage qualification interrupted")
            self.assertEqual(result["clusterDisposition"], "deleted")


if __name__ == "__main__":
    unittest.main()
