---
title: Architecture
description: How Apsides separates build-time declarations from runtime Kubernetes authority.
---

Apsides separates build-time authoring from runtime authority. TypeScript produces
data. Rust validates that data, derives the allowed effects and performs the
Kubernetes operations.

<div class="architecture-flow">
  <div><strong>Authoring</strong><span><code>@reakt/kubernetes</code> builds a domain, resources, observations and reactions.</span></div>
  <div><strong>Compiler</strong><span>Oxc, Rolldown, restricted Deno evaluation and Rust validation emit canonical IR.</span></div>
  <div><strong>Release</strong><span>A native binary, OCI image, SBOM, compiler closure and evidence are bound together.</span></div>
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
their own typed immutable stores. libSQL is the default backend, using either a
persistent local file or a remote Turso/libSQL primary. Protected Kubernetes
coordination stores compact transition inputs, immutable references, capacity
admission, writer checkpoints and the executor binding needed for conditional
lifecycle updates.

See
[Inventory, prune and upgrades](/concepts/inventory-prune-upgrades/) and the
[implementation status](/reakt/status/).

## Crate ownership

| Package | Responsibility |
| --- | --- |
| `compiler-core` | strict IR validation, canonical identity, capabilities and policy checks |
| `deploy` | provider-neutral lifecycle planning and transition reduction |
| `platform` | protected Kubernetes state, proxy transport and generation lifecycle |
| `runtime` | native fixed-plan reconciliation |
| `release` | signed release, compiler closure, OCI and evidence verification |
| `inventory` | metadata-only snapshots and libSQL storage |
| `cli` | `reakt` build, verification and deployment commands |

Internal packages use role names without product prefixes. The current executable
is `reakt`, and the authoring package is `@reakt/kubernetes`.
