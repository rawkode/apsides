# WebApplication example

This example is a complete TypeScript-authored Kubernetes controller. Its
single source file is
[`packages/example-bundle/src/controller.tsx`](../../packages/example-bundle/src/controller.tsx).
That file is also checked byte-for-byte against the frozen v1 fixture used by
the compiler tests.

The controller declares:

- a ConfigMap, Deployment, and optional Service;
- enrichment defaults shared by all three resources;
- a watched `demo-input` ConfigMap through `useResource`;
- a dependency-driven rerender of `root-config.data.message`; and
- one `configuration-changed` automation that emits a Kubernetes
  `ConfigurationChanged` Event for each observed source transition.

Run the commands below from the repository root.

## Check and compile the controller

Install the pinned workspace dependencies, typecheck the exact source, run its
local render checks, and build the immutable bundle consumed by the Rust
controller:

```sh
pnpm install --frozen-lockfile
pnpm --filter @oxideloop/example-bundle typecheck
pnpm --filter @oxideloop/example-bundle test
pnpm bundle:e2e -- \
  --out work/example/bundle.archive \
  --metadata work/example/bundle-metadata.json
```

Inspect the bundle identity with:

```sh
jq '{
  authoredSource: .buildBundle.authoredSource,
  bundleDigest: .buildBundle.bundleDigest,
  action: .buildBundle.actionModule,
  render: .buildBundle.renderModule
}' work/example/bundle-metadata.json
```

## Run the complete example on a fresh kind cluster

Prerequisites are a running Docker-compatible daemon plus `git`, `kind`,
`kubectl`, `jq`, `pnpm`, Rust, and Cargo. The acceptance runner intentionally
requires a clean committed checkout so every captured result names the exact
source revision it exercised.

```sh
test -z "$(git status --porcelain --untracked-files=all)"

RUN_ID="web-example-$(date -u +%Y%m%dt%H%M%Sz)-$$" \
  examples/web-application/run-kind-e2e.sh
```

The wrapper calls the repository's authoritative lifecycle runner. It creates
a disposable cluster, builds and loads the controller and isolated renderer
images, compiles and publishes the exact TSX bundle, installs OxideLoop, and
applies a `WebApplication`. A successful run verifies:

1. ConfigMap, Deployment, and Service creation with exact root ownership and
   durable inventory entries.
2. Server-side apply ownership and stable child UIDs across reconciliation.
3. An initial dependency baseline without an Event, followed by exactly one
   correlated `AutomationRun` and Event for each `demo-input.data.message`
   transition.
4. Recovery across controller restarts without recreating managed resources or
   duplicating completed automations.
5. Service pruning when `spec.expose` becomes false while the ConfigMap and
   Deployment remain.
6. Cleanup of managed resources, automation state, and Events when the root is
   deleted.

The runner also exercises the repository's guarded foreign-name collision and
durability boundaries. Those checks are part of the same lifecycle proof; no
second example implementation is maintained here.

## Inspect evidence

Every run writes an ignored evidence directory at
`work/kind-e2e/<RUN_ID>/`. After the command finishes, inspect its summary:

```sh
jq '{status,sourceRevision,mode,clusterDisposition,failureReason}' \
  work/kind-e2e/<RUN_ID>/result.json
```

Success requires `status: "passed"` and `clusterDisposition: "deleted"`.
`reproducibility.json` records the source revision, bundle digest, kind node
image, and loaded controller/renderer image identities. The artifact inventory
contains hashes for the captured Kubernetes objects and logs.

Set `KEEP_CLUSTER=1` only while diagnosing a failure. You are then responsible
for deleting the exact cluster printed by the runner.

## Compatibility boundary

This repository is still pre-release. The example's `WebApplication` requires
`spec.image`; legacy `spec.message` is accepted only as an optional compatibility
field. The generated CRD change is intended for a fresh cluster and is not a
supported in-place upgrade from an older message-only installation.
