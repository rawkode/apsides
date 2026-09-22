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

The local storage qualification is available through cuenv:

```sh
cuenv task storageProbeTest
cuenv task storageBuildTest
cuenv task storageRestartTest
cuenv exec python3 tests/storage/build.py --output work/storage-image
cuenv exec python3 tests/run_storage_restart.py --image-directory work/storage-image --output work/storage-results
```

Use fresh output directories. The live harness creates and removes its own kind
cluster and isolated kubeconfig. Its static local PV uses the disposable node's
filesystem. It checks inventory larger than 1 MiB, retained snapshots across Pods,
concurrent conditional publication, rollback after a writer SIGKILL, and read-only
storage refusal. This does not qualify CSI storage, node or power failure, remote
writer fencing, or interruption inside inventory publication. Build outputs
and results remain ignored; the image is a test artifact, not a production release.

The local-PV qualification passed with a 2,220,976-byte canonical inventory,
separate-Pod history reads, one successful and one conflicting publisher, and
recovery after SIGKILL with an observed populated rollback journal. Read-only
refusal and subsequent healthy reopen also passed. These results cover the
inventory store on the tested local PV, not the other durable stores.

Current-format claim, review and audit records separately passed a writable
seed followed by exact reference/body verification in a second Pod on the same
local PV. The second Pod mounted the volume read-only, and all three store write
methods rejected writes. Competing writer Pods also passed: identical claims
were idempotent, distinct reviews at one scope both persisted, and two audit
candidates for one immutable transition produced one winner and one exact
conflict. A fresh read-only Pod verified both seed and race records, including
rejection of the losing audit reference. This uses small synthetic fixtures and
does not prove transaction overlap, cross-store atomicity, crash recovery or backup.

```sh
cuenv exec python3 tests/storage/build.py --package coordination --output work/coordination-image
cuenv exec python3 tests/run_coordination_restart.py --image-directory work/coordination-image --output work/coordination-results
```

Local backup tests (`cuenv task coordinationProbeTest`) seed all four stores in
one database and use parameterized SQLite `VACUUM INTO` to create one standalone
backup. They synchronize the completed file and its parent directory, reopen it
through both read-only backends, verify exact records and inventory history, and
check that later source writes are absent. Existing destinations are refused;
missing or truncated backups fail without repair. This is local filesystem test
evidence, not power-loss durability or authority to resume a deploy.

The live backup qualification also passed on two distinct local PVCs. A backup
Pod mounted the source read-only and the destination writable; a later Pod
changed all four source stores. The verifier mounted only the backup PVC,
read-only, and recovered the original records and inventory revision 2 while
excluding the later source revision 3 and its metadata. The two volumes share
one node, so this does not establish independent failure domains, node-loss
recovery, or a production restore procedure.

```sh
cuenv exec python3 tests/run_storage_backup.py --image-directory work/coordination-image --output work/backup-results
```

Canonical state and completion-envelope budgets reject oversized records before
publication. The implementation does not provide snapshot garbage collection,
cross-executor takeover, automatic recovery for missing authority, or completed
live qualification of persistent-volume recovery for
claim, review and audit stores after interrupted writes.

See [Inventory, prune and upgrades](/concepts/inventory-prune-upgrades/),
[Transition audit history](/reference/audit/), [State capacity](/reference/state-capacity/),
and the [Deployment protocol](/reference/deployment-contract/).
