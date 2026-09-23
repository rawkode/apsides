#!/usr/bin/env python3
"""Packaging glue checks; these do not claim a Cargo or OCI build passed."""
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("apsides_package_oci", ROOT / "scripts/package_oci.py")
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)
sys.path.insert(0, str(ROOT / "scripts"))
import extract_binary  # noqa: E402


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="apsides-packaging-test-")
        self.root = Path(self.temporary.name).resolve()
        self.repo = self.root / "repo"
        self.artifact = self.root / "artifact"
        self.output = self.root / "context"
        (self.artifact / "controller/src").mkdir(parents=True)
        (self.artifact / "controller/src/main.rs").write_text("fn main() {}\n")
        (self.artifact / "controller/src/ir.json").write_text('{}\n')
        lock = b"version = 4\n"
        (self.artifact / "controller/Cargo.lock").write_bytes(lock)
        path = str(self.repo / "crates/runtime")
        self.manifest = ('[package]\nname="controller"\nversion="0.1.0"\nedition="2024"\n'
                         '[workspace]\n[dependencies]\nruntime={path=' + json.dumps(path) + '}\n'
                         'tokio={version="1", features=["macros", "rt-multi-thread"]}\n')
        (self.artifact / "controller/Cargo.toml").write_text(self.manifest)
        (self.artifact / "manifest.json").write_text(json.dumps({"irDigest": "sha256:" + "a" * 64,
            "files": {"controller/Cargo.lock": packaging.sha256(lock)}}))
        self.repo.mkdir()
        (self.repo / "Cargo.toml").write_text('[workspace]\nmembers=["crates/*"]\n')
        for crate in ["runtime", "compiler-core"]:
            (self.repo / f"crates/{crate}/src").mkdir(parents=True)
            (self.repo / f"crates/{crate}/Cargo.toml").write_text('[package]\nname="test"\nversion="0.1.0"\n')
            (self.repo / f"crates/{crate}/src/lib.rs").write_text("// trusted runtime snapshot\n")
        self.calls = []
        # A pinned runtime base, saved as the classic store writes it.
        self.base_config = image_config([BASE_LAYER])
        self.base_archive, _ = classic_save(self.root / "runtime-base.tar", self.base_config, [BASE_LAYER])
        self.pins, self.runtime_image, _, _ = write_pins(self.root / "pins", self.base_config, [BASE_LAYER])

    def tearDown(self):
        self.temporary.cleanup()

    def build(self, destination, run, no_cache=False):
        with patch.object(packaging, "command", self.fake_command), patch.object(packaging.subprocess, "run", run):
            return packaging.build(self.output, self.runtime_image, self.base_archive, destination,
                                   no_cache=no_cache, pins=self.pins)

    def test_current_runtime_dependency_closure_includes_coordination(self):
        closure = packaging.local_crates(ROOT)
        self.assertIn("coordination", closure)
        self.assertEqual(closure, sorted(set(closure)))

    def test_committed_runtime_base_pins_are_exact_registry_bytes(self):
        pins = ROOT / "scripts/runtime-base"
        for blob in (pins / "blobs/sha256").iterdir():
            self.assertEqual(packaging.sha256(blob.read_bytes()), "sha256:" + blob.name)
        reference = (pins / "reference").read_text().strip()
        index = json.loads(packaging.pinned_blob(pins, reference.rsplit("@", 1)[1]))
        for architecture in ["amd64", "arm64"]:
            [entry] = [item for item in index["manifests"]
                       if (item["platform"]["os"], item["platform"]["architecture"]) == ("linux", architecture)]
            packaging.pinned_blob(pins, entry["digest"], entry["size"])

    def fake_command(self, argv, cwd=None, timeout=180):
        self.calls.append(argv)
        if argv[:3] == ["docker", "image", "inspect"] and argv[3] != packaging.BUILDER:
            return json.dumps([{"Os": "linux", "Architecture": "arm64", "Id": packaging.sha256(self.base_config),
                                "RepoDigests": [argv[3]]}])
        if argv[:3] == ["docker", "image", "inspect"]:
            return json.dumps([{"Os": "linux", "Architecture": "arm64", "Id": "sha256:" + "b" * 64,
                                "RepoDigests": [packaging.BUILDER]}])
        if argv[:2] == ["cargo", "vendor"]:
            self.assertIn("--locked", argv)
            self.assertIn("--offline", argv)
            (cwd / "vendor/example").mkdir(parents=True)
            (cwd / "vendor/example/Cargo.toml").write_text('[package]\nname="example"\n')
            return '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="vendor"\n'
        if len(argv) > 1 and argv[1] == "check":
            return json.dumps({"status": "valid", "irDigest": "sha256:" + "a" * 64})
        return "{}"

    def prepare(self):
        with patch.object(packaging, "command", self.fake_command):
            return packaging.prepare(self.artifact, self.output, repository=self.repo)

    def verify(self):
        with patch.object(packaging, "command", self.fake_command):
            return packaging.verify_context(self.output)

    def test_context_preserves_lock_and_contains_only_source_dependency_snapshot(self):
        result = self.prepare()
        self.assertEqual((self.output / "controller/Cargo.lock").read_bytes(), (self.artifact / "controller/Cargo.lock").read_bytes())
        manifest = (self.output / "controller/Cargo.toml").read_text()
        self.assertIn('../workspace/crates/runtime', manifest)
        self.assertNotIn(str(self.repo), manifest)
        self.assertEqual(result, self.verify())
        self.assertIsNone(result["runtimeImage"])
        self.assertEqual(result["releaseStatus"], "blocked")
        self.assertFalse(any("pull" in argv for argv in self.calls))
        self.assertTrue(any(argv[:2] == ["cargo", "metadata"] for argv in self.calls))

    def test_context_verification_rejects_extra_and_changed_files(self):
        self.prepare()
        extra = self.output / "unexpected"
        extra.write_text("not declared")
        with self.assertRaisesRegex(ValueError, "file set or contents"):
            self.verify()
        extra.unlink()
        (self.output / "controller/src/ir.json").write_text('{"changed":true}')
        with self.assertRaisesRegex(ValueError, "file set or contents"):
            self.verify()

    def test_relabeling_context_ir_is_rejected_by_trusted_validator(self):
        self.prepare()
        path = self.output / "context.json"
        metadata = json.loads(path.read_text())
        metadata["irDigest"] = "sha256:" + "f" * 64
        path.write_text(json.dumps(metadata))
        with self.assertRaisesRegex(ValueError, "validated embedded IR"):
            self.verify()

    def test_build_refuses_changed_sources_or_metadata_before_publishing_receipt(self):
        for target in ["controller/src/main.rs", "context.json"]:
            with self.subTest(target=target):
                if self.output.exists():
                    shutil.rmtree(self.output)
                self.prepare()
                destination = self.root / ("image-source" if target.endswith("main.rs") else "image-metadata")
                def fake_build(argv, check):
                    path = self.output / target
                    if target == "context.json":
                        metadata = json.loads(path.read_text())
                        metadata["sourceArtifactManifestDigest"] = "sha256:" + "f" * 64
                        path.write_text(json.dumps(metadata))
                    else:
                        path.write_text("// source changed during image build\n")
                with self.assertRaisesRegex(ValueError, "changed"):
                    self.build(destination, fake_build)
                self.assertFalse((destination / "packaging-inputs.json").exists())

    def test_build_receipt_uses_initial_verified_snapshot_and_external_output(self):
        self.prepare()
        initial = packaging.read_regular(self.output / "context.json")
        destination = self.root / "image"
        with self.assertRaisesRegex(ValueError, "outside"):
            packaging.build(self.output, self.runtime_image, self.base_archive, self.output / "image")
        def fake_build(argv, check):
            self.assertIn("--network=none", argv)
            self.assertIn("--pull=false", argv)
            self.assertIn("--no-cache", argv)
            self.assertFalse(any("RUNTIME_IMAGE" in argument for argument in argv))
            [output] = [argument for argument in argv if argument.startswith("type=local,dest=")]
            exported = Path(output.removeprefix("type=local,dest="))
            exported.mkdir(parents=True)
            (exported / "apsides-controller").write_bytes(ELF)
        digest = self.build(destination, fake_build, no_cache=True)
        receipt = json.loads((destination / "packaging-inputs.json").read_text())
        self.assertEqual(receipt["contextDigest"], packaging.sha256(initial))
        self.assertEqual(receipt["releaseStatus"], "blocked")
        self.assertEqual(receipt["imageDigest"], digest)
        self.assertEqual(receipt["binaryDigest"], packaging.sha256(ELF))
        self.assertEqual(sorted(path.name for path in destination.iterdir()), ["controller.oci.tar", "packaging-inputs.json"])
        self.assertEqual(extract_binary.extract(destination, self.root / "binary"), packaging.sha256(ELF))

    def test_build_rejects_an_unpinned_runtime_base_before_compiling(self):
        self.prepare()
        self.base_config = image_config([CONTROLLER_LAYER])
        self.base_archive.unlink()
        classic_save(self.base_archive, self.base_config, [CONTROLLER_LAYER])
        def compile_(argv, check):
            raise AssertionError("BuildKit must not run for an unpinned base")
        with self.assertRaisesRegex(ValueError, "pinned manifest"):
            self.build(self.root / "image", compile_)
        self.assertFalse((self.root / "image").exists())

    def test_build_rejects_anything_but_the_binary_from_the_exporter(self):
        self.prepare()
        def fake_build(argv, check):
            [output] = [argument for argument in argv if argument.startswith("type=local,dest=")]
            exported = Path(output.removeprefix("type=local,dest="))
            (exported / "etc").mkdir(parents=True)
            (exported / "apsides-controller").write_bytes(ELF)
        with self.assertRaisesRegex(ValueError, "more or less than the controller"):
            self.build(self.root / "image", fake_build)
        self.assertFalse((self.root / "image/packaging-inputs.json").exists())

    def test_mutated_lock_during_vendor_never_publishes_a_context(self):
        def mutate(argv, cwd=None, timeout=180):
            result = self.fake_command(argv, cwd, timeout)
            if argv[:2] == ["cargo", "vendor"]:
                (cwd / "controller/Cargo.lock").write_text("changed")
            return result
        with patch.object(packaging, "command", mutate):
            with self.assertRaisesRegex(ValueError, "changed Cargo.lock"):
                packaging.prepare(self.artifact, self.output, repository=self.repo)
        self.assertFalse(self.output.exists())
        self.assertFalse(list(self.root.glob(".apsides-oci-*")))

    def test_unknown_generated_dependencies_and_symlink_sources_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "dependency contract"):
            packaging.relocated_manifest((self.manifest + 'unexpected="1"\n').encode(), self.repo)
        source = self.repo / "crates/compiler-core/src/escape.rs"
        source.symlink_to(self.artifact / "controller/src/ir.json")
        with self.assertRaisesRegex(ValueError, "non-symlink"):
            self.prepare()
        self.assertFalse(self.output.exists())

    def test_missing_or_unpinned_builder_fails_before_context_is_written(self):
        with self.assertRaisesRegex(ValueError, "repository@sha256"):
            packaging.pinned_image("rust:latest")
        def missing(argv, cwd=None, timeout=180):
            if argv[:3] == ["docker", "image", "inspect"]:
                raise RuntimeError("image is not cached")
            return "{}"
        with patch.object(packaging, "command", missing):
            with self.assertRaisesRegex(RuntimeError, "not cached"):
                packaging.prepare(self.artifact, self.output, repository=self.repo)
        self.assertFalse(self.output.exists())


def layer_tar(entries):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in entries:
            info = tarfile.TarInfo(name)
            if data is None:
                info.type, info.mode = tarfile.DIRTYPE, 0o755
                archive.addfile(info)
            else:
                info.size, info.mode = len(data), 0o755
                archive.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


def write_tar(path, entries):
    """Write `(name, bytes)` members; a `("->", target)` value is a symlink."""
    with tarfile.open(path, "w") as archive:
        for name, data in entries:
            info = tarfile.TarInfo(name)
            if isinstance(data, tuple):
                info.type, info.linkname = tarfile.SYMTYPE, data[1]
                archive.addfile(info)
            else:
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))


def blob_name(data):
    return "blobs/sha256/" + hashlib.sha256(data).hexdigest()


ELF = b"\x7fELF\x02\x01" + bytes(12) + (183).to_bytes(2, "little") + bytes(44)
BASE_LAYER = layer_tar([("etc", None), ("etc/os-release", b"ID=test\n")])
CONTROLLER_LAYER = layer_tar([("usr", None), ("usr/local", None), ("usr/local/bin", None),
                              ("usr/local/bin/apsides-controller", ELF)])


def image_config(layers, architecture="arm64"):
    return json.dumps({"architecture": architecture, "os": "linux", "config": {},
                       "rootfs": {"type": "layers", "diff_ids": [packaging.sha256(layer) for layer in layers]}}).encode()


def write_pins(pins, config, layers, compressed=None):
    """Registry pins for an image: an OCI index and its arm64 manifest."""
    compressed = [gzip.compress(layer, mtime=0) for layer in layers] if compressed is None else compressed
    manifest = json.dumps({"schemaVersion": 2, "mediaType": packaging.OCI_MANIFEST,
        "config": {"mediaType": packaging.OCI_CONFIG, "digest": packaging.sha256(config), "size": len(config)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": packaging.sha256(layer), "size": len(layer)} for layer in compressed]}).encode()
    index = json.dumps({"schemaVersion": 2, "mediaType": packaging.OCI_INDEX, "manifests": [
        {"mediaType": packaging.OCI_MANIFEST, "digest": packaging.sha256(manifest), "size": len(manifest), "platform": {"os": "linux", "architecture": "arm64"}},
        {"mediaType": packaging.OCI_MANIFEST, "digest": "sha256:" + "0" * 64, "size": 1, "platform": {"os": "linux", "architecture": "amd64"}}]}).encode()
    shutil.rmtree(pins, ignore_errors=True)
    (pins / "blobs/sha256").mkdir(parents=True)
    for data in [index, manifest]:
        (pins / "blobs/sha256" / packaging.sha256(data)[7:]).write_bytes(data)
    return pins, "example.invalid/runtime@" + packaging.sha256(index), index, manifest


def classic_save(path, config, layers):
    """Docker 25+ with the classic store: OCI layout of uncompressed layers plus manifest.json."""
    manifest = json.dumps({"schemaVersion": 2, "mediaType": packaging.OCI_MANIFEST,
        "config": {"mediaType": packaging.OCI_CONFIG, "digest": packaging.sha256(config), "size": len(config)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": packaging.sha256(layer), "size": len(layer)} for layer in layers]}).encode()
    index = json.dumps({"schemaVersion": 2, "mediaType": packaging.OCI_INDEX, "manifests": [
        {"mediaType": packaging.OCI_MANIFEST, "digest": packaging.sha256(manifest), "size": len(manifest)}]}).encode()
    write_tar(path, [("oci-layout", b'{"imageLayoutVersion": "1.0.0"}'), ("index.json", index),
                     ("manifest.json", json.dumps([{"Config": blob_name(config), "Layers": [blob_name(layer) for layer in layers]}]).encode()),
                     (blob_name(manifest), manifest), (blob_name(config), config),
                     *[(blob_name(layer), layer) for layer in layers]])
    return path, packaging.sha256(manifest)


class SavedImageTests(unittest.TestCase):
    """`docker image save` output in each format the local image store writes."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="apsides-saved-image-test-")
        self.root = Path(self.temporary.name)
        self.layers = [BASE_LAYER, CONTROLLER_LAYER]
        self.config = image_config(self.layers)

    def tearDown(self):
        self.temporary.cleanup()

    def classic_store(self, name="classic.tar", layers=None):
        return classic_save(self.root / name, self.config, self.layers if layers is None else layers)

    def containerd_store(self):
        """The containerd store: index.json names the pulled multi-platform index;
        only the local platform's manifest and gzip layers are present."""
        compressed = [gzip.compress(layer, mtime=0) for layer in self.layers]
        manifest = json.dumps({"schemaVersion": 2, "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "digest": packaging.sha256(self.config), "size": len(self.config)},
            "layers": [{"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "digest": packaging.sha256(layer), "size": len(layer)} for layer in compressed]}).encode()
        pulled = json.dumps({"schemaVersion": 2, "mediaType": packaging.OCI_INDEX, "manifests": [
            {"mediaType": packaging.OCI_MANIFEST, "digest": packaging.sha256(manifest), "size": len(manifest), "platform": {"os": "linux", "architecture": "arm64"}},
            {"mediaType": packaging.OCI_MANIFEST, "digest": "sha256:" + "0" * 64, "size": 1, "platform": {"os": "linux", "architecture": "amd64"}},
            {"mediaType": packaging.OCI_MANIFEST, "digest": "sha256:" + "1" * 64, "size": 1, "platform": {"os": "unknown", "architecture": "unknown"}}]}).encode()
        index = json.dumps({"schemaVersion": 2, "mediaType": packaging.OCI_INDEX, "manifests": [
            {"mediaType": packaging.OCI_INDEX, "digest": packaging.sha256(pulled), "size": len(pulled)}]}).encode()
        path = self.root / "containerd.tar"
        write_tar(path, [("oci-layout", b'{"imageLayoutVersion": "1.0.0"}'), ("index.json", index),
                         (blob_name(pulled), pulled), (blob_name(manifest), manifest), (blob_name(self.config), self.config),
                         *[(blob_name(layer), layer) for layer in compressed]])
        return path, packaging.sha256(pulled)

    def legacy_archive(self):
        """Docker before 25: manifest.json only, `<id>.json` config, `<id>/layer.tar` layers, shared layers as symlinks."""
        config_name = hashlib.sha256(self.config).hexdigest() + ".json"
        path = self.root / "legacy.tar"
        write_tar(path, [("manifest.json", json.dumps([{"Config": config_name, "RepoTags": None,
                                                        "Layers": ["base/layer.tar", "top/layer.tar"]}]).encode()),
                         (config_name, self.config), ("shared/layer.tar", BASE_LAYER), ("base/layer.tar", ("->", "../shared/layer.tar")),
                         ("top/layer.tar", CONTROLLER_LAYER), ("repositories", b"{}")])
        return path

    def test_every_save_format_yields_the_same_config_and_uncompressed_layers(self):
        classic, classic_root = self.classic_store()
        containerd, containerd_root = self.containerd_store()
        for path, root in [(classic, classic_root), (containerd, containerd_root), (self.legacy_archive(), None)]:
            with self.subTest(archive=path.name):
                image = packaging.saved_image(path, "linux/arm64")
                self.assertEqual(image["config"], self.config)
                self.assertEqual(image["root"], root)
                self.assertEqual([layer["diffSize"] for layer in image["layers"]], [len(layer) for layer in self.layers])
        self.assertEqual([layer["mediaType"] for layer in packaging.saved_image(containerd, "linux/arm64")["layers"]],
                         ["application/vnd.oci.image.layer.v1.tar+gzip"] * 2)

    def test_saved_images_reject_other_platforms_and_substituted_layers(self):
        classic, _ = self.classic_store()
        with self.assertRaisesRegex(ValueError, "platform"):
            packaging.saved_image(classic, "linux/amd64")
        containerd, _ = self.containerd_store()
        with self.assertRaises(ValueError):
            packaging.saved_image(containerd, "linux/amd64")
        substituted, _ = self.classic_store("substituted.tar", [BASE_LAYER, layer_tar([("usr", None)])])
        with self.assertRaisesRegex(ValueError, "layer differs"):
            packaging.saved_image(substituted, "linux/arm64")
        escaping = self.root / "escaping.tar"
        write_tar(escaping, [("manifest.json", b"[]"), ("link", ("->", "../../outside"))])
        with self.assertRaisesRegex(ValueError, "symlink"):
            packaging.saved_image(escaping, "linux/arm64")

    def controller_image(self, base, binary=ELF, epoch=0, name="controller.oci.tar"):
        """Package `binary` on the base saved in `base`, as `aps build` does."""
        output = self.root / name
        base_image = packaging.saved_image(base, "linux/arm64")
        return output, packaging.package(base, base_image, binary, "sha256:" + "a" * 64, epoch, output)

    def test_identical_inputs_give_a_byte_identical_layout_from_any_store(self):
        self.layers = [BASE_LAYER]
        self.config = image_config(self.layers)
        classic, _ = self.classic_store()
        containerd, _ = self.containerd_store()
        first, digest = self.controller_image(classic, name="first.oci.tar")
        again, repeated = self.controller_image(classic, name="again.oci.tar")
        other, from_containerd = self.controller_image(containerd, name="containerd.oci.tar")
        self.assertEqual(first.read_bytes(), again.read_bytes())
        self.assertEqual(first.read_bytes(), other.read_bytes())
        self.assertEqual({digest, repeated, from_containerd}, {digest})
        with tarfile.open(first) as archive:
            names = archive.getnames()
            index = json.loads(archive.extractfile("index.json").read())
            manifest = json.loads(archive.extractfile("blobs/sha256/" + digest[7:]).read())
            config = json.loads(archive.extractfile("blobs/sha256/" + manifest["config"]["digest"][7:]).read())
        self.assertEqual(sorted(name for name in names if not name.startswith("blobs/sha256/")), ["index.json", "oci-layout"])
        self.assertEqual([entry["digest"] for entry in index["manifests"]], [digest])
        # Every layer is stored uncompressed, so its blob is its diff ID.
        self.assertEqual([layer["digest"] for layer in manifest["layers"]], config["rootfs"]["diff_ids"])
        self.assertEqual({layer["mediaType"] for layer in manifest["layers"]}, {packaging.OCI_LAYER})
        _, changed = self.controller_image(classic, binary=ELF + b"\0", name="changed.oci.tar")
        _, later = self.controller_image(classic, epoch=1, name="later.oci.tar")
        self.assertEqual(len({digest, changed, later}), 3)

    def test_controller_layer_depends_only_on_the_binary_and_epoch(self):
        layer = packaging.controller_layer(ELF, 1700000000)
        self.assertEqual(layer, packaging.controller_layer(ELF, 1700000000))
        with tarfile.open(fileobj=io.BytesIO(layer)) as entries:
            members = entries.getmembers()
            self.assertEqual(entries.extractfile(members[-1]).read(), ELF)
        self.assertEqual([member.name for member in members], ["usr", "usr/local", "usr/local/bin", "usr/local/bin/apsides-controller"])
        for member in members:
            self.assertEqual((member.mtime, member.mode, member.uid, member.gid, member.uname, member.gname, member.pax_headers),
                             (1700000000, 0o755, 0, 0, "root", "root", {}))
        self.assertEqual([member.isdir() for member in members], [True, True, True, False])

    def test_image_config_extends_the_base_as_the_release_contract_requires(self):
        base = json.dumps({"architecture": "arm64", "os": "linux", "variant": "v8", "created": "2020-01-01T00:00:00Z",
                           "config": {"Env": ["PATH=/bin"], "User": "65532", "WorkingDir": "/home/nonroot", "Cmd": ["sh"],
                                      "Labels": {"base": "kept"}},
                           "history": [{"created_by": "base"}],
                           "rootfs": {"type": "layers", "diff_ids": [packaging.sha256(BASE_LAYER)]}}).encode()
        config = json.loads(packaging.image_config(base, packaging.sha256(CONTROLLER_LAYER), "sha256:" + "a" * 64, 86400))
        self.assertEqual(config["config"], {
            "Env": ["PATH=/bin"], "User": "65532:65532", "WorkingDir": "/", "Entrypoint": ["/usr/local/bin/apsides-controller"],
            "Labels": {"base": "kept", "dev.apsides.ir-digest": "sha256:" + "a" * 64, "dev.apsides.release-status": "blocked",
                       "org.opencontainers.image.title": "Apsides generated controller"}})
        self.assertEqual(config["rootfs"]["diff_ids"], [packaging.sha256(BASE_LAYER), packaging.sha256(CONTROLLER_LAYER)])
        self.assertEqual(config["created"], "1970-01-02T00:00:00Z")
        self.assertEqual([entry.get("created") for entry in config["history"]], [None, "1970-01-02T00:00:00Z"])
        self.assertEqual((config["os"], config["architecture"], config["variant"]), ("linux", "arm64", "v8"))

    def test_a_layer_changed_while_it_is_written_is_rejected(self):
        output = self.root / "changed.oci.tar"
        config = image_config([BASE_LAYER])
        with self.assertRaisesRegex(ValueError, "changed while it was written"):
            packaging.write_layout(output, "linux/arm64", config, [{
                "digest": packaging.sha256(BASE_LAYER), "size": len(BASE_LAYER),
                "open": lambda: io.BytesIO(BASE_LAYER[:-1] + b"x")}])
        self.assertFalse(output.exists())

    def pins(self, config=None, compressed=None):
        return write_pins(self.root / "pins", self.config if config is None else config, self.layers, compressed)

    def test_runtime_closure_verifies_the_pinned_chain_offline_for_either_store(self):
        classic, _ = self.classic_store()
        containerd, pulled = self.containerd_store()
        pins, reference, index, manifest = self.pins()
        # The classic store names an image by its config digest, the containerd store by what it pulled.
        for path, identity in [(classic, packaging.sha256(self.config)), (containerd, pulled)]:
            with self.subTest(archive=path.name):
                runtime = {"reference": reference, "platform": "linux/arm64", "localImageId": identity}
                with patch.object(subprocess, "run", side_effect=AssertionError("the chain is verified offline")):
                    closure = packaging.runtime_closure(path, runtime, pins)
                self.assertEqual(closure, {"baseImage": {"reference": reference, "platform": "linux/arm64"},
                                           "indexJson": index.decode(), "manifestJson": manifest.decode(), "configJson": self.config.decode()})
        with self.assertRaisesRegex(ValueError, "inspected pinned image"):
            packaging.runtime_closure(classic, {"reference": reference, "platform": "linux/arm64", "localImageId": "sha256:" + "f" * 64}, pins)

    def test_runtime_closure_rejects_unpinned_tampered_or_disconnected_bases(self):
        classic, _ = self.classic_store()
        runtime = {"platform": "linux/arm64", "localImageId": packaging.sha256(self.config)}
        pins, reference, index, _ = self.pins()
        with self.assertRaisesRegex(ValueError, "not pinned"):
            packaging.runtime_closure(classic, {**runtime, "reference": "example.invalid/runtime@sha256:" + "e" * 64}, pins)
        (pins / "blobs/sha256" / packaging.sha256(index)[7:]).write_bytes(index + b" ")
        with self.assertRaisesRegex(ValueError, "differs from its digest"):
            packaging.runtime_closure(classic, {**runtime, "reference": reference}, pins)
        pins, reference, _, _ = self.pins(config=image_config(self.layers, architecture="arm64") + b" ")
        with self.assertRaisesRegex(ValueError, "config differs from the pinned manifest"):
            packaging.runtime_closure(classic, {**runtime, "reference": reference}, pins)
        containerd, pulled = self.containerd_store()
        pins, reference, _, _ = self.pins(compressed=[gzip.compress(layer, mtime=1) for layer in self.layers])
        with self.assertRaisesRegex(ValueError, "layer differs from the pinned manifest"):
            packaging.runtime_closure(containerd, {**runtime, "localImageId": pulled, "reference": reference}, pins)

    def packaged(self, layers, binary=ELF):
        """An `aps build` image directory holding a layout of `layers`."""
        self.config = image_config(layers)
        image = self.root / "image"
        shutil.rmtree(image, ignore_errors=True)
        image.mkdir()
        digest = packaging.write_layout(image / "controller.oci.tar", "linux/arm64", self.config, [
            {"digest": packaging.sha256(layer), "size": len(layer), "open": lambda layer=layer: io.BytesIO(layer)} for layer in layers])
        (image / "packaging-inputs.json").write_text(json.dumps({
            "imageDigest": digest, "binaryDigest": packaging.sha256(binary), "builder": {"platform": "linux/arm64"}}))
        return image, digest

    def test_binary_is_extracted_from_the_final_layer_without_docker(self):
        image, digest = self.packaged(self.layers)
        output = self.root / "binary"
        with patch.object(subprocess, "run", side_effect=AssertionError("extraction must not run Docker")):
            self.assertEqual(extract_binary.extract(image, output), packaging.sha256(ELF))
        self.assertEqual((output / "apsides-controller").read_bytes(), ELF)
        self.assertEqual(json.loads((output / "binary-extraction.json").read_text())["imageDigest"], digest)

    def test_extraction_rejects_extra_final_layer_entries_and_unbuilt_binaries(self):
        extra = layer_tar([("usr", None), ("usr/local", None), ("usr/local/bin", None),
                           ("usr/local/bin/apsides-controller", ELF), ("etc/ld.so.preload", b"x")])
        image, _ = self.packaged([BASE_LAYER, extra])
        with self.assertRaisesRegex(ValueError, "more than the controller"):
            extract_binary.extract(image, self.root / "extra")
        image, _ = self.packaged(self.layers, binary=ELF + b"\0")
        with self.assertRaisesRegex(ValueError, "BuildKit built"):
            extract_binary.extract(image, self.root / "unbuilt")


class CommandBoundaryTests(unittest.TestCase):
    def test_invalid_inputs_return_bounded_errors(self):
        missing = "/nonexistent-private-canary/input"
        cases = [
            ("build.py", ["--artifact", missing, "--source-output", missing, "--profile", missing], "image build"),
            ("extract_binary.py", ["--image", missing, "--output", missing], "binary extraction"),
            ("sbom.py", ["--context", missing, "--tool", missing, "--tool-sha256", "invalid", "--target", "invalid", "--output", missing], "SBOM generation"),
            ("finalize_inputs.py", [flag for key in ["source", "context", "packaging-inputs", "runtime-base", "profile", "tool-snapshot", "output"] for flag in ["--" + key, missing]], "build input finalization"),
            ("package_oci.py", ["verify", "--context", missing], "OCI packaging"),
        ]
        for script, arguments, label in cases:
            with self.subTest(script=script):
                result = subprocess.run([sys.executable, str(ROOT / "scripts" / script), *arguments], capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(result.stderr, f"apsides: {label} failed\n")


if __name__ == "__main__":
    unittest.main()
