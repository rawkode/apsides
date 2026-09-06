# Resource inventory

`inventory::InventoryStore` is the metadata-only boundary for resource history.
The default backend is libSQL, using either a persistent local file or a remote
TLS libSQL/Turso primary. The same configured database hosts separate immutable
claim, review and audit records; those records do not grant each other authority.

## Stored data

An immutable inventory snapshot is scoped to the registry incarnation, cluster,
control and workload namespaces, domain and generation. Each resource records its
API identity, namespace, name, observed UID and deletion policy. The conditionally
updated head identifies the current committed snapshot.

Snapshots exclude manifests, desired and observed values, arbitrary labels and
annotations, source text, credentials, rollout keys, and private-content hashes.
Names and source-free identities remain operational metadata and must not contain
secrets. Canonical encoding, typed scope validation and digest verification apply
on every read, including results returned by a backend implementation.

## Deployment connection

Set `REAKT_INVENTORY_DATABASE` to an absolute persistent local path or to an
approved `libsql://` or `https://` endpoint. Remote access also requires
`REAKT_INVENTORY_TOKEN`. URLs, tokens and database paths are deployment
configuration; they are not serialized into registry, inventory or review data.

Writable deployment initialization creates and validates the required tables.
Read-only audit access performs no DDL or repair. Missing tables, malformed schema,
corrupt rows, foreign scope and oversized records fail closed.

## Publication and recovery

The runtime publishes a bounded inventory observation only after rereading the
protected activation and preserving its UID/resourceVersion. Deployment reads the
immutable predecessor snapshot referenced by the protected transition and checks
its exact scope, generation and contents before planning cleanup or ownership.

Final transition commit and inventory-head publication are separate conditional
operations. A lost response or database failure therefore retries the same
committed transition and immutable snapshot. An existing identical snapshot is
idempotent; a conflicting object under the same reference is an error. Inventory
does not prove writer termination, authorize mutation, fill a missing protected
claim UID, or turn missing history into an initial installation.

## Upgrade use

Cleanup uses the predecessor inventory's UID and deletion policy together with
the protected claim. `delete` requires exact live UID matching and a preconditioned
delete. `orphan` releases tracking without deleting the object. A missing trusted
UID permits only an absence check.

Registry schema v8 retains compact transition inputs and immutable inventory,
claim, review and audit references. Selected sessions reconstruct the plan from
the freshly verified release; no previous manifest, compiled IR or OCI artifact is
loaded. Current committed records remain available for idempotent retry until a
successor archives them.

## Current limits

Canonical state and completion-envelope budgets reject oversized records before
publication. The implementation does not provide snapshot garbage collection,
cross-executor takeover, automatic recovery for missing authority, or completed
live qualification of persistent-volume and remote Turso recovery.

See [Inventory, prune and upgrades](/concepts/inventory-prune-upgrades/),
[Transition audit history](/reakt/audit/), [State capacity](/reakt/state-capacity/),
and the [Deployment protocol](/reakt/deployment-contract/).
