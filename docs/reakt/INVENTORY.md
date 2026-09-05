# Resource inventory

Reakt's database boundary is `inventory::InventoryStore`. The default backend is libSQL: a persistent local file (for example on a PVC), or a remote Turso/libSQL primary. The trait exposes reading a head, reading immutable historical snapshots, and conditionally publishing a new head. SQL connections, transactions, and query syntax remain inside the backend. A future Parquet/S3 backend can implement this contract using immutable objects and conditional head writes.

The inventory contains cluster/controller scope, generation, resource group/plural/version/kind/namespace/name, observed UID, and deletion policy. It contains no resource manifests, desired or observed values, labels, annotations, source paths, arbitrary JSON, credentials, or hashes of private resource contents. The projection copies an explicit allowlist of fields; it never serializes an input resource. Resource names themselves are necessary inventory metadata and should not contain secrets.

During an upgrade, retain the previous inventory until reconciliation and cleanup complete. Compare resource identities to find omissions. Apply the previous resource's deletion policy: orphan releases tracking without changing the live resource; delete requires live UID checks and the existing lifecycle authorization. Missing UIDs do not authorize deleting whatever currently has the same name. Pending deletions retain their inventory until finalizers finish. A restarted pod must reload the existing head; an unavailable or corrupt store must not be interpreted as an empty inventory.

Snapshots are immutable and published with an opaque expected head revision. A stale publisher receives a conflict. Retrying the exact current snapshot is idempotent, including after a lost commit response. Snapshot digests cover only inventory metadata. Backend implementations must preserve these semantics; eventual-consistency reads alone are insufficient. History is stored in separate snapshots, rather than one continually growing Kubernetes object.

The local backend uses libSQL's rollback journal and FULL synchronization. Persistent volume attachment, filesystem durability, backup, and single-writer lifecycle fencing remain deployment concerns. A database lock or PVC access mode does not prove that an old pod has stopped making Kubernetes requests. Remote credentials are connection configuration, never inventory records.

The initial backend and contract tests are implemented separately from the current lifecycle registry. Full runtime/registry migration, completed-history retention, and PVC upgrade acceptance remain work in progress. The previous ConfigMap containing complete plans and history is rejected as the production storage design; its existing lifecycle tests do not validate this replacement.

The deployment planner now projects verified generations and matching protected claims through `generation_inventory`. Cleanup items use that metadata projection and include API version and kind, so the cleanup executor does not look up old resource manifests. Transition plans use schema v2; a v1 plan or altered cleanup API identity fails canonical-plan verification. This is an integration step, not a claim that the current registry no longer stores full transition requests. CLI loading and publication are now implemented as described below. Compacting the registry and exercising live restart/upgrade against persistent storage remain outstanding.

`PlatformClient::publish_committed_inventory` now publishes through the trait. It verifies the recorded transition by replay, requires the latest committed generation and complete active claims, reads the current inventory head, rechecks the Kubernetes registry, then conditionally publishes. It checks the registry again before reporting success. A registry change after database commit returns an error; retry can discover the committed snapshot idempotently. The two systems do not share a transaction, and the inventory remains tracking evidence rather than mutation authority.

An upgrade with missing prior inventory requires explicit recovery. Publication rejects unrelated/newer heads and refuses to drop or change an already recorded UID within the same generation. Live UID capture remains an independent step; this API never promotes a runtime-reported UID into a protected claim. The deployment CLI connection and invocation are described below.

## Deployment connection

`reakt deploy` now requires `REAKT_INVENTORY_DATABASE`. Use an absolute database path on persistent storage, or a TLS `libsql://` / `https://` primary endpoint. The default backend remains libSQL. Remote connections require `REAKT_INVENTORY_TOKEN`, supplied through the trusted job environment; do not put credentials in the endpoint. Endpoint credentials, query strings, fragments and non-root paths are rejected. Local connections reject remote tokens. Connection settings and tokens are never included in deployment reports or inventory snapshots.

For GitHub Actions, configure the primary endpoint as a repository/environment variable and the token as a secret; the deployment command remains `reakt deploy`. An ephemeral CI filesystem is not durable inventory storage. For a local database in a deployment pod, mount a persistent volume and configure its absolute database path. This change does not provision that volume or a database server.

Before reserving an upgrade, the CLI requires the inventory head to identify the protected active generation. It captures active UIDs through the platform and republishes that predecessor before starting cleanup. Resuming an unfinished transition requires its recorded predecessor; unavailable, corrupt, absent or unrelated history is not treated as an empty inventory. The head stays on the predecessor while cleanup is pending. After the Kubernetes transition commits, the CLI conditionally publishes the new inventory before reporting success, including its revision and snapshot identity in the report. A committed retry republishes idempotently, using the recorded transition executor identity; it does not take over an unfinished transition. If publication fails or times out after commit, retry the same transition. Workload activation is not rolled back by a tracking failure.

The CLI now invokes the publication path above. Compact registry storage, recovery migration from older installations without inventory, and live PVC/Turso upgrade qualification remain outstanding.

For a configured GitHub Actions deployment job:

```yaml
- name: Deploy
  env:
    REAKT_INVENTORY_DATABASE: ${{ vars.REAKT_INVENTORY_DATABASE }}
    REAKT_INVENTORY_TOKEN: ${{ secrets.REAKT_INVENTORY_TOKEN }}
  run: reakt deploy
```

The job still needs the approved release artifact, Kubernetes authentication and protected platform policy. The database settings are independent of the Kubernetes provider. A tokenless managed controller does not receive the database token: deployment/cleanup owns publication.

Upgrade preflight also checks the complete persisted snapshot against the approved predecessor and the protected claim snapshot used for planning. Matching generation IDs are insufficient: missing or extra resources, different API identities/types, changed deletion policies and mismatched UIDs reject reservation or resume. The publication path repeats this content check when advancing from the predecessor. Within a committed generation, publication may only complete unknown UIDs from protected claims; it rejects metadata changes, missing records and changes to known UIDs instead of silently replacing the stored history. The database remains tracking evidence and cannot authorize a larger cleanup scope.

The source-backed [state migration contract](STATE-MIGRATION.md) separates the CI-supplied new release, prune inventory and protected coordination. It records the current full-plan, Secret-hash and allocation-history dependencies that must be replaced together; copying the current registry into this database is explicitly excluded.

Active UID capture now accepts the persisted inventory snapshot directly. It validates its scope and active generation, rejects missing protected resources, and uses only API identity/version/kind to read live objects. Known inventory UIDs must match protected claims; live UID completion still requires exact approved Apply ownership and refuses replacements or deleting objects. The CLI loads the inventory before this capture. It does not read old desired manifests to enumerate capture targets. Subsequent publication and the larger transition request still retain old-plan dependencies; this is not yet the complete registry migration.

After capture, `publish_captured_inventory` now conditionally fills unknown UIDs in the existing snapshot instead of rebuilding it from an old compiled plan. It reads the protected registry afresh, checks the capture revision and active generation, requires the database head to equal the snapshot used for capture, and preserves every existing metadata field. Claims supplied inside a caller's registry copy are not trusted. Known UIDs cannot change. Registry rechecks bracket the database CAS; a change after publication returns an error and requires retry. The new-generation commit publication path and the wider transition model remain separate migration work.
