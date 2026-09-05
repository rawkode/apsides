# State migration contract

Status: implementation in progress. Historical OCI retrieval is not an upgrade prerequisite.

CI supplies the new compiled controller and updates its Roles, RoleBindings and image. Previous resource identities, UIDs and deletion policies come from persistent metadata-only inventory. The executor must not need old manifests, source or compiled artifacts to prune or preserve omitted resources.

## Boundaries

- **New release:** supplied by CI and verified against protected platform policy. OCI retrieval is an optional distribution mechanism for this release.
- **Resource inventory:** the existing `InventoryStore` trait, default libSQL/Turso. Preserve only API identity, generation, UID and deletion policy. Apply the previous recorded policy when a resource is omitted. No resource values, source, credentials or private-content hashes belong here.
- **Protected coordination:** compact active generation, executor ownership, activation and old-writer retirement information. Preserve namespace-wide claim arbitration and stale-executor rejection. Database availability or a PV attachment does not prove writer termination.
- **Runtime activation:** independently protected current-generation binding. The runtime receives no CI database credentials or authority to upgrade itself.

## Replacement order

1. Make cleanup planning consume previous inventory and next resource identities directly, with protected claim and cleanup-ceiling checks. This boundary is now implemented as `plan_inventory_cleanup` and used by transition planning.
2. Replace the previous `GenerationPlan` dependency in deployment requests with the metadata needed for generation identity, resource ownership and writer retirement. Stop revalidating an old compiled IR merely to recover inventory.
3. Replace committed-transition searches and historical event replay with explicit protected predecessor checkpoints. Remove full previous and next compiled plans from durable registry serialization; CI can resupply the new release for an interrupted deploy.
4. Replace permanently growing allocation arrays and CLI allocation counting with durable uniqueness that does not depend on a growing Kubernetes object. Failed and retired writer identities must remain unusable.
5. Remove staged Secret-content hashes and unbounded claim lists from coordination objects. Inventory must remain metadata-only; do not copy the existing registry wholesale into libSQL.
6. Verify upgrades after both CI workspace loss and controller pod replacement. Missing, corrupt or inconsistent metadata must fail rather than silently become an empty inventory.

## Acceptance evidence

Exercise omitted-resource delete and orphan policy, retained resources, replacement UIDs, finalizer-delayed cleanup, concurrent deployers, interruption across each storage/Kubernetes boundary, idempotent recovery and revoked old writers. Prove that more than one MiB of resource tracking and repeated upgrades do not enlarge a Kubernetes object without bound. Scan persisted inventory for private-value canaries and their hashes. Verify local persistent libSQL and a TLS Turso primary separately.

The current executor still serializes full transition plans in ConfigMaps. This document does not claim that migration is complete. Independent architecture and final adversarial review remain unavailable due to the configured agents' account usage limit.
