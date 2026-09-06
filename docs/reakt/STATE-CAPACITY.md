# Proposed state capacity contract

Status: proposed and unimplemented. Registry v5 bounds retained transition count;
it does not yet reserve space for a pending transition's future evidence. This
document proposes the next implementation, not a claim that v5 provides it.

## Requirement and current boundary

[SPEC-v0.2 §18](SPEC-v0.2.md) requires explicit control-object limits, actionable
rejection of oversized domains, no silent truncation, and recovery from persisted
authority. Section 19 requires repeatable phases and enough evidence to resume
after stopping writers. Neither requires unlimited domains or resources in one
Kubernetes object. Rejecting an oversized domain is acceptable; discovering that
its required evidence cannot fit after irreversible work is the gap to close.

V5 keeps one active committed transition and at most one pending transition per
domain. Older committed audit records and claim snapshots use immutable metadata
storage. There is no longer a permanent vector of full historical transitions in
the registry, but several current-state limits remain independent:

| Current source | Remaining limit or growth |
| --- | --- |
| `crates/platform/src/lib.rs`, `registry.rs`, `store.rs` | `state.json` is limited to 512 KiB. Publication checks actual bytes, without reserving future growth. |
| `crates/platform/src/model.rs` | Current and pending records retain receipts, staged UID/digest maps, drain evidence, ownership metadata, and predecessor checkpoints. |
| `crates/platform/src/model.rs`, `controller.rs`, `upgrade.rs` | Activation and inventory are separate control objects. Activation repeats every approved claim, and an upgrade replaces it after quiescence. Their capacity is not part of the registry byte count. |
| `crates/platform/src/writer_checkpoint.rs` | Each checkpoint permits 128 KiB; the same predecessor authority occurs in more than one protected location. |
| `crates/platform/src/registry.rs` | Bootstrap permits up to 4096 domains by count; their aggregate byte cost is checked only against the current serialization. |
| `crates/deploy/src/reducer.rs`, `ownership.rs` | A record and a frozen ownership vector independently permit 1 MiB, already larger than the whole registry budget. Receipts permit 256 events. |
| `crates/deploy/src/planner.rs` | The complete in-memory request is limited to 1 MiB and 4096 registry claims. External claim storage does not remove these planner limits. |

The 16 MiB proxy ledger is **not** copied into registry state. `proxy.rs` produces
a small `ProxyDrainObservation` containing identity, counters, and commitments.
`UpgradeDrainState` now uses strict typed termination and proxy observations, but
their strings, container cardinality, and total encoded size do not yet form a
closed capacity profile.

Current late-failure examples are concrete: `staging.rs` creates an identity before
persisting its UID; `quiescence.rs` freezes admission and stops writers before
persisting termination observations; `upgrade.rs` obtains cleanup receipts before
publishing Committed. The current size check safely refuses the oversized write,
but refusal can leave the transition requiring recovery.

## Proposed implementation: reserve completion capacity

Add `crates/platform/src/state_capacity.rs` with a versioned, typed
`TransitionCapacity` included in immutable `TransitionInputs`. It records the
admitted byte envelope and bounded evidence profile, not padding, manifests, or
new execution authority. Selected release reconstruction must recompute and
validate the envelope from the approved plan, predecessor checkpoint, and exact
frozen metadata. Session selection pins it with the other immutable inputs.

The registry envelope is computed from a canonical, non-overlapping
`DomainStateProjection`. A projection owns exactly one domain's history entry,
active checkpoint entry, audit-head entry, and retained active and pending
transition entries, including their serialized map keys and a conservative entry
delimiter for each top-level map. A canonical fixed skeleton owns every other
field and every top-level map delimiter. No byte belongs to both parts. The
projection ceiling is the largest serialized projection reachable at any phase;
its current size is recomputed from the actual protected state.

The projection must include the successor checkpoint and head at commit, not
assume that pruning the predecessor pays for every addition. Known ownership
metadata, cleanup identities, namespaces, and prior evidence use their actual
canonical sizes. Unknown generated IDs, versions, container IDs, and event fields
use explicit validated encoded-byte bounds. The capacity record's own schema,
encoded numbers, keys, and delimiters are part of the projection; fixed-width
encodings or a demonstrated fixed point avoid a self-sizing ambiguity.

Before reservation consumes an allocation or publishes claims, require:

`canonical fixed skeleton bytes + sum(max(actual domain projection bytes, admitted pending projection ceiling)) <= 512 KiB`.

Domains without a pending transition contribute only their actual projection.
This partition prevents current evidence from being counted once as actual state
and again as unused allowance. A test-only reconstruction must confirm that the
accounting result is never smaller than the canonical bytes of the complete wire
registry for empty, single-entry, and multi-entry maps.

Persist capacity admission in the same registry CAS as reservation. A competing
domain cannot spend another pending domain's unused capacity: every subsequent
load and write checks this equation against a fresh registry as well as the
existing actual-size check. A CAS loser reloads and recomputes the whole equation.
Do not infer reservations from current free space on resume. At commit, release
only the difference between the old envelope and the new committed domain's
actual contribution, in the existing promotion/pruning CAS.

Bounded retries require a separate event-slot rule. Before starting a phase,
reserve enough slots and bytes for its remaining successful path plus a durable
Blocked result. Resume consumes an explicitly admitted recovery allowance and
must refuse before new side effects if completion would no longer fit. A finite
record cannot promise unlimited recovery epochs. Diagnostics must identify the
exhausted allowance and require explicit recovery; never truncate receipts or
discard evidence used by `deploy::verify_record`.

The initial supported evidence profile must be chosen from a demonstrated size
calculation, not from unrelated existing maxima. In particular, multiplying 64
Pods by all maximum-length identity fields can make a valid profile too large
for a 512 KiB registry. A smaller supported profile is an explicit platform
admission limit, not an excuse to omit writers. If that limit is unacceptable,
protected per-domain storage is a separate architecture change; this proposal
does not claim admission alone increases capacity.

Bounds are on canonical encoded bytes, not Rust string length. Escaped control
characters can cost six JSON bytes each. The current termination path also walks
regular, init, and ephemeral containers without a total container count or
container-ID length bound. Likewise, a bounded Blocked reason may expand when
escaped and every distinct Blocked/Resume cycle appends receipts. Existing raw
length and 256-receipt checks are safety ceilings, not an admitted profile.

## Evidence and side-effect gates

Use the strict typed, deserializable drain observations as the starting point for
the profile. Preserve every field currently consumed or committed by lifecycle
verification. Bound writer sets, containers per writer, every UID/resourceVersion
and container ID, map entries, and encoded strings; reject unknown fields and
unexpected staged identity keys. Validate live inputs before issuing dependent
mutations. Do not replace source evidence with a boolean, arbitrary blob, or
unverified digest merely to reduce bytes.

All stage, quiesce, activation, credential-retirement, and cleanup entry points
must validate their completion reservation before external writes. Before a
create whose resulting UID is unknown, budget the maximum admitted UID. Before
stopping a writer, capture and validate its complete current identity set and
ensure its termination evidence fits. A server returning identity metadata
outside the supported profile remains an explicit recovery condition; a finite
budget cannot cover arbitrary server output.

Registry capacity alone is not completion capacity. Admission must also compute
and persist separate ceilings for every control object that the transition will
create or replace, initially the activation and runtime-inventory ConfigMaps.
The activation ceiling includes the exact approved-claim set and both the
Inactive/Draining and Active representations. The inventory ceiling includes the
largest admitted observation and condition profile, using the existing limits of
one observation per approved claim, 128-byte observation UIDs, at most 256
conditions, and validated condition identifiers. Each object is checked against
its own 512 KiB state budget before reservation and again immediately before its
write. The existing `state_object` size check prevents an oversized
individual ConfigMap create, but it may run after earlier staging writes and does
not reserve the later upgrade representation. It is therefore a local guard, not
completion admission.

The initial capacity profile must enumerate all transition-owned control objects
and the phase at which each reaches its maximum. An unlisted object or an object
whose required profile cannot be derived before reservation makes the transition
unsupported. Approval desired values may be read transiently to verify existing
protected approvals, but the envelope stores only the already permitted frozen
metadata and byte counts. Diagnostics contain component identifiers and
required/available counts, never desired values or new content hashes.

Keep the existing exact UID/resourceVersion CAS, admission-response equality,
fresh trust/policy verification, selected-plan replay, and immutable-store
readback checks. Capacity is an additional refusal condition and never grants
permission. Preserve platform metadata on the ConfigMap; validate its permitted
outer-object envelope as well, instead of assuming the `state.json` limit covers
arbitrary annotations or admission-injected metadata. This proves the
platform-owned request envelope. An admission plugin can still reject or enlarge
a response; such a response is an explicit external recovery condition and is
not covered by the finite completion guarantee.

No additional payload moves to the metadata database in this slice. In
particular, staged content commitments, evidence commitments, approval desired
payloads, and private-content hashes do not become generic database blobs. The
existing typed claim and audit privacy contracts remain unchanged.

## Planner budget and compatibility

Name the planner's request limit as an in-memory planning budget, not a
Kubernetes control-state budget. Keep its byte and claim-count limits explicit in
CLI diagnostics. Raising them toward the claim store's 32 MiB / 100k bounds is
separate work requiring capped serialization, allocation/CPU budgets, and tests;
the store's capacity is not evidence that planning supports that scale.

Introduce a registry/inputs schema cutover for protected capacity reservations.
Old pending records must not acquire an invented allowance during ordinary load.
An explicit migration would need the verified selected release and all required
evidence to establish the bound. No such migration is included in this proposal.
Bootstrap must account for every declared domain's fixed contribution and reject
with required/available byte counts. Later domain admission follows the same CAS
accounting; domain count alone is not a capacity proof.

## Acceptance gates

- For every reachable phase, serialize a maximally populated fixture for the
  admitted profile and prove its actual canonical contribution is within the
  computed envelope. Cover escaping, maximum identifiers, UID/version lengths,
  ownership approvals, all cleanup outcomes, retirement maps, and checkpoint
  duplication. Changing a production DTO must make this proof fail or update it.
- Admit a boundary-sized upgrade and complete staging, drain, activation,
  cleanup, archival, and commit without a capacity failure. One byte beyond the
  boundary fails before claim publication, allocation CAS, or Kubernetes writes.
- Independently overflow the future activation and inventory objects while the
  registry remains below its limit. Reject before allocation and before any
  earlier staging object is created. Cover initial installation and an upgrade
  whose larger activation would otherwise fail after quiescence.
- Exercise an almost-full registry with another domain reserving concurrently.
  A CAS loser reloads and rechecks; unused completion space cannot be stolen.
- Exhaust recovery slots immediately before a phase. The old writer remains
  untouched and a permitted terminal/Blocked record still fits. Repeated
  idempotent events consume neither extra bytes nor extra allowance.
- Oversized ownership metadata, cleanup sets, writer histories, or bootstrap
  domain sets produce component-specific required/available limits. No test may
  pass merely because unrelated trust or fixture validation failed first.
- Inject oversized or unknown drain fields and staged identities; reject before
  the relevant external mutation. Test configured evidence bounds independently
  from whole-registry overflow, including raw proxy ledger versus its summary.
- Change the protected capacity after session binding, forge a smaller envelope,
  or load a pre-cutover pending record: reject before writes. Exact plan replay,
  checkpoint matching, source-evidence checks, and ownership reconstruction must
  remain effective under negative tests.
- Put private canaries in the new release and approval payloads. Verify registry
  capacity diagnostics and metadata-store rows contain neither values nor newly
  introduced private-content hashes.

Implementation touches platform `model.rs`, `transition_inputs.rs`,
`transition_session.rs`, `registry.rs`, `registry_model.rs`, `staging.rs`,
`quiescence.rs`, `drain_evidence.rs`, `termination.rs`, `proxy.rs`, `activation.rs`,
`upgrade.rs`, and `cleanup.rs`; deploy event-budget helpers belong in `reducer.rs`.
Tests belong in the internal platform suite, focused capacity unit tests, and
deploy reducer tests. A source review must audit every registry mutation and
preceding external side effect, not just the public reserve function.
