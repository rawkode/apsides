# Deployment review proposal

Status: **partially implemented**. Deployment review v4 compares authenticated initial and upgrade declaration metadata using the protected registry v7 references. Live apply impact remains proposed. The current report does not satisfy all deployment-report requirements in [specification v0.2, section 25](SPEC-v0.2.md); live preview and controller diagnostics remain separate release work.

## Implemented declaration report

The `reakt.dev/deployment-review/v4` report uses an explicit empty baseline for initial installation and authenticated previous metadata for upgrades. It reports before/after, added, removed, changed pairs and retained entries for reactions, input authority, effects, dataflow, source locations, separate runtime/cleanup/deployment requirements, complete enforcement policy and compatibility. An authenticated empty category remains an upgrade baseline. Both policy ceilings and their accepted gaps/risks are compared, as are deployment scope, authorization and limitations. Arrays retain compiler-owned atomic field paths. Desired values, observed values and arbitrary compiler review documents are excluded.

Resource rows are keyed by Kubernetes identity and carry complete permitted before/after metadata, `beforeId`/`afterId`, prior tracked UID, and field-path differences. Renaming a declaration ID at the same identity is a declaration change; moving an ID to another identity is an old resource removal and a new addition. Effects and graph/source rows retain stable declaration IDs for correlation. Equal declaration metadata is `unchanged`; ownership-plan differences alone do not make it changed. Planned adoption remains linked to protected approval, and another added declaration is `create-if-absent`. None of these classifications proves a live operation completed. Live SSA impact is explicitly unavailable, and historical desired-value changes remain unknowable from value-free metadata.

Before fresh reservation, the CLI loads the prior review through an authenticated registry lookup at the exact UID/resourceVersion used for planning. A changed lookup revision rejects; reservation independently requires the same planning snapshot. It checks scope/allocation and the complete prior inventory correlation, then validates the complete report. A preallocation guard counts both snapshots and rejects more than 100,000 projection source elements. A separate exact byte limit rejects more than 8 MiB of serialized report before producing output. Repeated delta arrays and derived rows are covered by the byte limit, not by a promise of 100,000 output entries. Oversized reports fail as incomplete and are never truncated. Earlier upgrade identity preparation may precede preflight; claim publication and reservation do not.

The public report consumes the session's authenticated next/previous metadata and checks their exact protected references, canonical next plan, replayed record and predecessor writer checkpoint. The current top-level plan commitment remains permitted report metadata; resource bodies and private-value fingerprints do not. Deployment API compatibility covers the issued deployer's requirements, with invoking-identity bootstrap and token minting explicitly outside that catalog. No live preview permissions or API dry-run path are added by this declaration comparison.

## Two explicit comparison baselines

Each report combines two kinds of evidence:

1. **Declaration metadata changes:** compare the previous committed declaration metadata with the new verified release. This establishes resource membership, declared field ownership, reaction and dependency structure, input authority and sensitivity, permissions, lifecycle, source locations, and compatibility changes.
2. **Live apply impact:** compare an authenticated live object with the result of a conditional server-side apply dry-run for the proposed operation. This establishes the field changes that the API server previews for that identity, object version, request and admission context.

These baselines must remain separate in the machine-readable output. A live difference can represent drift correction rather than an author change. A removed declaration can release ownership without deleting a live field that another manager owns. Resource omission and planned deletion are distinct from completed deletion.

Historical desired-value changes are not reconstructed. For example, previous declarations of `replicas: 1` and `replicas: 2` can both have live `replicas: 2`; applying new `replicas: 2` produces the same preview. Value-free metadata and that preview cannot distinguish the historical author changes. The report must disclose this limitation without treating it as a missing live-impact comparison.

This interpretation satisfies the literal section 25 report categories without promising historical desired-to-desired value comparison. If that stronger comparison becomes a requirement, it needs a separate approved data-retention contract.

## Required report content

| Category | Proposed evidence and output |
| --- | --- |
| Managed resources and fields | Previous/new identities and declared field-path sets; live-impact field classifications; ownership releases separated from live deletion; explicit adoption or transfer intent linked to protected approval. |
| Reactions and dependencies | Added, removed and changed primitive/target/input edges, with previous/new source locations. Source-only changes are distinguishable from computation changes. |
| Input authority and sensitivity | Previous/new API identity, managed/observed mode, projection, sensitivity, missing-input policy and managed-resource association. No observed input values. |
| Runtime permissions | Previous active generation's requested capabilities versus the new generation's requested capabilities. |
| Deployment permissions | Previous deployment's required deployment capabilities versus this deployment's required capabilities, including any approved preview access. Generation-specific identity changes remain visible. |
| Cleanup permissions | Previous deployment's required cleanup capabilities versus this transition's required cleanup capabilities. This baseline is labelled explicitly; it is not the previous runtime permission set. |
| Enforcement and policy | Requested profile, enforcement gaps, indirect privileges, independent ceiling decisions and reasons. Required-access analysis does not claim effective RBAC or admission approval. |
| Lifecycle | Retained, omitted, orphaned, adopted, transferred and delete-planned identities, with prior approved policy and UID evidence. Unsupported transfer remains a rejection, never an invented successful outcome. |
| Compatibility and interruption | Previous/new schema and API requirements, runtime and transition compatibility requirements, relevant qualification assumptions and expected reconciliation pause. A duration estimate is not a completion guarantee. |

An initial installation has an explicit empty declaration baseline. Existing objects still require the normal adoption checks; they do not become managed merely because the declaration baseline is empty.

## Typed review metadata and storage

### Storage foundation and integration boundary

The selected storage design adds `ReviewStore` to `coordination`, with immutable typed `ReviewMetadataV1` bodies and `ReviewRef` references. The scope identifies the registry incarnation, cluster, control/workload namespaces, domain, allocation sequence and canonical transition. Generation and ServiceAccount names derive from that allocation. The reference digest is domain-separated over this metadata; artifact, plan and IR commitments stay outside the database body.

The v1 body owns its serialized types rather than embedding evolving compiler records. It retains resource identities and declared field paths, lifecycle/force and planned ownership metadata, complete input and reaction declarations with source locations, separate runtime/cleanup/deployment permissions, compatibility and runtime budgets, and both policy ceilings. Graph edges and source catalogs derive from those entries without duplicate storage. Unknown fields, malformed links, duplicate or unsorted entries and unsupported vocabulary are rejected. Limits are 256 resources, inputs and reactions each, 100,000 aggregate source elements and 8 MiB of canonical metadata. Validation counts serialized bytes before constructing the canonical value or buffer; exceeding a limit is an error, never truncation.

The libSQL implementation uses the existing durable primary connection. Review objects are immutable under the complete scope and metadata digest. Multiple unbound candidates can coexist for one transition: publication before a failed registry CAS must not prevent a later authorized candidate from using the same allocation sequence. Only a protected registry reference will select the authoritative candidate. Exact repeated publication is idempotent, and publication verifies the committed body through a fresh read. Corrupt exact-reference rows are errors and are never silently repaired.

Writable initialization adds `reakt_coordination_review_objects` transactionally, preserving the earlier `reakt_coordination_reviews` table and existing claim/audit tables. Reads select the exact reference from the new table first, then fall back to the exact legacy reference. A legacy row with another digest does not block publication. Existing table shapes are validated; malformed schemas are not repaired. Read-only access performs no DDL. With neither review table present, reads fail; with a valid table but no exact referenced row, reads return absence. Changing or deleting a stored lookup key consequently makes the original reference absent, which lifecycle resolution must treat as missing required evidence.

The trusted platform projector replays the canonical transition, derives permissions internally, consumes its complete effect catalog, and constructs the closed metadata types without copying desired values or compiler review JSON. CLI review preflight invokes it before fresh reservation. Real compiler/planner tests cover metadata completeness, normalization, adoption, forged catalogs, and invariance under private-value and provenance-hash changes. Reservation publishes and binds the validated projection as described below.

Registry v7 requires `StoredTransition.nextReviewMetadata` and an optional `previousReviewMetadata` reference, which is required for upgrades. The existing active allocation and retained committed transition identify the active review reference, so no second mutable active pointer is required. A pending successor must preserve the exact prior committed `next` reference as its `previous`. Verified sessions pin both references and their authenticated bodies, reread them at selected registry loads and pre-publication validation, and compare the next body against the canonical release projection. Snapshot publication and exact read-back finish before claim publication and reservation; commit promotes the allocation and transition in the existing registry CAS. Capacity admission includes both bounded references before any review publication. This establishes authenticated history availability; the CLI v4 renderer consumes those authenticated declarations for historical metadata deltas.

Registry v6 has no complete review baseline. The v7 cutover rejects that missing baseline explicitly; it cannot infer an empty history or reconstruct old reactions from live objects. The no-old-artifact acceptance scenario is an A-to-B upgrade where A was committed with protected review metadata. This contract covers active and pending comparisons, not indefinite lookup of every archived deployment report.

The bounded, versioned `ReviewMetadataV1` projection is derived by trusted code from a canonical transition plan; the calling release-resolution path remains responsible for artifact and provenance verification. It contains only the fields needed for the declaration comparisons above: resource identities and field-path catalogs, lifecycle/force settings, source locations, inputs, reactions, dependency edges, capability sets, enforcement metadata and compatibility requirements.

The proposal uses an immutable typed review snapshot with a protected reference. A report-specific store exposes only typed publication and lookup, not arbitrary JSON or bytes. Existing claim and inventory stores keep their current responsibilities. An implementation may choose a bounded inline protected checkpoint instead, but must first demonstrate that the complete accepted metadata fits the coordination budget; silent truncation is forbidden.

The binding protocol is:

1. Derive and validate the new review metadata before activation, including completeness and size. Publish it immutably and independently verify its read-back and metadata-only digest.
2. At reservation, require the previous active review checkpoint to match the predecessor allocation exactly. Freeze that exact reference on the selected transition. Bind the new review reference to the verified allocation as well.
3. Promote the new active review reference in the same registry UID/resourceVersion CAS that commits the transition and active allocation. Failure to prepare a required review snapshot must not first appear after activation.
4. During report resolution, authenticate the protected binding before reading a snapshot. Revalidate returned schema, scope, ordering, completeness constraints and digest even when a backend returns a typed value. Compare the new projection with the freshly verified release rather than trusting a caller's summary.

The database snapshot scope may identify registry incarnation, cluster, namespaces, domain and generation. Artifact, IR and plan commitments remain in the protected allocation/transition binding; they are not copied into the review database body. A review-reference digest commits only the allowed review metadata.

Specify byte, resource, field-path, graph-edge, source-path and capability limits before implementation. Exceeding a limit blocks a complete report; it cannot produce a successful truncated report. Any later snapshot garbage collection must retain active and pending predecessor references. This proposal does not implement garbage collection or imply bounded total history retention.

Older generations without an authenticated review checkpoint cannot supply missing graphs through live Kubernetes inspection. They require an explicit baseline/recovery procedure. An unavailable or corrupt snapshot is never an empty baseline, and a degraded report must not claim full section 25 compliance.

## Privacy and output boundaries

Do not persist desired values, resource bodies, source contents, observed input values, credentials, rollout keys, runtime rollout digests, per-value fingerprints or private-content hashes in the review metadata store. Do not reuse arbitrary compiler review JSON wholesale: existing documents contain whole-build commitments and must be projected into the new typed format.

Live objects and dry-run responses are sensitive transient inputs. Compare them in memory and emit only approved identity/path metadata and classifications. Do not serialize or log those bodies, attach them to errors, or hash them into durable preview evidence. API errors can echo rejected values; map them to bounded codes and safe resource/field identifiers.

Source paths, names, projections and field keys can themselves disclose information. They are permitted report metadata, not inherently public data. Normalize source paths to compiler-owned logical locations, bound strings and collections, and define the report's authorized audience. Field-path classification must handle arbitrary map keys conservatively; array selectors must not copy private element values into paths.

## Live SSA preview preconditions

The preview is a trusted, fixed operation derived from the resolved release, never a source callback or caller-supplied request body.

- Authenticate the selected transition, current trust and policy, target scope and expected claim or ownership-approval identity. A matching name or field manager does not authorize adoption.
- For an existing object, capture and check its expected UID and resourceVersion. Send conditional SSA with the stable domain field manager, the declared force behavior and strict field validation. Replaced objects and stale versions reject or require a bounded fresh preview.
- For an absent object, use the supported absent-only precondition contract. Never substitute an unconditional create/apply. Report generated/defaulted fields separately from declared ownership.
- Require `dryRun=All` on every request. Conflicts, unsupported dry-run behavior, unsafe admission, denied permissions and uncertain results block the preview. There is no real-write fallback or automatic force escalation.
- Preserve Kubernetes schema and list ownership semantics. A positional JSON-array diff is not evidence of Kubernetes field ownership. Distinguish changes to declared fields, ownership changes, and admission/defaulting changes.
- Bind the preview to target UID/RV, request identity, field manager, force mode, schema and policy context. Multiple objects do not constitute an atomic snapshot. Recheck transition authority around the operation and report the observation interval.
- Preview output never authorizes later execution. Execution retains its own current authority, ownership, conflict and admission checks.

Kubernetes dry-run follows admission, defaulting and merge processing without persisting the request, but generated values may differ from a later real operation. Dry-run and real requests require the same authorization. See [Kubernetes API dry-run semantics](https://kubernetes.io/docs/reference/using-api/api-concepts/#dry-run) and [server-side apply ownership](https://kubernetes.io/docs/reference/using-api/server-side-apply/).

### Permission and identity gate

Preview must use either an audited, restricted transport for the exact staged generation identity, or separately and explicitly approved CI capabilities. It must not silently expand deployment permissions, expose a general runtime credential to CI, broaden targets, or bypass the generation transport's request accounting and isolation contract.

The exact-generation transport is a proposed integration, not an existing preview capability. Its gate must show that the trusted preview path fixes dry-run mode and target requests and does not activate the controller or grant an unreviewed general request interface.

If approved CI capabilities are used, disclose that they permit real requests too; Kubernetes RBAC has no dry-run-only grant. Derive the required named access in the deployment permission report and obtain the independent policy approval before use. Admission may depend on identity, so a CI-identity preview cannot be presented as proof of the generation identity's eventual admission result. The report must either limit that claim explicitly or require an equivalent qualified context.

### Proposed issued-deployer integration

The selected implementation direction is a fixed static preview under the issued deployer, with explicit opt-in in a separately versioned deployment-identity policy. Existing identity documents must not opt in implicitly. Fresh deployment preview runs before reservation and accepts the verified release and exact planning snapshot, reusing reservation's trust, policy, ownership and inventory checks. It must not describe that snapshot as a selected transition session. Pending transitions use a session adapter to the same fixed preview operation and disclose their current phase; resumed observations are not historical pre-deployment evidence.

Report the exact named LIST and PATCH requirements for managed targets, plus CREATE permissions required for absent SSA by the qualified API contract. A CREATE grant can cover other names in that namespace/GVR; report exact intended creation targets and the admission limitation. No preview requests target observed inputs, production Secrets, rollout keys or generation credentials. Missing independent approval rejects; bootstrap credentials are never a fallback. This direction adds no permissions until that policy and request path are implemented.

Before returning a preview, reobserve target UID/resourceVersion or exact absence and revalidate the exact registry snapshot, trust, policy and identity approval. Reject races, incomplete reports and oversized responses. Reservation and execution retain their independent checks. A future typed live-impact report must label the issued-deployer admission context, observation interval and runtime-conditional targets, and must remain ephemeral. Real API and credential tests still gate claims about absent SSA, admission, field ownership and non-persistence.

## Shared request construction

The runtime's existing-object and absent-object apply paths now use the shared `platform::conditional_apply_request` builder. It preserves strict validation, the stable field manager and declared force, and installs an explicit UID/resourceVersion or absent-list resourceVersion precondition on a cloned desired object. Supplied concurrency metadata replaces any such metadata in the desired value. The builder performs no requests and establishes neither ownership nor absence; callers retain their authenticated claim checks, version qualification, dry-run interpretation and pre-write authority checks. The request type deliberately has no debug representation because it carries desired values.

This is a prerequisite for preview, not a preview endpoint. Preview must request dry-run parameters explicitly and must never call the runtime helpers that subsequently write. Its transport must also avoid the Kubernetes client's normal API-error decoding: that path can log response bodies before application-level sanitization. Use the client's raw response interface, bound successful bodies before deserialization, discard error bodies and expose only fixed error classifications. Do not print raw `FieldsV1` keys: list selectors can embed values, and live map keys are not automatically approved report metadata.

## Privacy boundary for live field classification

The low-level `platform::preview_fields` projection is implemented; the deployment report does not yet invoke it. Its caller must authenticate previous/next metadata and bind the objects to the authorized observation interval. It compares presence and values transiently only at those declaration paths and the fixed rollout annotation path. Its output classifies approved paths as added, removed, changed or unchanged. Arrays retain aggregate declaration paths; a changed array does not establish which Kubernetes list element changed ownership. The API server remains the merge authority. Do not infer schema-aware ownership from the compiler's aggregate array representation.

Fields outside that approved catalog may produce only a coarse server-difference classification. Ownership fieldsets may be compared transiently to disclose a coarse changed, unchanged or unknown ownership result, but their raw selectors, manager names, key names, values, hashes and cardinalities must not enter the report. Exact element-level ownership requires a separate schema-aware implementation and real API-server qualification. These limits must remain visible in the report rather than being described as complete field ownership analysis.

The implementation labels field-set inequality as `serializedFieldSetChanged`, not a semantic ownership transfer. Unsupported or malformed managed-field encodings yield `unknown`. An ignore trie masks approved subtrees when comparing remaining server fields; incompatible or missing partial ancestors yield `unknown`. Object-only pointer traversal preserves numeric map keys and rejects traversal into an array. Each object is capped at 4 MiB, depth 64 and 100,000 nodes before recursive operations. The union is capped at 100,000 paths and 1 MiB of path bytes, with an exact 8 MiB typed-output serialization limit. Oversized evidence rejects rather than truncates.

## Bounded transport mechanism

`platform::preview_transport` provides a low-level named LIST and conditional SSA dry-run mechanism for ConfigMaps, Services and Deployments. It does not authenticate a deployment, grant permissions, perform runtime composition, reobserve a complete preview interval, or publish a report. Its raw object accessors return sensitive data to trusted callers; omitting `Debug` does not make those objects safe to print or persist. The eventual adapter must use the approved issued-deployer client with retries and body-logging middleware disabled. Supplying custom middleware can invalidate that boundary.

Requests use the Kubernetes library's request builder and raw client transport. Every PATCH fixes `dryRun=All`; error bodies are discarded unread and failures use static text. Request JSON is structurally checked and capped at 1 MiB before serialization; successful response collection is capped at 4 MiB and one request/body read has a 15-second deadline. Strict decoding rejects duplicate keys. List responses must match the exact collection type, contain at most one matching object, have no continuation or nonzero remaining count, and carry bounded UID/resourceVersion evidence. List items may omit both type fields and inherit the exact list type; partial or conflicting item types reject. PATCH responses must supply the exact object type.

These mechanisms remain disconnected from `reakt deploy` until protected opt-in, the authenticated planning/session adapters, rollout composition, complete report budgeting and race revalidation are implemented. Read-only live list decoding does not qualify SSA non-persistence, absent-only creation, runtime admission or actual credential scope.

## Runtime-derived fields

A compiled base alone is not the effective desired state. Sending it under the stable field manager can incorrectly preview removal of a runtime-owned rollout annotation.

The implementation must explicitly partition static base fields, runtime contributions, delegated fields and server-managed fields. It may preview static changes only when the payload preserves unrelated current contributions using the same validated composition/ownership rules as runtime. It must not claim that this is an exact preview of unknown future reaction output.

Report declared additions, removals and changes to reaction-owned paths from the metadata comparison. Classify actual mutations dependent on external inputs as conditional. Do not read production Secrets or rollout keys merely to manufacture a deterministic preview, and do not publish an observed rollout digest. If a supported composition cannot be previewed without inventing a value or disturbing another writer's ownership, report the blocked preview and require an explicit contract decision before claiming completeness.

## Implementation gates and acceptance evidence

Before implementation, independently review the two comparison semantics, metadata schema/budgets, active-reference commit protocol, permitted metadata audience, transport identity, permission changes and dynamic-field composition. Unresolved authority or privacy findings pause the affected work. No permission expansion is authorized by this proposal.

Required tests include:

1. **Complete categories:** initial install and upgrade cover every report row above, including source-only changes, sensitivity changes, force changes, omission/orphan/delete and explicit adoption. Historical desired-value uncertainty is distinct from a known live preview result.
2. **History independence:** delete historical compiled transitions and make old OCI artifacts unavailable; the authenticated previous review snapshot still produces graph, permission, lifecycle and compatibility deltas.
3. **Commit and resume:** fail before snapshot publication, after publication, during registry CAS and after a lost response. The active allocation and review reference cannot disagree; a pending transition retains its exact predecessor reference.
4. **Corruption and bounds:** reject missing, foreign-scope, reordered, duplicate, oversized or substituted metadata and forged backend results. Reject a missing legacy baseline without treating it as initial installation.
5. **Privacy:** canaries in ConfigMap values, container arguments, annotations, admission responses and API errors never reach report output, logs or persisted snapshots. Inspect stored bytes for values and fingerprints. With scope and permitted metadata held fixed, changing only private values must not change the declaration-metadata snapshot.
6. **Actual SSA behavior:** real API-server tests cover defaults, nullable/removable fields, associative and atomic lists, shared field ownership, force/non-force conflicts, absent-only creation, UID replacement and RV races. Confirm dry-run leaves object data, ownership and resourceVersion unchanged in an otherwise isolated test.
7. **Actual credentials:** demonstrate permitted preview requests, denial outside the exact target scope, rejection of missing approved permissions and identity-dependent admission differences. Verify every request has `dryRun=All`, including retries and errors.
8. **Dynamic contributions:** unchanged rollout contributions survive static preview; added/removed reactions expose their field contract; changing sensitive inputs produces no value/digest leakage or false deterministic output claim.
9. **Authority changes:** trust, policy, registry incarnation or selected inputs changed between resolution and preview reject. Stale preview output cannot bypass execution-time validation.

Unit tests establish projection and classification behavior. Mock request tests establish request shape and error handling. Real API-server tests establish SSA, admission and credential behavior for the qualified versions. None alone proves later workload health, globally atomic application, or production performance.
