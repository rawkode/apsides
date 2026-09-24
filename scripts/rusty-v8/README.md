# rusty_v8 offline build pins (Phase 5a spike gate evidence)

`pins.json` records the offline build inputs for the `v8` crate (the
denoland/rusty_v8 V8 bindings that `deno_core` 0.412.0 pulls in transitively
through the `deno_v8` facade crate) pinned in `crates/engine/Cargo.toml`: the
prebuilt static-lib archive URL and its SHA-256 for `x86_64-unknown-linux-gnu`
and `aarch64-unknown-linux-gnu`, both downloaded and verified during this
spike (see the Phase 5a report).

This is spike-gate evidence, like `runtime-base/`'s pins are for the base
image, not yet wired into `scripts/build.py`'s `--network=none` build — that
wiring, plus a `RUSTY_V8_ARCHIVE`-setting build step and an SBOM entry, is
Phase 5d ("Build pipeline") in `/home/user/apsides-PLAN.md`. Follow
`runtime-base/README.md`'s model when that lands: commit the archive bytes
content-addressed (or fetch-and-verify against `pins.json` in the build
script) so the packaging build never contacts the network, and re-verify
`pins.json` by hand (`sha256sum`) if `crates/engine/Cargo.toml`'s `deno_core`
pin ever moves to a new `v8` crate version.

No separate binding-file pin is needed for either target: see
`bindingFile.reason` in `pins.json`.
