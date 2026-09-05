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
2. Replace the previous `GenerationPlan` dependency in deployment requests with the metadata needed for generation identity, resource ownership and writer retirement. New v3 requests now carry `PredecessorGeneration`: allocation, scope, control namespace, inventory and retirement RoleBinding namespaces. Planning and committed inventory publication consume this metadata directly.
3. Replace committed-transition searches and historical event replay with explicit protected predecessor checkpoints. Remove full previous and next compiled plans from durable registry serialization; CI can resupply the new release for an interrupted deploy.
4. Replace permanently growing allocation arrays and CLI allocation counting with durable uniqueness that does not depend on a growing Kubernetes object. Failed and retired writer identities must remain unusable.
5. Remove staged Secret-content hashes and unbounded claim lists from coordination objects. Inventory must remain metadata-only; do not copy the existing registry wholesale into libSQL.
6. Verify upgrades after both CI workspace loss and controller pod replacement. Missing, corrupt or inconsistent metadata must fail rather than silently become an empty inventory.

## Acceptance evidence

Exercise omitted-resource delete and orphan policy, retained resources, replacement UIDs, finalizer-delayed cleanup, concurrent deployers, interruption across each storage/Kubernetes boundary, idempotent recovery and revoked old writers. Prove that more than one MiB of resource tracking and repeated upgrades do not enlarge a Kubernetes object without bound. Scan persisted inventory for private-value canaries and their hashes. Verify local persistent libSQL and a TLS Turso primary separately.

The current executor still serializes full transition plans in ConfigMaps. This document does not claim that migration is complete.

## Protected inventory checkpoint

Each new generation allocation commits to a versioned digest of the approved inventory metadata and its cluster, namespace and controller scope. The digest covers resource membership, API identity, kind, generation and deletion policy. It excludes UIDs, which are captured later against protected ownership claims, and never includes desired values or source. Completing UIDs therefore preserves the allocation commitment.

Planning, live UID capture and inventory publication verify this checkpoint. Captured UID publication also binds claims to the allocation's transition. Altering persisted delete/orphan policy cannot authorize cleanup.

The v3 transition format is a POC compatibility boundary. Legacy registry JSON and full-plan predecessors still deserialize for diagnosis, but pre-v3 transitions cannot begin, resume, replay or republish committed inventory. This includes checkpoint-bearing v2 records created by commit `f9d37fd`; they cannot serve as a verified predecessor for a v3 upgrade. Missing checkpoints, legacy predecessor payloads and prior plan schemas reject with explicit recovery errors. There is no automatic legacy recovery implemented; this build is not a seamless upgrade for any pre-v3 deployment.

For v3 requests, the planner checks exact active allocation, scope, resource membership, deletion policy and claim generation/transition/UID correspondence. Unknown UIDs remain valid when both inventory and protected claims agree: cleanup can verify absence, but cannot delete a present object without a trusted UID.

Retirement namespaces are authenticated separately against the unique verified historical transition before reservation and writer retirement. The inventory digest does not grant retirement authority. Detailed CLI review likewise verifies the historical record separately before using its prior desired fields or capabilities.

The registry still stores each transition's new compiled plan and the predecessor inventory snapshot. Historical writer shutdown and detailed review still read those protected records; registry growth remains unbounded across upgrades. Removing the duplicated predecessor artifact is progress toward compact coordination, not completion of that requirement. Registry compaction and live persistent-backend upgrade acceptance remain required.
