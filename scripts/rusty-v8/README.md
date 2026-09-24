# rusty_v8 offline build pins

`pins.json` records the offline build inputs for the `v8` crate (the
denoland/rusty_v8 V8 bindings that `deno_core` 0.412.0 pulls in transitively
through the `deno_v8` facade crate) pinned in `crates/engine/Cargo.toml`: the
prebuilt static-lib archive URL and its SHA-256 for `x86_64-unknown-linux-gnu`
and `aarch64-unknown-linux-gnu`.

Unlike `runtime-base/`'s pins, the archive bytes themselves are not
committed here (they are a ~40 MB prebuilt binary; the network access
`runtime-base/README.md` argues against for its much smaller image
manifests is unavoidable here at least once). Instead, `scripts/rusty_v8.py`
fetches each target's archive from the `url` below, verifies it against
`sha256`, and caches it under the gitignored `work/rusty-v8-cache/` the
first time it is needed; a cache hit needs no network.
`scripts/package_oci.py`'s `prepare()` calls this before `build()` ever runs
BuildKit, stages the verified archive into the `--network=none` build
context at the fixed path `rusty-v8/librusty_v8.a.gz`, and
`scripts/Dockerfile` points `v8`'s `build.rs` at it with a static
`RUSTY_V8_ARCHIVE` `ENV` (`build.rs` then copies and gzip-decompresses it
directly, with no network access of its own). See
`docs/reference/OCI-PACKAGING.md`'s "The pinned rusty_v8 archive" section
for the full pipeline, including `build-inputs.json` and SBOM recording.

If the host is offline and nothing is cached, `scripts/rusty_v8.py` fails
with a clear error rather than falling through to `v8`'s own build.rs
network download (which would fail anyway, since the Cargo build itself
runs with `--network=none`).

Re-verify `pins.json` by hand (`sha256sum`) if `crates/engine/Cargo.toml`'s
`deno_core` pin ever moves to a new `v8` crate version, and delete the stale
entry from `work/rusty-v8-cache/` (or just clear the directory) so the next
build re-fetches the newly pinned archive.

No separate binding-file pin is needed for either target today: see
`bindingFile.reason` in `pins.json`. If a future `v8` crate version needs
one, `rusty_v8.load_pins()` fails closed (rather than silently compiling
without it) until `scripts/rusty_v8.py`, `scripts/package_oci.py` and
`scripts/Dockerfile` are extended to fetch, verify and wire
`RUSTY_V8_SRC_BINDING_PATH` the same way as the archive.
