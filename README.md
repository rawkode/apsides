# Apsides

Compiled reactive GitOps for Kubernetes.

Apsides turns build-time TypeScript declarations into a validated resource and reaction plan and a native Rust controller. The controller reconciles declared resources, reacts to selected configuration changes, and runs declared event handlers in a sandboxed V8 isolate with only their declared inputs, HTTPS egress hosts and output `ConfigMap`s.

The implementation follows the [specification](docs/reference/SPEC.md).
**Apsides is not ready for production use.** Read the
[current status](docs/reference/STATUS.md) before relying on a build or test
result.

## Authoring

The [TSX example](examples/web-application/program.tsx) composes a reusable
[`Api` component](examples/web-application/api.tsx) inside a controller boundary:

```tsx
<Controller
  id="team-api-production"
  namespace="production"
  controlNamespace="apsides-system"
  clusterBinding="local-fixture"
  policyRef="platform/team-api"
  enforcement="api-bound"
>
  <Api
    name="api"
    namespace="production"
    image="docker.io/nginxinc/nginx-unprivileged@sha256:9b87ad3dd9f431c733f19dfb278c7eb3dba9dca381942c79818bb42f1a566a83"
  />
</Controller>
```

Components return Kubernetes resource elements. `useConfigMap` and `useSecret`
declare observations; a Deployment's `rolloutOn` prop connects opaque input
projections to the fixed native rollout operation. `<On>` and
`useHandlerOutput` declare an [event handler](docs/guides/event-handlers.md): a
locked TypeScript module the controller runs when an input changes, which may
call declared HTTPS hosts and write a handler output that can itself trigger a
rollout. Components run at build time, not in Kubernetes. The compiler supplies source locations, provenance and
separate synthetic test fixtures.

The [complete example guide](examples/web-application/README.md) explains source locking and
compilation. The [IR contract](docs/reference/IR-CONTRACT.md) documents the supported
resource subset; unsupported operations fail validation.

A relocatable CLI bundle is available through `cuenv task packageCli`; see
[CLI distribution](docs/reference/CLI-DISTRIBUTION.md) for installation and prerequisites.

## Local compiler

```sh
cargo build --locked -p cli
cargo fetch --locked
target/debug/aps compile --source examples/web-application/program.tsx --output work/apsides-compiled
target/debug/aps check --ir work/apsides-compiled/ir.json
target/debug/aps build --source examples/web-application/program.tsx --output work/apsides-native --profile apsides.build.json
target/debug/aps verify --artifact work/apsides-native
```

Use a fresh output directory for each build. The source path uses Oxc and Rolldown for compilation and Deno with denied filesystem, network, environment, subprocess, and remote-import permissions for evaluation. SES compartments constrain ambient JavaScript capabilities, and the compiler compares independent bundle and evaluation runs. Final release reproducibility requires the separate native/image checks. Local compilation emits the validated IR, recomputed effects and capabilities, the `surface.json` deployment surface, generated scenario plan, bundled event handlers, Rust controller source, and an integrity manifest. `aps diff --base <old> --head <new>` compares two surfaces for review; see [surface diff in CI](docs/guides/surface-diff.md). The complete build uses a pinned [build profile](docs/reference/OCI-PACKAGING.md#complete-cli-build) that you write first — `apsides.build.json` is not shipped — plus the prepared Cargo cache, the pinned `rusty_v8` archive, podman or Docker with the pinned builder and runtime images, and cargo-cyclonedx to produce a Linux image, binary, SBOM, and compiler closure. `verify` checks local integrity and recomputes the plan; it does not establish approved provenance or production readiness.

The CI interface is `aps deploy`, independent of the Kubernetes provider. CI prepares a short-lived Kubernetes identity through its normal authentication setup; Apsides owns artifact/policy checks and the resumable deployment protocol. The privileged CI deployer creates the next generation’s Roles and RoleBindings, deploys its controller image, and retires the previous writer. The runtime receives only its approved reconciliation capabilities; it does not manage RBAC or upgrade itself. The deployment review lists [required CI access](docs/reference/DEPLOYMENT-PERMISSIONS.md). CI can select an immutable OCI release with `APSIDES_RELEASE_REFERENCE`; see [artifact retrieval](docs/reference/ARTIFACT-RESOLUTION.md#deploy-from-an-immutable-reference). Configure `APSIDES_INVENTORY_DATABASE` with an absolute path for persistent SQLite storage; see [resource inventory](docs/reference/INVENTORY.md#deployment-connection). Deployment requires platform-owned trust, ceiling, identity and proxy qualification records, a Kubernetes server at or above the program's `minKubernetesVersion` (default 1.34), and binds the deploy to the reviewed `surface.json` digest; current live acceptance limits are listed in the implementation status.

## Implementation and verification

- `packages/kubernetes`: build-time TypeScript authoring.
- `packages/compiler`: source locking, CRD type generation, and the Oxc/Rolldown/Deno compilation driver.
- `crates/compiler-core`: strict IR validation, canonical identity, capability derivation, policy ceiling, rollout primitives, and the deployment surface with `aps diff`'s comparison and review policy.
- `crates/coordination`: claim, review, and audit records backing the deployment protocol.
- `crates/deploy`: provider-neutral transition planning and recovery predicates.
- `crates/cli`: compiler, release assembly/verification, and deployment entrypoint.
- `crates/inventory`: metadata-only resource inventory for managed identities.
- `crates/platform`: protected Kubernetes state, per-generation proxy transport and lifecycle execution.
- `crates/release`: release integrity, signed provenance, compiler closure and OCI verification.
- `crates/packaging`: the native build behind `aps build`: build context, network-less compile, image assembly, SBOM and compiler closure.
- `crates/container`: podman and Docker selection, command lines and output parsing, shared by the native build and the cluster tests.
- `crates/runtime`: native reconciliation, handler dispatch, the synchronous Landlock-locked startup, and an explicit disposable-cluster fixture boundary.
- `crates/engine`: the embedded `deno_core`/V8 sandbox that runs event handlers, with its `fetch`, `writeOutput` and `log` operations and the Landlock lockdown primitives.
- `crates/test-cluster`: `aps test --cluster`, which runs a program's generated reactor cases on a disposable kind cluster.
- `crates/acceptance`: the acceptance suite, which deploys the CNPG examples with `aps deploy` on kind.

The [toolchain choices](docs/reference/TOOLCHAIN.md) explain the Oxc/Deno and OSS integrations. The [OCI packaging path](docs/reference/OCI-PACKAGING.md) vendors with Cargo, compiles in the pinned builder image under podman or Docker with no network, and assembles the image in `aps` itself.

The [current status](docs/reference/STATUS.md) distinguishes implemented behavior,
observed verification, and outstanding work. Full Kubernetes authorization,
workload readiness, deployment recovery, and enforced offline execution are not
yet qualified.

## Development tasks

Use cuenv 0.56.1 with the pinned schema in `cue.mod/module.cue`:

```sh
cuenv task build           # Build the CLI and its generated prerequisites
cuenv task authorCompilerTest # Test the @apsides/compiler integration
cuenv task authorCheck     # Typecheck the authoring package
cuenv task authorTest      # Test the authoring package
cuenv task docsDev         # Prepare and serve Astro documentation
cuenv task docsBuild       # Prepare, check and build the documentation
```

Generated code and schemas are ignored and must never be committed. Prefer the
configured tasks over lower-level commands from a fresh checkout. Commit
canonical sources, generators and dependency lockfiles. The docs content mirror
and static output are also ignored.
