#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "package_cli", ROOT / "scripts/package_cli.py"
)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("package_cli module is unavailable")
package_cli = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package_cli)


class PackageCliTests(unittest.TestCase):
    def test_source_closure_is_exact_and_tracked(self):
        files = set(package_cli.source_files())
        self.assertIn("Cargo.lock", files)
        self.assertIn("rust-toolchain.toml", files)
        self.assertIn("packages/compiler/src/compiler.ts", files)
        self.assertIn("packages/compiler/src/crds.ts", files)
        self.assertIn("packages/compiler/src/source-lock.ts", files)
        self.assertIn("packages/kubernetes/src/model.ts", files)
        self.assertIn("scripts/build.py", files)
        for crate in package_cli.CRATES:
            self.assertIn(f"crates/{crate}/Cargo.toml", files)
            self.assertTrue(any(path.startswith(f"crates/{crate}/src/") for path in files))
        self.assertFalse(any(path.endswith(".test.ts") for path in files))
        self.assertFalse(
            any(path.startswith(f"crates/{crate}/tests/") for crate in package_cli.CRATES for path in files)
        )

    def test_manifest_records_regular_files_and_internal_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "store/pkg").mkdir(parents=True)
            (root / "store/pkg/index.js").write_text("export {};\n")
            (root / "node_modules").mkdir()
            (root / "node_modules/pkg").symlink_to("../store/pkg")
            entries = package_cli.asset_entries(root)
            self.assertEqual(
                entries,
                {
                    "node_modules/pkg": "symlink:../store/pkg",
                    "store/pkg/index.js": "sha256:"
                    "8e609bb71c20b858c77f0e9f90bb1319db8477b13f9f965f1a1e18524bf50881",
                },
            )
            self.assertEqual(json.loads(package_cli.manifest_bytes(entries)), entries)

    def test_manifest_rejects_dangling_and_escaping_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "dangling").symlink_to("missing")
            with self.assertRaises(package_cli.PackagingError):
                package_cli.asset_entries(root)
            (root / "dangling").unlink()
            (root / "escape").symlink_to("../outside")
            with self.assertRaises(package_cli.PackagingError):
                package_cli.asset_entries(root)

    def test_manifest_enforces_entry_depth_and_byte_budgets(self):
        cases = [
            ("MAX_ENTRIES", 1, "entry limit"),
            ("MAX_DEPTH", 0, "depth limit"),
            ("MAX_FILE_BYTES", 2, "oversized file"),
            ("MAX_TREE_BYTES", 5, "total byte limit"),
        ]
        for limit, value, message in cases:
            with self.subTest(limit=limit), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "nested").mkdir()
                (root / "first").write_bytes(b"abc")
                (root / "second").write_bytes(b"def")
                with mock.patch.object(package_cli, limit, value):
                    with self.assertRaisesRegex(package_cli.PackagingError, message):
                        package_cli.asset_entries(root)

    def test_package_rejects_non_posix_platform_before_writes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with mock.patch.object(package_cli.sys, "platform", "win32"):
                with self.assertRaisesRegex(package_cli.PackagingError, "Darwin and Linux"):
                    package_cli.package(root / "prefix", root / "target", "release")
            self.assertFalse((root / "prefix").exists())
            self.assertFalse((root / "target").exists())

    def test_install_uses_the_pinned_deno_and_frozen_lock(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with mock.patch.object(package_cli, "_run") as run:
                run.return_value = f"deno {package_cli.DENO} (stable, release, aarch64-apple-darwin)"
                package_cli._install_javascript(root)
            self.assertEqual(run.call_args_list[0].args[0], ["deno", "--version"])
            self.assertEqual(run.call_args.args[0], ["deno", "install", "--frozen"])
            self.assertEqual(run.call_args.kwargs["cwd"], root)
            self.assertEqual(run.call_args.kwargs["env"]["CI"], "1")

    def test_install_rejects_an_unpinned_deno_host(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with mock.patch.object(package_cli, "_run") as run:
                run.return_value = "deno 2.0.0 (stable, release, aarch64-apple-darwin)"
                with self.assertRaisesRegex(package_cli.PackagingError, "must be Deno"):
                    package_cli._install_javascript(root)

    def test_package_rejects_existing_output_before_commands(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "prefix"
            output.mkdir()
            with self.assertRaisesRegex(package_cli.PackagingError, "output already exists"):
                package_cli.package(output, root / "target", "dev")

    def test_package_creates_a_missing_output_parent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "new/work/prefix"
            with mock.patch.object(
                package_cli,
                "_copy_sources",
                side_effect=package_cli.PackagingError("stop after parent creation"),
            ):
                with self.assertRaisesRegex(package_cli.PackagingError, "stop after parent"):
                    package_cli.package(output, root / "target", "dev")
            self.assertTrue(output.parent.is_dir())
            self.assertFalse(output.exists())

    def test_dev_profile_reads_the_cargo_debug_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            executable = root / "target/debug/aps"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"aps")
            with mock.patch.object(package_cli, "_run") as run:
                selected = package_cli._build_cli(root, root / "target", "dev", "a" * 64)
            self.assertEqual(selected, executable)
            self.assertIn("dev", run.call_args.args[0])


if __name__ == "__main__":
    unittest.main()
