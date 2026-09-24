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
given Rust target triple, verifies it against the pinned SHA-256, and caches
it under a gitignored directory (`work/`, per `.gitignore`) so repeat builds
and CI runs do not re-download it. The archive is never committed; only its
pin (URL + digest, per architecture) is source-controlled. See
`scripts/rusty-v8/README.md`.
"""
import hashlib
import json
from pathlib import Path
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PINS_PATH = ROOT / "scripts/rusty-v8/pins.json"
DEFAULT_CACHE_DIR = ROOT / "work/rusty-v8-cache"
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


def fetch(pin, cache_dir=DEFAULT_CACHE_DIR, opener=urllib.request.urlopen):
    """Return a verified local path to the archive `pin` names.

    Cached under `cache_dir`, named by the pinned digest. If not cached (or
    the cached bytes no longer verify), fetches from the pinned URL --
    network access happens here, never inside the `--network=none` Docker
    build -- and verifies its SHA-256 before caching. Raises a clear error
    if the host is offline and the cache holds nothing usable.
    """
    cache_dir = Path(cache_dir)
    cache_dir.mkdir(parents=True, exist_ok=True)
    cached = cache_dir / f"{pin['sha256']}.a.gz"
    if cached.is_file():
        data = cached.read_bytes()
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
    tmp.write_bytes(data)
    tmp.replace(cached)
    return cached


def resolve(target, cache_dir=DEFAULT_CACHE_DIR, pins_path=PINS_PATH, opener=urllib.request.urlopen):
    """Load the pin for `target`, fetch/verify its archive, and return
    `(local_path, pin)`."""
    pin = archive_pin(target, load_pins(pins_path))
    return fetch(pin, cache_dir=cache_dir, opener=opener), pin
