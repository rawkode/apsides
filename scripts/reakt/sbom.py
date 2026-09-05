#!/usr/bin/env python3
"""Generate a CycloneDX SBOM with pinned cargo-cyclonedx and normalized build paths."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from package_oci import verify_context


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def generate(context, tool, tool_digest, target, output):
    context, tool, output = context.resolve(), tool.resolve(), output.absolute()
    if output.exists():
        raise ValueError("SBOM output exists; use a fresh output directory")
    if digest(tool.read_bytes()) != tool_digest:
        raise ValueError("cargo-cyclonedx executable digest mismatch")
    if target not in ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]:
        raise ValueError("unsupported SBOM target")
    metadata = verify_context(context)
    epoch = metadata["sourceDateEpoch"]
    if not isinstance(epoch, int) or epoch < 0:
        raise ValueError("invalid declared source timestamp")
    lock = (context / "controller/Cargo.lock").read_bytes()
    if digest(lock) != metadata["controllerLockDigest"]:
        raise ValueError("Cargo lock differs from prepared build inputs")
    # The established generator writes beside Cargo.toml; use an isolated copy
    # so generating documentation cannot mutate the verified OCI context.
    with tempfile.TemporaryDirectory(prefix="reakt-sbom-") as temporary:
        build = Path(temporary).resolve() / "build"
        shutil.copytree(context, build)
        env = {"PATH": os.environ.get("PATH", ""), "CARGO_NET_OFFLINE": "true", "SOURCE_DATE_EPOCH": str(epoch), "LANG": "C", "TZ": "UTC"}
        subprocess.run([str(tool), "cyclonedx", "--manifest-path", str(build / "controller/Cargo.toml"), "--format", "json", "--spec-version", "1.5", "--all", "--target", target], cwd=build, env=env, check=True, timeout=180)
        if (build / "controller/Cargo.lock").read_bytes() != lock:
            raise ValueError("SBOM generation modified the dependency lock")
        raw = (build / "controller/controller.cdx.json").read_text()
        # Cargo package IDs contain absolute paths. This substitution only maps
        # our known isolated root to the actual container's fixed /build root;
        # it preserves every component and dependency edge from cargo-cyclonedx.
        value = json.loads(raw.replace(str(build), "/build"))
        if value.get("bomFormat") != "CycloneDX" or value.get("specVersion") != "1.5":
            raise ValueError("generator returned an unexpected SBOM format")
        if not value.get("components") or not value.get("dependencies"):
            raise ValueError("generator omitted the dependency inventory")
        data = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
        if str(build).encode() in data:
            raise ValueError("SBOM retains a host build path")
    if verify_context(context) != metadata or digest(tool.read_bytes()) != tool_digest:
        raise ValueError("SBOM inputs changed during generation")
    output.mkdir(parents=True)
    (output / "sbom.json").write_bytes(data)
    (output / "sbom-generation.json").write_text(json.dumps({
        "schemaVersion": "reakt.dev/sbom-generation/v1", "generatorDigest": tool_digest,
        "controllerLockDigest": digest(lock), "sourceDateEpoch": epoch, "target": target,
        "sbomDigest": digest(data), "scope": "native-controller-cargo-dependencies",
        "pathMapping": "isolated context root -> /build",
    }, sort_keys=True, indent=2) + "\n")
    return digest(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--context", type=Path, required=True)
    parser.add_argument("--tool", type=Path, required=True)
    parser.add_argument("--tool-sha256", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    print(generate(args.context, args.tool, args.tool_sha256, args.target, args.output))


if __name__ == "__main__":
    try:
        main()
    except Exception:
        raise SystemExit("reakt: SBOM generation failed") from None
