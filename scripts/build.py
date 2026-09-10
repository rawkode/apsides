#!/usr/bin/env python3
"""Assemble unsigned build outputs with Cargo, BuildKit and cargo-cyclonedx."""
import argparse
import json
import os
from pathlib import Path
import subprocess

from extract_binary import extract
from finalize_inputs import finalize
from package_oci import prepare, build, verify_context
from sbom import generate
from toolchain_snapshot import capture as capture_tools, verify as verify_tools


def assemble(artifact, source, profile_path, *, no_cache=False):
    artifact, source, profile_path = artifact.resolve(), source.resolve(), profile_path.resolve()
    profile_bytes = profile_path.read_bytes()
    profile = json.loads(profile_bytes)
    fields = {"schemaVersion", "builderImage", "runtimeImage", "platform", "sbomTool", "sbomToolSha256", "sourceDateEpoch"}
    if set(profile) != fields or profile["schemaVersion"] != "apsides.dev/build-profile/v1":
        raise ValueError("build profile has unknown or missing fields")
    targets = {"linux/arm64": "aarch64-unknown-linux-gnu", "linux/amd64": "x86_64-unknown-linux-gnu"}
    if profile["platform"] not in targets:
        raise ValueError("build platform must be linux/arm64 or linux/amd64")
    if type(profile["sourceDateEpoch"]) is not int or profile["sourceDateEpoch"] < 0:
        raise ValueError("sourceDateEpoch must be a declared nonnegative integer")
    tool = Path(profile["sbomTool"])
    if not tool.is_absolute():
        tool = profile_path.parent / tool
    cli = Path(os.environ.get("APSIDES_CLI", str(Path(__file__).resolve().parents[1] / "target/debug/aps")))
    tools = capture_tools(tool, cli)
    context, image = artifact.with_name(artifact.name + ".oci-context"), artifact / "image"
    metadata = prepare(artifact, context, builder_image=profile["builderImage"], source_date_epoch=profile["sourceDateEpoch"])
    if metadata["builder"]["platform"] != profile["platform"]:
        raise ValueError("cached builder platform differs from build profile")
    build(context, profile["runtimeImage"], image, no_cache=no_cache)
    binary_digest = extract(image, artifact / "linux-binary")
    sbom_digest = generate(context, tool, profile["sbomToolSha256"], targets[profile["platform"]], artifact / "sbom")
    base = artifact / "runtime-base.oci.tar"
    subprocess.run(["docker", "image", "save", "-o", str(base), profile["runtimeImage"]], check=True)
    verify_tools(tools, tool, cli)
    finalize(source / "build-inputs.json", context, base, image / "packaging-inputs.json", profile_path, tools, artifact / "build-inputs.json")
    verify_tools(tools, tool, cli)
    if profile_path.read_bytes() != profile_bytes or verify_context(context) != metadata:
        raise ValueError("build profile or context changed during assembly")
    result = {"schemaVersion": "apsides.dev/build-result/v1", "irDigest": metadata["irDigest"],
              "platform": profile["platform"], "binaryDigest": binary_digest, "sbomDigest": sbom_digest,
              "image": "image/controller.oci.tar", "binary": "linux-binary/apsides-controller", "sbom": "sbom/sbom.json",
              "buildInputs": "build-inputs.json", "releaseVerified": False,
              "remainingReleaseGates": ["approved-provenance", "reactor-and-security-evidence", "trusted-release-verification"]}
    (artifact / "build-result.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return result


if __name__ == "__main__":
    try:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--artifact", type=Path, required=True)
        parser.add_argument("--source-output", type=Path, required=True)
        parser.add_argument("--profile", type=Path, required=True)
        parser.add_argument("--no-cache", action="store_true")
        args = parser.parse_args()
        print(json.dumps(assemble(args.artifact, args.source_output, args.profile, no_cache=args.no_cache), sort_keys=True))
    except Exception:
        raise SystemExit("apsides: image build failed") from None
