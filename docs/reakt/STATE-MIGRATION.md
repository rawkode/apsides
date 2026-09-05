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

The earlier v3 transition format established a POC compatibility boundary; the allocation cutover below supersedes it with v4. Legacy registry JSON and full-plan predecessors still deserialize for diagnosis, but pre-v3 transitions cannot begin, resume, replay or republish committed inventory. This includes checkpoint-bearing v2 records created by commit `f9d37fd`; they cannot serve as a verified predecessor for a v3 upgrade. Missing checkpoints, legacy predecessor payloads and prior plan schemas reject with explicit recovery errors. There is no automatic legacy recovery implemented; this build is not a seamless upgrade for any pre-v3 deployment.

The metadata checks introduced in v3, and retained in v4, check exact active allocation, scope, resource membership, deletion policy and claim generation/transition/UID correspondence. Unknown UIDs remain valid when both inventory and protected claims agree: cleanup can verify absence, but cannot delete a present object without a trusted UID.

Retirement namespaces are authenticated separately against the unique verified historical transition when reserving an upgrade or backfilling its writer checkpoint. The inventory digest does not grant retirement authority. Detailed CLI review still verifies the historical record separately before using its prior desired fields or capabilities.

The registry still stores each transition's new compiled plan and the predecessor inventory snapshot. Detailed review still reads historical protected records; registry growth remains unbounded across upgrades. Removing the duplicated predecessor artifact is progress toward compact coordination, not completion of that requirement. Registry compaction and live persistent-backend upgrade acceptance remain required.

## Writer retirement checkpoint

An upgrade reservation now atomically records its predecessor's allocation, runtime binding, isolated proxy epoch, controller/activation/inventory UIDs and exact credential retirement identities on the new transition. Quiescence, retirement and activation validate and consume that checkpoint without looking up the old transition. A checkpoint is authority input, never evidence that the writer has stopped: live proxy freeze, request drain, node/process termination, UID-bound retirement and registry CAS checks remain required.

Before the v4 cutover, existing v3 upgrades could fill an absent checkpoint from verified historical state using the same executor and a conditional registry write followed by reload. A present checkpoint is validated and never overwritten. That backfill did not change the v3 transition-plan format or allow recovery from an unverified historical record. New v4 records retain checkpoint backfill, while pre-v4 plans require explicit recovery.

The checkpoint excludes desired manifests, resource values, TLS material and staged content maps. It retains proxy Pod/configuration and policy digests that identify the approved isolated writer. Generated TLS Secrets now rely on protected UID, exact scope/type and Kubernetes immutability. The create response must match the locally generated Secret before its UID is recorded. Secret-content hashes are no longer produced or consulted; exact legacy generation-TLS digest entries are removed from loaded state so the next registry write discards them. Kubernetes guarantees that an immutable Secret's data cannot change without deletion and recreation ([immutable Secrets](https://kubernetes.io/docs/concepts/configuration/secret/#secret-immutable)).

## Bounded allocation cutover (qualification in progress)

The replacement allocation authority uses one namespace-registry counter, not a counter per domain. Each reservation consumes a sequence in the same conditional write as its claims and pending transition. Failed deployment sequences are never returned to the allocator. Domain deletion cannot reset the counter. Canonical generation, transition and ServiceAccount identities encode the complete registry incarnation and sequence, without a truncated hash or domain-name prefix.

The registry must bind its serialized state to the enclosing Kubernetes object's UID. Provisioning creates the object, reads its assigned UID, then initializes state with a resourceVersion-conditional update. An incomplete initialization cannot authorize deployment. Copying state into a recreated object must fail this binding check. Planning snapshots bind the UID, counter and resourceVersion together; an uncertain reservation response must resume its exact pending transition rather than allocate a replacement.

Only the final committed registry update promotes a domain's active allocation. During staging, activation and cleanup, the pending transition remains recovery authority and the previous committed allocation remains recorded. Historical transition removal is a separate migration: removing the allocation vector alone does not eliminate retained plans, resource claims or inventory snapshots.

A fresh registry UID does not establish that previous writers have stopped. A separately protected, stable scope anchor in `reakt-platform` binds the admitted registry UID. Deployment has exact GET access to this anchor and cannot create or update it. Missing or mismatched authority requires platform recovery, which must independently quiesce old writers before rebinding and must not race deployment. Same-UID restoration of an older etcd snapshot cannot be detected by this local counter alone; disaster recovery must establish a fresh incarnation and fence surviving writers. Ordinary deployment must never infer a counter from legacy history or interpret missing authority as an empty cluster.

The v4 implementation replaces allocation vectors with this counter and committed active allocation. Historical transitions and their full new plans remain stored, so overall registry compaction is still incomplete. Qualification results are recorded in STATUS.md; no live upgrade acceptance is implied here.
