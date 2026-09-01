# OxideLoop

OxideLoop is an experimental Rust kernel for continuously reconciling Kubernetes resources described by TypeScript and TSX.

This repository began as an M0 feasibility spike and now includes an experimental, single-root controller path through ownership, durable inventory, pruning, dependency-driven rendering, and declarative event actions. It is still not a production system: interfaces remain provisional and the kind run below is an isolated acceptance harness, not a release or hosted-cluster certification.

The name and every `v0` interface remain provisional through M1 and until a later explicit compatibility freeze.

## Ownership and apply policy (P13-E)

Create remains `POST` with strict no-adoption. Lazy SSA ownership acquisition may
use force only for a fully fenced State Persist write, an inventory-pinned
ConfigMap/Service ConditionalApply, or normal WebApplication status after its
bootstrap Merge. AutomationRun status and every POST, delete, finalizer,
metadata, and bootstrap path remain non-force.

A definite forced-write `409` gets one proof-only reread; it never retries or
stale-rebases. Unproven State or root status holds. A child mutation emits a
token-bound failure and then holds. Canonical legacy objects may be lazily
claimed on their next authorized write; malformed or ownerless objects hold.
Broad installed-cluster migration or reset remains an explicit M5 non-goal.

## Durable mutation lifecycle (P14-E)

Each ordinary mutation has a durable, root-bound lifecycle: `Prepare` records
the frozen render and one next command, `PreparedNotIssued` records that no
dispatch authorization has been granted, `DispatchAuthorizedMayHaveIssued`
records the only point at which a Kubernetes request may have been sent, and
`MutationResult`/completion records its result. A restart at
`PreparedNotIssued` invalidates and replans the transaction; it never replays
the prepared bytes. The kind harness captures this state, restarts the
controller, proves the journal returns to empty, and verifies existing object
UIDs remain unchanged without a POST.

The event materialization window after an Event POST but before durable
`AutomationRun` status is intentionally not an automatic recovery path. With
no trusted admission witness, a restart must hold and require intervention;
the successful kind path therefore documents that ambiguity rather than
claiming exactly-once recovery. The retained `AfterStatusBeforeAck` path is
safe only because the Event UID and status are re-read and pinned exactly.
OxideLoop makes no exactly-once claim for that unresolved window; exactly-once
POST semantics require an admission-side idempotency/witness contract that is
not yet implemented.

## Design boundary

The spike selects an explicitly React-like TSX runtime for a future process-isolated worker. M0 evaluated its callable bundle in the deliberately rejected in-process harness. Ordinary React rendering works for the tested fixture, but unrestricted React is rejected because a layout-effect callback runs during commit. The current in-process `deno_core` host is rejected as a production boundary because it cannot enforce every memory and fatal-failure limit.

The Rust kernel remains the source of truth for input projection, ABI validation, canonical encoding, hashing, and—later—Kubernetes reconciliation. JavaScript receives immutable JSON capabilities and never receives a Kubernetes client.

An empty desired graph is destructive intent, so ordinary `null`, booleans, primitives, empty fragments, and malformed elements fail closed. Author code must return the explicit `explicitEmpty()` sentinel before the ABI will accept an empty graph.

## Workspace

- `crates/renderer-abi`: Rust-owned renderer contracts, schemas, canonical bytes, and hashes.
- `crates/renderer-host`: constrained JavaScript host and containment probes.
- `crates/compiler-spike`: Rust/Oxc TSX transform spike.
- `crates/mutation-model`: model-only conditional mutation planner.
- `crates/spike-runner`: integrated M0 evidence runner.
- `packages/sdk`: generated types and the TypeScript authoring surface.
- `packages/react-renderer`: real React feasibility adapter.
- `packages/react-like-runtime`: deterministic fallback JSX runtime.
- `packages/fixtures`: shared transform, bundle, and runtime fixtures.

See `docs/m0/BOUNDARY.md` before interpreting any test result as Kubernetes or production evidence.

## Authoring a TSX program

The author-facing program is pure TSX. It receives an immutable root projection
and named observations; it cannot obtain a Kubernetes client. `useResource`
declares a dependency, and `useAutomation` declares one immutable action for a
particular observed transition.

```tsx
import { Fragment, jsx } from "@oxideloop/react-like-runtime/jsx-runtime";
import { createRenderer } from "@oxideloop/react-like-runtime";
import { createHookSession, useAutomation, useResource } from "@oxideloop/sdk/hooks";
import { defineAction } from "@oxideloop/sdk";

export const action = defineAction(({ eventMetadata }) => ({
  abiVersion: "renderer.oxideloop.dev/v0",
  actions: [{
    logicalId: "dependency-changed",
    kind: "Event",
    specification: { apiVersion: "events.k8s.io/v1", kind: "Event",
      metadata: { namespace: eventMetadata.namespace },
      reason: "InputChanged", type: "Normal", note: "dependency changed" },
  }], diagnostics: [],
}));

export function render(input) {
  const session = createHookSession({
    observationToken: input.observationToken,
    read: (ref) => input.observations.find((o) => o.name === ref.name) ?? { ref, present: false },
  });
  const inputConfig = useResource(session, { apiVersion: "v1", kind: "ConfigMap", namespace: input.root.namespace, name: "demo-input" });
  if (inputConfig.observation?.uid) {
    useAutomation(session, { identity: "example.input-changed", version: "1",
      sourceDependency: { type: "object", apiVersion: "v1", kind: "ConfigMap", namespace: input.root.namespace, name: "demo-input", uid: inputConfig.observation.uid, resourceVersion: inputConfig.observation.resourceVersion, observationToken: input.observationToken },
      eventMetadata: { namespace: input.root.namespace }, bundleDigest: input.bundleDigest,
      actionModuleIdentity: "example.input-changed", actionModuleDigest: "<build-time digest>", rendererAbiVersion: "renderer.oxideloop.dev/v0" });
  }
  const tree = jsx(Fragment, { children: [jsx("ConfigMap", { apiVersion: "v1", kind: "ConfigMap", metadata: { name: `${input.root.name}-config`, namespace: input.root.namespace }, data: { value: "managed" } })] });
  const rendered = createRenderer(session).render(tree);
  return { abiVersion: "renderer.oxideloop.dev/v0", disposition: rendered.disposition, resources: rendered.resources.map((object) => ({ logicalId: `configmap/${object.metadata.name}`, object, readiness: null })), dependencies: rendered.dependencies, automations: rendered.automations, diagnostics: [] };
}
```

The complete, typechecked version lives in [packages/example-bundle/src/render.tsx](packages/example-bundle/src/render.tsx); its paired action is [packages/example-bundle/src/action.ts](packages/example-bundle/src/action.ts).

## Fresh kind E2E acceptance

One public command builds a new bundle, creates a new kind cluster, builds and
loads separate production and E2E-failpoint images, installs the CRDs and E2E
overlay, and runs the serialized ownership/recovery/deletion matrix:

```sh
scripts/verify-production-no-failpoint.sh
scripts/run-kind-e2e.sh
```

It records the bundle metadata, generated manifests, controller logs, cluster
objects, exact transition snapshots, and deletion midpoint snapshots under the
ignored `work/kind-e2e/<RUN_ID>/` directory. `RUN_ID` defaults to a lowercase,
DNS-safe timestamp-and-process identifier; caller-provided IDs must also be
lowercase. Successful empty command output is retained as evidence rather than
misreported as a capture failure.

`result.json` points to `reproducibility.json` (the clean source revision,
kind/node input, built bundle digest, and loaded image identities) and to an
atomic `artifact-inventory.json` whose entries include every completed evidence
file's path, SHA-256, and byte length. The inventory intentionally excludes its
own bytes and `result.json`, avoiding a self-referential hash. The cluster is
deleted even after failure; use `KEEP_CLUSTER=1` only to inspect a failure. The
runner refuses every nonignored working-tree change; commit or stash all source
changes before starting it. Its own ignored evidence directory is the only
run-time write.

The fresh kind run proves controller/renderer process separation and rendered
Kubernetes lifecycle behavior. Finite renderer protocol framing, EOF,
deadlines, and response-identity guarantees are covered by the Rust protocol
tests and `scripts/verify-production-no-failpoint.sh`; they are not claims made
by kind lifecycle evidence alone.

For OrbStack, start its Docker-compatible engine and run the same command. The
runner detects OrbStack only within its own process and selects Docker; callers
may override that choice with `KIND_EXPERIMENTAL_PROVIDER`. Common failures are
missing `kind`, `kubectl`, `docker`, `jq`, or `pnpm`; an unavailable Docker API;
or a bounded wait timeout. Inspect `runner.log`, `controller.log`,
`pods.describe.txt`, and `result.json` in the evidence directory before rerun.
This harness is an acceptance path. Treat a run as passed only when its
`result.json` records `status: "passed"` for the exact source revision.

## Local renderer checks

```sh
pnpm install --frozen-lockfile
pnpm run typecheck
pnpm run test
pnpm run fixtures:bundle
cargo test --workspace --locked
cargo run -p spike-runner --locked -- evaluate --phase render --fixture packages/fixtures/dist/react-like.callable.js --input packages/fixtures/host-render-input.json --repeat 100
cargo run -p spike-runner --locked -- render-safety-probe --explicit-fixture packages/fixtures/dist/react-like-empty.callable.js --null-fixture packages/fixtures/dist/react-like-null.callable.js --input packages/fixtures/host-render-input.json
```

The roadmap deliberately adds a read-only controller before any cluster mutation. See `docs/ROADMAP.md` for the staged Rust-kernel build.
