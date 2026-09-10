# Cleanup upgrade predecessor

This version adds one `Backup` named `cleanup-backup` to the parent CNPG
example. Its recorded deletion policy is `delete`. Upgrading to the parent
example removes that resource and the runtime's access to the Backup API.
The database, API workload, configuration, and reactions remain identical.

The Backup references `cleanup-unconfigured-database`, which must remain
absent during the rehearsal. It is a cleanup target, not a request to back up
the demo database. CNPG 1.30.0 accepted this resource in both owned-cluster
rehearsals at runner revision `7070c1e`.

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

Both separate live scenarios passed at runner revision `7070c1e`: preserving a
test-owned finalizer until its owner removed it, then completing cleanup; and
refusing a replacement UID while retaining the original protected claim.
Both preserved an unclaimed Backup and verified that the successor's workload
Role omitted Backup access. Each cluster and its credentials were deleted.
Their packaged receipts are accepted by the normal evidence merger for the
current successor. Full production release acceptance remains incomplete.

The local runner accepts this signed predecessor and the signed parent
successor, verified together under one external trust policy:

```sh
cuenv exec -- python3 tests/run_cnpg_cleanup.py \
  --initial-artifact "$CLEANUP_PREDECESSOR" \
  --successor-artifact "$SUCCESSOR_CANDIDATE" \
  --trust-policy "$JOINT_TRUST_POLICY" \
  --operator-manifest "$PINNED_OPERATOR_MANIFEST" \
  --operator-sha256 "$OPERATOR_MANIFEST_DIGEST" \
  --scenario finalizer \
  --output "$PWD/work/cnpg-finalizer"
```

Use absolute input paths, a clean committed checkout, and a fresh ignored output
directory. Run again with `--scenario replacement` and a separate fresh output
directory. Each run creates and deletes its own kind cluster through the
existing Docker context; existing clusters remain untouched.

The runner emits a non-release measurement only after cluster and credential
cleanup and repeated input verification. The reviewed archival adapter packages
that measurement into a production receipt; see the
[packaging contract](../../../docs/reference/PRODUCTION-EVIDENCE.md).
Offline runner checks use `cuenv task cnpgCleanupTest`.
