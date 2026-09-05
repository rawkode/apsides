#!/usr/bin/env python3
"""Real API lifecycle acceptance on one freshly created, explicitly marked kind cluster."""
import argparse
import base64
import http.client
import ssl
from urllib.parse import urlsplit, urlencode
import hashlib
import json
import os
from pathlib import Path
import subprocess
import shutil
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
NODE = "kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48"
CONTROL, WORKLOAD, DOMAIN = "reakt-test-control", "reakt-test-workload", "lifecycle"
BINDING = "reakt-disposable-test"


def digest(value):
    return "sha256:" + hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


class LifecycleRunner:
    def __init__(self, args):
        self.args = args
        self.output = args.output.resolve()
        self.output.mkdir()
        shutil.copyfile(__file__, self.output / "lifecycle-runner.py")
        self.cluster = "reakt-lifecycle-" + uuid.uuid4().hex[:8]
        self.admin = self.output / "admin.kubeconfig"
        self.issuer = self.output / "issuer.kubeconfig"
        self.results = []
        self.created = False
        self.adopted_uid = None
        self.candidates = [args.first.resolve(), args.second.resolve()]
        self.manifests = [json.loads((p / "release.json").read_text()) for p in self.candidates]
        scopes = [json.loads((p / "ir.json").read_text())["domain"] for p in self.candidates]
        if any(scopes[0][key] != scopes[1][key] for key in ["controlNamespace", "namespace", "id", "clusterBinding"]):
            raise RuntimeError("lifecycle candidates must share one domain and scope")
        scope = scopes[0]
        self.control, self.workload, self.domain, self.binding = (scope[k] for k in ["controlNamespace", "namespace", "id", "clusterBinding"])
        if not self.control.startswith("reakt-test-") or not self.workload.startswith("reakt-test-") or not self.binding.startswith("reakt-disposable-"):
            raise RuntimeError("test provisioner refuses non-disposable scopes")

    def command(self, args, data=None, check=True, env=None, timeout=180):
        result = subprocess.run([str(a) for a in args], input=data, text=True, capture_output=True,
                                check=False, cwd=ROOT, env=env, timeout=timeout)
        if check and result.returncode:
            raise RuntimeError(f"{args[0]} {args[1]} failed: {result.stderr[-2500:]}")
        return result

    def k(self, *args, data=None, check=True):
        return self.command(["kubectl", "--kubeconfig", self.admin, *args], data=data, check=check)

    def apply(self, value):
        return json.loads(self.k("apply", "-f", "-", "-o", "json", data=json.dumps(value)).stdout)

    def state(self, name, value):
        return self.apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": self.control},
                           "data": {"state.json": json.dumps(value, sort_keys=True, separators=(",", ":"))}})

    def get(self, kind, name, namespace=None):
        namespace = namespace or self.workload
        result = self.k("get", kind, name, "-n", namespace, "-o", "json", "--ignore-not-found")
        return json.loads(result.stdout) if result.stdout.strip() else None

    def cargo(self, test, env=None):
        return self.command([self.driver, test, "--exact", "--ignored", "--nocapture"], check=False, env=env, timeout=120)

    def prepare_driver(self):
        result = self.command(["cargo", "test", "-p", "platform", "--features", "test-support", "--test", "live_candidate",
                               "--offline", "--no-run", "--message-format=json"], timeout=240)
        executables = [row["executable"] for line in result.stdout.splitlines()
                       if (row := json.loads(line)).get("reason") == "compiler-artifact"
                       and row.get("target", {}).get("name") == "live_candidate" and row.get("executable")]
        if len(executables) != 1:
            raise RuntimeError("Cargo did not identify exactly one lifecycle test driver")
        self.driver = self.output / "platform-test-runner"
        shutil.copy2(executables[0], self.driver)
        (self.output / "driver.json").write_text(json.dumps({"sha256": "sha256:" + hashlib.sha256(self.driver.read_bytes()).hexdigest(),
                                                           "scope": "frozen test-support executable; not a production release"}, indent=2))

    def create(self):
        self.command(["docker", "image", "inspect", NODE])
        self.created = True
        self.command(["kind", "create", "cluster", "--name", self.cluster, "--image", NODE,
                      "--kubeconfig", self.admin, "--wait", "120s"])
        for namespace in [self.control, self.workload]:
            self.apply({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": namespace}})
        profile_result = self.cargo("proxy_profile")
        if profile_result.returncode:
            raise RuntimeError(profile_result.stderr)
        profile = next(json.loads(line.split("=", 1)[1]) for line in profile_result.stdout.splitlines() if line.startswith("REAKT_TEST_PROFILE="))
        self.profile = profile
        for index, candidate in enumerate(self.candidates):
            self.import_archive(candidate / "controller.oci.tar", "localhost/reakt-lifecycle", self.manifests[index]["image"]["reference"])
        for key in ["envoyImage", "adminImage"]:
            reference = profile[key]
            archive = self.output / (key + ".tar")
            self.command(["docker", "image", "save", "-o", archive, reference])
            self.import_archive(archive, reference.split("@")[0], reference)
        cluster_uid = self.get("namespace", "kube-system", "default")["metadata"]["uid"]
        marker = self.state("reakt-test-support", {"schemaVersion": "reakt.dev/disposable-test-cluster/v1", "clusterBinding": self.binding,
                            "controlNamespace": self.control, "workloadNamespace": self.workload, "kubeSystemNamespaceUid": cluster_uid})
        self.target = {"kubeSystemNamespaceUid": cluster_uid, "markerUid": marker["metadata"]["uid"]}
        self.bootstrap()

    def import_archive(self, archive, repository, reference):
        node = self.cluster + "-control-plane"
        if "." not in repository.split("/")[0] and ":" not in repository.split("/")[0] and repository.split("/")[0] != "localhost":
            repository = "docker.io/" + repository
        with archive.open("rb") as payload:
            result = subprocess.run(["docker", "exec", "-i", node, "ctr", "-n", "k8s.io", "images", "import", "--digests", "--platform", "linux/arm64", "--base-name", repository, "-"],
                                    stdin=payload, capture_output=True, text=True, timeout=180, check=False)
        with (self.output / "image-import.log").open("a") as log:
            log.write(reference + "\n" + result.stdout + result.stderr)
        if result.returncode:
            raise RuntimeError("containerd OCI import failed: " + result.stderr[-2500:])
        self.command(["docker", "exec", node, "crictl", "inspecti", reference])

    def role(self, namespace, name, rules, account):
        self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": {"name": name, "namespace": namespace}, "rules": rules})
        self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": name, "namespace": namespace},
                    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": name},
                    "subjects": [{"kind": "ServiceAccount", "name": account, "namespace": self.control}]})

    def bootstrap(self):
        plans = [json.loads((p / "plan.json").read_text()) for p in self.candidates]
        for plan in plans:
            if plan["ir"]["domain"]["clusterBinding"] != self.binding:
                raise RuntimeError("candidate scope differs from disposable cluster")
        capabilities = {json.dumps(c, sort_keys=True): c for p in plans for c in p["capabilities"]}
        ceiling = {"policyRef": plans[0]["ir"]["domain"]["policyRef"], "domainId": self.domain, "clusterBinding": self.binding, "workloadNamespace": self.workload,
                   "controlNamespace": self.control, "capabilities": list(capabilities.values()),
                   "acceptedEnforcementGaps": sorted({g for p in plans for g in p["enforcementGaps"]}),
                   "acceptedIndirectRisks": sorted({g for p in plans for g in p["indirectRisks"]})}
        cleanup = dict(ceiling)
        identities = {(r["kind"], r["name"]) for plan in plans for r in plan["ir"]["resources"]}
        kinds = {"ConfigMap": ("", "configmaps"), "Service": ("", "services"), "Deployment": ("apps", "deployments")}
        cleanup["capabilities"] = [{"namespace": self.workload, "apiGroup": kinds[kind][0], "resource": kinds[kind][1],
                                    "subresource": None, "verb": verb, "names": [name]}
                                   for kind, name in sorted(identities) for verb in ["get", "delete"]]
        self.state("reakt-policy-" + self.domain, {"schemaVersion": "reakt.dev/platform/v1", "runtimeCeiling": ceiling, "cleanupCeiling": cleanup})
        public_key = self.args.public_key.read_text().strip()
        self.state("reakt-trust-" + self.domain, {"schemaVersion": "reakt.dev/trust/v1", "repository": self.manifests[0]["source"]["repository"],
                    "approvedCompilerDigests": sorted({m["compilerDigest"] for m in self.manifests}),
                    "verifier": {"executable": "/usr/bin/ssh-keygen", "sha256": "sha256:" + hashlib.sha256(Path("/usr/bin/ssh-keygen").read_bytes()).hexdigest()},
                    "signature": {"kind": "open-ssh", "principal": "reakt-disposable-test", "allowedSigners": "reakt-disposable-test " + public_key + "\n"}})
        qualification = json.loads(self.args.proxy_qualification.read_text())
        self.state("reakt-proxy-policy-" + self.domain, {"schemaVersion": "reakt.dev/platform/v1", "domainId": self.domain, "clusterBinding": self.binding,
                    **self.profile, "environmentQualificationDigest": digest(qualification), "nodeImageDigest": NODE.split("@")[1], "certificateLifetimeSeconds": 86400})
        registry_name = "reakt-registry-" + self.workload
        registry = self.get("configmap", registry_name, self.control)
        state = json.loads(registry["data"]["state.json"]) if registry else {"schemaVersion": "reakt.dev/platform/v1", "clusterBinding": self.binding,
                 "workloadNamespace": self.workload, "claims": [], "transitions": {}, "histories": {}}
        if state["clusterBinding"] != self.binding or state["workloadNamespace"] != self.workload or self.domain in state["histories"]:
            raise RuntimeError("test bootstrap refuses to replace existing domain history or scope")
        state["histories"][self.domain] = {"domainId": self.domain, "clusterBinding": self.binding, "revision": "bootstrap", "activeGenerationId": None, "allocations": []}
        if registry:
            registry["data"]["state.json"] = json.dumps(state, sort_keys=True, separators=(",", ":"))
            self.k("replace", "-f", "-", data=json.dumps(registry))
        else:
            self.state(registry_name, state)
        for account in ["test-deployer", "test-issuer"]:
            self.apply({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": account, "namespace": self.control}, "automountServiceAccountToken": False})
        secret = self.apply({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "deployment-binding", "namespace": self.control}, "immutable": True, "type": "Opaque"})
        account = self.get("serviceaccount", "test-deployer", self.control)
        self.state("reakt-deploy-identity-" + self.domain, {"schemaVersion": "reakt.dev/platform/v1", "domainId": self.domain, "clusterBinding": self.binding,
                    "serviceAccountName": "test-deployer", "serviceAccountUid": account["metadata"]["uid"], "bindingSecretName": "deployment-binding",
                    "bindingSecretUid": secret["metadata"]["uid"], "audience": "https://kubernetes.default.svc.cluster.local", "lifetimeSeconds": 600})
        self.permissions()
        config = json.loads(self.k("config", "view", "--raw", "--minify", "-o", "json").stdout)
        token = self.k("create", "token", "test-issuer", "-n", self.control, "--duration=10m").stdout.strip()
        config["users"] = [{"name": "test-issuer", "user": {"token": token}}]
        config["contexts"][0]["context"]["user"] = "test-issuer"
        self.issuer.write_text(json.dumps(config))
        self.issuer.chmod(0o600)

    def permissions(self):
        def rule(group, resources, verbs, names=None):
            value = {"apiGroups": [group], "resources": resources, "verbs": verbs}
            if names is not None:
                value["resourceNames"] = names
            return value
        rules = [rule("", ["configmaps"], ["get", "create"]),
                 rule("", ["configmaps"], ["update"], ["reakt-registry-"+self.workload, "reakt-activation-"+self.domain, "reakt-inventory-"+self.domain]),
                 rule("", ["secrets"], ["get", "create"]), rule("", ["serviceaccounts"], ["get", "create", "delete"]),
                 rule("", ["pods"], ["get", "list"]), rule("", ["pods/exec"], ["get"]), rule("", ["services"], ["get", "create"]),
                 rule("apps", ["deployments"], ["get", "create", "update"]), rule("apps", ["replicasets"], ["get"]),
                 rule("rbac.authorization.k8s.io", ["roles", "rolebindings"], ["get", "create"]), rule("rbac.authorization.k8s.io", ["rolebindings"], ["delete"])]
        self.role(self.control, "test-deployer", rules, "test-deployer")
        self.role(self.workload, "test-deployer", [rule("", ["configmaps", "services"], ["get", "list", "watch", "create", "patch", "delete"]),
                  rule("rbac.authorization.k8s.io", ["roles", "rolebindings"], ["get", "create"]), rule("rbac.authorization.k8s.io", ["rolebindings"], ["delete"])], "test-deployer")
        self.role(self.control, "test-issuer", [rule("", ["configmaps"], ["get"], ["reakt-trust-"+self.domain, "reakt-deploy-identity-"+self.domain]),
                  rule("", ["serviceaccounts"], ["get"], ["test-deployer"]), rule("", ["serviceaccounts/token"], ["create"], ["test-deployer"]),
                  rule("", ["secrets"], ["get"], ["deployment-binding"])], "test-issuer")
        name = "reakt-disposable-" + self.cluster
        self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": name},
                    "rules": [rule("", ["nodes"], ["get"]), rule("", ["namespaces"], ["get"], ["kube-system"])]})
        self.apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding", "metadata": {"name": name},
                    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": name},
                    "subjects": [{"kind": "ServiceAccount", "name": "test-deployer", "namespace": self.control}]})

    def step(self, candidate, transition, action, timeout=120, expected_pending=False, expected_error=None):
        path = self.output / "step.json"
        path.write_text(json.dumps({"artifact": str(candidate), "transition": transition, "action": action, "target": self.target,
                                   "ownershipIntents": ["adopt-retained"] if self.adopted_uid and transition == "install" else []}))
        environment = dict(os.environ, KUBECONFIG=str(self.issuer), REAKT_TEST_STEP=str(path))
        deadline = time.monotonic() + timeout
        while True:
            result = self.cargo("candidate_transition_step", env=environment)
            records = [json.loads(line.split("=", 1)[1]) for line in result.stdout.splitlines() if line.startswith("REAKT_TEST_RESULT=") or line.startswith("REAKT_TEST_ERROR=")]
            self.results.append({"transition": transition, "action": action, "records": records, "exitCode": result.returncode})
            (self.output / "steps.json").write_text(json.dumps(self.results, indent=2))
            if expected_error is not None:
                if result.returncode and records and not records[-1].get("pending") and expected_error in records[-1].get("message", ""):
                    return
                raise RuntimeError("expected the specific cleanup rejection: " + result.stdout[-2000:] + result.stderr[-1000:])
            if expected_pending:
                if result.returncode and records and records[-1].get("pending"):
                    return
                raise RuntimeError("expected a typed pending cleanup result: " + result.stdout[-2000:] + result.stderr[-1000:])
            if result.returncode == 0:
                return
            if not records or not records[-1].get("pending") or time.monotonic() >= deadline:
                raise RuntimeError(result.stdout[-3500:] + result.stderr[-1500:])
            time.sleep(1)

    def wait_value(self, value):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            obj = self.get("configmap", "retained")
            if obj and obj.get("data", {}).get("value") == value:
                return obj
            time.sleep(1)
        raise RuntimeError("active controller did not converge retained ConfigMap")

    def prepare_adoption(self):
        resource = next(r for r in json.loads((self.candidates[0] / "ir.json").read_text())["resources"] if r["name"] == "retained")
        self.apply({key: resource["desired"][key] for key in ["apiVersion", "kind", "metadata"]})
        existing = json.loads(self.k("get", "configmap", "retained", "-n", self.workload, "--show-managed-fields=true", "-o", "json").stdout)
        self.adopted_uid = existing["metadata"]["uid"]
        self.apply({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "reakt-platform-intents"}})
        release_digest = "sha256:" + hashlib.sha256((self.candidates[0] / "release.json").read_bytes()).hexdigest()
        approval = {
            "schemaVersion": "reakt.dev/ownership-intent/v1", "transitionId": "install",
            "domainId": self.domain, "generationId": "g1-" + release_digest[7:19], "releaseDigest": release_digest,
            "clusterBinding": self.binding, "controlNamespace": self.control,
            "identity": {"apiGroup": "", "resource": "configmaps", "namespace": self.workload, "name": "retained"},
            "observedUid": self.adopted_uid, "observedResourceVersion": existing["metadata"]["resourceVersion"],
            "priorManagedFieldsDigest": digest(existing["metadata"].get("managedFields")),
            "permittedApply": resource["desired"], "permittedRuntimeFields": [],
            "deletionPolicy": resource["lifecycle"]["deletionPolicy"], "priorManagerConflicts": [], "action": {"kind": "adopt"},
        }
        self.apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "adopt-retained", "namespace": "reakt-platform-intents"},
                    "data": {"state.json": json.dumps(approval, sort_keys=True, separators=(",", ":"))}})
        self.role("reakt-platform-intents", "read-adoption", [{"apiGroups": [""], "resources": ["configmaps"],
                  "verbs": ["get"], "resourceNames": ["adopt-retained"]}], "test-deployer")

    def old_credential(self):
        registry = json.loads(self.get("configmap", "reakt-registry-" + self.workload, self.control)["data"]["state.json"])
        account = registry["transitions"]["install"]["runtimeBinding"]["serviceAccountName"]
        config = json.loads(self.k("config", "view", "--raw", "--minify", "-o", "json").stdout)
        token = self.k("create", "token", account, "-n", self.control, "--duration=10m").stdout.strip()
        config["users"] = [{"name": "old-generation", "user": {"token": token}}]
        config["contexts"][0]["context"]["user"] = "old-generation"
        path = self.output / "old-generation.kubeconfig"
        with open(path, "w", opener=lambda name, flags: os.open(name, flags, 0o600)) as stream:
            json.dump(config, stream)
        identity = self.command(["kubectl", "--kubeconfig", path, "auth", "whoami", "-o", "json"])
        expected = f"system:serviceaccount:{self.control}:{account}"
        if json.loads(identity.stdout)["status"]["userInfo"]["username"] != expected:
            raise RuntimeError("old credential did not authenticate as the old generation")
        result = self.command(["kubectl", "--kubeconfig", path, "get", "configmaps", "-n", self.workload,
                               "--field-selector=metadata.name=retained", "-o", "json"])
        items = json.loads(result.stdout)["items"]
        if len(items) != 1 or items[0]["metadata"]["name"] != "retained":
            raise RuntimeError("old credential could not read the retained resource before retirement")
        return path, time.monotonic()

    def verify_retired_credential(self, path, issued_at):
        config = json.loads(path.read_text())
        cluster = config["clusters"][0]["cluster"]
        server = urlsplit(cluster["server"])
        if server.scheme != "https" or server.username or server.password:
            raise RuntimeError("credential probe requires the verified TLS API endpoint")
        context = ssl.create_default_context(cadata=base64.b64decode(cluster["certificate-authority-data"]).decode())
        token = config["users"][0]["user"]["token"]
        route = server.path.rstrip("/") + f"/api/v1/namespaces/{self.workload}/configmaps?" + urlencode({"fieldSelector": "metadata.name=retained"})
        deadline = min(time.monotonic() + 30, issued_at + 540)
        probes = []
        while time.monotonic() < deadline:
            connection = http.client.HTTPSConnection(server.hostname, server.port, timeout=5, context=context)
            try:
                connection.request("GET", route, headers={"Authorization": "Bearer " + token})
                response = connection.getresponse()
                status = response.status
                probes.append({"status": status, "secondsSinceIssue": time.monotonic() - issued_at})
            finally:
                connection.close()
            (self.output / "old-credential-probes.json").write_text(json.dumps(probes, indent=2))
            if status in (401, 403):
                self.results.append({"check": "old-generation-credential-revoked-before-expiry", "passed": True, "httpStatus": status})
                return
            if status != 200:
                raise RuntimeError(f"credential probe returned unexpected HTTP {status}")
            time.sleep(1)
        raise RuntimeError("old generation credential was not denied before the test expiry boundary")

    def verify_replacement_cleanup(self, old_uid):
        self.k("patch", "service", "obsolete-service", "-n", self.workload, "--type=merge", "-p",
               json.dumps({"metadata": {"finalizers": []}}))
        self.k("delete", "service", "obsolete-service", "-n", self.workload, "--wait=true")
        resource = next(r for r in json.loads((self.candidates[0] / "ir.json").read_text())["resources"] if r["name"] == "obsolete-service")
        # Deliberately reuse the cooperative manager string: it cannot authorize deletion of a new UID.
        replacement = json.loads(self.k("apply", "--server-side", "--field-manager=reakt-" + self.domain,
                                        "-f", "-", "-o", "json", data=json.dumps(resource["desired"])).stdout)
        new_uid = replacement["metadata"]["uid"]
        if old_uid == new_uid:
            raise RuntimeError("replacement fixture did not obtain a new UID")
        self.step(self.candidates[1], "upgrade", "commit", expected_error="cleanup refuses to delete a replacement UID")
        preserved = self.get("service", "obsolete-service")
        if not preserved or preserved["metadata"]["uid"] != new_uid or preserved["metadata"].get("deletionTimestamp"):
            raise RuntimeError("cleanup mutated or deleted the replacement object")
        registry = json.loads(self.get("configmap", "reakt-registry-" + self.workload, self.control)["data"]["state.json"])
        claims = [c for c in registry["claims"] if c["identity"]["name"] == "obsolete-service"]
        if len(claims) != 1 or claims[0]["uid"] != old_uid:
            raise RuntimeError("cleanup changed or released the protected original UID claim")
        self.results.append({"check": "replacement-uid-cleanup-rejected-and-claim-retained", "passed": True,
                             "originalUid": old_uid, "replacementUid": new_uid})

    def run(self):
        passed = False
        try:
            self.prepare_driver()
            self.create()
            if self.args.adopt_retained:
                self.prepare_adoption()
            for action in ["reserve", "stage", "activate"]:
                self.step(self.candidates[0], "install", action)
            old = self.wait_value("version-1")
            if self.adopted_uid:
                if old["metadata"]["uid"] != self.adopted_uid:
                    raise RuntimeError("adoption replaced the operator-approved object")
                deadline = time.monotonic() + 90
                while True:
                    observed = json.loads(self.k("get", "configmap", "retained", "-n", self.workload, "--show-managed-fields=true", "-o", "json").stdout)
                    if observed["metadata"]["uid"] != self.adopted_uid:
                        raise RuntimeError("adoption target UID changed while awaiting ownership")
                    if any(entry.get("manager") == "reakt-" + self.domain and entry.get("operation") == "Apply"
                           for entry in observed["metadata"].get("managedFields", [])):
                        break
                    if time.monotonic() >= deadline:
                        raise RuntimeError("approved adoption did not establish the stable SSA manager")
                    time.sleep(1)
                self.results.append({"check": "operator-approved-adoption-preserves-uid", "passed": True, "ssaManagerObserved": True})
            deadline = time.monotonic() + 90
            while self.get("service", "obsolete-service") is None or self.get("configmap", "orphan") is None:
                if time.monotonic() >= deadline:
                    raise RuntimeError("initial generation did not create all lifecycle targets")
                time.sleep(1)
            self.step(self.candidates[1], "upgrade", "reserve")
            obsolete = self.get("service", "obsolete-service")
            obsolete_uid = obsolete["metadata"]["uid"]
            self.k("patch", "service", "obsolete-service", "-n", self.workload, "--type=merge", "-p",
                   json.dumps({"metadata": {"finalizers": ["reakt.test/hold-cleanup"]}}))
            self.step(self.candidates[1], "upgrade", "stage")
            old_credential, issued_at = self.old_credential()
            self.step(self.candidates[1], "upgrade", "quiesce")
            self.verify_retired_credential(old_credential, issued_at)
            if self.args.force_retained:
                target = next(r for r in json.loads((self.candidates[1] / "ir.json").read_text())["resources"] if r["name"] == "retained")
                if target.get("force") is not True:
                    raise RuntimeError("force test requires explicit force in the approved candidate")
                foreign = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "retained", "namespace": self.workload}, "data": {"value": "foreign"}}
                self.k("apply", "--server-side", "--force-conflicts", "--field-manager=fixture-foreign", "-f", "-", data=json.dumps(foreign))
            self.step(self.candidates[1], "upgrade", "activate")
            new = self.wait_value("version-2")
            if old["metadata"]["uid"] != new["metadata"]["uid"]:
                raise RuntimeError("upgrade replaced retained resource UID")
            if self.args.force_retained:
                self.results.append({"check": "approved-force-reconciles-foreign-field-without-uid-replacement", "passed": True})
            if self.args.replace_cleanup_target:
                self.verify_replacement_cleanup(obsolete_uid)
                passed = True
                return
            self.step(self.candidates[1], "upgrade", "commit", expected_pending=True)
            held = self.get("service", "obsolete-service")
            if not held or held["metadata"]["uid"] != obsolete_uid or not held["metadata"].get("deletionTimestamp") or held["metadata"].get("finalizers") != ["reakt.test/hold-cleanup"]:
                raise RuntimeError("cleanup bypassed the real finalizer or replaced the target")
            registry = json.loads(self.get("configmap", "reakt-registry-" + self.workload, self.control)["data"]["state.json"])
            if not any(c["identity"]["name"] == "obsolete-service" for c in registry["claims"]):
                raise RuntimeError("cleanup released claim before finalizer completion")
            self.k("patch", "service", "obsolete-service", "-n", self.workload, "--type=merge", "-p",
                   json.dumps({"metadata": {"finalizers": []}}))
            self.step(self.candidates[1], "upgrade", "commit")
            self.step(self.candidates[1], "upgrade", "commit")
            if self.get("service", "obsolete-service") is not None or self.get("configmap", "orphan") is None:
                raise RuntimeError("prior deletion/orphan policy was not preserved")
            passed = True
        finally:
            (self.output / "result.json").write_text(json.dumps({"passed": passed, "releaseVerified": False, "cluster": self.cluster,
                "scope": "signed candidate deployment through actual short-lived credentials; not final release acceptance", "steps": self.results}, indent=2))
            if self.created:
                for namespace in [self.control, self.workload]:
                    result = self.k("get", "pods,deployments,configmaps,events", "-n", namespace, "-o", "json", check=False)
                    (self.output / (namespace + ".json")).write_text(result.stdout)
                    pods = self.k("get", "pods", "-n", namespace, "-o", "json", check=False)
                    if pods.returncode == 0:
                        for pod in json.loads(pods.stdout).get("items", []):
                            name = pod["metadata"]["name"]
                            logs = self.k("logs", name, "-n", namespace, "--all-containers", "--prefix", "--tail=200", check=False)
                            (self.output / (namespace + "-" + name + ".log")).write_text(logs.stdout + logs.stderr)
                self.command(["kind", "delete", "cluster", "--name", self.cluster], check=False)
            self.issuer.unlink(missing_ok=True)
            self.admin.unlink(missing_ok=True)
            (self.output / "old-generation.kubeconfig").unlink(missing_ok=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["first", "second", "public-key", "proxy-qualification", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--adopt-retained", action="store_true", help="exercise explicit operator-approved adoption before upgrade")
    parser.add_argument("--replace-cleanup-target", action="store_true", help="verify real cleanup rejects a replacement UID and retains its claim")
    parser.add_argument("--force-retained", action="store_true", help="verify policy-approved force repairs a foreign SSA field after writer retirement")
    LifecycleRunner(parser.parse_args()).run()
