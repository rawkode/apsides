#!/usr/bin/env python3
"""Focused tests for deterministic patched dependency preparation."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "scripts/reakt/patched_libsql.py"
MODULE_SPEC = importlib.util.spec_from_file_location("apsides_patched_libsql", MODULE_PATH)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError("patched dependency helper cannot be loaded")
patched = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_SPEC.name] = patched
MODULE_SPEC.loader.exec_module(patched)


class PatchedDependencyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.archive = self.root / "example-1.2.3.crate"
        self.patch = self.root / "example.patch"
        self.output = self.root / "generated/example-1.2.3"
        self._archive(
            [
                ("example-1.2.3", None, tarfile.DIRTYPE),
                ("example-1.2.3/Cargo.toml", b'[package]\nname="example"\nversion="1.2.3"\n', tarfile.REGTYPE),
                ("example-1.2.3/src", None, tarfile.DIRTYPE),
                ("example-1.2.3/src/lib.rs", b'pub const VALUE: &str = "old";\n', tarfile.REGTYPE),
            ]
        )
        self.spec = patched.PatchSpec(
            name="example",
            version="1.2.3",
            archive_sha256=self._digest(self.archive.read_bytes()),
            patched_files=("src/lib.rs",),
        )
        self.patch.write_text(
            "--- a/src/lib.rs\n"
            "+++ b/src/lib.rs\n"
            "@@ -1 +1 @@\n"
            "-pub const VALUE: &str = \"old\";\n"
            "+pub const VALUE: &str = \"new\";\n"
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    @staticmethod
    def _digest(data: bytes) -> str:
        return hashlib.sha256(data).hexdigest()

    def _archive(self, entries: list[tuple[str, bytes | None, bytes]]) -> None:
        with tarfile.open(self.archive, "w:gz") as archive:
            for name, contents, kind in entries:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.mode = 0o755 if kind == tarfile.DIRTYPE else 0o644
                if contents is not None:
                    info.size = len(contents)
                    archive.addfile(info, io.BytesIO(contents))
                else:
                    archive.addfile(info)

    def _prepare(self) -> dict[str, object]:
        return patched.prepare(self.archive, self.patch, self.output, self.spec)

    def test_prepare_reuse_copy_and_receipts_are_exact(self) -> None:
        first = self._prepare()
        self.assertEqual((self.output / "src/lib.rs").read_text(), 'pub const VALUE: &str = "new";\n')
        self.assertEqual(
            patched.verify_generated(self.output, self.spec, self.patch, self.archive), first
        )
        self.assertEqual(self._prepare(), first)

        copied = self.root / "context/patched/example-1.2.3"
        self.assertEqual(
            patched.copy_generated(
                self.output, copied, self.spec, self.patch, self.archive
            ),
            first,
        )
        self.assertEqual(
            patched.verify_generated(copied, self.spec, self.patch, self.archive), first
        )
        with self.assertRaisesRegex(ValueError, "already exists"):
            patched.copy_generated(
                self.output, copied, self.spec, self.patch, self.archive
            )

    def test_missing_tampered_and_drifted_inputs_fail_without_output(self) -> None:
        missing = self.root / "missing.crate"
        with self.assertRaisesRegex(ValueError, "regular non-symlink"):
            patched.prepare(missing, self.patch, self.output, self.spec)
        self.assertFalse(self.output.exists())

        self.archive.write_bytes(self.archive.read_bytes() + b"tampered")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self._prepare()
        self.assertFalse(self.output.exists())

        self.archive.unlink()
        self._archive(
            [
                ("example-1.2.3", None, tarfile.DIRTYPE),
                ("example-1.2.3/Cargo.toml", b'[package]\nname="example"\nversion="1.2.3"\n', tarfile.REGTYPE),
                ("example-1.2.3/src", None, tarfile.DIRTYPE),
                ("example-1.2.3/src/lib.rs", b'pub const VALUE: &str = "old";\n', tarfile.REGTYPE),
            ]
        )
        self.spec = patched.PatchSpec("example", "1.2.3", self._digest(self.archive.read_bytes()), ("src/lib.rs",))
        self.patch.write_text(self.patch.read_text().replace('"old"', '"absent"'))
        with self.assertRaisesRegex(ValueError, "does not apply exactly"):
            self._prepare()
        self.assertFalse(self.output.exists())

    def test_archive_traversal_and_links_are_rejected(self) -> None:
        for name, kind in [
            ("../outside", tarfile.REGTYPE),
            ("example-1.2.3/link", tarfile.SYMTYPE),
        ]:
            self._archive(
                [
                    ("example-1.2.3", None, tarfile.DIRTYPE),
                    ("example-1.2.3/Cargo.toml", b"[package]\n", tarfile.REGTYPE),
                    (name, b"x" if kind == tarfile.REGTYPE else None, kind),
                ]
            )
            spec = patched.PatchSpec("example", "1.2.3", self._digest(self.archive.read_bytes()), ("src/lib.rs",))
            with self.assertRaisesRegex(ValueError, "noncanonical path|unsupported entry"):
                patched.prepare(self.archive, self.patch, self.output, spec)
            self.assertFalse(self.output.exists())

    def test_generated_output_tampering_fails_closed(self) -> None:
        self._prepare()
        (self.output / "src/lib.rs").write_text("changed after publication\n")
        with self.assertRaisesRegex(ValueError, "differs from its receipt"):
            patched.verify_generated(self.output, self.spec, self.patch, self.archive)
        with self.assertRaisesRegex(ValueError, "differs from its receipt"):
            self._prepare()

        (self.output / patched.RECEIPT).unlink()
        with self.assertRaisesRegex(ValueError, "regular non-symlink"):
            patched.verify_generated(self.output, self.spec, self.patch, self.archive)

    def test_generated_output_is_bound_to_current_patch_bytes(self) -> None:
        self._prepare()
        self.patch.write_text(self.patch.read_text() + "\n")
        with self.assertRaisesRegex(ValueError, "differs from its receipt"):
            patched.verify_generated(self.output, self.spec, self.patch, self.archive)

    def test_fetch_is_explicit_bounded_and_checksum_pinned(self) -> None:
        contents = self.archive.read_bytes()

        class Response(io.BytesIO):
            status = 200

            def __init__(self, body: bytes, url: str) -> None:
                super().__init__(body)
                self.headers = {"Content-Length": str(len(body))}
                self.url = url

            def geturl(self) -> str:
                return self.url

        class Opener:
            def __init__(self, body: bytes, url: str) -> None:
                self.body = body
                self.url = url

            def open(self, url: str, timeout: int) -> Response:
                self.test_case.assertEqual(url, patched.DEFAULT_ARCHIVE_URL)
                self.test_case.assertEqual(timeout, 30)
                return Response(self.body, self.url)

        opener = Opener(contents, patched.DEFAULT_ARCHIVE_URL)
        opener.test_case = self
        destination = self.root / "downloads/example.crate"
        self.assertEqual(
            patched.fetch_archive(destination, self.spec, opener), destination.absolute()
        )
        self.assertEqual(destination.read_bytes(), contents)

        redirected = Opener(contents, "https://example.invalid/example.crate")
        redirected.test_case = self
        with self.assertRaisesRegex(ValueError, "changed endpoint"):
            patched.fetch_archive(self.root / "redirected.crate", self.spec, redirected)

        wrong = Opener(contents + b"tampered", patched.DEFAULT_ARCHIVE_URL)
        wrong.test_case = self
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            patched.fetch_archive(self.root / "wrong.crate", self.spec, wrong)

    def test_forged_source_and_self_consistent_receipt_do_not_verify(self) -> None:
        self._prepare()
        (self.output / "src/lib.rs").write_text("forged source\n")
        receipt_path = self.output / patched.RECEIPT
        receipt = json.loads(receipt_path.read_text())
        receipt["treeSha256"], receipt["files"] = patched._tree(self.output)
        receipt_path.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
        with self.assertRaisesRegex(ValueError, "not the deterministic patched source"):
            patched.verify_generated(self.output, self.spec, self.patch, self.archive)

    def test_patch_shape_and_cargo_config_are_closed(self) -> None:
        self.patch.write_text(self.patch.read_text().replace("src/lib.rs", "src/other.rs"))
        with self.assertRaisesRegex(ValueError, "approved files"):
            self._prepare()
        self.assertEqual(
            patched.cargo_patch_config(".generated/example-1.2.3", self.spec),
            '[patch.crates-io]\nexample = { path = ".generated/example-1.2.3" }\n',
        )
        for value in ["/absolute", "../escape", 'bad"path', "a//b"]:
            with self.assertRaises(ValueError):
                patched.cargo_patch_config(value, self.spec)

    def test_cargo_metadata_must_resolve_exact_path_package(self) -> None:
        manifest = self.output / "Cargo.toml"
        manifest.parent.mkdir(parents=True)
        manifest.write_text("[package]\n")
        package_id = "path+file:///generated/example#1.2.3"
        valid = {
            "packages": [
                {
                    "name": "example",
                    "version": "1.2.3",
                    "id": package_id,
                    "source": None,
                    "manifest_path": str(manifest),
                }
            ],
            "resolve": {"nodes": [{"id": package_id}]},
        }
        encoded = json.dumps(valid).encode()
        patched.validate_cargo_metadata(encoded, manifest, self.spec)

        absent = {"packages": [], "resolve": {"nodes": []}}
        patched.validate_cargo_metadata(
            json.dumps(absent).encode(), manifest, self.spec, require_resolved=False
        )
        with self.assertRaisesRegex(ValueError, "patched dependency"):
            patched.validate_cargo_metadata(json.dumps(absent).encode(), manifest, self.spec)

        wrong_version = {
            "packages": [{"name": "example", "version": "9.9.9"}],
            "resolve": {"nodes": []},
        }
        with self.assertRaisesRegex(ValueError, "patched dependency"):
            patched.validate_cargo_metadata(
                json.dumps(wrong_version).encode(),
                manifest,
                self.spec,
                require_resolved=False,
            )

        for mutation in ("source", "path", "resolve"):
            changed = json.loads(encoded)
            if mutation == "source":
                changed["packages"][0]["source"] = "registry+https://github.com/rust-lang/crates.io-index"
            elif mutation == "path":
                changed["packages"][0]["manifest_path"] = str(self.root / "wrong/Cargo.toml")
            else:
                changed["resolve"]["nodes"] = []
            with self.assertRaisesRegex(ValueError, "patched dependency"):
                patched.validate_cargo_metadata(json.dumps(changed).encode(), manifest, self.spec)
            if mutation == "source":
                with self.assertRaisesRegex(ValueError, "patched dependency"):
                    patched.validate_cargo_metadata(
                        json.dumps(changed).encode(),
                        manifest,
                        self.spec,
                        require_resolved=False,
                    )


if __name__ == "__main__":
    unittest.main()
