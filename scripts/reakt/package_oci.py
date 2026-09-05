#!/usr/bin/env python3
"""Glue around Cargo vendor and Docker BuildKit; never pulls images or publishes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]
BUILDER = "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922"
CONTEXT_SCHEMA = "reakt.dev/oci-context/v1"


def command(argv, cwd=None, timeout=180):
    result = subprocess.run(argv, cwd=cwd, check=False, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} {argv[1]} failed: {result.stderr[-3000:]}")
    return result.stdout


def sha256(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def read_regular(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"expected a regular non-symlink file: {path}")
    return path.read_bytes()


def copy_file(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(read_regular(source))


def copy_source(source, destination):
    for directory, dirs, files in os.walk(source, followlinks=False):
        here = Path(directory)
        if here.is_symlink() or any((here / name).is_symlink() for name in dirs):
            raise ValueError("source snapshots must not contain symlink directories")
        for name in files:
            copy_file(here / name, destination / (here / name).relative_to(source))


def pinned_image(image):
    if not re.fullmatch(r"[a-z0-9][a-z0-9./:_-]*@sha256:[0-9a-f]{64}", image):
        raise ValueError("image must be an explicit repository@sha256 digest")
    details = json.loads(command(["docker", "image", "inspect", image]))
    if len(details) != 1 or details[0].get("Os") != "linux":
        raise ValueError("one locally cached Linux image is required")
    detail = details[0]
    if not any(ref.rsplit("@", 1)[-1] == image.rsplit("@", 1)[-1] for ref in detail.get("RepoDigests", [])):
        raise ValueError("local image inspection does not confirm the requested repository digest")
    return {"reference": image, "platform": f"linux/{detail['Architecture']}", "localImageId": detail["Id"]}


def file_manifest(root):
    files = {}
    for directory, dirs, names in os.walk(root, followlinks=False):
        here = Path(directory)
        if any((here / name).is_symlink() for name in dirs):
            raise ValueError("prepared context contains a symlink directory")
        for name in names:
            path = here / name
            relative = path.relative_to(root).as_posix()
            if relative == "context.json":
                continue
            files[relative] = sha256(read_regular(path))
    return dict(sorted(files.items()))


def relocated_manifest(original, repository):
    parsed = tomllib.loads(original.decode())
    expected_path = str((repository / "crates/runtime").resolve())
    expected = {
        "package": {"name": "controller", "version": "0.1.0", "edition": "2024"},
        "workspace": {},
        "dependencies": {
            "runtime": {"path": expected_path},
            "tokio": {"version": "1", "features": ["macros", "rt-multi-thread"]},
        },
    }
    if parsed != expected:
        raise ValueError("generated Cargo.toml differs from the supported trusted runtime dependency contract")
    old = json.dumps(expected_path)
    text = original.decode()
    if text.count(old) != 1:
        raise ValueError("generated runtime path cannot be relocated unambiguously")
    return text.replace(old, '"../workspace/crates/runtime"').encode()


def local_crates(repository):
    """Copy the declared local Cargo closure, including optional path manifests."""
    pending = [repository / "crates/runtime", repository / "crates/compiler-core"]
    seen = set()
    while pending:
        directory = pending.pop().resolve()
        if directory in seen:
            continue
        if directory.parent != repository / "crates" or directory.name not in {"compiler-core", "runtime", "deploy", "platform", "inventory", "release", "cli"}:
            raise ValueError("runtime path dependency escapes the trusted Reakt crate closure")
        seen.add(directory)
        manifest = tomllib.loads(read_regular(directory / "Cargo.toml").decode())
        tables = [manifest]
        tables.extend(manifest.get("target", {}).values())
        for table in tables:
            for section in ["dependencies", "build-dependencies", "dev-dependencies"]:
                for dependency in table.get(section, {}).values():
                    if isinstance(dependency, dict) and "path" in dependency:
                        pending.append(directory / dependency["path"])
    return sorted(path.name for path in seen)


def prepare(artifact, output, repository=ROOT, builder_image=BUILDER, source_date_epoch=0):
    if not isinstance(source_date_epoch, int) or not 0 <= source_date_epoch <= 253402300799:
        raise ValueError("source date epoch must be a nonnegative supported Unix timestamp")
    artifact, output, repository = artifact.resolve(), output.absolute(), repository.resolve()
    if output.exists():
        raise ValueError("output already exists; choose a fresh build-context directory")
    if output == artifact or artifact in output.parents:
        raise ValueError("build context must be outside the input artifact")
    # Reuse the trusted IR/artifact validator. Packaging adds no approval semantics.
    command([os.environ.get("REAKT_CLI", str(repository / "target/debug/reakt")), "verify", "--artifact", str(artifact)])
    manifest_bytes = read_regular(artifact / "manifest.json")
    manifest = json.loads(manifest_bytes)
    lock = read_regular(artifact / "controller/Cargo.lock")
    if manifest["files"].get("controller/Cargo.lock") != sha256(lock):
        raise ValueError("artifact must contain a recorded exact controller/Cargo.lock; run the native build first")
    builder = pinned_image(builder_image)
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".reakt-oci-", dir=output.parent))
    try:
        copy_source(artifact / "controller/src", staging / "controller/src")
        (staging / "controller/Cargo.toml").write_bytes(relocated_manifest(read_regular(artifact / "controller/Cargo.toml"), repository))
        (staging / "controller/Cargo.lock").write_bytes(lock)
        copy_file(repository / "Cargo.toml", staging / "workspace/Cargo.toml")
        for crate in local_crates(repository):
            copy_file(repository / f"crates/{crate}/Cargo.toml", staging / f"workspace/crates/{crate}/Cargo.toml")
            copy_source(repository / f"crates/{crate}/src", staging / f"workspace/crates/{crate}/src")
        copy_file(ROOT / "scripts/reakt/Dockerfile", staging / "Dockerfile")
        (staging / ".dockerignore").write_text("context.json\n")
        cargo_config = command(["cargo", "vendor", "--locked", "--offline", "--versioned-dirs", "--manifest-path", "controller/Cargo.toml", "vendor"], cwd=staging)
        parsed_config = tomllib.loads(cargo_config)
        if parsed_config.get("source", {}).get("vendored-sources", {}).get("directory") != "vendor":
            raise ValueError("cargo vendor returned an unexpected source directory")
        (staging / ".cargo").mkdir()
        (staging / ".cargo/config.toml").write_text(cargo_config)
        if (staging / "controller/Cargo.lock").read_bytes() != lock:
            raise ValueError("locked dependency preparation changed Cargo.lock")
        command(["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--manifest-path", "controller/Cargo.toml"], cwd=staging)
        metadata = {
            "schemaVersion": CONTEXT_SCHEMA,
            "irDigest": manifest["irDigest"],
            "sourceArtifactManifestDigest": sha256(manifest_bytes),
            "controllerLockDigest": sha256(lock),
            "sourceDateEpoch": source_date_epoch,
            "builder": builder,
            "runtimeImage": None,
            "releaseStatus": "blocked",
            "verification": "local-integrity-only",
            "files": file_manifest(staging),
        }
        (staging / "context.json").write_text(json.dumps(metadata, sort_keys=True, indent=2) + "\n")
        staging.rename(output)
        return metadata
    except BaseException:
        shutil.rmtree(staging)
        raise


def verify_context(context):
    context = context.resolve()
    metadata = json.loads(read_regular(context / "context.json"))
    if metadata.get("schemaVersion") != CONTEXT_SCHEMA or metadata.get("releaseStatus") != "blocked":
        raise ValueError("unsupported context or false release claim")
    if file_manifest(context) != metadata["files"]:
        raise ValueError("prepared context file set or contents changed")
    if sha256(read_regular(context / "controller/Cargo.lock")) != metadata["controllerLockDigest"]:
        raise ValueError("controller lock identity changed")
    checked_ir = json.loads(command([os.environ.get("REAKT_CLI", str(ROOT / "target/debug/reakt")), "check", "--ir", str(context / "controller/src/ir.json")]))
    if checked_ir.get("status") != "valid" or checked_ir.get("irDigest") != metadata["irDigest"]:
        raise ValueError("context IR digest differs from the validated embedded IR")
    return metadata


def build(context, runtime_image, output, no_cache=False):
    context, output = context.resolve(), output.resolve()
    if output == context or context in output.parents:
        raise ValueError("OCI output must be outside the prepared context")
    context_snapshot = read_regular(context / "context.json")
    metadata = verify_context(context)
    if read_regular(context / "context.json") != context_snapshot:
        raise ValueError("context metadata changed during verification")
    builder = pinned_image(metadata["builder"]["reference"])
    runtime = pinned_image(runtime_image)
    if builder != metadata["builder"] or builder["platform"] != runtime["platform"]:
        raise ValueError("cached builder changed or runtime platform differs from the prepared builder")
    if output.exists():
        raise ValueError("OCI output directory already exists")
    output.mkdir(parents=True)
    # --network none disables RUN networking. This is not an assertion about
    # BuildKit's own metadata resolution; whole-process egress needs an external gate.
    argv = ["docker", "buildx", "build", "--pull=false", "--network=none", "--provenance=false", "--sbom=false", "--platform", builder["platform"],
            "--build-arg", f"SOURCE_DATE_EPOCH={metadata['sourceDateEpoch']}",
            "--build-arg", f"BUILDER_IMAGE={builder['reference']}",
            "--build-arg", f"RUNTIME_IMAGE={runtime['reference']}",
            "--build-arg", f"IR_DIGEST={metadata['irDigest']}",
            "--metadata-file", str(output / "buildkit-metadata.json"),
            "--output", f"type=oci,rewrite-timestamp=true,dest={output / 'controller.oci.tar'}", str(context)]
    if no_cache:
        argv.insert(3, "--no-cache")
    # Build logs remain visible; callers explicitly request this potentially long step.
    subprocess.run(argv, check=True)
    verify_context(context)
    if read_regular(context / "context.json") != context_snapshot:
        raise ValueError("context metadata changed during the build; no packaging receipt published")
    if not (output / "controller.oci.tar").is_file() or not (output / "buildkit-metadata.json").is_file():
        raise ValueError("BuildKit did not produce the requested OCI archive and metadata")
    (output / "packaging-inputs.json").write_text(json.dumps({
        "contextDigest": sha256(context_snapshot),
        "builder": builder, "runtime": runtime, "irDigest": metadata["irDigest"],
        "sourceDateEpoch": metadata["sourceDateEpoch"],
        "releaseStatus": "blocked", "verification": "build-output-not-authenticated",
    }, sort_keys=True, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="action", required=True)
    prep = subcommands.add_parser("prepare")
    prep.add_argument("--artifact", type=Path, required=True)
    prep.add_argument("--output", type=Path, required=True)
    prep.add_argument("--builder-image", default=BUILDER)
    prep.add_argument("--source-date-epoch", type=int, default=0)
    verify = subcommands.add_parser("verify")
    verify.add_argument("--context", type=Path, required=True)
    image = subcommands.add_parser("build")
    image.add_argument("--context", type=Path, required=True)
    image.add_argument("--runtime-image", required=True)
    image.add_argument("--output", type=Path, required=True)
    image.add_argument("--no-cache", action="store_true")
    args = parser.parse_args()
    if args.action == "prepare":
        result = prepare(args.artifact, args.output, builder_image=args.builder_image, source_date_epoch=args.source_date_epoch)
        print(json.dumps({"context": str(args.output.absolute()), "files": len(result["files"]), "releaseStatus": result["releaseStatus"]}))
    elif args.action == "verify":
        result = verify_context(args.context)
        print(json.dumps({"status": "integrity-verified", "irDigest": result["irDigest"], "releaseStatus": result["releaseStatus"]}))
    else:
        build(args.context, args.runtime_image, args.output, no_cache=args.no_cache)


if __name__ == "__main__":
    try:
        main()
    except Exception:
        raise SystemExit("reakt: OCI packaging failed") from None
