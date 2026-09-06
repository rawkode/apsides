# Deployment review proposal

Status: **partially implemented**. Deployment review v3 implements the initial declaration projection described below. The typed immutable storage and trusted platform projection are implemented; protected registry bindings are not integrated. Durable historical comparison and live apply impact remain proposed. The current report does not satisfy all deployment-report requirements in [specification v0.2, section 25](SPEC-v0.2.md). Independent architecture review and the acceptance tests below remain implementation gates for the unfinished scope.

## Implemented initial declaration report

The `reakt.dev/deployment-review/v3` report uses an explicit empty declaration baseline for initial installation. It projects compiler-derived resource field paths, reactions, compact dependency rows, source locations, complete input authority and sensitivity, separate runtime/cleanup/deployment permission requirements, and allowlisted compatibility requirements. Arrays retain the compiler's atomic field ownership paths. Desired values and arbitrary compiler review documents are excluded.

Ownership classifications describe planned actions: an approved adoption is linked to its protected intent and observed UID/resourceVersion; another added declaration is `create-if-absent`. Neither proves a live operation completed. The report explicitly labels live SSA impact unavailable. Upgrades retain authenticated inventory comparisons but label unavailable historical graphs, field structures, permissions and compatibility as unavailable, never empty.

The CLI validates the shared typed report projection before reserving a fresh transition. A preallocation guard rejects more than 100,000 counted projection source elements (catalog rows, paths, references, capability names and policy lists). A separate exact byte limit rejects more than 8 MiB of serialized report content before producing output. Repeated change arrays and derived permission, compatibility and source-location rows are covered by the byte limit; the source-element guard is not an output-entry limit. This is a bounded partial implementation, not a claim that every compiler-accepted input fits those limits. Oversized reports fail as incomplete; they are never truncated. Upgrade identity preparation may precede report preflight, but reservation does not.

This slice adds no durable review snapshots, database fields, preview permissions or live API dry-run calls. Deployment API compatibility covers the issued deployer's requirements; invoking-identity bootstrap and token minting remain outside that catalog, as stated in the report. The storage and preview contracts below describe remaining work.

The scope is deployment reporting for the initial supported resource and reaction vocabulary. It does not change lifecycle authority, grant permissions, restore historical compiled plans, or move other coordination state into a database. Controller diagnostics and logging obligations in section 25 remain separate requirements.

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

The trusted platform projector replays the canonical transition, derives permissions internally, consumes its complete effect catalog, and constructs the closed metadata types without copying desired values or compiler review JSON. CLI review preflight invokes it before fresh reservation. Real compiler/planner tests cover metadata completeness, normalization, adoption, forged catalogs, and invariance under private-value and provenance-hash changes. This preflight validates the projection but does not yet publish or bind it.

The planned registry v7 binding is `StoredTransition.reviewBindings { next, previous }`. The existing active allocation and retained committed transition identify the active review reference, so no second mutable active pointer is required. A pending successor must preserve the exact prior committed `next` reference as its `previous`. Verified sessions must pin both bindings. Snapshot publication and exact read-back must finish before claim publication and reservation; commit promotes the allocation and transition in the existing registry CAS. Capacity admission must include the bounded references. These lifecycle changes are a separate integration gate; a working database trait alone does not implement durable upgrade reporting.

Registry v6 has no complete review baseline. The v7 cutover must reject that missing baseline explicitly; it cannot infer an empty history or reconstruct old reactions from live objects. The no-old-artifact acceptance scenario is an A-to-B upgrade where A was committed with protected review metadata. This contract covers active and pending comparisons, not indefinite lookup of every archived deployment report.

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
