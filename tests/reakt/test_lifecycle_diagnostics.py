import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("lifecycle_diagnostics.py")
SPEC = importlib.util.spec_from_file_location("lifecycle_diagnostics", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("lifecycle diagnostics helper could not be imported")
DIAGNOSTICS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DIAGNOSTICS)


class Runner:
    def __init__(self, root):
        self.output = root
        self.admin = root / "admin.kubeconfig"
        self.control = "control"
        self.workload = "workload"
        self.domain = "lifecycle"
        self.results = []
        self.registry_uid = "registry-uid"
        self.generation = "generation-a"
        self.transition = "transition-a"
        self.deployment_uid = "deployment-uid"
        self.replica_uid = "rs-uid"
        self.pod_uid = "pod-uid"
        self.image = "registry.example/controller@sha256:" + "a" * 64
        self.reported_image = "sha256:" + "d" * 64
        self.image_id = "docker-pullable://" + self.image

    def get(self, kind, name, namespace=None):
        if kind == "configmap":
            active = {"artifactDigest": "sha256:" + "b" * 64,
                      "generationId": self.generation, "transitionId": self.transition,
                      "serviceAccountName": "reakt-generation-a"}
            state = {"histories": {self.domain: {"activeAllocation": active}}}
            return {"metadata": {"uid": self.registry_uid, "resourceVersion": "43"},
                    "data": {"state.json": json.dumps(state)}}
        if kind == "deployment":
            return {"metadata": {"name": name, "uid": self.deployment_uid}}
        if kind == "replicaset":
            return {"kind": "ReplicaSet", "metadata": {"uid": self.replica_uid,
                "ownerReferences": [{
                "controller": True, "kind": "Deployment", "name": "reakt-generation-a",
                "uid": self.deployment_uid,
            }]}}
        raise AssertionError((kind, name, namespace))

    def k(self, *args, **_kwargs):
        if args[:2] != ("get", "pods"):
            raise AssertionError(args)
        pod = {
            "kind": "Pod",
            "metadata": {"name": "runtime-pod", "uid": self.pod_uid,
                         "labels": {"reakt.dev/generation": self.generation},
                         "ownerReferences": [{"controller": True, "kind": "ReplicaSet",
                                              "name": "runtime-rs", "uid": "rs-uid"}]},
            "spec": {"serviceAccountName": "reakt-generation-a-process",
                     "containers": [{"name": "controller", "image": self.image}]},
            "status": {"phase": "Running", "containerStatuses": [{
                "name": "controller", "image": self.reported_image,
                "imageID": self.image_id,
                "containerID": "containerd://controller", "restartCount": 0,
            }]},
        }
        return subprocess.CompletedProcess([], 0, stdout=json.dumps({"items": [pod]}), stderr="")


def candidate(root):
    path = root / "candidate"
    path.mkdir()
    image = "registry.example/controller@sha256:" + "a" * 64
    (path / "release.json").write_text(json.dumps({"image": {"reference": image}}))
    (path / "ir.json").write_text(json.dumps({
        "inputs": [], "resources": [{"id": "retained", "name": "retained",
                                       "kind": "ConfigMap", "force": False,
                                       "desired": {"data": {"value": "version-1"}}}],
    }))
    return path


def status(**changes):
    value = {key: 0 for key in DIAGNOSTICS.COUNTER_KEYS}
    value.update({
        "schemaVersion": "reakt.dev/runtime-status/v1", "domain": "lifecycle",
        "buildDigest": "sha256:" + "b" * 64, "generation": "generation-a",
        "phase": "ready", "cache": "synchronized", "conditions": [],
        "lastReasonCode": None, "reconciliations": 1,
    })
    value.update(changes)
    return value


class Client:
    def __init__(self, response):
        self.response = response

    def get(self, path):
        if path != "/status":
            raise AssertionError(path)
        return self.response


class LifecycleDiagnosticsTests(unittest.TestCase):
    def test_runtime_identity_is_bound_to_active_owner_pod_sa_and_image(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = Runner(root)
            identity = DIAGNOSTICS._runtime_identity(
                runner, candidate(root),
                {"generation": runner.generation, "transition": runner.transition},
            )
            self.assertEqual(identity, {
                "pod": "runtime-pod", "podUid": "pod-uid",
                "ownerDeploymentUid": "deployment-uid", "generation": "generation-a",
                "serviceAccount": "reakt-generation-a-process", "image": runner.image,
                "imageID": "docker-pullable://" + runner.image,
                "reportedImage": runner.reported_image,
                "containerID": "containerd://controller", "restartCount": 0,
                "buildDigest": "sha256:" + "b" * 64,
                "registryUid": "registry-uid", "registryResourceVersion": "43",
            })

    def test_runtime_identity_rejects_replacement_and_wrong_image(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = candidate(root)
            allocation = {"generation": "generation-a", "transition": "transition-a"}
            for attribute, changed in [("deployment_uid", "replacement"),
                                       ("replica_uid", "replacement"),
                                       ("image_id", "docker-pullable://registry.example/controller@sha256:" + "e" * 64),
                                       ("reported_image", ""),
                                       ("image", "registry.example/wrong@sha256:" + "c" * 64)]:
                runner = Runner(root)
                setattr(runner, attribute, changed)
                if attribute == "deployment_uid":
                    original_get = runner.get
                    runner.get = lambda kind, name, namespace=None: (
                        {"metadata": {"name": name, "uid": "expected-deployment"}}
                        if kind == "deployment" else original_get(kind, name, namespace))
                with self.subTest(attribute=attribute), self.assertRaisesRegex(
                        RuntimeError, "runtime Pod identity"):
                    DIAGNOSTICS._runtime_identity(runner, path, allocation)

    def test_runtime_identity_rejects_registry_replacement(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runner = Runner(root)
            runner.registry_uid = "expected-registry"
            original_get = runner.get
            runner.get = lambda kind, name, namespace=None: (
                {**original_get(kind, name, namespace),
                 "metadata": {"uid": "replacement-registry", "resourceVersion": "44"}}
                if kind == "configmap" else original_get(kind, name, namespace))
            with self.assertRaisesRegex(RuntimeError, "registry identity"):
                DIAGNOSTICS._runtime_identity(
                    runner, candidate(root),
                    {"generation": runner.generation, "transition": runner.transition},
                )

    def test_status_requires_closed_active_binding_and_exact_conflict_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = Runner(Path(directory))
            identity = {"buildDigest": "sha256:" + "b" * 64, "generation": "generation-a"}
            ready = DIAGNOSTICS._validated_status(Client((200, status())), runner, identity)
            self.assertTrue(DIAGNOSTICS._ready(ready))
            conflict = status(
                phase="degraded", blockedTargets=1, fieldOrVersionConflicts=1,
                conditions=[{"reasonCode": "field-or-version-conflict",
                             "resourceId": "retained"}],
                lastReasonCode="field-or-version-conflict",
            )
            self.assertTrue(DIAGNOSTICS._conflicted(conflict, "retained"))
            self.assertEqual(DIAGNOSTICS._status_evidence(conflict), {
                "phase": "degraded", "cache": "synchronized", "blockedTargets": 1,
                "conditions": [{"reasonCode": "field-or-version-conflict",
                                "resourceId": "retained"}],
                "fieldOrVersionConflicts": 1,
                "lastReasonCode": "field-or-version-conflict",
                "reconciliations": 1, "errors": 0,
            })
            with self.assertRaisesRegex(RuntimeError, "shape"):
                DIAGNOSTICS._validated_status(
                    Client((200, dict(ready, privateValue="secret"))), runner, identity,
                )
            with self.assertRaisesRegex(RuntimeError, "active generation"):
                DIAGNOSTICS._validated_status(
                    Client((200, dict(ready, generation="replacement"))), runner, identity,
                )

    def test_fixture_apply_uses_admin_ssa_force_then_same_manager_relinquishment(self):
        calls = []

        class ApplyRunner:
            workload = "workload"

            def k(self, *args, data=None):
                calls.append((args, json.loads(data)))
                return subprocess.CompletedProcess(
                    [], 0, stdout=json.dumps({"metadata": {"uid": "retained-uid"}}), stderr="",
                )

        runner = ApplyRunner()
        DIAGNOSTICS._apply_fixture_field(runner, {"value": "foreign"})
        DIAGNOSTICS._apply_fixture_field(runner, None)
        self.assertIn("--force-conflicts", calls[0][0])
        self.assertNotIn("--force-conflicts", calls[1][0])
        self.assertIn("--field-manager=" + DIAGNOSTICS.FIELD_MANAGER, calls[0][0])
        self.assertIn("--field-manager=" + DIAGNOSTICS.FIELD_MANAGER, calls[1][0])
        self.assertEqual(calls[0][1]["data"], {"value": "foreign"})
        self.assertNotIn("data", calls[1][1])

    def test_forward_cleanup_closes_output_and_preserves_reap_failure(self):
        stream = mock.Mock()
        process = mock.Mock(stdout=stream)
        drain = mock.Mock()
        with mock.patch.object(DIAGNOSTICS, "_reap", side_effect=RuntimeError("reap failed")):
            with self.assertRaisesRegex(RuntimeError, "reap failed"):
                DIAGNOSTICS._finish_forward(process, drain)
        drain.finish.assert_called_once_with()
        stream.close.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
