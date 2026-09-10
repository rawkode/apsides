# Apsides

Compiled reactive GitOps for Kubernetes.

Apsides turns build-time TypeScript declarations into a validated resource and reaction plan and a native Rust controller. The controller reconciles declared resources and reacts to selected configuration changes without executing JavaScript.

The implementation follows [specification 0.2](docs/reference/SPEC-v0.2.md).
**The production release gates are not complete.** Read the
[current status](docs/reference/STATUS.md) before interpreting a build or test as
production evidence.

## Authoring

The [TSX example](examples/web-application/program.tsx) composes a reusable
[`Api` component](examples/web-application/api.tsx) inside a controller boundary:

```tsx
<Controller {...platformOptions}>
  <Api name="api" namespace="production" image={images.api} />
</Controller>
```

Components return Kubernetes resource elements. `useConfigMap` and `useSecret`
declare observations; a Deployment's `rolloutOn` prop connects opaque input
projections to the fixed native rollout operation. Components run at build time,
not in Kubernetes. The compiler supplies source locations, provenance and
separate synthetic test fixtures.

The [complete example guide](examples/web-application/README.md) explains source locking and
compilation. The [IR contract](docs/reference/IR-CONTRACT.md) documents the supported
resource subset; unsupported operations fail validation.

A relocatable CLI bundle is available through `cuenv task packageCli`; see
[CLI distribution](docs/reference/CLI-DISTRIBUTION.md) for installation and prerequisites.

## Local compiler

```sh
cargo build -p cli
target/debug/aps compile --source examples/web-application/program.tsx --output work/apsides-compiled
target/debug/aps check --ir work/apsides-compiled/ir.json
target/debug/aps build --source examples/web-application/program.tsx --output work/apsides-native --profile apsides.build.json
target/debug/aps verify --artifact work/apsides-native
```

Use a fresh output directory for each build. The source path uses Oxc and Rolldown for compilation and Deno with denied filesystem, network, environment, subprocess, and remote-import permissions for evaluation. SES compartments constrain ambient JavaScript capabilities, and the compiler compares independent bundle and evaluation runs. Final release reproducibility requires the separate native/image checks. Local compilation emits the validated IR, recomputed effects and capabilities, generated scenario plan, Rust controller source, and an integrity manifest. The complete build uses a pinned [build profile](docs/reference/OCI-PACKAGING.md#complete-cli-build), the prepared Cargo cache, Docker BuildKit, and cargo-cyclonedx to produce a Linux image, binary, SBOM, and compiler closure. `verify` checks local integrity and recomputes the plan; it does not establish approved provenance or production readiness.

The CI interface is `aps deploy`, independent of the Kubernetes provider. CI prepares a short-lived Kubernetes identity through its normal authentication setup; Apsides owns artifact/policy checks and the resumable deployment protocol. The privileged CI deployer creates the next generation’s Roles and RoleBindings, deploys its controller image, and retires the previous writer. The runtime receives only its approved reconciliation capabilities; it does not manage RBAC or upgrade itself. The deployment review lists [required CI access](docs/reference/DEPLOYMENT-PERMISSIONS.md). CI can select an immutable OCI release with `APSIDES_RELEASE_REFERENCE`; see [artifact retrieval](docs/reference/ARTIFACT-RESOLUTION.md#deploy-from-an-immutable-reference). Configure `APSIDES_INVENTORY_DATABASE` with an absolute path for persistent SQLite storage; see [resource inventory](docs/reference/INVENTORY.md#deployment-connection). Deployment requires platform-owned trust, ceiling, identity and proxy qualification records; current live acceptance limits are listed in the implementation status.

## Implementation and verification

- `packages/kubernetes`: build-time TypeScript authoring.
- `crates/compiler-core`: strict IR validation, canonical identity, capability derivation, policy ceiling, and rollout primitives.
- `crates/deploy`: provider-neutral transition planning and recovery predicates.
- `crates/cli`: compiler, release assembly/verification, and deployment entrypoint.
- `crates/platform`: protected Kubernetes state, per-generation proxy transport and lifecycle execution.
- `crates/release`: signed artifact, compiler closure, OCI and acceptance verification.
- `crates/runtime`: native reconciliation and an explicit disposable-cluster fixture boundary.

The [toolchain choices](docs/reference/TOOLCHAIN.md) explain the Oxc/Deno and OSS integrations. The [OCI packaging path](docs/reference/OCI-PACKAGING.md) uses Cargo vendor and BuildKit.

The [current status](docs/reference/STATUS.md) distinguishes implemented behavior,
observed verification, and outstanding release gates. Full Kubernetes
authorization, workload readiness, deployment recovery, and enforced offline
execution require their own evidence.

## Development tasks

Use cuenv 0.55.0 with the pinned schema in `cue.mod/module.cue`:

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
