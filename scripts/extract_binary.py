#!/usr/bin/env python3
"""Extract the native binary from the final layer of a built OCI layout archive."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import tarfile

from package_oci import MAX_IMAGE_METADATA, read_regular, sha256

TARGET = "usr/local/bin/apsides-controller"
ANCESTORS = {"usr", "usr/local", "usr/local/bin"}
MAX_LAYER_BYTES = 512 * 1024 * 1024


def _files(contents):
    files = {}
    for member in contents:
        if member.isdir():
            continue
        if not member.isfile() or member.name in files:
            raise ValueError("OCI archive contains a non-regular or duplicate entry")
        files[member.name] = member
    return files


def _blob(contents, files, descriptor, limit=MAX_IMAGE_METADATA):
    digest = descriptor["digest"]
    member = files.get("blobs/sha256/" + str(digest).removeprefix("sha256:"))
    if member is None or member.size != descriptor["size"] or member.size > limit:
        raise ValueError("OCI archive lacks a bounded referenced blob")
    data = contents.extractfile(member).read()
    if sha256(data) != digest:
        raise ValueError("OCI blob digest mismatch")
    return data


def _controller(layer):
    """The controller bytes of a final layer that holds only it and its directories."""
    found, directories = None, set()
    with tarfile.open(fileobj=io.BytesIO(layer)) as entries:
        for entry in entries:
            path = PurePosixPath(entry.name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("OCI layer path escapes its root")
            name = path.as_posix().removeprefix("./")
            if name in ANCESTORS and entry.isdir():
                directories.add(name)
            elif name == TARGET and found is None and entry.isfile() and entry.mode & 0o001 and not entry.mode & 0o6000:
                found = entries.extractfile(entry).read()
            else:
                raise ValueError("final image layer holds more than the controller")
    if found is None or directories != ANCESTORS:
        raise ValueError("final image layer does not hold the controller")
    return found


def extract(image, output):
    image, output = image.resolve(), output.absolute()
    if output.exists():
        raise ValueError("binary output already exists")
    packaging = json.loads(read_regular(image / "packaging-inputs.json"))
    reported = json.loads(read_regular(image / "buildkit-metadata.json"))
    image_digest = packaging["imageDigest"]
    if not re.fullmatch("sha256:[0-9a-f]{64}", image_digest):
        raise ValueError("invalid packaged image digest")
    archive = image / "controller.oci.tar"
    read_regular(archive)
    with tarfile.open(archive) as contents:
        files = _files(contents)
        manifests = json.loads(contents.extractfile(files["index.json"]).read()).get("manifests", [])
        if len(manifests) != 1 or manifests[0]["digest"] != image_digest:
            raise ValueError("OCI archive does not select the packaged image")
        manifest = json.loads(_blob(contents, files, manifests[0]))
        if manifest["config"]["digest"] != reported.get("containerimage.config.digest"):
            raise ValueError("OCI archive config differs from the image BuildKit built")
        config = json.loads(_blob(contents, files, manifest["config"]))
        final = manifest["layers"][-1]
        layer = _blob(contents, files, final, MAX_LAYER_BYTES)
    platform = f"{config.get('os')}/{config.get('architecture')}"
    if platform != packaging["builder"]["platform"]:
        raise ValueError("OCI image platform differs from the build platform")
    if final["mediaType"] == "application/vnd.oci.image.layer.v1.tar+gzip":
        layer = gzip.decompress(layer)
    elif final["mediaType"] != "application/vnd.oci.image.layer.v1.tar":
        raise ValueError("unsupported final layer media type")
    if sha256(layer) != config["rootfs"]["diff_ids"][-1]:
        raise ValueError("final layer differs from its diff ID")
    binary = _controller(layer)
    machine = {"linux/arm64": 183, "linux/amd64": 62}.get(platform)
    if len(binary) < 64 or binary[:6] != b"\x7fELF\x02\x01" or int.from_bytes(binary[18:20], "little") != machine:
        raise ValueError("image does not contain the expected native Linux controller")
    output.mkdir(parents=True)
    (output / "apsides-controller").write_bytes(binary)
    (output / "apsides-controller").chmod(0o755)
    binary_digest = "sha256:" + hashlib.sha256(binary).hexdigest()
    (output / "binary-extraction.json").write_text(json.dumps({
        "schemaVersion": "apsides.dev/binary-extraction/v1", "imageDigest": image_digest,
        "binaryDigest": binary_digest, "platform": platform,
        "verification": "OCI-layout-final-layer-extraction; signature-not-verified",
    }, sort_keys=True, indent=2) + "\n")
    return binary_digest


if __name__ == "__main__":
    try:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--image", type=Path, required=True)
        parser.add_argument("--output", type=Path, required=True)
        args = parser.parse_args()
        print(extract(args.image, args.output))
    except Exception:
        raise SystemExit("apsides: binary extraction failed") from None
