# Native controller OCI packaging

`aps build` turns a verified native artifact into a controller image, a Linux
binary, an SBOM and a completed compiler closure. The whole pipeline runs in
Rust inside `aps` (`crates/packaging`, with engine knowledge in
`crates/container`). Only the cargo compile leaves the process, as one
network-less `podman run` or `docker run` of the pinned builder image. No
image is built in the engine's store, and no Dockerfile, BuildKit or Python is
involved. The controller requires independently protected activation and
policy at startup. Packaging alone does not establish release authenticity or
authorization.

## Complete CLI build

```sh
aps build --source examples/web-application/program.tsx \
  --output dist/build --profile apsides.build.json
```

`aps build` first runs the source compiler and emits the same verified artifact
as `aps compile`, then performs the native build into the same directory. The
build profile uses schema `apsides.dev/build-profile/v1` and exactly these
fields: `builderImage`, `runtimeImage` (both immutable digest references),
`platform` (`linux/arm64` or `linux/amd64`), `sbomTool` (an executable path
relative to the profile or absolute), `sbomToolSha256`, and `sourceDateEpoch`
(an integer from 0 to 8589934591). The `runtimeImage` digest must have registry
pins in `scripts/runtime-base`. The platform, cached image identities,
executable digest, source lock, and compiler closure must agree. Missing cached
dependencies or images fail without a pull fallback. The profile defaults to
`apsides.build.json` in the invoking directory.

The native build runs these steps in order, and stops at the first failure
with `aps: native build failed during <stage>: <detail>; no deployable release
was produced`:

1. Read the profile and identify the four host tools: the container engine,
   host cargo, this `aps` and the profile's SBOM tool, whose bytes must hash to
   `sbomToolSha256`.
2. Confirm that the packaging sources under the assets root (the checkout, or
   a distribution's `share/apsides`) are the ones this `aps` was built from.
   Their digests are embedded at compile time by `crates/packaging/build.rs`;
   a mismatch fails with "this aps was built from different packaging sources
   than … ; rebuild aps".
3. Reserve the outputs and inspect the builder image in the engine's local
   store.
4. Prepare the [build context](#the-build-context), including the pinned
   [rusty_v8 archive](#the-pinned-rusty_v8-archive).
5. Save the [runtime base](#runtime-base) from the engine's store and bind it
   to the committed registry pins.
6. [Compile](#the-compile-run) the controller in the builder image.
7. Assemble the [image](#image-byte-rules) and check it with the same function
   `aps release` uses, write the binary, and generate the SBOM.
8. Complete the [compiler closure](#compiler-closure) and check it as
   `aps release` will.
9. Recheck the host tools, the profile bytes and the context, then write
   `build-result.json`.

A failed build removes the outputs it wrote and keeps the build context for
inspection. Nothing is written under the assets root, and packaging writes
nothing to stdout; the compile's output streams to stderr. On success `aps`
prints one JSON summary with `irDigest`, `output`, `productionReady: false`,
`stage: "build"` and `imageDigest`.

## Container engine

`aps build` uses podman when `podman` is on `PATH` and Docker otherwise. Set
`APSIDES_CONTAINER_ENGINE` to `podman` or `docker` to choose; any other value
is rejected. When neither is on `PATH` the build fails with "neither podman nor
docker is on PATH; install one, or set APSIDES_CONTAINER_ENGINE to the one you
have". `aps test
--cluster` and the acceptance suite choose their engine the same way.

The engine must be local: the compile bind-mounts host directories, and a
probe fails with "the <Engine> daemon cannot see <path>; aps build needs a
local engine" when a remote daemon cannot see them. Rootless and rootful
podman and Docker are supported. A Docker daemon with user-namespace
remapping (`userns-remap`) is rejected, because the build's files would belong
to another host user.

## The build context

The context is staged beside the artifact as `<out>.native-context` (for
`--output dist/build`, `dist/build.native-context`). It is everything the
compile container sees, in five top-level directories:

| Directory | Contents |
| --- | --- |
| `.cargo` | `config.toml`, which replaces crates.io with the vendored sources |
| `controller` | the verified generated controller sources, with the artifact's exact `Cargo.lock` and a `Cargo.toml` whose single runtime path is relocated to `../workspace/crates/runtime` |
| `workspace` | the workspace `Cargo.toml` and the `Cargo.toml` and `src/` of each trusted Apsides crate the controller depends on by path |
| `vendor` | every locked crates.io crate, from `cargo vendor` |
| `rusty-v8` | the pinned `rusty_v8` archive |

Preparation first verifies the artifact with the existing artifact verifier.
It requires the artifact's recorded `controller/Cargo.lock` and preserves its
exact bytes, and it requires the generated `controller/Cargo.toml` to be
exactly the supported runtime dependency contract. A locked offline `cargo
metadata` of the artifact must name only path packages and crates.io: a path
dependency outside the trusted crate set (`compiler-core`, `coordination`,
`deploy`, `engine`, `inventory`, `platform`, `runtime`), a compiled target
outside a crate's `src/` or a build script in a trusted crate fails
preparation. No kubeconfig, credential store, JavaScript source evaluator,
Cargo cache or unrelated workspace source is copied.

`cargo vendor --locked --offline --versioned-dirs` prepares the registry
dependency closure from the existing local Cargo cache, and a second locked
offline `cargo metadata` proves that the vendored sources satisfy the lock.
Missing cached dependencies or an outdated artifact lock fail preparation with
the `cargo fetch` command that populates the cache; there is no automatic
fetch, lock update, or network fallback. The staged lock must be unchanged and
the staged IR must validate to the artifact's `irDigest`.

Host cargo always runs with an explicit toolchain, because it runs from
directories outside the checkout where rustup would otherwise fall back to a
default: the caller's `RUSTUP_TOOLCHAIN` when set, else the channel
`rust-toolchain.toml` pins. rustup is told never to install anything.

The context is staged in a temporary directory, published by renaming it into
place, and sealed read-only (files 0444, directories 0555). The build hashes
every file and rechecks the whole tree before and after the compile, after
SBOM generation and before writing `build-result.json`; any added, removed or
changed file fails the build, naming the paths. These checks detect
persistent accidental changes. They do not isolate the context from a
malicious process running as the same user that changes files and restores
them between checks. Runtime sources are the current checkout snapshot, so
keep the trusted workspace stable during a build.

The context is kept after the build. Remove it with
`chmod -R u+w dist/build.native-context && rm -rf dist/build.native-context`.

### The pinned rusty_v8 archive

`crates/runtime` depends on `crates/engine`, which pins `deno_core`;
`deno_core` depends on the `v8` crate (denoland/rusty_v8), whose `build.rs`
otherwise downloads a prebuilt static-lib archive from GitHub Releases, which
is unreachable from the network-less compile. Preparation fetches that archive
itself, before the compile, from the URL pinned per target architecture in
`scripts/rusty-v8/pins.json`. It verifies the archive against the pinned
SHA-256 and checks that the pinned version matches the `v8` crate version
locked in `controller/Cargo.lock`, failing closed on either mismatch. The
archive is never committed; only its pin is. If the host is offline and
nothing is cached, preparation fails with a clear error naming the missing
target and its URL. See `scripts/rusty-v8/README.md`.

The fetched archive is cached at `$APSIDES_RUSTY_V8_CACHE` if set, else
`$XDG_CACHE_HOME/apsides/rusty-v8`, else `$HOME/.cache/apsides/rusty-v8`, and
never under the source checkout or any installed CLI distribution prefix. A
distribution's equivalent of the source checkout is `<prefix>/share/apsides`,
the tree `crates/cli/src/assets.rs`'s `assets::root()` hash-verifies in full on
every `aps` invocation; a cache written there would make every `aps` command
after the first `aps build` fail with "distribution assets do not match this
binary", so the build rejects one. CI sets `APSIDES_RUSTY_V8_CACHE` explicitly
and caches that path (see `.github/workflows/check.yml`'s `reactor` and
`acceptance-prepare` jobs).

The archive is staged at the fixed path `rusty-v8/librusty_v8.a.gz`, and the
compile sets `RUSTY_V8_ARCHIVE` to its in-container path, so `v8`'s `build.rs`
copies and gzip-decompresses it instead of downloading. No
`RUSTY_V8_SRC_BINDING_PATH` is set: the pinned `v8` crate version ships a
pre-generated binding for both supported targets inside the vendored crate
source (`pins.json`'s `bindingFile.required` is `false`). If that pin ever
flips to `true`, the build fails closed until `crates/packaging` fetches and
passes the binding file too.

The SBOM declares the archive by hand, because cargo-cyclonedx only walks
Cargo's dependency graph and never sees a `build.rs` input outside it: its
name, version, a `pkg:generic` purl carrying the pinned download URL and
checksum, and a `SHA-256` hash entry. The compiler closure records the same
archive under `nativeCompiler.rustyV8`, and `aps release` cross-checks it
against the context file at that path.

## The compile run

The compile runs the builder image by the image ID the build inspected, so a
store change between inspection and run cannot substitute another image; the
image is re-inspected afterwards and must be unchanged. The argv is fixed in
`crates/packaging/src/compile.rs` and `crates/container/src/run.rs`:

```text
<engine> run --rm --name apsides-build-<pid>-<time> --pull=never
  --platform linux/<arch> --network=none --hostname apsides-build --workdir /build
  [podman rootless: --userns=keep-id] --user <uid>:<gid> [podman: --unsetenv-all]
  --security-opt label=disable --security-opt no-new-privileges --cap-drop=ALL
  --mount type=bind,source=<context>/.cargo,target=/build/.cargo,readonly
  --mount type=bind,source=<context>/controller,target=/build/controller,readonly
  --mount type=bind,source=<context>/rusty-v8,target=/build/rusty-v8,readonly
  --mount type=bind,source=<context>/vendor,target=/build/vendor,readonly
  --mount type=bind,source=<context>/workspace,target=/build/workspace,readonly
  --mount type=bind,source=<target directory>,target=/build/target
  --mount type=bind,source=<fresh cargo home>,target=/opt/apsides-cargo
  --env K=V ... --entrypoint cargo <builder image ID>
  build --frozen --release --manifest-path controller/Cargo.toml
```

- **Mounts.** The context directories are read-only, so a build script that
  writes into its source tree fails with a hint saying so. Only the target
  directory and an empty cargo home, created fresh for every build and removed
  afterwards, are writable. Mount sources must be absolute UTF-8 paths without
  commas, and each is checked to be a real directory right before the run.
- **Environment.** The builder image's own `Config.Env`, with `CARGO_HOME` and
  `HOME` set to `/opt/apsides-cargo`, `CARGO_NET_OFFLINE=true`,
  `CARGO_TARGET_DIR=/build/target`, `RUSTY_V8_ARCHIVE` and `SOURCE_DATE_EPOCH`
  from the profile. Each variable is passed explicitly, and podman's
  `--unsetenv-all` removes what podman would otherwise add.
- **User.** The process runs as the invoking user's effective uid and gid
  (`--userns=keep-id` under rootless podman), so the outputs belong to that
  user. Under rootless Docker it runs as uid 0, which the daemon maps to the
  invoking user.
- **Probe.** Before the compile, a container with the same configuration
  prints its environment and the mounted `.cargo/config.toml`. Any variable
  added, missing or changed, for example by an engine or `containers.conf`
  default, fails the build. A cargo config that differs from the staged one
  means the engine cannot see the context, which happens with a remote
  daemon, and also fails the build.
- **Network.** `--network=none` and `--pull=never` apply to the probe and the
  compile. They do not by themselves establish whole-process egress isolation
  of the host, which needs separate evidence.

The compile has a four-hour limit, after which the container is removed. If
`aps` itself is killed, the named container may remain; remove it with
`podman rm -f` or `docker rm -f` and the `apsides-build-…` name. After the
compile the build reads `target/release/controller` and requires it to be a
Linux ELF binary for the profile's platform.

### `--cache`

| Mode | Behaviour |
| --- | --- |
| `--cache enabled` (default) | Reuses a target directory under `$APSIDES_BUILD_CACHE`, else `$XDG_CACHE_HOME/apsides/build`, else `$HOME/.cache/apsides/build`, at `cargo-target/<key>`. The key covers the compile recipe version, the builder reference, platform and epoch, the controller lock, the `rusty_v8` archive digest and the staged workspace sources. The controller sources are not in the key, so programs with the same dependency closure share a directory: after taking the key's lock, a build removes the cached controller binary and its cargo fingerprint, so cargo always recompiles the controller and only its dependencies are reused. A build holds an exclusive lock on the key until it has read its binary, so concurrent builds with the same key do not interleave; a build that has to wait says so on stderr. |
| `--cache disabled` | A fresh empty target directory beside the output, removed afterwards. Nothing persists. |

Only `--cache disabled` builds are reproducibility evidence; the
reproducibility tooling always uses it. A cache directory inside the assets
root is rejected. The cache is never pruned automatically; remove
`~/.cache/apsides/build` (or the directory you configured) to reclaim the
space.

## Runtime base

The runtime image must already be in the engine's store, match the builder's
platform, and supply the compatible GNU/Linux C runtime and CA certificates. A
digest-pinned nonroot Debian 12 C/C++ distroless base is the intended
deployment profile; `scripts/runtime-base/reference` pins
`gcr.io/distroless/cc-debian12@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f`.
Its upstream image family is documented by
[Distroless](https://github.com/GoogleContainerTools/distroless). This is a
digest pin, not a verified signature.

The image's registry index and its linux/amd64 and linux/arm64 manifests are
committed as source pins in `scripts/runtime-base`, byte for byte and named by
their digests. Neither Docker's classic image store nor podman keeps these
documents, so the build cannot read them locally. `cuenv task runtimeBasePin`
refreshes them for a new digest and is the only step that contacts a registry.

The build inspects the runtime image, requires it to carry the pinned
repository digest, and saves it with `podman image save --format
docker-archive` or `docker image save`. It reads every format the engines
write: podman's docker-archive, Docker's docker-archive before Docker 25 and
the OCI layout Docker 25 and later write from its classic or containerd store.
It verifies offline that the pinned index hashes to the pinned digest and
lists exactly one manifest for the build platform, and that the saved config
is the one that manifest names. Every saved layer, decompressed where the store
keeps it compressed, must hash to the config's diff ID. Engine image IDs and
compressed layer digests are never relied on. The compiler closure records the
index, manifest and config, and `aps release` checks the same chain offline.

## Image byte rules

`aps build` writes the image itself, so its bytes follow from the inputs
alone:

- **Base layers.** The runtime base's layers in diff ID order, each stored
  uncompressed, so its blob digest is its diff ID.
- **Controller layer.** A USTAR tar of `usr`, `usr/local`, `usr/local/bin` and
  `usr/local/bin/apsides-controller`, in that order. Every entry has the
  profile's `sourceDateEpoch` as its modification time, owner `root:root`
  (0:0), and mode 0755. There are no PAX headers, so no sub-second or access
  timestamps. The layer depends only on the binary and the epoch.
- **Config.** The pinned base config, which must be ASCII and contain no JSON
  floats, with the controller layer's diff ID appended to `rootfs.diff_ids`
  and one history entry appended. `created` is the epoch. The runtime
  configuration keeps the base's settings except that user is `65532:65532`,
  the working directory is `/`, the entrypoint is
  `/usr/local/bin/apsides-controller`, `Cmd` is removed and three labels are
  added: `dev.apsides.ir-digest`, `dev.apsides.release-status=blocked` and
  `org.opencontainers.image.title`. The packaging and `aps release` share one
  definition of this configuration. The binary embeds the validated IR.
  Runtime fixture configuration must be provided separately at the explicitly
  configured file path.

`image/controller.oci.tar` is an OCI layout holding one manifest, with no
`ref.name` annotation. The archive's entries have fixed metadata and a fixed
order. The image digest is the manifest's digest, recorded as `imageDigest` in
`build-result.json` beside `binaryDigest`. Before keeping the archive the build
runs `aps release`'s own image verification on it: the layers must be exactly
the base config's diff IDs plus the controller layer, and the controller in the
final layer must equal `binaryDigest`.

## Compiler closure

The build completes the source compiler's `build-inputs.json` with the native
compiler under schema `apsides.dev/compiler-closure/v4`, checks the result
with the validator `aps release` uses, and writes it as canonical bytes. A
source receipt with another schema is rejected; `aps` and the Deno compiler
must come from the same checkout. `nativeCompiler` records:

| Field | Content |
| --- | --- |
| `builderImage` | the profile's builder reference and the inspected platform |
| `cargoLockSha256` | the controller `Cargo.lock` digest, which must equal the context's entry |
| `buildContextFiles` | every context file and its digest: everything the compile container sees |
| `packagingFiles` | the root `Cargo.toml` and `Cargo.lock`, and the `Cargo.toml` and `src/` of `cli`, `compiler-core`, `container`, `packaging` and `release`; the compile recipe is code in these sources |
| `buildProfileSha256` | the raw profile bytes |
| `hostTools` | exactly four, in order: `engine`, `cargo`, `apsides`, `sbom` |
| `approvedRuntime` | the runtime base reference and platform, its pinned index and manifest, and its config |
| `rustyV8` | the archive record above; `bindingFile` is null |

Each host tool is recorded by the SHA-256 of its resolved executable and the
version it reports: the engine CLI with its client and server versions, host
cargo (under rustup, the proxy binary and the selected toolchain's cargo
version), `aps` itself, and the SBOM tool. All four are identified again before
the outputs are kept, and a change fails the build.

The engine appears in the closure only as the `engine` host tool. The image,
binary, SBOM and `build-result.json` do not depend on which engine ran the
compile, but `build-inputs.json`, and with it `compilerClosureDigest` and the
release `compilerDigest`, differ between a podman host and a Docker host by
design. A trust policy that approves compiler digests needs one entry per
engine host. That podman and Docker compile identical controller binaries is
the design goal; `cuenv task engineParityBuild` checks it, and
[STATUS.md](STATUS.md) records whether it has been shown.

## Outputs

```text
dist/build/
  build-result.json
  build-inputs.json
  image/controller.oci.tar
  linux-binary/apsides-controller
  sbom/sbom.json
  ...                          the verified artifact from aps compile
dist/build.source/             the source compiler's staging directory
dist/build.native-context/     the sealed build context
```

The SBOM is cargo-cyclonedx's CycloneDX 1.5 inventory of the context, run
offline with a cleared environment, with the host build path replaced by
`/build`, the `rusty_v8` archive appended, and canonical JSON bytes.
`build-result.json` (`apsides.dev/build-result/v1`) records the IR, image,
binary and SBOM digests, the platform and the output paths. It explicitly
remains unsigned (`releaseVerified: false`) and cannot authorize deployment.
`aps check --ir` remains available for inspecting the compiled IR from
`aps compile` without producing an image.

The build must be assembled into a release with `aps release` and signed with
approved provenance before `aps deploy` accepts it. Reproducible image builds,
candidate provenance verification, and containerized reconciliation have
separate receipts; consult [STATUS.md](STATUS.md) for their current
verification boundaries. A prepared context or a successful build must not be
reported as satisfying them. Development builds use the source checkout. The
[CLI distribution](CLI-DISTRIBUTION.md) bundles the compiler and native
packaging source closure and verifies it against a manifest digest embedded in
its executable.
