#!/usr/bin/env python3
"""Prepare equivalent bounded controller workloads; does not build or deploy them."""
import argparse
import base64
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import tarfile

HERE = Path(__file__).resolve().parent
NAMESPACE = "reakt-test-benchmark"
CONTROL = "reakt-test-benchmark-control"


def obj(kind, name, body=None, api="v1"):
    return {"apiVersion": api, "kind": kind, "metadata": {"name": name, "namespace": NAMESPACE}, **(body or {})}


def deployment(name, image, annotations=None):
    return obj("Deployment", name, {"spec": {"replicas": 0, "selector": {"matchLabels": {"app": name}},
        "template": {"metadata": {"labels": {"app": name}, "annotations": annotations or {}},
                     "spec": {"containers": [{"name": "fixture", "image": image}]}}}}, "apps/v1")


def archive(objects):
    """Standard tar/gzip, fixed metadata. The Flux artifact is not a controller image."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as output:
        files = {"resources.json": json.dumps({"apiVersion": "v1", "kind": "List", "items": objects}, sort_keys=True)}
        files["kustomization.yaml"] = 'apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n- resources.json\n'
        for name, data in sorted(files.items()):
            content = data.encode()
            entry = tarfile.TarInfo(name)
            entry.size, entry.mode, entry.mtime = len(content), 0o644, 0
            output.addfile(entry, io.BytesIO(content))
    return gzip.compress(buffer.getvalue(), mtime=0)


def source(name, managed, image):
    desired = deployment(name, image)
    resources = ['<Deployment id="target" metadata={' + json.dumps(desired["metadata"]) +
                 '} spec={' + json.dumps(desired["spec"]) +
                 '} deletionPolicy="delete" rolloutOn={[input.data]} />']
    for i in range(managed):
        resources.append('<ConfigMap id="managed-' + str(i) + '" metadata={' +
                         json.dumps({"name": f"{name}-managed-{i}", "namespace": NAMESPACE}) +
                         '} data={{value:"stable"}} deletionPolicy="delete" />')
    options = {"id": name, "namespace": NAMESPACE, "controlNamespace": CONTROL,
               "clusterBinding": "reakt-disposable-benchmark", "policyRef": "platform/" + name,
               "enforcement": "api-bound", "budgets": {"maxResources": managed + 2, "maxInputs": 1,
               "maxReactions": 1, "maxObjectBytes": 262144, "maxQueueDepth": 128,
               "maxConcurrentRequests": 1}}
    lines = ['import {ConfigMap, Controller, Deployment, useConfigMap} from "@reakt/kubernetes";',
             'function Workload() {',
             'const input = useConfigMap(' + json.dumps({"id": "input", "name": name + "-input", "namespace": NAMESPACE}) + ');',
             'return <>' + ''.join(resources) + '</>;',
             '}',
             'export default <Controller {...' + json.dumps(options) + '}><Workload /></Controller>;']
    return "\n".join(lines) + "\n"


def prepare(output, domains, managed, image, server_image, interval):
    for ref in [image, server_image]:
        if not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", ref):
            raise ValueError("both fixture and Python artifact-server images must be immutable digest references")
    if not 1 <= domains <= 32 or not 0 <= managed <= 128 or not 1 <= interval <= 30:
        raise ValueError("bounded profile requires 1..32 domains, 0..128 managed objects, interval 1..30 seconds")
    output.mkdir()
    pins = json.loads((HERE / "pins.json").read_text())
    common, flux, statuses, binaries = [], [], [], {}
    for index in range(domains):
        name = f"bench-{index}"
        root = output / name
        root.mkdir()
        (root / "program.tsx").write_text(source(name, managed, image))
        input_object = obj("ConfigMap", name + "-input", {"data": {"revision": "initial"}})
        input_object["metadata"]["labels"] = {"reconcile.fluxcd.io/watch": "Enabled"}
        common.append(input_object)
        desired = [deployment(name, image, {"benchmark.reakt.dev/revision": "${revision}"})]
        desired += [obj("ConfigMap", f"{name}-managed-{i}", {"data": {"value": "stable"}}) for i in range(managed)]
        payload = archive(desired)
        (root / "artifact.tar.gz").write_bytes(payload)
        binaries[name + ".tar.gz"] = base64.b64encode(payload).decode()
        digest = "sha256:" + hashlib.sha256(payload).hexdigest()
        flux.append(obj("GitRepository", name, {"spec": {"interval": "24h", "url": "https://invalid.example/benchmark-only", "suspend": True}}, "source.toolkit.fluxcd.io/v1"))
        statuses.append({"name": name, "status": {"artifact": {"path": name + ".tar.gz", "url": f"http://benchmark-artifacts.{NAMESPACE}.svc:8000/{name}.tar.gz", "revision": digest, "digest": digest,
                         "lastUpdateTime": "2026-09-05T00:00:00Z"}}})
        flux.append(obj("Kustomization", name, {"spec": {"interval": f"{interval}s", "retryInterval": f"{interval}s", "timeout": "30s", "path": "./", "prune": True,
            "sourceRef": {"kind": "GitRepository", "name": name}, "postBuild": {"substituteFrom": [{"kind": "ConfigMap", "name": name + "-input"}]}}}, "kustomize.toolkit.fluxcd.io/v1"))
    flux += controller_objects(pins["kustomizeController"]["image"], server_image, binaries)
    for name, value in [("inputs.json", common), ("flux.json", flux)]:
        (output / name).write_text(json.dumps({"apiVersion": "v1", "kind": "List", "items": value}, indent=2))
    (output / "source-status.json").write_text(json.dumps(statuses, indent=2))
    sequence = [{"iteration": iteration, "domain": f"bench-{index}", "value": f"benchmark-{iteration:04d}-{index:02d}"}
                for iteration in range(100) for index in range(domains)]
    profile = {"schemaVersion": "reakt.dev/benchmark-profile/v1", "domains": domains, "managedConfigMapsPerDomain": managed,
               "inputConfigMapsPerDomain": 1, "deploymentsPerDomain": 1, "workloadReplicas": 0, "namespace": NAMESPACE,
               "controlNamespace": CONTROL, "reconcileIntervalSeconds": interval, "pins": pins,
               "fixtureImage": image, "artifactServerImage": server_image, "inputSequence": sequence,
               "reaktProvisioning": "signed candidate platform proxy staging required"}
    (output / "profile.json").write_text(json.dumps(profile, indent=2))
    return profile


def controller_objects(image, server_image, binaries):
    account = obj("ServiceAccount", "benchmark-flux")
    role = obj("Role", "benchmark-flux", {"rules": [
        {"apiGroups": ["source.toolkit.fluxcd.io"], "resources": ["gitrepositories", "ocirepositories", "buckets"], "verbs": ["get", "list", "watch"]},
        {"apiGroups": ["source.extensions.fluxcd.io"], "resources": ["externalartifacts"], "verbs": ["get", "list", "watch"]},
        {"apiGroups": ["kustomize.toolkit.fluxcd.io"], "resources": ["kustomizations", "kustomizations/status", "kustomizations/finalizers"], "verbs": ["get", "list", "watch", "patch", "update"]},
        {"apiGroups": [""], "resources": ["configmaps", "secrets"], "verbs": ["get", "list", "watch"]},
        {"apiGroups": [""], "resources": ["configmaps", "events"], "verbs": ["create", "patch", "update", "delete"]},
        {"apiGroups": ["apps"], "resources": ["deployments"], "verbs": ["get", "list", "watch", "create", "patch", "update", "delete"]},
    ]}, "rbac.authorization.k8s.io/v1")
    binding = obj("RoleBinding", "benchmark-flux", {"roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "benchmark-flux"},
        "subjects": [{"kind": "ServiceAccount", "name": "benchmark-flux", "namespace": NAMESPACE}]}, "rbac.authorization.k8s.io/v1")
    controller = deployment("benchmark-flux", image)
    controller["spec"]["replicas"] = 1
    spec = controller["spec"]["template"]["spec"]
    spec["serviceAccountName"] = "benchmark-flux"
    container = spec["containers"][0]
    container.update({"name": "manager", "args": ["--watch-all-namespaces=false", "--watch-configs-label-selector=reconcile.fluxcd.io/watch=Enabled", "--no-cross-namespace-refs=true", "--no-remote-bases=true", "--enable-leader-election=false", "--interval-jitter-percentage=0", "--concurrent=1", "--concurrent-ssa=1", "--feature-gates=ExternalArtifact=false", "--metrics-addr=:8080"],
        "env": [{"name": "RUNTIME_NAMESPACE", "value": NAMESPACE}], "ports": [{"name": "metrics", "containerPort": 8080}]})
    server = deployment("benchmark-artifacts", server_image)
    server["spec"]["replicas"] = 1
    server_spec = server["spec"]["template"]["spec"]
    server_spec["automountServiceAccountToken"] = False
    server_spec["volumes"] = [{"name": "artifacts", "configMap": {"name": "benchmark-artifacts"}}]
    server_spec["containers"][0].update({"name": "http", "command": ["python3", "-m", "http.server", "8000", "--directory", "/artifacts"],
        "volumeMounts": [{"name": "artifacts", "mountPath": "/artifacts", "readOnly": True}]})
    return [account, role, binding, controller, obj("ConfigMap", "benchmark-artifacts", {"binaryData": binaries}), server,
            obj("Service", "benchmark-artifacts", {"spec": {"selector": {"app": "benchmark-artifacts"}, "ports": [{"port": 8000, "targetPort": 8000}]}})]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--domains", type=int, default=1)
    parser.add_argument("--managed", type=int, default=8)
    parser.add_argument("--interval", type=int, default=5)
    parser.add_argument("--fixture-image", required=True)
    parser.add_argument("--artifact-server-image", required=True, help="Pinned image providing Python3 standard http.server")
    args = parser.parse_args()
    prepare(args.output, args.domains, args.managed, args.fixture_image, args.artifact_server_image, args.interval)
