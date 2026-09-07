#!/usr/bin/env python3
"""Build a relocatable aps prefix from one explicit tracked source closure."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
PNPM = "pnpm@11.22.0"
MAX_ENTRIES = 20_000
MAX_DEPTH = 64
MAX_FILE_BYTES = 128 * 1024 * 1024
MAX_TREE_BYTES = 512 * 1024 * 1024
HASH_CHUNK_BYTES = 1024 * 1024
CRATES = (
    "cli",
    "compiler-core",
    "coordination",
    "deploy",
    "inventory",
    "platform",
    "release",
    "runtime",
)
STATIC_SOURCES = (
    "Cargo.lock",
    "Cargo.toml",
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "rust-toolchain.toml",
    "packages/compiler/package.json",
    "packages/compiler/src/compiler.ts",
    "packages/compiler/src/crds.ts",
    "packages/compiler/src/fixtures.ts",
    "packages/compiler/src/source-lock.ts",
    "packages/reakt/package.json",
    "packages/reakt/src/compiler.ts",
    "packages/reakt/src/index.ts",
    "packages/reakt/src/jsx-dev-runtime.ts",
    "packages/reakt/src/jsx-runtime.ts",
    "packages/reakt/src/model.ts",
    "packages/reakt/src/runtime.ts",
    "scripts/reakt/Dockerfile",
    "scripts/reakt/build.py",
    "scripts/reakt/extract_binary.py",
    "scripts/reakt/finalize_inputs.py",
    "scripts/reakt/package_oci.py",
    "scripts/reakt/sbom.py",
    "scripts/reakt/toolchain_snapshot.py",
)


class PackagingError(Exception):
    """A bounded distribution packaging failure."""


def _run(argv: list[str], *, cwd: Path, env: dict[str, str] | None = None) -> str:
    result = subprocess.run(
        argv,
        cwd=cwd,
        env=env,
        check=False,
        capture_output=True,
        text=True,
        timeout=900,
    )
    if result.returncode:
        raise PackagingError(f"{Path(argv[0]).name} failed with exit code {result.returncode}")
    return result.stdout


def _git_files(arguments: list[str]) -> list[str]:
    result = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "-z", "--", *arguments],
        check=False,
        capture_output=True,
        timeout=30,
    )
    if result.returncode:
        raise PackagingError("unable to read the tracked distribution source closure")
    return sorted(item.decode("utf-8") for item in result.stdout.split(b"\0") if item)


def source_files() -> tuple[str, ...]:
    """Return the closed, tracked source set used to build and run the CLI."""
    recursive = _git_files([*(f"crates/{crate}/src" for crate in CRATES)])
    expected = [*STATIC_SOURCES, *(f"crates/{crate}/Cargo.toml" for crate in CRATES)]
    tracked = set(_git_files(expected))
    if tracked != set(expected):
        missing = sorted(set(expected) - tracked)
        raise PackagingError(f"distribution source is not tracked: {missing[0]}")
    files = sorted({*expected, *recursive})
    if not recursive or any(not item.startswith(tuple(f"crates/{crate}/src/" for crate in CRATES)) for item in recursive):
        raise PackagingError("invalid tracked Rust source closure")
    return tuple(files)


def _copy_sources(destination: Path) -> None:
    for relative in source_files():
        source = ROOT / relative
        metadata = source.lstat()
        if not stat.S_ISREG(metadata.st_mode) or source.is_symlink():
            raise PackagingError(f"distribution source must be a regular file: {relative}")
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        if source.read_bytes() != target.read_bytes():
            raise PackagingError(f"distribution source copy changed bytes: {relative}")


def asset_entries(share: Path) -> dict[str, str]:
    """Hash every regular file and record every internal relative symlink."""
    root = share.resolve(strict=True)
    entries: dict[str, str] = {}
    regular: list[tuple[Path, os.stat_result]] = []
    entry_count = 0
    tree_bytes = 0
    for directory, directories, files in os.walk(root, topdown=True, followlinks=False):
        here = Path(directory)
        if len(here.relative_to(root).parts) > MAX_DEPTH:
            raise PackagingError("asset closure exceeds the directory depth limit")
        for name in sorted(list(directories)):
            path = here / name
            entry_count += 1
            if entry_count > MAX_ENTRIES:
                raise PackagingError("asset closure exceeds the entry limit")
            if len(path.relative_to(root).parts) > MAX_DEPTH:
                raise PackagingError("asset closure exceeds the directory depth limit")
            if path.is_symlink():
                directories.remove(name)
                _record_symlink(root, path, entries)
            elif not path.is_dir():
                raise PackagingError("asset closure contains an unsupported directory entry")
        directories.sort()
        for name in sorted(files):
            path = here / name
            entry_count += 1
            if entry_count > MAX_ENTRIES:
                raise PackagingError("asset closure exceeds the entry limit")
            if len(path.relative_to(root).parts) > MAX_DEPTH:
                raise PackagingError("asset closure exceeds the directory depth limit")
            if path.is_symlink():
                _record_symlink(root, path, entries)
                continue
            metadata = path.lstat()
            if not stat.S_ISREG(metadata.st_mode):
                raise PackagingError("asset closure contains an unsupported file type")
            if metadata.st_size > MAX_FILE_BYTES:
                raise PackagingError("asset closure contains an oversized file")
            tree_bytes += metadata.st_size
            if tree_bytes > MAX_TREE_BYTES:
                raise PackagingError("asset closure exceeds the total byte limit")
            regular.append((path, metadata))
    for path, metadata in regular:
        relative = path.relative_to(root).as_posix()
        entries[relative] = f"sha256:{_hash_regular(path, metadata)}"
    return dict(sorted(entries.items()))


def _hash_regular(path: Path, expected: os.stat_result) -> str:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    digest = hashlib.sha256()
    size = 0
    try:
        opened = os.fstat(descriptor)
        if not _same_file(expected, opened):
            raise PackagingError("asset file changed before hashing")
        while True:
            chunk = os.read(descriptor, HASH_CHUNK_BYTES)
            if not chunk:
                break
            size += len(chunk)
            if size > expected.st_size or size > MAX_FILE_BYTES:
                raise PackagingError("asset file changed size while hashing")
            digest.update(chunk)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    current = path.lstat()
    if size != expected.st_size or not _same_file(expected, after) or not _same_file(expected, current):
        raise PackagingError("asset file changed while hashing")
    return digest.hexdigest()


def _same_file(left: os.stat_result, right: os.stat_result) -> bool:
    return (
        left.st_dev,
        left.st_ino,
        left.st_mode,
        left.st_size,
        left.st_mtime_ns,
    ) == (
        right.st_dev,
        right.st_ino,
        right.st_mode,
        right.st_size,
        right.st_mtime_ns,
    )


def _record_symlink(root: Path, path: Path, entries: dict[str, str]) -> None:
    target = os.readlink(path)
    if Path(target).is_absolute():
        raise PackagingError("asset closure contains an absolute symlink")
    try:
        resolved = path.resolve(strict=True)
        resolved.relative_to(root)
    except (FileNotFoundError, RuntimeError, ValueError) as error:
        raise PackagingError("asset symlink is dangling or escapes the share root") from error
    entries[path.relative_to(root).as_posix()] = f"symlink:{target}"


def manifest_bytes(entries: dict[str, str]) -> bytes:
    return (json.dumps(entries, indent=2, sort_keys=True) + "\n").encode("utf-8")


def _install_javascript(share: Path) -> None:
    package = json.loads((share / "package.json").read_text())
    if package.get("packageManager") != PNPM:
        raise PackagingError(f"workspace must pin {PNPM}")
    environment = os.environ.copy()
    environment["CI"] = "1"
    print("aps: installing pinned source compiler dependencies", flush=True)
    _run(
        ["pnpm", "dlx", PNPM, "install", "--frozen-lockfile", "--prod"],
        cwd=share,
        env=environment,
    )


def _build_cli(share: Path, target_dir: Path, profile: str, digest: str) -> Path:
    environment = os.environ.copy()
    environment["APS_ASSET_MANIFEST_SHA256"] = digest
    print("aps: building the distribution CLI", flush=True)
    _run(
        [
            "cargo",
            "build",
            "--locked",
            "-p",
            "cli",
            "--features",
            "distribution",
            "--profile",
            profile,
            "--target-dir",
            str(target_dir),
        ],
        cwd=share,
        env=environment,
    )
    profile_directory = "debug" if profile == "dev" else "release"
    executable = target_dir / profile_directory / ("aps.exe" if os.name == "nt" else "aps")
    if executable.is_symlink() or not executable.is_file():
        raise PackagingError("Cargo did not produce the aps executable")
    return executable


def package(output: Path, target_dir: Path, profile: str) -> None:
    if sys.platform != "darwin" and not sys.platform.startswith("linux"):
        raise PackagingError("CLI distribution packaging supports Darwin and Linux only")
    output = output.absolute()
    target_dir = target_dir.absolute()
    if output.exists() or output.is_symlink():
        raise PackagingError("output already exists; choose a fresh prefix")
    if profile not in {"dev", "release"}:
        raise PackagingError("profile must be dev or release")
    output.parent.mkdir(parents=True, exist_ok=True)
    parent = output.parent.resolve(strict=True)
    if not parent.is_dir():
        raise PackagingError("output parent must be a real directory")
    target_dir.mkdir(parents=True, exist_ok=True)
    target_dir = target_dir.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix=f".{output.name}-", dir=parent) as temporary:
        stage = Path(temporary)
        share = stage / "share/apsides"
        share.mkdir(parents=True)
        _copy_sources(share)
        _install_javascript(share)
        entries = asset_entries(share)
        raw_manifest = manifest_bytes(entries)
        digest = hashlib.sha256(raw_manifest).hexdigest()
        (stage / "asset-manifest.json").write_bytes(raw_manifest)
        executable = _build_cli(share, target_dir, profile, digest)
        binary = stage / "bin/aps"
        binary.parent.mkdir()
        shutil.copyfile(executable, binary)
        binary.chmod(0o755)
        if asset_entries(share) != entries or manifest_bytes(entries) != raw_manifest:
            raise PackagingError("distribution assets changed while building the CLI")
        if hashlib.sha256((stage / "asset-manifest.json").read_bytes()).hexdigest() != digest:
            raise PackagingError("asset manifest changed while building the CLI")
        if output.exists() or output.is_symlink():
            raise PackagingError("output appeared while packaging")
        stage.rename(output)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--profile", choices=("dev", "release"), default="release")
    arguments = parser.parse_args()
    try:
        package(arguments.output, arguments.target_dir, arguments.profile)
    except (
        OSError,
        PackagingError,
        UnicodeError,
        ValueError,
        json.JSONDecodeError,
        subprocess.TimeoutExpired,
    ):
        raise SystemExit("aps: CLI distribution packaging failed") from None


if __name__ == "__main__":
    main()
