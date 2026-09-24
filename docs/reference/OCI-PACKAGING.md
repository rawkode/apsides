# Native controller OCI packaging

This path prepares dependencies with Cargo and delegates image creation to Docker BuildKit. Preparation produces a build context, not an OCI image. The controller requires independently protected activation and policy at startup. Packaging alone does not establish release authenticity or authorization.

```sh
python3 scripts/package_oci.py prepare \
  --artifact work/apsides-native \
  --output work/apsides-oci-context

python3 scripts/package_oci.py verify --context work/apsides-oci-context
```

Preparation first invokes the existing artifact verifier. It requires the artifact's recorded `controller/Cargo.lock`, preserves its exact bytes, snapshots the current trusted `compiler-core` and `runtime` sources, and relocates the generated manifest's single runtime path into the context. It copies no kubeconfig, credential store, JavaScript source evaluator, Cargo cache, or unrelated workspace source.

`cargo vendor --locked --offline --versioned-dirs` prepares the registry dependency closure from the existing local Cargo cache. Cargo validates checksums and writes its own source-replacement configuration. A subsequent locked offline `cargo metadata` verifies resolution from the prepared context. Missing cached dependencies or an outdated artifact lock fail preparation; there is no automatic fetch, lock update, or network fallback. Rebuild the native artifact after runtime dependency changes before preparing it again.

`context.json` hashes the exact prepared files and records the input artifact manifest, controller lock, IR identity, and builder image. These hashes provide local integrity evidence only. Runtime sources are the current checkout snapshot; they were not embedded in earlier local artifacts. Preparation therefore establishes a new packaging-input snapshot and does not claim those sources produced a pre-existing host binary. Keep the trusted workspace stable during preparation.

Context verification also runs the existing trusted `aps check` against the embedded IR and requires its recomputed digest to match the context's declared IR identity. The build captures the initial context metadata, verifies the files and IR before dispatch, and verifies them again after BuildKit finishes. Changed files or metadata prevent writing `controller.oci.tar` and `packaging-inputs.json`. The output directory must be outside the context. These checks detect persistent accidental changes during a build; they do not provide filesystem isolation against a malicious concurrent writer that changes files and restores them before verification.

The locally inspected builder is:

```text
rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
linux/arm64
```

Image creation is a separate explicit action:

```sh
docker image save --output work/runtime-base.tar 'REGISTRY/COMPATIBLE-RUNTIME@sha256:EXACT_DIGEST'
python3 scripts/package_oci.py build \
  --context work/apsides-oci-context \
  --runtime-image 'REGISTRY/COMPATIBLE-RUNTIME@sha256:EXACT_DIGEST' \
  --runtime-base work/runtime-base.tar \
  --output work/apsides-oci-image
```

The runtime image must already be cached, match the builder's platform, and supply the compatible GNU/Linux C runtime and CA certificates. A digest-pinned nonroot Debian 12 C/C++ distroless base is the intended deployment profile. Integration preparation cached `gcr.io/distroless/cc-debian12@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f`. Its upstream image family is documented by [Distroless](https://github.com/GoogleContainerTools/distroless). This is a digest pin, not a verified signature. The integration startup result is recorded below.

The Dockerfile runs `cargo build --frozen --release` with vendored dependencies and `RUN --network=none`, then copies only the binary into an empty stage. BuildKit's local exporter, which works on every Docker driver and image store, writes that stage to a temporary directory, so BuildKit compiles the controller and hands over the binary and nothing else. It builds no image and leaves nothing in the local image store. The export must hold exactly the binary.

### The pinned rusty_v8 archive

`crates/runtime` depends on `crates/engine`, which pins `deno_core`; `deno_core` depends on the `v8` crate (denoland/rusty_v8), whose `build.rs` otherwise downloads a prebuilt static-lib archive from GitHub Releases -- unreachable from the `--network=none` `RUN` step above. `prepare()` fetches that archive itself, before BuildKit ever runs, from the URL pinned per target architecture in `scripts/rusty-v8/pins.json`, verifies it against the pinned SHA-256, and caches it under the gitignored `work/rusty-v8-cache/`. The archive is never committed; only its pin is. If the host is offline and nothing is cached, preparation fails with a clear error naming the missing target instead of silently falling through to BuildKit's own (network-less, therefore also failing) download. See `scripts/rusty-v8/README.md`.

The fetched archive is staged into the context at the fixed path `rusty-v8/librusty_v8.a.gz` (the context only ever holds one target's archive, so this path does not vary), and `context.json` records its identity under `rustyV8.archive`: name, version, target, source URL and SHA-256, cross-checked against the same path's entry in the context's own file manifest. `scripts/Dockerfile` copies that directory into the build and sets `RUSTY_V8_ARCHIVE` to its in-container path as a plain `ENV`, so `v8`'s `build.rs` copies and gzip-decompresses it directly instead of downloading. No `RUSTY_V8_SRC_BINDING_PATH` is set: the pinned `v8` crate version ships a pre-generated binding for both supported targets inside the vendored crate source itself (`scripts/rusty-v8/pins.json`'s `bindingFile.required` is `false`); `prepare()` fails closed instead of silently compiling wrong bits if that pin ever flips to `true` without the fetch/staging/Dockerfile logic being updated for it.

`scripts/sbom.py` adds the archive (and binding file, when one is pinned) to the generated CycloneDX SBOM as manually declared components -- cargo-cyclonedx only walks Cargo's own dependency graph and never sees a `build.rs` download outside it -- each with its name, version, a `pkg:generic` purl carrying the pinned download URL and checksum, and a `SHA-256` hash entry. `scripts/finalize_inputs.py` carries the same record into `build-inputs.json`'s `compilerClosure.nativeCompiler.rustyV8` (schema `apsides.dev/compiler-closure/v3`); `aps release` cross-checks it against the same-path `buildContextFiles` entry it also validates.

The script then assembles the image itself, so its bytes follow from the inputs alone:

- **Base layers.** `--runtime-base` is a `docker image save` of the runtime image. Before BuildKit runs, the script verifies the pinned index, platform manifest and saved config chain described under [Complete CLI build](#complete-cli-build), and it checks every saved layer against the config's diff IDs.
- **Controller layer.** The final layer is a USTAR tar of `usr`, `usr/local`, `usr/local/bin` and `usr/local/bin/apsides-controller`, in that order. Every entry has the profile's `sourceDateEpoch` as its modification time, owner `root:root` (0:0), and mode 0755. There are no PAX headers, so no sub-second or access timestamps. The layer depends only on the binary and the epoch.
- **Config.** The image config is the pinned base config with the controller layer's diff ID appended to `rootfs.diff_ids` and one history entry appended. `created` is the epoch. The runtime configuration keeps the base's settings except that user is `65532:65532`, the working directory is `/`, the entrypoint is `/usr/local/bin/apsides-controller`, `Cmd` is removed and three labels are added: `dev.apsides.ir-digest`, `dev.apsides.release-status=blocked` and `org.opencontainers.image.title`. This is the configuration `aps release` requires. The binary embeds the validated IR. Runtime fixture configuration must be provided separately at the explicitly configured file path.

`controller.oci.tar` is an OCI layout holding one manifest. Every layer, base layers included, is stored uncompressed, so each layer's blob digest is its diff ID. The archive's entries have fixed metadata and a fixed order. The image digest is the manifest's digest, recorded as `imageDigest` in `packaging-inputs.json` beside the binary's `binaryDigest`. Two builds from the same context, runtime base and epoch give a byte-identical archive and the same image digest. That holds on the classic (overlay2) store and the containerd store alike, because the containerd store's compressed base layers are decompressed and hashed against their diff IDs before they are written. Binary extraction requires the image's controller to equal `binaryDigest`.

Both the build invocation and its Cargo RUN steps disable build-step networking. These flags do not by themselves prove that BuildKit's source/metadata resolver made no external requests. Whole-process egress isolation and production deployment authorization require separate evidence. Reproducible image builds, candidate provenance verification, and containerized reconciliation have separate receipts; consult STATUS.md for their current verification boundaries. A prepared context or successful image export must not be reported as satisfying them.

## Complete CLI build

`aps build --source examples/web-application/program.tsx --output dist/build --profile apsides.build.json`
composes source compilation, offline Cargo vendoring, BuildKit compilation,
image assembly, binary extraction, cargo-cyclonedx SBOM generation, and compiler
closure assembly. The binary is read from the image's final layer, which must
hold only the controller and its parent directories; no container is created.
The runtime base's registry index and its linux/amd64 and linux/arm64
manifests are committed as source pins in `scripts/runtime-base`, byte for
byte and named by their digests, beside the pinned `reference`. Docker's
classic image store keeps neither document, so the build cannot read them
locally. `cuenv task runtimeBasePin` refreshes them for a new digest and is
the only step that contacts a registry. The build verifies offline that the
pinned index hashes to the pinned digest and lists exactly one manifest for
the build platform, and that that manifest hashes to its descriptor and names
the image config held by the local store. It reads that config from
`docker image save` in either format Docker writes (an OCI layout, or the
docker-archive of Docker before 25). The saved image must be the one
`docker image inspect` matched to the pinned digest, and where the store keeps
compressed layers they must be the manifest's layer blobs. The compiler
closure records the index, manifest and config, and `aps release` checks the
same chain offline. It then requires the image's layers to be exactly the base
config's diff IDs plus the controller layer, decompressing and hashing each
layer against its diff ID. It does not compare compressed base-layer
descriptors, because the build stores every layer uncompressed.

The build emits `build-result.json`, `image/controller.oci.tar`,
`linux-binary/apsides-controller`, `sbom/sbom.json`, and `build-inputs.json`.
The separately prepared context is the sibling `dist/build.oci-context`.
`aps check --ir` remains available for inspecting the compiled IR from `aps compile` without producing an image.

The build profile uses schema `apsides.dev/build-profile/v1` and exactly these fields:
`builderImage`, `runtimeImage` (both immutable digest references), `platform`
(`linux/arm64` or `linux/amd64`), `sbomTool` (an executable path relative to the
profile or absolute), `sbomToolSha256`, and `sourceDateEpoch` (nonnegative integer).
The `runtimeImage` digest must have registry pins in `scripts/runtime-base`.
The platform, cached image identities, executable digest, source lock, and compiler
closure must agree. Missing cached dependencies/images fail without a pull fallback.
The profile defaults to `apsides.build.json` in the invoking directory.

`build-result.json` explicitly remains unsigned and cannot authorize deployment.
The build must be assembled into a release with `aps release` and signed with
approved provenance before `aps deploy` accepts it. Development builds use
the source checkout. The [CLI distribution](CLI-DISTRIBUTION.md) bundles the
compiler and native packaging source closure and verifies it against a manifest
digest embedded in its executable.
