#!/usr/bin/env python3
"""Qualify fixture-promotion rejection for one exact production candidate."""
import argparse
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import subprocess
import sys
import tempfile

import run_reproducibility as shared


RESULT_PREFIX = "APS_FIXTURE_PROMOTION_RESULT="
RESULT_SCHEMA = "apsides.dev/fixture-promotion-result/v1"
MEASUREMENT_SCHEMA = "apsides.dev/fixture-promotion-measurement/v1"
MAX_DRIVER_OUTPUT = 16 * 1024
DRIVER = Path(__file__).resolve().parents[1] / "crates/release/tests/fixture_promotion.rs"
PROJECTION = Path(__file__).resolve().parents[1] / "crates/release/src/fixture.rs"
SHARED = Path(shared.__file__).resolve()
SOURCE_ARCHIVES = {
    Path(__file__).resolve(): "fixture-promotion-runner.py",
    SHARED: "fixture-promotion-shared.py",
    DRIVER: "fixture-promotion-driver.rs",
    PROJECTION: "fixture-promotion-projection.rs",
}
FIXTURE_DOCUMENTS = ("ir.json", "fixture-ir.json", "fixture-overlay.json")


def bounded_bytes(path, maximum=shared.MAX_JSON_BYTES):
    if path.is_symlink() or not path.is_file():
        raise shared.QualificationError("fixture input is not a regular file")
    with path.open("rb") as stream:
        value = stream.read(maximum + 1)
    if not value or len(value) > maximum:
        raise shared.QualificationError("fixture input is empty or oversized")
    return value


def strict_json_text(value):
    if len(value.encode()) > MAX_DRIVER_OUTPUT:
        raise shared.QualificationError("fixture driver result is oversized")
    try:
        return json.loads(
            value,
            object_pairs_hook=shared.unique_object,
            parse_float=shared.reject_nonintegral,
            parse_constant=shared.reject_nonintegral,
        )
    except (UnicodeError, json.JSONDecodeError) as error:
        raise shared.QualificationError("fixture driver result is invalid") from error


def strict_document(value):
    try:
        return json.loads(
            value.decode("utf-8"),
            object_pairs_hook=shared.unique_object,
            parse_float=shared.reject_nonintegral,
            parse_constant=shared.reject_nonintegral,
        )
    except (UnicodeError, json.JSONDecodeError) as error:
        raise shared.QualificationError("fixture compiler document is invalid") from error


def result_record(stdout):
    records = [line[len(RESULT_PREFIX):] for line in stdout.splitlines()
               if line.startswith(RESULT_PREFIX)]
    if len(records) != 1:
        raise shared.QualificationError("fixture driver emitted an invalid result count")
    value = strict_json_text(records[0])
    expected = {
        "schemaVersion", "passed",
        "productionReleaseDigest", "productionIrDigest",
        "productionBinaryDigest", "productionImageDigest",
        "fixtureReleaseDigest", "fixtureIrDigest",
        "fixtureBinaryDigest", "fixtureImageDigest",
    }
    if (not isinstance(value, dict) or set(value) != expected
            or value.get("schemaVersion") != RESULT_SCHEMA
            or value.get("passed") is not True
            or any(not shared.valid_digest(value.get(name))
                   for name in expected if name.endswith("Digest"))):
        raise shared.QualificationError("fixture driver result is invalid")
    return value


def image_digest(manifest):
    image = manifest.get("image")
    reference = image.get("reference") if isinstance(image, dict) else None
    if not isinstance(reference, str) or reference.count("@") != 1:
        raise shared.QualificationError("release image identity is invalid")
    digest = reference.rsplit("@", 1)[1]
    if not shared.valid_digest(digest):
        raise shared.QualificationError("release image identity is invalid")
    return digest


def validate_result(value, production_manifest, fixture_manifest,
                    production_release_digest, fixture_release_digest):
    expected = {
        "schemaVersion": RESULT_SCHEMA,
        "passed": True,
        "productionReleaseDigest": production_release_digest,
        "productionIrDigest": production_manifest.get("irDigest"),
        "productionBinaryDigest": production_manifest.get("binaryDigest"),
        "productionImageDigest": image_digest(production_manifest),
        "fixtureReleaseDigest": fixture_release_digest,
        "fixtureIrDigest": fixture_manifest.get("irDigest"),
        "fixtureBinaryDigest": fixture_manifest.get("binaryDigest"),
        "fixtureImageDigest": image_digest(fixture_manifest),
    }
    if value != expected or any(not shared.valid_digest(item)
                                for key, item in expected.items()
                                if key.endswith("Digest")):
        raise shared.QualificationError("fixture driver result refers to another subject")


def compiler_artifact(stdout):
    matches = []
    for line in stdout.splitlines():
        if not line:
            continue
        value = strict_json_text(line)
        if not isinstance(value, dict) or value.get("reason") != "compiler-artifact":
            continue
        target = value.get("target")
        executable = value.get("executable")
        if (isinstance(target, dict) and target.get("name") == "fixture_promotion"
                and target.get("kind") == ["test"] and isinstance(executable, str)):
            matches.append(executable)
    if len(matches) != 1:
        raise shared.QualificationError("fixture driver build did not identify one test binary")
    return Path(matches[0])


def snapshot_files(files):
    if not files or any(not isinstance(role, str) or not role for role in files):
        raise shared.QualificationError("fixture snapshot roles are invalid")
    snapshot = {}
    for role, path in files.items():
        try:
            snapshot[role] = shared.digest_file(path)
        except OSError as error:
            raise shared.QualificationError("fixture snapshot input is unavailable") from error
    return snapshot


def require_unchanged(files, snapshot):
    if set(files) != set(snapshot) or snapshot_files(files) != snapshot:
        raise shared.QualificationError("fixture qualification inputs changed")


def release_manifest(root, mode):
    manifest = shared.read_json(root / "release.json")
    if not isinstance(manifest, dict):
        raise shared.QualificationError("fixture release manifest is invalid")
    files = manifest.get("files")
    source = manifest.get("source")
    image = manifest.get("image")
    if (manifest.get("schemaVersion") != "apsides.dev/release/v1"
            or manifest.get("mode") != mode or not isinstance(files, dict)
            or not files or len(files) > 128 or not isinstance(source, dict)
            or not isinstance(image, dict)
            or not shared.valid_digest(manifest.get("irDigest"))
            or not shared.valid_digest(manifest.get("binaryDigest"))
            or not shared.valid_digest(manifest.get("compilerDigest"))
            or not shared.valid_digest(source.get("inputsDigest"))):
        raise shared.QualificationError("fixture release manifest is invalid")
    image_digest(manifest)
    return manifest


def payload_path(root, name):
    if not isinstance(name, str) or not name or len(name) > 4096:
        raise shared.QualificationError("release payload path is invalid")
    relative = PurePosixPath(name)
    if relative.is_absolute() or any(part in {"", ".", ".."} for part in relative.parts):
        raise shared.QualificationError("release payload path is invalid")
    path = root
    for part in relative.parts:
        path = path / part
        if path.is_symlink():
            raise shared.QualificationError("release payload path is symbolic")
    if not path.is_file():
        raise shared.QualificationError("release payload is missing")
    return path


def release_files(root, manifest, role):
    result = {f"{role}/release.json": root / "release.json"}
    for name, expected in manifest["files"].items():
        if not shared.valid_digest(expected):
            raise shared.QualificationError("release payload digest is invalid")
        path = payload_path(root, name)
        if shared.digest_file(path) != expected:
            raise shared.QualificationError("release payload digest differs from manifest")
        result[f"{role}/{name}"] = path
    for name in ("provenance.json", "provenance.sig", "provenance.sigstore.json"):
        path = root / name
        if path.exists() and f"{role}/{name}" not in result:
            if path.is_symlink() or not path.is_file():
                raise shared.QualificationError("release authentication input is invalid")
            result[f"{role}/{name}"] = path
    return result


def validate_projection(production_artifact, fixture_artifact, fixture_source,
                        production_manifest=None, fixture_manifest=None):
    if production_manifest is None:
        production_manifest = shared.read_json(production_artifact / "release.json")
    if fixture_manifest is None:
        fixture_manifest = shared.read_json(fixture_artifact / "release.json")
    production_files = production_manifest.get("files") if isinstance(production_manifest, dict) else None
    fixture_files = fixture_manifest.get("files") if isinstance(fixture_manifest, dict) else None
    if (not isinstance(production_files, dict) or not isinstance(fixture_files, dict)
            or not all(name in production_files for name in FIXTURE_DOCUMENTS)
            or not all(name in fixture_files for name in FIXTURE_DOCUMENTS)):
        raise shared.QualificationError("release lacks its compiler fixture projection")

    stage_ir = bounded_bytes(fixture_source / "ir.json")
    stage_fixture = bounded_bytes(fixture_source / "fixture-ir.json")
    stage_overlay = bounded_bytes(fixture_source / "fixture-overlay.json")
    production_ir = bounded_bytes(production_artifact / "ir.json")
    production_fixture = bounded_bytes(production_artifact / "fixture-ir.json")
    production_overlay = bounded_bytes(production_artifact / "fixture-overlay.json")
    fixture_ir = bounded_bytes(fixture_artifact / "ir.json")
    fixture_fixture = bounded_bytes(fixture_artifact / "fixture-ir.json")
    fixture_overlay = bounded_bytes(fixture_artifact / "fixture-overlay.json")

    if (shared.canonical(strict_document(stage_ir))
            != shared.canonical(strict_document(production_ir))
            or shared.canonical(strict_document(stage_fixture))
            != shared.canonical(strict_document(fixture_ir))
            or production_fixture != stage_fixture or fixture_fixture != stage_fixture
            or production_overlay != stage_overlay or fixture_overlay != stage_overlay
            or any(shared.digest_bytes(value) != production_files[name]
                   for name, value in zip(FIXTURE_DOCUMENTS,
                                          [production_ir, production_fixture, production_overlay]))
            or any(shared.digest_bytes(value) != fixture_files[name]
                   for name, value in zip(FIXTURE_DOCUMENTS,
                                          [fixture_ir, fixture_fixture, fixture_overlay]))):
        raise shared.QualificationError("fixture artifacts differ from the signed compiler projection")
    return production_manifest, fixture_manifest


def tracked_build_files(repository):
    roots = ["Cargo.lock", "Cargo.toml", "crates/release/Cargo.toml",
             "crates/release/src", "crates/release/build.rs",
             "crates/compiler-core/Cargo.toml", "crates/compiler-core/src",
             "crates/compiler-core/build.rs", ".cargo", "rust-toolchain",
             "rust-toolchain.toml"]
    names = [line for line in shared.git(repository, "ls-files", "--", *roots).splitlines()
             if line]
    required = {"Cargo.lock", "Cargo.toml", "crates/release/Cargo.toml",
                "crates/compiler-core/Cargo.toml"}
    if not required.issubset(names) or not any(
            name.startswith("crates/release/src/") and name.endswith(".rs") for name in names
    ) or not any(
            name.startswith("crates/compiler-core/src/") and name.endswith(".rs") for name in names
    ):
        raise shared.QualificationError("fixture driver source closure is incomplete")
    for relative in ["crates/release/src", "crates/compiler-core/src"]:
        directory = repository / relative
        actual = set()
        for path in directory.rglob("*"):
            if path.is_symlink():
                raise shared.QualificationError("fixture driver source closure is symbolic")
            if path.is_file():
                actual.add(path.relative_to(repository).as_posix())
        tracked = {name for name in names if name.startswith(relative + "/")}
        if actual != tracked:
            raise shared.QualificationError("fixture driver source closure contains untracked files")
    for relative in ["crates/release/build.rs", "crates/compiler-core/build.rs",
                     ".cargo/config", ".cargo/config.toml"]:
        path = repository / relative
        if path.exists() and relative not in names:
            raise shared.QualificationError("fixture driver build configuration is untracked")
    return {f"source/{name}": repository / name for name in names}


def run_captured(command, repository, timeout, environment=None):
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        result = subprocess.run(command, cwd=repository, env=environment, stdout=stdout,
                                stderr=stderr, timeout=timeout, check=False)
        stdout.seek(0, os.SEEK_END)
        size = stdout.tell()
        if result.returncode or size > shared.MAX_EVIDENCE_BYTES:
            raise shared.QualificationError("fixture qualification subprocess failed")
        stdout.seek(0)
        try:
            return stdout.read().decode("utf-8")
        except UnicodeError as error:
            raise shared.QualificationError("fixture subprocess output is invalid") from error


class FixturePromotionRunner:
    def __init__(self, artifact, trust_policy, fixture_artifact, fixture_source, output):
        self.runner = Path(__file__).resolve()
        self.repository, self.commit = shared.source_state(self.runner)
        self.artifact = self.directory(artifact, "production artifact")
        self.fixture_artifact = self.directory(fixture_artifact, "fixture artifact")
        self.fixture_source = self.directory(fixture_source, "fixture source")
        self.trust_policy = self.file(trust_policy, "trust policy")
        self.output = self.output_path(output)
        if (self.output.exists() or not self.output.is_relative_to(self.repository)
                or any(self.overlaps(self.output, path) for path in
                       [self.artifact, self.fixture_artifact, self.fixture_source,
                        self.trust_policy])):
            raise shared.QualificationError("fixture output must be a fresh separate repository path")
        shared.require_ignored(self.repository, self.output)
        if len({self.artifact, self.fixture_artifact, self.fixture_source}) != 3:
            raise shared.QualificationError("fixture inputs must be distinct")

        self.production_manifest = release_manifest(self.artifact, "production")
        self.fixture_manifest = release_manifest(self.fixture_artifact, "fixture")
        validate_projection(self.artifact, self.fixture_artifact, self.fixture_source,
                            self.production_manifest, self.fixture_manifest)
        self.production_release_digest = shared.digest_file(self.artifact / "release.json")
        self.fixture_release_digest = shared.digest_file(self.fixture_artifact / "release.json")
        self.files = {}
        self.files.update(release_files(self.artifact, self.production_manifest, "production"))
        self.files.update(release_files(self.fixture_artifact, self.fixture_manifest, "fixture"))
        self.files["trust/policy"] = self.trust_policy
        self.files.update({f"stage/{name}": self.fixture_source / name
                           for name in FIXTURE_DOCUMENTS})
        self.build_files = tracked_build_files(self.repository)
        self.files.update(self.build_files)
        for path, archive in SOURCE_ARCHIVES.items():
            self.files[f"producer/{archive}"] = path
        self.snapshot = snapshot_files(self.files)
        self.source_digests = {archive: shared.digest_file(path)
                               for path, archive in SOURCE_ARCHIVES.items()}
        self.result = None
        self.driver_binary_digest = None

    @staticmethod
    def directory(path, label):
        if (not path.is_absolute() or path != path.resolve()
                or path.is_symlink() or not path.is_dir()):
            raise shared.QualificationError(f"fixture {label} is invalid")
        return path

    @staticmethod
    def file(path, label):
        if (not path.is_absolute() or path != path.resolve()
                or path.is_symlink() or not path.is_file()):
            raise shared.QualificationError(f"fixture {label} is invalid")
        return path

    @staticmethod
    def output_path(path):
        if not path.is_absolute() or path != path.resolve() or path.is_symlink():
            raise shared.QualificationError("fixture output path is invalid")
        return path

    @staticmethod
    def overlaps(left, right):
        if right.is_dir():
            return left == right or left.is_relative_to(right) or right.is_relative_to(left)
        return left == right or right.is_relative_to(left)

    def require_inputs_unchanged(self):
        if shared.source_state(self.runner) != (self.repository, self.commit):
            raise shared.QualificationError("fixture producer source changed")
        current_build_files = tracked_build_files(self.repository)
        if current_build_files != self.build_files:
            raise shared.QualificationError("fixture driver source closure changed")
        require_unchanged(self.files, self.snapshot)
        production, fixture = validate_projection(
            self.artifact, self.fixture_artifact, self.fixture_source)
        if production != self.production_manifest or fixture != self.fixture_manifest:
            raise shared.QualificationError("fixture release manifests changed")

    def final_check(self):
        self.require_inputs_unchanged()
        if self.result is None:
            raise shared.QualificationError("fixture driver result is absent")
        validate_result(self.result, self.production_manifest, self.fixture_manifest,
                        self.production_release_digest, self.fixture_release_digest)

    def run_driver(self):
        self.output.mkdir(parents=True)
        cargo_stdout = run_captured(
            ["cargo", "test", "--locked", "--offline", "-p", "release", "--test",
             "fixture_promotion", "--no-run", "--message-format=json"],
            self.repository, 600)
        self.require_inputs_unchanged()
        executable = compiler_artifact(cargo_stdout)
        if not executable.is_absolute() or executable.is_symlink() or not executable.is_file():
            raise shared.QualificationError("fixture driver executable is invalid")
        executable_digest = shared.digest_file(executable)
        copied = self.output / "fixture-promotion-driver"
        shutil.copy2(executable, copied)
        copied.chmod(copied.stat().st_mode | stat.S_IXUSR)
        if shared.digest_file(copied) != executable_digest:
            raise shared.QualificationError("fixture driver snapshot changed")
        self.files["executable/fixture-promotion-driver"] = copied
        self.snapshot["executable/fixture-promotion-driver"] = executable_digest
        self.require_inputs_unchanged()
        self.driver_binary_digest = executable_digest
        environment = {
            "APS_FIXTURE_PRODUCTION_ARTIFACT": str(self.artifact),
            "APS_FIXTURE_ARTIFACT": str(self.fixture_artifact),
            "APS_FIXTURE_TRUST_POLICY": str(self.trust_policy),
            "RUST_BACKTRACE": "0",
            "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
        }
        stdout = run_captured(
            [str(copied), "--ignored", "--exact", "fixture_promotion_step", "--nocapture"],
            self.repository, 180, environment)
        if len(stdout.encode()) > MAX_DRIVER_OUTPUT:
            raise shared.QualificationError("fixture driver result is oversized")
        self.result = result_record(stdout)
        validate_result(self.result, self.production_manifest, self.fixture_manifest,
                        self.production_release_digest, self.fixture_release_digest)
        self.require_inputs_unchanged()

    def publish(self):
        self.final_check()
        if not shared.valid_digest(self.driver_binary_digest):
            raise shared.QualificationError("fixture driver binary identity is absent")
        measurement = {
            "schemaVersion": MEASUREMENT_SCHEMA,
            "scope": "offline-exact-fixture-release-production-promotion-rejection",
            "productionSubject": {
                "releaseDigest": self.production_release_digest,
                "irDigest": self.production_manifest["irDigest"],
                "binaryDigest": self.production_manifest["binaryDigest"],
                "image": self.production_manifest["image"],
            },
            "fixtureSubject": {
                "releaseDigest": self.fixture_release_digest,
                "irDigest": self.fixture_manifest["irDigest"],
                "binaryDigest": self.fixture_manifest["binaryDigest"],
                "image": self.fixture_manifest["image"],
            },
            "driverResult": self.result,
            "driverBinaryDigest": self.driver_binary_digest,
            "trustPolicyDigest": self.snapshot["trust/policy"],
            "runnerSourceCommit": self.commit,
            "runnerSources": self.source_digests,
            "sourceArchiveFiles": {
                path.relative_to(self.repository).as_posix(): archive
                for path, archive in SOURCE_ARCHIVES.items()
            },
            "buildSourceFiles": {
                role.removeprefix("source/"): digest
                for role, digest in self.snapshot.items() if role.startswith("source/")
            },
            "releaseVerified": False,
            "limitation": "offline release integrity, signature, compiler projection and rejection only; no deployment or runtime qualification",
        }
        receipt = {
            "schemaVersion": shared.SCHEMA,
            "irDigest": self.production_manifest["irDigest"],
            "compilerDigest": self.production_manifest["compilerDigest"],
            "binaryDigest": self.production_manifest["binaryDigest"],
            "image": self.production_manifest["image"],
            "sourceInputsDigest": self.production_manifest["source"]["inputsDigest"],
            "payloadFiles": {name: self.production_manifest["files"][name] for name in
                             ["build-inputs.json", "controller.oci.tar", "apsides-controller"]},
            "runnerSourceFile": "evidence/fixture-promotion-runner.py",
            "runnerSourceDigest": self.source_digests["fixture-promotion-runner.py"],
            "checks": {"fixture-promotion-rejection": {
                "status": "passed",
                "assertions": [{"checkId": "fixture-release-rejected", "expected": True,
                                "actual": True, "passed": True}],
                "evidenceFiles": ["evidence/fixture-promotion-measurement.json",
                                  *["evidence/" + name for name in SOURCE_ARCHIVES.values()]],
            }},
        }
        measurement_bytes = shared.canonical(measurement)
        receipt_bytes = shared.canonical(receipt)
        if max(len(measurement_bytes), len(receipt_bytes)) > shared.MAX_EVIDENCE_BYTES:
            raise shared.QualificationError("fixture promotion evidence is oversized")

        evidence = self.output / "evidence"
        if evidence.exists():
            raise shared.QualificationError("fixture evidence publication path exists")
        with tempfile.TemporaryDirectory(prefix=".fixture-promotion-evidence-",
                                         dir=self.output) as temporary:
            staging = Path(temporary)
            for source, archive in SOURCE_ARCHIVES.items():
                shutil.copyfile(source, staging / archive)
                if shared.digest_file(staging / archive) != self.source_digests[archive]:
                    raise shared.QualificationError("fixture evidence source archive changed")
            (staging / "fixture-promotion-measurement.json").write_bytes(measurement_bytes)
            (staging / "fixture-promotion-evidence.json").write_bytes(receipt_bytes)
            self.final_check()
            staging.rename(evidence)
        return evidence / "fixture-promotion-evidence.json"


def parse_arguments(arguments=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--trust-policy", type=Path, required=True)
    parser.add_argument("--fixture-artifact", type=Path, required=True)
    parser.add_argument("--fixture-source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args(arguments)


def main(arguments=None):
    try:
        options = parse_arguments(arguments)
        runner = FixturePromotionRunner(options.artifact, options.trust_policy,
                                        options.fixture_artifact, options.fixture_source,
                                        options.output)
        runner.run_driver()
        receipt = runner.publish()
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError,
            shared.QualificationError):
        print("apsides: fixture promotion qualification failed", file=sys.stderr)
        return 1
    print(json.dumps({"check": "fixture-promotion-rejection", "evidence": str(receipt)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
