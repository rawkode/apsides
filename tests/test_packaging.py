#!/usr/bin/env python3
"""Packaging glue checks; these do not claim a Cargo or OCI build passed."""
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
# package_oci.py now imports sibling modules (diagnostics, rusty_v8) itself,
# so scripts/ must be on sys.path before it is exec'd below, not only before
# the sibling imports further down.
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("apsides_package_oci", ROOT / "scripts/package_oci.py")
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)
import diagnostics  # noqa: E402
import extract_binary  # noqa: E402
# The real-name `package_oci` module (as sbom.py/finalize_inputs.py import
# it), a *separate* load from `packaging` above (which is exec'd under the
# alias "apsides_package_oci" so its own tests can freely patch it without
# perturbing sibling scripts' imports of the real name). Patching a function
# defined in one does not affect the other; tests that exercise sbom.py's
# `from package_oci import verify_context` patch attributes on this module,
# not on `packaging`.
import finalize_inputs  # noqa: E402
import package_oci  # noqa: E402
import rusty_v8  # noqa: E402
import sbom  # noqa: E402


class _FakeProcess:
    """A minimal `subprocess.Popen`-shaped fake: `stdout` behaves like a
    real text-mode pipe (iterable for lines, closeable), `wait()` returns
    `returncode`."""

    def __init__(self, lines=(), returncode=0):
        self.stdout = io.StringIO("".join(lines))
        self.returncode = returncode

    def wait(self):
        return self.returncode


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
        # A fake rusty_v8 archive, so `prepare()`'s fetch never touches the
        # network in a test: `fake_rusty_v8_resolve` hands back this local
        # file (with a pin whose sha256 matches it) for whatever target the
        # inspected builder platform asks for.
        self.rusty_v8_archive = self.root / "fake-rusty-v8.a.gz"
        self.rusty_v8_archive.write_bytes(b"fake rusty_v8 static archive bytes")
        self.rusty_v8_sha256 = hashlib.sha256(self.rusty_v8_archive.read_bytes()).hexdigest()

    def tearDown(self):
        self.temporary.cleanup()

    def fake_rusty_v8_resolve(self, target, **kwargs):
        return self.rusty_v8_archive, {
            "target": target, "name": "librusty_v8", "version": "150.4.0",
            "url": f"https://example.invalid/rusty-v8/{target}.a.gz", "sha256": self.rusty_v8_sha256,
        }

    def build(self, destination, popen, no_cache=False):
        with patch.object(packaging, "command", self.fake_command), patch.object(packaging.subprocess, "Popen", popen):
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
        with patch.object(packaging, "command", self.fake_command), patch.object(rusty_v8, "resolve", self.fake_rusty_v8_resolve):
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
                def fake_build(argv, **kwargs):
                    path = self.output / target
                    if target == "context.json":
                        metadata = json.loads(path.read_text())
                        metadata["sourceArtifactManifestDigest"] = "sha256:" + "f" * 64
                        path.write_text(json.dumps(metadata))
                    else:
                        path.write_text("// source changed during image build\n")
                    return _FakeProcess(["Compiling controller v0.1.0\n"])
                with self.assertRaisesRegex(ValueError, "changed"):
                    self.build(destination, fake_build)
                self.assertFalse((destination / "packaging-inputs.json").exists())

    def test_build_receipt_uses_initial_verified_snapshot_and_external_output(self):
        self.prepare()
        initial = packaging.read_regular(self.output / "context.json")
        destination = self.root / "image"
        with self.assertRaisesRegex(ValueError, "outside"):
            packaging.build(self.output, self.runtime_image, self.base_archive, self.output / "image")
        def fake_build(argv, **kwargs):
            self.assertIn("--network=none", argv)
            self.assertIn("--pull=false", argv)
            self.assertIn("--no-cache", argv)
            self.assertFalse(any("RUNTIME_IMAGE" in argument for argument in argv))
            self.assertEqual(kwargs.get("stdout"), subprocess.PIPE)
            self.assertEqual(kwargs.get("stderr"), subprocess.STDOUT)
            [output] = [argument for argument in argv if argument.startswith("type=local,dest=")]
            exported = Path(output.removeprefix("type=local,dest="))
            exported.mkdir(parents=True)
            (exported / "apsides-controller").write_bytes(ELF)
            return _FakeProcess(["Compiling controller v0.1.0\n"])
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
        def compile_(argv, **kwargs):
            raise AssertionError("BuildKit must not run for an unpinned base")
        with self.assertRaisesRegex(ValueError, "pinned manifest"):
            self.build(self.root / "image", compile_)
        self.assertFalse((self.root / "image").exists())

    def test_build_rejects_anything_but_the_binary_from_the_exporter(self):
        self.prepare()
        def fake_build(argv, **kwargs):
            [output] = [argument for argument in argv if argument.startswith("type=local,dest=")]
            exported = Path(output.removeprefix("type=local,dest="))
            (exported / "etc").mkdir(parents=True)
            (exported / "apsides-controller").write_bytes(ELF)
            return _FakeProcess(["Compiling controller v0.1.0\n"])
        with self.assertRaisesRegex(ValueError, "more or less than the controller"):
            self.build(self.root / "image", fake_build)
        self.assertFalse((self.root / "image/packaging-inputs.json").exists())

    def test_mutated_lock_during_vendor_never_publishes_a_context(self):
        def mutate(argv, cwd=None, timeout=180):
            result = self.fake_command(argv, cwd, timeout)
            if argv[:2] == ["cargo", "vendor"]:
                (cwd / "controller/Cargo.lock").write_text("changed")
            return result
        with patch.object(packaging, "command", mutate), patch.object(rusty_v8, "resolve", self.fake_rusty_v8_resolve):
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

    def test_prepared_context_records_a_verifiable_rusty_v8_component(self):
        result = self.prepare()
        archive = result["rustyV8"]["archive"]
        self.assertEqual(archive["name"], "librusty_v8")
        self.assertEqual(archive["version"], "150.4.0")
        self.assertIn(archive["target"], rusty_v8.PLATFORM_TARGETS.values())
        self.assertTrue(archive["url"].startswith("https://"))
        self.assertEqual(archive["sha256"], "sha256:" + self.rusty_v8_sha256)
        self.assertEqual(archive["path"], rusty_v8.CONTEXT_RELATIVE_PATH)
        self.assertIsNone(result["rustyV8"]["bindingFile"])
        # This is exactly the record scripts/finalize_inputs.py carries
        # verbatim into build-inputs.json's compilerClosure.nativeCompiler.
        self.assertIn(rusty_v8.CONTEXT_RELATIVE_PATH, result["files"])
        self.assertEqual(result["files"][rusty_v8.CONTEXT_RELATIVE_PATH], archive["sha256"])
        self.assertEqual((self.output / rusty_v8.CONTEXT_RELATIVE_PATH).read_bytes(), self.rusty_v8_archive.read_bytes())
        self.assertEqual(result, self.verify())

    def _relock(self, v8_version):
        """Rewrite the artifact's Cargo.lock (and its recorded manifest
        digest) so `prepare()`'s real `rusty_v8.resolve()` -- not
        `fake_rusty_v8_resolve` -- sees a controller/Cargo.lock that locks
        the `v8` crate at `v8_version`."""
        lock = f'version = 4\n\n[[package]]\nname = "v8"\nversion = "{v8_version}"\n'.encode()
        (self.artifact / "controller/Cargo.lock").write_bytes(lock)
        manifest = json.loads((self.artifact / "manifest.json").read_text())
        manifest["files"]["controller/Cargo.lock"] = packaging.sha256(lock)
        (self.artifact / "manifest.json").write_text(json.dumps(manifest))

    def test_prepare_reports_an_actionable_error_when_no_cache_directory_can_be_determined(self):
        # scripts/rusty-v8/pins.json's real v8CrateVersion, so resolve()
        # reaches default_cache_dir() instead of failing the pin/lock check
        # first.
        self._relock("150.4.0")
        with patch.object(packaging, "command", self.fake_command), \
             patch.dict(os.environ, {"APSIDES_RUSTY_V8_CACHE": "", "XDG_CACHE_HOME": "", "HOME": ""}):
            with self.assertRaises(diagnostics.ActionableError) as failure:
                packaging.prepare(self.artifact, self.output, repository=self.repo)
        # Goes through failure_message(), the same as build.py's __main__:
        # the remedy must survive to the user-facing text, not be dropped
        # behind a bare "apsides: image build failed".
        message = diagnostics.failure_message("image build", failure.exception)
        self.assertTrue(message.startswith("apsides: image build failed\n"))
        self.assertIn("APSIDES_RUSTY_V8_CACHE", message)

    def test_prepare_reports_an_actionable_error_on_a_pin_lock_version_mismatch(self):
        self._relock("999.0.0")
        with patch.object(packaging, "command", self.fake_command):
            with self.assertRaises(diagnostics.ActionableError) as failure:
                packaging.prepare(self.artifact, self.output, repository=self.repo)
        message = diagnostics.failure_message("image build", failure.exception)
        self.assertTrue(message.startswith("apsides: image build failed\n"))
        self.assertIn("pins.json", message)
        self.assertIn("999.0.0", message)

    def test_verify_context_rejects_a_rusty_v8_record_inconsistent_with_its_staged_file(self):
        self.prepare()
        path = self.output / "context.json"
        original = json.loads(path.read_text())
        cases = [
            ("sha256", "sha256:" + "0" * 64),
            ("target", "x86_64-pc-windows-msvc"),
            ("url", None),
            ("name", None),
        ]
        for field, value in cases:
            with self.subTest(field=field):
                metadata = json.loads(json.dumps(original))
                metadata["rustyV8"]["archive"][field] = value
                path.write_text(json.dumps(metadata))
                with self.assertRaisesRegex(ValueError, "rusty_v8 archive record"):
                    self.verify()
        path.write_text(json.dumps(original))

    def test_finalize_inputs_packaging_sources_include_the_new_scripts(self):
        self.assertIn("scripts/diagnostics.py", finalize_inputs.PACKAGING_SOURCES)
        self.assertIn("scripts/rusty_v8.py", finalize_inputs.PACKAGING_SOURCES)

    def test_finalize_inputs_requires_the_matching_source_closure_schema(self):
        finalize_inputs.ensure_source_closure_schema({"schemaVersion": "apsides.dev/compiler-closure/v3"})
        with self.assertRaisesRegex(ValueError, "unexpected schema version"):
            finalize_inputs.ensure_source_closure_schema({"schemaVersion": "apsides.dev/compiler-closure/v2"})
        with self.assertRaisesRegex(ValueError, "unexpected schema version"):
            finalize_inputs.ensure_source_closure_schema({})
        # crates/release/src/compiler.rs's COMPILER_CLOSURE_SCHEMA and
        # packages/compiler/src/compiler.ts's COMPILER_CLOSURE_SCHEMA must
        # both agree with this, one version end to end.
        self.assertEqual(finalize_inputs.SOURCE_CLOSURE_SCHEMA, "apsides.dev/compiler-closure/v3")

    def test_build_raises_subprocess_failure_with_a_bounded_raw_tail_on_compile_failure(self):
        self.prepare()
        lines = [f"line {i}\n" for i in range(250)] + ["error: token=SEKRET1234567890\n"]
        def fake_popen(argv, **kwargs):
            self.assertEqual(kwargs.get("stdout"), subprocess.PIPE)
            self.assertEqual(kwargs.get("stderr"), subprocess.STDOUT)
            return _FakeProcess(lines, returncode=1)
        with self.assertRaises(diagnostics.SubprocessFailure) as failure:
            self.build(self.root / "image-compile-failure", fake_popen)
        # Raw and unbounded-by-redaction at this layer -- diagnostics.
        # failure_message() is the only place that redacts -- but already
        # bounded to the last diagnostics.DEFAULT_TAIL_LINES lines, since
        # stream() never buffers the whole (potentially huge) build log.
        self.assertIn("SEKRET1234567890", failure.exception.output)
        self.assertNotIn("line 0\n", failure.exception.output)
        self.assertIn("line 249", failure.exception.output)
        self.assertEqual(failure.exception.output.count("\n"), diagnostics.DEFAULT_TAIL_LINES - 1)
        self.assertFalse((self.root / "image-compile-failure/packaging-inputs.json").exists())

    def test_build_echoes_redacted_output_to_stderr_as_it_streams(self):
        self.prepare()
        def fake_popen(argv, **kwargs):
            [output] = [argument for argument in argv if argument.startswith("type=local,dest=")]
            exported = Path(output.removeprefix("type=local,dest="))
            exported.mkdir(parents=True)
            (exported / "apsides-controller").write_bytes(ELF)
            return _FakeProcess(["Compiling controller v0.1.0\n", "token=SEKRET1234567890\n"])
        captured = io.StringIO()
        with patch.object(sys, "stderr", captured):
            self.build(self.root / "image-echo", fake_popen)
        echoed = captured.getvalue()
        self.assertIn("Compiling controller v0.1.0", echoed)
        self.assertIn("token=[REDACTED]", echoed)
        self.assertNotIn("SEKRET1234567890", echoed)

    def test_sbom_adds_rusty_v8_manual_components_from_the_verified_context(self):
        self.prepare()
        tool = self.root / "cargo-cyclonedx"
        tool.write_bytes(b"#!/bin/sh\necho fake cargo-cyclonedx\n")
        tool_sha256 = packaging.sha256(tool.read_bytes())
        def fake_cyclonedx(argv, cwd=None, env=None, check=None, timeout=None, stdout=None, stderr=None, text=None):
            self.assertEqual(stdout, subprocess.PIPE)
            self.assertEqual(stderr, subprocess.STDOUT)
            document = {
                "bomFormat": "CycloneDX", "specVersion": "1.5",
                "components": [{"type": "library", "bom-ref": "pkg:cargo/tokio@1.0.0", "name": "tokio", "version": "1.0.0"}],
                "dependencies": [{"ref": "pkg:cargo/tokio@1.0.0"}],
            }
            (cwd / "controller/controller.cdx.json").write_text(json.dumps(document))
            return subprocess.CompletedProcess(argv, 0, stdout="")
        output = self.root / "sbom"
        with patch.object(package_oci, "command", self.fake_command), patch.object(subprocess, "run", fake_cyclonedx):
            digest = sbom.generate(self.output, tool, tool_sha256, "aarch64-unknown-linux-gnu", output)
        written = json.loads((output / "sbom.json").read_bytes())
        self.assertEqual(packaging.sha256(json.dumps(written, sort_keys=True, separators=(",", ":")).encode()), digest)
        names = {component["name"] for component in written["components"]}
        self.assertEqual(names, {"tokio", "librusty_v8"})
        [manual] = [component for component in written["components"] if component["name"] == "librusty_v8"]
        self.assertEqual(manual["version"], "150.4.0")
        self.assertEqual(manual["hashes"], [{"alg": "SHA-256", "content": self.rusty_v8_sha256}])
        self.assertIn("download_url=", manual["purl"])
        self.assertIn(f"checksum=sha256:{self.rusty_v8_sha256}", manual["purl"])
        self.assertEqual(manual["externalReferences"], [{"type": "distribution", "url": manual["externalReferences"][0]["url"]}])
        generation = json.loads((output / "sbom-generation.json").read_text())
        self.assertEqual(generation["manualComponents"], [manual["bom-ref"]])


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


class _FakeResponse:
    """A minimal `urllib.request.urlopen`-shaped context manager over fixed bytes."""

    def __init__(self, data):
        self.data = data

    def __enter__(self):
        return self

    def __exit__(self, *exc_info):
        return False

    def read(self):
        return self.data


class RustyV8Tests(unittest.TestCase):
    """`scripts/rusty_v8.py`'s fetch/verify/cache logic, entirely offline."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="apsides-rusty-v8-test-")
        self.root = Path(self.temporary.name).resolve()
        self.cache = self.root / "cache"
        self.data = b"a fake rusty_v8 prebuilt static archive\n"
        self.sha256 = hashlib.sha256(self.data).hexdigest()
        self.pins = {
            "v8CrateVersion": "150.4.0",
            "bindingFile": {"required": False},
            "archives": {
                "x86_64-unknown-linux-gnu": {
                    "url": "https://example.invalid/rusty-v8/x86_64.a.gz",
                    "sha256": self.sha256,
                },
            },
        }

    def tearDown(self):
        self.temporary.cleanup()

    def pin(self, **overrides):
        pins = {**self.pins, "archives": {"x86_64-unknown-linux-gnu": {**self.pins["archives"]["x86_64-unknown-linux-gnu"], **overrides}}}
        return rusty_v8.archive_pin("x86_64-unknown-linux-gnu", pins)

    def test_fetch_verifies_and_caches_the_pinned_archive_then_never_refetches_it(self):
        calls = []
        def opener(url, timeout=None):
            calls.append(url)
            return _FakeResponse(self.data)
        path = rusty_v8.fetch(self.pin(), cache_dir=self.cache, opener=opener)
        self.assertEqual(path.read_bytes(), self.data)
        self.assertEqual(calls, [self.pin()["url"]])
        def must_not_be_called(url, timeout=None):
            raise AssertionError("a valid cache hit must not be refetched")
        again = rusty_v8.fetch(self.pin(), cache_dir=self.cache, opener=must_not_be_called)
        self.assertEqual(again, path)

    def test_fetch_rejects_a_wrong_sha256_as_a_hard_failure_and_caches_nothing(self):
        def opener(url, timeout=None):
            return _FakeResponse(self.data)
        wrong = self.pin(sha256="0" * 64)
        with self.assertRaisesRegex(ValueError, "sha256 verification"):
            rusty_v8.fetch(wrong, cache_dir=self.cache, opener=opener)
        self.assertEqual(list(self.cache.glob("**/*")) if self.cache.exists() else [], [])

    def test_fetch_fails_clearly_when_offline_and_the_cache_is_empty(self):
        def offline(url, timeout=None):
            raise OSError("network is unreachable")
        with self.assertRaisesRegex(ValueError, "is not cached under.*failed"):
            rusty_v8.fetch(self.pin(), cache_dir=self.cache, opener=offline)

    def test_a_corrupted_cache_entry_is_refetched_rather_than_trusted(self):
        pin = self.pin()
        self.cache.mkdir(parents=True)
        (self.cache / f"{pin['sha256']}.a.gz").write_bytes(b"corrupted, not the real archive")
        path = rusty_v8.fetch(pin, cache_dir=self.cache, opener=lambda url, timeout=None: _FakeResponse(self.data))
        self.assertEqual(path.read_bytes(), self.data)

    def test_load_pins_fails_closed_once_a_binding_file_becomes_required(self):
        path = self.root / "pins.json"
        path.write_text(json.dumps({**self.pins, "bindingFile": {"required": True}}))
        with self.assertRaisesRegex(ValueError, "binding file"):
            rusty_v8.load_pins(path)

    def test_archive_pin_rejects_an_unknown_target_or_a_malformed_pin(self):
        with self.assertRaisesRegex(ValueError, "no pinned"):
            rusty_v8.archive_pin("x86_64-pc-windows-msvc", self.pins)
        with self.assertRaisesRegex(ValueError, "https URL"):
            self.pin(url="http://insecure.example/archive.gz")
        with self.assertRaisesRegex(ValueError, "64 lowercase hex"):
            self.pin(sha256="not-hex")

    def test_resolve_loads_pins_from_disk_and_fetches_by_target(self):
        path = self.root / "pins.json"
        path.write_text(json.dumps(self.pins))
        fetched, pin = rusty_v8.resolve(
            "x86_64-unknown-linux-gnu", cache_dir=self.cache, pins_path=path,
            opener=lambda url, timeout=None: _FakeResponse(self.data),
        )
        self.assertEqual(fetched.read_bytes(), self.data)
        self.assertEqual(pin["sha256"], self.sha256)

    def test_fetch_maps_a_filesystem_error_creating_the_cache_directory_to_a_clear_error(self):
        # A privilege-independent way to force a real OSError from mkdir()
        # (chmod alone would not: this sandbox runs as root, which bypasses
        # permission bits): the cache path has a regular file, not a
        # directory, as one of its own parent components.
        blocking_file = self.root / "not-a-directory"
        blocking_file.write_text("occupies the path a cache directory would need")
        with self.assertRaisesRegex(ValueError, "cannot create the rusty_v8 archive cache directory"):
            rusty_v8.fetch(self.pin(), cache_dir=blocking_file / "nested", opener=lambda url, timeout=None: _FakeResponse(self.data))

    def test_locked_version_reads_the_pinned_v8_crate_version(self):
        lock = b'[[package]]\nname = "v8"\nversion = "150.4.0"\n\n[[package]]\nname = "other"\nversion = "1.0.0"\n'
        self.assertEqual(rusty_v8.locked_version(lock), "150.4.0")
        self.assertIsNone(rusty_v8.locked_version(b'[[package]]\nname = "other"\nversion = "1.0.0"\n'))
        duplicated = b'[[package]]\nname = "v8"\nversion = "150.4.0"\n\n[[package]]\nname = "v8"\nversion = "150.4.1"\n'
        with self.assertRaisesRegex(ValueError, "more than one v8 crate version"):
            rusty_v8.locked_version(duplicated)

    def test_ensure_pin_matches_lock_fails_closed_on_a_version_mismatch(self):
        lock = b'[[package]]\nname = "v8"\nversion = "150.4.0"\n'
        rusty_v8.ensure_pin_matches_lock(lock, self.pins)  # matches; must not raise
        with self.assertRaisesRegex(ValueError, "pins v8 crate version"):
            rusty_v8.ensure_pin_matches_lock(lock, {**self.pins, "v8CrateVersion": "150.4.1"})
        with self.assertRaisesRegex(ValueError, "does not lock the v8 crate"):
            rusty_v8.ensure_pin_matches_lock(b'[[package]]\nname = "other"\nversion = "1.0.0"\n', self.pins)

    def test_resolve_cross_checks_lock_bytes_against_the_pin_when_given(self):
        path = self.root / "pins.json"
        path.write_text(json.dumps(self.pins))
        matching = b'[[package]]\nname = "v8"\nversion = "150.4.0"\n'
        rusty_v8.resolve(
            "x86_64-unknown-linux-gnu", lock_bytes=matching, cache_dir=self.cache, pins_path=path,
            opener=lambda url, timeout=None: _FakeResponse(self.data),
        )
        mismatched = b'[[package]]\nname = "v8"\nversion = "999.0.0"\n'
        def must_not_be_called(url, timeout=None):
            raise AssertionError("a mismatched pin must fail before any fetch")
        with self.assertRaisesRegex(ValueError, "pins v8 crate version"):
            rusty_v8.resolve(
                "x86_64-unknown-linux-gnu", lock_bytes=mismatched, cache_dir=self.cache, pins_path=path,
                opener=must_not_be_called,
            )


class DiagnosticsTests(unittest.TestCase):
    """`scripts/diagnostics.py`'s redaction, bounding and failure-message shaping."""

    def test_redact_removes_common_credential_shapes_but_keeps_ordinary_text(self):
        cases = [
            ("Authorization: Bearer abc123.def456ghi", "Authorization: Bearer [REDACTED]"),
            ("https://user:hunterpassword@example.invalid/path", "https://[REDACTED]@example.invalid/path"),
            ("token=sekret-value-1234567890", "token=[REDACTED]"),
            ("PASSWORD: hunter2value", "PASSWORD: [REDACTED]"),
            ("ghp_" + "a" * 36, "[REDACTED]"),
            ("AKIAABCDEFGHIJKLMNOP", "[REDACTED]"),
            ("https://example.invalid/x?sig=abcDEF123%3D&other=1", "sig=[REDACTED]&other=1"),
        ]
        for original, expected_fragment in cases:
            with self.subTest(original=original):
                self.assertIn(expected_fragment, diagnostics.redact(original))
        ordinary = "cargo build --release --manifest-path controller/Cargo.toml\nCompiling engine v0.1.0"
        self.assertEqual(diagnostics.redact(ordinary), ordinary)

    def test_bounded_keeps_only_the_configured_last_lines(self):
        text = "\n".join(f"line {i}" for i in range(500))
        result = diagnostics.bounded(text, lines=200)
        self.assertEqual(result.count("\n"), 199)
        self.assertNotIn("line 0", result.splitlines())
        self.assertTrue(result.endswith("line 499"))
        short = "only\ntwo lines"
        self.assertEqual(diagnostics.bounded(short, lines=200), short)

    def test_failure_message_is_plain_for_non_subprocess_errors(self):
        self.assertEqual(diagnostics.failure_message("image build", ValueError("bad input")), "apsides: image build failed")
        self.assertEqual(diagnostics.failure_message("image build", FileNotFoundError(2, "No such file")), "apsides: image build failed")

    def test_failure_message_appends_a_bounded_redacted_diagnostic_for_subprocess_failures(self):
        lines = [f"line {i}" for i in range(250)] + ["Authorization: Bearer sekret-token-value-should-not-leak"]
        error = diagnostics.SubprocessFailure("cargo build failed", "\n".join(lines))
        message = diagnostics.failure_message("image build", error)
        self.assertTrue(message.startswith("apsides: image build failed\n"))
        self.assertNotIn("sekret-token-value-should-not-leak", message)
        self.assertIn("Authorization: Bearer [REDACTED]", message)
        self.assertNotIn("line 0\n", message)
        self.assertIn("line 249", message)
        self.assertEqual(message.count("\n"), 200)

    def test_failure_message_omits_the_diagnostic_block_when_output_is_empty(self):
        error = diagnostics.SubprocessFailure("docker build failed", "")
        self.assertEqual(diagnostics.failure_message("image build", error), "apsides: image build failed")

    def test_actionable_error_is_a_value_error_so_existing_catches_still_match(self):
        self.assertIsInstance(diagnostics.ActionableError("set FOO"), ValueError)

    def test_failure_message_always_appends_an_actionable_errors_own_message(self):
        error = diagnostics.ActionableError(
            "cannot determine a rusty_v8 archive cache directory: set APSIDES_RUSTY_V8_CACHE "
            "(or XDG_CACHE_HOME, or HOME) to a writable directory"
        )
        message = diagnostics.failure_message("image build", error)
        self.assertEqual(message, f"apsides: image build failed\n{error}")
        self.assertIn("APSIDES_RUSTY_V8_CACHE", message)

    def test_failure_message_redacts_an_actionable_errors_message_too(self):
        error = diagnostics.ActionableError("update pins.json: token=SEKRET1234567890")
        message = diagnostics.failure_message("image build", error)
        self.assertNotIn("SEKRET1234567890", message)
        self.assertIn("token=[REDACTED]", message)

    def test_failure_message_omits_the_block_for_an_empty_actionable_error(self):
        self.assertEqual(diagnostics.failure_message("image build", diagnostics.ActionableError("")), "apsides: image build failed")


class SubprocessDiagnosticIntegrationTests(unittest.TestCase):
    """`command()` raises `SubprocessFailure` carrying the real, unredacted
    stderr; redaction and bounding happen only once, in `failure_message()`."""

    def test_command_helper_raises_subprocess_failure_with_raw_captured_stderr(self):
        def fake_run(argv, cwd=None, check=False, text=True, capture_output=True, timeout=180):
            return subprocess.CompletedProcess(argv, 1, stdout="", stderr="cargo error: token=SEKRET1234567890\n")
        with patch.object(subprocess, "run", fake_run):
            with self.assertRaises(diagnostics.SubprocessFailure) as failure:
                packaging.command(["cargo", "vendor", "--locked"])
        # Raw and unbounded at this layer: SEKRET1234567890 is redacted only
        # when diagnostics.failure_message() builds the final message.
        self.assertIn("SEKRET1234567890", failure.exception.output)
        message = diagnostics.failure_message("image build", failure.exception)
        self.assertNotIn("SEKRET1234567890", message)
        self.assertIn("token=[REDACTED]", message)


class DockerfileWiringTests(unittest.TestCase):
    """The committed Dockerfile itself wires RUSTY_V8_ARCHIVE for the
    network-less build, at the fixed path `prepare()` always stages the
    fetched archive at."""

    def test_dockerfile_sets_rusty_v8_archive_before_the_network_less_build(self):
        text = (ROOT / "scripts/Dockerfile").read_text()
        self.assertIn(f"ENV RUSTY_V8_ARCHIVE=/build/{rusty_v8.CONTEXT_RELATIVE_PATH}", text)
        self.assertIn("COPY rusty-v8 rusty-v8", text)
        run_index = text.index("RUN --network=none")
        self.assertLess(text.index("ENV RUSTY_V8_ARCHIVE="), run_index)
        self.assertLess(text.index("COPY rusty-v8 rusty-v8"), run_index)


class DistributionLayoutTests(unittest.TestCase):
    """The rusty_v8 archive cache must never resolve under an installed CLI
    distribution's verified asset tree.

    `crates/cli/src/assets.rs`'s `assets::root()` hash-checks the entire
    `<prefix>/share/apsides` tree (where `scripts/` lives in a distribution)
    against a manifest on every `aps` invocation; a cache directory
    `rusty_v8.py`'s own `ROOT` (`scripts/`'s parent) that landed inside that
    tree would make every `aps` command after the first `aps build` fail
    with "distribution assets do not match this binary" -- or, on a
    read-only installation prefix, fail immediately with a raw
    PermissionError. This copies the real scripts into a temporary
    `<prefix>/share/apsides/scripts/` layout (as `package_cli.py` would
    install them) and imports that copy fresh, so `ROOT` genuinely resolves
    to the simulated prefix, the same way it would for an installed CLI.
    """

    def test_default_cache_dir_and_a_real_fetch_never_write_under_the_prefix(self):
        with tempfile.TemporaryDirectory(prefix="apsides-distribution-test-") as temporary:
            temporary = Path(temporary).resolve()
            prefix = temporary / "prefix"
            assets = prefix / "share/apsides"
            scripts_dir = assets / "scripts"
            scripts_dir.mkdir(parents=True)
            # rusty_v8.py imports diagnostics, so a real distribution would
            # install both (scripts/diagnostics.py is in package_cli.py's
            # STATIC_SOURCES); copied here for the same reason, even though
            # this process already has the real diagnostics module cached
            # under that name and would reuse it either way -- it is pure
            # and stateless, so nothing about the property under test
            # (nothing lands under the prefix) depends on which copy runs.
            shutil.copy(ROOT / "scripts/rusty_v8.py", scripts_dir / "rusty_v8.py")
            shutil.copy(ROOT / "scripts/diagnostics.py", scripts_dir / "diagnostics.py")
            before = sorted(path.relative_to(prefix) for path in prefix.rglob("*"))
            home = temporary / "home"
            home.mkdir()
            spec = importlib.util.spec_from_file_location("apsides_distribution_rusty_v8", scripts_dir / "rusty_v8.py")
            distributed = importlib.util.module_from_spec(spec)
            # Simulates an installed distribution's environment: HOME is the
            # only thing naming a cache location (no APSIDES_RUSTY_V8_CACHE
            # or XDG_CACHE_HOME override, which would trivially pass this).
            # dont_write_bytecode matches crates/cli/src/commands/compile.rs
            # setting PYTHONDONTWRITEBYTECODE=1 for the real invocation, so
            # this doesn't fail on an unrelated __pycache__ artifact of
            # exec'ing the copy directly (a subprocess launch, unlike this,
            # reads that env var at interpreter startup).
            with patch.dict(os.environ, {"HOME": str(home), "XDG_CACHE_HOME": "", "APSIDES_RUSTY_V8_CACHE": ""}), \
                 patch.object(sys, "dont_write_bytecode", True):
                spec.loader.exec_module(distributed)
                self.assertEqual(distributed.ROOT, assets.resolve())
                cache_dir = distributed.default_cache_dir()
                self.assertEqual(cache_dir, home / ".cache/apsides/rusty-v8")
                self.assertFalse(str(cache_dir).startswith(str(prefix)))
                data = b"a fake distributed rusty_v8 archive\n"
                pin = {
                    "target": "x86_64-unknown-linux-gnu", "name": "librusty_v8", "version": "150.4.0",
                    "url": "https://example.invalid/a.gz", "sha256": hashlib.sha256(data).hexdigest(),
                }
                fetched = distributed.fetch(pin, opener=lambda url, timeout=None: _FakeResponse(data))
            self.assertTrue(str(fetched).startswith(str(home)))
            self.assertEqual(fetched.read_bytes(), data)
            after = sorted(path.relative_to(prefix) for path in prefix.rglob("*"))
            self.assertEqual(before, after, "a real fetch must not add, remove or change anything under the prefix")

    def test_default_cache_dir_fails_closed_without_any_usable_environment(self):
        with tempfile.TemporaryDirectory(prefix="apsides-distribution-test-") as temporary:
            with patch.dict(os.environ, {"APSIDES_RUSTY_V8_CACHE": "", "XDG_CACHE_HOME": "", "HOME": ""}):
                with self.assertRaisesRegex(ValueError, "cannot determine a rusty_v8 archive cache directory"):
                    rusty_v8.default_cache_dir()


if __name__ == "__main__":
    unittest.main()
