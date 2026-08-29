# M0 renderer decision

**Status: conditional Go for M1 architecture work**

| Candidate | Status | Reason |
|---|---|---|
| Actual React + `react-reconciler` | NoGo | The ordinary workload is deterministic, but `useLayoutEffect` executes once during the synchronous commit. Unrestricted React cannot satisfy the pure-render contract. |
| Explicit React-like JSX runtime | Go | The same workload produced canonical hash `f09b9b06...47a` in 100 fresh isolates for each of four UTC/Auckland and C/en_US.UTF-8 combinations. |
| In-process `deno_core` host | NoGo | Capability lockdown and watchdog probes pass, but the process cannot enforce the declared heap limit or contain V8/native fatal exits. |

M1 may proceed with a Rust controller kernel, the explicit React-like runtime, and a process-isolated renderer worker. It may not ship the current in-process host, claim unrestricted React compatibility, or write to a Kubernetes API.

## Evidence

- Rust/Oxc lowers the shared TypeScript/TSX fixture with the automatic JSX runtime.
- React-like and ordinary React workloads each completed the happy path in 400 fresh isolates across the locale/time-zone matrix with the same Rust-canonicalized hash.
- The typed actual-React lifecycle probe reported exactly one layout-effect callback.
- Every public host completion has passed strict phase-specific Rust input and output validation; raw execution remains private to unit probes.
- The selected workload records one deduplicated absent `Service` dependency with null UID/resourceVersion, and Rust rejects half-present dependency identities.
- Only the root `explicitEmpty()` sentinel authorizes an empty desired graph. Component-returned null, booleans, primitives, empty fragments, malformed or unknown elements, and a nested sentinel fail closed; a bundled host probe covers the sentinel-versus-null boundary.
- JSON is transferred through captured `JSON.parse`, with regressions for `__proto__`, `constructor`, escaped source-like strings, Unicode separators, and nesting.
- The pinned capability gate compares property descriptors and prototype identities for every allowed global and every determinism override, not only global names.
- Eight conditional-mutation fixtures matched the pure Rust model. No Kubernetes API server was contacted.
- Supervised recoverable, infinite-loop, and fatal child probes distinguish typed failure classes and preserve the parent process.
- ABI tests cover recursive duplicate keys, unknown fields, depth and byte boundaries, unsafe numbers, UTF-16 key ordering, generated-schema drift, and cross-spelling Rust goldens.
- The evaluated host is pinned to `deno_core` 0.411.0, `deno_v8` 0.3.0, and Rust `v8` 150.4.0.

These results choose a direction; they are not production-security or Kubernetes-conformance evidence.
