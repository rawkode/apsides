#!/usr/bin/env python3
"""Qualify current coordination records across Pods on a local PV."""
import json
import sys

from run_storage_restart import StorageRunner, main, valid_digest


FIELDS = {"schemaVersion", "nonce", "phase", "outcome", "claimDigest",
          "reviewDigest", "auditDigest", "claimCount", "reviewResourceCount",
          "auditEventCount"}
PREFIX = "APS_COORDINATION_RESULT="


def parse_coordination_event(output, nonce, phase):
    if len(output.encode()) > 64 * 1024:
        raise RuntimeError("coordination probe output exceeds its bound")
    lines = [line for line in output.splitlines() if line.startswith(PREFIX)]
    if len(lines) != 1 or len(lines[0].encode()) > 4096:
        raise RuntimeError("coordination probe event count or size differs")
    try:
        record = json.loads(lines[0][len(PREFIX):])
    except ValueError as error:
        raise RuntimeError("coordination probe event is malformed") from error
    if (not isinstance(record, dict) or set(record) != FIELDS
            or record["schemaVersion"] != "reakt.dev/pvc-coordination-probe/v1"
            or record["nonce"] != nonce or record["phase"] != phase
            or phase not in {"seed", "verify"}
            or record["outcome"] != {"seed": "published", "verify": "verified"}[phase]
            or any(not valid_digest(record[key]) for key in
                   ["claimDigest", "reviewDigest", "auditDigest"])
            or any(type(record[key]) is not int or not 0 <= record[key] <= 100_000
                   for key in ["claimCount", "reviewResourceCount", "auditEventCount"])
            or record["claimCount"] == 0 or record["auditEventCount"] == 0):
        raise RuntimeError("coordination probe event is invalid")
    if "private" in output.lower() or "desired" in output.lower():
        raise RuntimeError("coordination probe emitted a private-field canary")
    if (output.count("test pvc_coordination_phase ... ok") != 1
            or output.count("test result: ok. 1 passed; 0 failed; 0 ignored") != 1):
        raise RuntimeError("coordination probe test did not execute exactly once")
    return record


class CoordinationRunner(StorageRunner):
    PACKAGE = "coordination"
    RESULT_SCHEMA = "apsides.dev/coordination-restart-qualification/v1"
    SCOPE = "current-coordination-local-pv-reopen"
    BOUNDARY = "current ClaimStore, ReviewStore and AuditStore on a static local PV; no backup, concurrency, crash, CSI, node-failure, Turso, or custody-transfer claim"

    def pod_object(self, name, phase, *, read_only=False, crash=False, barrier=False):
        if crash or barrier or phase not in {"seed", "verify"}:
            raise RuntimeError("unsupported coordination probe operation")
        value = super().pod_object(name, phase, read_only=read_only)
        container = value["spec"]["containers"][0]
        container["args"] = ["--exact", "pvc_coordination_phase", "--ignored", "--nocapture"]
        container["env"] = [
            {"name": "APS_COORDINATION_TEST_PHASE", "value": phase},
            {"name": "APS_COORDINATION_TEST_PATH", "value": "/store/coordination.db"},
            {"name": "APS_COORDINATION_TEST_NONCE", "value": self.nonce},
        ]
        return value

    def execute(self):
        self.create_cluster()
        self.provision_volume()
        expected = None
        for name, phase, read_only in [("seed", "seed", False), ("verify", "verify", True)]:
            self.validate_volume()
            self.create_pod(name, phase, read_only=read_only)
            record = parse_coordination_event(self.wait_pod(name), self.nonce, phase)
            identity = {key: value for key, value in record.items()
                        if key not in {"phase", "outcome"}}
            if expected is not None and identity != expected:
                raise RuntimeError("coordination fixture changed across Pods")
            expected = identity
            self.events.append(record)
        if len({run["podUid"] for run in self.pod_runs}) != 2:
            raise RuntimeError("coordination reopen requires two distinct Pods")


if __name__ == "__main__":
    sys.exit(main(runner_class=CoordinationRunner))
