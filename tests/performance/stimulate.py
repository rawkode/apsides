#!/usr/bin/env python3
"""Observe input-to-template latency, which is distinct from controller queue latency."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time

from collect import command
from generate import NAMESPACE
from install_flux import verify_target


def template(kubeconfig, name, annotation):
    value = json.loads(command(kubeconfig, "-n", NAMESPACE, "get", "deployment", name, "-o", "json"))
    return value["spec"]["template"].get("metadata", {}).get("annotations", {}).get(annotation)


def sequence_value(profile, iteration, domain):
    values = [item["value"] for item in profile["inputSequence"] if item["iteration"] == iteration and item["domain"] == domain]
    if len(values) != 1 or not isinstance(values[0], str) or not 1 <= len(values[0]) <= 128:
        raise ValueError("missing, duplicate or invalid shared input sequence entry")
    return values[0]


def expected_revision(args, name, value):
    if args.controller == "flux":
        return {"revision": value, "inputValue": value}
    if args.revision_helper is None or args.compiled_ir_root is None:
        raise ValueError("Apsides latency requires the trusted core revision helper and actual compiled per-domain IR")
    ir = args.compiled_ir_root / name / "ir.json"
    result = subprocess.run([str(args.revision_helper.resolve()), str(ir), value], capture_output=True, text=True, check=True, timeout=30)
    expected = json.loads(result.stdout)
    if expected["domainId"] != name or expected["namespace"] != NAMESPACE or expected["inputValue"] != value:
        raise ValueError("reference helper returned a different benchmark binding")
    expected["irFileSha256"] = hashlib.sha256(ir.read_bytes()).hexdigest()
    return expected


def run(args):
    args.output.mkdir()
    profile = json.loads((args.profile / "profile.json").read_text())
    profile_digest = hashlib.sha256((args.profile / "profile.json").read_bytes()).hexdigest()
    helper_digest = hashlib.sha256(args.revision_helper.read_bytes()).hexdigest() if args.revision_helper else None
    annotation = "apsides.dev/revision" if args.controller == "apsides" else "benchmark.apsides.dev/revision"
    samples = []
    for iteration in range(args.iterations):
        pending = {}
        # All domains receive one independent change before convergence polling.
        for index in range(profile["domains"]):
            verify_target(args.kubeconfig, args.cluster_uid)
            name = "bench-" + str(index)
            previous = template(args.kubeconfig, name, annotation)
            if previous is None:
                raise ValueError("initial target revision is absent")
            value = sequence_value(profile, iteration, name)
            expected = expected_revision(args, name, value)
            if previous == expected["revision"]:
                raise ValueError("input sequence was already applied; use the same fresh starting state on both clusters")
            start = time.monotonic()
            command(args.kubeconfig, "-n", NAMESPACE, "patch", "configmap", name + "-input", "--type=merge", "-p", json.dumps({"data": {"revision": value}}))
            pending[name] = {"start": start, "previous": previous, "input": value, "expected": expected}
        deadline = time.monotonic() + args.timeout
        while pending:
            for name, change in list(pending.items()):
                current = template(args.kubeconfig, name, annotation)
                converged = current == change["expected"]["revision"]
                if converged:
                    samples.append({"domain": name, "iteration": iteration, "seconds": time.monotonic() - change["start"],
                                    "inputValue": change["input"], "expected": change["expected"], "observedRevision": current,
                                    "status": "observed-exact-template-revision"})
                    del pending[name]
            if time.monotonic() > deadline:
                raise RuntimeError("dependent templates did not converge before deadline: " + ",".join(pending))
            if pending:
                time.sleep(0.1)
    (args.output / "latency.json").write_text(json.dumps({"metric": "input-write-start-to-observed-template-change", "controller": args.controller,
        "includes": ["kubectl startup", "API patch latency", "controller scheduling and reconciliation", "polling observation delay"],
        "queueLatency": False, "profileSha256": profile_digest, "revisionHelperSha256": helper_digest,
        "samples": samples}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["kubeconfig", "profile", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--cluster-uid", required=True)
    parser.add_argument("--controller", choices=["apsides", "flux"], required=True)
    parser.add_argument("--iterations", type=int, default=10)
    parser.add_argument("--revision-helper", type=Path)
    parser.add_argument("--compiled-ir-root", type=Path, help="Actual compiled bench-N/ir.json directories (required for Apsides)")
    parser.add_argument("--timeout", type=float, default=60)
    args = parser.parse_args()
    if not 1 <= args.iterations <= 100 or not 1 <= args.timeout <= 600:
        parser.error("iterations 1..100 and timeout 1..600 required")
    run(args)
