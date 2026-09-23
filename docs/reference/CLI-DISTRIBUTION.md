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
checkout. Neither packaging nor local compilation produces a deployable release;
deployment still requires a release with signed provenance.

## Verification

Unit tests in `crates/cli/src/assets.rs` build an asset tree and its manifest in
a temporary directory. They check that verification fails when a file is
added, changed or removed, when a symlink escapes the installation, and when
the tree exceeds its entry, size or depth limits. One test removes or extends
each of the compiler, SDK, `deno.lock`, a dependency reached through an
internal link, the runtime crate and the native builder in turn, and checks
that the restored tree passes again. `cuenv task packageCliTest` covers the
packaging script.

No test runs a relocated bundle with the source checkout unreadable, and no
test runs a bundle on a Linux host. Signed package delivery is separate work.
The embedded manifest checks asset integrity relative to the executable; it does
not authenticate the executable's publisher.
