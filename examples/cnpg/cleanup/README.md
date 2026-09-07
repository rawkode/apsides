# Cleanup upgrade predecessor

This version adds one `Backup` named `cleanup-backup` to the parent CNPG
example. Its recorded deletion policy is `delete`. Upgrading to the parent
example removes that resource and the runtime's access to the Backup API.
The database, API workload, configuration, and reactions remain identical.

The Backup references `cleanup-unconfigured-database`, which must remain
absent during the rehearsal. It is a cleanup target, not a request to back up
the demo database. Acceptance by the operator's admission webhook still needs
live verification.

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

The signed-pair qualification uses `last-type-cleanup` for the exact
`postgresql.cnpg.io/backups/default/cleanup-backup` identity. Both generations
use one stable policy: the predecessor runtime ceiling and a cleanup ceiling
that additionally authorizes named `get` and `delete` for this Backup. The
successor runtime Role still derives from its own compiled capabilities.

Two independent live scenarios remain required: preserving a test-owned
finalizer until its owner removes it, then completing cleanup; and refusing a
replacement UID while retaining the original protected claim. Compilation and
driver checks alone do not satisfy either live release check.
