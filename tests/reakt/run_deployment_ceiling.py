#!/usr/bin/env python3
"""Measure protected deployment-ceiling rejection for one unchanged signed candidate."""
import argparse
from collections import Counter
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import uuid
import time

import run_reproducibility as shared
import ceiling_observations as observations

ROOT = Path(__file__).resolve().parents[2]
NODE = "kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48"
SCHEMA = "reakt.dev/deployment-ceiling-result/v1"
MARKER = "reakt-candidate-qualification"
BINDING_FIELDS = ["name", "namespace", "uid", "resourceVersion", "type", "immutable", "deletionTimestamp"]
BINDING_PATH = r'jsonpath={.metadata.name}{"\t"}{.metadata.namespace}{"\t"}{.metadata.uid}{"\t"}{.metadata.resourceVersion}{"\t"}{.type}{"\t"}{.immutable}{"\t"}{.metadata.deletionTimestamp}'
FIELDS = {
    "verified": {"releaseDigest", "irDigest", "domainId", "clusterBinding", "controlNamespace", "workloadNamespace"},
    "bootstrapped": {"registryUid", "registryResourceVersion"},
    "prepared": {"generation", "transition", "registryUid", "registryResourceVersion", "releaseDigest", "requestDigest", "creationTargets", "creationTargetsSource"},
    "ceiling-rejected": {"generation", "transition", "registryUid", "registryResourceVersion", "releaseDigest", "missingCapabilityDigest", "requestDigest"},
    "planned": {"generation", "transition", "registryUid", "registryResourceVersion", "phase", "releaseDigest", "nextSequence", "claimCount", "requestDigest"},
}


def result_record(stdout, expected, *, prefix="APS_CEILING_RESULT=", schema=SCHEMA, fields=FIELDS):
    records = [line.removeprefix(prefix) for line in stdout.splitlines()
               if line.startswith(prefix)]
    if len(records) != 1 or len(records[0]) > 128 * 1024:
        raise shared.QualificationError("ceiling driver did not emit one bounded result")
    value = json.loads(records[0], object_pairs_hook=shared.unique_object)
    if (not isinstance(value, dict) or set(value) != fields[expected] | {"schemaVersion", "outcome"}
            or value["schemaVersion"] != schema or value["outcome"] != expected):
        raise shared.QualificationError("ceiling driver result has an unexpected contract")
    for key, item in value.items():
        if key == "creationTargets":
            if not isinstance(item, list) or not item or len(item) > 1024:
                raise shared.QualificationError("invalid creation-target projection")
            for target in item:
                if not isinstance(target, dict) or set(target) != {"namespace", "apiGroup", "resource", "name"} or any(not isinstance(v, str) for v in target.values()):
                    raise shared.QualificationError("invalid creation target")
        elif key in {"nextSequence", "claimCount", "overlapCount"}:
            if type(item) is not int or item < 0:
                raise shared.QualificationError("invalid ceiling result counter")
        elif not isinstance(item, str) or not item or len(item) > 1024:
            raise shared.QualificationError("invalid ceiling result identity")
    for key in ["releaseDigest", "irDigest", "requestDigest", "missingCapabilityDigest"]:
        if key in value and not shared.valid_digest(value[key]):
            raise shared.QualificationError("invalid ceiling result digest")
    if expected == "planned" and (value["nextSequence"] != 2 or value["phase"] != "Planned"):
        raise shared.QualificationError("positive ceiling control did not reach initial Planned")
    return value


def ceiling(plan):
    domain = plan["ir"]["domain"]
    return {"policyRef": domain["policyRef"], "domainId": domain["id"],
            "clusterBinding": domain["clusterBinding"], "workloadNamespace": domain["namespace"],
            "controlNamespace": domain["controlNamespace"], "capabilities": plan["capabilities"],
            "acceptedEnforcementGaps": sorted(plan["enforcementGaps"]),
            "acceptedIndirectRisks": sorted(plan["indirectRisks"])}


def restricted_policy(plan):
    full = ceiling(plan)
    if not full["capabilities"]:
        raise shared.QualificationError("candidate has no required runtime capability to remove")
    missing = sorted(full["capabilities"], key=shared.canonical)[0]
    reduced = dict(full, capabilities=[cap for cap in full["capabilities"] if cap != missing])
    return {"schemaVersion": "reakt.dev/platform/v1", "runtimeCeiling": reduced,
            "cleanupCeiling": full}, missing


def unchanged_except(before, after, allowed):
    if {key: value for key, value in before.items() if key not in allowed} != {
            key: value for key, value in after.items() if key not in allowed}:
        raise shared.QualificationError("ceiling experiment changed an unapproved Kubernetes object")


def public_snapshot(value):
    return {key: value[key] for key in ["objects", "coordination", "inventory"]}


class CeilingRunner:
    def __init__(self, args):
        self.artifact = args.artifact
        self.trust = args.trust_policy
        for path in [self.artifact, self.trust]:
            if not path.is_absolute() or path.is_symlink():
                raise shared.QualificationError("candidate and trust inputs must be absolute nonsymlink paths")
        requested_output = args.output.absolute()
        for parent in [requested_output, *requested_output.parents]:
            if parent.is_symlink():
                raise shared.QualificationError("output has a symlink component")
        self.output = requested_output.resolve()
        if self.output.exists() or not self.output.is_relative_to(ROOT):
            raise shared.QualificationError("ceiling output must be a new ignored repository directory")
        shared.require_ignored(ROOT, self.output)
        self.repository, self.commit = shared.source_state(Path(__file__).resolve())
        self.output.mkdir(parents=True, mode=0o700)
        self.cluster = "reakt-qual-" + uuid.uuid4().hex
        self.admin = self.output / "admin.kubeconfig"
        self.issuer = self.output / "issuer.kubeconfig"
        self.coordination = self.output / "coordination.db"
        self.inventory = self.output / "inventory.db"
        self.owned = False
        self.attempted = False
        self.cleanup_confirmed = False
        self.container_id = None
        self.steps = []
        self.failures = []
        self.primary_failure = None
        self.phase = "prepare"
        self.sources = [Path(__file__).resolve(), Path(shared.__file__).resolve(),
                        Path(observations.__file__).resolve(), ROOT / "crates/platform/tests/deployment_ceiling.rs"]
        self.sources += sorted((ROOT / "crates/platform/tests/support").glob("deployment_ceiling*.rs"))
        self.sources.append(ROOT / "crates/platform/tests/support/qualification_driver.rs")
        self.driver_error_reasons = set(re.findall(r'DriverError\(\s*"([a-zA-Z0-9 -]+)"',
            "\n".join(path.read_text() for path in self.sources if path.suffix == ".rs")))
        self.failure_reasons = set(re.findall(r'(?:QualificationError|RuntimeError)\(\s*"([^"\n]+)"', Path(__file__).read_text()))
        self.source_digests = {path.name: shared.digest_file(path) for path in self.sources}
        if len(self.source_digests) != len(self.sources):
            raise shared.QualificationError("ceiling runner source filenames collide")

    def command(self, args, data=None, environment=None, timeout=180):
        arguments = [str(item) for item in args]
        environment = dict(os.environ if environment is None else environment)
        if hasattr(self, "docker_context"):
            environment["DOCKER_CONTEXT"] = self.docker_context
            environment["KIND_EXPERIMENTAL_PROVIDER"] = "docker"
            for name in ["DOCKER_HOST", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"]:
                environment.pop(name, None)
            if arguments[0] == "docker":
                arguments[1:1] = ["--context", self.docker_context]
        result = subprocess.run(arguments, input=data, text=True,
                                capture_output=True, timeout=timeout, check=False,
                                cwd=ROOT, env=environment)
        if result.returncode or len(result.stdout.encode()) > shared.MAX_EVIDENCE_BYTES:
            failure = {"program": Path(arguments[0]).name, "returnCode": result.returncode, "phase": self.phase}
            categories = {"permission-denied": "permission denied", "operation-not-permitted": "operation not permitted",
                          "forbidden": "forbidden", "not-found": "notfound", "connection-refused": "connection refused",
                          "timeout": "timed out", "jsonpath": "jsonpath", "discovery": "resource type"}
            failure["errorClasses"] = [name for name, pattern in categories.items() if pattern in result.stderr.lower()]
            if hasattr(self, "driver") and arguments[0] == str(self.driver):
                observed = re.findall(r'DriverError\("([a-zA-Z0-9 -]+)"\)', result.stderr + result.stdout)
                if len(observed) == 1 and observed[0] in self.driver_error_reasons:
                    failure["driverReason"] = observed[0]
            if len(self.failures) < 2:
                self.failures.append(failure)
            raise shared.QualificationError("ceiling subprocess failed or exceeded its output limit")
        return result.stdout

    def k(self, *args, data=None):
        return self.command(["kubectl", "--kubeconfig", self.admin, *args], data=data)

    def create(self, value):
        return json.loads(self.k("create", "-f", "-", "-o", "json", data=json.dumps(value)))

    def state(self, name, value, namespace=None, immutable=False):
        return self.create({"apiVersion": "v1", "kind": "ConfigMap", "immutable": immutable,
                            "metadata": {"name": name, "namespace": namespace or self.control},
                            "data": {"state.json": shared.canonical(value).decode()}})

    def prepare(self):
        output = self.command(["cargo", "test", "--locked", "--offline", "-p", "platform", "--features",
                               "test-support", "--test", "deployment_ceiling", "--no-run", "--message-format=json"], timeout=300)
        executables = [row["executable"] for line in output.splitlines()
                       if (row := json.loads(line)).get("reason") == "compiler-artifact"
                       and row.get("target", {}).get("name") == "deployment_ceiling" and row.get("executable")]
        if len(executables) != 1:
            raise shared.QualificationError("Cargo did not identify one ceiling driver")
        self.driver = self.output / "ceiling-driver"
        shutil.copy2(executables[0], self.driver)
        self.driver_digest = shared.digest_file(self.driver)
        self.verified = self.step("verify", "verified")
        self.control = self.verified["controlNamespace"]
        self.workload = self.verified["workloadNamespace"]
        self.domain = self.verified["domainId"]
        self.binding = self.verified["clusterBinding"]
        self.manifest = shared.read_json(self.artifact / "release.json")
        self.plan = shared.read_json(self.artifact / "plan.json")
        if (self.manifest["mode"] != "production" or self.plan["ir"]["testOnly"] is not False
                or shared.digest_file(self.artifact / "release.json") != self.verified["releaseDigest"]):
            raise shared.QualificationError("ceiling subject differs from verified production candidate")
        self.policy, self.missing = restricted_policy(self.plan)

    def step(self, action, expected, kubeconfig=None, **extra):
        settings = {"action": action, "artifact": str(self.artifact), "trustPolicy": str(self.trust), **extra}
        if action != "verify":
            settings["target"] = self.target
        path = self.output / ("step-" + str(len(self.steps)) + ".json")
        path.write_bytes(shared.canonical(settings))
        environment = dict(os.environ, APS_CEILING_STEP=str(path),
                           APS_CEILING_COORDINATION_DB=str(self.coordination),
                           APS_CEILING_INVENTORY_DB=str(self.inventory))
        if kubeconfig:
            environment["KUBECONFIG"] = str(kubeconfig)
        else:
            environment.pop("KUBECONFIG", None)
        output = self.command([self.driver, "deployment_ceiling_step", "--exact", "--ignored", "--nocapture"],
                              environment=environment, timeout=180)
        result = result_record(output, expected)
        self.steps.append(result)
        (self.output / "steps.json").write_bytes(shared.canonical(self.steps))
        return result

    def cluster_names(self):
        return self.command(["kind", "get", "clusters"]).splitlines()

    def docker_identity(self):
        context = json.loads(self.command(["docker", "context", "inspect", self.docker_context]))
        daemon = self.command(["docker", "info", "--format", "{{.ID}}"]).strip()
        if len(context) != 1 or not daemon or len(daemon) > 256:
            raise shared.QualificationError("Docker context or daemon identity is unavailable")
        return {"contextDigest": shared.digest_bytes(shared.canonical(context[0])),
                "daemonDigest": shared.digest_bytes(daemon.encode())}

    def verify_docker(self):
        if self.docker_identity() != self.docker_binding:
            raise shared.QualificationError("Docker context or daemon changed")

    def create_cluster(self):
        self.docker_context = self.command(["docker", "context", "show"]).strip()
        if not self.docker_context or len(self.docker_context) > 256:
            raise shared.QualificationError("Docker context is invalid")
        self.docker_binding = self.docker_identity()
        self.prior_clusters = sorted(self.cluster_names())
        self.prior_containers = sorted(self.command(["docker", "ps", "-aq", "--no-trunc"]).splitlines())
        if self.cluster in self.prior_clusters:
            raise shared.QualificationError("cluster name existed before creation; ownership not established")
        image = json.loads(self.command(["docker", "image", "inspect", NODE]))
        if len(image) != 1:
            raise shared.QualificationError("pinned kind image is ambiguous")
        self.node_image_id = image[0]["Id"]
        if self.command(["docker", "ps", "-aq", "--filter", "name=^/" + self.cluster + "-control-plane$"]).strip():
            raise shared.QualificationError("kind container name already exists")
        self.attempted = True
        self.command(["kind", "create", "cluster", "--name", self.cluster, "--image", NODE,
                      "--kubeconfig", self.admin, "--wait", "120s"], timeout=240)
        self.owned = True
        self.admin.chmod(0o600)
        containers = json.loads(self.command(["docker", "inspect", self.cluster + "-control-plane"]))
        if len(containers) != 1:
            raise shared.QualificationError("kind container identity is ambiguous")
        self.container_id = containers[0]["Id"]
        node = json.loads(self.k("get", "node", self.cluster + "-control-plane", "-o", "json"))
        # The helper verifies exact kind labels, container image, API UID/provider and Ready state.
        self.host_identity = observations.validate_kind_target(node, containers[0], self.cluster, NODE, self.node_image_id)
        namespaces = {}
        for name in [self.control, self.workload, self.cluster]:
            namespaces[name] = self.create({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name}})["metadata"]["uid"]
        system = json.loads(self.k("get", "namespace", "kube-system", "-o", "json"))
        self.target = {"runId": self.cluster.removeprefix("reakt-qual-"),
                       "kubeSystemNamespaceUid": system["metadata"]["uid"],
                       "controlNamespaceUid": namespaces[self.control], "workloadNamespaceUid": namespaces[self.workload],
                       "sentinelNamespaceUid": namespaces[self.cluster], "nodeName": node["metadata"]["name"],
                       "nodeUid": node["metadata"]["uid"], "nodeProviderId": node["spec"]["providerID"],
                       "nodeImageDigest": NODE.split("@")[1]}
        marker = {"schemaVersion": "reakt.dev/candidate-qualification/v1", "runId": self.target["runId"],
                  "clusterName": self.cluster, "sentinelNamespace": self.cluster, "domainId": self.domain,
                  "releaseDigest": self.verified["releaseDigest"], "clusterBinding": self.binding,
                  "controlNamespace": self.control, "workloadNamespace": self.workload,
                  **{key: value for key, value in self.target.items() if key != "runId"}}
        created = self.state(MARKER, marker, self.cluster, immutable=True)
        self.target.update(markerUid=created["metadata"]["uid"], markerResourceVersion=created["metadata"]["resourceVersion"])

    def binding_metadata(self, encoded, expected_uid=None):
        fields = encoded.split("\t")
        if (len(fields) != len(BINDING_FIELDS) or fields[0] != "ceiling-binding" or fields[1] != self.control
                or not fields[2] or not fields[3] or fields[4:] != ["Opaque", "true", ""]
                or (expected_uid is not None and fields[2] != expected_uid)):
            raise shared.QualificationError("binding Secret metadata changed")
        return dict(zip(BINDING_FIELDS, fields))

    def bootstrap(self):
        self.state("reakt-trust-" + self.domain, shared.read_json(self.trust))
        self.policy_object = self.state("reakt-policy-" + self.domain, self.policy)
        registry = self.create({"apiVersion": "v1", "kind": "ConfigMap",
                                "metadata": {"name": "reakt-registry-" + self.workload, "namespace": self.control}})
        self.create({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "reakt-platform"}})
        self.state("reakt-registry-authority." + self.control + "." + self.workload,
                   {"schemaVersion": "reakt.dev/registry-authority/v1", "registryUid": registry["metadata"]["uid"],
                    "clusterBinding": self.binding, "controlNamespace": self.control, "workloadNamespace": self.workload},
                   "reakt-platform")
        self.step("bootstrap", "bootstrapped", self.admin, registryUid=registry["metadata"]["uid"],
                  registryResourceVersion=registry["metadata"]["resourceVersion"])
        accounts = {}
        for name in ["ceiling-issuer", "ceiling-deployer"]:
            accounts[name] = self.create({"apiVersion": "v1", "kind": "ServiceAccount", "automountServiceAccountToken": False,
                                         "metadata": {"name": name, "namespace": self.control}})
        secret_definition = {"apiVersion": "v1", "kind": "Secret", "immutable": True, "type": "Opaque",
                             "metadata": {"name": "ceiling-binding", "namespace": self.control}}
        secret = self.binding_metadata(self.k("create", "-f", "-", "-o", BINDING_PATH, data=json.dumps(secret_definition)))
        self.secret_uid = secret["uid"]
        self.state("reakt-deploy-identity-" + self.domain,
                   {"schemaVersion": "reakt.dev/platform/v1", "domainId": self.domain, "clusterBinding": self.binding,
                    "serviceAccountName": "ceiling-deployer", "serviceAccountUid": accounts["ceiling-deployer"]["metadata"]["uid"],
                    "bindingSecretName": "ceiling-binding", "bindingSecretUid": self.secret_uid,
                    "audience": "https://kubernetes.default.svc.cluster.local", "lifetimeSeconds": 600})
        self.permissions()
        config = json.loads(self.k("config", "view", "--raw", "--minify", "-o", "json"))
        token = self.k("create", "token", "ceiling-issuer", "-n", self.control, "--duration=10m").strip()
        config["users"] = [{"name": "ceiling-issuer", "user": {"token": token}}]
        config["contexts"][0]["context"]["user"] = "ceiling-issuer"
        descriptor = os.open(self.issuer, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w") as stream:
            json.dump(config, stream)

    def permissions(self, domains=None):
        domains = [self.domain] if domains is None else domains
        trust_names = ["reakt-trust-" + domain for domain in domains]
        identity_names = ["reakt-deploy-identity-" + domain for domain in domains]
        policy_names = ["reakt-policy-" + domain for domain in domains]
        def rule(resource, verbs, names):
            return {"apiGroups": [""], "resources": [resource], "verbs": verbs, "resourceNames": names}

        def role(namespace, name, rules, account, cluster=False):
            metadata = {"name": self.cluster + "-" + name} if cluster else {"name": name, "namespace": namespace}
            kind = "ClusterRole" if cluster else "Role"
            self.create({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": kind, "metadata": metadata, "rules": rules})
            self.create({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": kind + "Binding", "metadata": metadata,
                         "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": kind, "name": metadata["name"]},
                         "subjects": [{"kind": "ServiceAccount", "name": account, "namespace": self.control}]})

        role(self.control, "issuer", [rule("configmaps", ["get"], trust_names + identity_names),
                                      rule("serviceaccounts", ["get"], ["ceiling-deployer"]),
                                      rule("serviceaccounts/token", ["create"], ["ceiling-deployer"]),
                                      rule("secrets", ["get"], ["ceiling-binding"])], "ceiling-issuer")
        role(self.control, "deployer", [rule("configmaps", ["get"], trust_names + identity_names + policy_names + ["reakt-registry-" + self.workload]),
                                        rule("configmaps", ["update"], ["reakt-registry-" + self.workload])], "ceiling-deployer")
        role(self.cluster, "marker", [rule("configmaps", ["get"], [MARKER])], "ceiling-deployer")
        role("reakt-platform", "authority", [rule("configmaps", ["get"], ["reakt-registry-authority." + self.control + "." + self.workload])], "ceiling-deployer")
        role(None, "target", [rule("namespaces", ["get"], ["kube-system", self.control, self.workload, self.cluster]),
                              rule("nodes", ["get"], [self.cluster + "-control-plane"])], "ceiling-deployer", cluster=True)

    def snapshot(self):
        objects = {}
        raw_objects = {}
        resources = ["configmaps", "serviceaccounts", "secrets", "roles", "rolebindings", "pods", "services", "deployments", "replicasets"]
        for namespace in [self.control, self.workload, self.cluster, "reakt-platform"]:
            for resource in resources:
                if resource == "secrets":
                    metadata = self.k("get", "secrets", "-n", namespace, "-o", r'jsonpath={range .items[*]}{.metadata.name}{"\t"}{.metadata.uid}{"\t"}{.metadata.resourceVersion}{"\n"}{end}')
                    for line in metadata.splitlines():
                        fields = line.split("\t")
                        if len(fields) != 3 or any(not item for item in fields):
                            raise shared.QualificationError("invalid Secret metadata observation")
                        key = "/".join([namespace, resource, fields[0]])
                        raw_objects[key] = shared.canonical(dict(zip(["name", "uid", "resourceVersion"], fields)))
                        objects[key] = shared.digest_bytes(raw_objects[key])
                    if namespace == self.control:
                        binding = self.binding_metadata(self.k("get", "secret", "ceiling-binding", "-n", self.control, "-o", BINDING_PATH), self.secret_uid)
                        key = self.control + "/secrets/ceiling-binding"
                        raw_objects[key] = shared.canonical(binding)
                        objects[key] = shared.digest_bytes(raw_objects[key])
                    continue
                listed = json.loads(self.k("get", resource, "-n", namespace, "-o", "json"))["items"]
                for item in listed:
                    key = "/".join([namespace, resource, item["metadata"]["name"]])
                    raw_objects[key] = shared.canonical(item)
                    objects[key] = shared.digest_bytes(raw_objects[key])
                if resource in {"pods", "deployments", "replicasets"} and listed:
                    raise shared.QualificationError("ceiling reservation started a workload")
        for resource in ["clusterroles", "clusterrolebindings"]:
            item = json.loads(self.k("get", resource, self.cluster + "-target", "-o", "json"))
            raw_objects[resource] = shared.canonical(item)
            objects[resource] = shared.digest_bytes(raw_objects[resource])
        coordination, raw_coordination = observations.capture(self.coordination)
        inventory, raw_inventory = observations.capture(self.inventory)
        return {"objects": objects, "coordination": coordination, "inventory": inventory,
                "rawObjects": raw_objects, "rawCoordination": raw_coordination, "rawInventory": raw_inventory}

    def assert_targets_absent(self, prepared):
        if prepared is not None and prepared["creationTargetsSource"] != "capability-complete-projection":
            raise shared.QualificationError("unknown creation target projection")
        # None requests only the candidate's declared resource absence check.
        targets = [] if prepared is None else list(prepared["creationTargets"])
        kinds = {"ConfigMap": ("", "configmaps"), "Service": ("", "services"), "Deployment": ("apps", "deployments")}
        for resource in self.plan["ir"]["resources"]:
            group, plural = kinds[resource["kind"]]
            targets.append({"namespace": resource["namespace"], "apiGroup": group,
                            "resource": plural, "name": resource["name"]})
        for target in targets:
            if target["namespace"] not in {self.control, self.workload}:
                raise shared.QualificationError("creation target escaped candidate namespaces")
            resource = target["resource"] + ("." + target["apiGroup"] if target["apiGroup"] else "")
            if self.k("get", resource, target["name"], "-n", target["namespace"], "--ignore-not-found", "-o", "name").strip():
                raise shared.QualificationError("candidate or generation object exists during reservation-only measurement")

    def stable_baseline(self):
        previous = self.snapshot()
        for _ in range(15):
            time.sleep(1)
            current = self.snapshot()
            if current == previous:
                return current
            previous = current
        raise shared.QualificationError("namespace bootstrap did not stabilize")

    def measure(self):
        self.phase = "prepare-denial"
        prepared_denial = self.step("prepare", "prepared", self.issuer)
        self.phase = "denial-target-absence"
        self.assert_targets_absent(prepared_denial)
        self.phase = "denial-baseline"
        before = self.stable_baseline()
        self.phase = "measure-denial"
        denied = self.step("measure", "ceiling-rejected", self.issuer, expectedMissingCapability=self.missing,
                           expectedRequestDigest=prepared_denial["requestDigest"])
        if any(denied[key] != prepared_denial[key] for key in ["requestDigest", "registryUid", "registryResourceVersion", "releaseDigest", "generation", "transition"]):
            raise shared.QualificationError("denial differs from frozen request")
        after = self.snapshot()
        if before != after:
            raise shared.QualificationError("denied candidate mutated protected or metadata state")
        if denied["releaseDigest"] != self.verified["releaseDigest"] or denied["missingCapabilityDigest"] != shared.digest_bytes(shared.canonical(self.missing)):
            raise shared.QualificationError("ceiling denial measured another candidate or capability")
        self.policy_object["data"]["state.json"] = shared.canonical({**self.policy, "runtimeCeiling": ceiling(self.plan)}).decode()
        self.k("replace", "-f", "-", "-o", "json", data=json.dumps(self.policy_object))
        prepared_positive = self.step("prepare", "prepared", self.issuer)
        self.assert_targets_absent(prepared_positive)
        restored = self.snapshot()
        policy_key = self.control + "/configmaps/reakt-policy-" + self.domain
        unchanged_except(after["rawObjects"], restored["rawObjects"], {policy_key})
        if (after["rawCoordination"] != restored["rawCoordination"] or after["rawInventory"] != restored["rawInventory"]
                or any(prepared_positive[key] != denied[key] for key in ["generation", "transition", "registryUid", "registryResourceVersion", "releaseDigest"])):
            raise shared.QualificationError("policy restoration changed the frozen allocation or metadata state")
        planned = self.step("measure", "planned", self.issuer, expectedRequestDigest=prepared_positive["requestDigest"])
        self.assert_targets_absent(prepared_positive)
        if planned["requestDigest"] != prepared_positive["requestDigest"]:
            raise shared.QualificationError("positive control differs from frozen request")
        final = self.snapshot()
        if any(planned[key] != denied[key] for key in ["generation", "transition", "registryUid", "releaseDigest"]):
            raise shared.QualificationError("positive control used another candidate or allocation")
        registry_key = self.control + "/configmaps/reakt-registry-" + self.workload
        unchanged_except(restored["rawObjects"], final["rawObjects"], {registry_key})
        if restored["objects"][registry_key] == final["objects"][registry_key] or restored["rawInventory"] != final["rawInventory"]:
            raise shared.QualificationError("positive reservation did not update only expected state")
        observations.validate_reservation_rows(restored["coordination"], final["coordination"])
        for table, rows in restored["rawCoordination"].items():
            prior, current = Counter(rows), Counter(final["rawCoordination"][table])
            if table in {"reakt_coordination_claims", "reakt_coordination_review_objects"}:
                if (prior & current) != prior or sum((current - prior).values()) != 1:
                    raise shared.QualificationError("positive reservation changed retained canonical metadata rows")
            elif prior != current:
                raise shared.QualificationError("positive reservation changed unrelated canonical metadata rows")
        self.measurement = {"schemaVersion": "reakt.dev/deployment-ceiling-measurement/v1", "runnerSourceCommit": self.commit, "candidateSourceCommit": self.manifest["source"]["commit"],
                            "scope": "exact-candidate-reservation-on-owned-kind-cluster", "hostIdentity": self.host_identity, "dockerIdentity": self.docker_binding,
                            "steps": self.steps, "beforeDenial": public_snapshot(before), "afterDenial": public_snapshot(after),
                            "beforePositive": public_snapshot(restored), "afterPositive": public_snapshot(final), "runnerSources": self.source_digests,
                            "driverDigest": self.driver_digest, "releaseVerified": False}

    def cleanup(self):
        deleted = False
        try:
            if self.attempted:
                self.verify_docker()
                names = self.command(["docker", "ps", "-aq", "--no-trunc", "--filter", "label=io.x-k8s.kind.cluster=" + self.cluster]).splitlines()
                if names:
                    current = json.loads(self.command(["docker", "inspect", *names]))
                    if len(current) != 1:
                        raise shared.QualificationError("unexpected containers have this attempt's cluster label")
                    container = current[0]
                    labels = container.get("Config", {}).get("Labels", {})
                    if (container.get("Name") != "/" + self.cluster + "-control-plane"
                            or container.get("Image") != self.node_image_id
                            or labels.get("io.x-k8s.kind.cluster") != self.cluster
                            or labels.get("io.x-k8s.kind.role") != "control-plane"
                            or (self.container_id and container.get("Id") != self.container_id)):
                        raise shared.QualificationError("owned cluster container identity changed; deletion refused")
                    self.command(["kind", "delete", "cluster", "--name", self.cluster, "--kubeconfig", self.admin], timeout=180)
                deleted = self.cluster not in self.cluster_names() and not self.command([
                    "docker", "ps", "-aq", "--filter", "name=^/" + self.cluster + "-control-plane$"]).strip()
                current_clusters = sorted(self.cluster_names())
                current_containers = sorted(self.command(["docker", "ps", "-aq", "--no-trunc"]).splitlines())
                (self.output / "cleanup.json").write_bytes(shared.canonical({"cluster": self.cluster, "deleted": deleted,
                    "previousClusters": self.prior_clusters, "currentClusters": current_clusters,
                    "previousContainerIds": self.prior_containers, "currentContainerIds": current_containers}))
                if (not deleted or current_clusters != self.prior_clusters or current_containers != self.prior_containers):
                    raise shared.QualificationError("cluster/container cleanup or preservation was not confirmed")
        finally:
            failed = False
            for path in [self.admin, self.issuer]:
                try:
                    path.unlink(missing_ok=True)
                except OSError:
                    failed = True
            if failed:
                raise shared.QualificationError("runner credential removal failed")
        self.cleanup_confirmed = deleted
        return deleted

    def publish(self):
        self.verify_docker()
        if (shared.source_state(Path(__file__).resolve()) != (self.repository, self.commit)
                or any(shared.digest_file(path) != self.source_digests[path.name] for path in self.sources)
                or self.step("verify", "verified") != self.verified
                or shared.digest_file(self.driver) != self.driver_digest):
            raise shared.QualificationError("ceiling subject or measurement code changed")
        measurement_bytes = shared.canonical({**self.measurement, "clusterDeleted": True,
            "credentialsRemoved": not self.admin.exists() and not self.issuer.exists()})
        if len(measurement_bytes) > shared.MAX_EVIDENCE_BYTES or any(path.stat().st_size > shared.MAX_EVIDENCE_BYTES for path in self.sources):
            raise shared.QualificationError("ceiling evidence exceeds its size limit")
        evidence = self.output / "evidence"
        evidence.mkdir()
        for path in self.sources:
            shutil.copyfile(path, evidence / path.name)
            if shared.digest_file(evidence / path.name) != self.source_digests[path.name]:
                raise shared.QualificationError("staged ceiling source differs from measured source")
        (evidence / "deployment-ceiling-measurement.json").write_bytes(measurement_bytes)
        manifest = self.manifest
        receipt = {"schemaVersion": shared.SCHEMA, "irDigest": manifest["irDigest"], "compilerDigest": manifest["compilerDigest"],
                   "binaryDigest": manifest["binaryDigest"], "image": manifest["image"],
                   "sourceInputsDigest": shared.digest_bytes(shared.canonical(shared.read_json(self.artifact / "build-inputs.json"))),
                   "payloadFiles": {name: manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "reakt-controller"]},
                   "runnerSourceFile": "evidence/" + Path(__file__).name, "runnerSourceDigest": self.source_digests[Path(__file__).name],
                   "checks": {"deployment-ceiling-rejection": {"status": "passed", "assertions": [
                       {"checkId": name, "expected": value, "actual": value, "passed": True}
                       for name, value in [("above-ceiling-rejected", True), ("next-writer-started", False)]],
                       "evidenceFiles": ["evidence/deployment-ceiling-measurement.json", *["evidence/" + path.name for path in self.sources]]}}}
        (evidence / "deployment-ceiling-evidence.json").write_bytes(shared.canonical(receipt))
        return evidence


def failure_reason(runner, error):
    return str(error) if str(error) in getattr(runner, "failure_reasons", set()) else type(error).__name__


def main(runner_class=None, extra_arguments=()):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["artifact", "trust-policy", "output", *extra_arguments]:
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    runner = None
    try:
        runner = (runner_class or CeilingRunner)(args)
        try:
            runner.prepare()
            runner.phase = "cluster-create"
            runner.create_cluster()
            runner.phase = "bootstrap"
            runner.bootstrap()
            runner.phase = "measurement"
            runner.measure()
        except Exception as error:
            runner.primary_failure = {"phase": runner.phase, "reason": failure_reason(runner, error)}
            raise
        finally:
            runner.phase = "cleanup"
            deleted = runner.cleanup()
        if not deleted:
            raise shared.QualificationError("ceiling measurement did not own and delete its cluster")
        runner.phase = "publication"
        evidence = runner.publish()
        result = {"passed": True, "releaseVerified": False, "evidence": str(evidence),
                  "cluster": runner.cluster, "clusterDeleted": True, "credentialsRemoved": True}
        (runner.output / "result.json").write_bytes(shared.canonical(result))
        print(json.dumps(result))
    except Exception as error:
        reason = failure_reason(runner, error)
        if runner:
            (runner.output / "result.json").write_bytes(shared.canonical({"passed": False, "releaseVerified": False,
                "cluster": runner.cluster, "phase": runner.phase, "failureReason": reason, "primaryFailure": runner.primary_failure, "subprocessFailures": runner.failures, "owned": runner.owned, "cleanupConfirmed": runner.cleanup_confirmed, "credentialsRemoved": not runner.admin.exists() and not runner.issuer.exists()}))
        print("Deployment ceiling qualification failed; no completed evidence is available.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
