#!/usr/bin/env python3
"""Verify a shared metadata backup with no source volume in the verifier Pod."""
import json
import sys
import time

from run_storage_restart import NAMESPACE, LOCAL_PATH, StorageRunner, main, valid_digest


BACKUP_PV = "apsides-backup-pv"
BACKUP_PVC = "apsides-backup-pvc"
BACKUP_PATH = "/var/local/apsides-backup-qualification"
PREFIX = "APS_COORDINATION_BACKUP_RESULT="
DIGESTS = {"seedClaimDigest", "seedReviewDigest", "seedAuditDigest",
           "firstInventorySnapshotId", "secondInventorySnapshotId",
           "laterClaimDigest", "laterReviewDigest", "laterAuditDigest",
           "thirdInventorySnapshotId"}
OUTCOMES = {"backup-seed": "seeded", "backup-create": "created",
            "backup-mutate": "mutated", "backup-verify": "verified"}


def parse_backup_event(output, nonce, phase):
    lines = [line for line in output.splitlines() if line.startswith(PREFIX)]
    if len(output.encode()) > 64 * 1024 or len(lines) != 1 or len(lines[0].encode()) > 4096:
        raise RuntimeError("backup event count or size differs")
    try:
        record = json.loads(lines[0][len(PREFIX):])
    except ValueError as error:
        raise RuntimeError("backup event is malformed") from error
    fields = DIGESTS | {"schemaVersion", "nonce", "phase", "outcome",
                        "expectedInventoryRevision", "observedInventoryRevision"}
    if (not isinstance(record, dict) or set(record) != fields
            or record["schemaVersion"] != "apsides.dev/pvc-coordination-backup/v1"
            or record["nonce"] != nonce or record["phase"] != phase
            or phase not in OUTCOMES or record["outcome"] != OUTCOMES[phase]
            or record["expectedInventoryRevision"] != "2"
            or record["observedInventoryRevision"] != ("3" if phase == "backup-mutate" else "2")
            or any(not valid_digest(record[key]) for key in DIGESTS)):
        raise RuntimeError("backup event is invalid")
    if any(record[left] == record[right] for left, right in [
        ("seedClaimDigest", "laterClaimDigest"), ("seedReviewDigest", "laterReviewDigest"),
        ("seedAuditDigest", "laterAuditDigest"),
        ("firstInventorySnapshotId", "secondInventorySnapshotId"),
        ("secondInventorySnapshotId", "thirdInventorySnapshotId"),
    ]):
        raise RuntimeError("backup fixtures are not distinct")
    if "private" in output.lower() or "desired" in output.lower():
        raise RuntimeError("backup probe emitted a private-field canary")
    if (output.count("test pvc_coordination_phase ... ok") != 1
            or output.count("test result: ok. 1 passed; 0 failed; 0 ignored") != 1):
        raise RuntimeError("backup probe did not execute exactly once")
    return record


class BackupRunner(StorageRunner):
    PACKAGE = "coordination"
    RESULT_SCHEMA = "apsides.dev/storage-backup-qualification/v1"
    SCOPE = "four-store-backup-on-distinct-local-pv-mounts"
    BOUNDARY = "SQLite store backups on two local PV paths on one owned node; no independent failure domains, power-loss, concurrent snapshot timing, or restore-authority claim"

    def __init__(self, arguments):
        self.backup_identity = None
        super().__init__(arguments)

    def provision_volume(self):
        super().provision_volume()
        self.command(["docker", "exec", self.node, "mkdir", "-p", BACKUP_PATH])
        self.command(["docker", "exec", self.node, "chown", "65532:65532", BACKUP_PATH])
        pv = self.apply({"apiVersion": "v1", "kind": "PersistentVolume",
                         "metadata": {"name": BACKUP_PV}, "spec": {
            "capacity": {"storage": "256Mi"}, "volumeMode": "Filesystem",
            "accessModes": ["ReadWriteOnce"], "persistentVolumeReclaimPolicy": "Retain",
            "storageClassName": "", "local": {"path": BACKUP_PATH},
            "nodeAffinity": {"required": {"nodeSelectorTerms": [{"matchExpressions": [{
                "key": "kubernetes.io/hostname", "operator": "In", "values": [self.node],
            }]}]}},
        }})
        pvc = self.apply({"apiVersion": "v1", "kind": "PersistentVolumeClaim",
                          "metadata": {"name": BACKUP_PVC, "namespace": NAMESPACE}, "spec": {
            "accessModes": ["ReadWriteOnce"], "volumeMode": "Filesystem",
            "storageClassName": "", "volumeName": BACKUP_PV,
            "resources": {"requests": {"storage": "256Mi"}},
        }})
        self.backup_identity = {"pvUid": pv["metadata"]["uid"], "pvcUid": pvc["metadata"]["uid"],
                                "pv": BACKUP_PV, "pvc": BACKUP_PVC, "localPath": BACKUP_PATH}
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.get("pvc", BACKUP_PVC, NAMESPACE).get("status", {}).get("phase") == "Bound":
                self.validate_backup_volume()
                return
            time.sleep(0.5)
        raise RuntimeError("backup PVC binding deadline exceeded")

    def validate_backup_volume(self):
        identity = self.backup_identity
        pvc = self.get("pvc", BACKUP_PVC, NAMESPACE)
        pv = self.get("pv", BACKUP_PV)
        expected_affinity = {"required": {"nodeSelectorTerms": [{"matchExpressions": [{
            "key": "kubernetes.io/hostname", "operator": "In", "values": [self.node],
        }]}]}}
        if (identity is None or identity["pvUid"] == self.pv_identity["uid"]
                or identity["pvcUid"] == self.pvc_identity["uid"] or BACKUP_PATH == LOCAL_PATH
                or pvc.get("metadata", {}).get("uid") != identity["pvcUid"]
                or pvc.get("spec", {}).get("volumeName") != BACKUP_PV
                or pvc.get("spec", {}).get("storageClassName") != ""
                or pvc.get("spec", {}).get("accessModes") != ["ReadWriteOnce"]
                or pvc.get("status", {}).get("phase") != "Bound"
                or pv.get("metadata", {}).get("uid") != identity["pvUid"]
                or pv.get("spec", {}).get("claimRef", {}).get("uid") != identity["pvcUid"]
                or pv.get("spec", {}).get("local", {}).get("path") != BACKUP_PATH
                or pv.get("spec", {}).get("storageClassName", "") != ""
                or pv.get("spec", {}).get("accessModes") != ["ReadWriteOnce"]
                or pv.get("spec", {}).get("persistentVolumeReclaimPolicy") != "Retain"
                or pv.get("spec", {}).get("nodeAffinity") != expected_affinity):
            raise RuntimeError("backup PVC/PV identity or storage contract changed")

    def validate_volume(self, pod=None, terminal=False):
        super().validate_volume(pod, terminal)
        if self.backup_identity is not None:
            self.validate_backup_volume()

    def pod_object(self, name, phase, *, read_only=False, crash=False, barrier=False):
        if phase not in OUTCOMES or crash or barrier:
            raise RuntimeError("unsupported backup probe operation")
        value = super().pod_object(name, phase, read_only=read_only)
        spec = value["spec"]
        container = spec["containers"][0]
        container["args"] = ["--exact", "pvc_coordination_phase", "--ignored", "--nocapture"]
        container["env"] = [
            {"name": "APS_COORDINATION_TEST_PHASE", "value": phase},
            {"name": "APS_COORDINATION_TEST_PATH", "value": "/store/coordination.db"},
            {"name": "APS_COORDINATION_TEST_NONCE", "value": self.nonce},
        ]
        if phase == "backup-create":
            spec["volumes"].append({"name": "backup", "persistentVolumeClaim": {
                "claimName": BACKUP_PVC, "readOnly": False}})
            container["volumeMounts"].append({"name": "backup", "mountPath": "/backup", "readOnly": False})
        elif phase == "backup-verify":
            spec["volumes"][0]["persistentVolumeClaim"]["claimName"] = BACKUP_PVC
        return value

    def execute(self):
        self.create_cluster()
        self.provision_volume()
        expected = None
        for phase in OUTCOMES:
            self.validate_volume()
            self.create_pod(phase, phase, read_only=phase in {"backup-create", "backup-verify"})
            record = parse_backup_event(self.wait_pod(phase), self.nonce, phase)
            identity = {key: record[key] for key in DIGESTS}
            if expected is not None and identity != expected:
                raise RuntimeError("backup fixture references changed across Pods")
            expected = identity
            self.events.append(record)
        if len({run["podUid"] for run in self.pod_runs}) != 4:
            raise RuntimeError("backup qualification requires four distinct Pods")

    def finish(self, success, error=None):
        status = super().finish(success, error)
        result_path = self.output / "result.json"
        result = json.loads(result_path.read_text())
        result["backupVolume"] = self.backup_identity
        result_path.write_text(json.dumps(result, indent=2) + "\n")
        return status


if __name__ == "__main__":
    sys.exit(main(runner_class=BackupRunner))
