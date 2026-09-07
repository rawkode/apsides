#!/usr/bin/env python3
"""Package recorded CNPG cleanup measurements as signer-attestable production evidence."""
import argparse
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile

import run_cnpg_cleanup as cleanup

shared = cleanup.shared
LIMIT = shared.MAX_EVIDENCE_BYTES
# This adapter interprets the readback assertions of this reviewed producer version only.
PRODUCER_COMMIT = "64f2b396d190705c5e81faacdfcb3f280523e093"
PRODUCER_ARCHIVE = "sha256:4cd4bcfa5f4ab0911ee673adeacaa6ad8e6b1204335581f8886be6b337cf4680"
SOURCES = {
    "cnpg-cleanup-runner.py": "tests/reakt/run_cnpg_cleanup.py",
    "permission-upgrade-runner.py": "tests/reakt/run_cnpg_demo.py",
    "permission-upgrade-cluster-base.py": "tests/reakt/run_deployment_ceiling.py",
    "permission-upgrade-crd-support.py": "tests/reakt/run_cluster.py",
    "permission-upgrade-sampling.py": "tests/reakt/performance/sampling.py",
    "permission-upgrade-collector.py": "tests/reakt/performance/collect.py",
    "permission-upgrade-shared.py": "tests/reakt/run_reproducibility.py",
    "permission-upgrade-observations.py": "tests/reakt/ceiling_observations.py",
    "permission-upgrade-proxy-canary.py": "crates/platform/tests/envoy/canary.py",
    "permission-upgrade-entry.rs": "crates/platform/tests/permission_upgrade.rs",
    "permission-upgrade-driver.rs": "crates/platform/tests/support/permission_upgrade_driver.rs",
    "permission-upgrade-qualification-support.rs": "crates/platform/tests/support/qualification_driver.rs",
    **{"permission-upgrade-" + name + ".rs": "crates/platform/tests/support/permission_upgrade/" + name + ".rs"
       for name in ["actions", "protocol", "support"]},
}
MEASUREMENT_FIELDS = {"schemaVersion", "scenario", "candidatePair", "purpose", "runnerSourceCommit",
                      "frozenInputDigests", "policyDigest", "checks", "runtimeRoles", "steps", "passed",
                      "clusterDeleted", "credentialsRemoved", "releaseVerified", "productionEvidenceStatus", "limitation"}


def require(condition, reason):
    if not condition:
        raise shared.QualificationError(reason)


def bounded(path):
    require(not path.is_symlink() and path.is_file() and 0 < path.stat().st_size <= LIMIT,
            "cleanup packaging input is not a bounded regular file")
    raw = path.read_bytes()
    require(len(raw) <= LIMIT, "cleanup packaging input grew beyond its bound")
    return raw


def parse(raw):
    return json.loads(raw, object_pairs_hook=shared.unique_object)


def exact(value, fields):
    require(isinstance(value, dict) and set(value) == set(fields), "cleanup measurement has unexpected fields")


def identifier(value):
    return isinstance(value, str) and 0 < len(value) <= 253


def validate_measurement(value):
    exact(value, MEASUREMENT_FIELDS)
    require(value["schemaVersion"] == "apsides.dev/cnpg-cleanup-measurement/v1"
            and value["scenario"] in {"finalizer", "replacement"}
            and value["runnerSourceCommit"] == PRODUCER_COMMIT
            and value["purpose"] == cleanup.CnpgCleanupRunner.purpose
            and all(value[key] is True for key in ["passed", "clusterDeleted", "credentialsRemoved"])
            and value["releaseVerified"] is False and value["productionEvidenceStatus"] == "not-produced"
            and value["limitation"] == "live measurement only; no production cleanup receipt contract",
            "cleanup measurement is not a supported successful archival run")
    require(shared.valid_digest(value["policyDigest"]), "cleanup policy digest is invalid")
    snapshot = value["frozenInputDigests"]
    require(isinstance(snapshot, dict) and 1 <= len(snapshot) <= 256
            and all(isinstance(key, str) and shared.valid_digest(item) for key, item in snapshot.items())
            and snapshot.get("producer/source-archive") == PRODUCER_ARCHIVE,
            "cleanup measurement does not bind the supported source archive")
    steps = value["steps"]
    require(isinstance(steps, list) and 1 <= len(steps) <= 512, "cleanup lifecycle steps are missing or excessive")
    for step in steps:
        require(isinstance(step, dict) and isinstance(step.get("outcome"), str), "cleanup lifecycle step is invalid")
        cleanup.result_record(cleanup.demo.PREFIX + json.dumps(step), step["outcome"])
        require(step["candidates"] == value["candidatePair"], "cleanup lifecycle changed the candidate pair")
    require(steps[0]["outcome"] == "verified-pair" and steps[-1] == steps[0], "cleanup pair lacks final revalidation")
    require(all(steps[0][key] == item for key, item in cleanup.demo.EXPECTED_SCOPE.items()),
            "cleanup measurement escaped the CNPG scope")
    sequence = ["verified-pair", "bootstrapped", "initial-prepared", "initial-reserved",
                "initial-staged", "initial-activated", "initial-inventory-published", "successor-prepared",
                "successor-reserved", "successor-staged", "upgrade-quiesced", "upgrade-activated"]
    if value["scenario"] == "finalizer":
        sequence += ["upgrade-commit-pending", "upgrade-committed", "successor-inventory-published", "successor-resolved"]
    else:
        sequence += ["cleanup-uid-rejected"]
    sequence += ["verified-pair"]
    pending = {"initial-staged": "initial-stage-pending", "initial-activated": "initial-activation-pending",
               "successor-staged": "successor-stage-pending", "upgrade-quiesced": "upgrade-quiescence-pending",
               "upgrade-activated": "upgrade-activation-pending", "upgrade-committed": "upgrade-commit-pending"}
    position = 0
    selected = {}
    for outcome in sequence:
        while position < len(steps) and steps[position]["outcome"] == pending.get(outcome):
            position += 1
        require(position < len(steps) and steps[position]["outcome"] == outcome,
                "cleanup lifecycle is incomplete or out of order")
        selected[outcome] = steps[position]
        position += 1
    require(position == len(steps), "cleanup lifecycle has trailing operations")
    registry = selected["bootstrapped"]["registryUid"]
    for step in steps:
        if "registryUid" in step:
            require(step["registryUid"] == registry, "cleanup lifecycle changed the registry UID")
    for role in ["initial", "successor"]:
        prepared = selected[role + "-prepared"]
        reserved = selected[role + "-reserved"]
        require(all(reserved[key] == prepared[key] for key in ["generation", "transition", "requestDigest"]),
                "cleanup reservation changed the prepared allocation")
        for step in steps:
            belongs = step["outcome"].startswith(role + "-") or (role == "successor" and step["outcome"].startswith(("upgrade-", "cleanup-")))
            if belongs and "transition" in step:
                require(step["transition"] == prepared["transition"], "cleanup lifecycle changed its transition")
            if belongs and "generation" in step:
                require(step["generation"] == prepared["generation"], "cleanup lifecycle changed its generation")
    require(selected["initial-prepared"]["generation"] != selected["successor-prepared"]["generation"],
            "cleanup did not upgrade generations")
    phases = {"initial-reserved": "Planned", "initial-staged": "Staged", "initial-activated": "Committed",
              "successor-reserved": "Planned", "successor-staged": "Staged", "upgrade-quiesced": "Quiesced",
              "upgrade-activated": "Cleaning", "upgrade-commit-pending": "Cleaning",
              "cleanup-uid-rejected": "Cleaning", "upgrade-committed": "Committed", "successor-resolved": "Committed"}
    for outcome, phase in phases.items():
        if outcome in selected:
            require(selected[outcome]["phase"] == phase, "cleanup lifecycle has an invalid phase")
    checks = value["checks"]
    require(isinstance(checks, list) and len(checks) == 3 and all(isinstance(row, dict) for row in checks),
            "cleanup measurement checks are incomplete")
    by_check = {row.get("check"): row for row in checks}
    cnpg = by_check.get("cnpg-ready-and-secret-bound")
    exact(cnpg, {"check", "clusterUid", "secretUid", "uriKeyPresent", "passed"})
    require(cnpg["passed"] is True and cnpg["uriKeyPresent"] is True
            and identifier(cnpg["clusterUid"]) and identifier(cnpg["secretUid"]), "cleanup CNPG precondition failed")
    control = by_check.get("unclaimed-same-type-resource-preserved")
    exact(control, {"check", "identity", "uid", "passed"})
    require(control["passed"] is True and identifier(control["uid"])
            and control["identity"] == cleanup.TARGET | {"name": cleanup.CONTROL_NAME},
            "cleanup lacks its surviving unclaimed control")
    if value["scenario"] == "finalizer":
        observed = by_check.get("finalizer-pending-then-last-type-cleanup")
        exact(observed, {"check", "passed", "originalUid", "finalizer", "pending"})
        require(observed["finalizer"] == cleanup.FINALIZER
                and observed["pending"] == selected["upgrade-commit-pending"], "cleanup finalizer observation is incomplete")
        assertions = {"last-api-type-cleanup": [("last-api-type-cleanup-complete", True), ("wrong-object-deleted", False)],
                      "finalizer-preservation": [("finalizer-block-reported", True), ("finalizers-forced", False)]}
    else:
        observed = by_check.get("replacement-uid-rejected-with-original-claim-retained")
        exact(observed, {"check", "passed", "originalUid", "replacementUid", "rejection"})
        require(identifier(observed["replacementUid"]) and observed["replacementUid"] != observed["originalUid"]
                and observed["rejection"] == selected["cleanup-uid-rejected"], "cleanup replacement rejection is incomplete")
        assertions = {"uid-replacement-rejection": [("replacement-deleted", False), ("uid-mismatch-rejected", True)]}
    require(observed["passed"] is True and identifier(observed["originalUid"])
            and observed["originalUid"] != control["uid"], "cleanup observation has invalid object identities")
    roles = value["runtimeRoles"]
    require(isinstance(roles, list) and len(roles) == 1, "cleanup successor runtime Role proof is missing")
    exact(roles[0], {"name", "uid", "rulesDigest"})
    require(roles[0]["name"] == selected["successor-prepared"]["serviceAccountName"]
            and identifier(roles[0]["uid"]) and shared.valid_digest(roles[0]["rulesDigest"]),
            "cleanup successor runtime Role proof is not bound to its allocation")
    return assertions


def archive_sources(raw, frozen):
    require(shared.digest_bytes(raw) == PRODUCER_ARCHIVE, "cleanup source archive changed")
    result = {}
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
        members = archive.getmembers()
        require(len(members) <= 20000 and len({member.name for member in members}) == len(members),
                "cleanup source archive has duplicate or excessive members")
        by_name = {member.name: member for member in members}
        for alias, path in SOURCES.items():
            member = by_name.get(path)
            require(member is not None and member.isfile() and 0 < member.size <= LIMIT,
                    "cleanup source archive lacks an expected producer file")
            stream = archive.extractfile(member)
            require(stream is not None, "cleanup source archive member is unreadable")
            with stream:
                content = stream.read(LIMIT + 1)
            require(len(content) <= LIMIT and shared.digest_bytes(content) == frozen.get("producer/" + alias),
                    "cleanup producer source digest changed")
            result[alias] = content
    return result


def freeze_inputs(args, measurement, source_bytes):
    frozen = measurement["frozenInputDigests"]
    files = {}
    manifests = {}
    for role, root in [("initial", args.initial_artifact), ("successor", args.successor_artifact)]:
        manifest = parse(bounded(root / "release.json"))
        require(manifest.get("mode") == "production", "cleanup subject is not a production release")
        manifests[role] = manifest
        files.update(cleanup.demo.release_files(root, manifest, role))
        for name, digest in manifest["files"].items():
            require(frozen.get(role + "/" + name) == digest, "cleanup release file differs from the measured payload")
        pair = measurement["candidatePair"]
        require(frozen.get(role + "/release.json") == pair[role + "ReleaseDigest"]
                and frozen.get(role + "/provenance.json") == pair[role + "ProvenanceDigest"]
                and manifest["irDigest"] == pair[role + "IrDigest"], "cleanup subject differs from the verified pair")
        plan = parse(bounded(root / "plan.json"))
        require(plan["ir"].get("testOnly") is False, "cleanup measured a fixture plan")
    files.update({
        "trust/policy": args.trust_policy, "operator/source": args.measurement / "operator-manifest.yaml",
        "operator/snapshot": args.measurement / "operator-manifest.yaml",
        "producer/source-archive": args.measurement / "permission-upgrade-source.tar",
        "executable/permission-upgrade-driver": args.measurement / "permission-upgrade-driver",
        "executable/proxy-profile-driver": args.measurement / "proxy-profile-driver",
        "canary/source-snapshot": args.measurement / "proxy-canary.py",
        "canary/result": args.measurement / "proxy-canary/result.json",
        "proxy/archive-0": args.measurement / "proxy-0.tar", "proxy/archive-1": args.measurement / "proxy-1.tar",
    })
    expected_sources = {"producer/" + alias: shared.digest_bytes(raw) for alias, raw in source_bytes.items()}
    require(set(frozen) == set(files) | set(expected_sources), "cleanup frozen input closure is incomplete")
    require(all(frozen[key] == digest for key, digest in expected_sources.items()), "cleanup source input changed")
    require(cleanup.demo.snapshot_files(files) == {key: frozen[key] for key in files}, "cleanup frozen inputs changed")
    trust = parse(bounded(args.trust_policy))
    require(shared.digest_bytes(shared.canonical(trust)) == measurement["candidatePair"]["trustPolicyDigest"],
            "cleanup canonical trust policy changed")
    initial = parse(bounded(args.initial_artifact / "plan.json"))
    successor = parse(bounded(args.successor_artifact / "plan.json"))
    policy = cleanup.cleanup_policy(initial, successor)
    require(shared.digest_bytes(shared.canonical(policy)) == measurement["policyDigest"], "cleanup approved policy changed")
    return files, manifests["successor"]


def package(args):
    raw = bounded(args.measurement / "measurement.json")
    require(shared.valid_digest(args.measurement_sha256) and shared.digest_bytes(raw) == args.measurement_sha256,
            "cleanup measurement digest differs from the selected recorded result")
    measurement = parse(raw)
    assertions = validate_measurement(measurement)
    require(bounded(args.measurement / "result.json") == raw, "cleanup result differs from the published measurement")
    require(parse(bounded(args.measurement / "steps.json")) == measurement["steps"], "cleanup step archive changed")
    source_archive = bounded(args.measurement / "permission-upgrade-source.tar")
    sources = archive_sources(source_archive, measurement["frozenInputDigests"])
    files, manifest = freeze_inputs(args, measurement, sources)
    cleanup_raw = bounded(args.measurement / "cleanup.json")
    disposition = parse(cleanup_raw)
    exact(disposition, {"cluster", "deleted", "previousClusters", "currentClusters", "previousContainerIds", "currentContainerIds"})
    require(disposition["deleted"] is True and identifier(disposition["cluster"])
            and disposition["cluster"] not in disposition["previousClusters"]
            and disposition["previousClusters"] == disposition["currentClusters"]
            and disposition["previousContainerIds"] == disposition["currentContainerIds"]
            and all(not (args.measurement / name).exists() for name in ["admin.kubeconfig", "issuer.kubeconfig", "issuer-next.kubeconfig"]),
            "cleanup disposition does not prove restored host inventories and credential removal")
    first_settings = parse(bounded(args.measurement / "step-1.json"))
    require(first_settings["target"]["purpose"] == measurement["purpose"]
            and first_settings["target"]["initialPolicyDigest"] == measurement["policyDigest"]
            and first_settings["target"]["successorPolicyDigest"] == measurement["policyDigest"]
            and "reakt-qual-" + first_settings["target"]["cluster"]["runId"] == disposition["cluster"],
            "cleanup disposition changed the measured cluster identity")
    prefix = "cnpg-cleanup-" + measurement["scenario"]
    documents = {
        prefix + "-measurement.json": raw,
        prefix + "-runner.py": sources["cnpg-cleanup-runner.py"],
        prefix + "-source.tar": source_archive,
        prefix + "-operator.yaml": bounded(args.measurement / "operator-manifest.yaml"),
        prefix + "-cleanup.json": cleanup_raw,
        prefix + "-steps.json": bounded(args.measurement / "steps.json"),
        prefix + "-adapter.py": bounded(Path(__file__).resolve()),
    }
    evidence_files = ["evidence/" + name for name in sorted(documents)]
    receipt = {"schemaVersion": shared.SCHEMA,
               **{key: manifest[key] for key in ["irDigest", "compilerDigest", "binaryDigest", "image"]},
               "sourceInputsDigest": manifest["source"]["inputsDigest"],
               "payloadFiles": {name: manifest["files"][name] for name in ["build-inputs.json", "controller.oci.tar", "reakt-controller"]},
               "runnerSourceFile": "evidence/" + prefix + "-runner.py",
               "runnerSourceDigest": measurement["frozenInputDigests"]["producer/cnpg-cleanup-runner.py"],
               "checks": {name: {"status": "passed", "assertions": [
                   {"checkId": check, "expected": expected, "actual": expected, "passed": True}
                   for check, expected in checks], "evidenceFiles": evidence_files}
                   for name, checks in assertions.items()}}
    documents[prefix + "-evidence.json"] = shared.canonical(receipt)
    require(all(len(content) <= LIMIT for content in documents.values()), "cleanup evidence exceeds the file bound")
    require(not args.output.exists() and not args.output.is_symlink(), "cleanup packaging output already exists")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".cnpg-cleanup-package-", dir=args.output.parent) as temporary:
        staged = Path(temporary) / "evidence"
        staged.mkdir()
        for name, content in documents.items():
            (staged / name).write_bytes(content)
        require(cleanup.demo.snapshot_files(files) == {key: measurement["frozenInputDigests"][key] for key in files}
                and bounded(args.measurement / "measurement.json") == raw
                and bounded(args.measurement / "cleanup.json") == cleanup_raw
                and parse(bounded(args.measurement / "steps.json")) == measurement["steps"],
                "cleanup inputs changed during evidence packaging")
        # Exclusive directory creation rejects raced paths and dangling symlinks. The
        # receipt is the completion marker and is copied only after all evidence files.
        args.output.mkdir()
        for name in sorted(documents, key=lambda item: item == prefix + "-evidence.json"):
            with (args.output / name).open("xb") as destination:
                destination.write((staged / name).read_bytes())
    return args.output / (prefix + "-evidence.json")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["measurement", "initial-artifact", "successor-artifact", "trust-policy", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--measurement-sha256", required=True)
    try:
        result = package(parser.parse_args())
        print(json.dumps({"productionEvidence": str(result), "releaseVerified": False,
                          "basis": "archived successful local measurement; packaging performs no new cluster execution"}))
    except Exception:
        print("CNPG cleanup evidence packaging failed; no acceptance is claimed.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
