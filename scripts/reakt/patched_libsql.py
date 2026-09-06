#!/usr/bin/env python3
"""Prepare and verify the pinned, locally patched libsql source tree."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parents[2]
RECEIPT = ".apsides-patch.json"
RECEIPT_SCHEMA = "apsides.dev/patched-crate/v1"
MAX_ARCHIVE_BYTES = 64 * 1024 * 1024
MAX_EXTRACTED_BYTES = 256 * 1024 * 1024
MAX_FILE_BYTES = 128 * 1024 * 1024
MAX_FILES = 4096
MAX_METADATA_BYTES = 32 * 1024 * 1024


@dataclass(frozen=True)
class PatchSpec:
    name: str
    version: str
    archive_sha256: str
    patched_files: tuple[str, ...]

    @property
    def directory(self) -> str:
        return f"{self.name}-{self.version}"


DEFAULT_SPEC = PatchSpec(
    name="libsql",
    version="0.9.30",
    archive_sha256="30fe980ac5693ed1f3db490559fb578885e913a018df64af8a1a46e1959a78df",
    patched_files=(
        "Cargo.toml",
        "src/hrana/hyper.rs",
        "src/hrana/mod.rs",
        "src/hrana/stream.rs",
    ),
)
DEFAULT_PATCH = ROOT / "scripts/reakt/patches/libsql-0.9.30.patch"
DEFAULT_OUTPUT = ROOT / ".generated/libsql-0.9.30"
DEFAULT_DOWNLOAD = ROOT / ".generated/downloads/libsql-0.9.30.crate"
DEFAULT_ARCHIVE_URL = "https://static.crates.io/crates/libsql/libsql-0.9.30.crate"


def _regular_bytes(path: Path, limit: int) -> bytes:
    if path.is_symlink() or not path.is_file():
        raise ValueError("patched dependency input must be a regular non-symlink file")
    size = path.stat().st_size
    if size > limit:
        raise ValueError("patched dependency input exceeds its bound")
    return path.read_bytes()


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _safe_relative(value: str) -> PurePosixPath:
    path = PurePosixPath(value)
    if (
        not value
        or "\x00" in value
        or path.is_absolute()
        or value != path.as_posix()
        or any(part in {"", ".", ".."} for part in path.parts)
    ):
        raise ValueError("archive contains a noncanonical path")
    return path


def _extract_archive(archive: Path, destination: Path, spec: PatchSpec) -> Path:
    archive_bytes = _regular_bytes(archive, MAX_ARCHIVE_BYTES)
    if _sha256(archive_bytes) != spec.archive_sha256:
        raise ValueError("patched dependency archive checksum mismatch")

    seen: set[str] = set()
    total = 0
    root_name = spec.directory
    with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode="r|gz") as source:
        for count, member in enumerate(source, start=1):
            if count > MAX_FILES:
                raise ValueError("patched dependency archive contains too many entries")
            relative = _safe_relative(member.name)
            if relative.parts[0] != root_name:
                raise ValueError("archive entry is outside the expected crate root")
            canonical = relative.as_posix()
            if canonical in seen:
                raise ValueError("archive contains a duplicate path")
            seen.add(canonical)
            target = destination.joinpath(*relative.parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                target.chmod(0o755)
                continue
            if not member.isfile() or member.size < 0 or member.size > MAX_FILE_BYTES:
                raise ValueError("archive contains an unsupported entry")
            total += member.size
            if total > MAX_EXTRACTED_BYTES:
                raise ValueError("patched dependency archive exceeds its extracted bound")
            target.parent.mkdir(parents=True, exist_ok=True)
            extracted = source.extractfile(member)
            if extracted is None:
                raise ValueError("archive regular file cannot be read")
            remaining = member.size
            with target.open("xb") as output:
                while remaining:
                    chunk = extracted.read(min(1024 * 1024, remaining))
                    if not chunk:
                        raise ValueError("archive regular file is truncated")
                    output.write(chunk)
                    remaining -= len(chunk)
                if extracted.read(1):
                    raise ValueError("archive regular file exceeds its declared size")
            target.chmod(0o755 if member.mode & 0o111 else 0o644)
    crate = destination / root_name
    if not crate.is_dir() or not (crate / "Cargo.toml").is_file():
        raise ValueError("archive does not contain the expected crate")
    return crate


def _patch_targets(patch: bytes, spec: PatchSpec) -> None:
    try:
        text = patch.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError("dependency patch must be UTF-8") from error
    targets: list[str] = []
    before_paths: list[str] = []
    after_paths: list[str] = []
    forbidden = ("new file mode ", "deleted file mode ", "old mode ", "new mode ", "GIT binary patch")
    for line in text.splitlines():
        if line.startswith(forbidden):
            raise ValueError("dependency patch changes the supported file shape")
        if line.startswith("diff --git "):
            fields = line.split(" ")
            if len(fields) != 4 or not fields[2].startswith("a/") or not fields[3].startswith("b/"):
                raise ValueError("dependency patch header is malformed")
            before, after = fields[2][2:], fields[3][2:]
            if before != after:
                raise ValueError("dependency patch renames a file")
            _safe_relative(before)
            targets.append(before)
        elif line.startswith("--- "):
            before_paths.append(line.removeprefix("--- a/"))
        elif line.startswith("+++ "):
            after_paths.append(line.removeprefix("+++ b/"))
    if targets and tuple(targets) != spec.patched_files:
        raise ValueError("dependency patch targets differ from the approved files")
    if tuple(before_paths) != spec.patched_files or tuple(after_paths) != spec.patched_files:
        raise ValueError("dependency patch targets differ from the approved files")


def _apply_patch(crate: Path, patch: Path, spec: PatchSpec) -> str:
    patch_bytes = _regular_bytes(patch, 1024 * 1024)
    _patch_targets(patch_bytes, spec)
    environment = {**os.environ, "LC_ALL": "C", "LANG": "C"}
    arguments = ["git", "apply", "--whitespace=error-all", "-"]
    checked = subprocess.run(
        [*arguments[:2], "--check", *arguments[2:]],
        cwd=crate,
        env=environment,
        check=False,
        capture_output=True,
        input=patch_bytes,
    )
    if checked.returncode:
        raise ValueError("dependency patch does not apply exactly")
    applied = subprocess.run(
        arguments,
        cwd=crate,
        env=environment,
        check=False,
        capture_output=True,
        input=patch_bytes,
    )
    if applied.returncode:
        raise ValueError("dependency patch application failed")
    return _sha256(patch_bytes)


def _tree(crate: Path) -> tuple[str, int]:
    entries = []
    total = 0
    for directory, directories, files in os.walk(crate, followlinks=False):
        here = Path(directory)
        if here.is_symlink() or any((here / name).is_symlink() for name in directories):
            raise ValueError("generated dependency contains a symlink directory")
        for name in files:
            path = here / name
            relative = path.relative_to(crate).as_posix()
            if relative == RECEIPT:
                continue
            data = _regular_bytes(path, MAX_FILE_BYTES)
            total += len(data)
            if total > MAX_EXTRACTED_BYTES:
                raise ValueError("generated dependency exceeds its extracted bound")
            entries.append(
                {
                    "path": relative,
                    "mode": path.stat().st_mode & 0o777,
                    "size": len(data),
                    "sha256": _sha256(data),
                }
            )
    if not entries or len(entries) > MAX_FILES:
        raise ValueError("generated dependency file set is invalid")
    encoded = json.dumps(
        sorted(entries, key=lambda item: item["path"]), separators=(",", ":")
    ).encode()
    return _sha256(encoded), len(entries)


def _receipt(crate: Path) -> dict[str, object]:
    data = json.loads(_regular_bytes(crate / RECEIPT, 64 * 1024))
    if not isinstance(data, dict):
        raise ValueError("generated dependency receipt is invalid")
    return data


def _candidate_receipt(crate: Path, spec: PatchSpec, patch: Path) -> dict[str, object]:
    if crate.is_symlink() or not crate.is_dir():
        raise ValueError("generated dependency must be a directory, not a symlink")
    crate = crate.resolve()
    receipt = _receipt(crate)
    tree_sha256, files = _tree(crate)
    patch_bytes = _regular_bytes(patch, 1024 * 1024)
    _patch_targets(patch_bytes, spec)
    expected = {
        "schemaVersion": RECEIPT_SCHEMA,
        "crate": spec.name,
        "version": spec.version,
        "archiveSha256": spec.archive_sha256,
        "patchSha256": _sha256(patch_bytes),
        "treeSha256": tree_sha256,
        "files": files,
    }
    if receipt != expected:
        raise ValueError("generated dependency differs from its receipt")
    return receipt


def _expected_receipt(archive: Path, patch: Path, spec: PatchSpec) -> dict[str, object]:
    with tempfile.TemporaryDirectory(prefix=f".{spec.directory}-expected-") as temporary:
        crate = _extract_archive(archive, Path(temporary), spec)
        patch_sha256 = _apply_patch(crate, patch, spec)
        tree_sha256, files = _tree(crate)
        return {
            "schemaVersion": RECEIPT_SCHEMA,
            "crate": spec.name,
            "version": spec.version,
            "archiveSha256": spec.archive_sha256,
            "patchSha256": patch_sha256,
            "treeSha256": tree_sha256,
            "files": files,
        }


def verify_generated(
    crate: Path,
    spec: PatchSpec = DEFAULT_SPEC,
    patch: Path = DEFAULT_PATCH,
    archive: Path | None = None,
) -> dict[str, object]:
    actual = _candidate_receipt(crate, spec, patch)
    expected = _expected_receipt(archive or _find_archive(spec), patch, spec)
    if actual != expected:
        raise ValueError("generated dependency is not the deterministic patched source")
    return actual


def _find_archive(spec: PatchSpec, cargo_home: Path | None = None) -> Path:
    home = cargo_home or Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    candidates = sorted((home / "registry/cache").glob(f"*/{spec.directory}.crate"))
    if spec == DEFAULT_SPEC:
        candidates.append(DEFAULT_DOWNLOAD)
    valid = []
    for path in candidates:
        try:
            if _sha256(_regular_bytes(path, MAX_ARCHIVE_BYTES)) == spec.archive_sha256:
                valid.append(path)
        except ValueError:
            continue
    if not valid:
        raise ValueError("locked dependency archive is unavailable from the Cargo cache")
    return valid[0]


class _RejectRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, file_pointer, code, message, headers, new_url):
        del request, file_pointer, code, message, headers, new_url
        return None


def fetch_archive(
    destination: Path = DEFAULT_DOWNLOAD,
    spec: PatchSpec = DEFAULT_SPEC,
    opener=None,
) -> Path:
    if spec != DEFAULT_SPEC and opener is None:
        raise ValueError("network bootstrap supports only the pinned libsql archive")
    destination = destination.absolute()
    if destination.exists() or destination.is_symlink():
        if _sha256(_regular_bytes(destination, MAX_ARCHIVE_BYTES)) != spec.archive_sha256:
            raise ValueError("downloaded dependency archive checksum mismatch")
        return destination
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=".libsql-0.9.30-", dir=destination.parent)
    os.close(descriptor)
    staging = Path(name)
    try:
        client = opener or urllib.request.build_opener(_RejectRedirects())
        with client.open(DEFAULT_ARCHIVE_URL, timeout=30) as response, staging.open("wb") as output:
            if response.geturl() != DEFAULT_ARCHIVE_URL or getattr(response, "status", 200) != 200:
                raise ValueError("dependency archive download changed endpoint or status")
            length = response.headers.get("Content-Length")
            declared = None
            if length is not None:
                try:
                    declared = int(length)
                except ValueError as error:
                    raise ValueError("dependency archive length is invalid") from error
                if declared < 0 or declared > MAX_ARCHIVE_BYTES:
                    raise ValueError("dependency archive download exceeds its bound")
            size = 0
            digest = hashlib.sha256()
            while True:
                chunk = response.read(min(1024 * 1024, MAX_ARCHIVE_BYTES + 1 - size))
                if not chunk:
                    break
                size += len(chunk)
                if size > MAX_ARCHIVE_BYTES:
                    raise ValueError("dependency archive download exceeds its bound")
                digest.update(chunk)
                output.write(chunk)
            if declared is not None and size != declared:
                raise ValueError("dependency archive download length mismatch")
            output.flush()
            os.fsync(output.fileno())
        if digest.hexdigest() != spec.archive_sha256:
            raise ValueError("downloaded dependency archive checksum mismatch")
        try:
            os.link(staging, destination)
        except FileExistsError:
            if _sha256(_regular_bytes(destination, MAX_ARCHIVE_BYTES)) != spec.archive_sha256:
                raise ValueError("downloaded dependency archive checksum mismatch")
        return destination
    finally:
        staging.unlink(missing_ok=True)


def prepare(
    archive: Path | None = None,
    patch: Path = DEFAULT_PATCH,
    output: Path = DEFAULT_OUTPUT,
    spec: PatchSpec = DEFAULT_SPEC,
) -> dict[str, object]:
    output = output.absolute()
    if output.exists() or output.is_symlink():
        return verify_generated(output, spec, patch, archive)
    archive = archive or _find_archive(spec)
    output.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{spec.directory}-", dir=output.parent))
    try:
        crate = _extract_archive(archive, staging, spec)
        patch_sha256 = _apply_patch(crate, patch, spec)
        tree_sha256, files = _tree(crate)
        receipt = {
            "schemaVersion": RECEIPT_SCHEMA,
            "crate": spec.name,
            "version": spec.version,
            "archiveSha256": spec.archive_sha256,
            "patchSha256": patch_sha256,
            "treeSha256": tree_sha256,
            "files": files,
        }
        (crate / RECEIPT).write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
        try:
            crate.rename(output)
        except FileExistsError:
            return verify_generated(output, spec, patch, archive)
        return verify_generated(output, spec, patch, archive)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def copy_generated(
    source: Path,
    destination: Path,
    spec: PatchSpec = DEFAULT_SPEC,
    patch: Path = DEFAULT_PATCH,
    archive: Path | None = None,
) -> dict[str, object]:
    expected = verify_generated(source, spec, patch, archive)
    destination = destination.absolute()
    if destination.exists() or destination.is_symlink():
        raise ValueError("patched dependency destination already exists")
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{spec.directory}-copy-", dir=destination.parent))
    copied = staging / spec.directory
    try:
        shutil.copytree(source, copied, symlinks=False)
        if verify_generated(copied, spec, patch, archive) != expected:
            raise ValueError("copied dependency receipt changed")
        copied.rename(destination)
        return expected
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def cargo_patch_config(relative_path: str, spec: PatchSpec = DEFAULT_SPEC) -> str:
    path = _safe_relative(relative_path)
    if any(character in relative_path for character in {'"', "'", "\\"}):
        raise ValueError("Cargo patch path contains an unsupported character")
    return f'[patch.crates-io]\n{spec.name} = {{ path = "{path.as_posix()}" }}\n'


def validate_cargo_metadata(
    encoded: bytes,
    expected_manifest: Path,
    spec: PatchSpec = DEFAULT_SPEC,
    require_resolved: bool = True,
) -> None:
    if len(encoded) > MAX_METADATA_BYTES:
        raise ValueError("Cargo metadata exceeds its bound")
    try:
        metadata = json.loads(encoded)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError("Cargo metadata is invalid") from error
    if not isinstance(metadata, dict) or not isinstance(metadata.get("packages"), list):
        raise ValueError("Cargo metadata is invalid")
    named = [
        package
        for package in metadata.get("packages", [])
        if isinstance(package, dict) and package.get("name") == spec.name
    ]
    if not named and not require_resolved:
        return
    packages = [package for package in named if package.get("version") == spec.version]
    if len(named) != len(packages):
        raise ValueError("Cargo metadata did not select the patched dependency")
    if expected_manifest.is_symlink() or not expected_manifest.is_file():
        raise ValueError("patched dependency manifest is unavailable")
    manifest = expected_manifest.resolve()
    manifest_path = packages[0].get("manifest_path") if len(packages) == 1 else None
    if (
        len(packages) != 1
        or packages[0].get("source") is not None
        or not isinstance(manifest_path, str)
        or Path(manifest_path).resolve() != manifest
    ):
        raise ValueError("Cargo metadata did not select the patched dependency")
    resolve = metadata.get("resolve")
    package_id = packages[0].get("id")
    nodes = resolve.get("nodes") if isinstance(resolve, dict) else None
    if (
        not isinstance(nodes, list)
        or sum(isinstance(node, dict) and node.get("id") == package_id for node in nodes) != 1
    ):
        raise ValueError("Cargo metadata does not resolve the patched dependency")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("prepare", "verify"), nargs="?", default="prepare")
    parser.add_argument("--fetch", action="store_true")
    arguments = parser.parse_args()
    if arguments.action == "verify" and arguments.fetch:
        parser.error("--fetch is only valid while preparing")
    if arguments.action == "verify":
        receipt = verify_generated(DEFAULT_OUTPUT)
    elif arguments.fetch:
        try:
            archive = _find_archive(DEFAULT_SPEC)
        except ValueError:
            archive = fetch_archive()
        receipt = prepare(archive=archive)
    else:
        receipt = prepare()
    print(json.dumps(receipt, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise SystemExit(f"patched dependency preparation failed: {error}") from None
