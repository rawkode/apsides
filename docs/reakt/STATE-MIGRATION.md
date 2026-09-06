# State migration contract

Status: implementation in progress. Historical OCI retrieval is not an upgrade prerequisite. The current cutover below supersedes the historical implementation checkpoints later in this document.

## Current state contract (registry v7)

Registry schema `reakt.dev/namespace-registry/v7` retains the compact `TransitionInputs` introduced by registry v4 and requires protected audit heads, a typed capacity admission, and exact next/previous review references on retained transitions. Canonical in-memory plans remain `reakt.dev/transition-plan/v4`; these are separate schema versions. No full compiled plan is part of the new registry wire type.

Frozen inputs select the original pre-reservation claim snapshot, registry revision/counter/history, next allocation, policy digest and plan digest. Upgrades also select the exact predecessor inventory snapshot by scope and snapshot ID, its allocation and retirement namespaces. Frozen ownership metadata retains approval object identity/version and observed resource identity/version. Permitted apply payloads are reconstructed from the verified new release. The metadata database contains no desired values or their hashes; protected Kubernetes coordination still retains plan, artifact, IR and policy commitments.

Reservation independently verifies that claim and predecessor inventory snapshots are readable and exact before the registry CAS. Nonempty runtime observations are rejected because this format does not freeze them. Resume requires a `TransitionSession` created from a `VerifiedRelease` and the immutable stores. It reconstructs the request, requires exact canonical plan digest/allocation, and verifies record replay. Missing or corrupt snapshots, changed policy or a different release fail; none can become empty state.

The selected plan lives only in memory, outside `NamespaceRegistry`. Lifecycle reloads pin exact frozen inputs, capacity admission, the immutable predecessor writer checkpoint and review references, and recheck protected release trust and runtime policy. Read-only review resolves its own authenticated registry load; callers cannot pair a plan with arbitrary copied registry state. It grants no mutation authority. Other compact records receive structural validation without historical release retrieval. Full event replay applies to the selected release-bound transition.

Active UID capture no longer searches historical transitions. Final commit publishes the active writer checkpoint, and upgrade reservation freezes it on the new transition. Missing checkpoints require explicit recovery; the new registry schema does not backfill from old plans. Pre-v7 registries require administrative recovery, with no automatic migration implemented.

Review metadata is published and independently read back before claim publication and reservation CAS. Its content-addressed store permits multiple unbound candidates under an unconsumed allocation. The protected transition selects the exact next reference and, for upgrades, the unique active committed predecessor's reference. Both enter capacity accounting before publication. Commit preserves the predecessor reference on the successor while pruning the predecessor record in the existing allocation CAS. Generic registry loads validate reference scope and relationships without needing review-store credentials; selected deploy sessions additionally read and validate both bodies and compare the next body against the freshly reconstructed release. Missing v6 review history requires explicit recovery and cannot be inferred as empty.

Review v4 compares authenticated prior and next declaration structure, lifecycle, permissions, policy and compatibility; historical desired values remain unavailable. Complete specification section 25 reporting remains unresolved. The registry now retains one committed and at most one unfinished transition per domain. Pending histories are now limited to forty receipts and eight resumes, with typed evidence profiles and reserved canonical completion bytes. Activation and inventory have separate admitted ceilings. The reconstructed-request size limit remains independent. See [state capacity admission](STATE-CAPACITY.md) for checks and verification limits.

The preceding registry v4 cutover passed 116 tests using `cuenv task registryTest`, with two live tests skipped (`work/compact-registry-final-tests.log`), and `cuenv task registryCheck` passed strict all-target Clippy (`work/compact-registry-final-clippy.log`). Independent production, CLI, harness and test-diff review found no remaining blockers. Tests cover exact frozen-input substitution, meaningful trust-revocation rejection before writes, actual registry-wire private canaries, legacy schema rejection, local database reopen and the retained lifecycle cases. Formatting, whitespace and lifecycle harness syntax checks passed. The later historical sections record preceding checkpoints. No live Pod upgrade, persistent-volume attachment, Turso-primary behavior or greater-than-one-MiB end-to-end acceptance is claimed for this cutover. Snapshot garbage collection must preserve every reachable reference, including original planning snapshots needed for resume; garbage collection is not implemented.

Before a successor commit removes the prior committed record, its allowlisted lifecycle metadata is archived through `AuditStore` and independently read-verified. The registry CAS promotes the successor, removes only that predecessor and advances a protected per-domain hash chain. Missing or corrupt audit state blocks pruning. [Transition audit history](AUDIT.md) describes the read-only lookup command, privacy boundary and remaining limits. Combined v5 verification passed 129 tests with two live tests skipped (`work/audit-registry-final-tests.log`), strict all-target Clippy, and 18 coordination tests. Independent source and test review passed. This establishes local persistence and wire behavior, not live upgrade qualification.

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

## Superseded implementation checkpoints

The following sections describe intermediate commits before the compact registry v4 cutover above. References to retained full plans and checkpoint backfill are historical behavior, not supported recovery paths in the current schema. Plan v4 and registry v4 are distinct version numbers.

At the first migration checkpoint, the executor still serialized full transition plans in ConfigMaps. The following checks did not establish completion of that migration.

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

## Active writer metadata

The registry now preserves one active writer checkpoint per domain. Final commit publishes that checkpoint in the same conditional update as the committed record and active allocation. Predecessor preparation combines it with persistent resource inventory without an old compiled plan. Existing v4 state may backfill a missing checkpoint only from authenticated committed history; a present checkpoint is never replaced by backfill. No historical transition deletion is part of this step.

Review schema v2 distinguishes known resource membership and deletion policy from unavailable old field structure, source locations, reactions, inputs, permissions and compatibility. The reviewed transition's immutable predecessor checkpoint authenticates this metadata even after a later generation becomes active. A present but invalid historical record still rejects. This fallback does not establish the complete structural change report required by specification section 25; that qualification remains open wherever historical detail is unavailable.

Active-checkpoint backfill requires undisturbed committed state and refuses an unfinished transition in the domain. It verifies the source, writes with UID/resourceVersion preconditions and reloads the captured revision. A present checkpoint is scope-validated and returned unchanged. Existing v4 pending transitions continue to use their transition-local predecessor checkpoint; no new authority is inferred from runtime observations. Unqualified new writers now reject before activation or old-writer shutdown mutations, so they cannot activate and then fail to produce a committed checkpoint.

## Immutable claim storage boundary

The claim-store prerequisite uses a separate typed `ClaimStore` trait with immutable `put_claims` and `read_claims` operations. A snapshot contains only its registry incarnation, cluster/control/workload scope and sorted resource-claim identities, states and UIDs. It has no mutable database head and no API for storing arbitrary manifests, source or desired values. The default implementation uses libSQL/Turso. A database row by itself grants no Kubernetes authority.

The intended integration writes and verifies a candidate metadata snapshot before conditionally changing the protected registry's snapshot reference. The registry CAS remains the only authority switch. Losing that CAS leaves an unreferenced snapshot; an uncertain response requires reloading the registry, not inferring success from the database. A selected reference with missing or corrupted content must fail. Garbage collection is a later operation and must not remove reachable snapshots.

This library does not yet remove the planner's 1 MiB request limit, the embedded claim/inventory vectors, the runtime activation object's per-resource approvals, or the full plans and resource-sized evidence maps in stored transitions. Those consumers require verified hydration from immutable references and a qualified runtime read path before greater-than-one-MiB production acceptance can pass. The planner must not acquire a reverse dependency on `coordination`; hydration belongs above `deploy`, with verified typed planning evidence passed into it. Library tests for snapshots larger than one MiB are storage evidence only.

The `coordination` crate now implements this boundary. Snapshots are bounded to 32 MiB and 100,000 claims; strict decoding checks canonical encoding, scope, lifecycle identities and the domain-separated digest. The local backend uses durable libSQL transactions and verifies the committed row after insertion. Remote configuration accepts TLS primary endpoints with authentication supplied separately; remote durability and availability remain provider requirements, not established by local tests.

`cuenv task coordinationTest` passed nine tests, including a snapshot larger than one MiB surviving a local database reopen, competing identical inserts, malformed/corrupt rows, scope mismatch, private diagnostic canaries and bounds. `cuenv task coordinationCheck` passed strict all-target Clippy and a build without the default backend. Independent source review found no blocking issues. These checks do not establish live PV attachment, Turso connectivity, registry integration or end-to-end upgrade behavior.

## Registry snapshot integration (automated checks passed; live qualification pending)

Registry schema v3 replaces the top-level claim vector with a required `claimSnapshot` reference. The hydrated registry is a separate, non-serializable type. Readers authenticate the Kubernetes UID, scope and independent authority anchor before fetching metadata, then independently verify the returned snapshot against the selected reference. A missing database row cannot mean an empty registry.

Every registry mutation must publish and read-verify its candidate snapshot before the existing UID/resourceVersion-conditional Kubernetes update selects it. This applies even when only transition evidence changes. A database write failure prevents that update; a lost Kubernetes response requires reload. Unselected snapshots left by competing or interrupted deployers grant no authority.

Fresh bootstrap requires an empty ConfigMap with its actual UID/resourceVersion and an independently provisioned matching authority anchor. It publishes an empty scoped snapshot before initializing the registry. Existing v2 registries require explicit administrative migration; ordinary load cannot silently upgrade them or synthesize an empty snapshot. The first integration does not implement that migration.

The CLI uses `REAKT_INVENTORY_DATABASE` and `REAKT_INVENTORY_TOKEN` for both typed stores, with separate database tables. Runtime credentials and activation readers are unchanged by this integration. Stored transition plans still embed claim inputs, so moving the top-level vector alone does not complete bounded-state acceptance.

`cuenv task registryTest` passed 112 tests with two explicitly skipped live-cluster tests. Coverage includes normal reservation, stale revisions, backend-provided corrupt snapshots, false publication references, admission-mutated responses, empty-registry bootstrap, existing lifecycle regressions and authenticated loads after real local libSQL reopen. `cuenv task registryCheck` passed strict all-target Clippy for deploy, platform and CLI with test support. Formatting and lifecycle harness parsing passed. Independent production, harness and test-diff reviews found no remaining source blockers. These checks do not establish live pod upgrade, persistent-volume attachment or Turso connectivity.
