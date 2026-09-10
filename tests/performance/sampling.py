"""Shared bounded collection window for the owned CNPG comparison runners."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

import run_reproducibility as shared
from performance import collect

ROOT = Path(__file__).resolve().parents[2]
SAFE_FAILURE_REASONS = frozenset({
    'performance metadata is not a bounded regular file',
    'performance controller identity changed',
    'performance collector exceeded deadline',
    'first performance sample failed',
    'performance collector ended before first complete sample',
    'performance collector ended before stimulus',
    'performance collector failed',
    'performance collection is incomplete',
    'performance population is incomplete',
    'runtime performance counters are unavailable or inconsistent',
    'runtime sample differs from protected candidate identity',
    'performance sample timestamps are not increasing',
    'reaction completed outside sampled interval',
    'workload identity changed during HTTP startup',
    'PostgREST startup deadline exceeded',
})


def require(value, reason):
    if not value:
        raise shared.QualificationError(reason)


def read(path):
    require(path.is_file() and not path.is_symlink() and 0 < path.stat().st_size <= shared.MAX_EVIDENCE_BYTES,
            "performance metadata is not a bounded regular file")
    return path.read_bytes()


def collection_window(self, label, controllers, action=None, *, namespaces, bindings, metrics_paths=(),
                      runtime_status=(), expected_runtime=(), identity_gets=4, label_prefix="cnpg-baseline"):
    """Run the existing collector; all timestamps use the host monotonic clock."""
    require(bindings() == controllers, "performance controller identity changed")
    output = self.output / ("performance-" + label)
    paths = list(metrics_paths)
    command = [sys.executable, str(Path(__file__).parent / "collect.py"), "--kubeconfig", str(self.admin),
               "--output", str(output), "--label", label_prefix + "-" + label,
               "--samples", str(self.args.performance_samples), "--interval", str(self.args.performance_interval)]
    for namespace in namespaces:
        command += ["--namespace", namespace]
    for target in runtime_status:
        command += ["--runtime-status", target]
    for path in paths:
        command += ["--metrics-path", path]
    launched = time.monotonic()
    deadline = launched + self.args.performance_samples * (self.args.performance_interval + 120) + 60
    process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        first = output / "sample-0000" / "sample.json"
        while True:
            require(time.monotonic() < deadline, "performance collector exceeded deadline")
            try:
                sample = json.loads(read(first)) if first.exists() and first.stat().st_size else None
            except json.JSONDecodeError:
                sample = None
            if sample is not None:
                require(not sample["errors"] and sample["collectionSeconds"] >= 0, "first performance sample failed")
                break
            require(process.poll() is None, "performance collector ended before first complete sample")
            time.sleep(0.2)
        require(process.poll() is None, "performance collector ended before stimulus")
        require(bindings() == controllers, "performance controller identity changed")
        action_started = time.monotonic() - launched if action else None
        result = action() if action else None
        action_ended = time.monotonic() - launched if action else None
        require(bindings() == controllers, "performance controller identity changed")
        while process.poll() is None:
            require(time.monotonic() < deadline, "performance collector exceeded deadline")
            time.sleep(1)
        require(process.wait(timeout=10) == 0, "performance collector failed")
        require(bindings() == controllers, "performance controller identity changed")
        summary = json.loads(read(output / "summary.json"))
        samples = [json.loads(read(output / f"sample-{index:04d}" / "sample.json"))
                   for index in range(self.args.performance_samples)]
        require(summary["schemaVersion"] == "apsides.dev/performance-samples/v1"
                and summary["samples"] == self.args.performance_samples and not summary["errors"]
                and summary["namespaces"] == list(namespaces) and summary["rawMetricPaths"] == paths
                and all(not sample["errors"] for sample in samples), "performance collection is incomplete")
        require(len(list(output.glob("sample-*/sample.json"))) == self.args.performance_samples
                and all({row["namespace"] for row in sample["usage"]} >= set(namespaces)
                        and {row["podUid"] for row in sample["usage"]} >= {item["podUid"] for item in controllers.values()}
                        for sample in samples), "performance population is incomplete")
        if expected_runtime:
            diagnostics = collect.runtime_report(samples, [collect.runtime_target(target) for target in runtime_status])
            require(diagnostics["completePopulation"] and summary.get("runtimeDiagnostics") == diagnostics,
                    "runtime performance counters are unavailable or inconsistent")
        for sample in samples:
            actual = sample.get("runtime", [])
            require(len(actual) == len(expected_runtime) and all(
                all(row.get(key) == value for key, value in expected.items())
                for row, expected in zip(actual, expected_runtime)),
                "runtime sample differs from protected candidate identity")
        require(all(b["elapsedSeconds"] > a["elapsedSeconds"] for a, b in zip(samples, samples[1:])),
                "performance sample timestamps are not increasing")
        if action:
            # Child collection starts after launch; this conservative inequality
            # proves at least one sample started after the completed reaction.
            require(action_ended < samples[-1]["elapsedSeconds"], "reaction completed outside sampled interval")
        self.checks.append({"check": "performance-" + label, "directory": output.name,
                            "samples": len(samples), "controllers": controllers,
                            "observer": {"controllerIdentityChecks": 4, "kubectlGetsPerIdentityCheck": identity_gets,
                                         "runtimeStatusGetsPerSample": 3 * len(runtime_status),
                                         "controllerMetricsGetsPerSample": len(paths),
                                         "identityPolling": False, "apiRateIncludesObserverTraffic": True},
                            "actionStartedAfterLaunchSeconds": action_started,
                            "actionCompletedAfterLaunchSeconds": action_ended,
                            "completeSection26": False})
        return result
    finally:
        # The collector invokes kubectl children. Stop the entire session-owned
        # group, including children left behind if the collector already exited.
        for sig in [signal.SIGTERM, signal.SIGKILL]:
            try:
                os.killpg(process.pid, sig)
            except ProcessLookupError:
                pass
            if sig == signal.SIGTERM:
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    pass
        process.wait(timeout=10)


def wait_http(http_messages, workload_state, error_type, expected_count, expected_identity, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        require(workload_state() == expected_identity, "workload identity changed during HTTP startup")
        try:
            result = http_messages(expected_count)
        except error_type as error:
            if not (error.reason == "transport" or (error.reason == "status" and error.status == 503)):
                raise
        else:
            require(workload_state() == expected_identity, "workload identity changed during HTTP startup")
            require(time.monotonic() < deadline, "PostgREST startup deadline exceeded")
            return result
        remaining = deadline - time.monotonic()
        if remaining > 0:
            time.sleep(min(2, remaining))
    raise shared.QualificationError("PostgREST startup deadline exceeded")

