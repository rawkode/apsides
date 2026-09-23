# Resource inventory

`inventory::InventoryStore` is the metadata-only boundary for resource history.
The default backend is SQLite, using a persistent local file on
application-owned storage. The same configured database hosts separate immutable
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

Set `APSIDES_INVENTORY_DATABASE` to an absolute persistent local path. Database
paths are deployment configuration; they are not serialized into registry,
inventory or review data, and a rejected path is never echoed in the error.

There is no remote endpoint. Publication compares and sets the head revision
inside one immediate transaction and then reads the committed body back, which
requires reads to observe the last commit. A local database gives that; an
eventually consistent replica does not.

Writable deployment initialization creates and validates the required tables.
Inventory and coordination expose read-only openers without DDL or repair.
Missing tables, malformed schema, corrupt rows, foreign scope and oversized records
fail closed.

`SqliteInventory::open_local_read_only` opens an existing local database with
SQLite read-only flags. It checks the supported version and required columns
without initialization or repair; reads still validate each requested record.
Publication through that handle rejects before serialization or SQL. Missing
files are not created. This does not eagerly validate every row or certify the
complete SQL table layout.

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

Local write-failure tests exercise the public claim, review and audit publication
methods with a SQLite trigger that fails after insertion. A raw SQL control
checks that the failed statement retains its inserted row inside the transaction;
the store call must roll it back, preserve prior records and return a static error.
A fresh read-only handle verifies absence, then removing the injected failure
permits an exact, idempotent retry. This covers returned statement errors, not
process termination, failed commits, remote storage or cross-store atomicity.

Separate Unix process-crash tests kill and reap an owned child for each local
claim, review and audit table. Before SIGKILL, the child verifies exact candidate
bytes inside a raw immediate transaction and a populated rollback journal under
DELETE/FULL durability settings. It also deletes committed synthetic filler rows
to exercise cache spill. Writable recovery followed by public read-only access
verifies prior records, candidate absence and restored row counts. Two exact
public retries and readback then succeed. These tests qualify local rollback of
the test-owned transaction; they do not interrupt the public publication method
or qualify uncertain commits, persistent volumes, node loss or remote storage.

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

`cuenv task inventoryTest` and `cuenv task coordinationTest` run the durability
tests in `crates/inventory/tests/durability.rs` and
`crates/coordination/tests/durability/`. Each test uses a SQLite file in a
temporary directory.

- Inventory: a canonical snapshot larger than 1 MiB holds only metadata. History
  survives reopening. Two conditional publishers against one revision produce
  one success and one conflict. Rolling back an uncommitted head change with a
  populated rollback journal leaves the committed head in place. A database
  under a missing directory fails to open.
- Claim, review and audit records: a seeded database reopens read-only with
  exact records, and each write through the read-only handle is rejected. Two
  concurrent writers both keep their content-addressed reviews. Of two audits
  for one transition, exactly one is stored and the other gets an exact
  conflict.
- Backup: the test copies all four stores into one standalone file with
  parameterized SQLite `VACUUM INTO`, then syncs the file and its directory. The
  copy reopens through both read-only backends with exact records and inventory
  history, and later source writes do not appear in it. An existing destination
  is refused. Missing or truncated backups fail without repair.

The write-failure and process-crash tests are described above. None of these
tests covers persistent volumes, CSI storage, node or power loss, remote writer
fencing, interruption inside inventory publication, cross-store atomicity or a
production restore procedure.

Canonical state and completion-envelope budgets reject oversized records before
publication. The implementation does not provide snapshot garbage collection,
cross-executor takeover or automatic recovery for missing authority.

See [Inventory, prune and upgrades](/concepts/inventory-prune-upgrades/),
[Transition audit history](/reference/audit/), [State capacity](/reference/state-capacity/),
and the [Deployment protocol](/reference/deployment-contract/).
