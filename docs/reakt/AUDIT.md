# Transition audit history

Status: implemented, with local integration verification. This does not establish live upgrade or production-release qualification.

The protected namespace registry retains each domain's current committed transition and at most one unfinished transition. Before a successor replaces the committed record, deployment archives its predecessor through the `AuditStore` trait. The default libSQL/Turso backend shares the configured metadata database with claims and resource inventory, in separate tables.

Audit records contain registry scope, allocation names and sequence, executor identity, phase events, ownership actions, cleanup outcomes and retired resource identities/UIDs. They contain no manifests, source, desired values, credentials, blocked-reason strings, or artifact/IR/plan/evidence/content hashes. The audit digest commits only the allowed metadata. Audit records grant no ownership, deletion, deployment or resume authority.

Deployment writes the already-committed predecessor's immutable record and verifies an exact readback before the Kubernetes registry CAS removes that predecessor. The same CAS commits the successor, promotes its active writer checkpoint and advances the domain's protected audit head. Each audit links to the preceding reference, preserving addressable history without growing the registry's transition list. Other domains retain their records and heads.

An audit write followed by a lost CAS can leave an unselected row. That row truthfully describes a previously committed transition; it does not claim that the successor committed. Conflicting immutable rows, missing or corrupt prior heads and false backend readbacks block pruning. No archive garbage collection is implemented.

## Read transition status

Use standard Kubernetes authentication with permission to GET the namespace registry and its authority anchor. Use a read-only metadata credential for remote storage:

```sh
reakt audit \
  --domain team-api-production \
  --transition TRANSITION_ID \
  --cluster-binding production \
  --namespace production \
  --control-namespace reakt-system
```

`REAKT_INVENTORY_DATABASE` selects the existing absolute local database path or TLS libSQL endpoint. `REAKT_INVENTORY_TOKEN` supplies remote authentication separately. The audit command does not initialize or repair database tables. Local access opens SQLite read-only; remote access performs SELECTs and requires existing schema. Deployment owns initialization.

A current record returns `status: retained` and its recorded phase. An archived record returns `status: retired` and sanitized audit metadata, reached through the authenticated protected head chain. Both outputs have `authority: false`. They are observations, not permission to repeat old lifecycle actions. A retired transition cannot reactivate its identity; deploying an older build allocates a new generation.

Missing or corrupt referenced data is an error, not an empty history. Lookup has a traversal budget and a 30-second CLI deadline; exhaustion or timeout does not mean that a transition never existed. The command does not fetch historical releases or mint deployment credentials.

## Remaining limits

Registry schema v5 requires explicit audit heads and a configured audit backend. Pre-v5 registries require administrative recovery; there is no automatic conversion. Current committed records remain available for idempotent deployment retry and inventory publication until a later successor commits.

This bounds retained transition count per domain, not every state dimension. Pending event/evidence maps, per-resource activation approvals, reconstructed request limits, total domain count and database history still need their own budgets and qualification. Audit metadata also cannot recover historical desired values or satisfy the complete deployment change report by itself. See the [state migration contract](STATE-MIGRATION.md).

## Verification boundary

The coordination backend passes 18 tests, including local database reopen, immutable conflicts, corrupt rows, scope and size validation, and read-only access without schema creation. CLI unit tests cover retained/retired output and unknown-transition errors; a process test proves a missing local database is not created and a credential canary is not printed. These tests do not establish successful remote Turso access or a complete live CLI audit session.

Platform wire tests exercise the successor commit path, exact predecessor removal, preservation of an unrelated inactive domain, failed archive publication/readback, protected-chain lookup, malformed registry rejection retry after a rejected admission response or an HTTP 409 registry conflict, and missing/corrupt prior audit heads during a third-generation archival attempt. Their synthetic lifecycle evidence tests the persistence boundary, not live writer termination.
