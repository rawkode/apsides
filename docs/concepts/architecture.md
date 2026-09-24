---
title: Architecture
description: How Apsides separates build-time declarations from runtime Kubernetes authority.
---

Apsides separates build-time authoring from runtime authority. TypeScript produces
data. Rust validates that data, derives the allowed effects and performs the
Kubernetes operations.

<div class="architecture-flow">
  <div><strong>Authoring</strong><span><code>@apsides/kubernetes</code> builds a domain, resources, observations and reactions.</span></div>
  <div><strong>Compiler</strong><span>Oxc, Rolldown, restricted Deno evaluation and Rust validation emit canonical IR.</span></div>
  <div><strong>Release</strong><span>A native binary, OCI image, SBOM, compiler closure and signed provenance are bound together.</span></div>
  <div><strong>Runtime</strong><span>A generation-specific controller reconciles only its approved plan through a constrained proxy.</span></div>
</div>

## Build-time TSX

The authoring program composes TSX components into a fixed graph. Observation
hooks declare inputs, and resource props declare desired state and rollout dependencies. It cannot make Kubernetes calls,
read arbitrary files, use the network, inspect environment variables or supply
runtime callbacks. The supported source path type-checks the declared closure,
runs independent bundle and evaluation passes, and then validates the emitted
IR in Rust.

This is deliberately smaller than general React or an unrestricted Kubernetes
SDK. Unknown fields and unsupported resource shapes fail compilation.

## Compiler and release boundary

The compiler produces canonical resource and reaction plans, capabilities,
source maps and scenario requirements. A complete build also produces a Linux
controller binary, OCI archive, SBOM and compiler closure.

Local artifact verification checks integrity and recomputes the plan. Deployment
accepts a separately verified release under platform-owned trust policy. OCI is
an optional distribution format for that new release, not the durable source of
prior state.

## Runtime boundary

The controller has no JavaScript evaluator and cannot expand its resource set.
It uses strict Kubernetes Server-Side Apply only. There is no client-side apply,
merge patch, strategic merge patch, full-object replacement or delete/recreate
fallback for managed-resource reconciliation.

Each controller generation receives only its approved runtime capabilities.
Privileged CI owns Roles, RoleBindings, the controller image, activation and
retirement of the previous writer. A generation-specific proxy and protected
coordination records form the writer-quiescence boundary.

The generated controller intentionally reads the filesystem only during a
short, synchronous startup: it reads its startup config, the proxy client
identity (or the fixture kubeconfig), `/etc/resolv.conf`, and
`available_parallelism()`'s cgroup-aware CPU count, converts them into
in-memory values, then applies a deny-all Landlock ruleset to the process's
main thread before any other thread exists and before the V8 platform or the
tokio runtime is initialised. Threads created afterward -- V8's worker pool,
the tokio runtime's own workers -- inherit that restriction. The production
kube client reaches the proxy Service by its original hostname (resolved
through `resolv.conf`'s own `search`/`ndots` directives, read before
lockdown) through a dedicated `hickory-resolver`-backed connector rather than
glibc's `getaddrinfo`, so a proxy reconnect never needs a file again, and V8's
platform and per-isolate RNGs are seeded from `getrandom(2)` directly rather
than a `/dev/urandom` read Landlock would deny. `strace` against the locked
down process still shows two glibc/V8-internal reads after lockdown (V8's own
CPU-count path, and a new thread's stack-bounds probe); both are denied and
both already fall back to a working, file-free alternative (the process's CPU
affinity mask, and `__libc_stack_end`, respectively) with no code here
involved. An unenforced Landlock lockdown fails startup closed in production;
a `PartiallyEnforced` kernel (an older Landlock ABI missing some requested
rights) still counts as enforced, reported distinctly at `/status`. A
fixture-only `allowNoLandlock` escape hatch exists for disposable-cluster
testing and is structurally unavailable in production mode.

## Policy and force

SSA force defaults to `false`. Authors may set a domain default with
the Controller `force` prop and override it with an individual resource
component's `force` prop. Compilation records the effective value and emits
an exact `force-apply:<Kind>/<namespace>/<name>` risk. Independent platform
policy must accept that risk before deployment.

Force transfers SSA field ownership. It does not bypass object UID checks,
adoption approval, lifecycle policy, finalizers or old-writer termination.

## Persistence boundaries

Resource history belongs in immutable metadata-only inventory through the
`InventoryStore` trait. Review metadata, claims and transition audit records use
their own typed immutable stores. SQLite is the default backend, using a
persistent local file on application-owned storage. Protected Kubernetes
coordination stores compact transition inputs, immutable references, capacity
admission, writer checkpoints and the executor binding needed for conditional
lifecycle updates.

See
[Inventory, prune and upgrades](/concepts/inventory-prune-upgrades/) and the
[implementation status](/reference/status/).

## Crate ownership

| Package | Responsibility |
| --- | --- |
| `compiler-core` | strict IR validation, canonical identity, capabilities and policy checks |
| `deploy` | provider-neutral lifecycle planning and transition reduction |
| `platform` | protected Kubernetes state, proxy transport and generation lifecycle |
| `runtime` | native fixed-plan reconciliation |
| `engine` | Phase 5a spike gate for the embedded `deno_core`/V8 handler engine (feasibility evidence only, not yet wired into `runtime` or `aps build`) |
| `release` | release integrity, signed provenance, compiler closure and OCI verification |
| `inventory` | metadata-only snapshots and SQLite storage |
| `coordination` | typed immutable stores for claims, transition audits and review metadata |
| `cli` | `aps` build, verification and deployment commands |
| `test-cluster` | `aps test --cluster`: generated reactor cases on a disposable kind cluster |
| `acceptance` | the acceptance suite: the CNPG examples deployed with `aps deploy` on kind |

Internal packages use role names without product prefixes. The current executable
is `aps`, and the authoring package is `@apsides/kubernetes`.
