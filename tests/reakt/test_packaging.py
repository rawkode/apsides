#!/usr/bin/env python3
"""Packaging glue checks; these do not claim a Cargo or OCI build passed."""
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("reakt_package_oci", ROOT / "scripts/reakt/package_oci.py")
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="reakt-packaging-test-")
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
        patch_copy = patch.object(packaging.patched_libsql, "copy_generated", self.copy_patch)
        patch_copy.start()
        self.addCleanup(patch_copy.stop)
        patch_verify = patch.object(packaging.patched_libsql, "verify_generated", return_value={"qualification": "synthetic dependency fixture"})
        patch_verify.start()
        self.addCleanup(patch_verify.stop)

    def copy_patch(self, source, destination):
        destination.mkdir(parents=True)
        (destination / "Cargo.toml").write_text('[package]\nname="libsql"\nversion="0.9.30"\n')
        return {"qualification": "synthetic dependency fixture"}

    def tearDown(self):
        self.temporary.cleanup()

    def test_current_runtime_dependency_closure_includes_coordination(self):
        closure = packaging.local_crates(ROOT)
        self.assertIn("coordination", closure)
        self.assertEqual(closure, sorted(set(closure)))

    def fake_command(self, argv, cwd=None, timeout=180):
        self.calls.append(argv)
        if argv[:3] == ["docker", "image", "inspect"]:
            return json.dumps([{"Os": "linux", "Architecture": "arm64", "Id": "sha256:" + "b" * 64,
                                "RepoDigests": [packaging.BUILDER]}])
        if argv[:2] == ["cargo", "vendor"]:
            self.assertIn('[patch.crates-io]', (cwd / '.cargo/config.toml').read_text())
            self.assertTrue((cwd / '.generated/libsql-0.9.30/Cargo.toml').is_file())
            self.assertIn("--locked", argv)
            self.assertIn("--offline", argv)
            (cwd / "vendor/example").mkdir(parents=True)
            (cwd / "vendor/example/Cargo.toml").write_text('[package]\nname="example"\n')
            return '[source.crates-io]\nreplace-with="vendored-sources"\n[source.vendored-sources]\ndirectory="vendor"\n'
        if argv[:2] == ["cargo", "metadata"]:
            self.assertIn('[patch.crates-io]', (cwd / '.cargo/config.toml').read_text())
            return json.dumps({"packages": [], "resolve": {"nodes": []}})
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

    def test_context_refuses_missing_or_unverified_patch(self):
        with patch.object(packaging.patched_libsql, "copy_generated", side_effect=ValueError("unverified patch")):
            with self.assertRaisesRegex(ValueError, "unverified patch"):
                self.prepare()
        self.assertFalse(self.output.exists())
        self.assertFalse(any(argv[:2] == ["cargo", "vendor"] for argv in self.calls))

    def test_prepared_context_rejects_a_patch_that_is_no_longer_current(self):
        self.prepare()
        with patch.object(packaging.patched_libsql, "verify_generated", side_effect=ValueError("outdated patch")):
            with self.assertRaisesRegex(ValueError, "outdated patch"):
                self.verify()

    def test_context_rejects_registry_libsql_resolution(self):
        def registry_metadata(argv, cwd=None, timeout=180):
            if argv[:2] == ["cargo", "metadata"]:
                return json.dumps({"packages": [{"name": "libsql", "version": "0.9.30", "source": "registry+https://example.invalid"}]})
            return self.fake_command(argv, cwd, timeout)
        with patch.object(packaging, "command", registry_metadata):
            with self.assertRaisesRegex(ValueError, "patched dependency"):
                packaging.prepare(self.artifact, self.output, repository=self.repo)
        self.assertFalse(self.output.exists())

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
                with patch.object(packaging, "command", self.fake_command), patch.object(packaging.subprocess, "run", fake_build):
                    with self.assertRaisesRegex(ValueError, "changed"):
                        packaging.build(self.output, packaging.BUILDER, destination)
                self.assertFalse((destination / "packaging-inputs.json").exists())

    def test_build_receipt_uses_initial_verified_snapshot_and_external_output(self):
        self.prepare()
        initial = packaging.read_regular(self.output / "context.json")
        destination = self.root / "image"
        with self.assertRaisesRegex(ValueError, "outside"):
            packaging.build(self.output, packaging.BUILDER, self.output / "image")
        def fake_build(argv, check):
            self.assertIn("--network=none", argv)
            self.assertIn("--pull=false", argv)
            self.assertIn("--no-cache", argv)
            (destination / "controller.oci.tar").write_bytes(b"mock build output")
            (destination / "buildkit-metadata.json").write_text("{}")
        with patch.object(packaging, "command", self.fake_command), patch.object(packaging.subprocess, "run", fake_build):
            packaging.build(self.output, packaging.BUILDER, destination, no_cache=True)
        receipt = json.loads((destination / "packaging-inputs.json").read_text())
        self.assertEqual(receipt["contextDigest"], packaging.sha256(initial))
        self.assertEqual(receipt["releaseStatus"], "blocked")

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
        self.assertFalse(list(self.root.glob(".reakt-oci-*")))

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
                result = subprocess.run([sys.executable, str(ROOT / "scripts/reakt" / script), *arguments], capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertEqual(result.stderr, f"reakt: {label} failed\n")


if __name__ == "__main__":
    unittest.main()
