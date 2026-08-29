# OxideLoop

OxideLoop is an experimental Rust kernel for continuously reconciling Kubernetes resources described by TypeScript and TSX.

This repository currently contains the **M0 feasibility spike**. M0 is intentionally local-only: it evaluates the renderer boundary, real React versus an explicitly React-like runtime, deterministic ABI encoding, capability isolation, and a model of conditional Kubernetes mutations. It does not connect to a cluster or mutate Kubernetes resources.

The name and every `v0` interface remain provisional through M1 and until a later explicit compatibility freeze.

## M0 result

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

## Reproduce the positive path

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
