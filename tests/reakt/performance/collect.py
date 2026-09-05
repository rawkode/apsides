#!/usr/bin/env python3
"""Collect raw Kubernetes performance evidence; unsupported metrics remain unavailable."""
import argparse
import json
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


def collect(args):
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
        samples.append({"elapsedSeconds": stamp - start, "collectionSeconds": time.monotonic() - stamp, "usage": usage, "errors": errors})
        (directory / "sample.json").write_text(json.dumps(samples[-1], indent=2))
        if index + 1 < args.samples:
            time.sleep(max(0, args.interval - (time.monotonic() - stamp)))
    duration = samples[-1]["elapsedSeconds"] - samples[0]["elapsedSeconds"]
    result = {"schemaVersion": "reakt.dev/performance-samples/v1", "label": args.label, "namespaces": args.namespace,
              "durationSeconds": duration, "samples": len(samples), "cpu": cpu_delta(samples[0]["usage"], samples[-1]["usage"], duration),
              "cacheSize": {"status": "unavailable", "reason": "Neither RSS nor working set is an application cache measurement"},
              "reaktQueueLatency": {"status": "unavailable", "reason": "Current runtime does not export enqueue-to-evaluation timing"},
              "rawMetricPaths": args.metrics_path, "errors": [e for sample in samples for e in sample["errors"]],
              "completeSection26": False}
    (args.output / "summary.json").write_text(json.dumps(result, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kubeconfig", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--namespace", action="append", required=True)
    parser.add_argument("--metrics-path", action="append", default=[], help="Exact Kubernetes Pod/Service proxy API path ending /metrics")
    parser.add_argument("--samples", type=int, default=13)
    parser.add_argument("--interval", type=float, default=5)
    args = parser.parse_args()
    if not 2 <= args.samples <= 10000 or not 0.1 <= args.interval <= 60:
        parser.error("samples must be 2..10000; interval must be 0.1..60 seconds")
    collect(args)
