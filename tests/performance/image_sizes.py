#!/usr/bin/env python3
"""Measure pinned OCI image descriptors and matching images already in Docker."""
import argparse
from contextlib import nullcontext
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
import tarfile
import time

DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
INDEX_TYPES = {"application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json"}
MANIFEST_TYPES = {"application/vnd.oci.image.manifest.v1+json", "application/vnd.docker.distribution.manifest.v2+json"}
LIMIT = 8 * 1024 * 1024


def require(value, reason):
    if not value:
        raise ValueError(reason)


def command(arguments):
    # Spool into an automatically removed file so subprocess output cannot grow Python memory.
    with tempfile.TemporaryFile() as output:
        process = None
        try:
            process = subprocess.Popen(arguments, stdout=output, stderr=subprocess.DEVNULL)
            deadline = time.monotonic() + 45
            while process.poll() is None:
                require(output.tell() <= LIMIT, "image inspection output exceeds bound")
                require(time.monotonic() < deadline, "image inspection timed out")
                try:
                    process.wait(timeout=0.05)
                except subprocess.TimeoutExpired:
                    pass
            require(process.returncode == 0, "image inspection failed")
        except OSError as error:
            raise ValueError("image inspection did not complete") from error
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=5)
        require(0 < output.tell() <= LIMIT, "image inspection output exceeds bound")
        output.seek(0)
        return output.read(LIMIT + 1)


def unique(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate image metadata field")
        result[key] = value
    return result


def document(raw):
    require(isinstance(raw, bytes) and 0 < len(raw) <= LIMIT, "image document exceeds bound")
    return json.loads(raw, object_pairs_hook=unique)


def descriptor(value):
    require(isinstance(value, dict) and isinstance(value.get("digest"), str)
            and DIGEST.fullmatch(value["digest"]) and type(value.get("size")) is int
            and 0 <= value["size"] <= 2**63 - 1, "invalid image descriptor")
    return {"digest": value["digest"], "size": value["size"]}


class LocalArchive:
    """Read a bounded OCI layout without extracting any files."""
    def __init__(self, path):
        self.path = Path(path)
        self.archive = None

    def __enter__(self):
        require(self.path.is_file() and not self.path.is_symlink() and 0 < self.path.stat().st_size <= 8 * 1024**3,
                "local OCI archive is not a bounded regular file")
        self.archive = tarfile.open(self.path, mode="r:")
        try:
            self.members = {}
            for member in self.archive:
                require(len(self.members) < 8192 and member.name not in self.members
                        and not member.name.startswith("/") and ".." not in member.name.split("/")
                        and (member.isfile() or member.isdir()), "invalid OCI archive member")
                self.members[member.name] = member
            require(document(self.read("oci-layout")) == {"imageLayoutVersion": "1.0.0"}, "unsupported OCI layout")
            self.index_raw = self.read("index.json")
            self.index = document(self.index_raw)
            require(self.index.get("schemaVersion") == 2 and self.index.get("mediaType") in INDEX_TYPES
                    and isinstance(self.index.get("manifests"), list), "invalid OCI root index")
            return self
        except Exception:
            self.archive.close()
            raise

    def __exit__(self, *_):
        self.archive.close()

    def read(self, name):
        member = self.members.get(name)
        require(member is not None and member.isfile() and 0 < member.size <= LIMIT, "OCI metadata missing or excessive")
        with self.archive.extractfile(member) as stream:
            return stream.read(LIMIT + 1)

    def blob(self, item, metadata=False):
        item = descriptor(item)
        member = self.members.get("blobs/sha256/" + item["digest"].removeprefix("sha256:"))
        require(member is not None and member.isfile() and member.size == item["size"]
                and member.size <= 4 * 1024**3, "OCI blob size differs")
        hasher = hashlib.sha256()
        with self.archive.extractfile(member) as stream:
            while chunk := stream.read(1024 * 1024):
                hasher.update(chunk)
        require("sha256:" + hasher.hexdigest() == item["digest"], "OCI blob digest differs")
        return self.read(member.name) if metadata else None

    def root(self, expected, platform):
        if "sha256:" + hashlib.sha256(self.index_raw).hexdigest() == expected:
            return self.index_raw
        matches = [item for item in self.index["manifests"] if item.get("digest") == expected]
        require(len(matches) == 1, "OCI root index does not bind requested image")
        require("platform" not in matches[0] or matches[0]["platform"] == platform, "OCI root platform differs")
        return self.blob(matches[0], metadata=True)


def inspect_image(reference, platform, run=command, archive_path=None):
    with LocalArchive(archive_path) if archive_path is not None else nullcontext(None) as archive:
        return _inspect_image(reference, platform, run, archive)


def _inspect_image(reference, platform, run, archive):
    match = re.fullmatch(r"([^\s@]+)@(sha256:[0-9a-f]{64})", reference)
    require(match is not None and not match[1].startswith("-") and "://" not in match[1], "image reference must be immutable")
    pieces = platform.split("/")
    require(len(pieces) in {2, 3} and pieces[0] == "linux" and all(re.fullmatch(r"[a-z0-9_]+", p) for p in pieces), "expected explicit Linux platform")
    expected = {"os": pieces[0], "architecture": pieces[1]}
    if len(pieces) == 3:
        expected["variant"] = pieces[2]

    def fetch(ref, digest):
        raw = archive.root(digest, expected) if archive is not None and digest == match[2] else (
            archive.blob(chosen, metadata=True) if archive is not None else
            run(["docker", "buildx", "imagetools", "inspect", "--raw", ref]))
        # Some CLI versions add one output newline; it is not part of the OCI document.
        candidates = [raw, raw[:-1]] if raw.endswith(b"\n") else [raw]
        accepted = next((candidate for candidate in candidates if "sha256:" + hashlib.sha256(candidate).hexdigest() == digest), None)
        require(accepted is not None, "registry document differs from immutable digest")
        return document(accepted), len(accepted)

    manifest, root_size = fetch(reference, match[2])
    root_type = manifest.get("mediaType")
    manifest_size = root_size
    selected = match[2]
    if manifest.get("mediaType") in INDEX_TYPES:
        entries = manifest.get("manifests")
        require(isinstance(entries, list) and 0 < len(entries) <= 256, "invalid image index population")
        matches = [item for item in entries if isinstance(item, dict) and item.get("platform") == expected]
        require(len(matches) == 1, "image index lacks one exact platform descriptor")
        chosen = descriptor(matches[0])
        require(matches[0].get("mediaType") in MANIFEST_TYPES, "selected descriptor is not an image manifest")
        selected = chosen["digest"]
        manifest, size = fetch(match[1] + "@" + selected, selected)
        manifest_size = size
        require(size == chosen["size"], "platform manifest size differs from index")
    require(manifest.get("schemaVersion") == 2 and manifest.get("mediaType") in MANIFEST_TYPES, "unsupported image manifest")
    config = descriptor(manifest.get("config"))
    layers = manifest.get("layers")
    require(isinstance(layers, list) and len(layers) <= 1024, "invalid image layer population")
    require(all(isinstance(layer, dict) and layer.get("mediaType") in {
        "application/vnd.oci.image.layer.v1.tar+gzip", "application/vnd.oci.image.layer.v1.tar+zstd",
        "application/vnd.docker.image.rootfs.diff.tar.gzip"} for layer in layers), "unsupported or uncompressed image layer")
    layers = [descriptor(layer) for layer in layers]
    if archive is not None:
        for item in [config, *layers]:
            archive.blob(item)
    archive_config = document(archive.blob(config, metadata=True)) if archive is not None else None
    local = document(run(["docker", "image", "inspect", selected if archive is not None else reference]))
    require(isinstance(local, list) and len(local) == 1 and isinstance(local[0], dict), "expected one existing Docker image")
    local = local[0]
    local_descriptor = local.get("Descriptor")
    modern = isinstance(local_descriptor, dict) and local.get("Id") == local_descriptor.get("digest") and any(
        local_descriptor.get("digest") == d and local_descriptor.get("size") == n and local_descriptor.get("mediaType") == t
        for d, n, t in [(selected, manifest_size, manifest["mediaType"]), (match[2], root_size, root_type)])
    require((local.get("Id") == config["digest"] or modern) and local.get("Os") == expected["os"]
            and local.get("Architecture") == expected["architecture"]
            and (len(pieces) == 2 or local.get("Variant") == expected["variant"]), "local image config or platform differs")
    size = local.get("Size")
    diffs = local.get("RootFS", {}).get("Layers")
    require(type(size) is int and 0 <= size <= 2**63 - 1
            and isinstance(diffs, list) and len(diffs) == len(layers)
            and all(isinstance(d, str) and DIGEST.fullmatch(d) for d in diffs), "invalid local image size or rootfs identity")
    if archive_config is not None:
        require(archive_config.get("os") == expected["os"] and archive_config.get("architecture") == expected["architecture"]
                and archive_config.get("rootfs", {}).get("diff_ids") == diffs, "Docker rootfs differs from verified archive config")
    return {"reference": reference, "platform": platform, "manifestDigest": selected, "config": config, "layers": layers,
            "declaredCompressedLayerBytes": sum(layer["size"] for layer in layers),
            "dockerReportedSizeBytes": size, "rootfsDiffIds": diffs,
            "dockerSizeSemantics": "packed resources reported by descriptor-based image store" if modern else "engine-reported legacy image size; not measured disk allocation",
            "manifestConfigDigest": config["digest"], "dockerIdentity": local["Id"],
            "dockerIdentityKind": "descriptor" if modern else "config", "manifestSource": "local-oci-archive" if archive is not None else "registry"}


def summarize(images):
    require(bool(images) and len({i["reference"] for i in images}) == len(images), "images must be nonempty and unique")
    layers, blobs = {}, {}
    for image in images:
        for item in [image["config"], *image["layers"]]:
            require(item["digest"] not in blobs or blobs[item["digest"]] == item["size"], "shared image digest has inconsistent sizes")
            blobs[item["digest"]] = item["size"]
        for layer in image["layers"]:
            layers[layer["digest"]] = layer["size"]
    return {"schemaVersion": "apsides.dev/benchmark-image-sizes/v1", "images": images,
            "aggregate": {"uniqueCompressedLayerBytes": sum(layers.values()), "uniqueLayerAndConfigBytes": sum(blobs.values()),
                          "uniqueCompressedLayers": len(layers),
                          "deduplicatedUnpackedBytes": None,
                          "unpackedStatus": "unavailable: Docker reported Size and diff IDs do not establish per-layer unpacked byte sizes"},
            "semantics": "registry-declared compressed layer/config bytes and Docker-reported per-image Size (engine/store-dependent; not asserted unpacked); not network transfer or disk allocation",
            "completeSection26": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", action="append", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--oci-archive", action="append", default=[], help="IMAGE@sha256:DIGEST=/absolute/archive.tar")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        require(1 <= len(args.image) <= 64, "image population exceeds bound")
        require(not args.output.exists() and not args.output.is_symlink(), "image size output exists")
        archives = {}
        for mapping in args.oci_archive:
            reference, separator, path = mapping.partition("=")
            require(separator and reference in args.image and reference not in archives and Path(path).is_absolute(), "invalid OCI archive mapping")
            archives[reference] = Path(path)
        report = summarize([inspect_image(reference, args.platform, archive_path=archives.get(reference)) for reference in args.image])
        with args.output.open("x") as stream:
            json.dump(report, stream, indent=2)
    except (ValueError, KeyError, TypeError, OSError, tarfile.TarError):
        parser.exit(1, "Image size collection failed; no benchmark acceptance claimed.\n")


if __name__ == "__main__":
    main()
