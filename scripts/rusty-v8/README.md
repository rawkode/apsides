# rusty_v8 offline build pins

`pins.json` records the offline build inputs for the `v8` crate (the
denoland/rusty_v8 V8 bindings that `deno_core` 0.412.0 pulls in transitively
through the `deno_v8` facade crate) pinned in `crates/engine/Cargo.toml`: the
prebuilt static-lib archive URL and its SHA-256 for `x86_64-unknown-linux-gnu`
and `aarch64-unknown-linux-gnu`.

Unlike `runtime-base/`'s pins, the archive bytes themselves are not
committed here (they are a ~40 MB prebuilt binary; the network access
`runtime-base/README.md` argues against for its much smaller image
manifests is unavoidable here at least once). Instead,
`crates/packaging/src/rusty_v8.rs` fetches each target's archive from the
`url` below, verifies it against `sha256`, also verifies that
`v8CrateVersion` matches the `v8` crate version actually locked in
`controller/Cargo.lock`, and caches the archive the first time it is needed;
a cache hit needs no network. `aps build` does this while preparing the build
context, before the compile runs, and stages the verified archive at the
fixed path `rusty-v8/librusty_v8.a.gz`. `crates/packaging/src/compile.rs`
mounts it read-only into the `--network=none` compile container and points
`v8`'s `build.rs` at it with `RUSTY_V8_ARCHIVE` (`build.rs` then copies and
gzip-decompresses it directly, with no network access of its own). See
`docs/reference/OCI-PACKAGING.md`'s "The pinned rusty_v8 archive" section
for the full pipeline, including `build-inputs.json` and SBOM recording.

The cache directory is `$APSIDES_RUSTY_V8_CACHE` if set, else
`$XDG_CACHE_HOME/apsides/rusty-v8`, else `$HOME/.cache/apsides/rusty-v8` --
**never** under the assets root (this checkout, or an installed CLI
distribution's `<prefix>/share/apsides`). The latter is the exact tree
`crates/cli/src/assets.rs`'s `assets::root()` hash-verifies in full on every
`aps` invocation. A cache written there would make every `aps` command after
the first `aps build` fail with "distribution assets do not match this
binary", so `aps build` rejects such a cache directory. CI sets
`APSIDES_RUSTY_V8_CACHE` explicitly and caches that path across runs.

If the host is offline and nothing is cached, `aps build` fails with a clear
error rather than falling through to `v8`'s own build.rs network download
(which would fail anyway, since the compile itself runs with
`--network=none`); the same applies if the cache directory cannot be created
or written (for example a read-only installation prefix, should
`APSIDES_RUSTY_V8_CACHE` ever be pointed at one).

Re-verify `pins.json` by hand (`sha256sum`) if `crates/engine/Cargo.toml`'s
`deno_core` pin ever moves to a new `v8` crate version -- `aps build` fails
closed if `v8CrateVersion` and the locked `Cargo.lock` version disagree --
and delete the stale entry from the cache directory (or just clear it) so
the next build re-fetches the newly pinned archive.

No separate binding-file pin is needed for either target today: see
`bindingFile.reason` in `pins.json`. If a future `v8` crate version needs
one, `aps build` fails closed (rather than silently compiling without it)
until `crates/packaging` (`src/rusty_v8.rs` and `src/compile.rs`) is
extended to fetch, verify and pass `RUSTY_V8_SRC_BINDING_PATH` the same way
as the archive.
