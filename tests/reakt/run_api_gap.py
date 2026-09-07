#!/usr/bin/env python3
"""Exercise exact CNPG generation credentials and disclosed API-bound gaps."""
import argparse
import ast
import base64
from datetime import datetime, timezone
import json
import os
import re
from pathlib import Path
import ssl
import sys

import cnpg_credential_probes as probes
import run_cnpg_demo as demo

shared = demo.shared
KEY = "aps-qualification-probe"
STAGES = {"cnpg-lifecycle", "identity-pin", "projected-token", "http-probes", "sensitive-revision", "complete"}


def require(condition, message):
    if not condition:
        raise shared.QualificationError(message)



def static_failure_reasons(source):
    return {node.args[1].value for node in ast.walk(ast.parse(source))
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "require"
            and len(node.args) == 2 and isinstance(node.args[1], ast.Constant) and isinstance(node.args[1].value, str)}

def tls_endpoint(config):
    require(len(config.get("clusters", [])) == 1, "API probe needs one cluster")
    cluster = config["clusters"][0]["cluster"]
    require(not cluster.get("insecure-skip-tls-verify") and cluster.get("certificate-authority-data"),
            "API probe requires embedded verified CA")
    context = ssl.create_default_context(cadata=base64.b64decode(cluster["certificate-authority-data"], validate=True).decode())
    return cluster["server"], context


def token_identity(token, review, binding, now):
    """TokenReview authenticates claims; decoding alone never establishes trust."""
    require(isinstance(token, str) and len(token) <= 65536 and len(token.split(".")) == 3,
            "API token is malformed")
    part = token.split(".")[1]
    claims = json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
    kube = claims.get("kubernetes.io", {})
    status = review.get("status", {})
    user = status.get("user", {})
    expected = "system:serviceaccount:" + binding["namespace"] + ":" + binding["serviceAccountName"]
    require(status.get("authenticated") is True and user.get("username") == expected
            and user.get("uid") == binding["serviceAccountUid"] and claims.get("sub") == expected
            and kube.get("namespace") == binding["namespace"]
            and kube.get("serviceaccount") == {"name": binding["serviceAccountName"], "uid": binding["serviceAccountUid"]}
            and kube.get("pod") == {"name": binding["proxyPodName"], "uid": binding["proxyPodUid"]}
            and type(claims.get("exp")) is int and now + 300 < claims["exp"] <= now + binding["projectionLifetimeSeconds"] + 60
            and user.get("extra", {}).get("authentication.kubernetes.io/pod-uid") == [binding["proxyPodUid"]],
            "API token does not authenticate the bounded generation Pod identity")
    audiences = claims.get("aud")
    require(isinstance(audiences, list) and audiences and all(isinstance(a, str) and a for a in audiences)
            and bool(set(status.get("audiences", [])) & set(audiences)), "API token audience was not authenticated")
    return {"expiresAt": claims["exp"], "audiences": sorted(audiences), "tokenReviewAuthenticated": True,
            "credentialKind": "actual-projected-generation-proxy-token"}


def secret_patch(secret, value, remove=False):
    metadata = secret.get("metadata", {})
    data = secret.get("data", {})
    require(metadata.get("name") == "database-app" and metadata.get("namespace") == "default"
            and metadata.get("uid") and metadata.get("resourceVersion") and data.get("uri")
            and (data.get(KEY) == value if remove else KEY not in data), "Secret probe lacks exact guarded state")
    patch = [{"op": "test", "path": "/metadata/uid", "value": metadata["uid"]},
             {"op": "test", "path": "/metadata/resourceVersion", "value": metadata["resourceVersion"]}]
    if remove:
        patch += [{"op": "test", "path": "/data/" + KEY, "value": value}, {"op": "remove", "path": "/data/" + KEY}]
    else:
        patch.append({"op": "add", "path": "/data/" + KEY, "value": value})
    return patch


def revision_changed(before, after):
    require(before["deploymentUid"] == after["deploymentUid"]
            and before["selectedImageManifestDigest"] == after["selectedImageManifestDigest"]
            and before["containerImageId"] == after["containerImageId"]
            and all(before[k] != after[k] for k in ["revision", "podUid", "replicaSetUid"])
            and all(re.fullmatch(r"hmac-sha256:[A-Za-z0-9_.-]{1,128}:[0-9a-f]{64}", str(item["revision"])) for item in [before, after])
            and before["revision"].split(":")[1] == after["revision"].split(":")[1],
            "Secret revision did not cause opaque revision and Pod turnover")


class ApiGapRunner(demo.CnpgDemoRunner):
    def __init__(self, args):
        args.retain_on_success = False
        super().__init__(args)
        self.failure_reasons |= static_failure_reasons(Path(__file__).read_text())
        for source, alias in [(Path(__file__).resolve(), "api-gap-runner.py"),
                              (Path(probes.__file__).resolve(), "api-gap-probes.py")]:
            require(shared.source_state(source) == (self.repository, self.commit), "API probe source is not frozen")
            self.sources.append(source)
            self.source_archives[source] = alias
            self.source_digests[source] = shared.digest_file(source)
            self.freeze_file("producer/" + alias, source)

    def checkpoint(self, stage):
        require(stage in STAGES, "API probe diagnostic stage is invalid")
        self.phase = "api-gap-" + stage
        (self.output / "progress.json").write_bytes(shared.canonical({"stage": stage}))

    def public_binding(self):
        registry = json.loads(self.k("get", "configmap", "reakt-registry-" + self.workload, "-n", self.control, "-o", "json"))
        state = json.loads(registry["data"]["state.json"])
        stored = state["transitions"][self.successor["transition"]]
        binding, proxy = stored["runtimeBinding"], stored["proxyIdentity"]
        activation = json.loads(self.k("get", "configmap", "reakt-activation-" + self.domain, "-n", self.control, "-o", "json"))
        active = json.loads(activation["data"]["state.json"])
        require(stored["record"]["phase"] == "Committed" and active["status"] == "active"
                and active["binding"] == binding and binding["generationId"] == self.successor["generation"]
                and binding["serviceAccountName"] == self.successor["serviceAccountName"], "API probe generation is not active")
        account = json.loads(self.k("get", "serviceaccount", binding["serviceAccountName"], "-n", self.control, "-o", "json"))
        pod = json.loads(self.k("get", "pod", proxy["podName"], "-n", self.control, "-o", "json"))
        uid_key = "v1/ServiceAccount/" + self.control + "/" + binding["serviceAccountName"]
        require(account["metadata"]["uid"] == stored["stagedIdentityUids"][uid_key]
                and not account["metadata"].get("deletionTimestamp") and pod["metadata"]["uid"] == proxy["podUid"]
                and not pod["metadata"].get("deletionTimestamp") and pod["spec"]["serviceAccountName"] == binding["serviceAccountName"],
                "API probe ServiceAccount or proxy Pod changed")
        projections = [source["serviceAccountToken"] for volume in pod["spec"].get("volumes", [])
                       if volume["name"] == "api-token" for source in volume.get("projected", {}).get("sources", [])
                       if "serviceAccountToken" in source]
        require(len(projections) == 1 and projections[0].get("path") == "token", "proxy token projection is ambiguous")
        containers = [c for c in pod.get("status", {}).get("containerStatuses", []) if c["name"] == "envoy"]
        require(len(containers) == 1 and containers[0].get("containerID") == proxy["envoyContainerId"]
                and containers[0].get("restartCount") == 0 and containers[0].get("ready") is True
                and shared.digest_bytes(shared.canonical(pod["spec"])) == proxy["podSpecDigest"],
                "API proxy container or specification changed")
        roles = []
        for namespace in [self.control, self.workload]:
            for kind in ["roles", "rolebindings"]:
                rows = json.loads(self.k("get", kind, "-n", namespace, "-o", "json"))["items"]
                rows.sort(key=lambda row: (row["metadata"]["name"], row["metadata"]["uid"]))
                roles.append({"namespace": namespace, "kind": kind, "digest": shared.digest_bytes(shared.canonical(rows))})
        return {"releaseDigest": self.pair["candidates"]["successorReleaseDigest"], "generation": binding["generationId"],
                "namespace": self.control, "serviceAccountName": binding["serviceAccountName"],
                "serviceAccountUid": account["metadata"]["uid"], "proxyPodName": proxy["podName"],
                "proxyPodUid": proxy["podUid"], "projectionAudience": projections[0].get("audience"),
                "projectionLifetimeSeconds": projections[0]["expirationSeconds"],
                "envoyContainerId": containers[0]["containerID"], "podSpecDigest": proxy["podSpecDigest"],
                "authorizationDigests": roles}

    def credential(self, binding):
        token = self.k("exec", binding["proxyPodName"], "-n", self.control, "--container", "envoy", "--",
                       "/bin/cat", "/var/run/secrets/kubernetes.io/serviceaccount/token").strip()
        require(self.public_binding() == binding, "API proxy changed while reading projected token")
        spec = {"token": token}
        if binding["projectionAudience"]:
            spec["audiences"] = [binding["projectionAudience"]]
        review = json.loads(self.k("create", "--raw", "/apis/authentication.k8s.io/v1/tokenreviews", "-f", "-", data=json.dumps({
            "apiVersion": "authentication.k8s.io/v1", "kind": "TokenReview", "spec": spec})))
        public = token_identity(token, review, binding, int(datetime.now(timezone.utc).timestamp()))
        return token, public

    def configmap_versions(self):
        result = {}
        for name in ["api-config", "api-settings"]:
            value = json.loads(self.k("get", "configmap", name, "-n", self.workload, "-o", "json"))
            result[name] = {key: value["metadata"][key] for key in ["uid", "resourceVersion"]}
        return result

    def secret_observation(self):
        before = self.workload_identity()
        configmaps = self.configmap_versions()
        secret = json.loads(self.k("get", "secret", "database-app", "-n", self.workload, "-o", "json"))
        value = base64.b64encode(os.urandom(24)).decode()
        added = False
        try:
            self.k("patch", "secret", "database-app", "-n", self.workload, "--type=json", "-p", json.dumps(secret_patch(secret, value)), "-o", "name")
            added = True
            current = json.loads(self.k("get", "secret", "database-app", "-n", self.workload, "-o", "json"))
            require(current["metadata"]["uid"] == secret["metadata"]["uid"] and current["data"].get(KEY) == value
                    and {k: v for k, v in current["data"].items() if k != KEY} == secret["data"], "Secret probe changed connection binding")
            after = self.workload_identity(previous_uid=before["podUid"])
            revision_changed(before, after)
            observed = json.loads(self.k("get", "secret", "database-app", "-n", self.workload, "-o", "json"))
            require(observed["metadata"]["uid"] == secret["metadata"]["uid"] and observed["data"].get(KEY) == value
                    and {k: v for k, v in observed["data"].items() if k != KEY} == secret["data"]
                    and self.configmap_versions() == configmaps, "Secret rollout attribution changed")
            self.http_messages(1)
            return {"gap": probes.SENSITIVE, "passed": True, "secretUid": secret["metadata"]["uid"], "before": before, "after": after, "existingSecretDataUnchanged": True, "configMapsUnchanged": configmaps}
        finally:
            if added:
                current = json.loads(self.k("get", "secret", "database-app", "-n", self.workload, "-o", "json"))
                require(current["metadata"]["uid"] == secret["metadata"]["uid"] and {k: v for k, v in current["data"].items() if k != KEY} == secret["data"],
                        "Secret cleanup identity changed")
                self.k("patch", "secret", "database-app", "-n", self.workload, "--type=json", "-p", json.dumps(secret_patch(current, value, True)), "-o", "name")
                restored = json.loads(self.k("get", "secret", "database-app", "-n", self.workload, "-o", "json"))
                require(restored["metadata"]["uid"] == secret["metadata"]["uid"] and KEY not in restored["data"]
                        and restored["data"] == secret["data"], "Secret probe key cleanup failed")

    def measure(self):
        self.checkpoint("cnpg-lifecycle")
        super().measure()
        self.checkpoint("identity-pin")
        self.successor = next(step for step in self.steps if step["outcome"] == "successor-prepared")
        before = self.public_binding()
        self.checkpoint("projected-token")
        token, credential = self.credential(before)
        config = json.loads(self.k("config", "view", "--raw", "--minify", "-o", "json"))
        server, context = tls_endpoint(config)
        inventory = json.loads(self.k("get", "configmap", "reakt-inventory-" + self.domain, "-n", self.control, "-o", "json"))
        spec = probes.plan_probes(self.successor_plan, {("", "configmaps", self.control, "reakt-inventory-" + self.domain): inventory})
        require(not spec["unsupportedGaps"] and not spec["undeclaredGaps"] and not spec["unsupportedOperations"]
                and spec["outstandingObservations"] == [probes.SENSITIVE], "API gap probe plan is incomplete")
        transport = probes.https_status_transport(server, token, context)
        self.checkpoint("http-probes")
        statuses = []

        def record_status(method, path, query, body, content_type):
            status = transport(method, path, query, body, content_type)
            statuses.append({"method": method, "path": path, "query": query, "contentType": content_type, "status": status})
            (self.output / "http-probes.json").write_bytes(shared.canonical({"requests": statuses, "complete": False}))
            return status

        try:
            result = probes.execute_probes(spec, record_status, {key: before[key] for key in ["releaseDigest", "generation", "serviceAccountUid"]})
        finally:
            del transport, token
        (self.output / "http-probes.json").write_bytes(shared.canonical(result))
        require(result["httpChecksPassed"] and self.public_binding() == before, "API credential probes failed or identity changed")
        self.checkpoint("sensitive-revision")
        sensitive = self.secret_observation()
        sensitive["syntheticKeyRemoved"] = True
        require(self.public_binding() == before, "API probe identity changed during Secret observation")
        result["outstandingObservations"] = []
        self.api_observation = {"binding": before, "credential": credential, "http": result, "sensitiveRevision": sensitive}
        self.checks.append({"check": "api-bound-gap-conformance", "passed": True})
        self.checkpoint("complete")

    def publish(self):
        require(self.disposition == "deleted" and self.cleanup_confirmed and not self.admin.exists()
                and not self.issuer.exists() and hasattr(self, "api_observation"), "API evidence requires completed cleanup")
        self.verify_docker()
        self.require_inputs_unchanged()
        require(self.step("verify", "verified-pair") == self.pair, "API evidence pair changed")
        manifest = self.successor_manifest
        source = Path(__file__).resolve()
        measurement = {"schemaVersion": "apsides.dev/api-gap-measurement/v1", "candidatePair": self.pair["candidates"],
                       "runnerSourceCommit": self.commit, "frozenInputDigests": self.snapshot, "steps": self.steps,
                       "observation": self.api_observation, "clusterDeleted": True, "credentialsRemoved": True,
                       "releaseVerified": False, "limitation": "local generation Pod-bound API credential; actual projected proxy token; no GitHub release trust"}
        names = ["api-gap-measurement.json", "api-gap-runner.py", "api-gap-probes.py", "api-gap-source.tar", "api-gap-operator.yaml", "api-gap-cleanup.json"]
        receipt = {"schemaVersion": shared.SCHEMA, **{key: manifest[key] for key in ["irDigest", "compilerDigest", "binaryDigest", "image"]},
                   "sourceInputsDigest": manifest["source"]["inputsDigest"],
                   "payloadFiles": {name: manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "reakt-controller"]},
                   "runnerSourceFile": "evidence/api-gap-runner.py", "runnerSourceDigest": self.source_digests[source],
                   "checks": {"api-bound-gap-conformance": {"status": "passed", "assertions": [
                       {"checkId": key, "expected": value, "actual": value, "passed": True}
                       for key, value in [("actual-credential-tests-passed", True), ("undisclosed-gap", False)]],
                       "evidenceFiles": ["evidence/" + name for name in names]}}}
        files = {names[0]: shared.canonical(measurement), names[1]: source.read_bytes(), names[2]: Path(probes.__file__).read_bytes(),
                 names[3]: self.source_archive.read_bytes(), names[4]: self.operator_bytes, names[5]: (self.output / "cleanup.json").read_bytes(),
                 "api-gap-evidence.json": shared.canonical(receipt)}
        require(all(0 < len(raw) <= shared.MAX_EVIDENCE_BYTES for raw in files.values()), "API evidence exceeds size bound")
        self.require_inputs_unchanged()
        destination = self.output / "evidence"
        destination.mkdir()
        for name, raw in files.items():
            with (destination / name).open("xb") as stream:
                stream.write(raw)
        return destination / "api-gap-evidence.json"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["initial-artifact", "successor-artifact", "trust-policy", "operator-manifest", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--operator-sha256", required=True)
    runner = None
    try:
        runner = ApiGapRunner(parser.parse_args())
        runner.run()
    except Exception as error:
        if runner is not None:
            (runner.output / "result.json").write_bytes(shared.canonical({"passed": False, "releaseVerified": False,
                "phase": runner.phase, "clusterDisposition": runner.disposition,
                "failureReason": demo.base.failure_reason(runner, error), "primaryFailure": runner.primary_failure}))
        print("API gap qualification failed; no credential or response bodies reported.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
