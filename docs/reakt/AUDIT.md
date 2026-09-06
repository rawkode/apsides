# Transition audit history

The protected namespace registry retains each domain's current committed
transition and at most one unfinished transition. Before a successor replaces the
committed record, deployment archives that predecessor through `AuditStore`. The
default libSQL/Turso backend shares the configured metadata database with claims,
reviews and inventory through separate typed tables.

Audit records contain registry scope, canonical allocation identities and
sequence, executor identity, phase events, ownership actions, cleanup outcomes,
and retired resource identities and UIDs. They contain no manifests, source,
desired values, credentials, blocked-reason strings, or artifact, IR, plan,
evidence or private-content hashes. An audit record grants no ownership, deletion,
deployment or resume authority.

Deployment writes the already committed predecessor and verifies an exact typed
readback before the Kubernetes registry CAS removes that predecessor. The same CAS
commits the successor, promotes its active writer checkpoint and advances the
domain's protected audit head. Each audit links to the preceding reference, so
the registry remains bounded while the database retains an addressable chain.

An audit write followed by a lost registry CAS can leave an unselected row. That
row describes a transition that had already committed; it does not claim the
successor committed. Conflicting immutable rows, missing or corrupt prior heads,
false backend readbacks and malformed protected chains block pruning. Audit
garbage collection is not implemented.

## Read transition status

Use standard Kubernetes authentication with exact GET permission for the registry
and its authority anchor. Configure a read-only metadata credential for remote
storage:

```sh
reakt audit \
  --domain team-api-production \
  --transition TRANSITION_ID \
  --cluster-binding production \
  --namespace production \
  --control-namespace reakt-system
```

`REAKT_INVENTORY_DATABASE` selects the existing absolute local database or TLS
libSQL endpoint. `REAKT_INVENTORY_TOKEN` supplies remote authentication. The
command does not initialize or repair tables: local SQLite opens read-only and
remote access performs SELECTs against an existing validated schema.

A current record returns `status: retained` and its recorded phase. An archived
record returns `status: retired` and sanitized metadata reached through the
authenticated protected head chain. Both outputs set `authority: false`. A
retired transition cannot reactivate its identity; deploying an older build still
allocates a new generation.

Missing or corrupt referenced data is an error, not empty history. Lookup has a
bounded traversal and a 30-second CLI deadline. Exhaustion or timeout does not
mean the transition never existed. The command does not retrieve old releases or
mint deployment credentials.

Registry schema v8 requires structurally valid audit heads and records. Other
schemas require explicit administrative recovery. The audit chain preserves
operational transition history, but it cannot recover desired values, authorize
replay, prove workload health, or satisfy the deployment review contract alone.
