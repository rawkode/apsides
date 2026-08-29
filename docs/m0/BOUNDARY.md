# M0 evidence boundary

M0 is a local feasibility spike. Its purpose is to decide whether a capability-minimal Rust host can safely evaluate TypeScript/TSX authored with actual React, or whether OxideLoop must expose an explicitly React-like runtime.

Passing M0 can demonstrate:

- Rust-owned validation, canonical encoding, and hashing of the provisional renderer ABI.
- A Rust/Oxc TypeScript and JSX lowering stage, separate from type checking and bundling.
- Execution of a self-contained JavaScript bundle without filesystem, network, environment, clock, random, or arbitrary module-loader authority.
- Separation of render capabilities from action capabilities.
- Determinism for the exact tested bundles, host, V8 version, platforms, locales, and inputs.
- Parent-process survival when deliberately fatal probes run in supervised child processes.
- A pure state-machine model for constructing conditional Kubernetes mutation requests.

M0 cannot demonstrate:

- Correct behavior of a Kubernetes API server, server-side apply, watch delivery, admission, CRDs, or aggregated API servers.
- Controller reconciliation, finalizers, durable state, transactions, inventory, pruning, readiness, deletion recovery, or leader election.
- Safe automation execution or Job materialization.
- High availability, hostile multi-tenancy, production security, or release compatibility.
- In-process recovery from a fault that terminates the child process. Parent survival is containment evidence, not isolate recovery.

No `v0` contract in this repository is frozen. A passing model test must never be reported as Kubernetes conformance.

## Phase boundary

Render evaluation may receive immutable projected input and only the declared observation operations. Action evaluation may receive immutable occurrence metadata and only action declaration operations. An action cannot read render observations; a render cannot declare an action result through an action-only operation.

For M0 fixtures only, omission of an exact named object from the closed observation array is encoded as an absent dependency with null UID and resourceVersion. This is not proof of cache synchronization, watch activation, durable absence, or Kubernetes resourceVersion semantics; M2 must define that operational contract before the controller acts on absence.

Observation tracking is cooperative in M0: the trusted fixture environment receives the complete closed observation array and records dependencies when SDK hooks read it. M1 must replace direct array exposure with worker-host operations that expose only requested observations and record every read at the boundary.

The M0 fixture normalizes omitted namespaced references to the root namespace. An empty namespace is not yet a frozen signal for cluster scope. M2 must couple namespace normalization to discovery-backed resource scope and the root program's explicit capabilities before cluster-scoped reads or writes are possible.

An empty graph is treated as destructive intent. Only the root `explicitEmpty()` sentinel can produce the `explicitEmpty` disposition; accidental null, boolean, primitive, empty fragment, malformed element, or nested sentinel values fail before the Rust ABI receives an empty desired graph.

Both phases reject ambient runtime authority. Errors crossing the host boundary use opaque host-owned categories and must not include guest-authored text, source paths, JavaScript stacks, environment values, or observed object bodies.

`crates/renderer-host` is a spike-only falsification harness. M1 must replace its in-process thread boundary with a separately constrained process; it must not link this host into the controller and reinterpret `ProcessFatal` as recoverable.
