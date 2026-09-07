#!/usr/bin/env python3
"""Package six current-producer CNPG phase measurements; performs no cluster execution."""
import argparse
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile

import run_cnpg_phase_recovery as runner

demo, shared = runner.demo, runner.shared
LIMIT = shared.MAX_EVIDENCE_BYTES
SOURCES = {
    "phase-recovery-runner.py": "tests/reakt/run_cnpg_phase_recovery.py",
    "phase-recovery-adapter.rs": "crates/platform/src/pair_phase_interruption_live.rs",
    "permission-upgrade-runner.py": "tests/reakt/run_cnpg_demo.py",
    "permission-upgrade-cluster-base.py": "tests/reakt/run_deployment_ceiling.py",
    "permission-upgrade-crd-support.py": "tests/reakt/run_cluster.py",
    "permission-upgrade-shared.py": "tests/reakt/run_reproducibility.py",
    "permission-upgrade-observations.py": "tests/reakt/ceiling_observations.py",
    "permission-upgrade-proxy-canary.py": "crates/platform/tests/envoy/canary.py",
    "permission-upgrade-entry.rs": "crates/platform/tests/permission_upgrade.rs",
    "permission-upgrade-driver.rs": "crates/platform/tests/support/permission_upgrade_driver.rs",
    "permission-upgrade-qualification-support.rs": "crates/platform/tests/support/qualification_driver.rs",
    **{"permission-upgrade-" + name + ".rs": "crates/platform/tests/support/permission_upgrade/" + name + ".rs"
       for name in ["actions", "protocol", "support"]},
}
FIELDS = {"schemaVersion", "passed", "phase", "candidatePair", "successorSubject", "runnerSourceCommit",
          "sourceArchiveDigest", "runnerSourceDigest", "frozenInputDigests", "interruption", "finalCommit",
          "inventory", "inventoryIdempotence", "repeatedCommit", "oldWriterSafety", "workloadChecks", "steps",
          "cluster", "clusterDeleted", "credentialsRemoved", "releaseVerified", "limitation"}
BINDING = {"registryUid", "registryResourceVersion", "transition", "generation", "phase", "executorId",
           "executorBindingDigest", "registryDigest"}


def require(condition, reason):
    if not condition:
        raise shared.QualificationError(reason)


def exact(value, fields):
    require(isinstance(value, dict) and set(value) == set(fields), "phase evidence fields differ")


def bounded(path):
    require(not path.is_symlink() and path.is_file() and 0 < path.stat().st_size <= LIMIT,
            "phase packaging input is not a bounded regular file")
    raw = path.read_bytes()
    require(len(raw) <= LIMIT, "phase packaging input grew beyond bound")
    return raw


def parse(raw):
    return json.loads(raw, object_pairs_hook=shared.unique_object)


def validate_measurement(m):
    exact(m, FIELDS)
    require(m["schemaVersion"] == "apsides.dev/cnpg-phase-recovery-measurement/v1"
            and m["phase"] in runner.PHASES and all(m[k] is True for k in ["passed", "clusterDeleted", "credentialsRemoved"])
            and m["releaseVerified"] is False
            and m["limitation"] == "one post-CAS owned process killed and reaped; same logical owner, no stalled-executor takeover",
            "phase measurement is not a successful supported run")
    frozen = m["frozenInputDigests"]
    require(isinstance(frozen, dict) and 1 <= len(frozen) <= 256
            and all(shared.valid_digest(v) for v in frozen.values())
            and frozen.get("producer/source-archive") == m["sourceArchiveDigest"]
            and frozen.get("producer/phase-recovery-runner.py") == m["runnerSourceDigest"], "phase source commitments differ")
    steps = m["steps"]
    require(isinstance(steps, list) and 1 <= len(steps) <= 512, "phase lifecycle is missing or excessive")
    for step in steps:
        demo.result_record(demo.PREFIX + json.dumps(step), step.get("outcome"))
        require(step["candidates"] == m["candidatePair"], "phase lifecycle changed candidate pair")
    require(steps[0] == steps[-1] and steps[0]["outcome"] == "verified-pair"
            and all(steps[0][k] == v for k, v in demo.EXPECTED_SCOPE.items()), "phase lacks complete pair verification")
    order = ["verified-pair", "bootstrapped", "initial-prepared", "initial-reserved", "initial-staged", "initial-activated",
             "initial-inventory-published", "initial-resolved", "rejection-prepared", "upgrade-rejected", "successor-prepared",
             "successor-reserved", "successor-staged", "upgrade-quiesced", "upgrade-activated", "upgrade-committed",
             "successor-inventory-published", "successor-resolved", "upgrade-committed", "verified-pair"]
    pending = {v[0]: v[1] for v in demo.LIFECYCLE.values()}
    position, selected = 0, {}
    for outcome in order:
        while position < len(steps) and steps[position]["outcome"] == pending.get(outcome):
            position += 1
        require(position < len(steps) and steps[position]["outcome"] == outcome, "phase lifecycle is incomplete or out of order")
        selected[outcome] = steps[position]
        position += 1
    require(position == len(steps), "phase lifecycle has trailing operations")
    completed_phases = {"initial-reserved": "Planned", "initial-staged": "Staged", "initial-activated": "Committed",
                        "initial-resolved": "Committed", "successor-reserved": "Planned", "successor-staged": "Staged",
                        "upgrade-quiesced": "Quiesced", "upgrade-activated": "Cleaning", "upgrade-committed": "Committed",
                        "successor-resolved": "Committed"}
    for step in steps:
        if step["outcome"] in completed_phases:
            require(step["phase"] == completed_phases[step["outcome"]], "phase lifecycle reports an invalid completed phase")
    prepared = selected["successor-prepared"]
    for step in steps:
        if "registryUid" in step:
            require(step["registryUid"] == prepared["registryUid"], "phase registry identity changed")
        if (step["outcome"].startswith("successor-") or step["outcome"].startswith("upgrade-")) and "transition" in step:
            require(step["transition"] == prepared["transition"], "phase successor transition changed")
        if (step["outcome"].startswith("successor-") or step["outcome"].startswith("upgrade-")) and "generation" in step:
            require(step["generation"] == prepared["generation"], "phase successor generation changed")
    require(all(selected["successor-reserved"][k] == prepared[k] for k in ["generation", "transition", "requestDigest"]),
            "phase reservation differs from prepared request")
    i = m["interruption"]
    exact(i, {"interruptedAction", "checkpoint", "protectedBindingBefore", "protectedBindingAfterKill", "termination",
              "interruptedSession", "resumeAction", "recoveryMode", "resumedResult", "resumedBinding", "freshIssuedSession", "resumedSession"})
    before, after, resumed = i["protectedBindingBefore"], i["protectedBindingAfterKill"], i["resumedBinding"]
    exact(before, BINDING | {"stagedIdentityCount"}); exact(after, set(before)); exact(resumed, BINDING)
    exact(i["checkpoint"], {"registryUid", "registryResourceVersion", "transition", "phase"})
    require(before == after and all(i["checkpoint"][k] == before[k] for k in i["checkpoint"])
            and before["phase"] == m["phase"] and before["generation"] == prepared["generation"]
            and before["transition"] == prepared["transition"] and before["registryUid"] == prepared["registryUid"]
            and before["executorId"] == "candidate-pair-qualification"
            and all(shared.valid_digest(before[k]) for k in ["executorBindingDigest", "registryDigest"]),
            "phase checkpoint or protected owner changed")
    require(type(before["stagedIdentityCount"]) is int and (before["stagedIdentityCount"] == 0 if m["phase"] == "Planned" else before["stagedIdentityCount"] > 0),
            "phase staged identities are inconsistent")
    termination = i["termination"]
    exact(termination, {"pid", "signal", "returnCode", "reaped"})
    require(type(termination["pid"]) is int and termination["pid"] > 0 and termination["signal"] == "SIGKILL"
            and termination["returnCode"] == -9 and termination["reaped"] is True
            and i["freshIssuedSession"] is True and type(i["interruptedSession"]) is int
            and type(i["resumedSession"]) is int and 0 < i["interruptedSession"] < i["resumedSession"],
            "phase lacks killed and reaped executor with a fresh issued session")
    readback = m["phase"] in {"Planned", "Staged", "Quiesced"}
    require(i["interruptedAction"] == runner.PHASES[m["phase"]]
            and i["resumeAction"] == ("resume-checkpoint" if readback else i["interruptedAction"])
            and i["recoveryMode"] == ("protected-checkpoint-readback" if readback else "operation-replay")
            and all(resumed[k] == before[k] for k in ["registryUid", "transition", "generation", "executorId", "executorBindingDigest"]),
            "phase recovery action or logical owner differs")
    resumed_result = i["resumedResult"]
    allowed = {"Planned": {("successor-reserved", "Planned")}, "Staged": {("successor-staged", "Staged")},
               "Quiesced": {("upgrade-quiesced", "Quiesced")},
               "Activated": {("upgrade-activated", "Cleaning"), ("upgrade-activation-pending", "Activated")},
               "Cleaning": {("upgrade-activated", "Cleaning"), ("upgrade-activation-pending", "Cleaning")},
               "Committed": {("upgrade-committed", "Committed"), ("upgrade-commit-pending", "Committed")}}
    require(resumed_result in steps and all(resumed_result[k] == resumed[k] for k in ["phase", "registryUid", "registryResourceVersion", "transition", "generation"])
            and (resumed_result["outcome"], resumed["phase"]) in allowed[m["phase"]], "phase did not resume its actual persisted operation")
    if readback:
        require(all(resumed[k] == before[k] for k in BINDING), "checkpoint readback mutated protected state")
    final = m["finalCommit"]
    exact(final, BINDING)
    require(final["phase"] == "Committed" and all(final[k] == before[k] for k in ["registryUid", "transition", "generation", "executorId", "executorBindingDigest"])
            and m["repeatedCommit"] == selected["upgrade-committed"] and m["inventory"] == selected["successor-inventory-published"],
            "phase lacks final commit and inventory proof")
    idem = m["inventoryIdempotence"]
    exact(idem, {"before", "after", "unchanged"})
    require(idem["unchanged"] is True and idem["before"] == idem["after"]
            and set(idem["before"]) == demo.base.observations.INVENTORY_TABLES, "phase inventory was not idempotent")
    for table, snapshot in idem["before"].items():
        demo.base.observations._validated_table(snapshot, table)
    safety = m["oldWriterSafety"]
    exact(safety, {"nonterminalOldWriterPods", "terminalOldWriterPodUids", "retirementVerifiedByProductionActivation"})
    require(type(safety["nonterminalOldWriterPods"]) is int and safety["nonterminalOldWriterPods"] == 0
            and safety["retirementVerifiedByProductionActivation"] is True
            and isinstance(safety["terminalOldWriterPodUids"], list), "phase old writer safety is incomplete")
    checks = m["workloadChecks"]
    require(isinstance(checks, list) and len(checks) == 5 and all(x.get("passed") is True for x in checks)
            and {x.get("check") for x in checks} == {"cnpg-ready-and-secret-bound", "postgrest-messages-live",
            "configmap-driven-pod-turnover", "postgrest-messages-after-rollout", "secret-bound-before-successor-start"},
            "phase workload checks are incomplete")
    by_check = {x["check"]: x for x in checks}
    cnpg = by_check["cnpg-ready-and-secret-bound"]
    exact(cnpg, {"check", "passed", "clusterUid", "secretUid", "uriKeyPresent"})
    require(cnpg["uriKeyPresent"] is True, "phase lacks CNPG Secret binding")
    for name, count in [("postgrest-messages-live", 2), ("postgrest-messages-after-rollout", 1)]:
        http = by_check[name]
        exact(http, {"check", "passed", "status", "rowCount", "bodyDigest"})
        require(http["status"] == 200 and http["rowCount"] == count and shared.valid_digest(http["bodyDigest"]), "phase workload HTTP proof differs")
    rollout = by_check["configmap-driven-pod-turnover"]
    exact(rollout, {"check", "passed", "before", "after"})
    for identity in [rollout["before"], rollout["after"]]:
        exact(identity, {"deploymentUid", "deploymentResourceVersion", "revision", "replicaSetUid", "podUid", "containerImageId", "selectedImageManifestDigest", "restartCount"})
        require(identity["restartCount"] == 0 and shared.valid_digest(identity["selectedImageManifestDigest"]), "phase workload identity differs")
    require(rollout["before"]["deploymentUid"] == rollout["after"]["deploymentUid"]
            and all(rollout["before"][k] != rollout["after"][k] for k in ["revision", "replicaSetUid", "podUid"]), "phase rollout did not occur")
    claim = by_check["secret-bound-before-successor-start"]
    exact(claim, {"check", "passed", "claim"})
    require(claim["claim"] == "binding-only; no Secret creation reaction claimed", "phase Secret observation scope differs")
    return prepared


def validate_set(measurements):
    require(len(measurements) == 6 and {m["phase"] for m in measurements} == set(runner.PHASES), "phase set is missing or duplicated")
    for m in measurements:
        validate_measurement(m)
    first = measurements[0]
    for m in measurements[1:]:
        require(all(m[k] == first[k] for k in ["candidatePair", "successorSubject", "runnerSourceCommit", "sourceArchiveDigest", "runnerSourceDigest"]),
                "phase runs have mixed source or candidate subjects")
        for key in first["frozenInputDigests"]:
            if key.startswith(("producer/", "executable/")):
                require(m["frozenInputDigests"].get(key) == first["frozenInputDigests"][key], "phase producer executables differ")
    require(len({m["cluster"] for m in measurements}) == 6
            and len({m["interruption"]["checkpoint"]["registryUid"] for m in measurements}) == 6,
            "phase runs did not use independently owned clusters")


def source_closure(raw, measurement, repository, commit):
    require(measurement["runnerSourceCommit"] == commit and shared.digest_bytes(raw) == measurement["sourceArchiveDigest"],
            "phase archive is not from current clean producer")
    expected = subprocess.run(["git", "-C", str(repository), "archive", "--format=tar", commit], capture_output=True, check=True).stdout
    require(raw == expected, "phase source archive differs from current Git source")
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
        members = archive.getmembers()
        require(len(members) <= 20000 and len({x.name for x in members}) == len(members), "phase archive has duplicate or excessive members")
        for alias, path in SOURCES.items():
            member = archive.getmember(path)
            require(member.isfile() and 0 < member.size <= LIMIT, "phase producer member is invalid")
            stream = archive.extractfile(member)
            require(stream is not None, "phase producer member is unreadable")
            with stream:
                content = stream.read(LIMIT + 1)
            require(content == bounded(repository / path)
                    and shared.digest_bytes(content) == measurement["frozenInputDigests"].get("producer/" + alias),
                    "phase producer source closure changed")


def input_closure(args, root, m):
    files, manifests = {}, {}
    frozen = m["frozenInputDigests"]
    for role, path in [("initial", args.initial_artifact), ("successor", args.successor_artifact)]:
        manifest = parse(bounded(path / "release.json")); manifests[role] = manifest
        require(manifest.get("mode") == "production" and parse(bounded(path / "plan.json"))["ir"].get("testOnly") is False,
                "phase subject is a fixture or nonproduction artifact")
        files.update(demo.release_files(path, manifest, role))
        require(all(frozen.get(role + "/" + name) == digest for name, digest in manifest["files"].items())
                and frozen.get(role + "/release.json") == m["candidatePair"][role + "ReleaseDigest"]
                and frozen.get(role + "/provenance.json") == m["candidatePair"][role + "ProvenanceDigest"]
                and manifest["irDigest"] == m["candidatePair"][role + "IrDigest"], "phase signed pair payload changed")
    files.update({"trust/policy": args.trust_policy, "operator/source": root / "operator-manifest.yaml",
                  "operator/snapshot": root / "operator-manifest.yaml", "producer/source-archive": root / "permission-upgrade-source.tar",
                  "executable/permission-upgrade-driver": root / "permission-upgrade-driver",
                  "executable/proxy-profile-driver": root / "proxy-profile-driver",
                  "executable/phase-checkpoint-driver": root / "phase-checkpoint-driver",
                  "canary/source-snapshot": root / "proxy-canary.py", "canary/result": root / "proxy-canary/result.json",
                  "proxy/archive-0": root / "proxy-0.tar", "proxy/archive-1": root / "proxy-1.tar"})
    require(set(frozen) == set(files) | {"producer/" + k for k in SOURCES}, "phase frozen input closure is incomplete")
    require(demo.snapshot_files(files) == {key: frozen[key] for key in files}, "phase frozen input changed")
    require(shared.digest_bytes(shared.canonical(parse(bounded(args.trust_policy)))) == m["candidatePair"]["trustPolicyDigest"], "phase trust policy changed")
    manifest = manifests["successor"]
    subject = {key: manifest[key] for key in ["irDigest", "compilerDigest", "binaryDigest", "image"]}
    subject.update(releaseDigest=m["candidatePair"]["successorReleaseDigest"], sourceInputsDigest=manifest["source"]["inputsDigest"],
                   payloadFiles={name: manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "reakt-controller"]})
    require(m["successorSubject"] == subject, "phase successor subject changed")
    return files


def package(args):
    require(len(args.measurements) == 6, "exactly six phase run directories are required")
    repository, commit = shared.source_state(Path(__file__).resolve())
    repository = Path(repository)
    records = [(root, bounded(root / "evidence/measurement.json")) for root in args.measurements]
    measurements = [parse(raw) for _, raw in records]
    validate_set(measurements)
    documents, rechecks = {}, []
    for (root, raw), m in zip(records, measurements):
        archive = bounded(root / "evidence/phase-recovery-source.tar")
        source_closure(archive, m, repository, commit)
        files = input_closure(args, root, m)
        require(parse(bounded(root / "steps.json")) == m["steps"], "phase recorded driver steps changed")
        result = parse(bounded(root / "result.json"))
        require(result.get("passed") is True and result.get("phase") == m["phase"]
                and result.get("productionEvidenceStatus") == "not-produced", "phase run did not finish its measurement")
        cleanup_raw = bounded(root / "evidence/phase-recovery-cleanup.json")
        cleanup = parse(cleanup_raw)
        exact(cleanup, {"cluster", "deleted", "previousClusters", "currentClusters", "previousContainerIds", "currentContainerIds"})
        require(cleanup_raw == bounded(root / "cleanup.json") and cleanup["deleted"] is True
                and cleanup["cluster"] == m["cluster"] and cleanup["cluster"] not in cleanup["previousClusters"]
                and cleanup["previousClusters"] == cleanup["currentClusters"]
                and cleanup["previousContainerIds"] == cleanup["currentContainerIds"]
                and all(not (root / name).exists() and not (root / name).is_symlink()
                        for name in ["admin.kubeconfig", "issuer.kubeconfig", "issuer-next.kubeconfig"]), "phase cleanup is incomplete")
        target = parse(bounded(root / "step-1.json"))["target"]
        require("reakt-qual-" + target["cluster"]["runId"] == m["cluster"]
                and target["purpose"] == {"kind": "permission-expansion"}, "phase owned cluster or purpose differs")
        common = {"cnpg-phase-recovery-source.tar": archive,
                  "cnpg-phase-recovery-runner.py": bounded(root / "evidence/phase-recovery-runner.py"),
                  "cnpg-phase-recovery-operator.yaml": bounded(root / "evidence/phase-recovery-operator.yaml")}
        require(shared.digest_bytes(common["cnpg-phase-recovery-runner.py"]) == m["runnerSourceDigest"]
                and shared.digest_bytes(common["cnpg-phase-recovery-operator.yaml"]) == m["frozenInputDigests"]["operator/snapshot"], "phase packaged source or operator changed")
        for name, content in common.items():
            require(name not in documents or documents[name] == content, "phase shared evidence differs")
            documents[name] = content
        prefix = "cnpg-phase-" + m["phase"].lower()
        documents[prefix + "-measurement.json"] = raw
        documents[prefix + "-cleanup.json"] = cleanup_raw
        rechecks.append((root, raw, cleanup_raw, files, m))
    documents["cnpg-phase-recovery-packager.py"] = bounded(Path(__file__).resolve())
    subject = measurements[0]["successorSubject"]
    receipt = {"schemaVersion": shared.SCHEMA, **{k: subject[k] for k in ["irDigest", "compilerDigest", "binaryDigest", "image", "sourceInputsDigest", "payloadFiles"]},
               "runnerSourceFile": "evidence/cnpg-phase-recovery-runner.py", "runnerSourceDigest": measurements[0]["runnerSourceDigest"],
               "checks": {"phase-interruption-recovery": {"status": "passed", "assertions": [
                   {"checkId": name, "expected": value, "actual": value, "passed": True}
                   for name, value in [(p.lower() + "-interruption-recovered", True) for p in runner.PHASES] + [("unsafe-activation", False)]],
                   "evidenceFiles": ["evidence/" + name for name in sorted(documents)]}}}
    require(shared.source_state(Path(__file__).resolve()) == (repository, commit), "phase packaging source changed")
    for root, raw, cleanup_raw, files, m in rechecks:
        require(bounded(root / "evidence/measurement.json") == raw and bounded(root / "cleanup.json") == cleanup_raw
                and demo.snapshot_files(files) == {key: m["frozenInputDigests"][key] for key in files}, "phase inputs changed during packaging")
    require(all(0 < len(raw) <= LIMIT for raw in documents.values()), "phase evidence exceeds bounds")
    require(not args.output.exists() and not args.output.is_symlink(), "phase packaging output already exists")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.mkdir()
    # The receipt is the completion marker; incomplete destinations must not be merged.
    for name, raw in documents.items():
        with (args.output / name).open("xb") as stream:
            stream.write(raw)
    name = args.output / "cnpg-phase-recovery-evidence.json"
    with name.open("xb") as stream:
        stream.write(shared.canonical(receipt))
    return name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["initial-artifact", "successor-artifact", "trust-policy", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--measurements", type=Path, nargs=6, required=True)
    try:
        path = package(parser.parse_args())
        print(json.dumps({"productionEvidence": str(path), "releaseVerified": False,
                          "basis": "six successful current-producer local measurements"}))
    except Exception:
        print("CNPG phase recovery packaging failed; no acceptance is claimed.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
