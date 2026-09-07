#!/usr/bin/env python3
"""Collect raw Kubernetes performance evidence; unsupported metrics remain unavailable."""
import argparse
import json
import re
from pathlib import Path
import subprocess
import time


def command(kubeconfig, *args):
    result = subprocess.run(["kubectl", "--kubeconfig", str(kubeconfig), *args], capture_output=True, text=True, timeout=30, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr[-2000:])
    return result.stdout


def container_usage(summary, namespaces):
    result = []
    for pod in summary.get("pods", []):
        if pod["podRef"]["namespace"] not in namespaces:
            continue
        for container in pod.get("containers", []):
            result.append({"namespace": pod["podRef"]["namespace"], "pod": pod["podRef"]["name"], "podUid": pod["podRef"]["uid"],
                "container": container["name"], "containerStartTime": container.get("startTime"),
                "cpuCoreNanoSeconds": container.get("cpu", {}).get("usageCoreNanoSeconds"),
                "cpuNanoCores": container.get("cpu", {}).get("usageNanoCores"),
                "rssBytes": container.get("memory", {}).get("rssBytes"), "workingSetBytes": container.get("memory", {}).get("workingSetBytes")})
    return result


def cpu_delta(first, last, duration):
    """Never attribute a reset/replaced container counter to a CPU improvement."""
    def key(row):
        return tuple(row[field] for field in ["namespace", "pod", "podUid", "container", "containerStartTime"])
    before = {key(row): row for row in first}
    output = []
    for row in last:
        original = before.get(key(row))
        start = original.get("cpuCoreNanoSeconds") if original else None
        end = row.get("cpuCoreNanoSeconds")
        valid = isinstance(start, int) and isinstance(end, int) and end >= start and duration > 0 and row["containerStartTime"] is not None
        output.append({**row, "meanCpuCores": (end - start) / 1e9 / duration if valid else None,
                       "cpuStatus": "measured" if valid else "unavailable: missing, reset, or replaced counter"})
    removed = set(before) - {key(row) for row in last}
    return {"containers": output, "removedContainers": [list(item) for item in sorted(removed, key=str)],
            "completePopulation": bool(before) and bool(output) and not removed and all(row["cpuStatus"] == "measured" for row in output)}



RUNTIME_COUNTERS = ("triggerToCycleMicrosCount", "triggerToCycleMicrosSum", "triggerToCycleMicrosMax")
RUNTIME_GAUGES = ("applyCacheEntries", "applyCacheSerializedBytes", "pendingCoalescedTriggers")
IDENTITY = ("domain", "buildDigest", "generation", "podUid", "containerId", "restartCount")


def runtime_target(value):
    """Explicit domain and Pod proxy route; never accept arbitrary diagnostic URLs."""
    domain, separator, path = value.partition("=")
    match = re.fullmatch(r"/api/v1/namespaces/([a-z0-9][a-z0-9.-]*)/pods/([a-z0-9][a-z0-9.-]*)(?::([a-z0-9-]+))?/proxy/status", path)
    if not separator or not re.fullmatch(r"[a-z0-9][a-z0-9.-]{0,252}", domain) or not match:
        raise ValueError("runtime status requires DOMAIN=/api/v1/namespaces/NS/pods/POD:PORT/proxy/status")
    return {"domain": domain, "path": path, "namespace": match[1], "pod": match[2]}


def runtime_sample(kubeconfig, target):
    def pod_identity():
        pod = json.loads(command(kubeconfig, "get", "pod", target["pod"], "-n", target["namespace"], "-o", "json"))
        metadata = pod["metadata"]
        statuses = [x for x in pod["status"]["containerStatuses"] if x["name"] == "controller"]
        if (metadata["name"] != target["pod"] or metadata["namespace"] != target["namespace"]
                or metadata.get("deletionTimestamp") or len(statuses) != 1):
            raise ValueError("runtime Pod identity is invalid")
        status = statuses[0]
        identity = {"podUid": metadata["uid"], "containerId": status["containerID"], "restartCount": status["restartCount"]}
        if (not all(isinstance(identity[k], str) and re.fullmatch(r"[A-Za-z0-9_.:/-]{1,256}", identity[k]) for k in ["podUid", "containerId"])
                or type(identity["restartCount"]) is not int or identity["restartCount"] < 0
                or not status.get("state", {}).get("running")):
            raise ValueError("runtime container is not running with a bounded identity")
        return identity
    before = pod_identity()
    raw = command(kubeconfig, "get", "--raw", target["path"])
    if len(raw.encode()) > 1024 * 1024:
        raise ValueError("runtime status exceeds sample bound")
    status = json.loads(raw)
    after = pod_identity()
    if before != after:
        raise ValueError("runtime Pod changed while sampling")
    if (status.get("schemaVersion") != "reakt.dev/runtime-status/v1"
            or status.get("domain") != target["domain"]
            or not isinstance(status.get("buildDigest"), str)
            or not re.fullmatch(r"sha256:[a-f0-9]{64}", status["buildDigest"])
            or not isinstance(status.get("generation"), str)
            or not re.fullmatch(r"[A-Za-z0-9_.:-]{1,256}", status["generation"])):
        raise ValueError("runtime status identity is invalid")
    values = {key: status[key] for key in RUNTIME_COUNTERS + RUNTIME_GAUGES}
    if (any(type(v) is not int or not 0 <= v <= 2**64 - 1 for v in values.values())
            or values["pendingCoalescedTriggers"] > 1
            or values["triggerToCycleMicrosMax"] > values["triggerToCycleMicrosSum"]
            or (values["triggerToCycleMicrosCount"] == 0 and values["triggerToCycleMicrosSum"] != 0)):
        raise ValueError("runtime counters are invalid")
    # Deliberately exclude conditions, reason codes, cache contents, and diagnostic messages.
    return {**target, **before, **{k: status[k] for k in ["buildDigest", "generation"]}, **values}


def runtime_report(samples, targets):
    domains = []
    for target in targets:
        rows = [next((row for row in sample["runtime"] if row["domain"] == target["domain"]), None) for sample in samples]
        valid = len(rows) >= 2 and all(row is not None for row in rows)
        if valid:
            valid = all(all(row[k] == rows[0][k] for k in IDENTITY) for row in rows)
        if valid:
            valid = all(all(after[k] >= before[k] for k in RUNTIME_COUNTERS) for before, after in zip(rows, rows[1:]))
        if valid:
            count = rows[-1][RUNTIME_COUNTERS[0]] - rows[0][RUNTIME_COUNTERS[0]]
            total = rows[-1][RUNTIME_COUNTERS[1]] - rows[0][RUNTIME_COUNTERS[1]]
            valid = count > 0 or total == 0
        if not valid:
            domains.append({"domain": target["domain"], "status": "unavailable", "reason": "missing sample, changed identity, or reset counters"})
            continue
        domains.append({"domain": target["domain"], "status": "measured", "identity": {k: rows[0][k] for k in IDENTITY},
                        "applyCacheEntriesLast": rows[-1]["applyCacheEntries"],
                        "applyCacheEntriesPeak": max(row["applyCacheEntries"] for row in rows),
                        "applyCacheSerializedBytesLast": rows[-1]["applyCacheSerializedBytes"],
                        "applyCacheSerializedBytesPeak": max(row["applyCacheSerializedBytes"] for row in rows),
                        "pendingCoalescedTriggersPeak": max(row["pendingCoalescedTriggers"] for row in rows),
                        "triggerSamples": count, "triggerToCycleMicrosSum": total,
                        "triggerToCycleMicrosMean": total / count if count else None,
                        "processLifetimeTriggerToCycleMicrosMax": rows[-1]["triggerToCycleMicrosMax"]})
    return {"domains": domains, "completePopulation": bool(domains) and all(row["status"] == "measured" for row in domains),
            "cacheRepresentation": "retained apply-cache entries and serialized payload bytes; excludes allocator overhead and other caches",
            "schedulingSemantics": "coalesced trigger to cycle start; interval count/sum/mean, process-lifetime maximum; no event-queue or percentile claim"}


def collect(args):
    targets = [runtime_target(value) for value in getattr(args, "runtime_status", [])]
    if len({row["domain"] for row in targets}) != len(targets) or len({row["path"] for row in targets}) != len(targets):
        raise ValueError("runtime domains and status paths must be unique")
    if any(target["namespace"] not in args.namespace for target in targets):
        raise ValueError("runtime status namespace must be included in CPU and memory collection")
    args.output.mkdir()
    nodes = json.loads(command(args.kubeconfig, "get", "nodes", "-o", "json"))["items"]
    samples = []
    start = time.monotonic()
    for index in range(args.samples):
        stamp = time.monotonic()
        directory = args.output / f"sample-{index:04d}"
        directory.mkdir()
        usage, errors = [], []
        for node in nodes:
            name = node["metadata"]["name"]
            try:
                raw = command(args.kubeconfig, "get", "--raw", f"/api/v1/nodes/{name}/proxy/stats/summary")
                (directory / (name + ".summary.json")).write_text(raw)
                usage += container_usage(json.loads(raw), args.namespace)
            except (RuntimeError, ValueError) as error:
                errors.append({"source": name + "/stats/summary", "error": str(error)})
        try:
            (directory / "apiserver.prom").write_text(command(args.kubeconfig, "get", "--raw", "/metrics"))
        except RuntimeError as error:
            errors.append({"source": "apiserver/metrics", "error": str(error)})
        for metric_path in args.metrics_path:
            try:
                raw = command(args.kubeconfig, "get", "--raw", metric_path)
                (directory / ("controller-" + str(args.metrics_path.index(metric_path)) + ".prom")).write_text(raw)
            except RuntimeError as error:
                errors.append({"source": metric_path, "error": str(error)})
        runtime = []
        for target in targets:
            try:
                runtime.append(runtime_sample(args.kubeconfig, target))
            except (RuntimeError, ValueError, KeyError, TypeError):
                errors.append({"source": target["path"], "error": "runtime status unavailable or identity/counters invalid"})
        samples.append({"runtime": runtime, "elapsedSeconds": stamp - start, "collectionSeconds": time.monotonic() - stamp, "usage": usage, "errors": errors})
        (directory / "sample.json").write_text(json.dumps(samples[-1], indent=2))
        if index + 1 < args.samples:
            time.sleep(max(0, args.interval - (time.monotonic() - stamp)))
    duration = samples[-1]["elapsedSeconds"] - samples[0]["elapsedSeconds"]
    result = {"schemaVersion": "reakt.dev/performance-samples/v1", "label": args.label, "namespaces": args.namespace,
              "durationSeconds": duration, "samples": len(samples), "cpu": cpu_delta(samples[0]["usage"], samples[-1]["usage"], duration),
              "cacheSize": {"status": "unavailable", "reason": "Full controller cache size is unavailable; selected Apsides apply-cache measurements are in runtimeDiagnostics"},
              "reaktQueueLatency": {"status": "not-applicable", "reason": "Apsides coalesces triggers; scheduling measurements are in runtimeDiagnostics"},
              "runtimeDiagnostics": runtime_report(samples, targets), "rawMetricPaths": args.metrics_path, "errors": [e for sample in samples for e in sample["errors"]],
              "completeSection26": False}
    (args.output / "summary.json").write_text(json.dumps(result, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kubeconfig", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--namespace", action="append", required=True)
    parser.add_argument("--metrics-path", action="append", default=[], help="Exact Kubernetes Pod/Service proxy API path ending /metrics")
    parser.add_argument("--runtime-status", action="append", default=[], help="DOMAIN=/api/v1/namespaces/NS/pods/POD:PORT/proxy/status (repeat per domain)")
    parser.add_argument("--samples", type=int, default=13)
    parser.add_argument("--interval", type=float, default=5)
    args = parser.parse_args()
    if not 2 <= args.samples <= 10000 or not 0.1 <= args.interval <= 60:
        parser.error("samples must be 2..10000; interval must be 0.1..60 seconds")
    collect(args)
