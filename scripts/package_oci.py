#!/usr/bin/env python3
"""Glue around Cargo vendor and Docker BuildKit; never pulls images or publishes.

The one exception is `prepare()` fetching the pinned rusty_v8 archive (see
`scripts/rusty_v8.py`): that fetch and its SHA-256 verification happen here,
outside the `--network=none` Docker build `build()` later runs."""
import argparse
from datetime import datetime, timezone
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib

import diagnostics
import rusty_v8

ROOT = Path(__file__).resolve().parents[1]
BUILDER = "rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922"
CONTEXT_SCHEMA = "apsides.dev/oci-context/v1"
RUNTIME_PINS = ROOT / "scripts/runtime-base"
# The final layer holds exactly the controller and its parent directories.
TARGET = "usr/local/bin/apsides-controller"
ANCESTORS = ("usr", "usr/local", "usr/local/bin")


def command(argv, cwd=None, timeout=180):
    result = subprocess.run(argv, cwd=cwd, check=False, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise diagnostics.SubprocessFailure(f"{argv[0]} {argv[1]} failed", result.stderr)
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


OCI_INDEX = "application/vnd.oci.image.index.v1+json"
OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
OCI_CONFIG = "application/vnd.oci.image.config.v1+json"
OCI_LAYER = "application/vnd.oci.image.layer.v1.tar"
INDEX_TYPES = {OCI_INDEX, "application/vnd.docker.distribution.manifest.list.v2+json"}
MANIFEST_TYPES = {OCI_MANIFEST, "application/vnd.docker.distribution.manifest.v2+json"}
# Saved layer media types and their OCI equivalent. Docker's classic store saves
# uncompressed layers; the containerd store saves the compressed blobs it holds.
LAYER_TYPES = {
    "application/vnd.oci.image.layer.v1.tar": "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.v1.tar+gzip": "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.docker.image.rootfs.diff.tar": "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.docker.image.rootfs.diff.tar.gzip": "application/vnd.oci.image.layer.v1.tar+gzip",
}
MAX_IMAGE_METADATA = 1024 * 1024
MAX_IMAGE_LAYERS = 64


def canonical_json(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def _members(contents):
    """Regular files of an image archive, with in-archive symlinks resolved."""
    files, links = {}, {}
    for member in contents:
        name = PurePosixPath(member.name.removeprefix("./")).as_posix()
        if member.isdir():
            continue
        if name in files or name in links:
            raise ValueError("duplicate image archive member")
        if member.isfile():
            files[name] = member
        elif member.issym():
            # Legacy docker-archive saves share identical layers through
            # relative symlinks; nothing outside the archive is followed.
            links[name] = os.path.normpath(str(PurePosixPath(name).parent / member.linkname))
        else:
            raise ValueError("image archive contains a non-regular entry")
    for name, target in links.items():
        if target not in files:
            raise ValueError("image archive symlink does not name an archived file")
        files[name] = files[target]
    return files


def _read(contents, member, limit=MAX_IMAGE_METADATA):
    if member.size > limit:
        raise ValueError("image metadata is oversized")
    return contents.extractfile(member).read()


def _layer_digests(contents, member, media_type):
    """One layer's blob digest, its uncompressed (diff ID) digest and uncompressed size."""
    def digest_of(reader):
        value, size = hashlib.sha256(), 0
        while chunk := reader.read(1024 * 1024):
            value.update(chunk)
            size += len(chunk)
        return "sha256:" + value.hexdigest(), size
    blob, size = digest_of(contents.extractfile(member))
    if not media_type.endswith("+gzip"):
        return blob, blob, size
    with gzip.GzipFile(fileobj=contents.extractfile(member)) as uncompressed:
        return (blob, *digest_of(uncompressed))


def saved_image(archive, platform):
    """Read the one `platform` image of a `docker image save` archive.

    Docker writes an OCI layout (index.json, from Docker 25) or, before that, a
    legacy docker-archive (manifest.json only); the classic and containerd image
    stores fill the OCI layout differently. Every blob read is checked against
    its digest and every layer against the config's diff IDs. Each layer
    records its stored form and its uncompressed size.
    """
    os_name, architecture = platform.split("/")
    with tarfile.open(archive) as contents:
        members = _members(contents)
        def blob(digest, size=None):
            if not re.fullmatch("sha256:[0-9a-f]{64}", str(digest)):
                raise ValueError("image archive requires SHA-256 descriptors")
            member = members.get("blobs/sha256/" + digest[7:])
            if member is None:
                raise ValueError("image archive lacks a referenced blob")
            data = _read(contents, member)
            if sha256(data) != digest or (size is not None and len(data) != size):
                raise ValueError("image archive blob digest mismatch")
            return data
        if "index.json" in members:
            entries = json.loads(_read(contents, members["index.json"])).get("manifests", [])
            if len(entries) != 1:
                raise ValueError("image archive must hold exactly one image")
            root = entries[0]["digest"]
            descriptor = entries[0]
            for _ in range(3):
                raw = blob(descriptor["digest"], descriptor.get("size"))
                document = json.loads(raw)
                kind = document.get("mediaType", descriptor.get("mediaType"))
                if kind not in INDEX_TYPES:
                    break
                matches = [item for item in document.get("manifests", [])
                           if item.get("platform", {}).get("os") == os_name
                           and item.get("platform", {}).get("architecture") == architecture]
                if len(matches) != 1:
                    raise ValueError("saved image index must select exactly one platform manifest")
                descriptor = matches[0]
            if kind not in MANIFEST_TYPES:
                raise ValueError("saved image does not resolve to a single-platform manifest")
            config = blob(document["config"]["digest"], document["config"]["size"])
            layers = []
            for layer in document["layers"]:
                if layer.get("mediaType") not in LAYER_TYPES:
                    raise ValueError("saved image uses an unsupported layer media type")
                member = members.get("blobs/sha256/" + str(layer["digest"]).removeprefix("sha256:"))
                if member is None or member.size != layer["size"]:
                    raise ValueError("image archive lacks a layer blob")
                layers.append({"mediaType": LAYER_TYPES[layer["mediaType"]], "digest": layer["digest"], "member": member})
        elif "manifest.json" in members:
            entries = json.loads(_read(contents, members["manifest.json"]))
            if not isinstance(entries, list) or len(entries) != 1:
                raise ValueError("image archive must hold exactly one image")
            root = None
            member = members.get(entries[0]["Config"])
            if member is None:
                raise ValueError("legacy image archive lacks its config")
            config = _read(contents, member)
            layers = []
            for name in entries[0]["Layers"]:
                if name not in members:
                    raise ValueError("legacy image archive lacks a layer")
                layers.append({"mediaType": LAYER_TYPES["application/vnd.oci.image.layer.v1.tar"], "digest": None, "member": members[name]})
        else:
            raise ValueError("unrecognised docker image save archive")
        details = json.loads(config)
        diff_ids = details.get("rootfs", {}).get("diff_ids")
        if details.get("os") != os_name or details.get("architecture") != architecture:
            raise ValueError("saved image platform differs from the requested platform")
        if (details.get("rootfs", {}).get("type") != "layers" or not isinstance(diff_ids, list)
                or len(diff_ids) != len(layers) or len(layers) > MAX_IMAGE_LAYERS):
            raise ValueError("saved image config does not describe its layers")
        for layer, diff_id in zip(layers, diff_ids):
            blob_digest, uncompressed, diff_size = _layer_digests(contents, layer["member"], layer["mediaType"])
            if uncompressed != diff_id or (layer["digest"] is not None and blob_digest != layer["digest"]):
                raise ValueError("saved image layer differs from its recorded digest")
            layer["digest"], layer["size"], layer["diffSize"] = blob_digest, layer["member"].size, diff_size
            layer["member"] = layer["member"].name
    return {"root": root, "config": config, "platform": platform, "layers": layers}


def pinned_blob(pins, value, size=None):
    """Exact registry bytes committed under scripts/runtime-base, by digest."""
    if not isinstance(value, str) or not re.fullmatch("sha256:[0-9a-f]{64}", value):
        raise ValueError("runtime base pins require SHA-256 descriptors")
    path = pins / "blobs/sha256" / value[7:]
    if not path.is_file():
        raise ValueError("the runtime base is not pinned; run `cuenv task runtimeBasePin`")
    data = read_regular(path)
    if sha256(data) != value or (size is not None and len(data) != size):
        raise ValueError("runtime base pin differs from its digest")
    return data


def runtime_closure(archive, runtime, pins=RUNTIME_PINS):
    """Verify, offline, the pinned base index -> platform manifest -> saved config chain.

    The index and manifest are the registry bytes committed as pins; the
    config is what `docker image save` of the pinned base holds, in whichever
    format the local image store writes. The saved image must be the one
    `docker image inspect` matched to the pinned digest: the classic store
    names an image by its config digest, the containerd store by the index it
    pulled.
    """
    reference, platform = runtime["reference"], runtime["platform"]
    index_raw = pinned_blob(pins, reference.rsplit("@", 1)[1])
    index = json.loads(index_raw)
    if index.get("mediaType") != OCI_INDEX:
        raise ValueError("pinned runtime base must be an OCI image index")
    os_name, architecture = platform.split("/")
    matches = [item for item in index.get("manifests", [])
               if item.get("platform", {}).get("os") == os_name and item.get("platform", {}).get("architecture") == architecture]
    if len(matches) != 1:
        raise ValueError("pinned runtime base index must select exactly one platform manifest")
    manifest_raw = pinned_blob(pins, matches[0]["digest"], matches[0]["size"])
    manifest = json.loads(manifest_raw)
    if manifest.get("mediaType") != OCI_MANIFEST:
        raise ValueError("pinned runtime base platform entry must be an OCI manifest")
    image = saved_image(archive, platform)
    if runtime["localImageId"] not in {image["root"], sha256(image["config"])}:
        raise ValueError("saved runtime base differs from the inspected pinned image")
    if (manifest["config"]["digest"] != sha256(image["config"])
            or manifest["config"]["size"] != len(image["config"])):
        raise ValueError("saved runtime base config differs from the pinned manifest")
    layers = manifest.get("layers", [])
    if len(layers) != len(image["layers"]):
        raise ValueError("saved runtime base layers differ from the pinned manifest")
    for pinned, saved in zip(layers, image["layers"]):
        # An uncompressed saved layer is named by its diff ID, already checked
        # against the config; a compressed one must be the registry blob.
        if saved["mediaType"] == pinned["mediaType"] and saved["digest"] != pinned["digest"]:
            raise ValueError("saved runtime base layer differs from the pinned manifest")
    return {"baseImage": {"reference": reference, "platform": platform},
            "indexJson": index_raw.decode("utf-8"), "manifestJson": manifest_raw.decode("utf-8"),
            "configJson": image["config"].decode("utf-8")}


def timestamp(epoch):
    return datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def controller_layer(binary, epoch):
    """The final layer: the controller and its parent directories, nothing else.

    Its bytes depend only on the binary and the source date epoch: entries in
    a fixed order, every mtime the epoch, root ownership, fixed modes, and a
    USTAR header with no PAX timestamps.
    """
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as layer:
        for name in [*ANCESTORS, TARGET]:
            info = tarfile.TarInfo(name)
            info.mtime, info.mode = epoch, 0o755
            info.uid, info.gid, info.uname, info.gname = 0, 0, "root", "root"
            if name == TARGET:
                info.size = len(binary)
                layer.addfile(info, io.BytesIO(binary))
            else:
                info.type = tarfile.DIRTYPE
                layer.addfile(info)
    return buffer.getvalue()


def image_config(base, layer_diff_id, ir_digest, epoch):
    """The controller image config: the pinned base config with the controller
    layer appended and the runtime settings `aps release` requires."""
    config = json.loads(base)
    runtime = config.get("config")
    diff_ids = config.get("rootfs", {}).get("diff_ids")
    if not isinstance(runtime, dict) or not isinstance(diff_ids, list):
        raise ValueError("runtime base config lacks a runtime configuration or layers")
    labels = dict(runtime.get("Labels") or {})
    labels.update({"dev.apsides.ir-digest": ir_digest, "dev.apsides.release-status": "blocked",
                   "org.opencontainers.image.title": "Apsides generated controller"})
    runtime = {**runtime, "User": "65532:65532", "WorkingDir": "/",
               "Entrypoint": ["/" + TARGET], "Labels": labels}
    runtime.pop("Cmd", None)
    created = timestamp(epoch)
    config.update({
        "config": runtime, "created": created,
        "history": [*config.get("history", []), {
            "created": created, "comment": "apsides.dev/oci-packaging",
            "created_by": f"COPY controller /{TARGET}; USER 65532:65532; WORKDIR /; ENTRYPOINT [\"/{TARGET}\"]"}],
        "rootfs": {"type": "layers", "diff_ids": [*diff_ids, layer_diff_id]},
    })
    return canonical_json(config)


class _Verified:
    """A reader that hashes what it yields, so a written blob is checked against its digest."""

    def __init__(self, reader, digest):
        self.reader, self.digest, self.hash = reader, digest, hashlib.sha256()

    def read(self, size=-1):
        data = self.reader.read(size)
        self.hash.update(data)
        return data

    def check(self):
        if "sha256:" + self.hash.hexdigest() != self.digest:
            raise ValueError("layer changed while it was written")


def write_layout(output, platform, config, layers):
    """Write a single-manifest OCI layout archive and return the manifest digest.

    Every layer is stored uncompressed, so its blob digest is its diff ID and
    the archive does not depend on how any image store keeps layers. `layers`
    holds each layer's diff ID, uncompressed size and a callable opening its
    uncompressed bytes.
    """
    manifest = canonical_json({
        "schemaVersion": 2, "mediaType": OCI_MANIFEST,
        "config": {"mediaType": OCI_CONFIG, "digest": sha256(config), "size": len(config)},
        "layers": [{"mediaType": OCI_LAYER, "digest": layer["digest"], "size": layer["size"]} for layer in layers],
    })
    os_name, architecture = platform.split("/")
    index = canonical_json({"schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": [{
        "mediaType": OCI_MANIFEST, "digest": sha256(manifest), "size": len(manifest),
        "platform": {"architecture": architecture, "os": os_name}}]})
    def entry(target, name, size, reader):
        info = tarfile.TarInfo(name)
        info.size, info.mode, info.mtime = size, 0o644, 0
        target.addfile(info, reader)
    with open(output, "xb") as file:
        try:
            with tarfile.open(fileobj=file, mode="w", format=tarfile.USTAR_FORMAT) as target:
                entry(target, "oci-layout", 30, io.BytesIO(b'{"imageLayoutVersion":"1.0.0"}'))
                entry(target, "index.json", len(index), io.BytesIO(index))
                entry(target, "blobs/sha256/" + sha256(manifest)[7:], len(manifest), io.BytesIO(manifest))
                entry(target, "blobs/sha256/" + sha256(config)[7:], len(config), io.BytesIO(config))
                written = {sha256(manifest), sha256(config)}
                for layer in layers:
                    if layer["digest"] not in written:
                        written.add(layer["digest"])
                        reader = _Verified(layer["open"](), layer["digest"])
                        entry(target, "blobs/sha256/" + layer["digest"][7:], layer["size"], reader)
                        reader.check()
        except BaseException:
            Path(output).unlink()
            raise
    return sha256(manifest)


def package(base_archive, base, binary, ir_digest, epoch, output):
    """Write the controller image: the verified base layers, uncompressed, and
    a controller layer and config generated from the binary and the epoch."""
    layer = controller_layer(binary, epoch)
    config = image_config(base["config"], sha256(layer), ir_digest, epoch)
    diff_ids = json.loads(base["config"])["rootfs"]["diff_ids"]
    with tarfile.open(base_archive) as contents:
        def opener(saved):
            if saved["mediaType"].endswith("+gzip"):
                return lambda: gzip.GzipFile(fileobj=contents.extractfile(saved["member"]))
            return lambda: contents.extractfile(saved["member"])
        layers = [{"digest": diff_id, "size": saved["diffSize"], "open": opener(saved)}
                  for saved, diff_id in zip(base["layers"], diff_ids)]
        layers.append({"digest": sha256(layer), "size": len(layer), "open": lambda: io.BytesIO(layer)})
        return write_layout(output, base["platform"], config, layers)


def exported_binary(directory):
    """The controller BuildKit's local exporter wrote: the export's only entry."""
    entries = sorted(path.name for path in directory.iterdir())
    if entries != ["apsides-controller"]:
        raise ValueError("BuildKit exported more or less than the controller binary")
    return read_regular(directory / "apsides-controller")


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
        if directory.parent != repository / "crates" or directory.name not in {"compiler-core", "runtime", "engine", "deploy", "platform", "inventory", "release", "cli", "coordination"}:
            raise ValueError("runtime path dependency escapes the trusted Apsides crate closure")
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


def rusty_v8_component(pin, path):
    """The `rustyV8.archive`/`rustyV8.bindingFile` record `prepare()` writes
    into `context.json`, and `sbom.py`/`finalize_inputs.py` read back."""
    return {
        "name": pin["name"],
        "version": pin["version"],
        "target": pin["target"],
        "url": pin["url"],
        "sha256": "sha256:" + pin["sha256"],
        "path": path,
    }


def prepare(artifact, output, repository=ROOT, builder_image=BUILDER, source_date_epoch=0, rusty_v8_cache_dir=rusty_v8.DEFAULT_CACHE_DIR):
    if not isinstance(source_date_epoch, int) or not 0 <= source_date_epoch <= 253402300799:
        raise ValueError("source date epoch must be a nonnegative supported Unix timestamp")
    artifact, output, repository = artifact.resolve(), output.absolute(), repository.resolve()
    if output.exists():
        raise ValueError("output already exists; choose a fresh build-context directory")
    if output == artifact or artifact in output.parents:
        raise ValueError("build context must be outside the input artifact")
    # Reuse the trusted IR/artifact validator. Packaging adds no approval semantics.
    command([os.environ.get("APSIDES_CLI", str(repository / "target/debug/aps")), "verify", "--artifact", str(artifact)])
    manifest_bytes = read_regular(artifact / "manifest.json")
    manifest = json.loads(manifest_bytes)
    lock = read_regular(artifact / "controller/Cargo.lock")
    if manifest["files"].get("controller/Cargo.lock") != sha256(lock):
        raise ValueError("artifact must contain a recorded exact controller/Cargo.lock; run the native build first")
    builder = pinned_image(builder_image)
    target = rusty_v8.PLATFORM_TARGETS.get(builder["platform"])
    if target is None:
        raise ValueError(f"builder platform {builder['platform']} has no pinned rusty_v8 archive target")
    # Fetched and SHA-256-verified here, before the network-less Docker
    # build: crates/engine's pinned `v8` crate build.rs needs this archive
    # locally, and `build()` below runs Cargo with `--network=none`.
    archive_path, archive_pin = rusty_v8.resolve(target, cache_dir=rusty_v8_cache_dir)
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".apsides-oci-", dir=output.parent))
    try:
        copy_source(artifact / "controller/src", staging / "controller/src")
        (staging / "controller/Cargo.toml").write_bytes(relocated_manifest(read_regular(artifact / "controller/Cargo.toml"), repository))
        (staging / "controller/Cargo.lock").write_bytes(lock)
        copy_file(repository / "Cargo.toml", staging / "workspace/Cargo.toml")
        for crate in local_crates(repository):
            copy_file(repository / f"crates/{crate}/Cargo.toml", staging / f"workspace/crates/{crate}/Cargo.toml")
            copy_source(repository / f"crates/{crate}/src", staging / f"workspace/crates/{crate}/src")
        copy_file(ROOT / "scripts/Dockerfile", staging / "Dockerfile")
        copy_file(archive_path, staging / rusty_v8.CONTEXT_RELATIVE_PATH)
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
            # No bindingFile: the pinned v8 crate ships a pre-generated
            # binding for both supported targets (scripts/rusty-v8/pins.json
            # bindingFile.required is false); rusty_v8.load_pins() fails
            # closed if that ever changes without this being updated.
            "rustyV8": {"archive": rusty_v8_component(archive_pin, rusty_v8.CONTEXT_RELATIVE_PATH), "bindingFile": None},
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
    archive = (metadata.get("rustyV8") or {}).get("archive") or {}
    if (
        archive.get("target") not in rusty_v8.PLATFORM_TARGETS.values()
        or not re.fullmatch("sha256:[0-9a-f]{64}", str(archive.get("sha256")))
        or metadata["files"].get(archive.get("path")) != archive.get("sha256")
        or not str(archive.get("url") or "").startswith("https://")
        or not archive.get("name") or not archive.get("version")
    ):
        raise ValueError("context lacks a pinned rusty_v8 archive record consistent with its staged file")
    if sha256(read_regular(context / "controller/Cargo.lock")) != metadata["controllerLockDigest"]:
        raise ValueError("controller lock identity changed")
    checked_ir = json.loads(command([os.environ.get("APSIDES_CLI", str(ROOT / "target/debug/aps")), "check", "--ir", str(context / "controller/src/ir.json")]))
    if checked_ir.get("status") != "valid" or checked_ir.get("irDigest") != metadata["irDigest"]:
        raise ValueError("context IR digest differs from the validated embedded IR")
    return metadata


def build(context, runtime_image, runtime_base, output, no_cache=False, pins=RUNTIME_PINS):
    """Compile the controller with BuildKit and package it on the pinned runtime base.

    BuildKit only compiles: its local exporter hands over the binary. The
    image is assembled here from `runtime_base`, a `docker image save` of the
    runtime image, so its bytes follow from the inputs on any Docker driver
    and image store.
    """
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
    closure = runtime_closure(runtime_base, runtime, pins)
    if output.exists():
        raise ValueError("OCI output directory already exists")
    output.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix="apsides-controller-") as exported:
        destination = Path(exported) / "controller"
        if "," in str(destination):
            raise ValueError("BuildKit export directory must not contain a comma")
        # --network none disables RUN networking. This is not an assertion about
        # BuildKit's own metadata resolution; whole-process egress needs an external gate.
        # The local exporter works on every Docker driver and image store.
        argv = ["docker", "buildx", "build", "--pull=false", "--network=none", "--provenance=false", "--sbom=false", "--platform", builder["platform"],
                "--build-arg", f"SOURCE_DATE_EPOCH={metadata['sourceDateEpoch']}",
                "--build-arg", f"BUILDER_IMAGE={builder['reference']}",
                "--output", f"type=local,dest={destination}", str(context)]
        if no_cache:
            argv.insert(3, "--no-cache")
        # Build logs remain visible: stdout and stderr are merged and written
        # to this process's stderr below whether the build succeeds or fails,
        # so the RUN --network=none Cargo compile's own output (e.g. a
        # crates/engine v8/deno_core build.rs failure) is never silently
        # dropped. Captured (not inherited) so a failure can also raise a
        # SubprocessFailure carrying it, for build.py's bounded, redacted
        # diagnostic on top-level failure.
        try:
            compiled = subprocess.run(argv, check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        except subprocess.CalledProcessError as error:
            sys.stderr.write(error.stdout or "")
            raise diagnostics.SubprocessFailure(f"{argv[0]} {argv[1]} build failed", error.stdout) from None
        sys.stderr.write(compiled.stdout)
        verify_context(context)
        if read_regular(context / "context.json") != context_snapshot:
            raise ValueError("context metadata changed during the build; no packaging receipt published")
        binary = exported_binary(destination)
    base = saved_image(runtime_base, builder["platform"])
    if base["config"].decode("utf-8") != closure["configJson"]:
        raise ValueError("saved runtime base changed during the build")
    image_digest = package(runtime_base, base, binary, metadata["irDigest"], metadata["sourceDateEpoch"], output / "controller.oci.tar")
    (output / "packaging-inputs.json").write_text(json.dumps({
        "contextDigest": sha256(context_snapshot),
        "builder": builder, "runtime": runtime, "irDigest": metadata["irDigest"],
        "sourceDateEpoch": metadata["sourceDateEpoch"], "binaryDigest": sha256(binary), "imageDigest": image_digest,
        "releaseStatus": "blocked", "verification": "build-output-not-authenticated",
    }, sort_keys=True, indent=2) + "\n")
    return image_digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="action", required=True)
    prep = subcommands.add_parser("prepare")
    prep.add_argument("--artifact", type=Path, required=True)
    prep.add_argument("--output", type=Path, required=True)
    prep.add_argument("--builder-image", default=BUILDER)
    prep.add_argument("--source-date-epoch", type=int, default=0)
    prep.add_argument("--rusty-v8-cache-dir", type=Path, default=rusty_v8.DEFAULT_CACHE_DIR)
    verify = subcommands.add_parser("verify")
    verify.add_argument("--context", type=Path, required=True)
    image = subcommands.add_parser("build")
    image.add_argument("--context", type=Path, required=True)
    image.add_argument("--runtime-image", required=True)
    image.add_argument("--runtime-base", type=Path, required=True)
    image.add_argument("--output", type=Path, required=True)
    image.add_argument("--no-cache", action="store_true")
    args = parser.parse_args()
    if args.action == "prepare":
        result = prepare(args.artifact, args.output, builder_image=args.builder_image, source_date_epoch=args.source_date_epoch, rusty_v8_cache_dir=args.rusty_v8_cache_dir)
        print(json.dumps({"context": str(args.output.absolute()), "files": len(result["files"]), "releaseStatus": result["releaseStatus"]}))
    elif args.action == "verify":
        result = verify_context(args.context)
        print(json.dumps({"status": "integrity-verified", "irDigest": result["irDigest"], "releaseStatus": result["releaseStatus"]}))
    else:
        build(args.context, args.runtime_image, args.runtime_base, args.output, no_cache=args.no_cache)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        raise SystemExit(diagnostics.failure_message("OCI packaging", error)) from None
