#!/usr/bin/env python3
"""Prepare one exact CNPG workload comparison for scoped Flux and Reloader."""
import argparse
import base64
import copy
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

import generate

LIMIT = 8 * 1024 * 1024
ANNOTATIONS = {"ConfigMap": "configmap.reloader.stakater.com/reload", "Secret": "secret.reloader.stakater.com/reload"}
SOURCES = {
    "reloaderFlags": "https://raw.githubusercontent.com/stakater/Reloader/392be733e05b8e52c94199263abfd0f7632faab1/internal/pkg/util/util.go",
    "reloaderSecretHash": "https://raw.githubusercontent.com/stakater/Reloader/392be733e05b8e52c94199263abfd0f7632faab1/pkg/common/reload_source.go",
    "fluxAnnotations": "https://docs.stakater.com/reloader/1.4/how-to-guides/use-reloader-with-flux.html",
}


class UnsupportedProfile(ValueError):
    """The supplied plan is outside the bounded, reviewed comparison profile."""


def require(condition, reason):
    if not condition:
        raise UnsupportedProfile(reason)


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()


def digest(raw):
    return "sha256:" + hashlib.sha256(raw).hexdigest()


def unique(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate input JSON field")
        result[key] = value
    return result


def _project(plan):
    ir = plan["ir"]
    require(ir.get("testOnly") is False and digest(canonical(ir)) == plan["irDigest"], "expected intact production IR")
    namespace = ir["domain"]["namespace"]
    require(isinstance(namespace, str) and re.fullmatch(r"[a-z0-9]([-a-z0-9]*[a-z0-9])?", namespace), "invalid workload namespace")
    resources = ir["resources"]
    expected = {("postgresql.cnpg.io/v1", "Cluster"), ("v1", "ConfigMap"), ("apps/v1", "Deployment"), ("v1", "Service")}
    require(len(resources) == 4 and {(r["apiVersion"], r["kind"]) for r in resources} == expected, "expected four CNPG managed resource types")
    require(len({r["id"] for r in resources}) == 4, "duplicate managed resource ID")
    for r in resources:
        d = r["desired"]
        require(r["namespace"] == namespace and d["metadata"]["namespace"] == namespace
                and d["metadata"]["name"] == r["name"] and d["apiVersion"] == r["apiVersion"] and d["kind"] == r["kind"]
                and r["lifecycle"] == {"deletionPolicy": "orphan"}, "unsupported resource scope, identity or lifecycle")
        require(not any("reloader.stakater.com/" in k for k in d["metadata"].get("annotations", {})), "desired resource already configures Reloader")
        if r["kind"] == "Cluster":
            require(r.get("apiResource", {}).get("plural") == "clusters" and r["apiResource"].get("scope") == "Namespaced", "unsupported CNPG API mapping")
        if r["kind"] == "ConfigMap":
            require(not d.get("binaryData"), "binary ConfigMap values are outside data-only comparison")
    target = next(r for r in resources if r["kind"] == "Deployment")
    require(target["desired"]["spec"].get("replicas") == 1, "expected one PostgREST replica")
    inputs = ir["inputs"]
    require(len(inputs) == 3 and len({x["id"] for x in inputs}) == 3
            and sorted(x["kind"] for x in inputs) == ["ConfigMap", "ConfigMap", "Secret"], "expected two ConfigMaps and one Secret")
    for x in inputs:
        require(x["namespace"] == namespace and x["apiVersion"] == "v1" and x["projection"] == ["data"]
                and x["missing"] == "block-dependent-mutations" and x["sensitive"] == (x["kind"] == "Secret")
                and x["mode"] in {"managed", "observed"}, "unsupported input projection or scope")
        if x["mode"] == "managed":
            require(any(r["id"] == x.get("managedResourceId") and r["kind"] == x["kind"] and r["name"] == x["name"] for r in resources), "managed input does not bind a resource")
    require(sum(x["mode"] == "managed" for x in inputs) == 1 and all(x["mode"] == "observed" for x in inputs if x["kind"] == "Secret"), "unsupported managed input set")
    reactions = ir["reactions"]
    require(len(reactions) == 3 and all(x["op"] == "rollout" and x["target"] == target["id"] and len(x["inputs"]) == 1 for x in reactions)
            and {x["inputs"][0] for x in reactions} == {x["id"] for x in inputs}, "unsupported reaction graph")
    desired = [copy.deepcopy(r["desired"]) for r in resources]
    deployment = next(d for d in desired if d["kind"] == "Deployment")
    annotations = deployment["metadata"].setdefault("annotations", {})
    require("reloader.stakater.com/last-reloaded-from" not in deployment["spec"]["template"].get("metadata", {}).get("annotations", {}), "desired rollout annotation would conflict with Reloader")
    for kind, annotation in ANNOTATIONS.items():
        annotations[annotation] = ",".join(sorted(x["name"] for x in inputs if x["kind"] == kind))
    public_inputs = [{k: x[k] for k in ["id", "apiVersion", "kind", "name", "namespace", "mode", "projection"]} for x in inputs]
    return desired, public_inputs


def project(plan):
    try:
        return _project(plan)
    except (KeyError, TypeError, AttributeError) as error:
        raise UnsupportedProfile("malformed or unsupported CNPG plan") from error


def scoped(objects, namespace):
    """Adapt only generator-owned controller objects, never candidate desired state."""
    def visit(value):
        if isinstance(value, dict): return {k: visit(v) for k, v in value.items()}
        if isinstance(value, list): return [visit(v) for v in value]
        return namespace if value == generate.NAMESPACE else value
    return visit(objects)


def reloader_objects(namespace, image, target):
    def obj(kind, name, body=None, api="v1"):
        return {"apiVersion": api, "kind": kind, "metadata": {"name": name, "namespace": namespace}, **(body or {})}
    name = "benchmark-reloader"
    rules = [
        {"apiGroups": [""], "resources": ["configmaps", "secrets"], "verbs": ["get", "list", "watch"]},
        {"apiGroups": ["apps"], "resources": ["deployments", "daemonsets", "statefulsets"], "verbs": ["get", "list"]},
        {"apiGroups": ["apps"], "resources": ["deployments"], "resourceNames": [target], "verbs": ["patch", "update"]},
        {"apiGroups": [""], "resources": ["events"], "verbs": ["create", "patch", "update"]},
    ]
    deployment = scoped([generate.deployment(name, image)], namespace)[0]
    deployment["spec"]["replicas"] = 1
    pod = deployment["spec"]["template"]["spec"]
    pod["serviceAccountName"] = name
    pod["containers"][0].update(name="reloader", args=["--reload-strategy=annotations", "--namespaces=" + namespace,
        "--ignored-workload-types=jobs,cronjobs", "--auto-reload-all=false", "--reload-on-create=false", "--reload-on-delete=false",
        "--enable-ha=false", "--enable-csi-integration=false", "--log-level=warning"], ports=[{"name": "metrics", "containerPort": 9090}])
    return [obj("ServiceAccount", name), obj("Role", name, {"rules": rules}, "rbac.authorization.k8s.io/v1"),
            obj("RoleBinding", name, {"roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": name},
                "subjects": [{"kind": "ServiceAccount", "name": name, "namespace": namespace}]}, "rbac.authorization.k8s.io/v1"), deployment]


def prepare(plan_path, expected_digest, output, server_image, interval=1):
    require(plan_path.is_file() and not plan_path.is_symlink() and 0 < plan_path.stat().st_size <= LIMIT, "plan must be a bounded regular file")
    raw = plan_path.read_bytes()
    require(digest(raw) == expected_digest, "source plan digest differs")
    plan = json.loads(raw, object_pairs_hook=unique)
    desired, inputs = project(plan)
    require(re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", server_image) and type(interval) is int and interval == 1, "baseline requires pinned server image and current runtime one-second interval")
    pins = json.loads((generate.HERE / "pins.json").read_bytes(), object_pairs_hook=unique)
    require(pins["reloader"]["sourceCommit"] == "392be733e05b8e52c94199263abfd0f7632faab1"
            and pins["kustomizeController"]["sourceCommit"] == "1324b615da01038e2859cbcde604bd5434f21933"
            and all(re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", pins[k]["image"]) for k in ["reloader", "kustomizeController"]), "baseline pins changed from reviewed source")
    namespace, domain = plan["ir"]["domain"]["namespace"], plan["ir"]["domain"]["id"]
    name = "benchmark-cnpg"
    require(all(r["metadata"]["name"] not in {name, "benchmark-flux", "benchmark-artifacts", "benchmark-reloader"} for r in desired), "workload name collides with baseline support")
    payload = generate.archive(desired)
    require(len(payload) < 700000, "baseline artifact exceeds bounded ConfigMap transport")
    controllers = scoped(generate.controller_objects(pins["kustomizeController"]["image"], server_image,
                         {name + ".tar.gz": base64.b64encode(payload).decode()}), namespace)
    role = next(x for x in controllers if x["kind"] == "Role")
    role["rules"] += [{"apiGroups": [""], "resources": ["services"], "verbs": ["get", "list", "watch", "create", "patch", "update", "delete"]},
                      {"apiGroups": ["postgresql.cnpg.io"], "resources": ["clusters"], "verbs": ["get", "list", "watch", "create", "patch", "update", "delete"]}]
    controllers += reloader_objects(namespace, pins["reloader"]["image"], next(x["metadata"]["name"] for x in desired if x["kind"] == "Deployment"))
    def obj(kind, spec, api):
        return {"apiVersion": api, "kind": kind, "metadata": {"name": name, "namespace": namespace}, "spec": spec}
    controllers += [obj("GitRepository", {"interval": "24h", "url": "https://invalid.example/benchmark-only", "suspend": True}, "source.toolkit.fluxcd.io/v1"),
                    obj("Kustomization", {"interval": f"{interval}s", "retryInterval": f"{interval}s", "timeout": "120s", "path": "./", "prune": False,
                         "sourceRef": {"kind": "GitRepository", "name": name}}, "kustomize.toolkit.fluxcd.io/v1")]
    status = {"name": name, "namespace": namespace, "status": {"artifact": {"path": name + ".tar.gz",
        "url": f"http://benchmark-artifacts.{namespace}.svc:8000/{name}.tar.gz", "revision": digest(payload), "digest": digest(payload), "lastUpdateTime": "2026-09-07T00:00:00Z"}}}
    profile = {"schemaVersion": "apsides.dev/cnpg-baseline-preparation/v1", "sourcePlanDigest": expected_digest, "irDigest": plan["irDigest"],
        "domain": domain, "namespace": namespace, "managedResources": len(desired), "namedInputs": inputs, "reactions": len(plan["ir"]["reactions"]),
        "workloadReplicas": 1, "reconcileIntervalSeconds": interval, "expectedRuntimePollIntervalMillis": 1000,
        "concurrency": {"apsidesMutationLoop": "serial awaited applies", "apsidesDeclaredMaxConcurrentRequests": plan["ir"]["domain"].get("budgets", {}).get("maxConcurrentRequests"), "fluxConcurrent": 1, "fluxConcurrentSsa": 1, "reloader": "one worker per watched resource type"}, "pins": pins, "artifactServerImage": server_image, "artifactDigest": digest(payload),
        "rolloutAnnotations": ANNOTATIONS, "upstreamSources": SOURCES, "releaseVerified": False, "productionEvidenceStatus": "not-produced",
        "preconditions": ["Install pinned CNPG and Flux CRDs and CNPG operator separately.", "Establish all named inputs and healthy CNPG/PostgREST before steady-state measurement.",
            "ConfigMap binaryData must remain empty on both sides; update only data.", "Verify deployed Apsides pollIntervalMillis is 1000 before measurement; reject a changed runtime interval.", "CNPG must create the application Secret; this generator never provides Secret values."],
        "limitations": ["Workload comparison only: no initial-deployment or missing-input semantic equivalence.",
            "Reloader observes individual input events and publishes unkeyed Secret hashes in runtime rollout annotations; Apsides uses aggregate keyed revisions.",
            "Do not retain live Secret values, hashes, raw Deployment templates or Reloader annotation contents in measurement artifacts.",
            "Apsides maxConcurrentRequests bounds concurrent named watches, not parallel mutations. Flux and Reloader scheduling differs; no equal global concurrency budget is claimed.",
            "Namespace-scoped informer permissions differ from Apsides named API permissions; no authorization equivalence is claimed.",
            "All four resource deletion policies are orphan; Flux prune is disabled. No cleanup benchmark is claimed.",
            "Preparation performs no installation, readiness check, measurement or release acceptance."]}
    require(not output.exists() and not output.is_symlink(), "output already exists")
    ignored = subprocess.run(["git", "check-ignore", "--quiet", "--no-index", str(output.resolve())], cwd=generate.HERE, capture_output=True, check=False)
    require(ignored.returncode == 0, "baseline output must be ignored by Git")
    output.mkdir(parents=True)
    files = {"artifact.tar.gz": payload, "managed.json": canonical({"apiVersion": "v1", "kind": "List", "items": desired}),
             "flux.json": canonical({"apiVersion": "v1", "kind": "List", "items": controllers}), "source-status.json": canonical(status), "profile.json": canonical(profile)}
    for path, content in files.items():
        with (output / path).open("xb") as stream: stream.write(content)
    return profile


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--plan-sha256", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--artifact-server-image", required=True)
    parser.add_argument("--interval", type=int, choices=[1], default=1)
    args = parser.parse_args()
    try:
        result = prepare(args.plan, args.plan_sha256, args.output, args.artifact_server_image, args.interval)
        print(json.dumps({"output": str(args.output), "irDigest": result["irDigest"], "productionEvidenceStatus": "not-produced"}))
    except (ValueError, KeyError, TypeError, OSError):
        print("CNPG baseline preparation rejected unsupported or changed inputs.", file=sys.stderr)
        sys.exit(1)
