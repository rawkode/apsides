# CLI distribution

Build a relocatable, host-specific Apsides installation with:

```sh
cuenv task packageCli
```

The output is a fresh `work/aps-distribution` directory containing `bin/aps`,
`asset-manifest.json`, and `share/apsides`. Move or archive the whole directory;
add its `bin` directory to `PATH`. Copying only the executable is insufficient.
The task uses Cargo and the workspace-pinned Deno to install compiler dependencies
and build the CLI from the same staged sources as its packaged SDK. Generated
distributions and dependency files remain ignored.

For a development-profile bundle or another output directory:

```sh
cuenv exec -- python3 scripts/package_cli.py \
  --output work/aps-distribution-dev \
  --target-dir work/cli-package-target \
  --profile dev
```

The package supports macOS and Linux and includes platform-specific Oxc and
Rolldown dependencies. Build it on the intended operating system and architecture.
Deno, Cargo, Python, Docker, the prepared native dependency cache and the
build profile's pinned SBOM tool remain external prerequisites for their
respective commands. This is not a self-contained executable.

Distribution builds resolve assets from their installation prefix. They do not
fall back to a source checkout or accept an environment override. Before using
assets, the CLI verifies the generated manifest against its embedded digest and
checks the complete asset tree. Missing, additional or modified files and
escaping symlinks fail verification. Asset traversal permits at most 20,000
entries, depth 64, 128 MiB per file and 512 MiB in total. Restore a damaged
installation from its original bundle rather than editing its manifest.

Development builds made with `cuenv task build` continue to use the source
checkout. Neither packaging nor local compilation satisfies the production
release gate; deployment still requires the complete signed release evidence.

## Verification

The macOS development-profile bundle passed an external-directory rehearsal
after two moves. A sandbox denied reads of the source checkout for every CLI
command: `init`, CNPG `crds fetch`, `lock`, `compile`, and `verify`. The generated
SDK matched the packaged SDK, compiler receipts retained their closed contract,
and generated artifacts contained no checkout paths. Six separate asset changes
or removals were rejected without publishing an initialized project, and each
asset was restored. The local summary is `work/cli-distribution-acceptance.json`; it binds the tested
CLI binary and retained asset manifest.

Repeat this authoring acceptance on macOS with a fresh output outside both the
checkout and the installed prefix:

```sh
cuenv exec -- python3 tests/run_cli_distribution.py \
  --prefix /absolute/path/to/aps-distribution \
  --output /absolute/path/to/acceptance
```

A separate checkout-denied `aps build` also produced a Linux/arm64 native
controller, OCI image and SBOM using the existing pinned build profile. Its
summary is `work/cli-distribution-native-build.json`. This was a local build;
approved provenance and full production release acceptance remain pending.

Linux-host isolation acceptance and signed package delivery remain separate work.
The embedded manifest checks asset integrity relative to the executable; it does
not authenticate the executable's publisher.
