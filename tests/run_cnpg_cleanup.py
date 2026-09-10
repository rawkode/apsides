#!/usr/bin/env python3
"""Measure exact signed CNPG cleanup on a fresh owned kind cluster, without a release receipt."""
import argparse
import json
from pathlib import Path
import re
import sys
import time

import run_cnpg_demo as demo

shared = demo.shared
TARGET = {"apiGroup": "postgresql.cnpg.io", "resource": "backups",
          "namespace": "default", "name": "cleanup-backup"}
RESOURCE = "backups.postgresql.cnpg.io"
FINALIZER = "qualification.apsides.dev/cleanup-hold"
CONTROL_NAME = "cleanup-unclaimed-control"


def result_record(output, expected):
    # The rejection outcome has the same closed metadata schema as lifecycle results.
    records = [line.removeprefix(demo.PREFIX) for line in output.splitlines()
               if line.startswith(demo.PREFIX)]
    if len(records) != 1 or len(records[0].encode()) > 128 * 1024:
        raise shared.QualificationError("cleanup driver did not emit one bounded result")
    value = json.loads(records[0], object_pairs_hook=shared.unique_object)
    outcomes = {expected} if isinstance(expected, str) else set(expected)
    if isinstance(value, dict) and value.get("outcome") == "cleanup-uid-rejected":
        if "cleanup-uid-rejected" not in outcomes:
            raise shared.QualificationError("cleanup rejection was not expected")
        translated = dict(value, outcome="upgrade-commit-pending")
        checked = demo.result_record(demo.PREFIX + json.dumps(translated), "upgrade-commit-pending")
        if checked["phase"] != "Cleaning":
            raise shared.QualificationError("cleanup rejection changed the transition phase")
        return value
    return demo.result_record(output, expected)


def cleanup_policy(initial, successor):
    resources = initial["ir"]["resources"]
    selected = [item for item in resources if demo.managed_target(item) == TARGET]
    retained = [item for item in resources if demo.managed_target(item) != TARGET]
    expected = {("Cluster", "database"), ("ConfigMap", "api-config"),
                ("Deployment", "database-api"), ("Service", "database-api")}
    if (len(selected) != 1 or selected[0].get("lifecycle", {}).get("deletionPolicy") != "delete"
            or len(retained) != 4 or len(successor["ir"]["resources"]) != 4
            or {(item["kind"], item["name"]) for item in retained} != expected
            or [demo.resource_projection(item) for item in retained]
            != [demo.resource_projection(item) for item in successor["ir"]["resources"]]
            or any(item["apiGroup"] == TARGET["apiGroup"] and item["resource"] == TARGET["resource"]
                   for item in successor["capabilities"])):
        raise shared.QualificationError("cleanup pair is not the bounded CNPG last-type removal")
    runtime = demo.base.ceiling(initial)
    capabilities = [*runtime["capabilities"], *[{
        "apiGroup": TARGET["apiGroup"], "resource": TARGET["resource"], "subresource": None,
        "namespace": TARGET["namespace"], "verb": verb, "names": [TARGET["name"]],
    } for verb in ["get", "delete"]]]
    unique = {shared.canonical(item): item for item in capabilities}
    # Capability derives Ord in Rust in this exact field order (Option: None before Some).
    ordered = sorted(unique.values(), key=lambda item: (
        item["namespace"], item["apiGroup"], item["resource"],
        (item["subresource"] is not None, item["subresource"] or ""), item["verb"], item["names"],
    ))
    cleanup = dict(runtime, capabilities=ordered)
    return {"schemaVersion": "apsides.dev/platform/v1", "runtimeCeiling": runtime,
            "cleanupCeiling": cleanup}


def finalizer_patch(resource, add):
    metadata = resource["metadata"]
    uid, version = metadata.get("uid"), metadata.get("resourceVersion")
    finalizers = metadata.get("finalizers", [])
    if (not isinstance(uid, str) or not uid or not isinstance(version, str) or not version
            or not isinstance(finalizers, list) or any(not isinstance(item, str) for item in finalizers)
            or (FINALIZER in finalizers) == add):
        raise shared.QualificationError("cleanup finalizer mutation lacks exact object identity")
    changed = [*finalizers, FINALIZER] if add else [item for item in finalizers if item != FINALIZER]
    return [{"op": "test", "path": "/metadata/uid", "value": uid},
            {"op": "test", "path": "/metadata/resourceVersion", "value": version},
            {"op": "add", "path": "/metadata/finalizers", "value": changed}]


def object_identity(resource):
    metadata = resource["metadata"]
    content = {key: value for key, value in resource.items() if key != "status"}
    content["metadata"] = {key: value for key, value in metadata.items()
                           if key not in {"resourceVersion", "managedFields"}}
    return {"uid": metadata["uid"], "resourceVersion": metadata["resourceVersion"],
            "contentDigest": shared.digest_bytes(shared.canonical(content)),
            "deletionTimestamp": metadata.get("deletionTimestamp"),
            "finalizers": metadata.get("finalizers", []),
            "specDigest": shared.digest_bytes(shared.canonical(resource.get("spec", {})))}


def runtime_roles_without_backups(bindings, roles, account, control, workload):
    names = []
    by_name = {role["metadata"]["name"]: role for role in roles}
    for binding in bindings:
        if not any(subject.get("kind") == "ServiceAccount" and subject.get("name") == account
                   and subject.get("namespace") == control for subject in binding.get("subjects", [])):
            continue
        ref = binding.get("roleRef", {})
        if (ref.get("kind") != "Role" or ref.get("apiGroup") != "rbac.authorization.k8s.io"
                or binding["metadata"].get("namespace") != workload or ref.get("name") not in by_name):
            raise shared.QualificationError("successor runtime binding has an unexpected role reference")
        role = by_name[ref["name"]]
        if role["metadata"].get("namespace") != workload:
            raise shared.QualificationError("successor runtime Role escaped the workload namespace")
        for rule in role.get("rules", []):
            if (set(rule.get("apiGroups", [])) & {"*", TARGET["apiGroup"]}
                    and any(resource in {"*", "backups", "backups/*"}
                            or resource.startswith(("backups/", "*/"))
                            for resource in rule.get("resources", []))):
                raise shared.QualificationError("successor runtime Role still grants Backup access")
        names.append({"name": ref["name"], "uid": role["metadata"]["uid"],
                      "rulesDigest": shared.digest_bytes(shared.canonical(role.get("rules", [])))})
    if not names:
        raise shared.QualificationError("successor runtime Role binding is absent")
    return sorted(names, key=lambda item: item["name"])


class CnpgCleanupRunner(demo.CnpgDemoRunner):
    purpose = {"kind": "last-type-cleanup", "identity": TARGET}
    parse_result = staticmethod(result_record)

    def __init__(self, args):
        args.retain_on_success = False
        self.scenario = args.scenario
        super().__init__(args)
        source = Path(__file__).resolve()
        if shared.source_state(source) != (self.repository, self.commit):
            raise shared.QualificationError("cleanup runner differs from the frozen producer source")
        self.sources.append(source)
        self.source_archives[source] = "cnpg-cleanup-runner.py"
        self.source_digests[source] = shared.digest_file(source)
        self.freeze_file("producer/cnpg-cleanup-runner.py", source)
        self.failure_reasons |= set(re.findall(
            r'QualificationError\(\s*"([^"\n]+)"', source.read_text()))

    def configure_pair(self):
        self.initial_policy = cleanup_policy(self.initial_plan, self.successor_plan)
        self.successor_policy = self.initial_policy
        self.initial_policy_digest = shared.digest_bytes(shared.canonical(self.initial_policy))
        self.successor_policy_digest = self.initial_policy_digest

    def permissions(self):
        super().permissions()
        reads = [self._rule(target["apiGroup"], [target["resource"]], ["get"], [target["name"]])
                 for target in map(demo.managed_target, self.initial_plan["ir"]["resources"])]
        self._role(self.workload, "cleanup-inventory-capture", reads, "ceiling-deployer")
        self._role(self.workload, "cleanup-target", [self._rule(
            TARGET["apiGroup"], [TARGET["resource"]], ["get", "delete"], [TARGET["name"]],
        )], "ceiling-deployer")

    def get_backup(self, name=TARGET["name"]):
        output = self.k("get", RESOURCE, name, "-n", self.workload,
                        "--ignore-not-found", "--show-managed-fields", "-o", "json")
        return json.loads(output) if output.strip() else None

    def change_finalizer(self, current, add):
        patch = finalizer_patch(current, add)
        return json.loads(self.k("patch", RESOURCE, TARGET["name"], "-n", self.workload,
                                "--type=json", "-p", json.dumps(patch), "-o", "json"))

    def reserve(self, role, plan):
        prepared = self.step("prepare-" + role, role + "-prepared", self.issuer)
        self.prepared_projection(prepared, plan)
        reserved = self.step("reserve-" + role, role + "-reserved", self.issuer,
                             expectedRequestDigest=prepared["requestDigest"])
        if any(reserved[key] != prepared[key] for key in [
                "transition", "generation", "requestDigest", "registryUid"]):
            raise shared.QualificationError("cleanup reservation differs from the prepared request")
        return prepared

    def measure(self):
        self.k("wait", "--for=condition=Established", "crd/backups.postgresql.cnpg.io", "--timeout=120s")
        self.create({"apiVersion": "v1", "kind": "ConfigMap",
                     "metadata": {"name": "api-settings", "namespace": self.workload},
                     "data": {"PGRST_DB_MAX_ROWS": "100"}})
        initial = self.reserve("initial", self.initial_plan)
        self.lifecycle("stage-initial", initial["transition"])
        self.lifecycle("activate-initial", initial["transition"])
        self.wait_cnpg()
        self.workload_identity()
        self.wait_object(RESOURCE, TARGET["name"], self.workload, 180)
        # Publishing authenticates initial inventory and captures the managed UID before quiescence.
        self.step("publish-initial", "initial-inventory-published", self.issuer,
                  transition=initial["transition"])
        current = self.get_backup()
        if current is None or current["metadata"].get("deletionTimestamp"):
            raise shared.QualificationError("initial cleanup Backup is absent or terminating")
        self.original = object_identity(current)
        control = self.create({"apiVersion": "postgresql.cnpg.io/v1", "kind": "Backup",
                               "metadata": {"name": CONTROL_NAME, "namespace": self.workload},
                               "spec": {"cluster": {"name": "cleanup-unconfigured-database"}}})
        self.control_identity = object_identity(control)
        successor = self.reserve("successor", self.successor_plan)
        transition = successor["transition"]
        self.lifecycle("stage-successor", transition)
        self.lifecycle("quiesce-upgrade", transition)
        self.lifecycle("activate-upgrade", transition)
        if self.scenario == "finalizer":
            self.finalizer_scenario(transition)
            self.step("publish-successor", "successor-inventory-published", self.issuer, transition=transition)
            resolved = self.step("resolve-successor", "successor-resolved", self.issuer, transition=transition)
            if resolved["phase"] != "Committed":
                raise shared.QualificationError("cleanup successor did not commit")
        else:
            self.replacement_scenario(transition)
        surviving = self.get_backup(CONTROL_NAME)
        if (surviving is None or surviving["metadata"].get("uid") != self.control_identity["uid"]
                or surviving["metadata"].get("deletionTimestamp")
                or object_identity(surviving)["specDigest"] != self.control_identity["specDigest"]):
            raise shared.QualificationError("cleanup affected the unclaimed same-type control resource")
        roles = json.loads(self.k("get", "roles", "-n", self.workload, "-o", "json"))["items"]
        bindings = json.loads(self.k("get", "rolebindings", "-n", self.workload, "-o", "json"))["items"]
        self.runtime_roles = runtime_roles_without_backups(
            bindings, roles, successor["serviceAccountName"], self.control, self.workload)
        self.checks.append({"check": "unclaimed-same-type-resource-preserved", "passed": True,
                            "identity": TARGET | {"name": CONTROL_NAME}, "uid": self.control_identity["uid"]})
        self.require_inputs_unchanged()

    def finalizer_scenario(self, transition):
        current = self.get_backup()
        if current is None or current["metadata"].get("uid") != self.original["uid"]:
            raise shared.QualificationError("cleanup target changed before finalizer insertion")
        held = self.change_finalizer(current, True)
        pending = self.step("commit-upgrade", "upgrade-commit-pending", self.issuer, transition=transition)
        deleting = self.get_backup()
        if (pending["phase"] != "Cleaning" or pending["transition"] != transition
                or deleting is None or deleting["metadata"].get("uid") != self.original["uid"]
                or not deleting["metadata"].get("deletionTimestamp")
                or deleting["metadata"].get("finalizers") != held["metadata"]["finalizers"]):
            raise shared.QualificationError("cleanup did not preserve the finalizer on the pinned UID")
        self.change_finalizer(deleting, False)
        self.lifecycle("commit-upgrade", transition)
        if self.get_backup() is not None:
            raise shared.QualificationError("cleanup committed before the selected Backup disappeared")
        self.checks.append({"check": "finalizer-pending-then-last-type-cleanup", "passed": True,
                            "originalUid": self.original["uid"], "finalizer": FINALIZER,
                            "pending": pending})

    def replacement_scenario(self, transition):
        current = self.get_backup()
        if (current is None or current["metadata"].get("uid") != self.original["uid"]
                or current["metadata"].get("finalizers")):
            raise shared.QualificationError("replacement fixture cannot delete a changed or finalized target")
        metadata = current["metadata"]
        self.k("delete", "--raw", "/apis/postgresql.cnpg.io/v1/namespaces/" + self.workload
               + "/backups/" + TARGET["name"], "-f", "-", data=json.dumps({
                   "apiVersion": "v1", "kind": "DeleteOptions",
                   "preconditions": {"uid": metadata["uid"], "resourceVersion": metadata["resourceVersion"]},
               }))
        deadline = time.monotonic() + 60
        while self.get_backup() is not None:
            if time.monotonic() >= deadline:
                raise shared.QualificationError("replacement fixture original Backup deletion remained pending")
            time.sleep(1)
        resource = next(item for item in self.initial_plan["ir"]["resources"]
                        if demo.managed_target(item) == TARGET)
        created = json.loads(self.k("apply", "--server-side", "--field-manager=apsides-" + self.domain,
                                    "-f", "-", "-o", "json", data=json.dumps(resource["desired"])))
        replacement = object_identity(created)
        if replacement["uid"] == self.original["uid"] or replacement["deletionTimestamp"]:
            raise shared.QualificationError("replacement fixture did not create a new live UID")
        rejected = self.step("commit-upgrade", "cleanup-uid-rejected", self.issuer, transition=transition)
        after = self.get_backup()
        if (rejected["transition"] != transition or after is None
                or object_identity(after)["uid"] != replacement["uid"]
                or object_identity(after)["deletionTimestamp"]
                or object_identity(after)["contentDigest"] != replacement["contentDigest"]
                or object_identity(after)["finalizers"] != replacement["finalizers"]):
            raise shared.QualificationError("cleanup rejection did not preserve the replacement Backup")
        self.checks.append({"check": "replacement-uid-rejected-with-original-claim-retained", "passed": True,
                            "originalUid": self.original["uid"], "replacementUid": replacement["uid"],
                            "rejection": rejected})

    def publish(self):
        if (self.disposition != "deleted" or not self.cleanup_confirmed
                or self.admin.exists() or self.issuer.exists()):
            raise shared.QualificationError("cleanup measurement requires cluster and credential cleanup")
        self.verify_docker()
        self.require_inputs_unchanged()
        if self.step("verify", "verified-pair") != self.pair:
            raise shared.QualificationError("cleanup measurement candidate pair changed")
        self.require_inputs_unchanged()
        measurement = {"schemaVersion": "apsides.dev/cnpg-cleanup-measurement/v1", "passed": True,
                       "scenario": self.scenario, "candidatePair": self.pair["candidates"],
                       "purpose": self.purpose, "runnerSourceCommit": self.commit,
                       "frozenInputDigests": self.snapshot, "policyDigest": self.initial_policy_digest,
                       "checks": self.checks, "runtimeRoles": self.runtime_roles, "steps": self.steps,
                       "clusterDeleted": True, "credentialsRemoved": True, "releaseVerified": False,
                       "productionEvidenceStatus": "not-produced",
                       "limitation": "live measurement only; no production cleanup receipt contract"}
        (self.output / "measurement.json").write_bytes(shared.canonical(measurement))
        return measurement

    def run(self):
        passed = False
        try:
            self.prepare()
            self.phase = "cluster-create"
            self.create_cluster()
            self.phase = "bootstrap"
            self.bootstrap()
            self.phase = "cleanup-lifecycle"
            self.measure()
            passed = True
        except Exception as error:
            self.primary_failure = {"phase": self.phase, "reason": demo.base.failure_reason(self, error)}
            raise
        finally:
            self.phase = "owned-cluster-cleanup"
            self.finish(passed)
        self.phase = "measurement"
        result = self.publish()
        (self.output / "result.json").write_bytes(shared.canonical(result))
        print(json.dumps({"passed": True, "measurement": str(self.output / "measurement.json"),
                          "releaseVerified": False, "productionEvidenceStatus": "not-produced"}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["initial-artifact", "successor-artifact", "trust-policy", "operator-manifest", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--operator-sha256", required=True)
    parser.add_argument("--scenario", choices=["finalizer", "replacement"], required=True)
    runner = None
    try:
        runner = CnpgCleanupRunner(parser.parse_args())
        runner.run()
    except Exception as error:
        if runner is not None:
            (runner.output / "result.json").write_bytes(shared.canonical({
                "passed": False, "releaseVerified": False, "productionEvidenceStatus": "not-produced",
                "phase": runner.phase, "clusterDisposition": runner.disposition,
                "failureReason": demo.base.failure_reason(runner, error),
                "primaryFailure": runner.primary_failure,
            }))
        print("CNPG cleanup failed; no release acceptance is claimed.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
