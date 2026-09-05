# Reakt

Compiled reactive GitOps for Kubernetes.

Reakt turns build-time TypeScript declarations into a validated resource and reaction plan and a native Rust controller. The controller reconciles declared resources and reacts to selected configuration changes without executing JavaScript.

The production migration follows [specification 0.2](docs/reakt/SPEC-v0.2.md). **The v1 release gates are not complete.** The new implementation is separate from the retained experimental OxideLoop runtime; passing its old tests does not establish Reakt compliance.

## Authoring

```typescript
const domain = defineDomain({ /* platform identity, scope and budgets */ });
const config = domain.observe(ConfigMap, {
  name: "api-settings",
  namespace: "production",
  projection: ["data"],
  missing: "block-dependent-mutations",
});
const api = domain.manage(new Deployment({ /* fixed desired resource */ }), {
  deletionPolicy: "orphan",
});
watch(config).project("data").rollout(api);
```

The [complete example](examples/reakt/README.md) includes explicit domain identity, source locations, and a separately marked fixture overlay. The [IR contract](docs/reakt/IR-CONTRACT.md) documents the current accepted subset. Unsupported operations fail validation.

## Local compiler

```sh
cargo build -p cli
target/debug/reakt compile --source examples/reakt/program.ts --output work/reakt-compiled
target/debug/reakt check --ir work/reakt-compiled/ir.json
target/debug/reakt build --source examples/reakt/program.ts --output work/reakt-native --profile reakt.build.json
target/debug/reakt verify --artifact work/reakt-native
```

Use a fresh output directory for each build. The source path uses Oxc and Rolldown for compilation and Deno with denied filesystem, network, environment, subprocess, and remote-import permissions for evaluation. SES compartments constrain ambient JavaScript capabilities, and the compiler compares independent bundle and evaluation runs. Final release reproducibility requires the separate native/image checks. Local compilation emits the validated IR, recomputed effects and capabilities, generated scenario plan, Rust controller source, and an integrity manifest. The complete build uses a pinned [build profile](docs/reakt/OCI-PACKAGING.md#complete-cli-build), the prepared Cargo cache, Docker BuildKit, and cargo-cyclonedx to produce a Linux image, binary, SBOM, and compiler closure. `verify` checks local integrity and recomputes the plan; it does not establish approved provenance or production readiness.

The CI interface is `reakt deploy`, independent of the Kubernetes provider. CI prepares a short-lived Kubernetes identity through its normal authentication setup; Reakt owns artifact/policy checks and the resumable deployment protocol. The privileged CI deployer creates the next generation’s Roles and RoleBindings, deploys its controller image, and retires the previous writer. The runtime receives only its approved reconciliation capabilities; it does not manage RBAC or upgrade itself. The deployment review lists [required CI access](docs/reakt/DEPLOYMENT-PERMISSIONS.md). CI can select an immutable OCI release with `REAKT_RELEASE_REFERENCE`; see [artifact retrieval](docs/reakt/ARTIFACT-RESOLUTION.md#deploy-from-an-immutable-reference). Configure `REAKT_INVENTORY_DATABASE` for persistent libSQL storage and `REAKT_INVENTORY_TOKEN` for a remote primary; see [resource inventory](docs/reakt/INVENTORY.md#deployment-connection). Deployment requires platform-owned trust, ceiling, identity and proxy qualification records; current live acceptance limits are listed in the migration status.

## Implementation and verification

- `packages/reakt`: build-time TypeScript authoring.
- `crates/compiler-core`: strict IR validation, canonical identity, capability derivation, policy ceiling, and rollout primitives.
- `crates/deploy`: provider-neutral transition planning and recovery predicates.
- `crates/cli`: compiler, release assembly/verification, and deployment entrypoint.
- `crates/platform`: protected Kubernetes state, per-generation proxy transport and lifecycle execution.
- `crates/release`: signed artifact, compiler closure, OCI and acceptance verification.
- `crates/runtime`: native reconciliation and an explicit disposable-cluster fixture boundary.

The [toolchain choices](docs/reakt/TOOLCHAIN.md) explain the Oxc/Deno and OSS integrations. The [OCI packaging path](docs/reakt/OCI-PACKAGING.md) uses Cargo vendor and BuildKit.

The [migration status](docs/reakt/STATUS.md) distinguishes implemented behavior, observed verification, and outstanding release gates. Full Kubernetes authorization, workload readiness, deployment recovery, and enforced offline execution require their own evidence.

The previous implementation and its historical commands remain documented in [the legacy POC notes](docs/reakt/LEGACY-POC.md). Existing API group names and persisted formats are not silently rewritten; migration of installed controllers requires the explicit lifecycle protocol.

## Development tasks

Use cuenv 0.55.0 with the pinned schema in `cue.mod/module.cue`:

```sh
cuenv task generate        # ABI bindings, JSON schemas and CRDs
cuenv task build           # Generate prerequisites and build the CLI
cuenv task test-generated  # Regenerate and test the contracts
cuenv task sdk-check       # Install dependencies, generate and typecheck SDK
cuenv task legacy-example  # Generate prerequisites and run the legacy example
cuenv task docsDev         # Prepare and serve Astro documentation
cuenv task docsBuild       # Prepare, check and build the documentation
```

Generated code and schemas are ignored and must never be committed. Run
`cuenv task generate` before invoking lower-level Cargo, SDK or legacy deployment
commands directly from a fresh checkout. Commit canonical sources, generators
and dependency lockfiles. The docs content mirror and static output are also ignored.
