#!/usr/bin/env python3
"""Install the prepared baseline only into an explicitly marked disposable cluster."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import urllib.request

from collect import command
from generate import CONTROL, HERE, NAMESPACE


def verify_target(kubeconfig, expected_uid):
    actual = json.loads(command(kubeconfig, "get", "namespace", "kube-system", "-o", "json"))["metadata"]["uid"]
    marker = json.loads(command(kubeconfig, "-n", CONTROL, "get", "configmap", "reakt-test-support", "-o", "json"))
    state = json.loads(marker["data"]["state.json"])
    expected = {"schemaVersion": "reakt.dev/disposable-test-cluster/v1", "clusterBinding": "reakt-disposable-benchmark",
                "controlNamespace": CONTROL, "workloadNamespace": NAMESPACE, "kubeSystemNamespaceUid": expected_uid}
    if actual != expected_uid or state != expected or not marker["metadata"].get("uid"):
        raise ValueError("actual cluster UID and explicit disposable benchmark marker do not match")


def install(args):
    profile = json.loads((args.profile / "profile.json").read_text())
    if profile["namespace"] != NAMESPACE or profile["controlNamespace"] != CONTROL:
        raise ValueError("unexpected benchmark namespace")
    if profile["pins"] != json.loads((HERE / "pins.json").read_text()):
        raise ValueError("prepared baseline pins differ from the reviewed benchmark pins")
    verify_target(args.kubeconfig, args.cluster_uid)
    for pin in profile["pins"]["crds"]:
        content = urllib.request.urlopen(pin["url"], timeout=30).read(1048577)
        if len(content) > 1048576 or hashlib.sha256(content).hexdigest() != pin["sha256"]:
            raise ValueError("official CRD bytes differ from pinned profile")
        verify_target(args.kubeconfig, args.cluster_uid)
        subprocess.run(["kubectl", "--kubeconfig", str(args.kubeconfig), "apply", "--server-side", "-f", "-"], input=content, check=True, timeout=60)
    for filename in ["inputs.json", "flux.json"]:
        verify_target(args.kubeconfig, args.cluster_uid)
        command(args.kubeconfig, "apply", "-f", str(args.profile / filename))
    for source in json.loads((args.profile / "source-status.json").read_text()):
        verify_target(args.kubeconfig, args.cluster_uid)
        command(args.kubeconfig, "-n", NAMESPACE, "patch", "gitrepository", source["name"], "--subresource=status", "--type=merge", "-p", json.dumps({"status": source["status"]}))
    command(args.kubeconfig, "-n", NAMESPACE, "rollout", "status", "deployment/benchmark-flux", "--timeout=120s")
    command(args.kubeconfig, "-n", NAMESPACE, "rollout", "status", "deployment/benchmark-artifacts", "--timeout=120s")
    for index in range(profile["domains"]):
        command(args.kubeconfig, "-n", NAMESPACE, "wait", "kustomization/bench-" + str(index), "--for=condition=ready", "--timeout=120s")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kubeconfig", type=Path, required=True)
    parser.add_argument("--cluster-uid", required=True)
    parser.add_argument("--profile", type=Path, required=True)
    install(parser.parse_args())
