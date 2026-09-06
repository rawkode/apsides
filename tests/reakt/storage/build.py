#!/usr/bin/env python3
"""Build a disposable storage PVC probe image from cached inputs only."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[3]
SCRIPT = Path(__file__).resolve()
DOCKERFILE = SCRIPT.with_name("Dockerfile")
CRASH_WRAPPER = SCRIPT.with_name("crash.sh")
BUILDER = "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922"
PLATFORM = "linux/arm64"
TARGET_NAME = "pvc_restart"
PROFILES = {
    "inventory": {
        "context_schema": "reakt.dev/inventory-pvc-build-context/v1",
        "build_schema": "reakt.dev/inventory-pvc-test-image/v1",
        "archive": "inventory-pvc.oci.tar",
        "executable": "inventory-pvc-probe",
        "dependencies": {"compiler-core": {"path": "../compiler-core"}},
    },
    "coordination": {
        "context_schema": "reakt.dev/coordination-pvc-build-context/v1",
        "build_schema": "reakt.dev/coordination-pvc-test-image/v1",
        "archive": "coordination-pvc.oci.tar",
        "executable": "coordination-pvc-probe",
        "dependencies": {
            "compiler-core": {"path": "../compiler-core"},
            "deploy": {"path": "../deploy"},
        },
        "dev-dependencies": {"inventory": {"path": "../inventory"}},
    },
}


def profile(package):
    try:
        return PROFILES[package]
    except (KeyError, TypeError) as error:
        raise ValueError("unsupported storage probe package") from error

sys.path.insert(0, str(ROOT / "scripts/reakt"))
from package_oci import file_manifest, pinned_image, read_regular, sha256  # noqa: E402


TARGET_SELECTOR = r'''#!/usr/bin/env python3
"""Select one exact Cargo integration-test artifact from JSON messages."""
import json
import os
from pathlib import Path, PurePosixPath
import re
import sys

if len(sys.argv) != 2:
    raise SystemExit("cargo artifact input required")
source = Path(sys.argv[1])
if source.is_symlink() or not source.is_file() or source.stat().st_size > 16 * 1024 * 1024:
    raise SystemExit("cargo artifact input is invalid")
try:
    rows = [json.loads(line) for line in source.read_text().splitlines() if line]
except (OSError, UnicodeError, json.JSONDecodeError):
    raise SystemExit("cargo artifact JSON is invalid") from None
matches = [
    row for row in rows
    if row.get("reason") == "compiler-artifact"
    and row.get("target", {}).get("name") == "pvc_restart"
    and row.get("target", {}).get("kind") == ["test"]
    and row.get("target", {}).get("crate_types") == ["bin"]
    and row.get("profile", {}).get("test") is True
]
if len(matches) != 1:
    raise SystemExit("exactly one PVC integration-test artifact is required")
executable = matches[0].get("executable")
path = PurePosixPath(executable) if isinstance(executable, str) else None
if (
    path is None
    or path.parent != PurePosixPath("/build/target/release/deps")
    or re.fullmatch(r"pvc_restart-[0-9a-f]+", path.name) is None
    or not Path(executable).is_file()
    or not os.access(executable, os.X_OK)
):
    raise SystemExit("PVC integration-test executable is invalid")
print(executable)
'''


def run(argv, *, cwd=None, timeout=300):
    result = subprocess.run(
        [str(value) for value in argv],
        cwd=cwd,
        check=False,
        text=True,
        capture_output=True,
        timeout=timeout,
    )
    if result.returncode:
        raise RuntimeError("required cached build input or tool invocation failed")
    return result.stdout


def copy_regular(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(read_regular(source))


def copy_tree(source, destination):
    if source.is_symlink() or not source.is_dir():
        raise ValueError("source closure directory is missing or is a symlink")
    for directory, names, files in os.walk(source, followlinks=False):
        here = Path(directory)
        if here.is_symlink() or any((here / name).is_symlink() for name in names):
            raise ValueError("source closure contains a symlink directory")
        if any(name in {"target", "__pycache__", ".git", ".cargo"} for name in names):
            raise ValueError("source closure contains an external build directory")
        for name in files:
            copy_regular(here / name, destination / (here / name).relative_to(source))


def validate_repository(repository, package="inventory"):
    selected = profile(package)
    repository = repository.resolve()
    root = tomllib.loads(read_regular(repository / "Cargo.toml").decode())
    if root.get("workspace", {}).get("resolver") != "3":
        raise ValueError("workspace build contract changed")
    members = root.get("workspace", {}).get("members", [])
    if (
        not isinstance(members, list)
        or not all(isinstance(member, str) and re.fullmatch(r"crates/[a-z][a-z0-9-]*", member) for member in members)
        or len(members) != len(set(members))
        or not all(member in members for member in ["crates/compiler-core", f"crates/{package}"])
    ):
        raise ValueError("storage source closure is not a workspace member")
    manifests = {}
    for member in members:
        directory = (repository / member).resolve()
        if directory.parent != repository / "crates":
            raise ValueError("workspace member escapes the trusted crate directory")
        manifests[member] = tomllib.loads(read_regular(directory / "Cargo.toml").decode())
    package_manifest = manifests[f"crates/{package}"]
    local_dependencies = {}
    dependency_tables = [("root", package_manifest)]
    dependency_tables.extend(
        (f"target.{target}", table)
        for target, table in package_manifest.get("target", {}).items()
    )
    for table_name, table in dependency_tables:
        for section in ["dependencies", "dev-dependencies", "build-dependencies"]:
            for name, value in table.get(section, {}).items():
                if isinstance(value, dict) and "path" in value:
                    local_dependencies[(table_name, section, name)] = value
    expected_dependencies = {
        ("root", "dependencies", name): value
        for name, value in selected["dependencies"].items()
    }
    expected_dependencies.update({
        ("root", "dev-dependencies", name): value
        for name, value in selected.get("dev-dependencies", {}).items()
    })
    if local_dependencies != expected_dependencies:
        raise ValueError("storage local dependency closure changed")
    member_directories = {(repository / member).resolve() for member in members}
    for member, manifest in manifests.items():
        tables = [manifest, *manifest.get("target", {}).values()]
        for table in tables:
            for section in ["dependencies", "dev-dependencies", "build-dependencies"]:
                for value in table.get(section, {}).values():
                    if isinstance(value, dict) and "path" in value:
                        dependency_directory = ((repository / member) / value["path"]).resolve()
                        if dependency_directory not in member_directories:
                            raise ValueError("source dependency escapes the exact workspace")
    if not (repository / f"crates/{package}/tests/{TARGET_NAME}.rs").is_file():
        raise ValueError("PVC integration test source is missing")
    return repository, members


def prepare_context(repository, context, builder, package="inventory"):
    selected = profile(package)
    repository, members = validate_repository(repository, package)
    context.mkdir()
    copy_regular(repository / "Cargo.toml", context / "Cargo.toml")
    copy_regular(repository / "Cargo.lock", context / "Cargo.lock")
    for member in members:
        copy_regular(repository / member / "Cargo.toml", context / member / "Cargo.toml")
        copy_tree(repository / member / "src", context / member / "src")
        build_script = repository / member / "build.rs"
        if build_script.exists() or build_script.is_symlink():
            copy_regular(build_script, context / member / "build.rs")
    copy_tree(repository / f"crates/{package}/tests", context / f"crates/{package}/tests")
    copy_regular(DOCKERFILE, context / "Dockerfile")
    copy_regular(CRASH_WRAPPER, context / "storage-crash")
    copy_regular(SCRIPT, context / "build.py")
    (context / "select-target.py").write_text(TARGET_SELECTOR)
    (context / ".dockerignore").write_text("context.json\n")

    lock = read_regular(context / "Cargo.lock")
    vendor_config = run(
        [
            "cargo",
            "vendor",
            "--locked",
            "--offline",
            "--versioned-dirs",
            "--manifest-path",
            "Cargo.toml",
            "vendor",
        ],
        cwd=context,
    )
    parsed = tomllib.loads(vendor_config)
    if parsed.get("source", {}).get("vendored-sources", {}).get("directory") != "vendor":
        raise ValueError("Cargo returned an unexpected vendor configuration")
    (context / ".cargo").mkdir()
    (context / ".cargo/config.toml").write_text(vendor_config)
    if read_regular(context / "Cargo.lock") != lock:
        raise ValueError("offline dependency preparation changed Cargo.lock")
    run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--manifest-path",
            "Cargo.toml",
        ],
        cwd=context,
    )
    metadata = {
        "schemaVersion": selected["context_schema"],
        "builder": builder,
        "platform": PLATFORM,
        "target": {"package": package, "test": TARGET_NAME},
        "cargoLockDigest": sha256(lock),
        "files": file_manifest(context),
        "qualification": "test-only; no production or release authority",
    }
    (context / "context.json").write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n")
    return metadata


def verify_context(context, expected, package="inventory"):
    selected = profile(package)
    encoded = read_regular(context / "context.json")
    actual = json.loads(encoded)
    if actual != expected or actual.get("schemaVersion") != selected["context_schema"]:
        raise ValueError("prepared storage build context metadata changed")
    if actual.get("qualification") != "test-only; no production or release authority":
        raise ValueError("prepared storage build context has an invalid claim")
    if file_manifest(context) != actual.get("files"):
        raise ValueError("prepared storage build context files changed")
    return encoded


def archive_json(archive, name):
    try:
        member = archive.getmember(name)
        if not member.isfile() or member.size > 1024 * 1024:
            raise ValueError("OCI metadata member is invalid")
        stream = archive.extractfile(member)
        if stream is None:
            raise ValueError("OCI metadata member is missing")
        return json.loads(stream.read())
    except (KeyError, OSError, UnicodeError, json.JSONDecodeError, tarfile.TarError) as error:
        raise ValueError("OCI metadata is invalid") from error


def archive_blob(archive, digest):
    if not isinstance(digest, str) or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None:
        raise ValueError("OCI descriptor digest is invalid")
    name = "blobs/sha256/" + digest.removeprefix("sha256:")
    try:
        member = archive.getmember(name)
        if not member.isfile() or member.size > 1024 * 1024:
            raise ValueError("OCI metadata blob is invalid")
        stream = archive.extractfile(member)
        if stream is None:
            raise ValueError("OCI metadata blob is missing")
        value = stream.read()
    except (KeyError, OSError, tarfile.TarError) as error:
        raise ValueError("OCI metadata blob is invalid") from error
    if sha256(value) != digest:
        raise ValueError("OCI metadata blob digest differs")
    return value


def inspect_oci_archive(path, package="inventory"):
    selected = profile(package)
    try:
        with tarfile.open(path) as archive:
            index = archive_json(archive, "index.json")
            manifests = index.get("manifests") if isinstance(index, dict) else None
            if not isinstance(manifests, list) or len(manifests) != 1:
                raise ValueError("OCI archive must contain one platform manifest")
            descriptor = manifests[0]
            if not isinstance(descriptor, dict) or descriptor.get("platform") != {"architecture": "arm64", "os": "linux"}:
                raise ValueError("OCI archive platform differs")
            manifest_digest = descriptor.get("digest")
            manifest_bytes = archive_blob(archive, manifest_digest)
            if descriptor.get("size") != len(manifest_bytes):
                raise ValueError("OCI platform manifest size differs")
            manifest = json.loads(manifest_bytes)
            config_descriptor = manifest.get("config") if isinstance(manifest, dict) else None
            if not isinstance(config_descriptor, dict):
                raise ValueError("OCI image configuration is missing")
            config_digest = config_descriptor.get("digest")
            config_bytes = archive_blob(archive, config_digest)
            if config_descriptor.get("size") != len(config_bytes):
                raise ValueError("OCI image configuration size differs")
            config = json.loads(config_bytes)
    except (OSError, UnicodeError, json.JSONDecodeError, tarfile.TarError) as error:
        raise ValueError("OCI archive is invalid") from error
    if not isinstance(config, dict):
        raise ValueError("OCI image configuration is invalid")
    runtime = config.get("config")
    if (
        config.get("architecture") != "arm64"
        or config.get("os") != "linux"
        or not isinstance(runtime, dict)
        or runtime.get("User") != "65532:65532"
        or runtime.get("Entrypoint") != [f"/usr/local/bin/{selected['executable']}"]
    ):
        raise ValueError("OCI runtime identity differs")
    return manifest_digest, config_digest


def build(repository, output, package="inventory"):
    selected = profile(package)
    repository, _ = validate_repository(repository, package)
    output = output.absolute()
    work = (repository / "work").resolve()
    try:
        output.resolve().relative_to(work)
    except ValueError as error:
        raise ValueError("storage test image output must be under the ignored work directory") from error
    if output.exists() or output.is_symlink():
        raise ValueError("storage test image output already exists")
    builder = pinned_image(BUILDER)
    if builder.get("reference") != BUILDER or builder.get("platform") != PLATFORM:
        raise ValueError("the exact cached Linux ARM64 builder is required")

    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{package}-pvc-image-", dir=output.parent))
    try:
        context = staging / "context"
        prepared = prepare_context(repository, context, builder, package)
        context_bytes = verify_context(context, prepared, package)
        if pinned_image(BUILDER) != builder:
            raise ValueError("cached builder identity changed during preparation")
        archive = staging / selected["archive"]
        buildkit = staging / "buildkit-metadata.json"
        argv = [
            "docker",
            "buildx",
            "build",
            "--pull=false",
            "--network=none",
            "--provenance=false",
            "--sbom=false",
            "--platform",
            PLATFORM,
            "--target",
            package,
            "--build-arg",
            f"BUILDER_IMAGE={BUILDER}",
            "--build-arg",
            f"PACKAGE={package}",
            "--build-arg",
            "SOURCE_DATE_EPOCH=0",
            "--metadata-file",
            str(buildkit),
            "--output",
            f"type=oci,rewrite-timestamp=true,dest={archive}",
            str(context),
        ]
        subprocess.run(argv, check=True)
        if verify_context(context, prepared, package) != context_bytes:
            raise ValueError("storage build context changed during BuildKit execution")
        image_metadata = json.loads(read_regular(buildkit))
        image_digest = image_metadata.get("containerimage.digest")
        if not isinstance(image_digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", image_digest):
            raise ValueError("BuildKit did not report one canonical image digest")
        archive_bytes = read_regular(archive)
        platform_manifest_digest, config_digest = inspect_oci_archive(archive, package)
        receipt = {
            "schemaVersion": selected["build_schema"],
            "builder": builder,
            "platform": PLATFORM,
            "target": {"package": package, "test": TARGET_NAME},
            "contextDigest": sha256(context_bytes),
            "imageDigest": image_digest,
            "platformManifestDigest": platform_manifest_digest,
            "configDigest": config_digest,
            "archive": {"path": selected["archive"], "sha256": sha256(archive_bytes)},
            "qualification": "test-only; not a production release artifact",
        }
        (staging / "build.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
        staging.rename(output)
        return receipt
    except BaseException:
        shutil.rmtree(staging)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", choices=sorted(PROFILES), default="inventory")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = build(ROOT, args.output, args.package)
    print(json.dumps({"output": str(args.output.absolute()), "imageDigest": result["imageDigest"], "qualification": result["qualification"]}, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except Exception:
        raise SystemExit("reakt: storage test image build failed") from None
