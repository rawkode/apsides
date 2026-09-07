# Native controller OCI packaging

This path prepares dependencies with Cargo and delegates image creation to Docker BuildKit. Preparation produces a build context, not an OCI image. The controller requires independently protected activation and policy at startup. Packaging alone does not establish release authenticity or authorization.

```sh
bash scripts/reakt/package-oci.sh prepare \
  --artifact work/reakt-native \
  --output work/reakt-oci-context

bash scripts/reakt/package-oci.sh verify --context work/reakt-oci-context
```

Preparation first invokes the existing artifact verifier. It requires the artifact's recorded `controller/Cargo.lock`, preserves its exact bytes, snapshots the current trusted `compiler-core` and `runtime` sources, and relocates the generated manifest's single runtime path into the context. It copies no kubeconfig, credential store, JavaScript source evaluator, Cargo cache, or unrelated workspace source.

`cargo vendor --locked --offline --versioned-dirs` prepares the registry dependency closure from the existing local Cargo cache. Cargo validates checksums and writes its own source-replacement configuration. A subsequent locked offline `cargo metadata` verifies resolution from the prepared context. Missing cached dependencies or an outdated artifact lock fail preparation; there is no automatic fetch, lock update, or network fallback. Rebuild the native artifact after runtime dependency changes before preparing it again.

`context.json` hashes the exact prepared files and records the input artifact manifest, controller lock, IR identity, and builder image. These hashes provide local integrity evidence only. Runtime sources are the current checkout snapshot; they were not embedded in earlier local artifacts. Preparation therefore establishes a new packaging-input snapshot and does not claim those sources produced a pre-existing host binary. Keep the trusted workspace stable during preparation.

Context verification also runs the existing trusted `reakt check` against the embedded IR and requires its recomputed digest to match the context's declared IR identity. The build captures the initial context metadata, verifies the files and IR before dispatch, and verifies them again after BuildKit finishes. Changed files or metadata prevent publication of `packaging-inputs.json`; any already exported archive then remains an unaccepted output. The output directory must be outside the context. These checks detect persistent accidental changes during a build; they do not provide filesystem isolation against a malicious concurrent writer that changes files and restores them before verification.

The locally inspected builder is:

```text
rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
linux/arm64
```

Image creation is a separate explicit action:

```sh
bash scripts/reakt/package-oci.sh build \
  --context work/reakt-oci-context \
  --runtime-image 'REGISTRY/COMPATIBLE-RUNTIME@sha256:EXACT_DIGEST' \
  --output work/reakt-oci-image
```

The runtime image must already be cached, match the builder's platform, and supply the compatible GNU/Linux C runtime and CA certificates. A digest-pinned nonroot Debian 12 C/C++ distroless base is the intended deployment profile. Integration preparation cached `gcr.io/distroless/cc-debian12@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f`. Its upstream image family is documented by [Distroless](https://github.com/GoogleContainerTools/distroless). This is a digest pin, not a verified signature. The integration startup result is recorded below.

The two-stage Dockerfile runs `cargo build --frozen --release` with vendored dependencies and `RUN --network=none`. The final image receives only the generated native binary from the build stage, uses UID/GID 65532, and embeds the validated IR in that binary. Runtime fixture configuration must be provided separately at the explicitly configured file path. The script requests BuildKit's standard OCI archive exporter and metadata output; it never pushes an image. The selected BuildKit driver must support that exporter and already have access to the pinned base-image contents.

Both the build invocation and its Cargo RUN steps disable build-step networking. These flags do not by themselves prove that BuildKit's source/metadata resolver made no external requests. Whole-process egress isolation and production deployment authorization require separate evidence. Reproducible image builds, candidate provenance verification, and containerized reconciliation have separate receipts; consult STATUS.md for their current verification boundaries. A prepared context or successful image export must not be reported as satisfying them.

## Complete CLI build

`reakt build --source examples/reakt/program.tsx --output dist/build --profile reakt.build.json`
composes source compilation, offline Cargo vendoring, BuildKit image creation,
Docker rootfs binary extraction, cargo-cyclonedx SBOM generation, and compiler
closure assembly. It emits `build-result.json`, `image/controller.oci.tar`,
`linux-binary/reakt-controller`, `sbom/sbom.json`, and `build-inputs.json`.
The separately prepared context is the sibling `dist/build.oci-context`.
`reakt compile` remains available for inspecting raw IR without producing an image.

The build profile uses schema `reakt.dev/build-profile/v1` and exactly these fields:
`builderImage`, `runtimeImage` (both immutable digest references), `platform`
(`linux/arm64` or `linux/amd64`), `sbomTool` (an executable path relative to the
profile or absolute), `sbomToolSha256`, and `sourceDateEpoch` (nonnegative integer).
The platform, cached image identities, executable digest, source lock, and compiler
closure must agree. Missing cached dependencies/images fail without a pull fallback.
The profile defaults to `reakt.build.json` in the invoking directory.

`build-result.json` explicitly remains unsigned and cannot authorize deployment.
Approved provenance and complete reactor/security evidence must be assembled into
and verified as a release before `aps deploy` accepts it. Development builds use
the source checkout. The [CLI distribution](CLI-DISTRIBUTION.md) bundles the
compiler and native packaging source closure and verifies it against a manifest
digest embedded in its executable.
