#!/usr/bin/env python3
"""Measure exact-candidate claim collision on one runner-owned kind cluster."""
import json
import os
from pathlib import Path
import re
import shutil
import sys
from collections import Counter

import run_deployment_ceiling as base

shared = base.shared
ROOT = base.ROOT
PAIR = {"ownerReleaseDigest", "ownerIrDigest", "contenderReleaseDigest", "contenderIrDigest"}
REQUEST = {"generation", "transition", "registryUid", "registryResourceVersion", "releaseDigest", "requestDigest"}
FIELDS = {
    "verified-pair": PAIR | {"ownerDomainId", "contenderDomainId", "clusterBinding", "controlNamespace", "workloadNamespace", "overlapIdentityDigest", "overlapCount"},
    "bootstrapped": PAIR | {"registryUid", "registryResourceVersion"},
    "owner-prepared": PAIR | REQUEST | {"creationTargetsSource", "creationTargets"},
    "owner-planned": PAIR | REQUEST | {"phase", "nextSequence", "claimCount"},
    "contender-prepared": PAIR | REQUEST | {"creationTargetsSource", "creationTargets"},
    "collision-rejected": PAIR | REQUEST,
}
SCHEMA = "reakt.dev/ownership-collision-result/v1"


def result_record(output, expected):
    # The base parser enforces a closed, bounded metadata-only result contract.
    value = base.result_record(output, expected, prefix="APS_COLLISION_RESULT=", schema=SCHEMA, fields=FIELDS)
    if any(not shared.valid_digest(value[key]) for key in PAIR | ({"overlapIdentityDigest"} if expected == "verified-pair" else set())):
        raise shared.QualificationError("collision result has an invalid pair digest")
    if expected == "verified-pair" and (value["overlapCount"] != 1 or value["ownerDomainId"] == value["contenderDomainId"] or value["ownerReleaseDigest"] == value["contenderReleaseDigest"]):
        raise shared.QualificationError("collision result does not identify one distinct overlapping pair")
    role = "owner" if expected in {"owner-prepared", "owner-planned"} else "contender"
    if expected in {"owner-prepared", "owner-planned", "contender-prepared", "collision-rejected"} and value["releaseDigest"] != value[role + "ReleaseDigest"]:
        raise shared.QualificationError("collision result release differs from its candidate role")
    if expected == "owner-planned" and (value["phase"] != "Planned" or value["nextSequence"] != 2):
        raise shared.QualificationError("collision owner did not reach initial Planned")
    return value


def full_policy(plan):
    ceiling = base.ceiling(plan)
    return {"schemaVersion": "reakt.dev/platform/v1", "runtimeCeiling": ceiling, "cleanupCeiling": ceiling}


def reservation_delta(before, after, registry_key):
    base.unchanged_except(before["rawObjects"], after["rawObjects"], {registry_key})
    if before["rawObjects"][registry_key] == after["rawObjects"][registry_key] or before["rawInventory"] != after["rawInventory"]:
        raise shared.QualificationError("owner reservation did not change only expected state")
    base.observations.validate_reservation_rows(before["coordination"], after["coordination"])
    for table, rows in before["rawCoordination"].items():
        prior, current = Counter(rows), Counter(after["rawCoordination"][table])
        if table in {"reakt_coordination_claims", "reakt_coordination_review_objects"}:
            if (prior & current) != prior or sum((current - prior).values()) != 1:
                raise shared.QualificationError("owner reservation changed retained metadata rows")
        elif prior != current:
            raise shared.QualificationError("owner reservation changed unrelated metadata rows")


def marker_metadata(value):
    if value.get("immutable") is not True or value["metadata"].get("deletionTimestamp"):
        raise shared.QualificationError("collision marker is mutable or deleting")
    return {"uid": value["metadata"]["uid"], "resourceVersion": value["metadata"]["resourceVersion"],
            "contentDigest": shared.digest_bytes(shared.canonical(value["data"]))}


class CollisionRunner(base.CeilingRunner):
    def __init__(self, args):
        self.owner_artifact = args.owner_artifact
        self.owner_trust = args.owner_trust_policy
        for path in [self.owner_artifact, self.owner_trust]:
            if not path.is_absolute() or path.is_symlink():
                raise shared.QualificationError("owner inputs must be absolute nonsymlink paths")
        super().__init__(args)
        self.sources += [Path(__file__).resolve(), ROOT / "crates/platform/tests/ownership_collision.rs",
                         ROOT / "crates/platform/tests/support/ownership_collision_driver.rs"]
        self.source_digests = {path.name: shared.digest_file(path) for path in self.sources}
        if len(self.sources) != len(self.source_digests):
            raise shared.QualificationError("collision source filenames collide")
        self.archives = {path: "ownership-collision-" + path.name for path in self.sources}
        if len(set(self.archives.values())) != len(self.sources) or any(len(name) > 128 for name in self.archives.values()):
            raise shared.QualificationError("collision archive names are ambiguous or oversized")
        self.driver_error_reasons = set(re.findall(r'DriverError\(\s*"([a-zA-Z0-9 -]+)"', "\n".join(path.read_text() for path in self.sources if path.suffix == ".rs")))
        self.failure_reasons |= set(re.findall(r'(?:QualificationError|RuntimeError)\(\s*"([^"\n]+)"', Path(__file__).read_text()))

    def prepare(self):
        output = self.command(["cargo", "test", "--locked", "--offline", "-p", "platform", "--features", "test-support",
                               "--test", "ownership_collision", "--no-run", "--message-format=json"], timeout=300)
        executables = [row["executable"] for line in output.splitlines()
                       if (row := json.loads(line)).get("reason") == "compiler-artifact"
                       and row.get("target", {}).get("name") == "ownership_collision" and row.get("executable")]
        if len(executables) != 1:
            raise shared.QualificationError("Cargo did not identify one collision driver")
        self.driver = self.output / "collision-driver"
        shutil.copy2(executables[0], self.driver)
        self.driver_digest = shared.digest_file(self.driver)
        self.pair = self.step("verify", "verified-pair")
        self.control, self.workload, self.binding = (self.pair[key] for key in ["controlNamespace", "workloadNamespace", "clusterBinding"])
        self.owner_manifest = shared.read_json(self.owner_artifact / "release.json")
        self.manifest = shared.read_json(self.artifact / "release.json")
        self.owner_plan = shared.read_json(self.owner_artifact / "plan.json")
        self.contender_plan = shared.read_json(self.artifact / "plan.json")
        for role, artifact, manifest, plan in [("owner", self.owner_artifact, self.owner_manifest, self.owner_plan),
                                                ("contender", self.artifact, self.manifest, self.contender_plan)]:
            if (manifest["mode"] != "production" or plan["ir"]["testOnly"] is not False
                    or shared.digest_file(artifact / "release.json") != self.pair[role + "ReleaseDigest"]):
                raise shared.QualificationError("collision subject differs from verified production pair")
        self.domain = self.pair["ownerDomainId"]
        self.plan = self.owner_plan
        self.verified = {"releaseDigest": self.pair["ownerReleaseDigest"]}

    def crd_resources(self):
        return [*self.owner_plan["ir"]["resources"], *self.contender_plan["ir"]["resources"]]

    def step(self, action, expected, kubeconfig=None, **extra):
        settings = {"action": action, "ownerArtifact": str(self.owner_artifact), "ownerTrustPolicy": str(self.owner_trust),
                    "contenderArtifact": str(self.artifact), "contenderTrustPolicy": str(self.trust), **extra}
        if action in {"reserve-owner", "collide"}:
            settings["target"] = self.target
        path = self.output / ("step-" + str(len(self.steps)) + ".json")
        path.write_bytes(shared.canonical(settings))
        environment = dict(os.environ, APS_COLLISION_STEP=str(path), APS_COLLISION_COORDINATION_DB=str(self.coordination), APS_COLLISION_INVENTORY_DB=str(self.inventory))
        if kubeconfig:
            environment["KUBECONFIG"] = str(kubeconfig)
        else:
            environment.pop("KUBECONFIG", None)
        value = result_record(self.command([self.driver, "ownership_collision_step", "--exact", "--ignored", "--nocapture"], environment=environment), expected)
        if hasattr(self, "pair") and any(value[key] != self.pair[key] for key in PAIR):
            raise shared.QualificationError("collision step changed verified pair")
        self.steps.append(value)
        (self.output / "steps.json").write_bytes(shared.canonical(self.steps))
        return value

    def bootstrap(self):
        domains = [self.pair["ownerDomainId"], self.pair["contenderDomainId"]]
        for domain, trust, plan in zip(domains, [self.owner_trust, self.trust], [self.owner_plan, self.contender_plan]):
            self.state("reakt-trust-" + domain, shared.read_json(trust))
            self.state("reakt-policy-" + domain, full_policy(plan))
        registry = self.create({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "reakt-registry-" + self.workload, "namespace": self.control}})
        self.create({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "reakt-platform"}})
        self.state("reakt-registry-authority." + self.control + "." + self.workload,
                   {"schemaVersion": "reakt.dev/registry-authority/v1", "registryUid": registry["metadata"]["uid"], "clusterBinding": self.binding,
                    "controlNamespace": self.control, "workloadNamespace": self.workload}, "reakt-platform")
        self.step("bootstrap", "bootstrapped", self.admin, registryUid=registry["metadata"]["uid"], registryResourceVersion=registry["metadata"]["resourceVersion"])
        accounts = {}
        for name in ["ceiling-issuer", "ceiling-deployer"]:
            accounts[name] = self.create({"apiVersion": "v1", "kind": "ServiceAccount", "automountServiceAccountToken": False,
                                         "metadata": {"name": name, "namespace": self.control}})
        definition = {"apiVersion": "v1", "kind": "Secret", "immutable": True, "type": "Opaque", "metadata": {"name": "ceiling-binding", "namespace": self.control}}
        secret = self.binding_metadata(self.k("create", "-f", "-", "-o", base.BINDING_PATH, data=json.dumps(definition)))
        self.secret_uid = secret["uid"]
        for domain in domains:
            self.state("reakt-deploy-identity-" + domain,
                       {"schemaVersion": "reakt.dev/platform/v1", "domainId": domain, "clusterBinding": self.binding,
                        "serviceAccountName": "ceiling-deployer", "serviceAccountUid": accounts["ceiling-deployer"]["metadata"]["uid"],
                        "bindingSecretName": "ceiling-binding", "bindingSecretUid": self.secret_uid,
                        "audience": "https://kubernetes.default.svc.cluster.local", "lifetimeSeconds": 600})
        self.permissions(domains)
        config = json.loads(self.k("config", "view", "--raw", "--minify", "-o", "json"))
        token = self.k("create", "token", "ceiling-issuer", "-n", self.control, "--duration=10m").strip()
        config["users"] = [{"name": "ceiling-issuer", "user": {"token": token}}]
        config["contexts"][0]["context"]["user"] = "ceiling-issuer"
        with os.fdopen(os.open(self.issuer, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as stream:
            json.dump(config, stream)

    def assert_all_absent(self, owner, contender=None):
        original = self.plan
        try:
            for plan, prepared in [(self.owner_plan, owner), (self.contender_plan, contender)]:
                self.plan = plan
                if prepared is None:
                    self.assert_targets_absent(None)
                    continue
                expected_source = "claims-empty-capability-complete-projection" if plan is self.contender_plan else "capability-complete-projection"
                if prepared["creationTargetsSource"] != expected_source:
                    raise shared.QualificationError("collision target projection has unexpected authority")
                self.plan = plan
                # Only the absence checker consumes this projection; the frozen request is untouched.
                self.assert_targets_absent({**prepared, "creationTargetsSource": "capability-complete-projection"})
        finally:
            self.plan = original

    def rotate_marker(self):
        old = json.loads(self.k("get", "configmap", base.MARKER, "-n", self.cluster, "-o", "json"))
        old_meta = marker_metadata(old)
        if old_meta["uid"] != self.target["markerUid"] or old_meta["resourceVersion"] != self.target["markerResourceVersion"]:
            raise shared.QualificationError("owner marker changed before replacement")
        value = json.loads(old["data"]["state.json"], object_pairs_hook=shared.unique_object)
        if value["domainId"] != self.pair["ownerDomainId"] or value["releaseDigest"] != self.pair["ownerReleaseDigest"]:
            raise shared.QualificationError("owner marker does not bind verified owner")
        uri = "/api/v1/namespaces/" + self.cluster + "/configmaps/" + base.MARKER
        self.k("delete", "--raw", uri, "-f", "-", data=json.dumps({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": {"uid": old_meta["uid"], "resourceVersion": old_meta["resourceVersion"]}}))
        if self.k("get", "configmap", base.MARKER, "-n", self.cluster, "--ignore-not-found", "-o", "name").strip():
            raise shared.QualificationError("owner marker deletion was not confirmed")
        replacement = {**value, "domainId": self.pair["contenderDomainId"], "releaseDigest": self.pair["contenderReleaseDigest"]}
        created = self.state(base.MARKER, replacement, self.cluster, immutable=True)
        new_meta = marker_metadata(created)
        observed = json.loads(created["data"]["state.json"], object_pairs_hook=shared.unique_object)
        if observed != replacement or new_meta["uid"] == old_meta["uid"]:
            raise shared.QualificationError("replacement marker differs from exact contender target")
        self.target.update(markerUid=new_meta["uid"], markerResourceVersion=new_meta["resourceVersion"])
        self.rotated_marker = created
        return {"owner": old_meta, "contender": new_meta}

    def measure(self):
        self.phase = "prepare-owner"
        owner = self.step("prepare-owner", "owner-prepared", self.issuer)
        self.assert_all_absent(owner)
        before_owner = self.stable_baseline()
        self.phase = "reserve-owner"
        reserved = self.step("reserve-owner", "owner-planned", self.issuer, expectedRequestDigest=owner["requestDigest"])
        if any(owner[key] != reserved[key] for key in REQUEST - {"registryResourceVersion"}):
            raise shared.QualificationError("owner reservation differs from frozen request")
        self.assert_all_absent(owner)
        after_owner = self.snapshot()
        reservation_delta(before_owner, after_owner, self.control + "/configmaps/reakt-registry-" + self.workload)
        self.phase = "replace-marker"
        markers = self.rotate_marker()
        after_marker = self.snapshot()
        marker_key = self.cluster + "/configmaps/" + base.MARKER
        if after_marker["rawObjects"][marker_key] != shared.canonical(self.rotated_marker):
            raise shared.QualificationError("contender marker changed after creation")
        base.unchanged_except(after_owner["rawObjects"], after_marker["rawObjects"], {marker_key})
        if after_owner["rawCoordination"] != after_marker["rawCoordination"] or after_owner["rawInventory"] != after_marker["rawInventory"]:
            raise shared.QualificationError("marker replacement changed metadata stores")
        self.phase = "prepare-contender"
        contender = self.step("prepare-contender", "contender-prepared", self.issuer)
        if any(contender[key] != reserved[key] for key in ["registryUid", "registryResourceVersion"]):
            raise shared.QualificationError("contender request changed the owner reservation registry")
        self.assert_all_absent(owner, contender)
        before_denial = self.snapshot()
        if before_denial != after_marker:
            raise shared.QualificationError("contender preparation changed state")
        self.phase = "measure-collision"
        denied = self.step("collide", "collision-rejected", self.issuer, expectedRequestDigest=contender["requestDigest"])
        if any(denied[key] != contender[key] for key in REQUEST):
            raise shared.QualificationError("collision denial differs from frozen contender request")
        self.assert_all_absent(owner, contender)
        after_denial = self.snapshot()
        if before_denial != after_denial:
            raise shared.QualificationError("collision denial changed Kubernetes or metadata state")
        self.measurement = {"schemaVersion": "reakt.dev/ownership-collision-measurement/v1", "runnerSourceCommit": self.commit,
                            "candidateSourceCommit": self.manifest["source"]["commit"], "scope": "exact-candidate-reservation-on-owned-kind-cluster",
                            "executorScope": "one-qualification-executor-authorized-for-both-domains", "pair": self.pair, "markers": markers,
                            "ownerSubject": {"role": "owner", "releaseDigest": self.pair["ownerReleaseDigest"], "source": self.owner_manifest["source"],
                                             **{key: self.owner_manifest[key] for key in ["irDigest", "compilerDigest", "binaryDigest", "image"]},
                                             "sourceInputsDigest": shared.digest_bytes(shared.canonical(shared.read_json(self.owner_artifact / "build-inputs.json"))),
                                             "payloadFiles": {name: self.owner_manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "reakt-controller"]}},
                            "hostIdentity": self.host_identity, "dockerIdentity": self.docker_binding, "steps": self.steps,
                            "beforeOwner": base.public_snapshot(before_owner), "afterOwner": base.public_snapshot(after_owner),
                            "beforeDenial": base.public_snapshot(before_denial), "afterDenial": base.public_snapshot(after_denial),
                            "runnerSources": {self.archives[path]: self.source_digests[path.name] for path in self.sources},
                            "sourceArchiveFiles": {path.relative_to(ROOT).as_posix(): self.archives[path] for path in self.sources},
                            "driverDigest": self.driver_digest, "releaseVerified": False}

    def publish(self):
        self.verify_docker()
        self.verify_crds()
        if (shared.source_state(Path(__file__).resolve()) != (self.repository, self.commit)
                or any(shared.digest_file(path) != self.source_digests[path.name] for path in self.sources)
                or self.step("verify", "verified-pair") != self.pair or shared.digest_file(self.driver) != self.driver_digest):
            raise shared.QualificationError("collision subjects or measurement source changed")
        payload = shared.canonical({**self.measurement, "customResourceDefinitions": [
                {**item, "sourceFile": "evidence/ownership-collision-" + item["sourceFile"]} for item in self.crd_installations], "clusterDeleted": True, "credentialsRemoved": not self.admin.exists() and not self.issuer.exists()})
        if len(payload) > shared.MAX_EVIDENCE_BYTES or any(path.stat().st_size > shared.MAX_EVIDENCE_BYTES for path in self.sources):
            raise shared.QualificationError("collision evidence exceeds size limit")
        evidence = self.output / "evidence"
        evidence.mkdir()
        for path in self.sources:
            shutil.copyfile(path, evidence / self.archives[path])
            if shared.digest_file(evidence / self.archives[path]) != self.source_digests[path.name]:
                raise shared.QualificationError("staged collision source differs from measured source")
        crd_files = self.publish_crds(evidence, prefix="ownership-collision-")
        (evidence / "ownership-collision-measurement.json").write_bytes(payload)
        receipt = {"schemaVersion": shared.SCHEMA, "irDigest": self.manifest["irDigest"], "compilerDigest": self.manifest["compilerDigest"],
                   "binaryDigest": self.manifest["binaryDigest"], "image": self.manifest["image"],
                   "sourceInputsDigest": shared.digest_bytes(shared.canonical(shared.read_json(self.artifact / "build-inputs.json"))),
                   "payloadFiles": {name: self.manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "reakt-controller"]},
                   "runnerSourceFile": "evidence/" + self.archives[Path(__file__).resolve()], "runnerSourceDigest": self.source_digests[Path(__file__).name],
                   "checks": {"ownership-collision": {"status": "passed", "assertions": [
                       {"checkId": key, "expected": value, "actual": value, "passed": True}
                       for key, value in [("claim-collision-rejected", True), ("next-writer-started", False)]],
                       "evidenceFiles": ["evidence/ownership-collision-measurement.json", *["evidence/" + self.archives[path] for path in self.sources], *crd_files]}}}
        (evidence / "ownership-collision-evidence.json").write_bytes(shared.canonical(receipt))
        return evidence


if __name__ == "__main__":
    sys.exit(base.main(CollisionRunner, ("owner-artifact", "owner-trust-policy")))
