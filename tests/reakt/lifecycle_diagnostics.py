"""Optional live runtime diagnostics check for the disposable lifecycle cluster."""

from contextlib import contextmanager
import http.client
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import threading
import time


ROOT = Path(__file__).resolve().parents[2]
FIELD_MANAGER = "fixture-status-conflict"
MAX_BODY_BYTES = 64 * 1024
STATUS_KEYS = {
    "schemaVersion", "domain", "buildDigest", "generation", "phase", "cache",
    "blockedTargets", "conditions", "fieldOrVersionConflicts", "lastReasonCode",
    "reconciliations", "watchRecoveries", "mutations", "errors", "applyCacheEntries",
    "applyCacheSerializedBytes", "pendingCoalescedTriggers", "triggerToCycleMicrosCount",
    "triggerToCycleMicrosSum", "triggerToCycleMicrosMax",
}
COUNTER_KEYS = STATUS_KEYS - {
    "schemaVersion", "domain", "buildDigest", "generation", "phase", "cache",
    "conditions", "lastReasonCode",
}


def _controller_container(items):
    controllers = [item for item in items if item.get("name") == "controller"]
    if len(controllers) != 1:
        raise RuntimeError("runtime Pod lacks exactly one controller container")
    return controllers[0]


def _controller_owner(value, kind):
    owners = [owner for owner in value.get("metadata", {}).get("ownerReferences", [])
              if owner.get("controller") is True and owner.get("kind") == kind]
    if len(owners) != 1 or not owners[0].get("name") or not owners[0].get("uid"):
        raise RuntimeError(f"runtime {value.get('kind', 'object')} lacks exact {kind} ownership")
    return owners[0]


def _active_allocation(runner, allocation):
    registry = runner.get("configmap", "reakt-registry-" + runner.workload, runner.control)
    try:
        state = json.loads(registry["data"]["state.json"])
        active = state["histories"][runner.domain]["activeAllocation"]
    except (KeyError, TypeError, ValueError) as error:
        raise RuntimeError("active runtime allocation is unavailable") from error
    if (active.get("generationId") != allocation.get("generation")
            or active.get("transitionId") != allocation.get("transition")):
        raise RuntimeError("runtime diagnostics allocation is not the active generation")
    required = ["artifactDigest", "generationId", "serviceAccountName", "transitionId"]
    if any(not isinstance(active.get(key), str) or not active[key] for key in required):
        raise RuntimeError("active runtime allocation identity is incomplete")
    metadata = registry.get("metadata", {})
    registry_uid = metadata.get("uid")
    registry_version = metadata.get("resourceVersion")
    if (registry_uid != runner.registry_uid or not isinstance(registry_version, str)
            or not registry_version or registry_version == "0"):
        raise RuntimeError("active runtime registry identity is invalid")
    return active, registry_uid, registry_version


def _runtime_identity(runner, candidate, allocation):
    active, registry_uid, registry_version = _active_allocation(runner, allocation)
    deployment = runner.get("deployment", active["serviceAccountName"], runner.control)
    if deployment is None:
        raise RuntimeError("active runtime Deployment is absent")
    expected_image = json.loads((candidate / "release.json").read_text())["image"]["reference"]
    expected_account = active["serviceAccountName"] + "-process"
    generation = active["generationId"]
    selector = "reakt.dev/generation=" + generation
    pods = json.loads(runner.k("get", "pods", "-n", runner.control, "-l", selector,
                               "-o", "json").stdout).get("items", [])
    running = [pod for pod in pods if pod.get("status", {}).get("phase") == "Running"]
    if len(pods) != 1 or len(running) != 1:
        raise RuntimeError("active generation does not have exactly one running runtime Pod")
    pod = running[0]
    replica_owner = _controller_owner(pod, "ReplicaSet")
    replica = runner.get("replicaset", replica_owner["name"], runner.control)
    deployment_owner = _controller_owner(replica or {}, "Deployment")
    replica_uid = (replica or {}).get("metadata", {}).get("uid")
    metadata = deployment.get("metadata", {})
    spec_container = _controller_container(pod.get("spec", {}).get("containers", []))
    status_container = _controller_container(pod.get("status", {}).get("containerStatuses", []))
    pod_uid = pod.get("metadata", {}).get("uid")
    reported_image = status_container.get("image")
    image_id = status_container.get("imageID")
    container_id = status_container.get("containerID")
    restart_count = status_container.get("restartCount")
    if (replica_uid != replica_owner["uid"]
            or deployment_owner["name"] != metadata.get("name")
            or deployment_owner["uid"] != metadata.get("uid")
            or pod.get("spec", {}).get("serviceAccountName") != expected_account
            or pod.get("metadata", {}).get("labels", {}).get("reakt.dev/generation") != generation
            or spec_container.get("image") != expected_image
            or not isinstance(reported_image, str) or not reported_image
            or not isinstance(pod_uid, str) or not pod_uid
            or not isinstance(image_id, str) or not image_id
            or not isinstance(container_id, str) or not container_id
            or isinstance(restart_count, bool) or not isinstance(restart_count, int)
            or restart_count < 0
            or "@" not in expected_image or not image_id.endswith(expected_image.split("@", 1)[1])):
        raise RuntimeError("runtime Pod identity does not match the active generation")
    return {
        "pod": pod["metadata"]["name"], "podUid": pod_uid,
        "ownerDeploymentUid": metadata["uid"], "generation": generation,
        "serviceAccount": expected_account, "image": expected_image, "imageID": image_id,
        "reportedImage": reported_image,
        "containerID": container_id, "restartCount": restart_count,
        "buildDigest": active["artifactDigest"],
        "registryUid": registry_uid, "registryResourceVersion": registry_version,
    }


class _DiagnosticsClient:
    def __init__(self, port):
        self.port = port

    def get(self, path):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        try:
            connection.request("GET", path, headers={"Accept": "application/json"})
            response = connection.getresponse()
            body = response.read(MAX_BODY_BYTES + 1)
            if len(body) > MAX_BODY_BYTES:
                raise RuntimeError("runtime diagnostics response exceeds 64 KiB")
            try:
                value = json.loads(body)
            except (TypeError, ValueError) as error:
                raise RuntimeError("runtime diagnostics response is not JSON") from error
            if not isinstance(value, dict):
                raise RuntimeError("runtime diagnostics response is not an object")
            return response.status, value
        finally:
            connection.close()


def _await_forward(process, timeout=15):
    if process.stdout is None:
        raise RuntimeError("kubectl port-forward output is unavailable")
    deadline = time.monotonic() + timeout
    output = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("kubectl port-forward exited before readiness")
            for key, _ in selector.select(min(0.25, deadline - time.monotonic())):
                chunk = os.read(key.fileobj.fileno(), 4096)
                if not chunk:
                    raise RuntimeError("kubectl port-forward closed before readiness")
                output.extend(chunk)
                if len(output) > 8192:
                    raise RuntimeError("kubectl port-forward readiness output exceeds its bound")
                match = re.search(rb"Forwarding from 127\.0\.0\.1:(\d+) -> 8080", output)
                if match:
                    port = int(match.group(1))
                    if 1 <= port <= 65535:
                        return port, len(output)
    raise RuntimeError("kubectl port-forward readiness timed out")


class _ForwardOutput:
    def __init__(self, stream, initial_bytes):
        self.stream = stream
        self.total = initial_bytes
        self.error = None
        self.thread = threading.Thread(target=self._drain, daemon=True)

    def _drain(self):
        try:
            while chunk := self.stream.read(4096):
                self.total += len(chunk)
        except (OSError, ValueError) as error:
            self.error = error

    def start(self):
        self.thread.start()

    def finish(self):
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise RuntimeError("kubectl port-forward output drain did not terminate")
        if self.error is not None:
            raise RuntimeError("kubectl port-forward output drain failed") from self.error
        if self.total > 8192:
            raise RuntimeError("kubectl port-forward output exceeds its bound")


def _reap(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def _finish_forward(process, drain):
    cleanup_error = None
    try:
        _reap(process)
    except Exception as error:
        cleanup_error = error
    finally:
        try:
            if drain is not None:
                drain.finish()
        except Exception as error:
            if cleanup_error is None:
                cleanup_error = error
        finally:
            try:
                if process.stdout is not None:
                    process.stdout.close()
            except Exception as error:
                if cleanup_error is None:
                    cleanup_error = error
    if cleanup_error is not None:
        raise cleanup_error


@contextmanager
def _forwarded_diagnostics(runner, identity):
    process = subprocess.Popen(
        ["kubectl", "--kubeconfig", str(runner.admin), "-n", runner.control,
         "port-forward", "--address=127.0.0.1", "pod/" + identity["pod"], ":8080"],
        cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0,
    )
    drain = None
    try:
        port, initial_bytes = _await_forward(process)
        drain = _ForwardOutput(process.stdout, initial_bytes)
        drain.start()
        yield _DiagnosticsClient(port)
    finally:
        _finish_forward(process, drain)


def _validated_status(client, runner, identity):
    code, status = client.get("/status")
    if code != 200 or set(status) != STATUS_KEYS:
        raise RuntimeError("runtime status endpoint shape or HTTP code differs")
    expected = {
        "schemaVersion": "reakt.dev/runtime-status/v1", "domain": runner.domain,
        "buildDigest": identity["buildDigest"], "generation": identity["generation"],
    }
    if any(status.get(key) != value for key, value in expected.items()):
        raise RuntimeError("runtime status is not bound to the active generation")
    if any(isinstance(status.get(key), bool) or not isinstance(status.get(key), int)
           or status[key] < 0 for key in COUNTER_KEYS):
        raise RuntimeError("runtime status counters are invalid")
    return status


def _ready(status):
    return (status["phase"] == "ready" and status["cache"] == "synchronized"
            and status["blockedTargets"] == 0 and status["conditions"] == []
            and status["fieldOrVersionConflicts"] == 0
            and status["lastReasonCode"] is None)


def _conflicted(status, resource_id):
    condition = {"reasonCode": "field-or-version-conflict", "resourceId": resource_id}
    return (status["phase"] == "degraded" and status["cache"] == "synchronized"
            and status["blockedTargets"] == 1 and status["conditions"] == [condition]
            and status["fieldOrVersionConflicts"] == 1
            and status["lastReasonCode"] == "field-or-version-conflict")


def _wait_status(client, runner, identity, predicate, minimum_reconciliations, reason):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        status = _validated_status(client, runner, identity)
        if status["reconciliations"] >= minimum_reconciliations and predicate(status):
            return status
        time.sleep(0.25)
    raise RuntimeError("runtime diagnostics did not report " + reason)


def _probe_health(client, ready_code):
    ready = client.get("/readyz")
    live = client.get("/livez")
    if ready != (ready_code, {"ready": ready_code == 200}) or live != (200, {"live": True}):
        raise RuntimeError("runtime readiness or liveness HTTP response differs")
    return {"status": 200, "readyz": ready[0], "livez": live[0]}


def _status_evidence(status):
    return {key: status[key] for key in [
        "phase", "cache", "blockedTargets", "conditions", "fieldOrVersionConflicts",
        "lastReasonCode", "reconciliations", "errors",
    ]}


def _retained_resource(candidate):
    ir = json.loads((candidate / "ir.json").read_text())
    if ir.get("inputs") != []:
        raise RuntimeError("runtime diagnostics fixture requires no inputs")
    retained = [resource for resource in ir.get("resources", [])
                if resource.get("id") == "retained" and resource.get("name") == "retained"
                and resource.get("kind") == "ConfigMap"]
    if len(retained) != 1 or retained[0].get("force") is True:
        raise RuntimeError("runtime diagnostics requires one non-force retained ConfigMap")
    data = retained[0].get("desired", {}).get("data")
    if not isinstance(data, dict) or not data:
        raise RuntimeError("runtime diagnostics retained data is missing")
    return retained[0], data


def _apply_fixture_field(runner, data):
    value = {"apiVersion": "v1", "kind": "ConfigMap",
             "metadata": {"name": "retained", "namespace": runner.workload}}
    if data is not None:
        value["data"] = data
    result = runner.k("apply", "--server-side", "--field-manager=" + FIELD_MANAGER,
                      *(["--force-conflicts"] if data is not None else []),
                      "-f", "-", "-o", "json", data=json.dumps(value))
    return json.loads(result.stdout)


def verify_runtime_diagnostics(runner, candidate, allocation):
    """Verify live baseline, SSA conflict reporting, and recovery on retained."""
    resource, desired_data = _retained_resource(candidate)
    original = runner.get("configmap", "retained", runner.workload)
    if original is None or original.get("data") != desired_data:
        raise RuntimeError("retained ConfigMap is not at the expected baseline")
    original_uid = original.get("metadata", {}).get("uid")
    identity = _runtime_identity(runner, candidate, allocation)
    with _forwarded_diagnostics(runner, identity) as client:
        baseline = _wait_status(client, runner, identity, _ready, 1, "the ready baseline")
        baseline_codes = _probe_health(client, 200)

        foreign = _apply_fixture_field(runner, {"value": "runtime-diagnostics-foreign"})
        if foreign.get("metadata", {}).get("uid") != original_uid:
            raise RuntimeError("SSA conflict fixture replaced the retained ConfigMap")
        first = _wait_status(
            client, runner, identity, lambda value: _conflicted(value, resource["id"]),
            baseline["reconciliations"] + 1, "the first SSA conflict cycle",
        )
        first_codes = _probe_health(client, 503)
        second = _wait_status(
            client, runner, identity,
            lambda value: (_conflicted(value, resource["id"])
                           and value["errors"] > first["errors"]),
            first["reconciliations"] + 1, "a second SSA conflict cycle",
        )
        second_codes = _probe_health(client, 503)

        relinquished = _apply_fixture_field(runner, None)
        if relinquished.get("metadata", {}).get("uid") != original_uid:
            raise RuntimeError("SSA field relinquishment replaced the retained ConfigMap")
        recovered = _wait_status(
            client, runner, identity, _ready, second["reconciliations"] + 1,
            "ready recovery after SSA field relinquishment",
        )
        recovery_codes = _probe_health(client, 200)

    restored = runner.get("configmap", "retained", runner.workload)
    if (restored is None or restored.get("metadata", {}).get("uid") != original_uid
            or restored.get("data") != desired_data):
        raise RuntimeError("runtime recovery did not preserve UID and restore exact retained data")
    if _runtime_identity(runner, candidate, allocation) != identity:
        raise RuntimeError("runtime Pod identity changed during diagnostics verification")
    evidence = {
        "check": "runtime-diagnostics-ssa-conflict-recovery", "passed": True,
        **{key: identity[key] for key in ["generation", "podUid", "ownerDeploymentUid",
                                          "serviceAccount", "image", "imageID",
                                          "containerID", "restartCount", "buildDigest",
                                          "registryUid", "registryResourceVersion"]},
        "baseline": {**baseline_codes, **_status_evidence(baseline)},
        "firstConflict": {**first_codes, **_status_evidence(first)},
        "secondConflict": {**second_codes, **_status_evidence(second)},
        "recovery": {**recovery_codes, **_status_evidence(recovered)},
        "originalUidPreserved": True, "exactDataRestored": True,
        "statusFieldsReported": ["conditions", "fieldOrVersionConflicts", "lastReasonCode"],
    }
    runner.results.append(evidence)
    (runner.output / "steps.json").write_text(json.dumps(runner.results, indent=2))
    return evidence
