# Reakt TSX example

`program.tsx` declares the controller boundary and composes the reusable `Api`
component from `api.tsx`. The component declares Kubernetes resources with JSX
and observations with `useConfigMap` and `useSecret`. `rolloutOn` connects opaque
input projections to a Deployment; it never reads live values during the build.

Compile the digest-locked example from the repository root:

```sh
scripts/reakt/source-build.sh examples/reakt/program.tsx work/reakt-source
```

Choose a fresh output directory for each build. The compiler supplies source
locations and provenance, evaluates the component tree in its isolated build
process, and emits data for a native Kubernetes controller. No React or
TypeScript runtime is deployed into the cluster.

After editing either source file, refresh the explicit source lock:

```sh
scripts/reakt/source-lock.sh examples/reakt/program.tsx examples/reakt/api.tsx
```

The source build performs a Deno type check, two Oxc/Rolldown builds and two
permission-restricted Deno/SES evaluations. It generates production IR,
separate test fixtures, source maps and the toolchain receipt. Generated files
stay in ignored output directories. Fixtures never enter production IR.

Resource locations identify their JSX declarations. Observation hooks currently
identify the containing component's JSX invocation, rather than the hook's
individual line. These locations are diagnostics, not authorization.

The workload image digest is illustrative. The `api-workload` ServiceAccount
and external ConfigMap/Secret are platform prerequisites. Compilation does not
establish deployment or production release readiness.
