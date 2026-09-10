#!/usr/bin/env python3
"""Measure two independent builds from one clean, unchanged source checkout."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


SCHEMA = "apsides.dev/production-evidence/v1"
MAX_JSON_BYTES = 4 * 1024 * 1024
MAX_EVIDENCE_BYTES = 8 * 1024 * 1024
PAYLOAD_PATHS = {
    "build-inputs.json": "build-inputs.json",
    "controller.oci.tar": "image/controller.oci.tar",
    "apsides-controller": "linux-binary/apsides-controller",
}


class QualificationError(Exception):
    pass


def digest_bytes(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def digest_file(path):
    if path.is_symlink() or not path.is_file():
        raise QualificationError("build output is not a regular file")
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return "sha256:" + value.hexdigest()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise QualificationError("build output JSON contains a duplicate key")
        result[key] = value
    return result


def reject_nonintegral(_):
    raise QualificationError("build output JSON contains a nonintegral number")


def read_json(path):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_JSON_BYTES:
        raise QualificationError("build output JSON is missing or oversized")
    try:
        return json.loads(
            path.read_text(),
            object_pairs_hook=unique_object,
            parse_float=reject_nonintegral,
            parse_constant=reject_nonintegral,
        )
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise QualificationError("build output JSON is invalid") from error


def canonical(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False
    ).encode()


def valid_digest(value):
    return (
        isinstance(value, str)
        and len(value) == 71
        and value.startswith("sha256:")
        and all(character in "0123456789abcdef" for character in value[7:])
    )


def git(repository, *arguments):
    result = subprocess.run(
        ["git", "-C", str(repository), *arguments],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    if result.returncode:
        raise QualificationError("source repository inspection failed")
    return result.stdout


def source_state(source):
    repository = Path(git(source.parent, "rev-parse", "--show-toplevel").strip()).resolve()
    try:
        relative = source.relative_to(repository)
    except ValueError as error:
        raise QualificationError("source is outside its repository") from error
    git(repository, "ls-files", "--error-unmatch", "--", relative.as_posix())
    head = git(repository, "rev-parse", "--verify", "HEAD").strip()
    if not head or git(repository, "status", "--porcelain=v1", "--untracked-files=all"):
        raise QualificationError("source repository is not clean")
    return repository, head


def require_ignored(repository, path):
    result = subprocess.run(
        ["git", "-C", str(repository), "check-ignore", "--quiet", "--no-index", str(path)],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    if result.returncode:
        raise QualificationError("reproducibility output must use an ignored path")


def source_snapshot(source):
    if source.suffix != ".tsx" or source.is_symlink() or not source.is_file():
        raise QualificationError("source must be a regular TSX file")
    lock_path = source.parent / "apsides.lock.json"
    lock_bytes = lock_path.read_bytes()
    lock = read_json(lock_path)
    if set(lock) != {"schemaVersion", "entry", "inputs"} or lock["schemaVersion"] != "apsides.dev/source-lock/v1":
        raise QualificationError("source lock is invalid")
    if lock["entry"] != source.name or not isinstance(lock["inputs"], list) or not lock["inputs"]:
        raise QualificationError("source lock does not select the requested entry")
    paths = []
    locked_inputs = []
    for item in lock["inputs"]:
        if not isinstance(item, dict) or set(item) != {"path", "sha256"}:
            raise QualificationError("source lock input is invalid")
        relative = item["path"]
        if not isinstance(relative, str) or not relative or Path(relative).is_absolute():
            raise QualificationError("source lock input path is invalid")
        path = (source.parent / relative).resolve()
        try:
            path.relative_to(source.parent.resolve())
        except ValueError as error:
            raise QualificationError("source lock input escapes its directory") from error
        expected = item["sha256"]
        if not isinstance(expected, str) or expected != digest_file(path)[7:]:
            raise QualificationError("source lock input digest differs from source bytes")
        paths.append(relative)
        locked_inputs.append({"path": relative, "sha256": expected})
    if paths != sorted(set(paths)) or source.name not in paths:
        raise QualificationError("source lock inputs are not canonical")
    return {
        "entry": source.name,
        "inputs": locked_inputs,
        "lockDigest": digest_bytes(lock_bytes),
    }


def build(cli, source, profile, repository, output, index):
    destination = output / f"build-{index}"
    command = [
        str(cli),
        "build",
        "--source",
        str(source),
        "--output",
        str(destination),
        "--profile",
        str(profile),
        "--cache",
        "disabled",
    ]
    log_path = output / f"build-{index}.log"
    with log_path.open("w") as log:
        log.write(json.dumps({"build": index, "command": command}, separators=(",", ":")) + "\n")
        result = subprocess.run(
            command,
            cwd=repository,
            env=os.environ.copy(),
            stdout=log,
            stderr=subprocess.STDOUT,
            check=False,
        )
    if log_path.is_symlink() or not log_path.is_file() or log_path.stat().st_size > MAX_EVIDENCE_BYTES:
        raise QualificationError(f"independent build {index} transcript is oversized")
    if result.returncode:
        raise QualificationError(f"independent build {index} failed; inspect its transcript")
    return destination


def build_identity(directory, sources, profile_digest):
    result = read_json(directory / "build-result.json")
    metadata = read_json(directory / "image/buildkit-metadata.json")
    inputs = read_json(directory / "build-inputs.json")
    ir = read_json(directory / "ir.json")
    plan = read_json(directory / "plan.json")
    binary = digest_file(directory / "linux-binary/apsides-controller")
    archive = digest_file(directory / "image/controller.oci.tar")
    image_digest = metadata.get("containerimage.digest")
    ir_digest = digest_bytes(canonical(ir))
    closure = inputs.get("compilerClosure")
    compiler_digest = inputs.get("compilerClosureDigest")
    native = closure.get("nativeCompiler") if isinstance(closure, dict) else None
    if (
        result.get("schemaVersion") != "apsides.dev/build-result/v1"
        or result.get("releaseVerified") is not False
        or result.get("binary") != "linux-binary/apsides-controller"
        or result.get("image") != "image/controller.oci.tar"
        or result.get("buildInputs") != "build-inputs.json"
        or result.get("binaryDigest") != binary
        or result.get("irDigest") != ir_digest
        or not isinstance(result.get("platform"), str)
        or ir.get("testOnly") is not False
        or ir.get("provenance", {}).get("inputs") != sources["inputs"]
        or not valid_digest(image_digest)
        or not isinstance(closure, dict)
        or compiler_digest != digest_bytes(canonical(closure))
        or not isinstance(native, dict)
        or native.get("buildProfileSha256") != profile_digest
        or inputs.get("entry") != sources["entry"]
        or inputs.get("inputs") != sources["inputs"]
        or inputs.get("lock", {}).get("sha256") != sources["lockDigest"][7:]
        or plan.get("irDigest") != ir_digest
        or plan.get("ir") != ir
    ):
        raise QualificationError("build artifact identities are internally inconsistent")
    payload = {
        name: digest_file(directory / relative) for name, relative in PAYLOAD_PATHS.items()
    }
    source_inputs_digest = digest_bytes(canonical(inputs))
    # Release assembly rewrites this JSON to compiler-core canonical bytes.
    payload["build-inputs.json"] = source_inputs_digest
    return {
        "irDigest": ir_digest,
        "compiledDigest": digest_bytes(canonical(plan)),
        "compilerDigest": compiler_digest,
        "compilerClosure": closure,
        "sourceInputsDigest": source_inputs_digest,
        "buildInputs": inputs,
        "binaryDigest": binary,
        "imageDigest": image_digest,
        "platform": result["platform"],
        "payloadFiles": payload,
    }


def validate_arguments(source, profile, cli, output, image_repository):
    if not cli.is_absolute() or cli.is_symlink() or not cli.is_file():
        raise QualificationError("CLI must be an absolute regular file")
    if source.is_symlink() or profile.is_symlink():
        raise QualificationError("source and profile must not be symbolic links")
    source = source.resolve()
    profile = profile.resolve()
    output = output.resolve()
    if not profile.is_file():
        raise QualificationError("build profile must be a regular file")
    if output.exists():
        raise QualificationError("output exists; choose a fresh ignored path")
    if (
        not image_repository
        or len(image_repository) > 2048
        or "@" in image_repository
        or any(character.isspace() or ord(character) < 33 or ord(character) > 126 for character in image_repository)
    ):
        raise QualificationError("image repository is invalid")
    source_snapshot(source)
    return source, profile, cli.resolve(), output


def qualify(source, profile, cli, output, image_repository):
    source, profile, cli, output = validate_arguments(
        source, profile, cli, output, image_repository
    )
    repository, head = source_state(source)
    try:
        output.relative_to(repository)
    except ValueError as error:
        raise QualificationError("output must be inside the source repository") from error
    for path in [
        output,
        output / "build-1",
        output / "build-1.source",
        output / "build-1.oci-context",
        output / "build-2",
        output / "build-2.source",
        output / "build-2.oci-context",
    ]:
        require_ignored(repository, path)
    sources = source_snapshot(source)
    profile_bytes = profile.read_bytes()
    profile_digest = digest_bytes(profile_bytes)
    runner_bytes = Path(__file__).read_bytes()
    if len(runner_bytes) > MAX_EVIDENCE_BYTES:
        raise QualificationError("reproducibility runner source is oversized")
    output.mkdir(parents=True)
    runner = output / "run_reproducibility.py"
    runner.write_bytes(runner_bytes)

    first = build(cli, source, profile, repository, output, 1)
    second = build(cli, source, profile, repository, output, 2)
    first_identity = build_identity(first, sources, profile_digest)
    second_identity = build_identity(second, sources, profile_digest)
    if first_identity != second_identity:
        raise QualificationError("independent build identities differ")
    end_repository, end_head = source_state(source)
    if (
        end_repository != repository
        or end_head != head
        or source_snapshot(source) != sources
        or profile.read_bytes() != profile_bytes
        or Path(__file__).read_bytes() != runner_bytes
    ):
        raise QualificationError("source, profile, or runner changed during qualification")

    receipt = {
        "schemaVersion": SCHEMA,
        "irDigest": first_identity["irDigest"],
        "compilerDigest": first_identity["compilerDigest"],
        "binaryDigest": first_identity["binaryDigest"],
        "image": {
            "reference": image_repository + "@" + first_identity["imageDigest"],
            "platform": first_identity["platform"],
        },
        "sourceInputsDigest": first_identity["sourceInputsDigest"],
        "payloadFiles": first_identity["payloadFiles"],
        "runnerSourceFile": "evidence/" + runner.name,
        "runnerSourceDigest": digest_file(runner),
        "checks": {
            "reproducible-payload": {
                "status": "passed",
                "assertions": [
                    {
                        "checkId": "independent-builds",
                        "expected": 2,
                        "actual": 2,
                        "passed": True,
                    },
                    {
                        "checkId": "payload-digests-equal",
                        "expected": True,
                        "actual": True,
                        "passed": True,
                    },
                ],
                "evidenceFiles": ["evidence/build-1.log", "evidence/build-2.log"],
            }
        },
    }
    receipt_bytes = (
        json.dumps(receipt, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    ).encode()
    if len(receipt_bytes) > MAX_EVIDENCE_BYTES:
        raise QualificationError("reproducibility evidence is oversized")
    evidence = output / "evidence"
    evidence.mkdir()
    for name in [runner.name, "build-1.log", "build-2.log"]:
        shutil.copyfile(output / name, evidence / name)
    (evidence / "production-evidence.json").write_bytes(receipt_bytes)
    return receipt


def parse_arguments(arguments=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--image-repository", required=True)
    return parser.parse_args(arguments)


def main(arguments=None):
    options = parse_arguments(arguments)
    try:
        receipt = qualify(
            options.source,
            options.profile,
            options.cli,
            options.output,
            options.image_repository,
        )
    except (OSError, QualificationError, subprocess.SubprocessError):
        print("apsides: reproducibility qualification failed", file=sys.stderr)
        return 1
    print(json.dumps({"evidence": str(options.output / "evidence/production-evidence.json"), "schemaVersion": receipt["schemaVersion"]}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
