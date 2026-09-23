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

Context verification also runs the existing trusted `aps check` against the embedded IR and requires its recomputed digest to match the context's declared IR identity. The build captures the initial context metadata, verifies the files and IR before dispatch, and verifies them again after BuildKit finishes. Changed files or metadata prevent publication of `packaging-inputs.json`; any already exported archive then remains an unaccepted output. The output directory must be outside the context. These checks detect persistent accidental changes during a build; they do not provide filesystem isolation against a malicious concurrent writer that changes files and restores them before verification.

The locally inspected builder is:

```text
rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
linux/arm64
```

Image creation is a separate explicit action:

```sh
python3 scripts/package_oci.py build \
  --context work/apsides-oci-context \
  --runtime-image 'REGISTRY/COMPATIBLE-RUNTIME@sha256:EXACT_DIGEST' \
  --output work/apsides-oci-image
```

The runtime image must already be cached, match the builder's platform, and supply the compatible GNU/Linux C runtime and CA certificates. A digest-pinned nonroot Debian 12 C/C++ distroless base is the intended deployment profile. Integration preparation cached `gcr.io/distroless/cc-debian12@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f`. Its upstream image family is documented by [Distroless](https://github.com/GoogleContainerTools/distroless). This is a digest pin, not a verified signature. The integration startup result is recorded below.

The two-stage Dockerfile runs `cargo build --frozen --release` with vendored dependencies and `RUN --network=none`. The final image receives only the generated native binary from the build stage, uses UID/GID 65532, and embeds the validated IR in that binary. Runtime fixture configuration must be provided separately at the explicitly configured file path. The script uses BuildKit's image exporter, which every Docker driver and image store supports, so neither the containerd image store nor a `docker-container` builder is required. It never pushes an image; the built image stays in the local Docker store without a name. The script reads it back with `docker image save`, checks every blob against its digest and every layer against the config's diff IDs, and writes `controller.oci.tar`: an OCI layout holding one manifest that it generates from that config and those layers. The image digest is that manifest's digest, recorded as `imageDigest` in `packaging-inputs.json`. The classic store saves layers uncompressed and the containerd store saves them compressed, so the same build has a different image digest in each store. On the classic store BuildKit does not rewrite file timestamps inside layers, so image digests there are not reproducible; the binary digest does not depend on them.

Both the build invocation and its Cargo RUN steps disable build-step networking. These flags do not by themselves prove that BuildKit's source/metadata resolver made no external requests. Whole-process egress isolation and production deployment authorization require separate evidence. Reproducible image builds, candidate provenance verification, and containerized reconciliation have separate receipts; consult STATUS.md for their current verification boundaries. A prepared context or successful image export must not be reported as satisfying them.

## Complete CLI build

`aps build --source examples/web-application/program.tsx --output dist/build --profile apsides.build.json`
composes source compilation, offline Cargo vendoring, BuildKit image creation,
binary extraction, cargo-cyclonedx SBOM generation, and compiler
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
descriptors, because the classic store exports base layers uncompressed.

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
