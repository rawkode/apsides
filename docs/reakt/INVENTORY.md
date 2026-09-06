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

`LibsqlInventory::open_local_read_only` opens an existing local database with
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
cuenv exec python3 tests/reakt/storage/build.py --output work/storage-image
cuenv exec python3 tests/reakt/run_storage_restart.py --image-directory work/storage-image --output work/storage-results
```

Use fresh output directories. The live harness creates and removes its own kind
cluster and isolated kubeconfig. Its static local PV uses the disposable node's
filesystem. It checks inventory larger than 1 MiB, retained snapshots across Pods,
concurrent conditional publication, rollback after a writer SIGKILL, and read-only
storage refusal. This does not qualify CSI storage, node or power failure, remote
Turso, writer fencing, or interruption inside inventory publication. Build outputs
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
cuenv exec python3 tests/reakt/storage/build.py --package coordination --output work/coordination-image
cuenv exec python3 tests/reakt/run_coordination_restart.py --image-directory work/coordination-image --output work/coordination-results
```

Local backup tests (`cuenv task coordinationProbeTest`) seed all four stores in
one database and use parameterized libSQL `VACUUM INTO` to create one standalone
backup. They synchronize the completed file and its parent directory, reopen it
through both read-only backends, verify exact records and inventory history, and
check that later source writes are absent. Existing destinations are refused;
missing or truncated backups fail without repair. This is local filesystem test
evidence, not PVC restore, power-loss durability, or authority to resume a deploy.

Canonical state and completion-envelope budgets reject oversized records before
publication. The implementation does not provide snapshot garbage collection,
cross-executor takeover, automatic recovery for missing authority, or completed
live qualification of remote Turso recovery or persistent-volume recovery for
claim, review and audit stores after interrupted writes or backup restoration.

See [Inventory, prune and upgrades](/concepts/inventory-prune-upgrades/),
[Transition audit history](/reakt/audit/), [State capacity](/reakt/state-capacity/),
and the [Deployment protocol](/reakt/deployment-contract/).
