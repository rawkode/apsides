#!/usr/bin/env python3
"""Owned-cluster workload prerequisite check; deliberately produces no benchmark receipt."""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import time
import uuid

import cnpg_baseline as preparation

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_cnpg_demo as demo

shared = demo.shared
ROOT = demo.ROOT
FILES = ("profile.json", "managed.json", "flux.json", "source-status.json", "artifact.tar.gz")
OPERATOR_DIGEST = "sha256:0215a5d90eae629c741d08d9260d7d9f361d05ad3185d1e438875057f3c92557"
KEY = "aps-baseline-probe"


def require(value, reason):
    if not value:
        raise shared.QualificationError(reason)


def read(path):
    require(path.is_file() and not path.is_symlink() and 0 < path.stat().st_size <= preparation.LIMIT,
            "input is not a bounded regular file")
    return path.read_bytes()


def validate_bundle(directory, plan, scratch):
    originals = {name: read(directory / name) for name in FILES}
    profile = json.loads(originals["profile.json"], object_pairs_hook=shared.unique_object)
    preparation.prepare(plan, profile["sourcePlanDigest"], scratch,
                        profile["artifactServerImage"], profile["reconcileIntervalSeconds"])
    require(all(read(scratch / name) == raw for name, raw in originals.items()),
            "prepared baseline differs from exact regenerated plan")
    require(profile["namespace"] == "default" and profile["domain"] == "database-demo",
            "live baseline supports only the current CNPG demo scope")
    managed = json.loads(originals["managed.json"])["items"]
    require({(x["kind"], x["metadata"]["name"]) for x in managed} == {
        ("Cluster", "database"), ("ConfigMap", "api-config"),
        ("Deployment", "database-api"), ("Service", "database-api")}, "unsupported live workload names")
    return profile, originals


def identity(deployment, pods, replicasets):
    """Return selected metadata only: runtime Reloader annotations must never escape."""
    uid = deployment["metadata"]["uid"]
    spec, status = deployment["spec"], deployment.get("status", {})
    if not (spec.get("replicas") == 1 and status.get("observedGeneration") == deployment["metadata"]["generation"]
            and status.get("updatedReplicas") == status.get("readyReplicas") == status.get("availableReplicas") == 1):
        return None
    rs = {x["metadata"]["uid"] for x in replicasets if any(o.get("controller") is True and o.get("uid") == uid
          and o.get("kind") == "Deployment" for o in x["metadata"].get("ownerReferences", []))}
    active = [p for p in pods if not p["metadata"].get("deletionTimestamp") and any(
        o.get("controller") is True and o.get("kind") == "ReplicaSet" and o.get("uid") in rs
        for o in p["metadata"].get("ownerReferences", []))]
    if len(active) != 1:
        return None
    pod = active[0]
    if not any(c.get("type") == "Ready" and c.get("status") == "True" for c in pod.get("status", {}).get("conditions", [])):
        return None
    statuses = pod.get("status", {}).get("containerStatuses", [])
    if len(statuses) != 1 or statuses[0].get("restartCount") != 0 or not statuses[0].get("ready"):
        return None
    owner = next(o for o in pod["metadata"]["ownerReferences"] if o.get("controller") is True)
    return {"deploymentUid": uid, "podUid": pod["metadata"]["uid"], "replicaSetUid": owner["uid"],
            "image": pod["spec"]["containers"][0]["image"], "imageId": statuses[0]["imageID"]}


class BaselineRunner(demo.base.CeilingRunner):
    k = demo.CnpgDemoRunner.k
    wait_object = demo.CnpgDemoRunner.wait_object
    wait_cnpg = demo.CnpgDemoRunner.wait_cnpg
    forwarded_service = demo.CnpgDemoRunner.forwarded_service
    http_messages = demo.CnpgDemoRunner.http_messages
    import_archive = demo.CnpgDemoRunner.import_archive
    install_operator = demo.CnpgDemoRunner.install_operator

    def __init__(self, args):
        self.output = args.output.absolute()
        require(not any(p.is_symlink() for p in [self.output, *self.output.parents])
                and not self.output.exists() and self.output.is_relative_to(ROOT), "output must be a fresh repository directory")
        shared.require_ignored(ROOT, self.output)
        self.repository, self.commit = shared.source_state(Path(__file__).resolve())
        self.output.mkdir(parents=True, mode=0o700)
        self.cluster = "reakt-qual-" + uuid.uuid4().hex
        self.admin, self.issuer = self.output / "admin.kubeconfig", self.output / "issuer.kubeconfig"
        self.attempted = self.owned = self.cleanup_confirmed = False
        self.container_id = None
        self.failures, self.checks = [], []
        self.phase = "validate-inputs"
        self.args = args
        self.workload = "default"
        self.sources = [Path(__file__).resolve(), Path(preparation.__file__).resolve(),
                        Path(preparation.generate.__file__).resolve(), Path(demo.__file__).resolve(),
                        Path(demo.base.__file__).resolve(), Path(shared.__file__).resolve(),
                        Path(demo.base.observations.__file__).resolve(), Path(demo.base.crds.__file__).resolve(),
                        Path(__file__).parent / "pins.json"]
        self.source_digests = {str(p.relative_to(ROOT)): shared.digest_file(p) for p in self.sources}
        self.input_digests = {}

    def prepare(self):
        with tempfile.TemporaryDirectory(dir=self.output) as scratch:
            self.profile, self.bundle = validate_bundle(self.args.prepared, self.args.plan, Path(scratch) / "regenerated")
        self.operator_manifest = self.args.operator_manifest
        self.operator_bytes = read(self.operator_manifest)
        self.operator_sha256 = OPERATOR_DIGEST
        require(shared.digest_bytes(self.operator_bytes) == OPERATOR_DIGEST, "operator manifest differs from reviewed pin")
        images = set(re.findall(r"ghcr\.io/cloudnative-pg/cloudnative-pg:1\.30\.0@sha256:[a-f0-9]{64}", self.operator_bytes.decode()))
        require(len(images) == 1, "operator image pin is ambiguous")
        self.operator_image = next(iter(images))
        self.operator_snapshot = self.output / "operator.yaml"
        self.operator_snapshot.write_bytes(self.operator_bytes)
        require(len(self.args.flux_crd) == len(self.profile["pins"]["crds"]), "expected both pinned Flux CRD manifests")
        self.crds = []
        for path, pin in zip(self.args.flux_crd, self.profile["pins"]["crds"]):
            raw = read(path)
            require(shared.digest_bytes(raw) == "sha256:" + pin["sha256"], "Flux CRD manifest differs from pin")
            self.crds.append(raw)
        captured = [(self.operator_manifest, self.operator_bytes), *zip(self.args.flux_crd, self.crds),
                    *((self.args.prepared / name, raw) for name, raw in self.bundle.items())]
        self.input_digests = {str(p.absolute()): shared.digest_bytes(raw) for p, raw in captured}
        self.input_digests[str(self.args.plan.absolute())] = self.profile["sourcePlanDigest"]
        for name, raw in self.bundle.items():
            (self.output / name).write_bytes(raw)

    def frozen(self):
        require(shared.source_state(Path(__file__).resolve()) == (self.repository, self.commit)
                and all(shared.digest_file(ROOT / p) == d for p, d in self.source_digests.items())
                and all(shared.digest_file(Path(p)) == d for p, d in self.input_digests.items()), "source or inputs changed during baseline run")

    def create_cluster(self):
        self.docker_context = self.command(["docker", "context", "show"]).strip()
        require(bool(self.docker_context), "Docker context is unavailable")
        self.docker_binding = self.docker_identity()
        self.prior_clusters = sorted(self.cluster_names())
        self.prior_containers = sorted(self.command(["docker", "ps", "-aq", "--no-trunc"]).splitlines())
        require(self.cluster not in self.prior_clusters, "cluster name already exists")
        images = json.loads(self.command(["docker", "image", "inspect", demo.base.NODE]))
        require(len(images) == 1, "kind image is ambiguous")
        self.node_image_id = images[0]["Id"]
        self.attempted = True
        self.command(["kind", "create", "cluster", "--name", self.cluster, "--image", demo.base.NODE,
                      "--kubeconfig", self.admin, "--wait", "120s"], timeout=240)
        self.admin.chmod(0o600)
        containers = json.loads(self.command(["docker", "inspect", self.cluster + "-control-plane"]))
        require(len(containers) == 1, "owned container identity is ambiguous")
        self.container_id = containers[0]["Id"]
        node = json.loads(self.k("get", "node", self.cluster + "-control-plane", "-o", "json"))
        self.host_identity = demo.base.observations.validate_kind_target(node, containers[0], self.cluster, demo.base.NODE, self.node_image_id)
        self.node_platform = "/".join(node["status"]["nodeInfo"][k] for k in ["operatingSystem", "architecture"])
        self.owned = True
        self.namespace_identity = json.loads(self.k("get", "namespace", "default", "-o", "json"))["metadata"]["uid"]

    def install(self):
        self.phase = "install-input-verification"
        self.frozen()
        desired = json.loads(self.bundle["managed.json"])["items"]
        flux = json.loads(self.bundle["flux.json"])["items"]
        images = {self.operator_image, next(x["spec"]["imageName"] for x in desired if x["kind"] == "Cluster")}
        images.update(c["image"] for x in desired + flux if x["kind"] == "Deployment" for c in x["spec"]["template"]["spec"]["containers"])
        for index, reference in enumerate(sorted(images)):
            self.phase = f"image-{index}-pull"
            self.command(["docker", "pull", reference], timeout=300)
            archive = self.output / f"image-{index}.tar"
            try:
                self.phase = f"image-{index}-save"
                self.command(["docker", "image", "save", "--output", archive, reference], timeout=300)
                self.phase = f"image-{index}-import"
                self.import_archive(archive, reference)
            finally:
                archive.unlink(missing_ok=True)
        self.phase = "operator-install-and-readiness"
        self.install_operator()
        for index, raw in enumerate(self.crds):
            self.phase = f"flux-crd-{index}-apply"
            self.k("apply", "--server-side", "-f", "-", data=raw.decode())
        for index, name in enumerate(["kustomizations.kustomize.toolkit.fluxcd.io", "gitrepositories.source.toolkit.fluxcd.io"]):
            self.phase = f"flux-crd-{index}-established"
            self.k("wait", "--for=condition=Established", "crd/" + name, "--timeout=120s")
        self.phase = "public-configmap-apply"
        self.apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "api-settings", "namespace": self.workload}, "data": {"PGRST_DB_MAX_ROWS": "100"}})
        self.phase = "baseline-controllers-apply"
        self.k("apply", "--server-side", "--field-manager=aps-baseline", "-f", "-", data=self.bundle["flux.json"].decode())
        for name in ["benchmark-artifacts", "benchmark-flux", "benchmark-reloader"]:
            self.phase = name + "-ready"
            self.k("rollout", "status", "deployment/" + name, "-n", self.workload, "--timeout=300s")
        self.phase = "source-artifact-status-patch"
        status = json.loads(self.bundle["source-status.json"])
        self.k("patch", "gitrepository", status["name"], "-n", self.workload, "--subresource=status", "--type=merge", "-p", json.dumps({"status": status["status"]}))
        self.phase = "kustomization-ready"
        self.k("wait", "--for=condition=Ready", "kustomization/benchmark-cnpg", "-n", self.workload, "--timeout=300s")
        self.phase = "cnpg-ready-and-secret-present"
        self.wait_cnpg()

    def apply(self, obj):
        self.k("apply", "--server-side", "--field-manager=aps-baseline", "-f", "-", data=json.dumps(obj))

    def workload_state(self):
        deployment = json.loads(self.k("get", "deployment/database-api", "-n", "default", "-o", "json"))
        pods = json.loads(self.k("get", "pods", "-n", "default", "-l", "app=database-api", "-o", "json"))["items"]
        replicasets = json.loads(self.k("get", "replicasets", "-n", "default", "-l", "app=database-api", "-o", "json"))["items"]
        return identity(deployment, pods, replicasets)

    def ready(self, previous=None):
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            current = self.workload_state()
            if current and (not previous or (current["podUid"] != previous["podUid"] and current["replicaSetUid"] != previous["replicaSetUid"])):
                if previous:
                    require(current["deploymentUid"] == previous["deploymentUid"] and current["image"] == previous["image"], "rollout changed workload identity or image")
                return current
            time.sleep(2)
        raise shared.QualificationError("baseline workload did not reach expected readiness")

    def wait_http(self, expected_count, expected_identity, timeout=120):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            require(self.workload_state() == expected_identity, "workload identity changed during HTTP startup")
            try:
                result = self.http_messages(expected_count)
            except demo.HttpObservationError as error:
                if not (error.reason == "transport" or (error.reason == "status" and error.status == 503)):
                    raise
            else:
                require(self.workload_state() == expected_identity, "workload identity changed during HTTP startup")
                require(time.monotonic() < deadline, "PostgREST startup deadline exceeded")
                return result
            remaining = deadline - time.monotonic()
            if remaining > 0:
                time.sleep(min(2, remaining))
        raise shared.QualificationError("PostgREST startup deadline exceeded")

    def measure(self):
        self.phase = "initial-readiness"
        initial = self.ready()
        desired = json.loads(self.bundle["managed.json"])["items"]
        expected_image = next(x["spec"]["template"]["spec"]["containers"][0]["image"] for x in desired if x["kind"] == "Deployment")
        require(initial["image"] == expected_image, "workload image differs from prepared candidate")
        self.checks.append({"check": self.phase, "identity": initial, "http": self.wait_http(2, initial)})
        self.phase = "configmap-reaction"
        self.apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "api-settings", "namespace": self.workload}, "data": {"PGRST_DB_MAX_ROWS": "1"}})
        changed = self.ready(initial)
        self.checks.append({"check": self.phase, "identity": changed, "http": self.wait_http(1, changed)})
        self.phase = "equal-value-update"
        cm = json.loads(self.k("get", "configmap/api-settings", "-n", self.workload, "-o", "json"))
        patch = [{"op": "test", "path": "/metadata/uid", "value": cm["metadata"]["uid"]},
                 {"op": "test", "path": "/metadata/resourceVersion", "value": cm["metadata"]["resourceVersion"]},
                 {"op": "add", "path": "/metadata/annotations/aps-baseline-probe", "value": uuid.uuid4().hex}]
        if not cm["metadata"].get("annotations"):
            patch[-1] = {"op": "add", "path": "/metadata/annotations", "value": {"aps-baseline-probe": uuid.uuid4().hex}}
        before_rs = self.replica_set_uids()
        self.k("patch", "configmap/api-settings", "-n", self.workload, "--type=json", "-p", json.dumps(patch))
        written = json.loads(self.k("get", "configmap/api-settings", "-n", self.workload, "-o", "json"))
        require(written["metadata"]["uid"] == cm["metadata"]["uid"]
                and written["metadata"]["resourceVersion"] != cm["metadata"]["resourceVersion"]
                and written["data"] == cm["data"] and not written.get("binaryData"), "equal-value update was not an actual unchanged-data update")
        deadline = time.monotonic() + self.args.equal_observation_seconds
        while time.monotonic() < deadline:
            require(self.workload_state() == changed and self.replica_set_uids() == before_rs, "equal-value update changed workload identity")
            time.sleep(1)
        self.checks.append({"check": self.phase, "observationSeconds": self.args.equal_observation_seconds, "identity": changed})
        self.phase = "secret-reaction"
        self.secret_reaction(changed)

    def replica_set_uids(self):
        objects = json.loads(self.k("get", "replicasets", "-n", self.workload, "-l", "app=database-api", "-o", "json"))["items"]
        return sorted(x["metadata"]["uid"] for x in objects)

    def secret_reaction(self, previous):
        def config_versions():
            return {name: json.loads(self.k("get", "configmap/" + name, "-n", self.workload, "-o", "json"))["metadata"]["resourceVersion"]
                    for name in ["api-config", "api-settings"]}
        config_before = config_versions()
        def secret():
            return json.loads(self.k("get", "secret/database-app", "-n", self.workload, "-o", "json"))
        original = secret()
        require(KEY not in original["data"] and bool(original["data"].get("uri")), "Secret fixture precondition failed")
        value = base64.b64encode(os.urandom(24)).decode()
        def patch(current, remove=False):
            require(current["metadata"]["uid"] == original["metadata"]["uid"], "Secret identity changed")
            operations = [{"op": "test", "path": "/metadata/uid", "value": original["metadata"]["uid"]},
                          {"op": "test", "path": "/metadata/resourceVersion", "value": current["metadata"]["resourceVersion"]}]
            if remove:
                operations.append({"op": "test", "path": "/data/" + KEY, "value": value})
            operations.append({"op": "remove", "path": "/data/" + KEY} if remove else {"op": "add", "path": "/data/" + KEY, "value": value})
            self.k("patch", "secret/database-app", "-n", self.workload, "--type=json", "-p", json.dumps(operations))
        try:
            patch(original)
            current = self.ready(previous)
            observed = secret()
            require(observed["data"] == {**original["data"], KEY: value}, "Secret data changed outside synthetic key")
            require(config_versions() == config_before, "ConfigMap changed during Secret experiment")
            self.checks.append({"check": "secret-reaction", "identity": current, "secretUid": original["metadata"]["uid"], "http": self.wait_http(1, current)})
        finally:
            current = secret()
            if current.get("data", {}).get(KEY) == value:
                patch(current, remove=True)
            restored = secret()
            require(restored["metadata"]["uid"] == original["metadata"]["uid"]
                    and restored["data"] == original["data"], "Secret fixture restoration failed")

    def run(self):
        passed = False
        try:
            self.prepare()
            self.frozen()
            self.phase = "create-cluster"
            self.create_cluster()
            self.phase = "install"
            self.install()
            self.measure()
            self.frozen()
            passed = True
        except Exception as error:
            # Never reflect kubectl output, HTTP bodies, Secret values or annotations.
            failure = {"phase": self.phase, "reason": "baseline prerequisite failed"}
            if isinstance(error, demo.HttpObservationError):
                failure.update(httpFailureClass=error.reason, httpStatus=error.status)
            self.failures.append(failure)
        finally:
            try:
                self.cleanup()
            except Exception:
                passed = False
                self.failures.append({"phase": "cleanup", "reason": "owned cluster cleanup failed"})
        passed = passed and self.cleanup_confirmed and not self.admin.exists() and not self.issuer.exists()
        if passed:
            try:
                self.frozen()
            except Exception:
                passed = False
                self.failures.append({"phase": "publication", "reason": "source or inputs changed"})
        result = {"schemaVersion": "apsides.dev/cnpg-baseline-live/v1", "passed": passed,
                  "productionEvidenceStatus": "not-produced", "releaseVerified": False,
                  "sourceCommit": self.commit, "sourceFiles": self.source_digests, "inputFiles": self.input_digests,
                  "namespaceUid": getattr(self, "namespace_identity", None), "hostIdentity": getattr(self, "host_identity", None),
                  "checks": self.checks, "failures": self.failures, "clusterDeleted": self.cleanup_confirmed,
                  "credentialsRemoved": not self.admin.exists() and not self.issuer.exists()}
        (self.output / "result.json").write_bytes(shared.canonical(result))
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ["prepared", "plan", "operator-manifest", "output"]:
        parser.add_argument("--" + flag, type=Path, required=True)
    parser.add_argument("--flux-crd", type=Path, action="append", required=True)
    parser.add_argument("--equal-observation-seconds", type=int, default=30)
    args = parser.parse_args()
    if not 30 <= args.equal_observation_seconds <= 300:
        parser.error("equal observation must be between 30 and 300 seconds")
    try:
        result = BaselineRunner(args).run()
    except Exception:
        print(json.dumps({"passed": False, "reason": "baseline preparation failed", "productionEvidenceStatus": "not-produced"}))
        return 1
    print(json.dumps({"passed": result["passed"], "output": str(args.output), "productionEvidenceStatus": "not-produced"}))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
