#!/usr/bin/env python3
"""Qualify local libSQL inventory recovery on an owned kind local PV."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid


ROOT = Path(__file__).resolve().parents[2]
NODE_IMAGE = "kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48"
BUILDER_IMAGE = "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922"
LOCAL_PATH = "/var/local/apsides-storage-qualification"
MOUNT_PATH = "/store"
NAMESPACE = "apsides-storage-qualification"
PV_NAME = "apsides-storage-pv"
PVC_NAME = "apsides-storage-pvc"
PROBE_TEST = ["--exact", "pvc_inventory_phase", "--ignored", "--nocapture"]
EVENT_FIELDS = {
    "nonce", "schemaVersion", "phase", "outcome", "revision", "snapshotId",
    "firstSnapshotId", "secondSnapshotId", "largeSnapshotBytes", "journalBytes",
}


def closed_error(error):
    if isinstance(error, subprocess.TimeoutExpired):
        return "a bounded subprocess timed out"
    if isinstance(error, RuntimeError):
        return str(error)
    return "storage qualification failed"


def parse_event(output, prefix, nonce, phase, outcomes):
    if len(output.encode()) > 64 * 1024:
        raise RuntimeError("probe output exceeds its bound")
    records = []
    for line in output.splitlines():
        if line.startswith(prefix):
            if len(line.encode()) > 8192:
                raise RuntimeError("probe event exceeds its bound")
            try:
                records.append(json.loads(line[len(prefix):]))
            except (TypeError, ValueError) as error:
                raise RuntimeError("probe event is malformed") from error
    if len(records) != 1:
        raise RuntimeError("probe emitted an unexpected event count")
    record = records[0]
    if (not isinstance(record, dict) or set(record) != EVENT_FIELDS
            or record.get("nonce") != nonce
            or record.get("schemaVersion") != "reakt.dev/pvc-inventory-probe/v1"
            or record.get("phase") != phase or record.get("outcome") not in outcomes
            or record.get("revision") is not None and not isinstance(record["revision"], str)
            or record.get("snapshotId") is not None and not valid_digest(record["snapshotId"])
            or not valid_digest(record.get("firstSnapshotId"))
            or not valid_digest(record.get("secondSnapshotId"))
            or not isinstance(record.get("largeSnapshotBytes"), int)
            or record["largeSnapshotBytes"] <= 1024 * 1024
            or record["largeSnapshotBytes"] > 64 * 1024 * 1024
            or (prefix == "APS_STORAGE_CRASH_READY=" and
                (not isinstance(record.get("journalBytes"), int) or record["journalBytes"] <= 512))
            or (prefix != "APS_STORAGE_CRASH_READY=" and record.get("journalBytes") is not None)):
        raise RuntimeError("probe event is invalid")
    if "private" in output.lower() or "desired" in output.lower():
        raise RuntimeError("probe output contains a private-field canary")
    return record


def valid_digest(value):
    return (isinstance(value, str) and value.startswith("sha256:")
            and len(value) == 71
            and all(character in "0123456789abcdef" for character in value[7:]))


def runtime_image_matches(image_id, expected):
    if not isinstance(image_id, str) or not image_id or len(image_id) > 1024:
        return False
    tail = image_id.rsplit("@", 1)[-1]
    if "@" not in image_id and "://" in image_id:
        tail = image_id.split("://", 1)[1]
    return tail == expected


def file_digest(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def normalized_read_only(value):
    if value is None or value is False:
        return False
    if value is True:
        return True
    raise RuntimeError("probe Pod storage contract is invalid")


def pod_storage_contract(value):
    if not isinstance(value, dict) or not isinstance(value.get("spec"), dict):
        raise RuntimeError("probe Pod storage contract is invalid")
    spec = value["spec"]
    containers = spec.get("containers", [])
    volumes = spec.get("volumes", [])
    if (not isinstance(containers, list) or len(containers) != 1
            or not isinstance(containers[0], dict)
            or not isinstance(volumes, list) or not 1 <= len(volumes) <= 8
            or not all(isinstance(item, dict) for item in volumes)):
        raise RuntimeError("probe Pod storage contract is invalid")
    mounts = containers[0].get("volumeMounts", [])
    if (not isinstance(mounts, list) or len(mounts) != len(volumes)
            or not all(isinstance(item, dict) for item in mounts)):
        raise RuntimeError("probe Pod storage contract is invalid")
    volume_contract = []
    for volume in volumes:
        name = volume.get("name")
        claim = volume.get("persistentVolumeClaim")
        claim_name = claim.get("claimName") if isinstance(claim, dict) else None
        if (set(volume) != {"name", "persistentVolumeClaim"}
                or not isinstance(claim, dict) or set(claim) - {"claimName", "readOnly"}
                or not isinstance(name, str) or not name or len(name) > 63
                or not isinstance(claim_name, str) or not claim_name
                or len(claim_name) > 253):
            raise RuntimeError("probe Pod storage contract is invalid")
        volume_contract.append({"name": name, "claimName": claim_name,
                                "readOnly": normalized_read_only(claim.get("readOnly"))})
    mount_contract = []
    for mount in mounts:
        name = mount.get("name")
        path = mount.get("mountPath")
        if (set(mount) - {"name", "mountPath", "readOnly"}
                or not isinstance(name, str) or not name or len(name) > 63
                or not isinstance(path, str) or not path.startswith("/")
                or len(path) > 1024 or any(character.isspace() for character in path)):
            raise RuntimeError("probe Pod storage contract is invalid")
        mount_contract.append({"name": name, "mountPath": path,
                               "readOnly": normalized_read_only(mount.get("readOnly"))})
    volume_contract.sort(key=lambda item: item["name"])
    mount_contract.sort(key=lambda item: item["name"])
    names = [item["name"] for item in volume_contract]
    mount_names = [item["name"] for item in mount_contract]
    if len(names) != len(set(names)) or len(mount_names) != len(set(mount_names)) or names != mount_names:
        raise RuntimeError("probe Pod storage contract is invalid")
    return {"volumes": volume_contract, "mounts": mount_contract}


class StorageRunner:
    PACKAGE = "inventory"
    RESULT_SCHEMA = "apsides.dev/storage-restart-qualification/v1"
    SCOPE = "local-libsql-on-kind-local-pv-sigkill-reopen"
    BOUNDARY = "static local PV backed by the owned kind node; no CSI, node-failure, power-loss, Turso, fencing, or interruption-inside-publish claim"

    def __init__(self, arguments):
        self.output = arguments.output.resolve()
        self.output.mkdir(parents=True, exist_ok=False)
        self.image_directory = arguments.image_directory.resolve()
        self.cluster = "aps-storage-" + uuid.uuid4().hex[:12]
        self.node = self.cluster + "-control-plane"
        self.kubeconfig = self.output / "admin.kubeconfig"
        self.environment = {**os.environ, "KUBECONFIG": str(self.kubeconfig)}
        self.created = False
        self.image = self.load_build_receipt()
        self.nonce = uuid.uuid4().hex
        self.events = []
        self.mounts = []
        self.pvc_identity = None
        self.pv_identity = None
        self.node_identity = None
        self.fixture_identity = None
        self.winner_snapshot_id = None
        self.crash_evidence = None
        self.expected_pods = {}
        self.pod_runs = []
        self.runtime_image_id = None
        self.volume_checks = []

    def load_build_receipt(self):
        path = self.image_directory / "build.json"
        try:
            if (not path.is_file() or path.is_symlink() or path.stat().st_size > 64 * 1024):
                raise RuntimeError("storage image build receipt is invalid")
            receipt = json.loads(path.read_text())
        except (OSError, TypeError, ValueError) as error:
            raise RuntimeError("storage image build receipt is unavailable") from error
        expected = {"schemaVersion", "builder", "platform", "target", "contextDigest",
                    "imageDigest", "platformManifestDigest", "configDigest", "archive",
                    "qualification"}
        if not isinstance(receipt, dict) or set(receipt) != expected:
            raise RuntimeError("storage image build receipt is invalid")
        digest = receipt.get("imageDigest")
        archive_name = f"{self.PACKAGE}-pvc.oci.tar"
        archive = self.image_directory / archive_name
        archive_record = receipt.get("archive")
        builder = receipt.get("builder")
        if (receipt.get("schemaVersion") != f"reakt.dev/{self.PACKAGE}-pvc-test-image/v1"
                or receipt.get("platform") != "linux/arm64"
                or receipt.get("target") != {"package": self.PACKAGE, "test": "pvc_restart"}
                or receipt.get("qualification") != "test-only; not a production release artifact"
                or not isinstance(builder, dict)
                or builder.get("reference") != BUILDER_IMAGE
                or builder.get("platform") != "linux/arm64"
                or not valid_digest(builder.get("localImageId"))
                or not valid_digest(digest) or not valid_digest(receipt.get("contextDigest"))
                or not valid_digest(receipt.get("platformManifestDigest"))
                or not valid_digest(receipt.get("configDigest"))
                or not isinstance(archive_record, dict)
                or archive_record.get("path") != archive_name
                or not valid_digest(archive_record.get("sha256"))
                or not archive.is_file() or archive.is_symlink()
                or archive.stat().st_size <= 0):
            raise RuntimeError("storage image build receipt is invalid")
        archive_digest = file_digest(archive)
        if archive_digest != archive_record["sha256"]:
            raise RuntimeError("storage image archive digest differs")
        return {"reference": f"localhost/apsides-{self.PACKAGE}-pvc@" + digest,
                "digest": digest, "platformManifestDigest": receipt["platformManifestDigest"],
                "configDigest": receipt["configDigest"], "archive": archive,
                "archiveSha256": archive_digest}

    def command(self, arguments, *, data=None, timeout=180, check=True):
        result = subprocess.run([str(value) for value in arguments], input=data, text=True,
                                capture_output=True, check=False, cwd=ROOT,
                                env=self.environment, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"{arguments[0]} operation failed with exit code {result.returncode}")
        return result

    def k(self, *arguments, data=None, check=True, timeout=180):
        return self.command(["kubectl", "--kubeconfig", self.kubeconfig, *arguments],
                            data=data, check=check, timeout=timeout)

    def apply(self, value):
        result = self.k("apply", "-f", "-", "-o", "json",
                        data=json.dumps(value, separators=(",", ":")))
        try:
            return json.loads(result.stdout)
        except (TypeError, ValueError) as error:
            raise RuntimeError("Kubernetes returned malformed object metadata") from error

    def get(self, kind, name, namespace=None):
        arguments = ["get", kind, name]
        if namespace:
            arguments += ["-n", namespace]
        result = self.k(*arguments, "-o", "json")
        try:
            return json.loads(result.stdout)
        except (TypeError, ValueError) as error:
            raise RuntimeError("Kubernetes returned malformed object metadata") from error

    def cluster_inventory(self):
        result = self.command(["kind", "get", "clusters"], check=False, timeout=30)
        if result.returncode:
            raise RuntimeError("could not establish exact kind cluster inventory")
        return {line.strip() for line in result.stdout.splitlines() if line.strip()}

    def create_cluster(self):
        if self.cluster in self.cluster_inventory():
            raise RuntimeError("disposable cluster name already exists")
        self.command(["docker", "image", "inspect", NODE_IMAGE], timeout=30)
        creation = self.command([
            "kind", "create", "cluster", "--name", self.cluster, "--image", NODE_IMAGE,
            "--kubeconfig", self.kubeconfig, "--wait", "120s",
        ], check=False, timeout=240)
        if creation.returncode:
            raise RuntimeError("disposable cluster creation failed; ownership not established")
        self.created = True
        self.command(["docker", "exec", self.node, "mkdir", "-p", LOCAL_PATH])
        self.command(["docker", "exec", self.node, "chown", "65532:65532", LOCAL_PATH])
        with self.image["archive"].open("rb") as archive:
            imported = subprocess.run([
                "docker", "exec", "-i", self.node, "ctr", "-n", "k8s.io", "images",
                "import", "--digests", "--platform", "linux/arm64", "--base-name",
                f"localhost/apsides-{self.PACKAGE}-pvc", "-",
            ], stdin=archive, text=False, capture_output=True, check=False, timeout=180)
        if imported.returncode:
            raise RuntimeError("containerd image import failed")
        self.command(["docker", "exec", self.node, "crictl", "inspecti", self.image["reference"]])

    def provision_volume(self):
        self.apply({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NAMESPACE}})
        node = self.get("node", self.node)
        self.node_identity = {"name": self.node, "uid": node["metadata"]["uid"]}
        pv = self.apply({
            "apiVersion": "v1", "kind": "PersistentVolume",
            "metadata": {"name": PV_NAME},
            "spec": {
                "capacity": {"storage": "256Mi"}, "volumeMode": "Filesystem",
                "accessModes": ["ReadWriteOnce"], "persistentVolumeReclaimPolicy": "Retain",
                "storageClassName": "", "local": {"path": LOCAL_PATH},
                "nodeAffinity": {"required": {"nodeSelectorTerms": [{"matchExpressions": [{
                    "key": "kubernetes.io/hostname", "operator": "In", "values": [self.node],
                }]}]}},
            },
        })
        pvc = self.apply({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": {"name": PVC_NAME, "namespace": NAMESPACE},
            "spec": {"accessModes": ["ReadWriteOnce"], "volumeMode": "Filesystem",
                     "storageClassName": "", "volumeName": PV_NAME,
                     "resources": {"requests": {"storage": "256Mi"}}},
        })
        self.pv_identity = {"name": PV_NAME, "uid": pv["metadata"]["uid"],
                            "localPath": LOCAL_PATH, "storageClass": ""}
        self.pvc_identity = {"name": PVC_NAME, "uid": pvc["metadata"]["uid"],
                             "volumeName": PV_NAME}
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.get("pvc", PVC_NAME, NAMESPACE).get("status", {}).get("phase") == "Bound":
                self.validate_volume()
                return
            time.sleep(0.5)
        raise RuntimeError("PVC did not bind before its deadline")

    def validate_volume(self, pod=None, terminal=False):
        pvc = self.get("pvc", PVC_NAME, NAMESPACE)
        pv = self.get("pv", PV_NAME)
        node = self.get("node", self.node)
        expected_pvc = self.pvc_identity
        expected_pv = self.pv_identity
        affinity = pv.get("spec", {}).get("nodeAffinity", {}).get("required", {})
        expected_affinity = {"nodeSelectorTerms": [{"matchExpressions": [{
            "key": "kubernetes.io/hostname", "operator": "In", "values": [self.node],
        }]}]}
        if expected_pvc is None or expected_pv is None or self.node_identity is None:
            raise RuntimeError("PVC/PV/node validation failed: expected-identity")
        checks = [
            (pvc.get("metadata", {}).get("uid") == expected_pvc["uid"], "pvc-uid"),
            (pvc.get("spec", {}).get("volumeName") == PV_NAME, "pvc-volume"),
            (pvc.get("spec", {}).get("accessModes") == ["ReadWriteOnce"], "pvc-access-mode"),
            (pvc.get("spec", {}).get("storageClassName") == "", "pvc-storage-class"),
            (pvc.get("status", {}).get("phase") == "Bound", "pvc-phase"),
            (pv.get("metadata", {}).get("uid") == expected_pv["uid"], "pv-uid"),
            (pv.get("spec", {}).get("claimRef", {}).get("uid") == expected_pvc["uid"],
             "pv-claim-uid"),
            (pv.get("spec", {}).get("local", {}).get("path") == LOCAL_PATH, "pv-local-path"),
            (pv.get("spec", {}).get("accessModes") == ["ReadWriteOnce"], "pv-access-mode"),
            # The API omits this field for an explicitly empty PV storage class.
            (pv.get("spec", {}).get("storageClassName", "") == "", "pv-storage-class"),
            (pv.get("spec", {}).get("persistentVolumeReclaimPolicy") == "Retain",
             "pv-reclaim-policy"),
            (affinity == expected_affinity, "pv-node-affinity"),
            (node.get("metadata", {}).get("labels", {}).get("kubernetes.io/hostname")
             == self.node, "node-hostname"),
            (node.get("metadata", {}).get("uid") == self.node_identity["uid"], "node-uid"),
        ]
        failures = [field for valid, field in checks if not valid]
        if failures:
            raise RuntimeError("PVC/PV/node validation failed: " + ",".join(failures))
        if pod is None:
            self.volume_checks.append({"pvcUid": expected_pvc["uid"],
                                       "pvUid": expected_pv["uid"],
                                       "nodeUid": self.node_identity["uid"]})
        if pod is not None:
            value = self.get("pod", pod, NAMESPACE)
            self.validate_pod(value, pod, terminal)

    def validate_pod(self, value, name, terminal):
        expected = self.expected_pods.get(name)
        spec = value.get("spec", {})
        containers = spec.get("containers", [])
        if expected is None or len(containers) != 1:
            raise RuntimeError("probe Pod expectation is missing")
        container = containers[0]
        env = {item.get("name"): item.get("value") for item in container.get("env", [])}
        try:
            storage = pod_storage_contract(value)
        except RuntimeError as error:
            raise RuntimeError("probe Pod identity or admitted contract changed") from error
        if (value.get("metadata", {}).get("uid") != expected["uid"]
                or value.get("metadata", {}).get("name") != name
                or spec.get("nodeName") != self.node
                or spec.get("automountServiceAccountToken") is not False
                or container.get("name") != "probe"
                or container.get("image") != self.image["reference"]
                or container.get("imagePullPolicy") != "Never"
                or container.get("command") != expected["command"]
                or container.get("args", []) != expected["args"]
                or env != expected["env"]
                or storage != expected["storage"]):
            raise RuntimeError("probe Pod identity or admitted contract changed")
        if not terminal:
            return
        statuses = value.get("status", {}).get("containerStatuses", [])
        if len(statuses) != 1:
            raise RuntimeError("probe Pod terminal status is incomplete")
        status = statuses[0]
        terminated = status.get("state", {}).get("terminated", {})
        container_id = status.get("containerID")
        image_id = status.get("imageID")
        if (status.get("name") != "probe" or not isinstance(container_id, str)
                or not container_id or len(container_id) > 1024
                or not runtime_image_matches(image_id, self.image["platformManifestDigest"])
                or terminated.get("exitCode") != 0 or terminated.get("signal", 0) != 0
                or not isinstance(terminated.get("startedAt"), str)
                or not isinstance(terminated.get("finishedAt"), str)):
            raise RuntimeError("probe Pod terminal execution differs")
        if self.runtime_image_id is None:
            self.runtime_image_id = image_id
        elif self.runtime_image_id != image_id:
            raise RuntimeError("probe runtime image identity changed")
        self.mounts.append({"pod": name, "podUid": expected["uid"], "node": self.node,
                            "storage": expected["storage"]})
        self.pod_runs.append({"pod": name, "podUid": expected["uid"],
                              "containerId": container_id, "imageId": image_id,
                              "exitCode": 0, "startedAt": terminated["startedAt"],
                              "finishedAt": terminated["finishedAt"]})

    def pod_object(self, name, phase, *, read_only=False, crash=False, barrier=False):
        container = {
            "name": "probe", "image": self.image["reference"], "imagePullPolicy": "Never",
            "env": [
                {"name": "APS_STORAGE_TEST_PHASE", "value": phase},
                {"name": "APS_STORAGE_TEST_PATH", "value": MOUNT_PATH + "/inventory.db"},
                {"name": "APS_STORAGE_TEST_NONCE", "value": self.nonce},
            ],
            "args": PROBE_TEST,
            "securityContext": {"allowPrivilegeEscalation": False,
                                "capabilities": {"drop": ["ALL"]}},
            "volumeMounts": [{"name": "store", "mountPath": MOUNT_PATH,
                              "readOnly": read_only}],
        }
        if crash:
            container["command"] = ["/usr/local/bin/storage-crash"]
            container["args"] = []
        if barrier:
            container["command"] = ["/bin/sh", "-c"]
            container["args"] = ["set -eu; test ! -e /store/race-go; umask 077; : > /store/race-go; sync"]
            container["env"] = []
        return {
            "apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "namespace": NAMESPACE},
            "spec": {"automountServiceAccountToken": False, "restartPolicy": "Never",
                     "nodeName": self.node, "securityContext": {"runAsUser": 65532,
                     "runAsGroup": 65532, "fsGroup": 65532, "seccompProfile": {"type": "RuntimeDefault"}},
                     "containers": [container], "volumes": [{"name": "store",
                     "persistentVolumeClaim": {"claimName": PVC_NAME, "readOnly": read_only}}]},
        }

    def create_pod(self, name, phase, **options):
        value = self.pod_object(name, phase, **options)
        result = self.k("create", "-f", "-", "-o", "json",
                        data=json.dumps(value, separators=(",", ":")))
        try:
            created = json.loads(result.stdout)
        except (TypeError, ValueError) as error:
            raise RuntimeError("Pod creation returned malformed metadata") from error
        if created.get("metadata", {}).get("name") != name:
            raise RuntimeError("created Pod identity differs")
        container = value["spec"]["containers"][0]
        storage = pod_storage_contract(value)
        self.expected_pods[name] = {
            "uid": created.get("metadata", {}).get("uid"),
            "command": container.get("command"), "args": container.get("args", []),
            "env": {item["name"]: item["value"] for item in container.get("env", [])},
            "storage": storage,
        }
        self.validate_pod(created, name, False)
        return created

    def pod_logs(self, name):
        return self.k("logs", name, "-n", NAMESPACE, "--limit-bytes=65537", timeout=30).stdout

    @staticmethod
    def require_probe_test(output):
        if (output.count("test pvc_inventory_phase ... ok") != 1
                or output.count("test result: ok. 1 passed; 0 failed; 0 ignored") != 1):
            raise RuntimeError("expected ignored PVC probe did not execute exactly once")

    def wait_pod(self, name, timeout=180):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = self.get("pod", name, NAMESPACE)
            self.validate_pod(value, name, False)
            phase = value.get("status", {}).get("phase")
            if phase == "Succeeded":
                self.validate_volume(name, True)
                return self.pod_logs(name)
            if phase == "Failed":
                raise RuntimeError("probe Pod failed")
            time.sleep(0.5)
        raise RuntimeError("probe Pod exceeded its deadline")

    def run_phase(self, name, phase, *, read_only=False, crash=False):
        self.validate_volume()
        self.create_pod(name, phase, read_only=read_only, crash=crash)
        output = self.wait_pod(name)
        self.require_probe_test(output)
        expected = {"seed": "published", "verify": "verified", "recover": "verified",
                    "unavailable": "rejected"}[phase]
        record = parse_event(output, "APS_STORAGE_RESULT=", self.nonce, phase, {expected})
        self.accept_event(record)
        return record

    def accept_event(self, record):
        fixture = (record["firstSnapshotId"], record["secondSnapshotId"],
                   record["largeSnapshotBytes"])
        if self.fixture_identity is None:
            self.fixture_identity = fixture
        elif self.fixture_identity != fixture:
            raise RuntimeError("probe fixture identity changed between Pods")
        self.events.append(record)

    def wait_ready(self, name, phase):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            result = self.k("logs", name, "-n", NAMESPACE, "--limit-bytes=65537",
                            check=False, timeout=30)
            if result.returncode == 0 and "APS_STORAGE_READY=" in result.stdout:
                return parse_event(result.stdout, "APS_STORAGE_READY=", self.nonce,
                                   phase, {"ready"})
            pod = self.get("pod", name, NAMESPACE)
            self.validate_pod(pod, name, False)
            if pod.get("status", {}).get("phase") in {"Failed", "Succeeded"}:
                raise RuntimeError("race publisher exited before its barrier")
            time.sleep(0.5)
        raise RuntimeError("race publisher readiness deadline exceeded")

    def race(self):
        self.validate_volume()
        names = {"race-left": "race-left", "race-right": "race-right"}
        for phase, name in names.items():
            self.create_pod(name, phase)
        for phase, name in names.items():
            self.wait_ready(name, phase)
            self.validate_volume(name)
        self.create_pod("race-barrier", "verify", barrier=True)
        self.wait_pod("race-barrier")
        records = []
        for phase, name in names.items():
            output = self.wait_pod(name)
            self.require_probe_test(output)
            record = parse_event(output, "APS_STORAGE_RESULT=", self.nonce, phase,
                                 {"published", "conflict"})
            records.append(record)
        if sorted(record["outcome"] for record in records) != ["conflict", "published"]:
            raise RuntimeError("concurrent publishers did not produce one success and one conflict")
        if (any(record.get("revision") != "3" for record in records)
                or records[0].get("snapshotId") != records[1].get("snapshotId")):
            raise RuntimeError("concurrent publisher revision differs")
        self.winner_snapshot_id = records[0]["snapshotId"]
        for record in records:
            self.accept_event(record)

    def crash(self):
        self.validate_volume()
        self.create_pod("crash", "crash-ready", crash=True)
        output = self.wait_pod("crash")
        ready = parse_event(output, "APS_STORAGE_CRASH_READY=", self.nonce,
                            "crash-ready", {"uncommitted"})
        killed_lines = [line for line in output.splitlines() if line.startswith("APS_STORAGE_KILLED=")]
        marker_lines = [line for line in output.splitlines() if line.startswith("APS_STORAGE_")]
        if (len(killed_lines) != 1 or len(killed_lines[0].encode()) > 1024
                or len(marker_lines) != 2):
            raise RuntimeError("crash wrapper emitted an unexpected kill event")
        try:
            killed = json.loads(killed_lines[0].split("=", 1)[1])
        except (TypeError, ValueError) as error:
            raise RuntimeError("crash kill event is malformed") from error
        if killed != {"nonce": self.nonce, "signal": 9, "waitStatus": 137, "reaped": True}:
            raise RuntimeError("crash kill event is invalid")
        if ready.get("revision") != "3" or ready.get("snapshotId") != self.winner_snapshot_id:
            raise RuntimeError("crash transaction started from a different committed head")
        self.accept_event(ready)
        self.crash_evidence = killed

    def execute(self):
        self.create_cluster()
        self.provision_volume()
        seed = self.run_phase("seed", "seed")
        verify = self.run_phase("verify", "verify")
        if ((seed["revision"], verify["revision"]) != ("2", "2")
                or seed["snapshotId"] != seed["secondSnapshotId"]
                or verify["snapshotId"] != seed["snapshotId"]):
            raise RuntimeError("seed or separate-Pod verification revision differs")
        self.race()
        winner = self.run_phase("verify-winner", "recover")
        if winner["revision"] != "3" or winner["snapshotId"] != self.winner_snapshot_id:
            raise RuntimeError("winner verification differs")
        self.crash()
        recovered = self.run_phase("recover", "recover")
        if recovered["revision"] != "3" or recovered["snapshotId"] != self.winner_snapshot_id:
            raise RuntimeError("post-SIGKILL committed head differs")
        unavailable = self.run_phase("read-only", "unavailable", read_only=True)
        if unavailable["revision"] is not None or unavailable["snapshotId"] is not None:
            raise RuntimeError("read-only refusal unexpectedly reported a head")
        healthy = self.run_phase("healthy", "recover")
        if healthy["revision"] != "3" or healthy["snapshotId"] != self.winner_snapshot_id:
            raise RuntimeError("healthy reopen head differs")

    def finish(self, success, error=None):
        cleanup = {"stage": "not-owned", "deleteExitCode": None, "absenceConfirmed": False}
        if self.created:
            try:
                deletion = self.command(["kind", "delete", "cluster", "--name", self.cluster,
                                         "--kubeconfig", self.kubeconfig], check=False, timeout=180)
                cleanup = {"stage": "delete", "deleteExitCode": deletion.returncode,
                           "absenceConfirmed": False}
                inventory = self.cluster_inventory()
                cleanup["stage"] = ("complete" if deletion.returncode == 0
                                    and self.cluster not in inventory else "verify-absence")
                cleanup["absenceConfirmed"] = self.cluster not in inventory
            except (OSError, RuntimeError, subprocess.SubprocessError):
                cleanup = {"stage": "delete", "deleteExitCode": None,
                           "absenceConfirmed": False, "boundedFailure": True}
            if cleanup["stage"] != "complete":
                success = False
        try:
            self.kubeconfig.unlink(missing_ok=True)
        except OSError:
            success = False
        result = {
            "schemaVersion": self.RESULT_SCHEMA,
            "passed": bool(success), "cluster": self.cluster, "nodeImage": NODE_IMAGE,
            "scope": self.SCOPE,
            "storageBoundary": self.BOUNDARY,
            "clusterDisposition": "deleted" if cleanup["stage"] == "complete" else cleanup["stage"],
            "credentialsRemoved": not self.kubeconfig.exists(), "cleanup": cleanup,
            "image": {"reference": self.image["reference"],
                      "imageDigest": self.image["digest"],
                      "platformManifestDigest": self.image["platformManifestDigest"],
                      "configDigest": self.image["configDigest"],
                      "archiveSha256": self.image["archiveSha256"]},
            "pvc": self.pvc_identity, "pv": self.pv_identity, "node": self.node_identity,
            "mounts": self.mounts, "podRuns": self.pod_runs,
            "volumeChecks": self.volume_checks,
            "events": self.events, "crash": self.crash_evidence,
        }
        if error:
            result["error"] = error
        if not result["credentialsRemoved"] or result["clusterDisposition"] != "deleted":
            result["passed"] = False
        (self.output / "cluster-cleanup.json").write_text(json.dumps(cleanup, indent=2) + "\n")
        (self.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        return 0 if result["passed"] else 1


def parse_arguments(arguments=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--image-directory", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args(arguments)


def main(arguments=None, runner_class=None):
    os.umask(0o077)
    try:
        runner = (runner_class or StorageRunner)(parse_arguments(arguments))
    except (KeyError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(json.dumps({"passed": False, "error": closed_error(error)}))
        return 1
    success = False
    error_record = None
    try:
        runner.execute()
        success = True
    except KeyboardInterrupt:
        error_record = "storage qualification interrupted"
    except Exception as error:  # Cleanup must also cover unexpected harness defects.
        error_record = closed_error(error)
    finally:
        status = runner.finish(success, error_record)
    print(json.dumps({"passed": status == 0, "evidence": str(runner.output / "result.json")}))
    return status


if __name__ == "__main__":
    sys.exit(main())
