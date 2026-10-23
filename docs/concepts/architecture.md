---
title: Architecture
description: How Apsides separates build-time declarations from runtime Kubernetes authority.
---

Apsides separates build-time authoring from runtime authority. TypeScript produces
data. Rust validates that data, derives the allowed effects and performs the
Kubernetes operations.

<div class="architecture-flow">
  <div><strong>Authoring</strong><span><code>@apsides/kubernetes</code> builds a domain, resources, observations, reactions and event handlers.</span></div>
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

An `<On>` element names an event handler: a locked TypeScript module export
with declared inputs, HTTPS egress hosts and output `ConfigMap`s. The compiler
bundles the module and records its digest; it does not run it. See
[Event handlers](/guides/event-handlers/).

## Compiler and release boundary

The compiler produces canonical resource and reaction plans, capabilities,
source maps and scenario requirements. A complete build also produces a Linux
controller binary, OCI archive, SBOM and compiler closure.

Local artifact verification checks integrity and recomputes the plan. Deployment
accepts a separately verified release under platform-owned trust policy. OCI is
an optional distribution format for that new release, not the durable source of
prior state.

## Runtime boundary

The controller cannot expand its resource set. It uses strict Kubernetes
Server-Side Apply only. There is no client-side apply, merge patch, strategic
merge patch, full-object replacement or delete/recreate fallback for
managed-resource reconciliation.

The only code the controller runs besides its own is the set of declared
event handler bundles embedded in its binary, checked against the digests in
its validated IR at startup. Each invocation runs in a fresh V8 isolate from
the `engine` crate, with no filesystem, no Kubernetes credential, bounded time
and memory, and three operations: HTTPS `fetch` to the handler's declared
hosts, `writeOutput` to its declared outputs, and `log`. Handler output writes
use their own field manager and the same authority checks as reconciler
writes.

Each controller generation receives only its approved runtime capabilities.
Privileged CI owns Roles, RoleBindings, the controller image, activation and
retirement of the previous writer. A generation-specific proxy and protected
coordination records form the writer-quiescence boundary.

The generated controller reads the filesystem only during a short, synchronous
startup. It reads its startup configuration, the proxy client identity (or the
fixture kubeconfig) and `/etc/resolv.conf`, converts them into in-memory
values, and sizes the async runtime from the cgroup-aware CPU count. It then
applies a deny-all Landlock ruleset on the main thread while no other thread
exists, and only afterwards starts the V8 platform and the tokio runtime, whose
threads inherit the restriction. The production Kubernetes client reaches the
proxy Service by hostname through a `hickory-resolver` connector configured
from the `resolv.conf` contents read before lockdown, so reconnects never
open a file. V8's random number generators are seeded from the `getrandom(2)`
system call rather than `/dev/urandom`.

A kernel that cannot enforce Landlock stops the controller at startup. A
partially enforced ruleset (the normal state on kernels older than Linux 7.1,
whose Landlock ABI lacks only the `ResolveUnix` right) counts as enforced and is
reported separately at `/status` as `landlock-partial`. Only a fixture-mode
configuration for disposable clusters can opt out with `allowNoLandlock`;
production configuration has no such field. Some glibc and V8 internal reads
still happen after lockdown, such as CPU-count and thread stack probes; each is
denied and falls back to a file-free source. The
[implementation status](/reference/status/) lists them.

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
| `runtime` | native fixed-plan reconciliation, handler dispatch and output commit, and the synchronous startup with Landlock lockdown |
| `engine` | the embedded `deno_core`/V8 handler sandbox: isolate limits, the `apsides` operations, egress address vetting and the Landlock primitives |
| `release` | release integrity, signed provenance, compiler closure and OCI verification |
| `packaging` | the native build behind `aps build`: build context, network-less compile, image assembly, SBOM and compiler closure |
| `container` | podman and Docker selection, command lines and output parsing |
| `inventory` | metadata-only snapshots and SQLite storage |
| `coordination` | typed immutable stores for claims, transition audits and review metadata |
| `cli` | `aps` build, verification and deployment commands |
| `test-cluster` | `aps test --cluster`: generated reactor cases on a disposable kind cluster |
| `acceptance` | the acceptance suite: the CNPG examples deployed with `aps deploy` on kind |

Internal packages use role names without product prefixes. The current executable
is `aps`, and the authoring package is `@apsides/kubernetes`.
