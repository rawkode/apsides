# Cleanup upgrade predecessor

This version adds one `Backup` named `cleanup-backup` to the parent CNPG
example. Its recorded deletion policy is `delete`. Upgrading to the parent
example removes that resource and the runtime's access to the Backup API.
The database, API workload, configuration, and reactions remain identical.

The Backup references `cleanup-unconfigured-database`, which must remain
absent during the rehearsal. It is a cleanup target, not a request to back up
the demo database. CNPG 1.30.0 accepted this resource in both owned-cluster
rehearsals at runner revision `64f2b39`.

From the repository root:

```sh
cuenv task build
cuenv task cnpgCleanupTypes
target/debug/aps lock \
  --source examples/cnpg/cleanup/program.tsx \
  --modules examples/cnpg/cleanup/.aps/crds/cnpg.ts,examples/cnpg/cleanup/.aps/crds/backup.ts
target/debug/aps compile \
  --source examples/cnpg/cleanup/program.tsx \
  --output work/cnpg-cleanup-compiled
```

Use a fresh compile output directory. Generated CRD modules stay under ignored
`.aps/`; their pinned generator task and source lock are tracked.

The acceptance suite deploys this program and then the parent example with
`aps deploy`, under one policy for both generations: this program's runtime
ceiling, and a cleanup ceiling that also allows reading and deleting exactly
`postgresql.cnpg.io/backups/default/cleanup-backup`. The successor's runtime
Role still derives from its own compiled capabilities. One scenario holds the
deletion with a test-owned finalizer until its owner removes it, then commits.
The other replaces the Backup's UID and requires cleanup to refuse it. Both
check that an unclaimed Backup survives and that the successor's workload Role
grants no Backup access. See [the acceptance suite](/reference/acceptance/):

```sh
cuenv task acceptancePrepare
cuenv task acceptanceCleanupTest
```
