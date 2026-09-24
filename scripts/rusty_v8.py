#!/usr/bin/env python3
"""Fetch, cache and verify the pinned rusty_v8 (`v8` crate) prebuilt static
archive that `crates/engine`'s `deno_core` dependency needs to compile.

`crates/runtime` now depends on `crates/engine`, which pins `deno_core`,
which pulls in the `v8` crate (denoland/rusty_v8). That crate's `build.rs`
downloads a prebuilt static-lib archive from GitHub Releases unless
`RUSTY_V8_ARCHIVE` (a local path) is already set -- and the native controller
image is built with `docker buildx build --network=none`, so that download
must never happen inside the build.

This module fetches the archive named in `scripts/rusty-v8/pins.json` for a
given Rust target triple, verifies it against the pinned SHA-256 and against
the `v8` crate version actually locked in `Cargo.lock`, and caches it under
`default_cache_dir()` (never under this checkout or an installed CLI
distribution's asset tree -- see that function) so repeat builds and CI runs
do not re-download it. The archive is never committed; only its pin (URL +
digest, per architecture) is source-controlled. See
`scripts/rusty-v8/README.md`.
"""
import hashlib
import json
import os
from pathlib import Path
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PINS_PATH = ROOT / "scripts/rusty-v8/pins.json"
# Builder image platform (as `docker image inspect`/BuildKit name it) to the
# Rust target triple `v8-*/build.rs` names its prebuilt archives after. Same
# two targets `scripts/rusty-v8/pins.json` pins.
PLATFORM_TARGETS = {
    "linux/arm64": "aarch64-unknown-linux-gnu",
    "linux/amd64": "x86_64-unknown-linux-gnu",
}
# The archive is always staged at this fixed path inside the build context
# regardless of target (only one target's archive is ever staged in a given
# context), so `scripts/Dockerfile` can set `RUSTY_V8_ARCHIVE` as a plain,
# static `ENV` with no build-arg plumbing.
CONTEXT_RELATIVE_PATH = "rusty-v8/librusty_v8.a.gz"


def default_cache_dir():
    """Where fetched archives are cached, resolved fresh on every call
    (never at import time, so a missing HOME only matters if nothing else
    names a cache directory and one is actually needed).

    `$APSIDES_RUSTY_V8_CACHE` if set; else `$XDG_CACHE_HOME/apsides/rusty-v8`;
    else `$HOME/.cache/apsides/rusty-v8`.

    Deliberately never under `ROOT` (`scripts/`'s parent): in an installed
    CLI distribution `ROOT` is `<prefix>/share/apsides`, the exact tree
    `crates/cli/src/assets.rs`'s `assets::root()` hash-verifies in full on
    every `aps` invocation -- writing a cache there would make every `aps`
    command after the first `aps build` fail with "distribution assets do
    not match this binary".
    """
    override = os.environ.get("APSIDES_RUSTY_V8_CACHE")
    if override:
        return Path(override)
    xdg = os.environ.get("XDG_CACHE_HOME")
    if xdg:
        return Path(xdg) / "apsides/rusty-v8"
    home = os.environ.get("HOME")
    if not home:
        raise ValueError(
            "cannot determine a rusty_v8 archive cache directory: set APSIDES_RUSTY_V8_CACHE "
            "(or XDG_CACHE_HOME, or HOME) to a writable directory outside any installed "
            "Apsides CLI distribution prefix"
        )
    return Path(home) / ".cache/apsides/rusty-v8"


def load_pins(path=PINS_PATH):
    pins = json.loads(Path(path).read_text())
    if pins.get("bindingFile", {}).get("required"):
        # The pinned v8 crate version has changed to one that needs a
        # generated binding file for at least one target; wire
        # RUSTY_V8_SRC_BINDING_PATH the same way as the archive below
        # (fetch outside the network-less build, verify, stage, set the env
        # var in scripts/Dockerfile) before building. Failing closed here
        # rather than silently compiling without it.
        raise ValueError(
            "scripts/rusty-v8/pins.json now requires a rusty_v8 binding file; "
            "scripts/rusty_v8.py and scripts/Dockerfile must be updated to fetch, "
            "verify and wire RUSTY_V8_SRC_BINDING_PATH before building"
        )
    return pins


def archive_pin(target, pins):
    archives = pins.get("archives", {})
    entry = archives.get(target)
    if not isinstance(entry, dict):
        raise ValueError(f"no pinned rusty_v8 archive for target {target!r}")
    url, sha256 = entry.get("url"), entry.get("sha256")
    if not isinstance(url, str) or not url.startswith("https://"):
        raise ValueError(f"pinned rusty_v8 archive URL for {target} must be an explicit https URL")
    if not isinstance(sha256, str) or len(sha256) != 64 or any(c not in "0123456789abcdef" for c in sha256.lower()):
        raise ValueError(f"pinned rusty_v8 archive sha256 for {target} must be 64 lowercase hex characters")
    return {
        "target": target,
        "url": url,
        "sha256": sha256.lower(),
        "name": "librusty_v8",
        "version": pins.get("v8CrateVersion", ""),
    }


def _verify(data, pin):
    actual = hashlib.sha256(data).hexdigest()
    if actual != pin["sha256"]:
        raise ValueError(
            f"rusty_v8 archive for {pin['target']} failed sha256 verification: "
            f"expected {pin['sha256']}, got {actual}"
        )


def fetch(pin, cache_dir=None, opener=urllib.request.urlopen):
    """Return a verified local path to the archive `pin` names.

    Cached under `cache_dir` (`default_cache_dir()` if not given), named by
    the pinned digest. If not cached (or the cached bytes no longer verify),
    fetches from the pinned URL -- network access happens here, never inside
    the `--network=none` Docker build -- and verifies its SHA-256 before
    caching. Raises a clear error if the host is offline and the cache holds
    nothing usable, or if the cache directory cannot be created or written
    (for example a read-only installation prefix).
    """
    cache_dir = Path(cache_dir) if cache_dir is not None else default_cache_dir()
    try:
        cache_dir.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ValueError(
            f"cannot create the rusty_v8 archive cache directory {cache_dir} ({error}); set "
            "APSIDES_RUSTY_V8_CACHE to a writable directory"
        ) from None
    cached = cache_dir / f"{pin['sha256']}.a.gz"
    if cached.is_file():
        try:
            data = cached.read_bytes()
        except OSError as error:
            raise ValueError(f"cannot read the cached rusty_v8 archive {cached} ({error})") from None
        if hashlib.sha256(data).hexdigest() == pin["sha256"]:
            return cached
        # A cached file that no longer matches its name is corrupt, not a
        # legitimate cache entry; fall through and re-fetch instead of
        # trusting it.
    try:
        with opener(pin["url"], timeout=120) as response:  # noqa: S310 -- fixed https URL from source-controlled pins.json
            data = response.read()
    except (urllib.error.URLError, OSError, ValueError) as error:
        raise ValueError(
            f"rusty_v8 archive for {pin['target']} is not cached under {cache_dir} and fetching "
            f"{pin['url']} failed ({error}); connect to the network once to populate the cache, or "
            "restore a previously fetched archive into that directory"
        ) from None
    _verify(data, pin)
    tmp = cached.with_name(cached.name + ".tmp")
    try:
        tmp.write_bytes(data)
        tmp.replace(cached)
    except OSError as error:
        raise ValueError(
            f"cannot write the fetched rusty_v8 archive into {cache_dir} ({error}); set "
            "APSIDES_RUSTY_V8_CACHE to a writable directory"
        ) from None
    return cached


def locked_version(lock_bytes):
    """The `v8` crate's version pinned in a parsed `Cargo.lock`, or `None`
    if it names no `v8` package."""
    document = tomllib.loads(lock_bytes.decode("utf-8"))
    versions = {package["version"] for package in document.get("package", []) if package.get("name") == "v8"}
    if len(versions) > 1:
        raise ValueError("controller Cargo.lock pins more than one v8 crate version")
    return next(iter(versions), None)


def ensure_pin_matches_lock(lock_bytes, pins):
    """Fail closed if the pinned `v8CrateVersion` in `pins` differs from the
    `v8` crate version actually locked in `Cargo.lock`. Without this, a
    Cargo.lock bump could silently link a different `v8` crate build against
    an archive fetched for the wrong version.
    """
    locked = locked_version(lock_bytes)
    pinned = pins.get("v8CrateVersion")
    if locked is None:
        raise ValueError("controller Cargo.lock does not lock the v8 crate; cannot verify the rusty_v8 archive pin")
    if locked != pinned:
        raise ValueError(
            f"scripts/rusty-v8/pins.json pins v8 crate version {pinned!r} but controller/Cargo.lock "
            f"locks {locked!r}; update pins.json's archive URLs and SHA-256 digests to match the "
            "locked v8 crate version before building"
        )


def resolve(target, lock_bytes=None, cache_dir=None, pins_path=PINS_PATH, opener=urllib.request.urlopen):
    """Load the pin for `target`, optionally cross-check it against a
    `Cargo.lock`'s locked `v8` crate version, fetch/verify its archive, and
    return `(local_path, pin)`."""
    pins = load_pins(pins_path)
    if lock_bytes is not None:
        ensure_pin_matches_lock(lock_bytes, pins)
    pin = archive_pin(target, pins)
    return fetch(pin, cache_dir=cache_dir, opener=opener), pin
