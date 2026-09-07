#!/usr/bin/env python3
"""Qualify refusal of activation after retained predecessor Pod evidence is lost."""
import argparse
import ast
import json
from pathlib import Path
import sys
import time

import run_cnpg_demo as demo

shared = demo.shared
FINALIZER = "reakt.dev/termination-evidence"
OUTCOME = "uncertain-writer-rejected"
STAGES = frozenset({"initial-start", "initial-inventory", "initial-writer-pin", "successor-reserve",
                    "successor-stage", "quiesce", "baseline-protected-state", "protected-history-pin",
                    "successor-stopped", "fault-removal", "after-fault-protected-state",
                    "activation-refusal", "after-refusal-protected-state"})


def require(condition, message):
    if not condition:
        raise shared.QualificationError(message)


def static_failure_reasons(source):
    return {node.args[1].value for node in ast.walk(ast.parse(source))
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "require"
            and len(node.args) == 2 and isinstance(node.args[1], ast.Constant) and isinstance(node.args[1].value, str)}


def result_record(output, expected):
    records = [line.removeprefix(demo.PREFIX) for line in output.splitlines() if line.startswith(demo.PREFIX)]
    require(len(records) == 1 and len(records[0].encode()) <= 128 * 1024,
            "uncertain writer driver lacks one bounded result")
    value = json.loads(records[0], object_pairs_hook=shared.unique_object)
    if isinstance(value, dict) and value.get("outcome") == OUTCOME:
        require(OUTCOME in ({expected} if isinstance(expected, str) else set(expected)), "unexpected writer refusal")
        translated = dict(value, outcome="upgrade-quiesced")
        demo.result_record(demo.PREFIX + json.dumps(translated), "upgrade-quiesced")
        require(value["phase"] == "Quiesced", "writer refusal changed phase")
        return value
    return demo.result_record(output, expected)


def removal_patch(pod, expected, namespace):
    metadata = pod.get("metadata", {})
    status = pod.get("status", {})
    containers = status.get("containerStatuses", [])
    require(metadata.get("name") == expected["name"] and metadata.get("uid") == expected["uid"]
            and metadata.get("namespace") == namespace and metadata.get("resourceVersion")
            and pod.get("spec", {}).get("nodeName") == expected["nodeName"]
            and pod.get("spec", {}).get("serviceAccountName") == expected["serviceAccountName"]
            and any(owner.get("kind") == "ReplicaSet" and owner.get("controller") is True
                    and owner.get("uid") == expected["replicaSetUid"] for owner in metadata.get("ownerReferences", []))
            and metadata.get("finalizers", []).count(FINALIZER) == 1
            and status.get("phase") in {"Succeeded", "Failed"} and containers
            and all(item.get("state", {}).get("terminated") for item in containers),
            "fault requires exact retained terminal predecessor Pod")
    return [{"op": "test", "path": "/metadata/uid", "value": metadata["uid"]},
            {"op": "test", "path": "/metadata/resourceVersion", "value": metadata["resourceVersion"]},
            {"op": "replace", "path": "/metadata/finalizers",
             "value": [item for item in metadata["finalizers"] if item != FINALIZER]}]


def pinned_history(registry, transition, initial):
    stored = registry["transitions"][transition]
    writers = stored["upgradeDrain"]["runtimeWriters"]
    require(stored["record"]["phase"] == "Quiesced"
            and writers["deploymentName"] == initial["name"]
            and writers["deploymentUid"] == initial["uid"]
            and writers["serviceAccountName"] == initial["processServiceAccountName"]
            and len(writers["pods"]) == len(initial["pods"]) == 1,
            "protected writer history does not match the initial allocation")
    expected = writers["pods"][0]
    require(all(expected[key] == initial["pods"][0][key] for key in ["name", "uid", "nodeName"]),
            "protected predecessor Pod identity changed")
    return dict(expected, serviceAccountName=writers["serviceAccountName"],
                replicaSetUid=initial["pods"][0]["replicaSetUid"])


def stopped_successor(deployment, pods, account, runtime_uid, proxy, proxy_deployment):
    metadata = deployment.get("metadata", {})
    process_account = deployment.get("spec", {}).get("template", {}).get("spec", {}).get("serviceAccountName")
    require(metadata.get("name") == account and runtime_uid and metadata.get("uid") == runtime_uid
            and not metadata.get("deletionTimestamp") and deployment.get("spec", {}).get("replicas") == 0
            and process_account == account + "-process", "successor runtime Deployment differs from stopped staged identity")
    require(not any(pod.get("spec", {}).get("serviceAccountName") == process_account for pod in pods),
            "uncertain writer experiment started a successor runtime Pod")
    proxy_metadata = proxy_deployment.get("metadata", {})
    require(proxy_metadata.get("name") == proxy["deploymentName"]
            and proxy_metadata.get("uid") == proxy["deploymentUid"] and not proxy_metadata.get("deletionTimestamp"),
            "successor proxy Deployment differs from protected identity")
    base_pods = [pod for pod in pods if pod.get("spec", {}).get("serviceAccountName") == account]
    require(len(base_pods) == 1 and base_pods[0].get("metadata", {}).get("name") == proxy["podName"]
            and base_pods[0].get("metadata", {}).get("uid") == proxy["podUid"]
            and not base_pods[0]["metadata"].get("deletionTimestamp"),
            "successor base account Pod differs from protected staged proxy")
    return {"name": account, "uid": metadata["uid"], "processServiceAccountName": process_account,
            "replicas": 0, "runtimePodUids": [], "proxyDeploymentUid": proxy["deploymentUid"], "proxyPodUid": proxy["podUid"]}


def draining_activation(activation, registry, initial, successor, domain):
    previous = registry["transitions"][successor["transition"]]["previousWriterCheckpoint"]
    binding = activation.get("binding", {})
    require(activation.get("status") == "draining" and binding == previous["runtimeBinding"]
            and binding.get("domainId") == domain and binding.get("generationId") == initial["generation"]
            and binding.get("serviceAccountName") == initial["serviceAccountName"]
            and binding["generationId"] != successor["generation"],
            "activation is not the protected draining predecessor")


class UncertainWriterRunner(demo.CnpgDemoRunner):
    parse_result = staticmethod(result_record)

    def __init__(self, args):
        args.retain_on_success = False
        super().__init__(args)
        source = Path(__file__).resolve()
        self.failure_reasons |= static_failure_reasons(source.read_text())
        require(shared.source_state(source) == (self.repository, self.commit), "uncertain writer source is not frozen")
        self.sources.append(source)
        self.source_archives[source] = "uncertain-writer-runner.py"
        self.source_digests[source] = shared.digest_file(source)
        self.freeze_file("producer/uncertain-writer-runner.py", source)

    def checkpoint(self, stage):
        require(stage in STAGES, "uncertain writer diagnostic stage is invalid")
        self.phase = "uncertain-writer-" + stage
        (self.output / "progress.json").write_bytes(shared.canonical({"stage": stage}))

    def reserve(self, role, plan):
        prepared = self.step("prepare-" + role, role + "-prepared", self.issuer)
        self.prepared_projection(prepared, plan)
        reserved = self.step("reserve-" + role, role + "-reserved", self.issuer,
                             expectedRequestDigest=prepared["requestDigest"])
        require(all(reserved[key] == prepared[key] for key in ["transition", "generation", "requestDigest", "registryUid"]),
                "writer reservation differs from prepared allocation")
        return prepared

    def protected_state(self):
        objects = {}
        registry = None
        activation = None
        for name in ["reakt-registry-" + self.workload, "reakt-activation-" + self.domain, "reakt-inventory-" + self.domain]:
            raw = self.k("get", "configmap", name, "-n", self.control, "--ignore-not-found", "-o", "json")
            value = json.loads(raw) if raw.strip() else None
            require(value is not None, "protected control object is missing")
            objects[name] = shared.digest_bytes(shared.canonical(value))
            if name == "reakt-registry-" + self.workload:
                registry = json.loads(value["data"]["state.json"], object_pairs_hook=shared.unique_object)
            elif name == "reakt-activation-" + self.domain:
                activation = json.loads(value["data"]["state.json"], object_pairs_hook=shared.unique_object)
        draining_activation(activation, registry, self.initial_prepared, self.successor_prepared, self.domain)
        coordination, _ = demo.base.observations.capture(self.coordination)
        inventory, _ = demo.base.observations.capture(self.inventory)
        return {"objects": objects, "coordination": coordination, "inventory": inventory}, registry

    def successor_state(self, prepared, registry):
        stored = registry["transitions"][prepared["transition"]]
        proxy = stored["proxyIdentity"]
        runtime_uid = stored["stagedIdentityUids"]["apps/v1/Deployment/" + self.control + "/" + prepared["serviceAccountName"]]
        deployment = json.loads(self.k("get", "deployment", prepared["serviceAccountName"], "-n", self.control, "-o", "json"))
        proxy_deployment = json.loads(self.k("get", "deployment", proxy["deploymentName"], "-n", self.control, "-o", "json"))
        pods = json.loads(self.k("get", "pods", "-n", self.control, "-o", "json"))["items"]
        return stopped_successor(deployment, pods, prepared["serviceAccountName"], runtime_uid, proxy, proxy_deployment)

    def get_pod(self, name):
        raw = self.k("get", "pod", name, "-n", self.control, "--ignore-not-found", "-o", "json")
        return json.loads(raw) if raw.strip() else None

    def lose_pod_record(self, expected):
        # Fault injection is administrative and restricted to this runner's disposable cluster.
        self.verify_docker()
        containers = json.loads(self.command(["docker", "inspect", self.cluster + "-control-plane"]))
        require(len(containers) == 1 and containers[0].get("Id") == self.container_id
                and containers[0].get("Config", {}).get("Labels", {}).get("io.x-k8s.kind.cluster") == self.cluster,
                "writer fault target is not the owned cluster")
        pod = self.get_pod(expected["name"])
        require(pod is not None, "writer Pod record was already absent")
        patch = removal_patch(pod, expected, self.control)
        if not pod["metadata"].get("deletionTimestamp"):
            self.k("delete", "--raw", "/api/v1/namespaces/" + self.control + "/pods/" + expected["name"], "-f", "-",
                   data=json.dumps({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {
                       "uid": expected["uid"], "resourceVersion": pod["metadata"]["resourceVersion"]}}))
            pod = self.get_pod(expected["name"])
            require(pod is not None, "protected Pod vanished before finalizer removal")
            patch = removal_patch(pod, expected, self.control)
        self.k("patch", "pod", expected["name"], "-n", self.control, "--type=json", "-p", json.dumps(patch), "-o", "name")
        deadline = time.monotonic() + 60
        while self.get_pod(expected["name"]) is not None:
            require(time.monotonic() < deadline, "writer Pod removal remained pending; other finalizers preserved")
            time.sleep(1)
        return {"name": expected["name"], "uid": expected["uid"], "nodeName": expected["nodeName"],
                "resourceVersion": pod["metadata"]["resourceVersion"], "removedFinalizer": FINALIZER, "absent": True}

    def measure(self):
        self.checkpoint("initial-start")
        initial = self.reserve("initial", self.initial_plan)
        self.initial_prepared = initial
        self.lifecycle("stage-initial", initial["transition"])
        self.lifecycle("activate-initial", initial["transition"])
        self.wait_cnpg()
        self.checkpoint("initial-inventory")
        self.step("publish-initial", "initial-inventory-published", self.issuer, transition=initial["transition"])
        self.checkpoint("initial-writer-pin")
        writers = self.writer_identity(initial)
        runtime = [writer for writer in writers if writer["name"] == initial["serviceAccountName"]]
        require(len(runtime) == 1, "initial runtime writer is ambiguous")
        deployment = json.loads(self.k("get", "deployment", runtime[0]["name"], "-n", self.control, "-o", "json"))
        process_account = deployment["spec"]["template"]["spec"]["serviceAccountName"]
        require(deployment["metadata"]["uid"] == runtime[0]["uid"]
                and process_account == initial["serviceAccountName"] + "-process", "initial process identity changed")
        runtime[0]["processServiceAccountName"] = process_account
        self.create({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {
            "name": "api-settings", "namespace": self.workload}, "data": {"PGRST_DB_MAX_ROWS": "100"}})
        self.policy_change = self.replace_policy()
        self.checkpoint("successor-reserve")
        successor = self.reserve("successor", self.successor_plan)
        self.successor_prepared = successor
        transition = successor["transition"]
        self.checkpoint("successor-stage")
        self.lifecycle("stage-successor", transition)
        self.checkpoint("quiesce")
        self.lifecycle("quiesce-upgrade", transition)
        self.checkpoint("baseline-protected-state")
        before, registry = self.protected_state()
        self.checkpoint("protected-history-pin")
        expected = pinned_history(registry, transition, runtime[0])
        self.checkpoint("successor-stopped")
        stopped = self.successor_state(successor, registry)
        self.checkpoint("fault-removal")
        fault = self.lose_pod_record(expected)
        self.checkpoint("after-fault-protected-state")
        injected, registry = self.protected_state()
        require(before == injected and pinned_history(registry, transition, runtime[0]) == expected,
                "writer evidence fault changed protected state")
        require(self.successor_state(successor, registry) == stopped, "writer fault changed successor identity")
        self.checkpoint("activation-refusal")
        rejected = self.step("reject-uncertain-writer", OUTCOME, self.issuer, transition=transition)
        self.checkpoint("after-refusal-protected-state")
        after, registry = self.protected_state()
        require(before == after and pinned_history(registry, transition, runtime[0]) == expected
                and self.successor_state(successor, registry) == stopped and self.get_pod(expected["name"]) is None
                and rejected["transition"] == transition and rejected["generation"] == successor["generation"],
                "uncertain writer refusal changed protected state or started successor")
        self.observation = {"fault": fault, "before": before, "afterInjection": injected, "afterRefusal": after,
                            "successor": stopped, "rejection": rejected, "protectedPodHistoryRetained": True}
        self.checks.append({"check": "uncertain-writer-blocks", "passed": True, **self.observation})

    def final_check(self):
        require(self.disposition == "deleted" and self.cleanup_confirmed and not self.admin.exists()
                and not self.issuer.exists() and hasattr(self, "observation"), "writer evidence requires completed cleanup")
        self.verify_docker()
        self.require_inputs_unchanged()
        require(self.step("verify", "verified-pair") == self.pair, "writer evidence candidate pair changed")
        self.require_inputs_unchanged()

    def publish(self):
        self.final_check()
        manifest = self.successor_manifest
        source = Path(__file__).resolve()
        measurement = {"schemaVersion": "apsides.dev/uncertain-writer-measurement/v1", "passed": True,
                       "runnerSourceCommit": self.commit, "candidatePair": self.pair["candidates"],
                       "frozenInputDigests": self.snapshot, "steps": self.steps, "observation": self.observation,
                       "clusterDeleted": True, "credentialsRemoved": True, "releaseVerified": False,
                       "limitation": "retained predecessor Pod evidence lost after Quiesced; no network-partition claim"}
        evidence_names = ["uncertain-writer-measurement.json", "uncertain-writer-runner.py",
                          "uncertain-writer-source.tar", "uncertain-writer-operator.yaml", "uncertain-writer-cleanup.json"]
        receipt = {"schemaVersion": shared.SCHEMA, **{key: manifest[key] for key in ["irDigest", "compilerDigest", "binaryDigest", "image"]},
                   "sourceInputsDigest": manifest["source"]["inputsDigest"],
                   "payloadFiles": {name: manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "reakt-controller"]},
                   "runnerSourceFile": "evidence/uncertain-writer-runner.py", "runnerSourceDigest": self.source_digests[source],
                   "checks": {"uncertain-writer-blocks": {"status": "passed", "assertions": [
                       {"checkId": name, "expected": value, "actual": value, "passed": True}
                       for name, value in [("activation-blocked", True), ("unsafe-activation", False)]],
                       "evidenceFiles": ["evidence/" + name for name in evidence_names]}}}
        files = {evidence_names[0]: shared.canonical(measurement), evidence_names[1]: source.read_bytes(),
                 evidence_names[2]: self.source_archive.read_bytes(), evidence_names[3]: self.operator_bytes,
                 evidence_names[4]: (self.output / "cleanup.json").read_bytes(),
                 "uncertain-writer-evidence.json": shared.canonical(receipt)}
        require(all(0 < len(raw) <= shared.MAX_EVIDENCE_BYTES for raw in files.values()), "writer evidence exceeds size bound")
        self.require_inputs_unchanged()
        evidence = self.output / "evidence"
        evidence.mkdir()  # Exclusive creation; completion receipt is written last.
        for name, raw in files.items():
            with (evidence / name).open("xb") as output:
                output.write(raw)
        return evidence / "uncertain-writer-evidence.json"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["initial-artifact", "successor-artifact", "trust-policy", "operator-manifest", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--operator-sha256", required=True)
    runner = None
    try:
        runner = UncertainWriterRunner(parser.parse_args())
        runner.run()
    except Exception as error:
        if runner is not None:
            (runner.output / "result.json").write_bytes(shared.canonical({"passed": False, "releaseVerified": False,
                "phase": runner.phase, "failureReason": demo.base.failure_reason(runner, error),
                "primaryFailure": runner.primary_failure, "clusterDisposition": runner.disposition}))
        print("Uncertain writer qualification failed; no release acceptance is claimed.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
