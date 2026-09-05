---
title: Inventory, prune and upgrades
description: The metadata, lifecycle policies and authority used across controller generations.
---

Upgrades need the identity and lifecycle policy of previously managed resources.
They do not need the previous resource manifests, TypeScript source, compiled IR
or historical OCI artifact.

## Metadata-only inventory

`inventory::InventoryStore` records immutable snapshots and a conditionally
updated head. Each resource entry contains only:

- cluster and controller scope;
- generation;
- API group, resource, version and kind;
- namespace and name;
- observed UID; and
- deletion policy.

It excludes manifests, desired and observed values, arbitrary labels and
annotations, source paths, credentials, and hashes of private contents. Names
are operational metadata and should not contain secrets.

The default implementation is libSQL. A local deployment uses a persistent
absolute database path; a remote deployment uses a TLS `libsql://` or `https://`
primary and a token supplied by the trusted job environment. A future backend
can implement the same immutable-snapshot and conditional-head contract.

## Prune is a compiled lifecycle choice

`defineDomain({ prune: true })` makes `delete` the default for resources removed
by a later generation. `prune: false` defaults to `orphan`. A resource-level
`deletionPolicy` overrides the domain default. The effective policy is stored
with that resource in the approved plan and inventory.

During cleanup:

- `orphan` releases tracking and leaves the live resource in place;
- `delete` requires the protected claim and live object to match the recorded
  UID before issuing a preconditioned deletion; and
- a missing trusted UID permits only an absence check, never deletion by name.

Finalizers remain Kubernetes-owned. Inventory stays pending until a requested
deletion actually completes.

## Upgrade ownership

Privileged CI supplies and verifies the new release, creates generation-specific
Roles and RoleBindings, deploys the new controller image, activates it, and
retires the old writer. The runtime controller does not administer RBAC, fetch
inventory credentials or upgrade itself.

The intended upgrade inputs are:

1. the new release supplied by CI;
2. the previous metadata-only inventory snapshot; and
3. compact protected coordination for active generation and writer retirement.

OCI can distribute the new release when requested. Recovery and prune decisions
must not depend on pulling an old OCI artifact.

## Failure behaviour

Missing, corrupt or unrelated inventory is a recovery condition, not an empty
deployment. A restarted process must reload the existing head. Conditional
publication rejects stale writers; retrying the exact committed snapshot is
idempotent, including after a lost response.

Kubernetes coordination and the inventory database do not share a transaction.
If inventory publication fails after the Kubernetes transition commits, retry
the same transition. The database tracks resource identity; it does not grant
mutation authority or prove that an old writer stopped.

## Current implementation boundary

The trait, libSQL backend, deployment connection and several publication checks
are implemented and covered by focused tests. Compact registry storage, recovery
from installations without inventory, and live PVC/Turso upgrade qualification
remain outstanding. The current registry still serializes complete transition
plans.

For the canonical details, read [Resource inventory](/reakt/inventory/),
[State migration](/reakt/state-migration/), and the
[Deployment protocol](/reakt/deployment-contract/).
