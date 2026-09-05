#!/usr/bin/env python3
"""Use Docker's OCI implementation to extract the native binary from a built image."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


def command(args):
    result = subprocess.run(args, check=True, capture_output=True, text=True, timeout=120)
    return result.stdout.strip()


def extract(image, output):
    image, output = image.resolve(), output.absolute()
    if output.exists():
        raise ValueError("binary output already exists")
    metadata = json.loads((image / "buildkit-metadata.json").read_text())
    image_digest = metadata["containerimage.digest"]
    if not re.fullmatch("sha256:[0-9a-f]{64}", image_digest):
        raise ValueError("invalid BuildKit image digest")
    archive = image / "controller.oci.tar"
    loaded = command(["docker", "load", "--input", str(archive)])
    if image_digest not in loaded:
        raise ValueError("Docker did not load the expected immutable OCI identity")
    details = json.loads(command(["docker", "image", "inspect", image_digest]))
    if len(details) != 1 or details[0]["Id"] != image_digest or details[0]["Os"] != "linux":
        raise ValueError("Docker image identity/platform differs from build output")
    output.mkdir(parents=True)
    container = command(["docker", "create", "--network", "none", "--read-only", "--cap-drop", "ALL", image_digest])
    if not re.fullmatch("[0-9a-f]{64}", container):
        raise ValueError("Docker did not return an exact container identity")
    try:
        # The container is never started: extraction cannot run artifact code.
        command(["docker", "cp", f"{container}:/usr/local/bin/reakt-controller", str(output / "reakt-controller")])
    finally:
        command(["docker", "rm", container])
    binary = (output / "reakt-controller").read_bytes()
    arch = details[0]["Architecture"]
    machine = {"arm64":183, "amd64":62}.get(arch)
    if len(binary) < 64 or binary[:6] != b"\x7fELF\x02\x01" or int.from_bytes(binary[18:20], "little") != machine:
        raise ValueError("image does not contain the expected native Linux controller")
    binary_digest = "sha256:" + hashlib.sha256(binary).hexdigest()
    (output / "binary-extraction.json").write_text(json.dumps({
        "schemaVersion":"reakt.dev/binary-extraction/v1", "imageDigest":image_digest,
        "binaryDigest":binary_digest, "platform":f"linux/{arch}",
        "verification":"Docker-verified-OCI-rootfs-extraction; signature-not-verified",
    }, sort_keys=True, indent=2) + "\n")
    return binary_digest


if __name__ == "__main__":
    try:
        parser=argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--image",type=Path,required=True)
        parser.add_argument("--output",type=Path,required=True)
        args=parser.parse_args()
        print(extract(args.image,args.output))
    except Exception:
        raise SystemExit("reakt: binary extraction failed") from None
