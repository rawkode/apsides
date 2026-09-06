#!/usr/bin/env python3
"""Unit checks for the cached-only inventory PVC image builder."""

import importlib.util
import contextlib
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("build.py")
SPEC = importlib.util.spec_from_file_location("storage_build", SCRIPT)
storage_build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(storage_build)

WORKSPACE_MANIFEST = """[workspace]
resolver = "3"
members = ["crates/compiler-core", "crates/deploy", "crates/inventory", "crates/coordination"]

[workspace.package]
version = "0.0.0"
edition = "2024"
rust-version = "1.98"
license = "Apache-2.0"
repository = "https://github.com/rawkode/apsides"

[workspace.lints.rust]
unsafe_code = "deny"

[workspace.lints.clippy]
all = "warn"
pedantic = "warn"
"""


class StorageBuildTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="storage-build-test-")
        self.repository = Path(self.temporary.name) / "repo"
        self.repository.mkdir()
        (self.repository / "work").mkdir()
        (self.repository / "Cargo.toml").write_text(WORKSPACE_MANIFEST)
        (self.repository / "Cargo.lock").write_text("version = 4\n")
        compiler = self.repository / "crates/compiler-core"
        deploy = self.repository / "crates/deploy"
        inventory = self.repository / "crates/inventory"
        coordination = self.repository / "crates/coordination"
        (compiler / "src").mkdir(parents=True)
        (deploy / "src").mkdir(parents=True)
        (inventory / "src").mkdir(parents=True)
        (coordination / "src").mkdir(parents=True)
        (inventory / "tests/support").mkdir(parents=True)
        (coordination / "tests/support").mkdir(parents=True)
        (compiler / "Cargo.toml").write_text(
            '[package]\nname="compiler-core"\nversion="0.1.0"\nedition.workspace=true\n'
        )
        (compiler / "src/lib.rs").write_text("// compiler metadata\n")
        (deploy / "Cargo.toml").write_text(
            '[package]\nname="deploy"\nversion="0.1.0"\nedition.workspace=true\n'
            '[dependencies]\ncompiler-core={path="../compiler-core"}\n'
        )
        (deploy / "src/lib.rs").write_text("// deploy model\n")
        (inventory / "Cargo.toml").write_text(
            '[package]\nname="inventory"\nversion="0.1.0"\nedition.workspace=true\n'
            '[dependencies]\ncompiler-core={path="../compiler-core"}\n'
        )
        (inventory / "src/lib.rs").write_text("// inventory store\n")
        (inventory / "tests/pvc_restart.rs").write_text("mod support;\n")
        (inventory / "tests/support/mod.rs").write_text("// recursive test support\n")
        (coordination / "Cargo.toml").write_text(
            '[package]\nname="coordination"\nversion="0.1.0"\nedition.workspace=true\n'
            '[dependencies]\ncompiler-core={path="../compiler-core"}\ndeploy={path="../deploy"}\n'
            '[dev-dependencies]\ninventory={path="../inventory"}\n'
        )
        (coordination / "src/lib.rs").write_text("// coordination stores\n")
        (coordination / "tests/pvc_restart.rs").write_text("mod support;\n")
        (coordination / "tests/support/mod.rs").write_text("// coordination support\n")
        (self.repository / ".cargo").mkdir()
        (self.repository / ".cargo/credentials.toml").write_text("private-canary\n")
        self.calls = []

    def tearDown(self):
        self.temporary.cleanup()

    def command(self, argv, *, cwd=None, timeout=300):
        self.calls.append([str(value) for value in argv])
        if argv[:2] == ["cargo", "vendor"]:
            (cwd / "vendor/example-1.0.0").mkdir(parents=True)
            (cwd / "vendor/example-1.0.0/Cargo.toml").write_text('[package]\nname="example"\n')
            return (
                '[source.crates-io]\nreplace-with="vendored-sources"\n'
                '[source.vendored-sources]\ndirectory="vendor"\n'
            )
        return "{}"

    def builder(self):
        return {
            "reference": storage_build.BUILDER,
            "platform": storage_build.PLATFORM,
            "localImageId": "sha256:" + "b" * 64,
        }

    def archive(self, path, package="inventory"):
        executable = storage_build.profile(package)["executable"]
        config = json.dumps({
            "architecture": "arm64",
            "os": "linux",
            "config": {"User": "65532:65532", "Entrypoint": [f"/usr/local/bin/{executable}"]},
        }, sort_keys=True, separators=(",", ":")).encode()
        config_digest = storage_build.sha256(config)
        manifest = json.dumps({
            "schemaVersion": 2,
            "config": {"digest": config_digest, "size": len(config)},
            "layers": [],
        }, sort_keys=True, separators=(",", ":")).encode()
        manifest_digest = storage_build.sha256(manifest)
        index = json.dumps({
            "schemaVersion": 2,
            "manifests": [{
                "digest": manifest_digest,
                "size": len(manifest),
                "platform": {"architecture": "arm64", "os": "linux"},
            }],
        }).encode()
        with tarfile.open(path, "w") as archive:
            for name, value in [
                ("index.json", index),
                ("blobs/sha256/" + manifest_digest.removeprefix("sha256:"), manifest),
                ("blobs/sha256/" + config_digest.removeprefix("sha256:"), config),
            ]:
                info = tarfile.TarInfo(name)
                info.size = len(value)
                archive.addfile(info, io.BytesIO(value))
        return manifest_digest, config_digest

    def test_context_is_exact_recursive_and_excludes_host_configuration(self):
        context = self.repository / "prepared"
        with patch.object(storage_build, "run", self.command):
            metadata = storage_build.prepare_context(self.repository, context, self.builder())
        self.assertEqual(storage_build.verify_context(context, metadata), (context / "context.json").read_bytes())
        self.assertEqual((context / "crates/inventory/tests/support/mod.rs").read_text(), "// recursive test support\n")
        self.assertFalse((context / "crates/coordination/tests").exists())
        self.assertFalse((context / ".cargo/credentials.toml").exists())
        self.assertEqual((context / "storage-crash").read_bytes(), storage_build.CRASH_WRAPPER.read_bytes())
        vendor = next(call for call in self.calls if call[:2] == ["cargo", "vendor"])
        metadata_call = next(call for call in self.calls if call[:2] == ["cargo", "metadata"])
        for call in [vendor, metadata_call]:
            self.assertIn("--locked", call)
            self.assertIn("--offline", call)
        self.assertEqual(metadata["builder"]["localImageId"], "sha256:" + "b" * 64)

    def test_coordination_context_copies_only_its_selected_test_tree(self):
        context = self.repository / "coordination-context"
        with patch.object(storage_build, "run", self.command):
            metadata = storage_build.prepare_context(
                self.repository, context, self.builder(), "coordination"
            )
        self.assertEqual(
            storage_build.verify_context(context, metadata, "coordination"),
            (context / "context.json").read_bytes(),
        )
        self.assertEqual(metadata["schemaVersion"], "reakt.dev/coordination-pvc-build-context/v1")
        self.assertEqual(metadata["target"], {"package": "coordination", "test": "pvc_restart"})
        self.assertEqual(
            (context / "crates/coordination/tests/support/mod.rs").read_text(),
            "// coordination support\n",
        )
        self.assertFalse((context / "crates/inventory/tests").exists())

    def test_coordination_profile_rejects_inventory_as_a_build_dependency(self):
        manifest = self.repository / "crates/coordination/Cargo.toml"
        manifest.write_text(manifest.read_text() + '[build-dependencies]\ninventory={path="../inventory"}\n')
        with self.assertRaisesRegex(ValueError, "local dependency closure changed"):
            storage_build.validate_repository(self.repository, "coordination")

    def test_cargo_json_selector_requires_one_exact_executable(self):
        directory = self.repository / "selector"
        directory.mkdir()
        rows = directory / "cargo.json"
        valid = {
            "reason": "compiler-artifact",
            "target": {"name": "pvc_restart", "kind": ["test"], "crate_types": ["bin"]},
            "profile": {"test": True},
            "executable": "/build/target/release/deps/pvc_restart-abc123",
        }
        rows.write_text(json.dumps(valid) + "\n")
        output = io.StringIO()
        with (
            patch.object(sys, "argv", ["select-target.py", str(rows)]),
            patch("pathlib.Path.is_file", return_value=True),
            patch("os.access", return_value=True),
            contextlib.redirect_stdout(output),
        ):
            exec(compile(storage_build.TARGET_SELECTOR, "select-target.py", "exec"), {"__name__": "__main__"})
        self.assertEqual(output.getvalue(), valid["executable"] + "\n")
        rows.write_text(json.dumps(valid) + "\n" + json.dumps(valid) + "\n")
        with (
            patch.object(sys, "argv", ["select-target.py", str(rows)]),
            patch("pathlib.Path.is_file", return_value=True),
            patch("os.access", return_value=True),
            self.assertRaisesRegex(SystemExit, "exactly one"),
        ):
            exec(compile(storage_build.TARGET_SELECTOR, "select-target.py", "exec"), {"__name__": "__main__"})
        valid["target"]["kind"] = ["bin"]
        rows.write_text(json.dumps(valid) + "\n")
        with (
            patch.object(sys, "argv", ["select-target.py", str(rows)]),
            self.assertRaisesRegex(SystemExit, "exactly one"),
        ):
            exec(compile(storage_build.TARGET_SELECTOR, "select-target.py", "exec"), {"__name__": "__main__"})

    def test_context_rejects_external_build_trees_symlinks_and_mutation(self):
        target = self.repository / "crates/inventory/src/target"
        target.mkdir()
        with self.assertRaisesRegex(ValueError, "external build directory"):
            storage_build.prepare_context(self.repository, self.repository / "target-context", self.builder())
        target.rmdir()
        link = self.repository / "crates/inventory/tests/private"
        link.symlink_to(self.repository / ".cargo/credentials.toml")
        with self.assertRaisesRegex(ValueError, "non-symlink"):
            storage_build.prepare_context(self.repository, self.repository / "link-context", self.builder())
        link.unlink()
        context = self.repository / "valid-context"
        with patch.object(storage_build, "run", self.command):
            metadata = storage_build.prepare_context(self.repository, context, self.builder())
        (context / "crates/inventory/src/lib.rs").write_text("// changed\n")
        with self.assertRaisesRegex(ValueError, "files changed"):
            storage_build.verify_context(context, metadata)

    def test_build_requires_fresh_ignored_output_and_exact_cached_platform(self):
        outside = self.repository / "outside"
        with self.assertRaisesRegex(ValueError, "ignored work"):
            storage_build.build(self.repository, outside)
        existing = self.repository / "work/existing"
        existing.mkdir()
        with self.assertRaisesRegex(ValueError, "already exists"):
            storage_build.build(self.repository, existing)
        wrong = dict(self.builder(), platform="linux/amd64")
        with patch.object(storage_build, "pinned_image", return_value=wrong):
            with self.assertRaisesRegex(ValueError, "Linux ARM64"):
                storage_build.build(self.repository, self.repository / "work/wrong-platform")

    def test_build_emits_one_test_only_archive_from_locked_context(self):
        output = self.repository / "work/image"

        def buildkit(argv, check):
            self.assertTrue(check)
            args = [str(value) for value in argv]
            self.assertIn("--pull=false", args)
            self.assertIn("--network=none", args)
            self.assertIn(storage_build.PLATFORM, args)
            self.assertEqual(args[args.index("--target") + 1], "inventory")
            self.assertIn(f"BUILDER_IMAGE={storage_build.BUILDER}", args)
            self.assertIn("PACKAGE=inventory", args)
            metadata = Path(args[args.index("--metadata-file") + 1])
            archive_spec = args[args.index("--output") + 1]
            archive = Path(archive_spec.rsplit("dest=", 1)[1])
            metadata.write_text(json.dumps({"containerimage.digest": "sha256:" + "c" * 64}))
            self.manifest_digest, self.config_digest = self.archive(archive)

        with (
            patch.object(storage_build, "run", self.command),
            patch.object(storage_build, "pinned_image", return_value=self.builder()),
            patch.object(storage_build.subprocess, "run", buildkit),
        ):
            receipt = storage_build.build(self.repository, output)
        self.assertEqual(receipt["qualification"], "test-only; not a production release artifact")
        self.assertEqual(receipt["imageDigest"], "sha256:" + "c" * 64)
        archive_name = storage_build.profile("inventory")["archive"]
        self.assertEqual(receipt["archive"]["sha256"], storage_build.sha256((output / archive_name).read_bytes()))
        self.assertEqual(receipt["platformManifestDigest"], self.manifest_digest)
        self.assertEqual(receipt["configDigest"], self.config_digest)
        self.assertEqual(json.loads((output / "build.json").read_text()), receipt)
        self.assertTrue((output / archive_name).is_file())

    def test_coordination_build_has_distinct_target_archive_schema_and_entrypoint(self):
        output = self.repository / "work/coordination-image"

        def buildkit(argv, check):
            self.assertTrue(check)
            args = [str(value) for value in argv]
            self.assertEqual(args[args.index("--target") + 1], "coordination")
            self.assertIn("PACKAGE=coordination", args)
            metadata = Path(args[args.index("--metadata-file") + 1])
            archive = Path(args[args.index("--output") + 1].rsplit("dest=", 1)[1])
            metadata.write_text(json.dumps({"containerimage.digest": "sha256:" + "d" * 64}))
            self.archive(archive, "coordination")

        with (
            patch.object(storage_build, "run", self.command),
            patch.object(storage_build, "pinned_image", return_value=self.builder()),
            patch.object(storage_build.subprocess, "run", buildkit),
        ):
            receipt = storage_build.build(self.repository, output, "coordination")
        self.assertEqual(receipt["schemaVersion"], "reakt.dev/coordination-pvc-test-image/v1")
        self.assertEqual(receipt["target"], {"package": "coordination", "test": "pvc_restart"})
        self.assertEqual(receipt["archive"]["path"], "coordination-pvc.oci.tar")
        self.assertTrue((output / "coordination-pvc.oci.tar").is_file())

    def test_dockerfile_selects_the_exact_cargo_json_target(self):
        source = storage_build.DOCKERFILE.read_text()
        self.assertIn('cargo test --frozen --offline --release -p "${PACKAGE}" --test pvc_restart --no-run --message-format=json', source)
        self.assertIn("python3 /usr/local/bin/select-target /tmp/cargo.json", source)
        self.assertIn("/usr/local/bin/inventory-pvc-probe", source)
        self.assertIn("/usr/local/bin/coordination-pvc-probe", source)
        self.assertIn("FROM runtime AS inventory", source)
        self.assertIn("FROM runtime AS coordination", source)
        self.assertIn("/usr/local/bin/storage-crash", source)
        self.assertIn("USER 65532:65532", source)

    def test_oci_identity_rejects_wrong_platform_and_tampered_metadata(self):
        archive = self.repository / "valid.oci.tar"
        manifest_digest, config_digest = self.archive(archive)
        self.assertEqual(storage_build.inspect_oci_archive(archive), (manifest_digest, config_digest))
        wrong = self.repository / "wrong.oci.tar"
        with tarfile.open(archive) as source, tarfile.open(wrong, "w") as destination:
            for member in source.getmembers():
                stream = source.extractfile(member)
                value = stream.read() if stream is not None else b""
                if member.name == "index.json":
                    index = json.loads(value)
                    index["manifests"][0]["platform"]["architecture"] = "amd64"
                    value = json.dumps(index).encode()
                    member.size = len(value)
                destination.addfile(member, io.BytesIO(value))
        with self.assertRaisesRegex(ValueError, "platform differs"):
            storage_build.inspect_oci_archive(wrong)


if __name__ == "__main__":
    unittest.main()
