#!/usr/bin/env python3
"""Run the exact signed CNPG candidate pair on a fresh, owned kind cluster."""
import argparse
import base64
from contextlib import contextmanager
import http.client
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

import run_deployment_ceiling as base


shared = base.shared
ROOT = base.ROOT
SCHEMA = "reakt.dev/permission-upgrade-result/v1"
PREFIX = "APS_PERMISSION_UPGRADE_RESULT="
PAIR_MARKER = "reakt-candidate-pair-qualification"
PAIR_SCHEMA = "reakt.dev/candidate-pair-qualification/v1"
MAX_OPERATOR_BYTES = 8 * 1024 * 1024
MAX_HTTP_BYTES = 64 * 1024
EXPECTED_SCOPE = {
    "domainId": "database-demo",
    "clusterBinding": "local-demo",
    "controlNamespace": "apsides-system",
    "workloadNamespace": "default",
}
PAIR_DIGESTS = {
    "initialReleaseDigest",
    "initialIrDigest",
    "initialProvenanceDigest",
    "successorReleaseDigest",
    "successorIrDigest",
    "successorProvenanceDigest",
    "trustPolicyDigest",
}
PREPARED = {
    "generation", "transition", "registryUid", "registryResourceVersion",
    "requestDigest", "policyRole",
}
REGISTRY = {
    "transition", "generation", "phase", "registryUid",
    "registryResourceVersion", "requestDigest",
}
FIELDS = {
    "verified-pair": set(EXPECTED_SCOPE) | {"initialCapabilityCount", "successorCapabilityCount"},
    "bootstrapped": {"registryUid", "registryResourceVersion"},
    "initial-prepared": PREPARED,
    "initial-reserved": REGISTRY,
    "initial-stage-pending": REGISTRY,
    "initial-staged": REGISTRY,
    "initial-activation-pending": REGISTRY,
    "initial-activated": REGISTRY,
    "initial-inventory-published": {"transition", "inventoryGeneration", "inventorySnapshotId", "inventoryRevision"},
    "initial-resolved": {"transition", "generation", "phase", "registryUid", "registryResourceVersion"},
    "rejection-prepared": PREPARED,
    "upgrade-rejected": {"registryUid", "registryResourceVersion", "requestDigest", "missingCapabilityDigest"},
    "successor-prepared": PREPARED,
    "successor-reserved": REGISTRY,
    "successor-stage-pending": REGISTRY,
    "successor-staged": REGISTRY,
    "upgrade-quiescence-pending": REGISTRY,
    "upgrade-quiesced": REGISTRY,
    "upgrade-activation-pending": REGISTRY,
    "upgrade-activated": REGISTRY,
    "upgrade-commit-pending": REGISTRY,
    "upgrade-committed": REGISTRY,
    "successor-inventory-published": {"transition", "inventoryGeneration", "inventorySnapshotId", "inventoryRevision"},
    "successor-resolved": {"transition", "generation", "phase", "registryUid", "registryResourceVersion"},
}
LIFECYCLE = {
    "stage-initial": ("initial-staged", "initial-stage-pending", "Staged"),
    "activate-initial": ("initial-activated", "initial-activation-pending", "Committed"),
    "stage-successor": ("successor-staged", "successor-stage-pending", "Staged"),
    "quiesce-upgrade": ("upgrade-quiesced", "upgrade-quiescence-pending", "Quiesced"),
    "activate-upgrade": ("upgrade-activated", "upgrade-activation-pending", "Cleaning"),
    "commit-upgrade": ("upgrade-committed", "upgrade-commit-pending", "Committed"),
}


def result_record(output, expected):
    records = [line.removeprefix(PREFIX) for line in output.splitlines() if line.startswith(PREFIX)]
    if len(records) != 1 or len(records[0].encode()) > 128 * 1024:
        raise shared.QualificationError("permission upgrade driver did not emit one bounded result")
    value = json.loads(records[0], object_pairs_hook=shared.unique_object)
    outcomes = {expected} if isinstance(expected, str) else set(expected)
    outcome = value.get("outcome") if isinstance(value, dict) else None
    if outcome not in outcomes or outcome not in FIELDS:
        raise shared.QualificationError("permission upgrade driver returned an unexpected outcome")
    if set(value) != FIELDS[outcome] | {"schemaVersion", "outcome", "candidates"} or value["schemaVersion"] != SCHEMA:
        raise shared.QualificationError("permission upgrade driver result has an unexpected contract")
    candidates = value["candidates"]
    if not isinstance(candidates, dict) or set(candidates) != PAIR_DIGESTS:
        raise shared.QualificationError("permission upgrade result has an invalid candidate pair")
    if any(not isinstance(item, str) or not shared.valid_digest(item) for item in candidates.values()):
        raise shared.QualificationError("permission upgrade result has an invalid candidate digest")
    for key, item in value.items():
        if key in {"schemaVersion", "outcome", "candidates"}:
            continue
        if key in {"initialCapabilityCount", "successorCapabilityCount"}:
            if type(item) is not int or item < 1:
                raise shared.QualificationError("permission upgrade result has an invalid capability count")
        elif not isinstance(item, str) or len(item) > 1024 or (not item and key != "requestDigest"):
            raise shared.QualificationError("permission upgrade result has invalid metadata")
    for key in {"requestDigest", "missingCapabilityDigest"} & set(value):
        if value[key] and not shared.valid_digest(value[key]):
            raise shared.QualificationError("permission upgrade result has an invalid digest")
    return value


def full_policy(plan):
    ceiling = base.ceiling(plan)
    return {"schemaVersion": "reakt.dev/platform/v1", "runtimeCeiling": ceiling, "cleanupCeiling": ceiling}


def resource_projection(resource):
    return {key: resource.get(key) for key in [
        "apiVersion", "kind", "apiResource", "name", "namespace", "desired", "lifecycle",
    ]}


def validate_demo_pair(initial, successor):
    initial_resources = {(item["kind"], item["name"]): item for item in initial["ir"]["resources"]}
    successor_resources = {(item["kind"], item["name"]): item for item in successor["ir"]["resources"]}
    if set(initial_resources) != {("Cluster", "database"), ("ConfigMap", "api-config")}:
        raise shared.QualificationError("initial candidate is not the bounded CNPG database deployment")
    if set(successor_resources) != set(initial_resources) | {
            ("Deployment", "database-api"), ("Service", "database-api")}:
        raise shared.QualificationError("successor candidate is not the bounded CNPG API upgrade")
    if any(resource_projection(initial_resources[key]) != resource_projection(successor_resources[key])
           for key in initial_resources):
        raise shared.QualificationError("successor changed a retained initial resource")
    observed = {(item["kind"], item["name"], item["namespace"])
                for item in successor["ir"]["inputs"] if item["mode"] == "observed"}
    if observed != {("ConfigMap", "api-settings", "default"), ("Secret", "database-app", "default")}:
        raise shared.QualificationError("successor does not bind the exact observed ConfigMap and Secret")
    if initial["ir"]["inputs"]:
        raise shared.QualificationError("initial candidate unexpectedly depends on observed state")


def missing_capability(initial, successor):
    previous = {shared.canonical(item) for item in initial["capabilities"]}
    added = [item for item in successor["capabilities"] if shared.canonical(item) not in previous]
    if not added:
        raise shared.QualificationError("successor has no added capability")
    return added[0]


class CnpgDemoRunner(base.CeilingRunner):
    def __init__(self, args):
        args.artifact = args.successor_artifact
        self.initial_artifact = args.initial_artifact
        self.successor_artifact = args.successor_artifact
        self.operator_manifest = args.operator_manifest
        self.operator_sha256 = args.operator_sha256
        self.retain_on_success = args.retain_on_success
        for path in [self.initial_artifact, self.operator_manifest]:
            if not path.is_absolute() or path.is_symlink():
                raise shared.QualificationError("CNPG demo inputs must be absolute nonsymlink paths")
        if (not self.operator_manifest.is_file() or self.operator_manifest.stat().st_size <= 0
                or self.operator_manifest.stat().st_size > MAX_OPERATOR_BYTES
                or not shared.valid_digest(self.operator_sha256)):
            raise shared.QualificationError("pinned CNPG operator manifest failed digest validation")
        self.operator_bytes = self.operator_manifest.read_bytes()
        if shared.digest_bytes(self.operator_bytes) != self.operator_sha256:
            raise shared.QualificationError("pinned CNPG operator manifest failed digest validation")
        manifest_text = self.operator_bytes.decode("utf-8")
        images = set(re.findall(
            r"^\s*(?:image|value):\s*(ghcr\.io/cloudnative-pg/cloudnative-pg:1\.30\.0@sha256:[a-f0-9]{64})\s*$",
            manifest_text,
            re.MULTILINE,
        ))
        if len(images) != 1 or manifest_text.count(next(iter(images), "")) != 2:
            raise shared.QualificationError("CNPG operator manifest does not pin one exact operator image")
        self.operator_image = next(iter(images))
        super().__init__(args)
        self.operator_snapshot = self.output / "operator-manifest.yaml"
        self.operator_snapshot.write_bytes(self.operator_bytes)
        self.pair_sources = [
            Path(__file__).resolve(),
            ROOT / "crates/platform/tests/permission_upgrade.rs",
            ROOT / "crates/platform/tests/support/permission_upgrade_driver.rs",
            *sorted((ROOT / "crates/platform/tests/support/permission_upgrade").glob("*.rs")),
            ROOT / "crates/platform/tests/support/qualification_driver.rs",
        ]
        self.driver_error_reasons = set(re.findall(
            r'DriverError\(\s*"([a-zA-Z0-9 -]+)"',
            "\n".join(path.read_text() for path in self.pair_sources),
        ))
        self.failure_reasons |= set(re.findall(
            r'(?:QualificationError|RuntimeError)\(\s*"([^"\n]+)"', Path(__file__).read_text(),
        ))
        self.checks = []
        self.disposition = "not-created"

    def prepare(self):
        output = self.command([
            "cargo", "test", "--locked", "--offline", "-p", "platform", "--features", "test-support",
            "--test", "permission_upgrade", "--test", "live_candidate", "--no-run", "--message-format=json",
        ], timeout=300)
        executables = {}
        for line in output.splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            name = row.get("target", {}).get("name")
            if row.get("reason") == "compiler-artifact" and name in {"permission_upgrade", "live_candidate"} and row.get("executable"):
                executables.setdefault(name, []).append(row["executable"])
        if any(len(executables.get(name, [])) != 1 for name in ["permission_upgrade", "live_candidate"]):
            raise shared.QualificationError("Cargo did not identify the CNPG demo drivers")
        self.driver = self.output / "permission-upgrade-driver"
        self.profile_driver = self.output / "proxy-profile-driver"
        shutil.copy2(executables["permission_upgrade"][0], self.driver)
        shutil.copy2(executables["live_candidate"][0], self.profile_driver)
        self.pair = self.step("verify", "verified-pair")
        if any(self.pair[key] != value for key, value in EXPECTED_SCOPE.items()):
            raise shared.QualificationError("verified pair does not use the local CNPG demo scope")
        self.domain = self.pair["domainId"]
        self.binding = self.pair["clusterBinding"]
        self.control = self.pair["controlNamespace"]
        self.workload = self.pair["workloadNamespace"]
        self.initial_manifest = shared.read_json(self.initial_artifact / "release.json")
        self.successor_manifest = shared.read_json(self.successor_artifact / "release.json")
        self.initial_plan = shared.read_json(self.initial_artifact / "plan.json")
        self.successor_plan = shared.read_json(self.successor_artifact / "plan.json")
        subjects = [
            (self.initial_artifact, self.initial_manifest, self.initial_plan, "initialReleaseDigest"),
            (self.successor_artifact, self.successor_manifest, self.successor_plan, "successorReleaseDigest"),
        ]
        for artifact, manifest, plan, digest_key in subjects:
            if (manifest.get("mode") != "production" or plan["ir"].get("testOnly") is not False
                    or shared.digest_file(artifact / "release.json") != self.pair["candidates"][digest_key]):
                raise shared.QualificationError("CNPG demo subject differs from its verified production candidate")
        validate_demo_pair(self.initial_plan, self.successor_plan)
        self.initial_policy = full_policy(self.initial_plan)
        self.successor_policy = full_policy(self.successor_plan)
        self.initial_policy_digest = shared.digest_bytes(shared.canonical(self.initial_policy))
        self.successor_policy_digest = shared.digest_bytes(shared.canonical(self.successor_policy))
        if self.initial_policy_digest == self.successor_policy_digest:
            raise shared.QualificationError("CNPG demo policies do not identify an upgrade")
        self.missing = missing_capability(self.initial_plan, self.successor_plan)
        profile_output = self.command([
            self.profile_driver, "proxy_profile", "--exact", "--ignored", "--nocapture",
        ])
        records = [line.removeprefix("REAKT_TEST_PROFILE=") for line in profile_output.splitlines()
                   if line.startswith("REAKT_TEST_PROFILE=")]
        if len(records) != 1:
            raise shared.QualificationError("proxy profile driver did not emit one result")
        self.profile = json.loads(records[0], object_pairs_hook=shared.unique_object)
        if (set(self.profile) != {"envoyImage", "adminImage", "configurationTemplateDigest", "maxLedgerBytes"}
                or not shared.valid_digest(self.profile["configurationTemplateDigest"])
                or type(self.profile["maxLedgerBytes"]) is not int or self.profile["maxLedgerBytes"] <= 0
                or any("@sha256:" not in self.profile[key] for key in ["envoyImage", "adminImage"])):
            raise shared.QualificationError("proxy profile driver returned invalid metadata")
        self.run_proxy_canary()
        deployment = next(
            item for item in self.successor_plan["ir"]["resources"]
            if item["kind"] == "Deployment" and item["name"] == "database-api"
        )
        containers = deployment["desired"]["spec"]["template"]["spec"]["containers"]
        if len(containers) != 1 or "@sha256:" not in containers[0].get("image", ""):
            raise shared.QualificationError("signed successor does not pin one PostgREST image")
        self.workload_image = containers[0]["image"]

    def run_proxy_canary(self):
        source = ROOT / "crates/platform/tests/envoy/canary.py"
        source_bytes = source.read_bytes()
        source_digest = shared.digest_bytes(source_bytes)
        archive = self.output / "proxy-canary.py"
        archive.write_bytes(source_bytes)
        canary_output = self.output / "proxy-canary"
        self.docker_context = self.command(["docker", "context", "show"]).strip()
        if not self.docker_context or len(self.docker_context) > 256:
            raise shared.QualificationError("Docker context is invalid")
        self.docker_binding = self.docker_identity()
        before = sorted(self.command(["docker", "ps", "-aq", "--no-trunc"]).splitlines())
        try:
            output = self.command(["python3", "-I", archive, "--output", canary_output], timeout=240)
        finally:
            self.verify_docker()
            after = sorted(self.command(["docker", "ps", "-aq", "--no-trunc"]).splitlines())
            if after != before:
                raise shared.QualificationError("proxy canary changed the prior container inventory")
        records = [json.loads(line, object_pairs_hook=shared.unique_object) for line in output.splitlines()
                   if line.startswith("{")]
        result_path = canary_output / "result.json"
        if (len(records) != 1 or records[0] != {"result": "passed", "evidence": str(result_path)}
                or source.read_bytes() != source_bytes or archive.read_bytes() != source_bytes
                or not result_path.is_file() or result_path.stat().st_size > 256 * 1024):
            raise shared.QualificationError("fresh proxy environment canary was not bound to its source")
        raw = result_path.read_bytes()
        result = json.loads(raw, object_pairs_hook=shared.unique_object)
        checks = {
            "all_match_freeze_rejected_new_writes",
            "inflight_visible_until_complete",
            "client_scale_zero_scheduled_only_after_slow_response",
            "credential_overwrite_and_retry_stripping",
            "idle_with_upstream_504_rejected",
            "atomic_projected_token_rotation_observed",
            "exact_admin_filter_omits_fractional_histogram_metadata",
        }
        if (set(result) != {"image", "checks", "before", "during", "after", "ambiguous",
                            "received", "ledger", "qualification"}
                or result["image"] != self.profile["envoyImage"]
                or result["qualification"] != "canary-only; not a Kubernetes hard-drain authorization"
                or set(result["checks"]) != checks
                or any(result["checks"][name] is not True for name in checks)):
            raise shared.QualificationError("fresh proxy environment canary result is invalid")
        self.proxy_canary = {
            "sourceDigest": source_digest,
            "resultDigest": shared.digest_bytes(raw),
            "image": result["image"],
            "checks": sorted(checks),
            "qualification": result["qualification"],
        }
        result_path.chmod(0o600)

    def k(self, *args, data=None):
        return self.command(["kubectl", "--kubeconfig", self.admin, *args], data=data, timeout=360)

    def step(self, action, expected, kubeconfig=None, **extra):
        if kubeconfig == self.issuer:
            self.refresh_issuer()
        settings = {
            "action": action,
            "initialArtifact": str(self.initial_artifact),
            "successorArtifact": str(self.successor_artifact),
            "trustPolicy": str(self.trust),
            **extra,
        }
        if action != "verify":
            settings["target"] = self.target
        path = self.output / ("step-" + str(len(self.steps)) + ".json")
        path.write_bytes(shared.canonical(settings))
        environment = dict(
            os.environ,
            APS_PERMISSION_UPGRADE_STEP=str(path),
            APS_PERMISSION_UPGRADE_COORDINATION_DB=str(self.coordination),
            APS_PERMISSION_UPGRADE_INVENTORY_DB=str(self.inventory),
        )
        if kubeconfig is None:
            environment.pop("KUBECONFIG", None)
        else:
            environment["KUBECONFIG"] = str(kubeconfig)
        output = self.command([
            self.driver, "permission_upgrade_step", "--exact", "--ignored", "--nocapture",
        ], environment=environment, timeout=180)
        result = result_record(output, expected)
        if hasattr(self, "pair") and result["candidates"] != self.pair["candidates"]:
            raise shared.QualificationError("permission upgrade step changed the verified pair")
        self.steps.append(result)
        (self.output / "steps.json").write_bytes(shared.canonical(self.steps))
        return result

    def create_cluster(self):
        self.docker_context = self.command(["docker", "context", "show"]).strip()
        if not self.docker_context or len(self.docker_context) > 256:
            raise shared.QualificationError("Docker context is invalid")
        if self.docker_identity() != self.docker_binding:
            raise shared.QualificationError("Docker environment changed after the proxy canary")
        self.prior_clusters = sorted(self.cluster_names())
        self.prior_containers = sorted(self.command(["docker", "ps", "-aq", "--no-trunc"]).splitlines())
        if self.cluster in self.prior_clusters:
            raise shared.QualificationError("CNPG demo cluster name existed before creation")
        image = json.loads(self.command(["docker", "image", "inspect", base.NODE]))
        if len(image) != 1:
            raise shared.QualificationError("pinned kind image is ambiguous")
        self.node_image_id = image[0]["Id"]
        self.attempted = True
        self.command([
            "kind", "create", "cluster", "--name", self.cluster, "--image", base.NODE,
            "--kubeconfig", self.admin, "--wait", "120s",
        ], timeout=240)
        self.owned = True
        self.admin.chmod(0o600)
        containers = json.loads(self.command(["docker", "inspect", self.cluster + "-control-plane"]))
        if len(containers) != 1:
            raise shared.QualificationError("CNPG demo cluster identity is ambiguous")
        self.container_id = containers[0]["Id"]
        node = json.loads(self.k("get", "node", self.cluster + "-control-plane", "-o", "json"))
        self.node_platform = "/".join(node["status"]["nodeInfo"][key] for key in ["operatingSystem", "architecture"])
        self.host_identity = base.observations.validate_kind_target(
            node, containers[0], self.cluster, base.NODE, self.node_image_id,
        )
        control = self.create({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": self.control}})
        sentinel = self.create({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": self.cluster}})
        workload = json.loads(self.k("get", "namespace", self.workload, "-o", "json"))
        system = json.loads(self.k("get", "namespace", "kube-system", "-o", "json"))
        cluster_target = {
            "runId": self.cluster.removeprefix("reakt-qual-"),
            "kubeSystemNamespaceUid": system["metadata"]["uid"],
            "controlNamespaceUid": control["metadata"]["uid"],
            "workloadNamespaceUid": workload["metadata"]["uid"],
            "sentinelNamespaceUid": sentinel["metadata"]["uid"],
            "nodeName": node["metadata"]["name"],
            "nodeUid": node["metadata"]["uid"],
            "nodeProviderId": node["spec"]["providerID"],
            "nodeImageDigest": base.NODE.split("@")[1],
        }
        pair = self.pair["candidates"]
        marker = {
            "schemaVersion": PAIR_SCHEMA,
            "runId": cluster_target["runId"],
            "clusterName": self.cluster,
            "sentinelNamespace": self.cluster,
            **EXPECTED_SCOPE,
            **{key: value for key, value in cluster_target.items() if key != "runId"},
            **pair,
            "initialPolicyDigest": self.initial_policy_digest,
            "successorPolicyDigest": self.successor_policy_digest,
        }
        created = self.state(PAIR_MARKER, marker, self.cluster, immutable=True)
        cluster_target.update(
            markerUid=created["metadata"]["uid"],
            markerResourceVersion=created["metadata"]["resourceVersion"],
        )
        self.target = {
            "cluster": cluster_target,
            "initialPolicyDigest": self.initial_policy_digest,
            "successorPolicyDigest": self.successor_policy_digest,
        }

    def install_operator(self):
        if (self.operator_manifest.read_bytes() != self.operator_bytes
                or self.operator_snapshot.read_bytes() != self.operator_bytes):
            raise shared.QualificationError("pinned CNPG operator manifest changed before installation")
        self.k(
            "apply", "--server-side", "--field-manager=aps-cnpg-demo-bootstrap",
            "-f", self.operator_snapshot, "-o", "name",
        )
        self.k("wait", "--for=condition=Established", "crd/clusters.postgresql.cnpg.io", "--timeout=120s")
        self.k(
            "rollout", "status", "deployment/cnpg-controller-manager", "-n", "cnpg-system",
            "--timeout=300s",
        )
        deployment = json.loads(self.k(
            "get", "deployment", "cnpg-controller-manager", "-n", "cnpg-system", "-o", "json",
        ))
        containers = deployment["spec"]["template"]["spec"]["containers"]
        if (len(containers) != 1 or containers[0].get("image") != self.operator_image
                or self.operator_manifest.read_bytes() != self.operator_bytes
                or self.operator_snapshot.read_bytes() != self.operator_bytes):
            raise shared.QualificationError("CNPG operator deployment has an unexpected image contract")
        self.operator_identity = {
            "manifestDigest": self.operator_sha256,
            "deploymentUid": deployment["metadata"]["uid"],
            "image": self.operator_image,
        }

    def import_archive(self, archive, reference):
        repository = reference.split("@", 1)[0]
        registry = repository.split("/")[0]
        if "." not in registry and ":" not in registry and registry != "localhost":
            repository = "docker.io/" + repository
        environment = dict(os.environ, DOCKER_CONTEXT=self.docker_context, KIND_EXPERIMENTAL_PROVIDER="docker")
        for name in ["DOCKER_HOST", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"]:
            environment.pop(name, None)
        command = [
            "docker", "--context", self.docker_context, "exec", "-i",
            self.cluster + "-control-plane", "ctr", "-n", "k8s.io", "images", "import",
            "--digests", "--platform", self.node_platform, "--base-name", repository, "-",
        ]
        with archive.open("rb") as payload:
            result = subprocess.run(
                command, stdin=payload, capture_output=True, timeout=300, check=False, env=environment,
            )
        if result.returncode or len(result.stdout) + len(result.stderr) > shared.MAX_EVIDENCE_BYTES:
            raise shared.QualificationError("exact candidate image archive import failed")
        self.command([
            "docker", "exec", self.cluster + "-control-plane", "crictl", "inspecti", reference,
        ])

    def load_images(self):
        self.phase = "load-candidate-images"
        for artifact, manifest in [
            (self.initial_artifact, self.initial_manifest),
            (self.successor_artifact, self.successor_manifest),
        ]:
            if manifest["image"]["platform"] != self.node_platform:
                raise shared.QualificationError("candidate image platform differs from the demo node")
            archive = artifact / "controller.oci.tar"
            self.import_archive(archive, manifest["image"]["reference"])
        self.phase = "load-proxy-images"
        for index, reference in enumerate([self.profile["envoyImage"], self.profile["adminImage"]]):
            self.command(["docker", "pull", reference], timeout=300)
            archive = self.output / f"proxy-{index}.tar"
            self.command(["docker", "image", "save", "--output", archive, reference], timeout=300)
            self.import_archive(archive, reference)

    def _role(self, namespace, name, rules, account, cluster=False):
        kind = "ClusterRole" if cluster else "Role"
        metadata = {"name": self.cluster + "-" + name} if cluster else {"name": name, "namespace": namespace}
        self.create({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": kind,
            "metadata": metadata, "rules": rules,
        })
        self.create({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": kind + "Binding",
            "metadata": metadata,
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": kind, "name": metadata["name"]},
            "subjects": [{"kind": "ServiceAccount", "name": account, "namespace": self.control}],
        })

    @staticmethod
    def _rule(group, resources, verbs, names=None):
        value = {"apiGroups": [group], "resources": resources, "verbs": verbs}
        if names:
            value["resourceNames"] = names
        return value

    def permissions(self):
        rule = self._rule
        control_rules = [
            rule("", ["configmaps"], ["get", "create"]),
            rule("", ["configmaps"], ["update"], [
                "reakt-registry-" + self.workload,
                "reakt-activation-" + self.domain,
                "reakt-inventory-" + self.domain,
            ]),
            rule("", ["secrets"], ["get", "create"]),
            rule("", ["serviceaccounts"], ["get", "create", "delete"]),
            rule("", ["pods"], ["get", "list"]),
            rule("", ["pods/exec"], ["get"]),
            rule("", ["services"], ["get", "create"]),
            rule("apps", ["deployments"], ["get", "create", "update"]),
            rule("apps", ["replicasets"], ["get"]),
            rule("rbac.authorization.k8s.io", ["roles", "rolebindings"], ["get", "create"]),
            rule("rbac.authorization.k8s.io", ["rolebindings"], ["delete"]),
        ]
        self._role(self.control, "ceiling-deployer", control_rules, "ceiling-deployer")
        capabilities = {
            shared.canonical(item): item
            for plan in [self.initial_plan, self.successor_plan]
            for item in plan["capabilities"]
        }
        by_namespace = {}
        for capability in capabilities.values():
            namespace = capability["namespace"]
            if namespace not in {self.control, self.workload}:
                raise shared.QualificationError("candidate capability escapes the CNPG demo namespaces")
            resource = capability["resource"]
            if capability.get("subresource"):
                resource += "/" + capability["subresource"]
            names = capability["names"] if capability["verb"] != "create" else None
            by_namespace.setdefault(namespace, []).append(rule(
                capability["apiGroup"], [resource], [capability["verb"]], names,
            ))
        by_namespace.setdefault(self.workload, []).extend([
            rule("", ["configmaps"], ["get"], ["api-config"]),
            rule("postgresql.cnpg.io", ["clusters"], ["get"], ["database"]),
            rule("rbac.authorization.k8s.io", ["roles", "rolebindings"], ["get", "create"]),
            rule("rbac.authorization.k8s.io", ["rolebindings"], ["delete"]),
        ])
        for namespace, rules in by_namespace.items():
            self._role(namespace, "candidate-capabilities", rules, "ceiling-deployer")
        self._role(
            "reakt-platform", "registry-authority",
            [rule("", ["configmaps"], ["get"], [
                "reakt-registry-authority." + self.control + "." + self.workload,
            ])], "ceiling-deployer",
        )
        self._role(
            self.cluster, "pair-marker",
            [rule("", ["configmaps"], ["get"], [PAIR_MARKER])], "ceiling-deployer",
        )
        self._role(
            self.control, "ceiling-issuer", [
                rule("", ["configmaps"], ["get"], [
                    "reakt-trust-" + self.domain, "reakt-deploy-identity-" + self.domain,
                ]),
                rule("", ["serviceaccounts"], ["get"], ["ceiling-deployer"]),
                rule("", ["serviceaccounts/token"], ["create"], ["ceiling-deployer"]),
                rule("", ["secrets"], ["get"], ["ceiling-binding"]),
            ], "ceiling-issuer",
        )
        self._role(
            None, "target", [
                rule("", ["namespaces"], ["get"], [
                    "kube-system", self.control, self.workload, self.cluster,
                ]),
                rule("", ["nodes"], ["get"], [self.cluster + "-control-plane"]),
            ], "ceiling-deployer", cluster=True,
        )

    def bootstrap(self):
        self.install_operator()
        self.load_images()
        self.phase = "bootstrap-policy"
        self.state("reakt-trust-" + self.domain, shared.read_json(self.trust))
        self.policy_object = self.state("reakt-policy-" + self.domain, self.initial_policy)
        self.state("reakt-proxy-policy-" + self.domain, {
            "schemaVersion": "reakt.dev/platform/v1",
            "domainId": self.domain,
            "clusterBinding": self.binding,
            **self.profile,
            "environmentQualificationDigest": self.proxy_canary["resultDigest"],
            "nodeImageDigest": base.NODE.split("@")[1],
            "certificateLifetimeSeconds": 86_400,
        })
        registry = self.create({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "reakt-registry-" + self.workload, "namespace": self.control},
        })
        self.create({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "reakt-platform"}})
        self.state(
            "reakt-registry-authority." + self.control + "." + self.workload,
            {
                "schemaVersion": "reakt.dev/registry-authority/v1",
                "registryUid": registry["metadata"]["uid"],
                "clusterBinding": self.binding,
                "controlNamespace": self.control,
                "workloadNamespace": self.workload,
            },
            "reakt-platform",
        )
        self.step(
            "bootstrap", "bootstrapped", self.admin,
            registryUid=registry["metadata"]["uid"],
            registryResourceVersion=registry["metadata"]["resourceVersion"],
        )
        accounts = {}
        for name in ["ceiling-issuer", "ceiling-deployer"]:
            accounts[name] = self.create({
                "apiVersion": "v1", "kind": "ServiceAccount", "automountServiceAccountToken": False,
                "metadata": {"name": name, "namespace": self.control},
            })
        secret = self.binding_metadata(self.k(
            "create", "-f", "-", "-o", base.BINDING_PATH,
            data=json.dumps({
                "apiVersion": "v1", "kind": "Secret", "immutable": True, "type": "Opaque",
                "metadata": {"name": "ceiling-binding", "namespace": self.control},
            }),
        ))
        self.secret_uid = secret["uid"]
        self.state("reakt-deploy-identity-" + self.domain, {
            "schemaVersion": "reakt.dev/platform/v1",
            "domainId": self.domain,
            "clusterBinding": self.binding,
            "serviceAccountName": "ceiling-deployer",
            "serviceAccountUid": accounts["ceiling-deployer"]["metadata"]["uid"],
            "bindingSecretName": "ceiling-binding",
            "bindingSecretUid": self.secret_uid,
            "audience": "https://kubernetes.default.svc.cluster.local",
            "lifetimeSeconds": 900,
        })
        self.create({
            "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
            "metadata": {"name": "reakt-rollout-key-" + self.domain, "namespace": self.control},
            "data": {
                "keyId": base64.b64encode(b"cnpg-demo").decode(),
                "key": base64.b64encode(os.urandom(32)).decode(),
            },
        })
        self.permissions()

    def refresh_issuer(self):
        config = json.loads(self.k("config", "view", "--raw", "--minify", "-o", "json"))
        token = self.k(
            "create", "token", "ceiling-issuer", "-n", self.control, "--duration=10m",
        ).strip()
        config["users"] = [{"name": "ceiling-issuer", "user": {"token": token}}]
        config["contexts"][0]["context"]["user"] = "ceiling-issuer"
        pending = self.output / "issuer-next.kubeconfig"
        try:
            with os.fdopen(os.open(pending, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
                json.dump(config, stream)
            os.replace(pending, self.issuer)
        finally:
            pending.unlink(missing_ok=True)

    def lifecycle(self, action, transition, timeout=360):
        completed, pending, phase = LIFECYCLE[action]
        deadline = time.monotonic() + timeout
        while True:
            result = self.step(action, {completed, pending}, self.issuer, transition=transition)
            if result["transition"] != transition:
                raise shared.QualificationError("lifecycle step changed the frozen transition")
            if result["outcome"] == completed:
                if result["phase"] != phase:
                    raise shared.QualificationError("lifecycle step completed at an unexpected phase")
                return result
            if time.monotonic() >= deadline:
                raise shared.QualificationError("CNPG demo lifecycle step remained pending")
            time.sleep(2)

    def wait_object(self, resource, name, namespace, timeout):
        deadline = time.monotonic() + timeout
        while True:
            output = self.k(
                "get", resource, name, "-n", namespace, "-o", "json", "--ignore-not-found",
            ).strip()
            if output:
                return json.loads(output, object_pairs_hook=shared.unique_object)
            if time.monotonic() >= deadline:
                raise shared.QualificationError("expected CNPG demo object did not appear")
            time.sleep(2)

    def wait_cnpg(self):
        self.wait_object("cluster.postgresql.cnpg.io", "database", self.workload, 180)
        self.k(
            "wait", "--for=condition=Ready", "cluster.postgresql.cnpg.io/database",
            "-n", self.workload, "--timeout=300s",
        )
        cluster = json.loads(self.k(
            "get", "cluster.postgresql.cnpg.io", "database", "-n", self.workload, "-o", "json",
        ))
        deadline = time.monotonic() + 180
        while True:
            uid = self.k(
                "get", "secret", "database-app", "-n", self.workload,
                "-o", "jsonpath={.metadata.uid}", "--ignore-not-found",
            ).strip()
            present = self.k(
                "get", "secret", "database-app", "-n", self.workload,
                "-o", "go-template={{if index .data \"uri\"}}present{{end}}", "--ignore-not-found",
            ).strip()
            if uid and present == "present":
                result = {"clusterUid": cluster["metadata"]["uid"], "secretUid": uid, "uriKeyPresent": True}
                self.checks.append({"check": "cnpg-ready-and-secret-bound", "passed": True, **result})
                return result
            if time.monotonic() >= deadline:
                raise shared.QualificationError("CNPG application Secret did not become available")
            time.sleep(2)

    def workload_identity(self, previous_uid=None, timeout=300):
        deadline = time.monotonic() + timeout
        while True:
            deployment = self.wait_object(
                "deployment", "database-api", self.workload,
                max(1, int(deadline - time.monotonic())),
            )
            pods = json.loads(self.k(
                "get", "pods", "-n", self.workload, "-l", "app=database-api", "-o", "json",
            ))["items"]
            ready = [pod for pod in pods if not pod["metadata"].get("deletionTimestamp")
                     and pod.get("status", {}).get("phase") == "Running"
                     and any(item.get("type") == "Ready" and item.get("status") == "True"
                             for item in pod.get("status", {}).get("conditions", []))]
            if len(ready) == 1 and (previous_uid is None or ready[0]["metadata"]["uid"] != previous_uid):
                pod = ready[0]
                owners = [item for item in pod["metadata"].get("ownerReferences", []) if item.get("kind") == "ReplicaSet"]
                statuses = pod.get("status", {}).get("containerStatuses", [])
                revision = deployment["spec"]["template"].get("metadata", {}).get("annotations", {}).get("reakt.dev/revision")
                if len(owners) == 1 and len(statuses) == 1 and revision:
                    replica = json.loads(self.k(
                        "get", "replicaset", owners[0]["name"], "-n", self.workload, "-o", "json",
                    ))
                    replica_owners = [item for item in replica["metadata"].get("ownerReferences", [])
                                      if item.get("kind") == "Deployment"]
                    status = statuses[0]
                    if (replica["metadata"]["uid"] == owners[0]["uid"] and len(replica_owners) == 1
                            and replica_owners[0]["uid"] == deployment["metadata"]["uid"]
                            and status.get("ready") is True and status.get("restartCount") == 0
                            and status.get("imageID")):
                        selected_digest = self.selected_child_digest(
                            self.workload_image, status["imageID"], pod["spec"]["nodeName"],
                        )
                        return {
                            "deploymentUid": deployment["metadata"]["uid"],
                            "deploymentResourceVersion": deployment["metadata"]["resourceVersion"],
                            "revision": revision,
                            "replicaSetUid": replica["metadata"]["uid"],
                            "podUid": pod["metadata"]["uid"],
                            "containerImageId": status["imageID"],
                            "selectedImageManifestDigest": selected_digest,
                            "restartCount": status["restartCount"],
                        }
            if time.monotonic() >= deadline:
                raise shared.QualificationError("PostgREST workload did not reach the required identity")
            time.sleep(2)

    def selected_child_digest(self, reference, image_id, node):
        if node != self.cluster + "-control-plane":
            raise shared.QualificationError("PostgREST Pod ran on an unexpected node")
        expected_index = reference.rsplit("@", 1)[1]
        raw = self.command([
            "docker", "exec", node, "ctr", "-n", "k8s.io", "content", "get", expected_index,
        ])
        if shared.digest_bytes(raw.encode()) != expected_index:
            raise shared.QualificationError("containerd image content differs from the signed successor")
        manifest = json.loads(raw, object_pairs_hook=shared.unique_object)
        selected = image_id.rsplit("@", 1)[-1]
        if not shared.valid_digest(selected):
            raise shared.QualificationError("PostgREST CRI image identity is invalid")
        children = {item.get("digest") for item in manifest.get("manifests", [])}
        if selected != expected_index and selected not in children:
            raise shared.QualificationError("PostgREST CRI image is not a child of the signed image index")
        return selected

    @contextmanager
    def forwarded_service(self):
        log_path = self.output / "postgrest-port-forward.log"
        with log_path.open("w+b") as output:
            process = subprocess.Popen([
                "kubectl", "--kubeconfig", str(self.admin), "port-forward", "--address=127.0.0.1",
                "service/database-api", ":80", "-n", self.workload,
            ], cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 30
                port = None
                while time.monotonic() < deadline:
                    output.flush()
                    text = log_path.read_text(errors="replace")
                    match = re.search(r"Forwarding from 127\.0\.0\.1:(\d+) ->", text)
                    if match:
                        port = int(match.group(1))
                        break
                    if process.poll() is not None or len(text.encode()) > 64 * 1024:
                        break
                    time.sleep(0.2)
                if port is None:
                    raise shared.QualificationError("PostgREST port-forward did not become ready")
                yield port
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=10)

    def http_messages(self, expected_count):
        with self.forwarded_service() as port:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            try:
                connection.request("GET", "/messages?select=id,message&order=id.asc")
                response = connection.getresponse()
                body = response.read(MAX_HTTP_BYTES + 1)
            finally:
                connection.close()
        if response.status != 200 or len(body) > MAX_HTTP_BYTES:
            raise shared.QualificationError("PostgREST messages endpoint returned an unexpected response")
        value = json.loads(body, object_pairs_hook=shared.unique_object)
        expected = [
            {"id": 1, "message": "Hello from CloudNativePG"},
            {"id": 2, "message": "Configured by Apsides"},
        ][:expected_count]
        if value != expected:
            raise shared.QualificationError("PostgREST messages endpoint returned unexpected rows")
        return {"status": response.status, "rowCount": len(value), "bodyDigest": shared.digest_bytes(body)}

    def replace_policy(self):
        replacement = json.loads(json.dumps(self.policy_object))
        replacement["data"]["state.json"] = shared.canonical(self.successor_policy).decode()
        self.policy_object = json.loads(self.k("replace", "-f", "-", "-o", "json", data=json.dumps(replacement)))

    def measure(self):
        prepared = self.step("prepare-initial", "initial-prepared", self.issuer)
        reserved = self.step(
            "reserve-initial", "initial-reserved", self.issuer,
            expectedRequestDigest=prepared["requestDigest"],
        )
        if any(reserved[key] != prepared[key] for key in ["transition", "generation", "requestDigest", "registryUid"]):
            raise shared.QualificationError("initial reservation differs from its prepared request")
        initial_transition = prepared["transition"]
        self.lifecycle("stage-initial", initial_transition)
        self.lifecycle("activate-initial", initial_transition)
        self.step("publish-initial", "initial-inventory-published", self.issuer, transition=initial_transition)
        initial = self.step("resolve-initial", "initial-resolved", self.issuer, transition=initial_transition)
        if initial["phase"] != "Committed":
            raise shared.QualificationError("initial candidate did not commit")
        self.wait_cnpg()
        ownership = []
        for kind, name in [("cluster.postgresql.cnpg.io", "database"), ("configmap", "api-config")]:
            metadata = json.loads(self.k(
                "get", kind, name, "-n", self.workload, "--show-managed-fields", "-o", "json",
            ))["metadata"]
            ownership.append({
                "resource": kind, "name": name, "uid": metadata["uid"],
                "managers": [{key: entry.get(key) for key in [
                    "manager", "operation", "apiVersion", "fieldsType", "subresource",
                ]} for entry in metadata.get("managedFields", [])],
            })
        (self.output / "initial-ownership.json").write_bytes(shared.canonical(ownership))
        self.create({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "api-settings", "namespace": self.workload},
            "data": {"PGRST_DB_MAX_ROWS": "100"},
        })
        rejection = self.step("prepare-rejection", "rejection-prepared", self.issuer)
        denied = self.step(
            "reject-upgrade", "upgrade-rejected", self.issuer,
            expectedRequestDigest=rejection["requestDigest"], expectedMissingCapability=self.missing,
        )
        if (denied["requestDigest"] != rejection["requestDigest"]
                or denied["registryUid"] != rejection["registryUid"]
                or denied["registryResourceVersion"] != rejection["registryResourceVersion"]):
            raise shared.QualificationError("permission rejection differs from its frozen request")
        self.replace_policy()
        successor = self.step("prepare-successor", "successor-prepared", self.issuer)
        reserved = self.step(
            "reserve-successor", "successor-reserved", self.issuer,
            expectedRequestDigest=successor["requestDigest"],
        )
        if any(reserved[key] != successor[key] for key in ["transition", "generation", "requestDigest", "registryUid"]):
            raise shared.QualificationError("successor reservation differs from its prepared request")
        transition = successor["transition"]
        self.lifecycle("stage-successor", transition)
        self.lifecycle("quiesce-upgrade", transition)
        self.lifecycle("activate-upgrade", transition)
        self.lifecycle("commit-upgrade", transition)
        self.step("publish-successor", "successor-inventory-published", self.issuer, transition=transition)
        resolved = self.step("resolve-successor", "successor-resolved", self.issuer, transition=transition)
        if resolved["phase"] != "Committed":
            raise shared.QualificationError("successor candidate did not commit")
        before = self.workload_identity()
        first_http = self.http_messages(2)
        self.k(
            "patch", "configmap", "api-settings", "-n", self.workload, "--type=merge", "-p",
            json.dumps({"data": {"PGRST_DB_MAX_ROWS": "1"}}),
        )
        after = self.workload_identity(previous_uid=before["podUid"])
        if (after["deploymentUid"] != before["deploymentUid"]
                or after["revision"] == before["revision"]
                or after["replicaSetUid"] == before["replicaSetUid"]):
            raise shared.QualificationError("ConfigMap change did not produce the expected PostgREST rollout")
        second_http = self.http_messages(1)
        self.checks.extend([
            {"check": "postgrest-messages-live", "passed": True, **first_http},
            {"check": "configmap-driven-pod-turnover", "passed": True, "before": before, "after": after},
            {"check": "postgrest-messages-after-rollout", "passed": True, **second_http},
            {"check": "secret-bound-before-successor-start", "passed": True,
             "claim": "binding-only; no Secret creation reaction claimed"},
        ])

    def retain_cluster(self):
        self.verify_docker()
        containers = json.loads(self.command(["docker", "inspect", self.cluster + "-control-plane"]))
        if (len(containers) != 1 or containers[0].get("Id") != self.container_id
                or containers[0].get("Config", {}).get("Labels", {}).get("io.x-k8s.kind.cluster") != self.cluster):
            raise shared.QualificationError("retained CNPG demo cluster identity changed")
        current_clusters = sorted(self.cluster_names())
        current_containers = sorted(self.command(["docker", "ps", "-aq", "--no-trunc"]).splitlines())
        if (current_clusters != sorted([*self.prior_clusters, self.cluster])
                or current_containers != sorted([*self.prior_containers, self.container_id])):
            raise shared.QualificationError("retained host inventory differs from prior plus the owned cluster")
        self.issuer.unlink(missing_ok=True)
        if self.issuer.exists() or not self.admin.exists():
            raise shared.QualificationError("retained demo credential disposition failed")
        self.disposition = "retained"
        (self.output / "cleanup.json").write_bytes(shared.canonical({
            "cluster": self.cluster, "disposition": self.disposition,
            "issuerCredentialsRemoved": True, "adminKubeconfigMode": "0600",
            "previousClusters": self.prior_clusters, "currentClusters": current_clusters,
            "previousContainerIds": self.prior_containers, "currentContainerIds": current_containers,
        }))

    def finish(self, passed):
        if passed and self.retain_on_success:
            self.retain_cluster()
            return
        deleted = super().cleanup()
        if self.attempted and not deleted:
            raise shared.QualificationError("owned CNPG demo cluster deletion was not confirmed")
        self.disposition = "deleted" if deleted else "not-created"

    def run(self):
        passed = False
        try:
            self.prepare()
            self.phase = "cluster-create"
            self.create_cluster()
            self.phase = "bootstrap"
            self.bootstrap()
            self.phase = "lifecycle"
            self.measure()
            passed = True
        except Exception as error:
            self.primary_failure = {"phase": self.phase, "reason": base.failure_reason(self, error)}
            raise
        finally:
            self.phase = "cleanup"
            self.finish(passed)
        result = {
            "passed": True,
            "candidateIntegrityVerified": True,
            "releaseVerified": False,
            "scope": "local CNPG demonstration through the exact signed-candidate pair lifecycle",
            "cluster": self.cluster,
            "clusterDisposition": self.disposition,
            "issuerCredentialsRemoved": not self.issuer.exists(),
            "adminKubeconfig": str(self.admin) if self.disposition == "retained" else None,
            "operator": self.operator_identity,
            "proxyEnvironmentCanary": self.proxy_canary,
            "candidates": self.pair["candidates"],
            "checks": self.checks,
            "steps": self.steps,
        }
        (self.output / "result.json").write_bytes(shared.canonical(result))
        print(json.dumps(result))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in [
        "initial-artifact", "successor-artifact", "trust-policy",
        "operator-manifest", "output",
    ]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--operator-sha256", required=True)
    parser.add_argument("--retain-on-success", action="store_true")
    runner = None
    try:
        runner = CnpgDemoRunner(parser.parse_args())
        runner.run()
    except Exception as error:
        if runner is not None:
            result = {
                "passed": False,
                "candidateIntegrityVerified": hasattr(runner, "pair"),
                "releaseVerified": False,
                "cluster": runner.cluster,
                "clusterDisposition": runner.disposition,
                "phase": runner.phase,
                "failureReason": base.failure_reason(runner, error),
                "primaryFailure": runner.primary_failure,
                "subprocessFailures": runner.failures,
                "checks": runner.checks,
                "issuerCredentialsRemoved": not runner.issuer.exists(),
            }
            (runner.output / "result.json").write_bytes(shared.canonical(result))
        print("CNPG demo failed; no release acceptance is claimed.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
