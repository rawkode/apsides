#!/usr/bin/env python3
"""Measure the pinned author's declared-input and SES capability boundary."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

import run_reproducibility as shared

PROBES = {"declared", "undeclared", "clock", "random", "env", "read", "net"}
REASONS = {"undeclared": "undeclared-source-module", "clock": "clock-unavailable",
           "random": "random-unavailable", "env": "environment-unavailable",
           "read": "filesystem-unavailable", "net": "network-unavailable"}
HELPER = Path(__file__).with_name("isolation_probe.mts")


def run(command, log, repository):
    with log.open("w") as output:
        result = subprocess.run(command, cwd=repository, stdout=output,
                                stderr=subprocess.STDOUT, timeout=180, check=False)
    if log.is_symlink() or log.stat().st_size > shared.MAX_EVIDENCE_BYTES:
        raise shared.QualificationError("isolation transcript exceeds the evidence limit")
    if result.returncode:
        raise shared.QualificationError("isolation step did not establish its required outcome")


def authoring_identity(inputs, expected):
    closure = inputs.get("compilerClosure", {})
    evaluation = inputs.get("evaluation", {})
    if (closure.get("authoringCompiler") != expected
            or evaluation.get("sandbox", {}).get("engine") != "SES"
            or evaluation.get("sandbox", {}).get("moduleLoader") is not False
            or evaluation.get("permissions") != {
                name: "denied" for name in ["read", "write", "net", "env", "run", "ffi", "sys", "import"]
            }):
        raise shared.QualificationError("isolation authoring compiler differs from the measured build")


def baseline_compile(cli, source, destination, artifact, expected_author, repository):
    run([str(cli), "compile", "--source", str(source), "--output", str(destination)],
        destination.with_suffix(".log"), repository)
    if shared.read_json(destination / "ir.json") != shared.read_json(artifact / "ir.json"):
        raise shared.QualificationError("isolation baseline differs from the production IR")
    authoring_identity(shared.read_json(destination / "source/build-inputs.json"), expected_author)


def stage(source, snapshot, destination, probe, definition):
    destination.mkdir()
    for item in snapshot["inputs"]:
        target = destination / item["path"]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source.parent / item["path"], target)
    entry = destination / source.name
    entry.write_text(definition["prefix"] + "\n" + entry.read_text())
    names = [item["path"] for item in snapshot["inputs"]]
    dependency = definition.get("dependency")
    if dependency:
        name = dependency["name"]
        if name != "aps-isolation-dependency.ts" or name in names:
            raise shared.QualificationError("isolation dependency collides with declared source")
        (destination / name).write_text(dependency["source"])
        if probe != "undeclared":
            names.append(name)
    lock = {"schemaVersion": "reakt.dev/source-lock/v1", "entry": source.name,
            "inputs": [{"path": name, "sha256": shared.digest_file(destination / name)[7:]}
                       for name in sorted(names)]}
    (destination / "reakt.lock.json").write_text(
        json.dumps(lock, sort_keys=True, indent=2, ensure_ascii=False) + "\n")
    return entry


def qualify(source, artifact, cli, production_evidence, output):
    source, artifact = source.resolve(), artifact.resolve()
    if not cli.is_absolute() or cli.is_symlink() or not cli.is_file():
        raise shared.QualificationError("isolation CLI must be an absolute regular file")
    repository, head = shared.source_state(source)
    snapshot = shared.source_snapshot(source)
    output = output.resolve()
    if output.exists() or not output.is_relative_to(repository):
        raise shared.QualificationError("isolation output must be a fresh ignored repository path")
    shared.require_ignored(repository, output)
    inputs = shared.read_json(artifact / "build-inputs.json")
    native = inputs.get("compilerClosure", {}).get("nativeCompiler", {})
    identity = shared.build_identity(artifact, snapshot, native.get("buildProfileSha256"))
    previous = shared.read_json(production_evidence)
    for name in ["irDigest", "compilerDigest", "binaryDigest", "sourceInputsDigest", "payloadFiles"]:
        if previous.get(name) != identity[name]:
            raise shared.QualificationError("isolation subject differs from measured production payload")
    image = previous.get("image", {})
    if (image.get("platform") != identity["platform"]
            or image.get("reference", "").rsplit("@", 1)[-1] != identity["imageDigest"]):
        raise shared.QualificationError("isolation subject image differs from measured payload")
    expected_author = inputs["compilerClosure"]["authoringCompiler"]
    sources = [Path(__file__), HELPER, Path(shared.__file__)]
    source_digests = {path.name: shared.digest_file(path) for path in sources}
    if any(path.stat().st_size > shared.MAX_EVIDENCE_BYTES for path in sources):
        raise shared.QualificationError("isolation runner source exceeds the evidence limit")
    output.mkdir(parents=True)
    run([str(cli), "verify", "--artifact", str(artifact)], output / "artifact.log", repository)
    baseline = output / "baseline"
    baseline_compile(cli, source, baseline, artifact, expected_author, repository)
    run(["node", str(HELPER), "--describe"], output / "definitions.json", repository)
    catalogue = shared.read_json(output / "definitions.json")
    definitions = catalogue.get("probes", {})
    if catalogue.get("schemaVersion") != "reakt.dev/isolation-probes/v1" or set(definitions) != PROBES:
        raise shared.QualificationError("isolation helper did not describe the required probes")
    observed = {}
    for probe in ["declared", *sorted(PROBES - {"declared"})]:
        definition = definitions[probe]
        entry = stage(source, snapshot, output / (probe + "-source"), probe, definition)
        compiled = output / (probe + "-compiled")
        transcript = output / (probe + ".json")
        run(["node", str(HELPER), str(entry), str(compiled), probe], transcript, repository)
        result = shared.read_json(transcript)
        if result.get("probe") != probe or result.get("outcome") != ("compiled" if probe == "declared" else "rejected"):
            raise shared.QualificationError("isolation probe did not establish its expected result")
        if probe == "declared":
            authoring_identity(shared.read_json(compiled / "build-inputs.json"), expected_author)
        elif result.get("reason") != REASONS[probe] or (compiled / "ir.json").exists():
            raise shared.QualificationError("isolation rejection reason or output is invalid")
        observed[probe] = result
    baseline_compile(cli, source, output / "baseline-final", artifact, expected_author, repository)
    if (shared.source_state(source) != (repository, head)
            or shared.source_snapshot(source) != snapshot
            or any(shared.digest_file(path) != source_digests[path.name] for path in sources)
            or shared.build_identity(artifact, snapshot, native.get("buildProfileSha256")) != identity):
        raise shared.QualificationError("isolation source, runner or subject changed during measurement")
    evidence = output / "evidence"
    evidence.mkdir()
    for path in sources:
        shutil.copyfile(path, evidence / path.name)
    report = {"schemaVersion": "reakt.dev/isolation-measurement/v1", "probes": observed,
              "sourceCommit": head, "runnerSources": source_digests,
              "scope": "declared-module-closure-and-SES-capability-isolation",
              "limitation": "does not independently qualify operating-system syscall or network isolation"}
    (evidence / "isolation-measurement.json").write_bytes(shared.canonical(report))
    receipt = {name: previous[name] for name in ["irDigest", "compilerDigest", "binaryDigest", "image", "sourceInputsDigest", "payloadFiles"]}
    receipt.update({"schemaVersion": shared.SCHEMA,
                    "runnerSourceFile": "evidence/" + Path(__file__).name,
                    "runnerSourceDigest": source_digests[Path(__file__).name],
                    "checks": {"declared-input-isolation": {
                        "status": "passed", "assertions": [
                            {"checkId": "sandbox-engine", "expected": "SES", "actual": "SES", "passed": True},
                            {"checkId": "undeclared-input-rejected", "expected": True, "actual": True, "passed": True}],
                        "evidenceFiles": ["evidence/isolation-measurement.json", *["evidence/" + path.name for path in sources]]}}})
    receipt_path = evidence / "isolation-evidence.json"
    receipt_path.write_bytes(shared.canonical(receipt))
    return receipt_path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["source", "artifact", "cli", "production-evidence", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    try:
        receipt = qualify(args.source, args.artifact, args.cli, args.production_evidence, args.output)
    except (OSError, shared.QualificationError, subprocess.SubprocessError, ValueError, KeyError, TypeError):
        print("reakt: input isolation qualification failed", file=sys.stderr)
        return 1
    print(json.dumps({"evidence": str(receipt), "check": "declared-input-isolation"}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
