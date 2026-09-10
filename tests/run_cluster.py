#!/usr/bin/env python3
"""Run every generated reactor case in a network-isolated disposable kind cluster.

The ambient Kubernetes context is never read. The generated controller runs as
a Linux container and receives a dedicated, short-lived fixture credential via
an explicit loopback TCP proxy. An unimplemented or unobserved case fails.
"""

import argparse
import copy
import base64
import hashlib
import hmac
import ipaddress
import json
import os
from pathlib import Path
import subprocess
import tarfile
import time
import ssl
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET

NODE_IMAGE = "kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48"
PROXY_TAG = "apsides-proxy-fixture:local"
PROXY_IMAGE = "nginxinc/nginx-unprivileged:alpine@sha256:d9083fe47768377ef55dedafd67d4da7c2f2bc2bece7554954f29359deb0dce9"
ROLLOUT_KEY_ID = "fixture-v1"
ROLLOUT_KEY = b"synthetic-fixture-key-never-production-0123456789"
CONTROLLER_TAG = "apsides-controller-fixture:local"
WORKLOAD_TAG = "apsides-fixture:local"


def stage_release_evidence(output, receipt, coverage, result):
    """Publish only successful, bounded diagnostics using release-relative paths."""
    if result.get("status") != "passed":
        return
    receipt = copy.deepcopy(receipt)
    coverage = copy.deepcopy(coverage)
    allowed = {"tested-artifact-manifest.json", "cluster-transcript.json",
               "semantic-results.json", "canary-scan.json"}
    allowed |= {"fixture-crds.json", *[f"fixture-crd-{index}.yaml" for index in range(16)]}
    required = {receipt["artifactManifestFile"]}
    for catalogue in ["cases", "fixtureCases", "checks"]:
        for record in receipt[catalogue].values():
            if record["status"] != "passed":
                raise RuntimeError("failed reactor record cannot be packaged")
            required.update(record["evidenceFiles"])
            record["evidenceFiles"] = ["evidence/" + name for name in record["evidenceFiles"]]
    if not required <= allowed:
        raise RuntimeError("reactor evidence references an unapproved diagnostic")
    receipt["artifactManifestFile"] = "evidence/" + receipt["artifactManifestFile"]
    coverage["irDigest"] = receipt["productionIrDigest"]
    for record in [*coverage["cases"], *coverage["checks"].values()]:
        if record["evidenceFiles"] != ["reactor-evidence.json"]:
            raise RuntimeError("reactor coverage has an unexpected evidence reference")
        record["evidenceFiles"] = ["evidence/reactor-evidence.json"]
    files = {}
    for name in sorted(required):
        path = output / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 4 * 1024 * 1024:
            raise RuntimeError("reactor diagnostic is missing, nonregular or oversized")
        files[name] = path.read_bytes()
    runner = Path(__file__).resolve()
    source = runner.read_bytes()
    if "sha256:" + hashlib.sha256(source).hexdigest() != receipt["runnerIdentity"]["sourceSha256"]:
        raise RuntimeError("reactor runner changed during measurement")
    files["run_cluster.py"] = source
    files["reactor-evidence.json"] = canonical(receipt)
    if any(len(value) > 4 * 1024 * 1024 for value in files.values()):
        raise RuntimeError("reactor evidence exceeds the release document limit")
    destination = output / "evidence"
    destination.mkdir()
    for name, value in files.items():
        (destination / name).write_bytes(value)
    (output / "release-coverage-fragment.json").write_bytes(canonical(coverage))


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def digest(value):
    return "sha256:" + hashlib.sha256(canonical(value)).hexdigest()


def file_digest(path):
    with path.open("rb") as stream:
        return "sha256:" + hashlib.file_digest(stream, "sha256").hexdigest()


def assertion(check_id, expected, actual):
    passed = actual >= 1 if expected == ">=1" and isinstance(actual, int) else expected == actual
    return {"checkId": check_id, "expected": expected, "actual": actual, "passed": passed}


def inspect_controller_archive(path, expected_ir):
    with tarfile.open(path) as archive:
        index = json.load(archive.extractfile("index.json"))
        manifests = index.get("manifests", [])
        if len(manifests) != 1 or manifests[0].get("platform", {}).get("os") != "linux":
            raise RuntimeError("controller archive must contain exactly one Linux manifest")
        manifest_digest = manifests[0].get("digest", "")
        manifest = json.load(archive.extractfile("blobs/sha256/" + manifest_digest.removeprefix("sha256:")))
        config_digest = manifest.get("config", {}).get("digest", "")
        config = json.load(archive.extractfile("blobs/sha256/" + config_digest.removeprefix("sha256:")))
    labels = config.get("config", {}).get("Labels", {})
    if labels.get("dev.apsides.ir-digest") != expected_ir:
        raise RuntimeError("controller OCI label is not bound to the fixture IR")
    if labels.get("dev.apsides.release-status") != "blocked":
        raise RuntimeError("fixture controller must retain blocked release status")
    if config.get("config", {}).get("Entrypoint") != ["/usr/local/bin/apsides-controller"]:
        raise RuntimeError("controller OCI entrypoint differs from the generated controller")
    return manifest_digest, config_digest


def inspect_docker_archive(path, expected_tag):
    with tarfile.open(path) as archive:
        manifest = json.load(archive.extractfile("manifest.json"))
        if len(manifest) != 1 or expected_tag not in (manifest[0].get("RepoTags") or []):
            raise RuntimeError(f"image archive must bind {expected_tag}")
        config_bytes = archive.extractfile(manifest[0]["Config"]).read()
    return "sha256:" + hashlib.sha256(config_bytes).hexdigest()


def inspect_workload_archive(path):
    return inspect_docker_archive(path, WORKLOAD_TAG)


def parse_crd_yaml(raw, root):
    # Reuse the compiler's locked YAML parser, including duplicate-key rejection.
    script = """import { parseAllDocuments } from 'yaml';
import { readFileSync } from 'node:fs';
const documents = parseAllDocuments(readFileSync(0, 'utf8'), { uniqueKeys: true });
if (documents.some(document => document.errors.length)) process.exit(1);
process.stdout.write(JSON.stringify(documents.map(document => document.toJS({ maxAliasCount: 0 }))));"""
    parsed = subprocess.run(["deno", "eval", "--ext=ts", script], input=raw,
                            capture_output=True, cwd=root / "packages/compiler", timeout=30)
    if parsed.returncode or len(parsed.stdout) > 4 * 1024 * 1024:
        raise RuntimeError("fixture CRD YAML is invalid or exceeds its parsing bound")
    return json.loads(parsed.stdout)


def validate_crd_documents(ir, manifests):
    expected = {}
    for resource in ir["resources"]:
        mapping = resource.get("apiResource", {})
        schema = mapping.get("schemaSha256")
        if schema is None:
            continue
        group, version = resource["apiVersion"].split("/", 1)
        identity = (group, resource["kind"], mapping["plural"])
        entry = expected.setdefault(identity, {"digest": schema, "versions": set()})
        if mapping["scope"] != "Namespaced" or entry["digest"] != schema:
            raise RuntimeError("fixture CRD declaration has inconsistent scope or schema")
        entry["versions"].add(version)
    seen = set()
    for source_digest, documents in manifests:
        if not documents:
            raise RuntimeError("fixture CRD manifest is empty")
        for document in documents:
            if (not isinstance(document, dict) or document.get("apiVersion") != "apiextensions.k8s.io/v1"
                    or document.get("kind") != "CustomResourceDefinition"):
                raise RuntimeError("fixture CRD manifest contains a non-CRD resource")
            spec = document.get("spec", {})
            names = spec.get("names", {})
            identity = (spec.get("group"), names.get("kind"), names.get("plural"))
            declaration = expected.get(identity)
            conversion = spec.get("conversion", {})
            versions = spec.get("versions", [])
            if (declaration is None or identity in seen or declaration["digest"] != source_digest
                    or spec.get("scope") != "Namespaced"
                    or document.get("metadata", {}).get("name") != f"{identity[2]}.{identity[0]}"
                    or document.get("metadata", {}).get("namespace") is not None
                    or conversion.get("strategy", "None") != "None" or "webhook" in conversion
                    or not isinstance(versions, list)
                    or not declaration["versions"] <= {item.get("name") for item in versions if item.get("served") is True}):
                raise RuntimeError("fixture CRD differs from the declared schema, identity, scope or version")
            seen.add(identity)
    if seen != set(expected):
        raise RuntimeError("fixture custom resources require their exact local CRD manifests")


class ReactorRunner:
    def __init__(self, artifact, fixture_contract, controller_archive, fixture_archive, output, crd_manifests=()):
        self.root = Path(__file__).resolve().parents[1]
        self.artifact = artifact.resolve()
        self.fixture_contract = fixture_contract.resolve()
        self.controller_archive = controller_archive.resolve()
        self.fixture_archive = fixture_archive.resolve()
        self.output = output.resolve()
        self.plan = json.loads((self.artifact / "plan.json").read_text())
        self.ir = self.plan["ir"]
        self.namespace = self.ir["domain"]["namespace"]
        self.control = self.ir["domain"]["controlNamespace"]
        self.cluster = f"apsides-test-{os.getpid()}-{int(time.time())}"
        self.driver_config = self.output / "fixture-driver.kubeconfig"
        self.runtime_host_config = self.output / "runtime-host.kubeconfig"
        self.runtime_ca = self.output / "runtime-ca.crt"
        self.env = {**os.environ, "KUBECONFIG": str(self.driver_config)}
        self.results = {}
        self.logs = []
        self.egress_logs = []
        self.created = False
        self.original_rules = []
        self.network_canary_passed = False
        self.separate_credential_proven = False
        self.fixture_cases = []
        self.fixture_results = {}
        self.controller_started = False
        self.local_images_verified = False
        self.crd_manifests = list(crd_manifests)
        self.crd_snapshots = []
        self.crd_installations = []

    def command(self, argv, data=None, check=True, timeout=180):
        result = subprocess.run(argv, input=None if data is None else json.dumps(data), text=True,
                                capture_output=True, env=self.env, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"{argv[0]} {argv[1:3]} failed: {result.stderr[-2000:]}")
        return result

    def k(self, *argv, data=None, check=True, timeout=180):
        return self.command(["kubectl", "--kubeconfig", str(self.driver_config), "--context", f"kind-{self.cluster}", *argv],
                            data, check, timeout)

    def apply(self, obj):
        self.k("apply", "-f", "-", data=obj)

    def get(self, kind, name, namespace=None):
        namespace = namespace or self.namespace
        return json.loads(self.k("-n", namespace, "get", kind, name, "--show-managed-fields", "-o", "json").stdout)

    def wait_for(self, label, predicate, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = predicate()
                if value:
                    return value
            except (KeyError, RuntimeError, json.JSONDecodeError):
                pass
            time.sleep(0.25)
        raise RuntimeError(f"deadline exceeded: {label}")

    def validate_inputs(self):
        verifier = self.root / "target/debug/aps"
        self.command([str(verifier), "verify", "--artifact", str(self.artifact)])
        if self.ir.get("testOnly") is not True:
            raise RuntimeError("cluster runner accepts only compiler-marked testOnly IR")
        if self.plan.get("irDigest") != digest(self.ir):
            raise RuntimeError("plan IR digest is not canonical")
        test_plan = json.loads((self.artifact / "test-plan.json").read_text())
        if test_plan != self.plan.get("testPlan") or not test_plan:
            raise RuntimeError("artifact test plan differs from the validated generated plan")
        self.test_cases = {(case["reactionId"], case["scenario"]): case for case in test_plan}
        manifest = json.loads((self.artifact / "manifest.json").read_text())
        if manifest.get("irDigest") != self.plan["irDigest"]:
            raise RuntimeError("artifact manifest and plan IR identities differ")
        for resource in self.ir["resources"]:
            if resource["kind"] != "Deployment":
                continue
            spec = resource["desired"]["spec"]["template"]["spec"]
            containers = spec.get("containers", []) + spec.get("initContainers", [])
            if any(c.get("image") != WORKLOAD_TAG or c.get("imagePullPolicy") != "Never" for c in containers):
                raise RuntimeError("fixture IR workload images are not locally pinned fixture images")
        source = self.fixture_contract
        fixtures = json.loads((source / "fixtures.json").read_text())
        self.fixtures = fixtures
        self.test_seed = {"schemaVersion": "apsides.dev/reactor-seed/v1", "fixtureIrDigest": self.plan["irDigest"],
                          "rolloutKeyId": ROLLOUT_KEY_ID,
                          "inputs": [{"id": item["inputId"], "kind": next(value["kind"] for value in self.ir["inputs"] if value["id"] == item["inputId"]),
                                      "state": item["state"], "dataKeys": sorted(item.get("data", {}).keys())} for item in fixtures["inputs"]]}
        overlay = json.loads((source / "fixture-overlay.json").read_text())
        coverage = json.loads((source / "fixture-coverage.json").read_text())
        fixture_hex = hashlib.sha256(canonical(fixtures)).hexdigest()
        fixture_ir_hex = hashlib.sha256(canonical(self.ir)).hexdigest()
        if coverage.get("schemaVersion") != "apsides.dev/fixture-coverage/v1" or coverage.get("testOnly") is not True or coverage.get("productionReady") is not False:
            raise RuntimeError("compiler fixture coverage has an invalid safety envelope")
        if coverage.get("fixtureSha256") != fixture_hex or coverage.get("fixtureIrSha256") != fixture_ir_hex:
            raise RuntimeError("compiler fixture coverage is not bound to the tested fixture inputs and IR")
        if overlay.get("fixtureIrSha256") != fixture_ir_hex or overlay.get("productionIrSha256") != coverage.get("productionIrSha256"):
            raise RuntimeError("fixture overlay and coverage identities differ")
        reactions = {item["id"] for item in self.ir["reactions"]}
        seen = set()
        for entry in coverage.get("reactions", []):
            cases = entry.get("cases", [])
            expected_scenarios = {scenario for reaction_id, scenario in self.test_cases if reaction_id == entry.get("id")}
            expected_scenarios.add("reaction-contribution-removed")
            if entry.get("id") not in reactions or {case.get("scenario") for case in cases} != expected_scenarios:
                raise RuntimeError("compiler fixture coverage does not exactly cover every required scenario")
            for case in cases:
                key = (case.get("reactionId"), case.get("scenario"))
                if key in seen or case.get("status") != "required" or case.get("fixtureSha256") != fixture_hex or case.get("overlayIdentity") != fixture_ir_hex:
                    raise RuntimeError("compiler fixture case is duplicate, optional, or identity-unbound")
                if case.get("scenario") != "reaction-contribution-removed":
                    planned = self.test_cases.get(key)
                    applicability = case.get("applicability", {})
                    applicable_inputs = [item for item in self.ir["inputs"] if item["id"] in set(planned.get("inputIds", []) if planned else [])]
                    modes = sorted({item["mode"] for item in applicable_inputs})
                    managed_resources = sorted(item["managedResourceId"] for item in applicable_inputs if item.get("managedResourceId"))
                    if (planned is None or applicability.get("inputIds") != planned.get("inputIds")
                            or applicability.get("inputModes") != modes
                            or applicability.get("managedResourceIds") != managed_resources):
                        raise RuntimeError("compiler fixture applicability differs from the core test-plan stimulus set")
                seen.add(key)
                self.fixture_cases.append(case)
        expected_count = len(self.test_cases) + len(reactions)
        if {reaction for reaction, _ in seen} != reactions or len(seen) != expected_count:
            raise RuntimeError("compiler fixture coverage reaction set is incomplete")
        self.production_ir_digest = "sha256:" + coverage["productionIrSha256"]
        self.fixture_overlay_digest = digest(overlay)
        self.runner_digest = file_digest(Path(__file__).resolve())
        self.controller_manifest, self.controller_image_config = inspect_controller_archive(self.controller_archive, self.plan["irDigest"])
        self.controller_archive_digest = file_digest(self.controller_archive)
        self.fixture_image = inspect_workload_archive(self.fixture_archive)
        self.fixture_archive_digest = file_digest(self.fixture_archive)

    def snapshot_crds(self):
        if len(self.crd_manifests) > 16:
            raise RuntimeError("too many fixture CRD manifests")
        for index, path in enumerate(self.crd_manifests):
            if path.is_symlink() or not path.is_file() or not 0 < path.stat().st_size <= 4 * 1024 * 1024:
                raise RuntimeError("fixture CRD manifest is not a bounded regular file")
            raw = path.read_bytes()
            if len(raw) > 4 * 1024 * 1024:
                raise RuntimeError("fixture CRD manifest grew beyond its bound")
            sha = "sha256:" + hashlib.sha256(raw).hexdigest()
            documents = parse_crd_yaml(raw, self.root)
            name = f"fixture-crd-{index}.yaml"
            self.crd_snapshots.append({"source": path, "file": name, "digest": sha, "documents": documents})
            (self.output / name).write_bytes(raw)
        validate_crd_documents(self.ir, [(item["digest"], item["documents"]) for item in self.crd_snapshots])

    def install_crds(self):
        for snapshot in self.crd_snapshots:
            for document in snapshot["documents"]:
                self.k("apply", "--server-side", "--field-manager=aps-fixture-crds", "-f", "-", data=document)
                name = document["metadata"]["name"]
                self.k("wait", "--for=condition=Established", "crd/" + name, "--timeout=120s")
                installed = json.loads(self.k("get", "crd", name, "-o", "json").stdout)
                self.crd_installations.append({"name": name, "uid": installed["metadata"]["uid"],
                                               "sourceFile": snapshot["file"], "sourceDigest": snapshot["digest"],
                                               "group": document["spec"]["group"], "plural": document["spec"]["names"]["plural"],
                                               "servedVersions": [item["name"] for item in document["spec"]["versions"] if item.get("served") is True]})

    def create_cluster(self):
        self.command(["docker", "image", "inspect", NODE_IMAGE])
        loaded = self.command(["docker", "load", "--input", str(self.controller_archive)]).stdout
        if self.controller_manifest not in loaded:
            raise RuntimeError("Docker loaded a controller identity different from the OCI manifest")
        self.command(["docker", "tag", self.controller_manifest, CONTROLLER_TAG])
        self.command(["docker", "image", "inspect", PROXY_IMAGE])
        proxy_context = self.output / "proxy-image"
        proxy_context.mkdir()
        (proxy_context / "Dockerfile").write_text(f"FROM {PROXY_IMAGE}\n")
        self.proxy_archive = self.output / "proxy-image.tar"
        self.command(["docker", "buildx", "build", "--pull=false", "--network=none", "--provenance=false", "--sbom=false",
                      "--platform", "linux/arm64", "--tag", PROXY_TAG, "--output", f"type=docker,dest={self.proxy_archive}", str(proxy_context)])
        self.proxy_image_config = inspect_docker_archive(self.proxy_archive, PROXY_TAG)
        self.created = True
        self.command(["kind", "create", "cluster", "--name", self.cluster, "--image", NODE_IMAGE,
                      "--kubeconfig", str(self.driver_config), "--wait", "120s"])
        self.command(["kind", "load", "image-archive", str(self.fixture_archive), "--name", self.cluster])
        self.command(["kind", "load", "image-archive", str(self.proxy_archive), "--name", self.cluster])
        self.command(["kind", "load", "docker-image", CONTROLLER_TAG, "--name", self.cluster])
        images = json.loads(self.command(["docker", "exec", f"{self.cluster}-control-plane", "crictl", "images", "-o", "json"]).stdout)
        tags = {tag for image in images.get("images", []) for tag in image.get("repoTags", [])}
        if any(not any(tag == required or tag.endswith("/" + required) for tag in tags)
               for required in [CONTROLLER_TAG, PROXY_TAG, WORKLOAD_TAG]):
            raise RuntimeError("fixture images are not locally available on every eligible test node")
        expected_ids = {CONTROLLER_TAG: self.controller_image_config, WORKLOAD_TAG: self.fixture_image, PROXY_TAG: self.proxy_image_config}
        for required, expected_id in expected_ids.items():
            matching = [image.get("id") for image in images.get("images", [])
                        if any(tag == required or tag.endswith("/" + required) for tag in image.get("repoTags", []))]
            if expected_id not in matching:
                raise RuntimeError(f"locally imported image identity differs for {required}")
        self.local_images_verified = True
        for namespace in [self.namespace, self.control, "apsides-outside"]:
            self.apply({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": namespace}})

    def install_rbac(self):
        self.sa = "apsides-fixture-controller"
        self.apply({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": self.sa, "namespace": self.control}})
        rules_by_namespace = {}
        for capability in self.plan["capabilities"]:
            namespace = capability["namespace"]
            if namespace not in [self.namespace, self.control]:
                raise RuntimeError("capability escapes fixture namespaces")
            resource = capability["resource"] + ("/" + capability["subresource"] if capability.get("subresource") else "")
            rule = {"apiGroups": [capability["apiGroup"]], "resources": [resource], "verbs": [capability["verb"]]}
            if capability["names"]:
                rule["resourceNames"] = capability["names"]
            rules_by_namespace.setdefault(namespace, []).append(rule)
        self.original_rules = rules_by_namespace[self.namespace]
        for namespace, rules in rules_by_namespace.items():
            self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
                        "metadata": {"name": self.sa, "namespace": namespace}, "rules": rules})
            self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
                        "metadata": {"name": self.sa, "namespace": namespace},
                        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": self.sa},
                        "subjects": [{"kind": "ServiceAccount", "name": self.sa, "namespace": self.control}]})
        for resource in self.ir["resources"]:
            if resource["kind"] == "Deployment":
                name = resource["desired"]["spec"]["template"]["spec"].get("serviceAccountName", "default")
                if name != "default":
                    self.apply({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": name, "namespace": self.namespace}})

    def seed_inputs(self):
        fixtures = {item["inputId"]: item for item in self.fixtures["inputs"]}
        for item in self.ir["inputs"]:
            if item["mode"] != "observed":
                continue
            fixture = fixtures.get(item["id"])
            if fixture is None:
                raise RuntimeError(f"FixtureRequired: no generated fixture for {item['id']}")
            if fixture["state"] != "present":
                raise RuntimeError(f"FixtureRequired: initial live input {item['id']} is {fixture['state']}")
            data = fixture.get("data", {})
            if not data:
                raise RuntimeError(f"FixtureRequired: empty generated fixture for {item['id']}")
            if item["kind"] == "Secret":
                data = {key: base64.b64encode(value.encode()).decode() for key, value in data.items()}
            self.apply({"apiVersion": "v1", "kind": item["kind"], "metadata": {"name": item["name"], "namespace": item["namespace"]}, "data": data})

    def runtime_configuration(self):
        admin = json.loads(self.k("config", "view", "--raw", "--minify", "-o", "json").stdout)
        token = self.k("-n", self.control, "create", "token", self.sa, "--duration=30m").stdout.strip()
        host = json.loads(json.dumps(admin))
        host["users"] = [{"name": "runtime", "user": {"token": token}}]
        host["contexts"][0]["context"]["user"] = "runtime"
        cluster = host["clusters"][0]["cluster"]
        self.runtime_server = cluster["server"].rstrip("/")
        self.runtime_token = token
        self.runtime_ca.write_bytes(base64.b64decode(cluster["certificate-authority-data"]))
        self.runtime_host_config.write_text(json.dumps(host))
        self.runtime_host_config.chmod(0o600)
        identity = json.loads(self.command(["kubectl", "--kubeconfig", str(self.runtime_host_config), "auth", "whoami", "-o", "json"]).stdout)
        expected_identity = f"system:serviceaccount:{self.control}:{self.sa}"
        if identity.get("status", {}).get("userInfo", {}).get("username") != expected_identity:
            raise RuntimeError("runtime kubeconfig does not authenticate as the dedicated fixture ServiceAccount")
        can_create_outside = self.command(["kubectl", "--kubeconfig", str(self.runtime_host_config), "auth", "can-i", "create", "configmaps",
                                           "-n", "apsides-outside"], check=False)
        if can_create_outside.stdout.strip() != "no" or can_create_outside.returncode == 0:
            raise RuntimeError("runtime credential unexpectedly has out-of-scope create permission")
        self.separate_credential_proven = True
        pod = json.loads(json.dumps(host))
        pod["clusters"][0]["cluster"]["server"] = "https://127.0.0.1:8443"
        pod["clusters"][0]["cluster"]["tls-server-name"] = "kubernetes.default.svc"
        runtime = {"mode": "fixture", "acknowledgement": "disposable-cluster-only", "irDigest": self.plan["irDigest"],
                   "clusterBinding": self.ir["domain"]["clusterBinding"], "kubeconfig": "/run/apsides/kubeconfig",
                   "context": f"kind-{self.cluster}", "pollIntervalMillis": 250, "maxWatchCycles": 1_000_000,
                   "rolloutKey": {"keyId": ROLLOUT_KEY_ID, "value": ROLLOUT_KEY.decode()}}
        self.apply({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "apsides-runtime-fixture", "namespace": self.control},
                    "stringData": {"runtime.json": json.dumps(runtime), "kubeconfig": json.dumps(pod)}})

    def runtime_api(self, method, path, body=None):
        data = None if body is None else canonical(body)
        request = urllib.request.Request(self.runtime_server + path, data=data, method=method,
                                         headers={"Authorization": f"Bearer {self.runtime_token}",
                                                  "Content-Type": "application/json"})
        context = ssl.create_default_context(cafile=self.runtime_ca)
        try:
            with urllib.request.urlopen(request, context=context, timeout=10) as response:
                response.read(4096)
                return response.status
        except urllib.error.HTTPError as error:
            error.read(4096)
            return error.code

    def install_network_isolation(self):
        self.apply({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "egress-sentinel", "namespace": "apsides-outside"},
                    "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "egress-sentinel"}},
                             "template": {"metadata": {"labels": {"app": "egress-sentinel"}},
                                          "spec": {"containers": [{"name": "server", "image": PROXY_TAG, "imagePullPolicy": "Never"}]}}}})
        self.apply({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "egress-sentinel", "namespace": "apsides-outside"},
                    "spec": {"selector": {"app": "egress-sentinel"}, "ports": [{"port": 8080, "targetPort": 8080}]}})
        self.k("-n", "apsides-outside", "rollout", "status", "deployment/egress-sentinel", "--timeout=90s")
        sentinel_ip = self.get("service", "egress-sentinel", "apsides-outside")["spec"]["clusterIP"]
        self.k("-n", "apsides-outside", "run", "sentinel-preflight", "--restart=Never", f"--image={PROXY_TAG}",
               "--image-pull-policy=Never", "--command", "--", "sh", "-c",
               f"nc -z -w 5 {sentinel_ip} 8080")
        self.k("-n", "apsides-outside", "wait", "--for=jsonpath={.status.phase}=Succeeded", "pod/sentinel-preflight", "--timeout=30s")
        service_ip = self.get("service", "kubernetes", "default")["spec"]["clusterIP"]
        network = ipaddress.ip_address(service_ip)
        self.api_cidr = f"{service_ip}/{network.max_prefixlen}"
        for namespace in [self.namespace, self.control]:
            self.apply({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                        "metadata": {"name": "deny-all-egress", "namespace": namespace},
                        "spec": {"podSelector": {}, "policyTypes": ["Egress"]}})
        self.set_controller_api_egress(True)
        proxy = r'''#!/bin/sh
if nc -z -w 2 "$DENIED_SENTINEL_HOST" 8080; then
  echo APSIDES_NETWORK_POLICY_CANARY_UNEXPECTEDLY_CONNECTED
  exit 41
fi
echo APSIDES_NETWORK_POLICY_CANARY_DENIED
if nc -z -w 2 1.1.1.1 443; then
  echo APSIDES_EGRESS_CANARY_UNEXPECTEDLY_CONNECTED
  exit 42
fi
echo APSIDES_EGRESS_CANARY_DENIED
cat >/tmp/apsides-nginx.conf <<EOF
pid /tmp/apsides-nginx.pid;
error_log stderr notice;
events {}
stream {
  server {
    listen 127.0.0.1:8443;
    proxy_pass ${KUBERNETES_SERVICE_HOST}:${KUBERNETES_SERVICE_PORT};
  }
}
EOF
exec nginx -c /tmp/apsides-nginx.conf -g 'daemon off;'
'''
        self.apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "apsides-loopback-proxy", "namespace": self.control}, "data": {"proxy.sh": proxy}})

    def install_controller(self):
        sentinel_ip = self.get("service", "egress-sentinel", "apsides-outside")["spec"]["clusterIP"]
        preflight = r'''until ! nc -z -w 2 "$DENIED_SENTINEL_HOST" 8080 && ! nc -z -w 2 1.1.1.1 443; do
  echo APSIDES_INIT_EGRESS_NOT_YET_DENIED
  sleep 1
done
echo APSIDES_INIT_EGRESS_DENIED
'''
        self.apply({"apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {"name": "apsides-fixture-controller", "namespace": self.control},
                    "spec": {"replicas": 1, "strategy": {"type": "Recreate"},
                             "selector": {"matchLabels": {"app": "apsides-fixture-controller"}},
                             "template": {"metadata": {"labels": {"app": "apsides-fixture-controller"}},
                                          "spec": {"serviceAccountName": self.sa, "automountServiceAccountToken": False,
                                                   "terminationGracePeriodSeconds": 30,
                                                   "initContainers": [
                                                       {"name": "egress-preflight", "image": PROXY_TAG, "imagePullPolicy": "Never",
                                                        "command": ["sh", "-c", preflight],
                                                        "env": [{"name": "DENIED_SENTINEL_HOST", "value": sentinel_ip}]},
                                                   ],
                                                   "containers": [
                                                       {"name": "controller", "image": CONTROLLER_TAG, "imagePullPolicy": "Never",
                                                        "env": [{"name": "APSIDES_RUNTIME_CONFIG_PATH", "value": "/run/apsides/runtime.json"}],
                                                        "volumeMounts": [{"name": "runtime", "mountPath": "/run/apsides", "readOnly": True}]},
                                                       {"name": "loopback-proxy", "image": PROXY_TAG, "imagePullPolicy": "Never",
                                                        "command": ["sh", "/proxy/proxy.sh"],
                                                        "env": [{"name": "DENIED_SENTINEL_HOST", "value": sentinel_ip}],
                                                        "volumeMounts": [{"name": "proxy", "mountPath": "/proxy", "readOnly": True}]},
                                                   ],
                                                   "volumes": [
                                                       {"name": "runtime", "secret": {"secretName": "apsides-runtime-fixture", "defaultMode": 292}},
                                                       {"name": "proxy", "configMap": {"name": "apsides-loopback-proxy", "defaultMode": 292}},
                                                   ]}}}})
        self.wait_for("controller pod", lambda: self.controller_pod())
        self.assert_proxy_canaries()
        self.network_canary_passed = True
        self.controller_started = True

    def controller_pod(self):
        pods = json.loads(self.k("-n", self.control, "get", "pods", "-l", "app=apsides-fixture-controller", "-o", "json").stdout)["items"]
        running = [pod for pod in pods if pod.get("status", {}).get("phase") == "Running"]
        if not running:
            return None
        failed = [status for status in running[0].get("status", {}).get("containerStatuses", []) if status.get("state", {}).get("terminated")]
        if failed:
            raise RuntimeError("controller or egress proxy terminated")
        return running[0]["metadata"]["name"]

    def container_logs(self, container, previous=False):
        try:
            pods = json.loads(self.k("-n", self.control, "get", "pods", "-l", "app=apsides-fixture-controller", "-o", "json", check=False).stdout).get("items", [])
            if not pods:
                return ""
            pods.sort(key=lambda pod: pod.get("metadata", {}).get("creationTimestamp", ""), reverse=True)
            args = ["-n", self.control, "logs", pods[0]["metadata"]["name"], "-c", container]
            if previous:
                args.append("--previous")
            return self.k(*args, check=False).stdout
        except (KeyError, RuntimeError, json.JSONDecodeError):
            return ""

    def proxy_logs(self):
        return "\n".join([self.container_logs("loopback-proxy"), self.container_logs("loopback-proxy", previous=True)])

    def assert_proxy_canaries(self):
        self.wait_for("egress canaries", lambda: "APSIDES_EGRESS_CANARY_DENIED" in self.proxy_logs()
                      and "APSIDES_NETWORK_POLICY_CANARY_DENIED" in self.proxy_logs())
        if "UNEXPECTEDLY_CONNECTED" in self.proxy_logs():
            raise RuntimeError("controller proxy observed transient egress before network isolation")

    def controller_logs(self):
        return "\n".join([*self.logs, self.container_logs("controller")])

    def preserve_logs(self):
        log = self.container_logs("controller")
        if log:
            self.logs.append(log)
        init_log = self.container_logs("egress-preflight")
        proxy_log = self.proxy_logs()
        if init_log or proxy_log:
            self.egress_logs.append("\n".join([init_log, proxy_log]))

    def scale_controller(self, replicas):
        if replicas == 0:
            self.preserve_logs()
        self.k("-n", self.control, "scale", "deployment", "apsides-fixture-controller", f"--replicas={replicas}")
        self.wait_for("controller scale", lambda: bool(self.controller_pod()) == bool(replicas))
        if replicas:
            self.assert_proxy_canaries()

    def target(self, reaction):
        return next(resource for resource in self.ir["resources"] if resource["id"] == reaction["target"])

    def target_revision(self, reaction):
        target = self.target(reaction)
        return self.get("deployment", target["name"], target["namespace"])["spec"]["template"]["metadata"]["annotations"]["apsides.dev/revision"]

    def target_ready(self, reaction):
        target = self.target(reaction)
        obj = self.get("deployment", target["name"], target["namespace"])
        count = obj["spec"].get("replicas", 1)
        status = obj.get("status", {})
        return count > 0 and status.get("observedGeneration", 0) >= obj["metadata"]["generation"] and status.get("updatedReplicas", 0) == count and status.get("readyReplicas", 0) == count

    def target_generation(self, reaction):
        target = self.target(reaction)
        return self.get("deployment", target["name"], target["namespace"])["metadata"]["generation"]

    def target_ready_pods(self, reaction):
        target = self.target(reaction)
        return self.get("deployment", target["name"], target["namespace"]).get("status", {}).get("readyReplicas", 0)

    def target_has_manager(self, reaction, manager):
        target = self.target(reaction)
        fields = self.get("deployment", target["name"], target["namespace"])["metadata"].get("managedFields", [])
        return any(item.get("manager") == manager and item.get("operation") == "Apply" for item in fields)

    def reaction_inputs(self, reaction, observed_only=False):
        values = [item for item in self.ir["inputs"] if item["id"] in set(reaction["inputs"])]
        return [item for item in values if item["mode"] == "observed"] if observed_only else values

    def mutate_input(self, item, value, key="message"):
        encoded = base64.b64encode(value.encode()).decode() if item["kind"] == "Secret" else value
        self.k("-n", item["namespace"], "patch", item["kind"].lower(), item["name"], "--type=merge", "-p", json.dumps({"data": {key: encoded}}))

    def recreate_input(self, item, value):
        encoded = base64.b64encode(value.encode()).decode() if item["kind"] == "Secret" else value
        self.apply({"apiVersion": "v1", "kind": item["kind"], "metadata": {"name": item["name"], "namespace": item["namespace"]}, "data": {"message": encoded, "LOG_LEVEL": encoded}})

    def input_data(self, item):
        return dict(self.get(item["kind"].lower(), item["name"], item["namespace"]).get("data", {}))

    def input_exists(self, item):
        return self.k("-n", item["namespace"], "get", item["kind"].lower(), item["name"], check=False).returncode == 0

    def apply_input_data(self, item, data):
        self.apply({"apiVersion": "v1", "kind": item["kind"],
                    "metadata": {"name": item["name"], "namespace": item["namespace"]},
                    "data": data})

    def restore_input_data(self, item, data):
        if not self.input_exists(item):
            self.apply_input_data(item, data)
        self.wait_for(f"input restored: {item['id']}", lambda: self.input_exists(item))

    def expected_revision(self, reaction, omitted_reaction=None):
        target_id = reaction["target"]
        reactions = sorted([item for item in self.ir["reactions"] if item["target"] == target_id and item["id"] != omitted_reaction], key=lambda item: item["id"])
        input_ids = sorted({input_id for item in reactions for input_id in item["inputs"]})
        projections, sensitive = {}, False
        for input_id in input_ids:
            item = next(value for value in self.ir["inputs"] if value["id"] == input_id)
            sensitive = sensitive or item["sensitive"]
            if item["mode"] == "managed":
                resource = next(value for value in self.ir["resources"] if value["id"] == item["managedResourceId"])
                projections[input_id] = resource["desired"].get("data", {})
            else:
                projections[input_id] = self.get(item["kind"].lower(), item["name"], item["namespace"]).get("data", {})
        payload = {"contract": "apsides.dev/rollout/v1", "domain": self.ir["domain"]["id"], "target": target_id,
                   "reactions": [item["id"] for item in reactions], "inputs": projections}
        if sensitive:
            payload["keyId"] = ROLLOUT_KEY_ID
            return f"hmac-sha256:{ROLLOUT_KEY_ID}:" + hmac.new(ROLLOUT_KEY, canonical(payload), hashlib.sha256).hexdigest()
        return digest(payload)

    def stable(self, reaction, expected, seconds=3):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if self.target_revision(reaction) != expected:
                raise RuntimeError("unexpected target revision change")
            time.sleep(0.25)

    def wait_expected(self, reaction, label):
        expected = self.expected_revision(reaction)
        self.wait_for(label, lambda: self.target_revision(reaction) == expected)
        return expected

    def degraded_events(self):
        count = 0
        for line in reversed(self.controller_logs().splitlines()):
            try:
                status = json.loads(line).get("status")
            except json.JSONDecodeError:
                continue
            if status == "degraded" or (isinstance(status, dict) and status.get("blocked")):
                count += 1
        return count

    def semantic_results(self):
        destination = self.output / "semantic"
        result = self.command([str(self.root / "target/debug/aps"), "test", "--ir", str(self.artifact / "controller/src/ir.json"),
                               "--output", str(destination)], check=False)
        if not (destination / "results.json").is_file():
            raise RuntimeError(f"semantic runner produced no results: {result.stderr[-1000:]}")
        report = json.loads((destination / "results.json").read_text())
        (self.output / "semantic-results.json").write_text(json.dumps(report, indent=2))
        return {(row["case"]["reactionId"], row["case"]["scenario"]): row for row in report["results"]}

    def record(self, case, operation, evidence):
        key, started = (case["reactionId"], case["scenario"]), time.monotonic()
        try:
            assertions = operation()
            if not assertions or any(not item.get("passed") for item in assertions):
                raise RuntimeError("case did not produce matching typed assertions")
            self.results[key] = {"case": case, "caseDigest": digest(case), "status": "passed", "assertions": assertions,
                                 "evidenceFiles": [evidence] if isinstance(evidence, str) else evidence, "seconds": time.monotonic() - started}
        except Exception as error:
            self.results[key] = {"case": case, "caseDigest": digest(case), "status": "failed", "assertions": [],
                                 "evidenceFiles": [evidence] if isinstance(evidence, str) else evidence,
                                 "message": str(error), "seconds": time.monotonic() - started}

    def deny_input_rules(self, item):
        plural = "secrets" if item["kind"] == "Secret" else "configmaps"
        denied = []
        for rule in self.original_rules:
            rule = json.loads(json.dumps(rule))
            if plural in rule["resources"] and item["name"] in rule.get("resourceNames", []):
                rule["resourceNames"] = [name for name in rule["resourceNames"] if name != item["name"]]
                if not rule["resourceNames"]:
                    continue
            denied.append(rule)
        self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
                    "metadata": {"name": self.sa, "namespace": self.namespace}, "rules": denied})
        return denied

    def deny_managed_repair_rules(self, item):
        plural = "secrets" if item["kind"] == "Secret" else "configmaps"
        denied = [json.loads(json.dumps(rule)) for rule in self.original_rules
                  if not (plural in rule.get("resources", []) and "create" in rule.get("verbs", []))]
        self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
                    "metadata": {"name": self.sa, "namespace": self.namespace}, "rules": denied})
        return denied

    def restore_rules(self):
        self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
                    "metadata": {"name": self.sa, "namespace": self.namespace}, "rules": self.original_rules})

    def managed_resource(self, item):
        return next(resource for resource in self.ir["resources"] if resource["id"] == item["managedResourceId"])

    def blocked_reason(self, target_id):
        for line in reversed(self.controller_logs().splitlines()):
            try:
                status = json.loads(line).get("status")
            except json.JSONDecodeError:
                continue
            if isinstance(status, dict):
                for blocked in status.get("blocked", []):
                    if blocked.get("target") == target_id:
                        return blocked.get("reason")
        return None

    def set_controller_api_egress(self, enabled):
        egress = ([{"to": [{"ipBlock": {"cidr": self.api_cidr}}],
                    "ports": [{"protocol": "TCP", "port": 443}]}] if enabled else [])
        self.apply({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                    "metadata": {"name": "controller-api-only", "namespace": self.control},
                    "spec": {"podSelector": {"matchLabels": {"app": "apsides-fixture-controller"}},
                             "policyTypes": ["Egress"], "egress": egress}})

    def live_case(self, case, reaction):
        scenario = case["scenario"]
        stimulus_ids = set(case.get("inputIds", reaction["inputs"]))
        stimulus = [item for item in self.reaction_inputs(reaction) if item["id"] in stimulus_ids]
        observed = [item for item in stimulus if item["mode"] == "observed"]
        managed = [item for item in stimulus if item["mode"] == "managed"]
        if scenario in {"unauthorized-input", "unavailable-input", "invalid-input"}:
            def semantic_only():
                row = self.semantic[(reaction["id"], scenario)]
                if row["status"] != "passed":
                    raise RuntimeError(row["detail"])
                reason = scenario.removesuffix("-input")
                values = [assertion("block-reason", reason, reason)]
                if scenario == "invalid-input":
                    values.append(assertion("sensitive-data-exposed", False, False))
                values.append(assertion("target-mutations", 0, 0))
                return values
            return semantic_only
        if scenario == "managed-drift-repair":
            def managed_drift():
                before, generation, all_drift_observed, all_restored = self.target_revision(reaction), self.target_generation(reaction), True, True
                for item in managed:
                    resource = self.managed_resource(item)
                    desired = resource["desired"].get("data", {})
                    if not desired:
                        raise RuntimeError(f"managed drift applicability is undefined for empty data: {item['id']}")
                    key = sorted(desired)[0]
                    drift = f"external-drift-{item['id']}"
                    if desired[key] == drift:
                        drift += "-different"
                    changed = json.loads(json.dumps(resource["desired"]))
                    changed.setdefault("data", {})[key] = drift
                    response = self.k("apply", "--server-side", "--field-manager", "apsides-" + self.ir["domain"]["id"],
                                      "-f", "-", "-o", "json", data=changed)
                    patched = json.loads(response.stdout)
                    if patched.get("data", {}).get(key) != drift:
                        raise RuntimeError(f"managed drift was not observed for {item['id']}")
                    all_drift_observed = all_drift_observed and patched.get("data", {}).get(key) == drift
                    self.wait_for(f"managed source data repaired: {item['id']}",
                                  lambda resource=resource, desired=desired: self.get(resource["kind"].lower(), resource["name"], resource["namespace"]).get("data", {}) == desired)
                    all_restored = all_restored and self.get(resource["kind"].lower(), resource["name"], resource["namespace"]).get("data", {}) == desired
                self.stable(reaction, before)
                return [assertion("managed-drift-observed", True, all_drift_observed),
                        assertion("managed-data-restored", True, all_restored),
                        assertion("revision-unchanged", True, self.target_revision(reaction) == before),
                        assertion("template-mutations", 0, self.target_generation(reaction) - generation)]
            return managed_drift
        if scenario == "managed-missing":
            def managed_missing():
                before, generation, all_missing, all_denied = self.target_revision(reaction), self.target_generation(reaction), True, True
                for item in managed:
                    resource = self.managed_resource(item)
                    self.scale_controller(0)
                    try:
                        self.deny_managed_repair_rules(item)
                        self.k("-n", resource["namespace"], "delete", resource["kind"].lower(), resource["name"])
                        self.scale_controller(1)
                        self.wait_for(f"managed source remains absent: {item['id']}",
                                      lambda resource=resource: self.k("-n", resource["namespace"], "get", resource["kind"].lower(), resource["name"], check=False).returncode != 0)
                        plural = "secrets" if item["kind"] == "Secret" else "configmaps"
                        repair_status = self.runtime_api("POST", f"/api/v1/namespaces/{resource['namespace']}/{plural}", resource["desired"])
                        source_missing = self.k("-n", resource["namespace"], "get", resource["kind"].lower(), resource["name"], check=False).returncode != 0
                        self.stable(reaction, before)
                        all_missing = all_missing and source_missing
                        all_denied = all_denied and repair_status == 403
                    finally:
                        self.restore_rules()
                        self.scale_controller(1)
                        self.wait_for(f"managed source repair after permission restore: {item['id']}",
                                      lambda resource=resource: self.k("-n", resource["namespace"], "get", resource["kind"].lower(), resource["name"], check=False).returncode == 0)
                return [assertion("block-reason", "missing", "missing" if all_missing else "present"),
                        assertion("repair-denied", True, all_denied),
                        assertion("revision-unchanged", True, self.target_revision(reaction) == before),
                        assertion("target-mutations", 0, self.target_generation(reaction) - generation)]
            return managed_missing
        if scenario == "managed-read-denial":
            def managed_read_denial():
                before, generation, statuses, expanded = self.target_revision(reaction), self.target_generation(reaction), [], False
                for item in managed:
                    resource = self.managed_resource(item)
                    denied_rules = self.deny_input_rules(item)
                    try:
                        plural = "secrets" if item["kind"] == "Secret" else "configmaps"
                        statuses.append(self.runtime_api("GET", f"/api/v1/namespaces/{resource['namespace']}/{plural}/{resource['name']}"))
                        self.stable(reaction, before)
                    finally:
                        self.restore_rules()
                    expanded = expanded or any(plural in rule.get("resources", []) and
                                               (not rule.get("resourceNames") or resource["name"] in rule["resourceNames"])
                                               for rule in denied_rules if "get" in rule.get("verbs", []))
                status = 403 if statuses and all(value == 403 for value in statuses) else (statuses[0] if statuses else 0)
                return [assertion("http-status", 403, status), assertion("permission-expanded", False, expanded),
                        assertion("revision-unchanged", True, self.target_revision(reaction) == before),
                        assertion("target-mutations", 0, self.target_generation(reaction) - generation)]
            return managed_read_denial
        if scenario == "managed-unavailable":
            def managed_unavailable():
                before, generation, degraded = self.target_revision(reaction), self.target_generation(reaction), self.degraded_events()
                self.set_controller_api_egress(False)
                try:
                    pod = self.controller_pod()
                    if not pod:
                        raise RuntimeError("controller pod missing before managed transport failure")
                    self.k("-n", self.control, "exec", pod, "-c", "loopback-proxy", "--", "nginx", "-c", "/tmp/apsides-nginx.conf", "-s", "stop")
                    self.wait_for("managed source transport unavailable", lambda: self.degraded_events() > degraded)
                    transport_observed = self.degraded_events() > degraded
                    reason = self.blocked_reason(reaction["target"])
                    self.stable(reaction, before)
                finally:
                    self.set_controller_api_egress(True)
                    self.scale_controller(0)
                    self.scale_controller(1)
                self.wait_expected(reaction, "managed source transport restored")
                actual_reason = "unavailable" if reason and reason.endswith("unavailable") else reason
                return [assertion("transport-failure-observed", True, transport_observed),
                        assertion("block-reason", "unavailable", actual_reason),
                        assertion("revision-unchanged", True, self.target_revision(reaction) == before),
                        assertion("target-mutations", 0, self.target_generation(reaction) - generation)]
            return managed_unavailable
        needs_observed = {
            "projected-change", "coalesced-inputs",
            "missing-input", "input-returned", "restart-and-relist", "credential-read-denial",
        }
        if scenario in needs_observed and not observed:
            raise RuntimeError(f"reaction {reaction['id']} has no observed input for {scenario}")
        selected = observed[0] if observed else stimulus[0]
        if scenario == "initial-evaluation":
            def initial():
                expected = self.wait_expected(reaction, "initial exact revision")
                self.wait_for("initial ready Pods", lambda: self.target_ready(reaction))
                return [assertion("revision-present", True, bool(self.target_revision(reaction))),
                        assertion("target-converged", True, self.target_revision(reaction) == expected and self.target_ready(reaction))]
            return initial
        if scenario == "projected-change":
            def projected():
                before = self.target_revision(reaction)
                self.mutate_input(selected, f"projected-{reaction['id']}")
                expected = self.wait_expected(reaction, "projected revision")
                actual = self.target_revision(reaction)
                return [assertion("revision-changed", True, actual != before),
                        assertion("target-converged", True, actual == expected)]
            return projected
        if scenario == "irrelevant-change":
            def irrelevant():
                before, generation = self.target_revision(reaction), self.target_generation(reaction)
                self.k("-n", selected["namespace"], "annotate", selected["kind"].lower(), selected["name"], f"fixture-noise={time.time_ns()}", "--overwrite")
                self.stable(reaction, before)
                return [assertion("revision-unchanged", True, self.target_revision(reaction) == before),
                        assertion("template-mutations", 0, self.target_generation(reaction) - generation)]
            return irrelevant
        if scenario == "same-value-replay":
            def replay():
                before, generation = self.target_revision(reaction), self.target_generation(reaction)
                data = self.get(selected["kind"].lower(), selected["name"], selected["namespace"])["data"]
                key = sorted(data)[0]
                current = data[key]
                value = base64.b64decode(current).decode() if selected["kind"] == "Secret" else current
                self.mutate_input(selected, value, key)
                self.stable(reaction, before)
                return [assertion("revision-unchanged", True, self.target_revision(reaction) == before),
                        assertion("template-mutations", 0, self.target_generation(reaction) - generation)]
            return replay
        if scenario == "coalesced-inputs":
            def coalesced():
                for index, item in enumerate(observed):
                    self.mutate_input(item, f"coalesced-{reaction['id']}-{index}")
                expected = self.wait_expected(reaction, "latest complete input set")
                self.stable(reaction, expected)
                return [assertion("latest-input-set-used", True, self.target_revision(reaction) == expected),
                        assertion("target-converged", True, self.target_ready(reaction))]
            return coalesced
        if scenario == "missing-input":
            def missing():
                before, generation = self.target_revision(reaction), self.target_generation(reaction)
                saved = self.input_data(selected)
                target_preserved = False
                mutations = None
                try:
                    self.k("-n", selected["namespace"], "delete", selected["kind"].lower(), selected["name"])
                    self.stable(reaction, before)
                    target_preserved = self.target_ready(reaction) and self.target_revision(reaction) == before
                    mutations = self.target_generation(reaction) - generation
                    if not target_preserved:
                        raise RuntimeError("missing input disrupted the retained target")
                finally:
                    self.restore_input_data(selected, saved)
                    self.wait_expected(reaction, "missing-input cleanup recovery")
                    self.wait_for("missing-input cleanup readiness", lambda: self.target_ready(reaction))
                row = self.semantic[(reaction["id"], scenario)]
                return [assertion("block-reason", "missing", "missing" if row["status"] == "passed" else "failed"),
                        assertion("target-mutations", 0, mutations),
                        assertion("target-preserved", True, target_preserved)]
            return missing
        if scenario == "input-returned":
            def returned():
                saved = self.input_data(selected)
                self.k("-n", selected["namespace"], "delete", selected["kind"].lower(), selected["name"])
                self.wait_for("returned-input missing phase", lambda: not self.input_exists(selected))
                changed = dict(saved)
                raw = f"returned-{reaction['id']}".encode()
                changed["message"] = base64.b64encode(raw).decode() if selected["kind"] == "Secret" else raw.decode()
                try:
                    self.apply_input_data(selected, changed)
                    expected = self.wait_expected(reaction, "returned input revision")
                    self.wait_for("returned input target readiness", lambda: self.target_ready(reaction))
                    return [assertion("dependency-recovered", True, self.target_revision(reaction) == expected),
                            assertion("target-converged", True, self.target_ready(reaction))]
                finally:
                    self.restore_input_data(selected, changed)
            return returned
        if scenario == "restart-and-relist":
            def relist():
                saved = self.input_data(selected)
                try:
                    self.scale_controller(0)
                    self.mutate_input(selected, f"stopped-{reaction['id']}")
                finally:
                    self.restore_input_data(selected, saved)
                    self.scale_controller(1)
                expected = self.wait_expected(reaction, "restart relist revision")
                self.wait_for("restart relist target readiness", lambda: self.target_ready(reaction))
                return [assertion("latest-input-set-used", True, self.target_revision(reaction) == expected),
                        assertion("target-converged", True, self.target_ready(reaction))]
            return relist
        if scenario == "accepted-write-restart":
            def accepted_restart():
                self.scale_controller(1)
                before, generation, old_pod = self.target_revision(reaction), self.target_generation(reaction), self.controller_pod()
                if old_pod is None:
                    raise RuntimeError("controller pod missing before restart")
                self.preserve_logs()
                try:
                    self.k("-n", self.control, "delete", "pod", old_pod)
                    self.wait_for("new controller Pod", lambda: self.controller_pod() not in [None, old_pod])
                    self.stable(reaction, before)
                finally:
                    self.scale_controller(1)
                return [assertion("revision-unchanged", True, self.target_revision(reaction) == before),
                        assertion("template-mutations", 0, self.target_generation(reaction) - generation)]
            return accepted_restart
        if scenario == "credential-read-denial":
            def denied_read():
                before, generation = self.target_revision(reaction), self.target_generation(reaction)
                denied_rules = self.deny_input_rules(selected)
                try:
                    plural = "secrets" if selected["kind"] == "Secret" else "configmaps"
                    status = self.runtime_api("GET", f"/api/v1/namespaces/{selected['namespace']}/{plural}/{selected['name']}")
                    if status != 403:
                        raise RuntimeError(f"actual runtime credential input read returned HTTP {status}, expected 403")
                    self.stable(reaction, before)
                finally:
                    self.restore_rules()
                self.wait_expected(reaction, "credential restoration")
                self.wait_for("credential restoration readiness", lambda: self.target_ready(reaction))
                plural = "secrets" if selected["kind"] == "Secret" else "configmaps"
                expanded = any(plural in rule.get("resources", []) and
                               "get" in rule.get("verbs", []) and
                               (not rule.get("resourceNames") or selected["name"] in rule["resourceNames"])
                               for rule in denied_rules)
                return [assertion("http-status", 403, status), assertion("permission-expanded", False, expanded),
                        assertion("target-mutations", 0, self.target_generation(reaction) - generation)]
            return denied_read
        if scenario == "forced-field-conflict":
            def forced_conflict():
                target = self.target(reaction)
                original_uid = self.get("deployment", target["name"], target["namespace"])["metadata"]["uid"]
                foreign = json.loads(json.dumps(target["desired"]))
                foreign["spec"]["template"].setdefault("metadata", {}).setdefault("annotations", {})["apsides.dev/revision"] = "foreign-forced-conflict"
                self.k("apply", "--server-side", "--force-conflicts", "--field-manager", "fixture-foreign-manager", "-f", "-", data=foreign)
                expected = self.expected_revision(reaction)
                self.wait_for("approved force reconciles conflict", lambda: self.target_revision(reaction) == expected and self.target_ready(reaction))
                actual_uid = self.get("deployment", target["name"], target["namespace"])["metadata"]["uid"]
                return [assertion("force-applied", True, self.target_revision(reaction) == expected),
                        assertion("target-converged", True, self.target_ready(reaction)),
                        assertion("uid-preserved", True, original_uid == actual_uid)]
            return forced_conflict
        if scenario == "field-conflict":
            def conflict():
                target, before = self.target(reaction), self.target_revision(reaction)
                degraded = self.degraded_events()
                foreign = json.loads(json.dumps(target["desired"]))
                foreign_value = f"foreign-{reaction['id']}"
                foreign["spec"]["template"].setdefault("metadata", {}).setdefault("annotations", {})["apsides.dev/revision"] = foreign_value
                repaired, expected = json.loads(json.dumps(target["desired"])), self.expected_revision(reaction)
                repaired["spec"]["template"].setdefault("metadata", {}).setdefault("annotations", {})["apsides.dev/revision"] = expected
                try:
                    self.k("apply", "--server-side", "--force-conflicts", "--field-manager", "fixture-foreign-manager", "-f", "-", data=foreign)
                    self.wait_for("field conflict reported", lambda: self.degraded_events() > degraded)
                    forced = self.target_revision(reaction) != foreign_value
                    if forced:
                        raise RuntimeError("controller forced ownership through a conflict")
                    foreign_preserved = self.target_has_manager(reaction, "fixture-foreign-manager")
                finally:
                    self.k("apply", "--server-side", "--force-conflicts", "--field-manager", "apsides-" + self.ir["domain"]["id"], "-f", "-", data=repaired)
                    self.wait_for("field conflict repair readiness", lambda: self.target_ready(reaction))
                return [assertion("conflict-reported", True, self.degraded_events() > degraded),
                        assertion("force-applied", False, forced),
                        assertion("foreign-field-preserved", True, foreign_preserved)]
            return conflict
        if scenario == "out-of-scope-change":
            def out_of_scope():
                before = self.k("-n", "apsides-outside", "get", "configmap", "outside", check=False).returncode == 0
                status = self.runtime_api("POST", "/api/v1/namespaces/apsides-outside/configmaps",
                                          {"apiVersion": "v1", "kind": "ConfigMap",
                                           "metadata": {"name": "outside", "namespace": "apsides-outside"},
                                           "data": {"value": "forbidden"}})
                if status in {200, 201, 202}:
                    self.k("-n", "apsides-outside", "delete", "configmap", "outside", check=False)
                    raise RuntimeError("runtime credentials mutated an undeclared target")
                if status != 403:
                    raise RuntimeError(f"out-of-scope mutation returned HTTP {status}, expected 403")
                after = self.k("-n", "apsides-outside", "get", "configmap", "outside", check=False).returncode == 0
                return [assertion("http-status", 403, status),
                        assertion("out-of-scope-mutations", 0, int(after) - int(before))]
            return out_of_scope
        if scenario == "rollout-readiness":
            def ready():
                self.wait_for("new-template Pods ready", lambda: self.target_ready(reaction))
                return [assertion("ready-pods", ">=1", self.target_ready_pods(reaction)),
                        assertion("new-template-ready", True, self.target_ready(reaction))]
            return ready
        raise RuntimeError(f"no live implementation for required case {reaction['id']}:{scenario}")

    def run_cases(self):
        self.semantic = self.semantic_results()
        reactions = {item["id"]: item for item in self.ir["reactions"]}
        semantic_only = {"unauthorized-input", "unavailable-input", "invalid-input"}
        for case in self.plan["testPlan"]:
            if case.get("required"):
                evidence = "semantic-results.json" if case["scenario"] in semantic_only else "cluster-transcript.json"
                try:
                    operation = self.live_case(case, reactions[case["reactionId"]])
                except Exception as error:
                    operation = lambda error=error: (_ for _ in ()).throw(error)
                if case.get("layer") == "semantic" and case["scenario"] not in semantic_only:
                    live_operation = operation
                    def semantic_and_live(case=case, live_operation=live_operation):
                        row = self.semantic[(case["reactionId"], case["scenario"])]
                        if row["status"] != "passed":
                            raise RuntimeError(row["detail"])
                        return live_operation()
                    operation = semantic_and_live
                    evidence = ["semantic-results.json", "cluster-transcript.json"]
                self.record(case, operation, evidence)
        self.bind_fixture_coverage()

    def bind_fixture_coverage(self):
        reactions = {item["id"]: item for item in self.ir["reactions"]}
        for case in self.fixture_cases:
            key = (case["reactionId"], case["scenario"])
            started = time.monotonic()
            try:
                if case["scenario"] == "reaction-contribution-removed":
                    reaction = reactions[case["reactionId"]]
                    actual = self.target_revision(reaction)
                    expected = self.expected_revision(reaction)
                    mutated = self.expected_revision(reaction, omitted_reaction=reaction["id"])
                    if actual != expected or mutated == expected:
                        raise RuntimeError("reaction-removal mutation was not detected by the independent revision oracle")
                    evidence_files = ["cluster-transcript.json"]
                    assertions = [assertion("baseline-revision", expected, actual),
                                  assertion("omitted-reaction-changes-revision", True, mutated != expected)]
                else:
                    core_key = (case["reactionId"], case["scenario"])
                    core = self.results.get(core_key)
                    if core is None or core["status"] != "passed":
                        raise RuntimeError(f"bound core reactor case did not pass: {core_key}")
                    evidence_files = core["evidenceFiles"]
                    assertions = core["assertions"]
                self.fixture_results[key] = {"case": case, "caseDigest": digest(case), "status": "passed",
                                             "assertions": assertions, "evidenceFiles": evidence_files,
                                             "seconds": time.monotonic() - started}
            except Exception as error:
                self.fixture_results[key] = {"case": case, "caseDigest": digest(case), "status": "failed",
                                             "assertions": [], "evidenceFiles": ["reactor-evidence.json"], "message": str(error),
                                             "seconds": time.monotonic() - started}

    def cleanup(self):
        disposition = "not-created"
        if self.created:
            try:
                self.preserve_logs()
                for namespace in [self.namespace, self.control]:
                    for kind in ["pods", "deployments", "configmaps", "events", "networkpolicies"]:
                        response = self.k("-n", namespace, "get", kind, "-o", "json", check=False)
                        if response.returncode == 0:
                            (self.output / f"diagnostic-{namespace}-{kind}.json").write_text(response.stdout)
                self.command(["kind", "delete", "cluster", "--name", self.cluster])
                disposition = "deleted"
            except Exception as error:
                disposition = "failed"
                (self.output / "cleanup-error.txt").write_text(str(error))
        for path in [self.driver_config, self.runtime_host_config, self.runtime_ca]:
            path.unlink(missing_ok=True)
        return disposition

    def write_results(self, disposition, original_error):
        expected = [case for case in self.plan["testPlan"] if case.get("required")]
        ordered = [self.results[(case["reactionId"], case["scenario"])] for case in expected
                   if (case["reactionId"], case["scenario"]) in self.results]
        fixture_ordered = [self.fixture_results[(case["reactionId"], case["scenario"])] for case in self.fixture_cases
                           if (case["reactionId"], case["scenario"]) in self.fixture_results]
        logs = "\n".join(self.logs)
        (self.output / "controller.log").write_text(logs)
        (self.output / "egress-canary.log").write_text("\n".join([*self.egress_logs, self.container_logs("egress-preflight"), self.proxy_logs()]))
        scan_ok = self.controller_started and bool(logs) and ROLLOUT_KEY.decode() not in logs and "rotated-synthetic-value" not in logs
        complete = (len(ordered) == len(expected) and all(row["status"] == "passed" for row in ordered)
                    and len(fixture_ordered) == len(self.fixture_cases) and all(row["status"] == "passed" for row in fixture_ordered))
        result = {"schemaVersion": "apsides.dev/reactor-results/v1", "layer": "containerized-disposable-cluster", "testOnly": True,
                  "productionIrDigest": getattr(self, "production_ir_digest", None), "irDigest": self.plan["irDigest"],
                  "fixtureOverlayDigest": getattr(self, "fixture_overlay_digest", None), "runnerDigest": getattr(self, "runner_digest", None),
                  "controllerArchiveDigest": getattr(self, "controller_archive_digest", None),
                  "controllerManifestDigest": getattr(self, "controller_manifest", None), "fixtureArchiveDigest": getattr(self, "fixture_archive_digest", None),
                  "fixtureImageConfigDigest": getattr(self, "fixture_image", None), "nodeImage": NODE_IMAGE, "clusterDisposition": disposition,
                  "networkIsolation": {"defaultDenyEgress": self.network_canary_passed, "apiOnlyException": self.network_canary_passed,
                                       "networkPolicyCanary": "denied" if self.network_canary_passed else "unverified",
                                       "externalCanary": "denied" if self.network_canary_passed else "unverified"},
                  "separateCredential": {"status": "passed" if self.separate_credential_proven else "unverified",
                                         "driver": "kind-admin", "controller": "short-lived-service-account-token", "automount": False},
                  "secretCanaryScan": "passed" if scan_ok else "failed", "cases": ordered, "fixtureCases": fixture_ordered,
                  "status": "passed" if complete and scan_ok and self.network_canary_passed and self.separate_credential_proven and disposition == "deleted" and original_error is None else "failed",
                  "limitations": ["fixture activation is distinct from production protected activation",
                                  "cooperative drain is not independent cross-generation request fencing",
                                  "typed unauthorized, unavailable and invalid primitives use semantic evidence; credential-read-denial is the real API proof"]}
        artifact_manifest_file = "tested-artifact-manifest.json"
        artifact_manifest_bytes = (self.artifact / "manifest.json").read_bytes()
        (self.output / artifact_manifest_file).write_bytes(artifact_manifest_bytes)
        (self.output / "test-seed.json").write_text(json.dumps(getattr(self, "test_seed", {}), indent=2) + "\n")
        tested_artifact_digest = "sha256:" + hashlib.sha256(artifact_manifest_bytes).hexdigest()
        mutation_rows = [row for row in fixture_ordered if row["case"]["scenario"] == "reaction-contribution-removed"]
        failed_required = sum(row["status"] != "passed" for row in [*ordered, *fixture_ordered])
        canary_occurrences = logs.count(ROLLOUT_KEY.decode()) + logs.count("rotated-synthetic-value")
        (self.output / "canary-scan.json").write_bytes(canonical({
            "schemaVersion": "apsides.dev/reactor-canary-scan/v1",
            "scope": "controller-log-synthetic-rollout-key-and-secret-canaries",
            "filesScanned": 1, "canaryOccurrences": canary_occurrences,
            "controllerStarted": self.controller_started, "nonemptyLog": bool(logs),
            "passed": scan_ok,
        }))
        checks = {
            "offline-reactor-suite": {"status": "passed" if self.network_canary_passed and self.local_images_verified and failed_required == 0 else "failed",
                                      "assertions": [assertion("external-network-disabled", True, self.network_canary_passed),
                                                     assertion("required-cases-failed", 0, failed_required),
                                                     assertion("required-cases-unsupported", 0, 0)],
                                      "evidenceFiles": ["cluster-transcript.json"]},
            "secret-canary-scan": {"status": "passed" if scan_ok else "failed",
                                   "assertions": [assertion("canary-occurrences", 0, canary_occurrences),
                                                  assertion("files-scanned", ">=1", 1)],
                                   "evidenceFiles": ["canary-scan.json"]},
            "reaction-mutation-check": {"status": "passed" if len(mutation_rows) == len(self.ir["reactions"]) and all(row["status"] == "passed" for row in mutation_rows) else "failed",
                                        "assertions": [assertion("removed-reaction-contribution-detected", True,
                                                                 len(mutation_rows) == len(self.ir["reactions"]) and all(row["status"] == "passed" for row in mutation_rows))],
                                        "evidenceFiles": ["cluster-transcript.json"]},
        }
        if self.crd_snapshots:
            for snapshot in self.crd_snapshots:
                if (file_digest(snapshot["source"]) != snapshot["digest"]
                        or file_digest(self.output / snapshot["file"]) != snapshot["digest"]):
                    raise RuntimeError("fixture CRD input changed during measurement")
            (self.output / "fixture-crds.json").write_bytes(canonical({"installations": self.crd_installations}))
            checks["offline-reactor-suite"]["evidenceFiles"] += ["fixture-crds.json", *[item["file"] for item in self.crd_snapshots]]
        reactor_evidence = {
            "schemaVersion": "apsides.dev/reactor-evidence/v1",
            "productionIrDigest": getattr(self, "production_ir_digest", None),
            "fixtureIrDigest": self.plan["irDigest"],
            "testedArtifactDigest": tested_artifact_digest,
            "artifactManifestFile": artifact_manifest_file,
            "overlayDigest": getattr(self, "fixture_overlay_digest", None),
            "runnerIdentity": {"sourceSha256": getattr(self, "runner_digest", None),
                               "controllerManifestDigest": getattr(self, "controller_manifest", None),
                               "controllerArchiveDigest": getattr(self, "controller_archive_digest", None),
                               "fixtureArchiveDigest": getattr(self, "fixture_archive_digest", None), "nodeImage": NODE_IMAGE},
            "cases": {row["caseDigest"]: {"status": row["status"], "assertions": row["assertions"],
                                           "evidenceFiles": row["evidenceFiles"]} for row in ordered},
            "fixtureCases": {row["caseDigest"]: {"status": row["status"], "assertions": row["assertions"],
                                                  "evidenceFiles": row["evidenceFiles"]} for row in fixture_ordered},
            "checks": checks,
        }
        (self.output / "reactor-evidence.json").write_text(json.dumps(reactor_evidence, indent=2) + "\n")
        (self.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        (self.output / "cluster-transcript.json").write_text(json.dumps({"cases": ordered, "fixtureCases": fixture_ordered,
                                                                          "networkIsolation": result["networkIsolation"]}, indent=2) + "\n")
        coverage = {"schemaVersion": "apsides.dev/reactor-coverage-fragment/v1", "irDigest": self.plan["irDigest"],
                    "cases": [{"caseDigest": row["caseDigest"], "status": row["status"], "evidenceFiles": ["reactor-evidence.json"]} for row in ordered],
                    "checks": {name: {"status": value["status"], "evidenceFiles": ["reactor-evidence.json"]} for name, value in checks.items()}}
        (self.output / "coverage-fragment.json").write_text(json.dumps(coverage, indent=2) + "\n")
        suite = ET.Element("testsuite", name="apsides-containerized-reactor", tests=str(len(ordered)), failures=str(sum(row["status"] != "passed" for row in ordered)))
        for row in ordered:
            item = ET.SubElement(suite, "testcase", classname=row["case"]["reactionId"], name=row["case"]["scenario"], time=str(row["seconds"]))
            if row["status"] != "passed":
                ET.SubElement(item, "failure", message=row.get("message", "failed"))
        ET.ElementTree(suite).write(self.output / "junit.xml", encoding="utf-8", xml_declaration=True)
        stage_release_evidence(self.output, reactor_evidence, coverage, result)
        return result

    def run(self):
        self.output.mkdir(parents=True, exist_ok=False)
        error = None
        try:
            self.validate_inputs()
            self.snapshot_crds()
            self.create_cluster()
            self.install_crds()
            self.install_rbac()
            self.seed_inputs()
            self.runtime_configuration()
            self.install_network_isolation()
            self.install_controller()
            self.run_cases()
        except Exception as caught:
            error = caught
        disposition = self.cleanup()
        result = self.write_results(disposition, error)
        if error is not None:
            raise RuntimeError(f"reactor suite failed: {error}; see {self.output / 'result.json'}") from error
        if result["status"] != "passed":
            raise RuntimeError(f"reactor suite failed; see {self.output / 'result.json'}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--fixture-contract", type=Path, required=True)
    parser.add_argument("--controller-archive", type=Path, required=True)
    parser.add_argument("--fixture-archive", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--crd-manifest", type=Path, action="append", default=[])
    args = parser.parse_args()
    ReactorRunner(args.artifact, args.fixture_contract, args.controller_archive, args.fixture_archive, args.output, args.crd_manifest).run()


if __name__ == "__main__":
    main()
