#!/usr/bin/env python3
"""Qualify current coordination records across Pods on a local PV."""
import json
import sys
import time

from run_storage_restart import NAMESPACE, StorageRunner, main, valid_digest


FIELDS = {"schemaVersion", "nonce", "phase", "outcome", "claimDigest",
          "reviewDigest", "auditDigest", "claimCount", "reviewResourceCount",
          "auditEventCount"}
PREFIX = "APS_COORDINATION_RESULT="
RACE_DIGESTS = {"claimDigest", "leftReviewDigest", "rightReviewDigest",
                "winnerAuditDigest", "loserAuditDigest"}


def parse_race_event(output, nonce, phase, ready=False):
    prefix = "APS_COORDINATION_RACE_READY=" if ready else "APS_COORDINATION_RACE_RESULT="
    lines = [line for line in output.splitlines() if line.startswith(prefix)]
    if len(output.encode()) > 64 * 1024 or len(lines) != 1 or len(lines[0].encode()) > 4096:
        raise RuntimeError("coordination race event count or size differs")
    try:
        record = json.loads(lines[0][len(prefix):])
    except ValueError as error:
        raise RuntimeError("coordination race event is malformed") from error
    fields = {"schemaVersion", "nonce", "phase", "outcome"}
    if not ready:
        fields |= RACE_DIGESTS
    outcomes = {"ready"} if ready else ({"verified"} if phase == "verify-race"
                                       else {"published", "conflict"})
    if (not isinstance(record, dict) or set(record) != fields
            or record["schemaVersion"] != "reakt.dev/pvc-coordination-race/v1"
            or record["nonce"] != nonce or record["phase"] != phase
            or phase not in ({"race-left", "race-right"} if ready else
                             {"race-left", "race-right", "verify-race"})
            or record["outcome"] not in outcomes
            or not ready and any(not valid_digest(record[key]) for key in RACE_DIGESTS)):
        raise RuntimeError("coordination race event is invalid")
    if not ready and (record["leftReviewDigest"] == record["rightReviewDigest"]
                      or record["winnerAuditDigest"] == record["loserAuditDigest"]):
        raise RuntimeError("coordination race candidates are not distinct")
    if "private" in output.lower() or "desired" in output.lower():
        raise RuntimeError("coordination race emitted a private-field canary")
    if not ready and (output.count("test pvc_coordination_phase ... ok") != 1
                     or output.count("test result: ok. 1 passed; 0 failed; 0 ignored") != 1):
        raise RuntimeError("coordination race test did not execute exactly once")
    return record


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
    SCOPE = "current-coordination-local-pv-reopen-and-competing-writers"
    BOUNDARY = "current coordination records and competing processes on a static local PV; no cross-store atomicity, transaction-overlap, backup, crash, CSI, node-failure, Turso, or custody-transfer claim"

    def pod_object(self, name, phase, *, read_only=False, crash=False, barrier=False):
        if crash or phase not in {"seed", "verify", "race-left", "race-right", "verify-race"}:
            raise RuntimeError("unsupported coordination probe operation")
        value = super().pod_object(name, phase, read_only=read_only)
        container = value["spec"]["containers"][0]
        container["args"] = ["--exact", "pvc_coordination_phase", "--ignored", "--nocapture"]
        container["env"] = [
            {"name": "APS_COORDINATION_TEST_PHASE", "value": phase},
            {"name": "APS_COORDINATION_TEST_PATH", "value": "/store/coordination.db"},
            {"name": "APS_COORDINATION_TEST_NONCE", "value": self.nonce},
        ]
        if barrier:
            container["command"] = ["/bin/sh", "-c"]
            container["args"] = ["set -eu; test ! -e /store/coordination-race-go; umask 077; : > /store/coordination-race-go; sync"]
            container["env"] = []
        return value

    def wait_race_ready(self, phase):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            pod = self.get("pod", phase, NAMESPACE)
            self.validate_pod(pod, phase, False)
            result = self.k("logs", phase, "-n", NAMESPACE, "--limit-bytes=65537",
                            check=False, timeout=30)
            if result.returncode == 0 and "APS_COORDINATION_RACE_READY=" in result.stdout:
                return parse_race_event(result.stdout, self.nonce, phase, ready=True)
            if pod.get("status", {}).get("phase") in {"Succeeded", "Failed"}:
                raise RuntimeError("coordination writer exited before its barrier")
            time.sleep(0.5)
        raise RuntimeError("coordination race readiness deadline exceeded")

    def race(self):
        phases = ["race-left", "race-right"]
        for phase in phases:
            self.create_pod(phase, phase)
        for phase in phases:
            self.wait_race_ready(phase)
            self.validate_volume(phase)
        self.create_pod("race-barrier", "verify", barrier=True)
        self.wait_pod("race-barrier")
        records = [parse_race_event(self.wait_pod(phase), self.nonce, phase)
                   for phase in phases]
        if sorted(record["outcome"] for record in records) != ["conflict", "published"]:
            raise RuntimeError("coordination audit race did not produce one winner and conflict")
        expected = {key: records[0][key] for key in RACE_DIGESTS}
        if any(record[key] != expected[key] for record in records for key in RACE_DIGESTS):
            raise RuntimeError("coordination race references differ")
        self.events.extend(records)
        self.create_pod("verify-race", "verify-race", read_only=True)
        verified = parse_race_event(self.wait_pod("verify-race"), self.nonce, "verify-race")
        if any(verified[key] != expected[key] for key in RACE_DIGESTS):
            raise RuntimeError("coordination post-race references differ")
        self.events.append(verified)

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
        self.race()
        if len({run["podUid"] for run in self.pod_runs}) != 6:
            raise RuntimeError("coordination qualification requires six distinct Pods")


if __name__ == "__main__":
    sys.exit(main(runner_class=CoordinationRunner))
