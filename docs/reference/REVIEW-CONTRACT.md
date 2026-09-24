# Deployment review contract

Status: **partially implemented**. Deployment review v5 combines authenticated initial and upgrade declaration metadata with explicitly approved live impact, using protected registry v8 references. Actual issued-credential SSA qualification remains outstanding. The report does not yet satisfy every requirement in [specification v0.2, section 25](SPEC-v0.2.md).

## Implemented declaration report

The `apsides.dev/deployment-review/v5` report uses an explicit empty baseline for initial installation and authenticated previous metadata for upgrades. It reports before/after, added, removed, changed pairs and retained entries for reactions, input authority, effects, dataflow, source locations, separate runtime/cleanup/deployment requirements, complete enforcement policy and compatibility. An authenticated empty category remains an upgrade baseline. Both policy ceilings and their accepted gaps/risks are compared, as are deployment scope, authorization and limitations. Arrays retain compiler-owned atomic field paths. Desired values, observed values and arbitrary compiler review documents are excluded.

Resource rows are keyed by Kubernetes identity and carry complete permitted before/after metadata, `beforeId`/`afterId`, prior tracked UID, and field-path differences. Renaming a declaration ID at the same identity is a declaration change; moving an ID to another identity is an old resource removal and a new addition. Effects and graph/source rows retain stable declaration IDs for correlation. Equal declaration metadata is `unchanged`; ownership-plan differences alone do not make it changed. Planned adoption remains linked to protected approval, and another added declaration is `create-if-absent`. None of these classifications proves a live operation completed. Live SSA impact is reported separately under `liveImpact`; historical desired-value changes remain unknowable from value-free metadata.

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
| Managed resources and fields | Previous/new identities and declared field-path sets; live-impact field classifications; ownership releases separated from live deletion; explicit adoption intent linked to protected approval. |
| Reactions and dependencies | Added, removed and changed primitive/target/input edges, with previous/new source locations. Source-only changes are distinguishable from computation changes. |
| Input authority and sensitivity | Previous/new API identity, managed/observed mode, projection, sensitivity, missing-input policy and managed-resource association. No observed input values. |
| Runtime permissions | Previous active generation's requested capabilities versus the new generation's requested capabilities. |
| Deployment permissions | Previous deployment's required deployment capabilities versus this deployment's required capabilities, including any approved preview access. Generation-specific identity changes remain visible. |
| Cleanup permissions | Previous deployment's required cleanup capabilities versus this transition's required cleanup capabilities. This baseline is labelled explicitly; it is not the previous runtime permission set. |
| Enforcement and policy | Requested profile, enforcement gaps, indirect privileges, independent ceiling decisions and reasons. Required-access analysis does not claim effective RBAC or admission approval. |
| Lifecycle | Retained, omitted, orphaned, adopted and delete-planned identities, with prior approved policy and UID evidence. Ownership transfer is not a supported action; the parser rejects it rather than reporting an invented successful outcome. |
| Compatibility and interruption | Previous/new schema and API requirements, runtime and transition compatibility requirements, relevant qualification assumptions and expected reconciliation pause. A duration estimate is not a completion guarantee. |

An initial installation has an explicit empty declaration baseline. Existing objects still require the normal adoption checks; they do not become managed merely because the declaration baseline is empty.

## Typed review metadata and storage

### Storage and protected binding

`coordination::ReviewStore` stores immutable typed `ReviewMetadataV1` bodies addressed by `ReviewRef`. The scope identifies the registry incarnation, cluster, control/workload namespaces, domain, allocation sequence and canonical transition. Generation and ServiceAccount names derive from that allocation. The reference digest is domain-separated over this metadata; artifact, plan and IR commitments stay outside the database body.

The v1 body owns its serialized types rather than embedding evolving compiler records. It retains resource identities and declared field paths, lifecycle/force and planned ownership metadata, complete input and reaction declarations with source locations, separate runtime/cleanup/deployment permissions, compatibility and runtime budgets, and both policy ceilings. Graph edges and source catalogs derive from those entries without duplicate storage. Unknown fields, malformed links, duplicate or unsorted entries and unsupported vocabulary are rejected. Limits are 256 resources, inputs and reactions each, 100,000 aggregate source elements and 8 MiB of canonical metadata. Validation counts serialized bytes before constructing the canonical value or buffer; exceeding a limit is an error, never truncation.

The SQLite implementation uses the store's existing durable connection. Review objects are immutable under the complete scope and metadata digest. Multiple unbound candidates can coexist for one transition: publication before a failed registry CAS must not prevent a later authorized candidate from using the same allocation sequence. Only a protected registry reference will select the authoritative candidate. Exact repeated publication is idempotent, and publication verifies the committed body through a fresh read. Corrupt exact-reference rows are errors and are never silently repaired.

Writable initialization creates and validates `apsides_coordination_review_objects` transactionally alongside the claim and audit tables. Existing table shapes are validated; malformed schemas are not repaired. Read-only access performs no DDL. A missing table or exact reference is a required-evidence failure.

The trusted platform projector replays the canonical transition, derives permissions internally, consumes its complete effect catalog, and constructs the closed metadata types without copying desired values or compiler review JSON. CLI review preflight invokes it before fresh reservation. Real compiler/planner tests cover metadata completeness, normalization, adoption, forged catalogs, and invariance under private-value and provenance-hash changes. Reservation publishes and binds the validated projection as described below.

Registry v8 requires `StoredTransition.nextReviewMetadata` and an optional `previousReviewMetadata` reference, which is required for upgrades. The active allocation and retained committed transition identify the active review reference, so no second mutable pointer is required. A pending successor preserves the exact prior committed `next` reference as its `previous`. Verified sessions pin both references and their authenticated bodies, reread them at selected registry loads and pre-publication validation, and compare the next body against the canonical release projection. Snapshot publication and exact read-back finish before claim publication and reservation; commit promotes the allocation and transition in the registry CAS. Capacity admission includes both bounded references before publication. This contract covers active and pending comparisons, not indefinite lookup of every archived deployment report.

The bounded, versioned `ReviewMetadataV1` projection is derived by trusted code from a canonical transition plan; the calling release-resolution path remains responsible for artifact and provenance verification. It contains only the fields needed for the declaration comparisons above: resource identities and field-path catalogs, lifecycle/force settings, source locations, inputs, reactions, dependency edges, capability sets, enforcement metadata and compatibility requirements.

A report-specific store exposes only typed publication and lookup, not arbitrary JSON or bytes. Existing claim and inventory stores keep their current responsibilities.

The binding protocol is:

1. Derive and validate the new review metadata before activation, including completeness and size. Publish it immutably and independently verify its read-back and metadata-only digest.
2. At reservation, require the previous active review checkpoint to match the predecessor allocation exactly. Freeze that exact reference on the selected transition. Bind the new review reference to the verified allocation as well.
3. Promote the new active review reference in the same registry UID/resourceVersion CAS that commits the transition and active allocation. Failure to prepare a required review snapshot must not first appear after activation.
4. During report resolution, authenticate the protected binding before reading a snapshot. Revalidate returned schema, scope, ordering, completeness constraints and digest even when a backend returns a typed value. Compare the new projection with the freshly verified release rather than trusting a caller's summary.

The database snapshot scope may identify registry incarnation, cluster, namespaces, domain and generation. Artifact, IR and plan commitments remain in the protected allocation/transition binding; they are not copied into the review database body. A review-reference digest commits only the allowed review metadata.

The byte, resource, field-path, graph-edge, source-path and capability limits are validated before publication. Exceeding a limit blocks the report; it cannot produce a successful truncated result. Snapshot garbage collection is not implemented and total review history is not bounded.

A deployment without an authenticated predecessor review checkpoint requires explicit recovery. Live Kubernetes inspection cannot recreate the missing graph. An unavailable or corrupt snapshot is never an empty baseline.

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

### Issued-deployer integration constraints

The implementation uses a fixed static preview under the issued deployer, with explicit opt-in in a separately versioned deployment-identity policy. Existing identity documents must not opt in implicitly. Fresh deployment preview runs before reservation and accepts the verified release and exact planning snapshot, reusing reservation's trust, policy, ownership and inventory checks. It must not describe that snapshot as a selected transition session. Pending transitions use a session adapter to the same fixed preview operation and disclose their current phase; resumed observations are not historical pre-deployment evidence.

Report the exact named LIST and PATCH requirements for managed targets, plus CREATE permissions required for absent SSA by the qualified API contract. A CREATE grant can cover other names in that namespace/GVR; report exact intended creation targets and the admission limitation. No preview requests target observed inputs, production Secrets, rollout keys or generation credentials. Missing independent approval rejects; bootstrap credentials are never a fallback. The implemented policy and request path require separately provisioned RBAC; the code does not grant these permissions.

Before returning a preview, reobserve target UID/resourceVersion or exact absence and revalidate the exact registry snapshot, trust, policy and identity approval. Reject races, incomplete reports and oversized responses. Reservation and execution retain their independent checks. The typed live-impact report labels the issued-deployer admission context, observation interval and runtime-conditional targets, and remains ephemeral. Real API and credential tests still gate claims about absent SSA, admission, field ownership and non-persistence.

### Implemented fresh-request adapter

`IssuedDeployment::connect` now mints the approved credential and privately constructs the stock client from the same verified TLS configuration. It replaces inherited authentication and impersonation settings, clears custom headers and disables retries. A caller cannot supply an arbitrary client or bearer to this wrapper. The CLI uses this factory for deployment authentication.

`IssuedDeployment::preview_fresh` accepts a verified release, a fresh planning request and the inventory store. It requires the exact protected identity approval, registry snapshot, current trust and policy, immutable predecessor inventory and review metadata, and ownership approvals. All managed targets must be covered before workload requests begin. It rechecks authority around target previews and reobserves every target before returning a bounded, ephemeral report. The report describes an observation interval, not an atomic snapshot or execution authorization. The whole operation has a 120-second deadline.

Preview opt-in uses `apsides.dev/deployment-identity/v2` with the exact profile `static-ssa-issued-deployer/v1`. Its target list is sorted, unique and restricted to named ConfigMaps, Services and Deployments in the workload namespace. Approved supersets are allowed. An empty list authorizes zero targets; it is never a wildcard. A v1 identity document is rejected outright; a v2 document without `preview` remains opted out of preview only. RBAC is independently provisioned: the profile itself grants no access, and Kubernetes has no dry-run-only grant.

Static composition preserves a current rollout contribution only with exact stable-manager Apply evidence. Missing contributions remain missing and future reaction output stays conditional. Removed final reactions omit the old contribution. Adoption uses the approved non-force payload. No observed input, Secret value or rollout key is read to invent future runtime state.

The CLI invokes this adapter when the protected identity explicitly approves preview and includes the captured result in v5 deployment review. The fresh adapter parses the observed server's `major`/`minor` discovery fields and rejects a server below the domain's declared `minKubernetesVersion` floor (`"1.34"` by default; authors may only raise it). The acceptance suite's collision scenario runs it live against the qualified v1.34.3 kind node with the issued deployer credential: under an admission hold that denies the reservation's registry write, `aps deploy` with preview approved for one absent `ConfigMap` leaves the registry and every control and workload namespace object unchanged and does not create the target, and without the hold the committed report carries the `available` fresh preview of exactly that target. It covers one absent ConfigMap target only. Existing targets, adoption, pending-transition preview and missing preview permissions are not exercised live.

### CLI capture and resume

The normal deploy command needs no preview flag. A protected identity with `preview` enables it; missing opt-in yields `deployment-identity-preview-not-approved` without workload preview requests. V5 retains the declaration comparison and adds an available or explicitly unavailable `liveImpact` under the same compact-JSON 8 MiB review limit. Supplemental API access is not added to durable `ReviewMetadataV1`, so existing authenticated declaration references remain valid.

For a fresh deployment, active-writer checkpoint and inventory capture may occur first. The CLI then preflights the declarations, previews the exact request, and budgets the combined report before reserving that unchanged request. It emits only after the selected session matches the captured plan digest and both review references. Preview failures cannot reserve the transition, though the earlier protected capture prerequisite may already have run. The final command result retains the same captured review instead of observing again after commit.

For a pending deployment, requested capacity resizing precedes preview. The adapter creates its own verified session and uses that transition's pinned historical references. It pins registry UID/resourceVersion, phase, record revision, verification epoch and both review references. All targets are listed and classified before any dry-run PATCH, and authority and target identity are rechecked before output. The report is labeled `pending-current-state`; it is never historical pre-deployment evidence. Committed retries perform no preview. Blocked or otherwise non-resumable transitions retain their recovery gates.

A pending object whose current protected claim has no UID cannot gain CI ownership authority from its live name, field manager or runtime inventory. Such an existing target yields whole-operation `pending-target-uid-not-proven`. Adoption that no longer has its original approved resourceVersion/managed-fields evidence yields `pending-adoption-result-not-proven`; phase or Active claim state does not prove adoption completed. These closed evidence gaps return no partial field report and do not block the independently authorized resume. Replacement UIDs, changed authority, denied requests, transport failures and unsupported servers remain hard errors.

The supplemental permission report lists `/version`, the exact identity-policy GET, named LIST/PATCH targets and potential CREATE access only where the canonical create capability exists. LIST includes an exact name selector and `resourceNames`; CREATE remains namespace/GVR-wide authorization with intended names listed separately. Existing registry/anchor, policy, trust, ownership and metadata-store access, and invoking-identity credential minting, remain part of the separate deployment contract. These permissions grant no authority by themselves, and Kubernetes has no dry-run-only RBAC grant.

## Shared request construction

The runtime's existing-object and absent-object apply paths now use the shared `platform::conditional_apply_request` builder. It preserves strict validation, the stable field manager and declared force, and installs an explicit UID/resourceVersion or absent-list resourceVersion precondition on a cloned desired object. Supplied concurrency metadata replaces any such metadata in the desired value. The builder performs no requests and establishes neither ownership nor absence; callers retain their authenticated claim checks, version qualification, dry-run interpretation and pre-write authority checks. The request type deliberately has no debug representation because it carries desired values.

This is a prerequisite for preview, not a preview endpoint. Preview must request dry-run parameters explicitly and must never call the runtime helpers that subsequently write. Its transport must also avoid the Kubernetes client's normal API-error decoding: that path can log response bodies before application-level sanitization. Use the client's raw response interface, bound successful bodies before deserialization, discard error bodies and expose only fixed error classifications. Do not print raw `FieldsV1` keys: list selectors can embed values, and live map keys are not automatically approved report metadata.

## Privacy boundary for live field classification

The `platform::preview_fields` projection supplies field classifications to the authenticated adapter and v5 deployment report. Its caller must authenticate previous/next metadata and bind the objects to the authorized observation interval. It compares presence and values transiently only at those declaration paths and the fixed rollout annotation path. Its output classifies approved paths as added, removed, changed or unchanged. Arrays retain aggregate declaration paths; a changed array does not establish which Kubernetes list element changed ownership. The API server remains the merge authority. Do not infer schema-aware ownership from the compiler's aggregate array representation.

Fields outside that approved catalog may produce only a coarse server-difference classification. Ownership fieldsets may be compared transiently to disclose a coarse changed, unchanged or unknown ownership result, but their raw selectors, manager names, key names, values, hashes and cardinalities must not enter the report. Exact element-level ownership requires a separate schema-aware implementation and real API-server qualification. These limits must remain visible in the report rather than being described as complete field ownership analysis.

The implementation labels field-set inequality as `serializedFieldSetChanged`, not a semantic ownership transfer. Unsupported or malformed managed-field encodings yield `unknown`. An ignore trie masks approved subtrees when comparing remaining server fields; incompatible or missing partial ancestors yield `unknown`. Object-only pointer traversal preserves numeric map keys and rejects traversal into an array. Each object is capped at 4 MiB, depth 64 and 100,000 nodes before recursive operations. The union is capped at 100,000 paths and 1 MiB of path bytes, with an exact 8 MiB typed-output serialization limit. Oversized evidence rejects rather than truncates.

## Bounded transport mechanism

`platform::preview_transport` provides a low-level named LIST and conditional SSA dry-run mechanism for ConfigMaps, Services and Deployments. It does not authenticate a deployment, grant permissions, perform runtime composition, reobserve a complete preview interval, or publish a report. Its raw object accessors return sensitive data to trusted callers; omitting `Debug` does not make those objects safe to print or persist. The authenticated adapter uses the approved stock issued-deployer client with retries disabled and no custom body-logging middleware. Supplying custom middleware can invalidate that boundary.

Requests use the Kubernetes library's request builder and raw client transport. Every PATCH fixes `dryRun=All`; error bodies are discarded unread and failures use static text. Request JSON is structurally checked and capped at 1 MiB before serialization; successful response collection is capped at 4 MiB and one request/body read has a 15-second deadline. Strict decoding rejects duplicate keys. List responses must match the exact collection type, contain at most one matching object, have no continuation or nonzero remaining count, and carry bounded UID/resourceVersion evidence. List items may omit both type fields and inherit the exact list type; partial or conflicting item types reject. PATCH responses must supply the exact object type.

The fresh adapter connects these mechanisms under protected opt-in and authenticated planning, composition, budgeting and race checks. The CLI connects both fresh and pending adapters and reports supplemental preview permissions separately from deployment authority. Read-only live list decoding does not qualify SSA non-persistence, absent-only creation, runtime admission or actual credential scope.

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
4. **Corruption and bounds:** reject missing, foreign-scope, reordered, duplicate, oversized or substituted metadata and forged backend results. Reject a missing required predecessor baseline without treating it as initial installation.
5. **Privacy:** canaries in ConfigMap values, container arguments, annotations, admission responses and API errors never reach report output, logs or persisted snapshots. Inspect stored bytes for values and fingerprints. With scope and permitted metadata held fixed, changing only private values must not change the declaration-metadata snapshot.
6. **Actual SSA behavior:** real API-server tests cover defaults, nullable/removable fields, associative and atomic lists, shared field ownership, force/non-force conflicts, absent-only creation, UID replacement and RV races. Confirm dry-run leaves object data, ownership and resourceVersion unchanged in an otherwise isolated test.
7. **Actual credentials:** demonstrate permitted preview requests, denial outside the exact target scope, rejection of missing approved permissions and identity-dependent admission differences. Verify every request has `dryRun=All`, including retries and errors.
8. **Dynamic contributions:** unchanged rollout contributions survive static preview; added/removed reactions expose their field contract; changing sensitive inputs produces no value/digest leakage or false deterministic output claim.
9. **Authority changes:** trust, policy, registry incarnation or selected inputs changed between resolution and preview reject. Stale preview output cannot bypass execution-time validation.

Unit tests establish projection and classification behavior. Mock request tests establish request shape and error handling. Real API-server tests establish SSA, admission and credential behavior for the qualified versions. None alone proves later workload health, globally atomic application, or production performance.
