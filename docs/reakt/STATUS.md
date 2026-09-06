# Reakt implementation status

Reakt implements specification 0.2 in the workspace crates and the `@reakt/kubernetes` SDK. The complete production release gate is **not yet satisfied**. A successful local compile, signed candidate, or individual cluster test is not a production release.

## Current integration checkpoint

Fresh A/B lifecycle images and signed unpromoted candidates were built from
`9559fd1` (`work/lifecycle-9559fd1-*-build` and `*-candidate`). Their source
identities are explicitly synthetic and coverage remains pending. The temporary
private signing key was deleted. The actual CLI rejected B under A's ceiling and
accepted the A+B handoff ceiling (`work/lifecycle-9559fd1-ceiling-preflight/`).
Preflight review found a harness ownership bug before cluster creation: ownership
was recorded before `kind create` succeeded. The runner now refuses name collisions,
marks ownership only on success, and reports results after verified deletion and
credential removal. Diagnostic failures cannot bypass cleanup. These changes
still require the fresh lifecycle run; no additional release check is closed.
Seven cleanup tests and six ceiling tests pass, alongside docs build/link checks
(`work/lifecycle-cleanup-*.log`). Independent cleanup review found no blockers.
Failed creation remains explicitly unowned and failed; it does not authorize
deletion based on a non-atomic earlier absence check.

The disposable lifecycle harness now starts with only the first candidate's
runtime ceiling. Before approving the successor it requires a typed ceiling
rejection and unchanged registry UID/resourceVersion/data, authenticated claims,
generation object identities and Role/RoleBinding contents. Cleanup approval
remains separate. Only after that rejection does the harness explicitly approve
the combined A+B runtime ceiling, preserving A until retirement, and continue
the existing handoff. The test driver checks the ceiling before predecessor
metadata capture or publication. This is a
qualification harness change awaiting a fresh signed-candidate run, not a passed
production upgrade check or a `VerifiedRelease` deployment.
Six mocked harness tests and the typed Rust classification test pass; strict
deploy/platform/CLI Clippy and docs build/link checks pass. Independent source
review found no blockers. This verifies the harness's compiler-policy preflight;
the disposable deployer still has broader Kubernetes RBAC, and the result does
not establish API-server rejection. Evidence: `work/lifecycle-ceiling-*.log`.

The corrected reactor producer at `72f761a` passed a fresh run: all 43 core cases
and 46 compiler fixture cases passed, network denials were observed, and the owned
Kubernetes v1.34.3 cluster was deleted. Its six-file evidence directory staged
successfully (`work/reactor-72f761a-results/`). Real CLI assembly merged it with
the measured reproducibility and declared-input isolation receipts into
`work/reactor-72f761a-assembly/release`. The actual reactor consumer passed,
validating production/fixture plan equality, all 43 required core cases and the
three fixture checks (`work/reactor-72f761a-consumer.log`). Supplemental fixture
cases are reported by the runner and are not individually accepted by this
consumer. All compiled runtime crate sources and vendored dependencies match the
measured production context; the fixture controller remains a different artifact.
This is five-check partial coverage, not trusted production release promotion.
Eleven required production checks remain open.
Both production-evidence consumers also passed on the merged bundle
(`work/reactor-72f761a-repro-consumer.log` and
`work/reactor-72f761a-isolation-consumer.log`). Independent review rehashed all
14 packaged evidence files, verified subjects, counts and cleanup, and found no
blockers. Docs build and links passed (`work/reactor-72f761a-docs.log`).

A fresh current-runtime reactor run passed all 43 required core cases and 46
compiler fixture cases in its owned Kubernetes v1.34.3 cluster, then deleted the
cluster (`work/reactor-b986698-results/result.json`). The fixture contract binds
to the production IR measured at `3a45749`. Release staging subsequently failed
because successful mutation cases referenced the receipt itself instead of the
transcript containing their observations. That reference is corrected; historical
receipts remain untouched. No assembled-evidence interoperability is claimed from
this failed overall run. A fresh run is required for the corrected producer.

Reactor evidence packaging now stages only successful runs into a flat release
evidence directory, fixes all receipt references to the release namespace, and
binds the coverage fragment to the production IR. Raw controller logs and cluster
diagnostics remain local; only a typed, explicitly scoped synthetic-canary scan
summary is published. The release verifier now requires the exact runner source
as a signed file and rejects missing or substituted source identities. Independent
source review found no blockers. Seven packaging tests and 30 release tests pass
(four opt-in tests skipped), along with strict release Clippy and docs build/link
checks. Logs are `work/reactor-packaging-*.log`. A fresh cluster run,
complete coverage assembly and actual reactor-consumer qualification are still
required before this change establishes release acceptance.

Declared-input isolation now has a real production-subject qualification from
commit `026ad98`. The normal CLI baseline before and after the probes matched the
previously measured production IR and authoring compiler closure. All seven
probes passed: a declared dependency compiled; the same physical dependency
omitted from the lock rejected; clock/random access rejected; environment,
filesystem and network guest capabilities were absent. The initial run exposed
incorrect compact source-lock serialization in the producer; the runner now
writes the compiler's required canonical indented format. Five orchestration
tests pass, including the corrected encoding check.

Evidence is under `work/input-isolation-canonical/`. The real CLI assembled it
together with reproducibility evidence in
`work/input-isolation-assembly-026ad98/release`, rejecting differing duplicate
evidence files during staging. Both explicitly invoked Rust consumers passed
one test each (`work/input-isolation-consumer.log` and
`work/input-isolation-repro-consumer.log`). This establishes producer, package and
consumer interoperability for these two checks. Capability absence probes do
not independently test host syscalls or network enforcement. Fourteen other
required checks, reactor coverage and production release promotion remain open.
Independent review checked the real probe outcomes, baseline IR, source locks,
payload subjects and every packaged evidence digest and found no blockers.

The corrected producer passed a fresh two-build run from commit `3a45749`, and
the real CLI assembled its flat evidence directory into a release. The Rust
consumer then validated the assembled payload and reproducibility receipt
(`work/production-prefix-after.log`: one explicitly invoked test passed). Evidence
is under `work/production-reproducibility-3a45749/` and
`work/production-assembly-3a45749/`. Signing that incomplete bundle with a
disposable local test key still caused `verify-release` to reject missing required
reactor coverage; the temporary private key was removed. These results prove
local two-build reproducibility and producer/package/consumer interoperability.
Independent review rehashed the packaged evidence and payloads and found no blockers.
They do not establish production signer trust, the other fifteen release checks,
reactor acceptance, image publication, or a deployable production release.

Two real cache-disabled production builds of the TSX example completed with
identical native binary, OCI archive, image manifest, IR, compiler closure and
canonical build inputs (`work/production-reproducibility-1341522-docker/`).
Independent review recomputed the hashes and confirmed both compiler runs in the
BuildKit logs. Real release assembly then exposed an evidence-path mismatch:
the CLI prefixes evidence files with `evidence/`, while the original receipt used
bare names. The producer now emits a flat staging directory with the correct
references. The opt-in assembled-evidence check reproduces the original rejection;
the fresh positive qualification above supersedes that failed interoperability run.

Release evidence now separates fixture reactor conformance from exact production
payload checks. Thirteen checks require production evidence bound to the signed
IR, compiler, binary, image/platform, build inputs and authenticated runner source;
fixture receipts cannot satisfy them. Signed negative tests cover subject swaps,
unbound runner sources, unknown checks and malformed assertions. The release suite
passed 29 tests with two integration tests ignored. Release and deploy/platform/CLI
strict Clippy passed. The new reproducibility producer invokes two builds with
BuildKit caching disabled, checks the locked source and resulting payloads, and
emits only its measured check. Six mocked producer tests and nine packaging tests
pass; these are not evidence that two real production builds match. See
`PRODUCTION-EVIDENCE.md` for the contracts and remaining acceptance boundary.

The first real no-cache qualification attempt failed before image assembly because
the packaging allowlist omitted the runtime's `coordination` dependency. That
omission is corrected and all ten packaging tests pass, including traversal of
the current workspace dependency closure. The failed attempt produced no
production evidence receipt (`work/production-reproducibility-4ecf13d/`).

Issued-credential SSA qualification now passes against a fresh disposable kind
cluster pinned to Kubernetes `v1.34.3`. `cuenv task previewQualification` uses the
client minted by `IssuedDeployment` for version discovery, named LIST and
conditional dry-run apply. Existing and absent targets remain unchanged in the
API server; wrong UID is rejected with HTTP 422 and stale resourceVersion with
409. The same client receives 403 for Secret reads, unfiltered and unapproved-name
LISTs, and unapproved-name PATCH. The harness removes its owned cluster and
transient admin kubeconfig, and rejects a zero-test success. Five mocked harness
tests cover failures, timeouts, name collisions, result validation and cleanup
(`cuenv task previewQualificationTest`). Local live evidence is in
`work/aps-preview-ad862476daf448e2/result.json` and its `test.log`.
This proves the issued-client transport and RBAC boundary on that server version;
it does not prove a complete signed-release CLI deployment, pending upgrade,
Pod/PV recovery, remote Turso operation, or OrbStack SSA qualification.
The focused preview suite passed 40 tests, strict all-target Clippy passed, and
the Astro build checked all links across 29 pages. Independent adversarial review
found no blockers for this qualification harness. Service/Deployment dry-runs,
force conflicts and multi-target timing remain outside this live test's scope.

Deployment review v5 now integrates the approved fresh and pending live-preview paths. Fresh combined-report budgeting precedes reservation of the same request; output follows selected plan/reference validation. Resumed observations retain exact phase/revision/epoch binding and are labeled current-state. Only closed missing-UID/adoption evidence can produce whole-live-preview unavailable while resume continues; authority and transport failures remain errors. Supplemental preview permissions remain separate from the durable declaration projector. The combined deploy/platform/CLI suite passed 249 tests with three live tests skipped, and the final focused preview suite passed 40 tests after pending precomposition validation (`work/live-review-registry-tests.log`, `work/live-review-preview-tests.log`). Strict all-target Clippy passed (`work/live-review-clippy.log`). CLI fixtures prove live/declaration binding, private-value omission, explicit opt-out and the combined 8 MiB limit; pending wire fixtures prove current Active-claim UID handling, unavailable without PATCH, conditional existing apply, exact binding and replacement/final-drift rejection. These are in-process fixtures, not a real CLI deployment or issued-token API-server qualification. Multi-target ordering, timeout cancellation and pending adoption completion remain further coverage work.

The following paragraphs record prior checkpoints; their statements that CLI preview is disconnected are superseded by the v5 integration above.

Low-level bounded preview transport and declaration-path classification are implemented. Sixteen focused tests pass, covering dry-run request shape, strict response decoding, size/depth limits, identity and pagination rejection, array projection semantics, coarse unknown results and private-value canaries. A separately invoked native read-only probe passed against OrbStack `v1.35.6+orb1`, confirming exact-list decoding and inheritance of omitted item type fields (`work/preview-mechanisms-tests.log`, `work/preview-orbstack-readonly.log`, `work/preview-orbstack-version.json`). That probe reads the public root CA ConfigMap and performs no mutation. The CLI still qualifies absent-only SSA only on `v1.34.3`; OrbStack SSA and credential-scope qualification remain outstanding. The fresh-request issued-deployer adapter, runtime contribution composition and race revalidation are now implemented in the library. The CLI uses its issued-client factory but does not yet invoke preview or include live impact in its report; pending-session preview and permission-report integration remain outstanding. See the [review contract](REVIEW-CONTRACT.md) for the boundary between these mechanisms and an authorized live preview.

Fresh-adapter verification: the focused preview suite passed 33 tests, including a successful bounded orchestration run with named LIST, conditional dry-run PATCH and final LIST, denial before workload requests, post-PATCH registry/policy/trust changes, and final target reappearance (`work/issued-preview-focused-tests.log`). The orchestration tests begin after the public verified-release equality boundary; signature verification remains covered separately. Issued-client tests cover inherited authentication/header removal, verified TLS settings, expiry before requests and protected identity changes. Independent source/test review found no blockers for this implementation checkpoint. Multi-target ordering, timeout cancellation, retained/adoption adapter wire flows and actual issued-token SSA remain unverified. The CLI report integration is still incomplete. The final combined deploy/platform/CLI suite passed 238 tests with three live tests skipped; strict all-target Clippy, the runtime-only platform build, and the 29-page Astro build with link checks passed (`work/issued-preview-registry-tests.log`, `work/issued-preview-clippy.log`, `work/issued-preview-runtime-check.log`, `work/issued-preview-docs.log`).

Preview-mechanism regression verification: `registryTest` passed 217 tests with three live tests skipped; the read-only list probe was invoked separately as described above. Strict all-target Clippy passed, and independent source, test and documentation review found no blockers (`work/preview-mechanisms-registry-tests.log`, `work/preview-mechanisms-clippy.log`).

Conditional SSA request construction is shared between the runtime and platform as a prerequisite for live preview. The runtime retains its claim/authority checks and dry-run-before-write sequence. The helper validates target name/namespace and UID/resourceVersion inputs, preserves Strict validation and declared force, and does not mutate the original desired value. It performs no API requests and adds no permissions. Independent source review found no blocking regression. The runtime suite passed 24 tests and the registry suite passed 201 with two live tests skipped (`work/ssa-request-runtime-tests.log`, `work/ssa-request-registry-tests.log`). These earlier results cover the request-builder extraction; the newer transport and classifier evidence is described above.

Deployment review v4 preflight loads the active baseline at the exact planning registry UID/resourceVersion; revision drift rejects before reservation, which independently checks that same snapshot. Source-element admission counts both metadata bodies before building deltas, and an exact 8 MiB serialized-report budget covers repeated before/after and change entries. Oversized output fails without truncation. An authenticated empty category is still an upgrade baseline.

V4 verification: the combined `registryTest` suite passed 198 tests with two live tests skipped; strict all-target Clippy and the runtime-only platform build also passed (`work/review-v4-registry-tests.log`, `work/review-v4-clippy.log`, `work/review-v4-runtime-check.log`). Independent source and test review found no blockers. Real compiler/planner fixtures cover metadata deltas, private-value invariance, policy/permission changes, authenticated empty baselines, and combined source admission. A separate valid typed historical snapshot proves the serialized-report amplification limit; it is not claimed to have been generated by today's smaller request budget. The exact-revision lookup test uses mock Kubernetes transport. The Astro site builds 29 pages with internal links checked. Live SSA, remote Turso, Pod/PV upgrades, and production release qualification remain unverified.

Registry v7 now binds each retained transition to exact next review metadata and, for upgrades, its previous committed review reference. Reservation projects and publishes the new metadata, independently validates readback, and completes capacity checks before claim publication and the registry CAS. Unbound content-addressed candidates grant no authority and do not block later candidates after a failed reservation; they still consume database storage. The successor retains the predecessor reference when commit prunes the old transition; there is no second mutable active-review pointer.

Selected sessions pin both references and authenticated bodies, revalidate them on registry reload and before publication, and compare the next metadata with the canonical verified-release plan. Generic registry loads validate scopes and relationships without requiring review-store credentials. Registry v6 is explicitly rejected because it cannot supply an authenticated complete review baseline. No automatic migration or reconstruction from live resources is implemented.

The CLI v4 report now compares authenticated declaration metadata for initial deployments and upgrades: resource identities and field paths, inputs/reactions/dataflow/source locations, separate permission sets, both policy ceilings and accepted risks, and compatibility. Stable IDs correlate graph changes; resource rows follow Kubernetes identity and keep ownership plans separate. Desired-value history and live SSA impact are explicitly unavailable. See [deployment review contract](REVIEW-CONTRACT.md).

The v7 focused tests prove publication failures before claim or Kubernetes writes, distinct-candidate retry after a rejected reservation CAS, substituted session references/backend data rejection, legacy/missing-binding rejection, and fresh B-only release resolution after B commit prunes A and the test discards A's plan. Missing predecessor inventory or review snapshots reject without writes. These use a mock Kubernetes transport and in-memory trait stores, not a live cluster. Trust/policy checks and registry CAS are separate operations; a concurrent revocation can leave an unusable pending transition whose subsequent sessions fail closed. Atomic revocation admission is not claimed.

V7 verification: `cuenv task registryTest` passed 187 tests with two live tests skipped (`work/review-v7-registry-tests.log`). Strict all-target Clippy and `registryRuntimeCheck` passed, including the platform build without deployer features (`work/review-v7-clippy.log`, `work/review-v7-runtime-check.log`). Independent architecture, source and test reviews found no blocking findings. The Astro site builds 29 pages with internal links checked. Live API-server behavior, image/PV upgrades, remote Turso durability, and the complete production release remain unqualified for this checkpoint.

Pre-v7 candidate-storage verification: `coordinationTest` passed 38 tests, including 12 review-store tests (`work/review-objects-tests.log`). Strict all-target storage Clippy and compilation without the libSQL backend also passed (`work/review-objects-clippy.log`). The combined deployment suite passed 180 tests with two live tests skipped, and strict all-target deployment Clippy passed (`work/review-projection-registry-tests.log`, `work/review-projection-clippy.log`). Independent source and test review found no blockers under the documented new-first/legacy-fallback lookup contract. Concurrent distinct candidates, local reopen, exact-reference corruption, missing/malformed object tables, legacy promotion and read-only compatibility are covered. Those storage tests alone did not establish registry CAS-loss recovery. The v7 mock-transport tests above add that case; remote Turso/Pod/PV behavior remains unqualified.

Prior storage foundation verification: `coordinationTest` passed 34 tests; strict all-target Clippy and compilation without the libSQL backend passed (`work/review-store-tests.log`, `work/review-store-clippy.log`). The existing deployment suite passed 175 tests with two live tests skipped (`work/review-store-registry-tests.log`). Independent schema, backend and test reviews passed. Local reopen, concurrent immutable conflicts, corruption, missing/malformed tables, read-only behavior and private-field rejection are covered. The Astro site builds 29 pages with internal links checked. Remote Turso behavior, crash/power-loss durability, registry binding and live image/PV upgrades remain unqualified.

Deployment review v3 now reports initial declaration changes for canonical resource fields, reactions, dependencies, source locations, input authority and sensitivity, separate permission requirements, and compatibility. Adoption remains a planned action linked to protected approval. Fresh reservation follows report preflight, with a source-element allocation guard and an exact 8 MiB serialized-output cap. Upgrade reports retain authenticated inventory comparisons and explicitly unavailable historical structure; live SSA preview and deployment-bound durable review snapshots remain unimplemented. This is partial section 25 progress, not a complete deployment-review or production-release gate. See [deployment review contract](REVIEW-CONTRACT.md).

Review verification: the combined deploy/platform/CLI suite passed 175 tests with two live tests skipped (`work/review-v3-registry-tests.log`). After the final helper refactors, all eleven focused review tests and strict all-target Clippy passed (`work/review-v3-final-tests.log`, `work/review-v3-registry-clippy.log`). Independent source review covered canonical projection, privacy, planned ownership, explicit upgrade uncertainty and preflight ordering. The Astro site builds 29 pages with internal links checked. The byte-bound unit tests do not establish an end-to-end no-write overflow scenario; reservation ordering is source-reviewed. No live SSA, workload, Pod-upgrade, PV or remote Turso qualification was performed for this slice.

Registry v6 now reserves canonical completion capacity for each pending domain before claim publication, with separate activation/inventory ceilings, bounded typed writer evidence, and a forty-receipt/eight-resume allowance. Exact capacity is pinned to verified sessions. The owning CI can use `--transition ID --capacity resize` on an untouched upgrade; a successful resize rebinds the verified session before deployment resumes. No manifests or private-content hashes are added to metadata storage. Pre-v6 registries require explicit recovery.

`cuenv task registryTest` passed 168 tests with two live tests skipped (`work/capacity-registry-tests.log`), and strict all-target Clippy passed (`work/capacity-registry-clippy.log`). Default-feature platform strict Clippy also passed. Independent source and regression reviews covered aggregate admission before claim writes, oversized activation metadata before inventory creation, topology identity checks, monotonic resize, session invalidation, and CLI routing. The Astro site includes the capacity contract and builds 28 pages with internal links checked. These checks do not qualify live maximal-profile completion, Pod upgrades, PV reattachment or remote Turso. See [state capacity admission](STATE-CAPACITY.md) for the exact boundaries.

The following paragraphs record earlier checkpoints superseded by registry v6.

Receipt-slot admission now guards lifecycle mutations using the existing 256-event limit, preserving room for the remaining successful path and one failure receipt. It leaves committed retries available. `cuenv task registryTest` passed 139 tests with two live tests skipped (`work/event-capacity-registry-tests.log`), and strict all-target Clippy passed (`work/event-capacity-registry-clippy.log`). Independent source and test review found no blocking issues. Full byte-capacity admission remains unimplemented. A slot refusal does not automatically persist Blocked, and already exhausted historical records require explicit recovery.

The next capacity prerequisite replaces generic drain JSON with strict typed observations while preserving canonical wire content and termination-evidence commitments. Initial activation/inventory state construction now rejects an oversized encoded payload before its ConfigMap create. `cuenv task registryTest` passed 135 tests with two live tests skipped (`work/typed-drain-registry-tests.log`); strict all-target Clippy passed (`work/typed-drain-registry-clippy.log`). Four compatibility/decoding tests also passed under default and deployer feature builds. Independent source review passed. Neither change reserves completion capacity or prevents all earlier staging side effects. Typed decoding establishes field shapes, not new per-field semantic or size validation; authenticated lifecycle and live termination checks remain required.

Registry v5 adds immutable metadata-only transition audits, a protected head per domain and retention of only the active committed record plus at most one unfinished transition. The `reakt audit` command uses existing read-only storage and grants no lifecycle authority. `cuenv task registryTest` passed 129 tests with two live tests skipped (`work/audit-registry-final-tests.log`); `registryCheck` passed strict all-target Clippy. Coordination passed 18 tests and strict checks. Independent source and test review covered archive failures, authenticated chains, privacy exclusions, exact predecessor retention and commit conflicts. CLI output tests establish rendering; successful live audit command execution and PV/Turso upgrades remain unverified. See [transition audit history](AUDIT.md).

The registry v4 cutover replaces stored full transition plans with compact replay inputs and immutable metadata references. The CLI now resolves the selected plan through a verified-release session before lifecycle execution and review. `cuenv task registryTest` passed 116 tests with two live tests skipped (`work/compact-registry-final-tests.log`). `cuenv task registryCheck` passed strict all-target Clippy (`work/compact-registry-final-clippy.log`). Independent production, harness, CLI and test-diff reviews passed. Reservation wire canaries, substituted replay authority, trust revocation before writes, old-schema recovery errors, local libSQL reopen and lifecycle regressions are covered. These checks do not establish live Pod upgrades, PV/Turso behavior or final production qualification. See [the current state contract](STATE-MIGRATION.md) for the remaining size, review and live-upgrade requirements.

The sections below record earlier implementation checkpoints. Their claims about retained full plans, checkpoint backfill and unavailable repository/reviewer access describe those earlier states and are superseded by the current contract. The repository is now private `rawkode/apsides`; independent review is active.

## Pending architecture changes from user direction

Historical OCI retrieval is not required for upgrades. Earlier artifact-recovery notes below describe a superseded direction. CI supplies the new release; previous inventory and compact writer/ownership state must be sufficient. Automatic transition recovery-location recording has been removed. OCI remains optional distribution for a requested release.

- Managed resources support Server-Side Apply only.
- Force reconciliation must be configurable by controller default and resource override. The effective setting must be compiled, reviewed and independently policy-approved; the compiler/runtime and policy checks are implemented, with live force upgrade acceptance passing in `work/reakt-lifecycle-results-v15/result.json` (candidate scope). It must not bypass UID, adoption, lifecycle, finalizer or writer-termination checks.
- The single ConfigMap containing complete plans and append-only generation history is rejected as the production history backend because it grows beyond Kubernetes object limits. The replacement is a metadata-only resource inventory trait, defaulting to libSQL/Turso (local persistent file or remote primary). Immutable snapshots and conditional publication allow future object-store implementations, including Parquet. The new crate passes six contract tests, strict Clippy, and a build without the libSQL feature. The deployment CLI now uses it for predecessor checks and committed publication; remote Turso and PVC upgrade behavior are unverified. Existing lifecycle results qualify the current executor behavior, not the scalability of that rejected backend.

## Implemented boundaries

- TSX authoring now composes Controller, ConfigMap, Deployment and Service elements with synchronous build-time observation hooks and opaque rollout projections. Oxc emits JSX source locations; the trusted compiler traverses the tree and injects provenance. Hook locations identify the containing component invocation. All six SDK modules are captured in the compiler receipt. The imperative SDK remains at `/legacy` for existing fixtures.
- Source compilation uses Oxc/Rolldown, permission-restricted Deno, and SES compartments. Declared source closure and tool identities are digest-bound; independent bundling/evaluation runs must agree.
- Native controllers use kube-rs, fixed resource plans, bounded queues/watches/backoff, strict SSA dry-runs, exact object ownership, durable UID inventory, activation/policy checks, and non-sensitive diagnostics.
- OCI verification binds the signed IR, native executable, exact approved base index/manifest/config/layers, compiler source closure, reports and SBOM. Real OpenSSH verification and a pinned GitHub Actions attestation verifier are supported. Fixture artifacts cannot be promoted.
- `reakt deploy` uses the invoking environment's Kubernetes identity to obtain a bounded, UID-bound TokenRequest credential. Protected namespace state serializes generation allocation, claims and lifecycle phases. The normal CLI uses a separate credential-bearing Envoy proxy for each generation.
- Upgrade quiescence freezes admission, waits for admitted writes, stops current tokenless runtime clients, validates the complete write ledger, and verifies termination of the original proxy container epoch before retiring its identity. Missing or restarted proxy epochs and uncertain nodes block activation.
- Cleanup derives authority from prior approved plans and protected claims. Deletion uses UID/resource-version preconditions and preserves finalizers. Missing trusted UID authorizes only an absence check; runtime inventory cannot grant deletion authority.
- A separate `test-support` feature exposes explicitly marked disposable-cluster candidate staging. Its typestate is not `VerifiedRelease`, and it is absent from the normal CLI build. This permits first acceptance testing without manufacturing successful coverage.

The API-bound profile does not claim field-level confinement. Exact checks of Reakt-owned Roles and bindings do not establish total effective authority: Kubernetes RBAC is additive, and independently administered authorization/admission remains a platform trust boundary. Proxy termination proves termination of the credential-bearing generation API writer, not the death of every tokenless process or the revocation of unrelated workload privileges.

## Current evidence

- TSX integration: SDK typecheck/traversal tests and bundle-builder typecheck/full tests pass, including locked multi-file reproducibility, isolated configuration, restricted imports and opaque-reference rejection. Independent adversarial review found and fixed time-varying Proxy rereads and non-enumerable property materialization at the native-data boundary; both regression reproductions pass the corrected snapshot contract. Real CLI compile/check of `examples/reakt/program.tsx` emits a valid native plan with three resources, three inputs and three rollout reactions. This is source/compiler evidence, not new cluster or final release acceptance.
- Release verifier: 18 tests passed, including real signatures, assertion contracts, compiler closure linkage, OCI/executable coherence and promotion rejection. The version 2 compiler closure includes the complete build context, packaging/CLI sources, build profile, and six host executable identities. Strict Clippy passed at this checkpoint.
- Native runtime: 21 unit/wire tests and 3 integration tests passed, including live-resource-version cache invalidation, durable created-UID handling and safe HTTP-status-based SSA conflict/authorization diagnostics. Strict Clippy passed.
- Platform executor: 13 unit, 16 wire and 2 credential tests passed at the latest platform checkpoint. Independent review found and fixed proxy epoch, Role/config drift and activation-resume gaps. The adoption wire test additionally verifies approval/object replacement rejection and strict non-force UID/resourceVersion-bound dry-run before reservation; it does not establish live adoption acceptance.
- Deployment model: 18 lifecycle tests passed, including absence-only cleanup, UID replacement, finalizers, interrupted phases, rollback generation identity, exact adoption approval binding and incomplete evidence rejection. Non-force adoption passed live candidate acceptance; transfer remains explicitly rejected pending its qualified executor.
- Two frozen native image builds, one with build cache disabled, produced identical OCI digest `sha256:460a4126c852aed1e0844645c56d141410c56bad77f653233441baacb3d45f68`. Two SBOM generations also matched. These establish the recorded intermediate snapshot, not the final application build.
- Pinned Envoy canaries exercised freeze rejection, admitted slow writes, overwritten credentials, stripped retry controls, SDS token rotation, unsafe upstream outcomes and shutdown scheduling. `work/reakt-envoy-canary-v11/result.json` also verifies that the exact admin filter excludes unrelated fractional histogram metadata. It is not by itself a Kubernetes lifecycle qualification.
- Reactor run v13 passed all 43 core cases and 46 compiler fixture cases, including managed-input transport unavailability using fast proxy shutdown. Its disposable cluster was deleted. This remains fixture artifact evidence, not exact production-artifact acceptance.
- Complete CLI builds produced two Linux arm64 lifecycle images, extracted binaries, real SBOMs and version 2 compiler closures (`work/reakt-complete-build-v2-first` and `work/reakt-complete-build-v2-second`). Candidate assembly and signatures passed. These are explicitly unpromoted candidates.
- Live lifecycle runs passed candidate verification, initial activation/resource convergence and upgrade staging. A Deployment revision annotation retry bug and Envoy statistics parsing bug were identified and narrowly fixed. Run v10 passed the complete signed-candidate install/upgrade lifecycle on a fresh kind cluster on OrbStack: writer quiescence, actual HTTP 401 before old-token expiry, retained UID, pending finalizer and held claim, eventual deletion, orphan retention, and idempotent commit. Run v11 established adoption but its later data change conflicted with the fixture’s original kubectl field ownership. Run v13 passed non-force adoption and rejected a replacement cleanup UID even with the same field-manager name. Run v14 passed adoption plus the complete finalizer/orphan lifecycle using fresh native images containing the diagnostics fix. Both disposable clusters were deleted. These are candidate lifecycle results, not final production-release acceptance.
- CLI: five tests and strict Clippy passed, including deployment selection and value-free field diff reporting. CLI reports now include protected prior/next resource, reaction, authority, permission and cleanup changes.

## Remaining release work

Complete exact production-artifact acceptance, live TLS/proxy lifecycle and failure injection, UID-safe cleanup/finalizer checks, adoption/transfer integration, operational recovery/uninstall and complete deployment permission reporting, scoped performance comparisons, standalone toolchain distribution, final reproducible artifacts, trusted GitHub workflow integration and independent final review. The private repository is `rawkode/apsides`; the trusted production workflow integration remains release work. Independent subagent review is available and has reviewed the current storage and projection design.

The retained OxideLoop code is historical POC work. Its pre-existing uncommitted changes were captured before migration. Whole-workspace tests previously reached a pre-existing legacy failure in `acknowledgement_observability_follows_successful_priority_reduction` (`missing desired observation`); that is separate from the new crate checks above. Implementation checkpoints have been committed and pushed to the private repository.

The v15 lifecycle candidate run passed operator-approved adoption, retirement of the previous writer, credential rejection before expiry, explicit force repairing a foreign SSA field while preserving UID, finalizer-delayed cleanup, orphan preservation, and idempotent commit. This run uses the existing lifecycle registry, not the new inventory backend.

Inventory integration now projects approved generations and protected claims into metadata-only snapshots for cleanup planning. The executor consumes API identity from v2 cleanup items instead of old manifests; private-value/source canaries and tampered API identity checks pass. The complete transition request still resides in the existing registry. The v15 live evidence predates transition-plan v2 and does not qualify this refactor or persistent inventory integration.

The platform publication path now writes committed metadata through `InventoryStore`. Real local libSQL tests cover reopen/idempotence, registry changes before and after the database commit, rejection of forged transition receipts and wrong claims, and retention of existing UIDs/newer heads. Six inventory tests and 31 platform tests pass; strict Clippy passes. Kubernetes responses in these publication tests are simulated. CLI connection setup and automatic invocation are now implemented; live persistent-volume upgrade acceptance is still outstanding.

Internal Rust packages and crate directories use role names without product prefixes (for example `compiler-core`, `deploy`, `platform`, `runtime`, `release`, and `inventory`). The CLI package is `cli`; its public binary remains `reakt`. The generated controller package is `controller`.

## Error handling hardening

Reakt's seven production crates deny Clippy's `unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`, and `unimplemented` lints outside test builds. Invalid data and broken internal assumptions propagate errors; they must not be replaced with silent defaults or successful-looking empty results. Ownership, release verification, fixture kubeconfig selection and transition lookup use checked access. Proxy and controller profiles are constructed without nested JSON mutation. The source compilation and lock command entrypoints catch operation failures, return nonzero exit codes and omit raw exception traces; subprocess stdin errors are handled too. Python build command boundaries return bounded failure messages without exception traces. The Rust CLI rejects non-UTF-8 arguments and propagates output errors; runtime diagnostics and generated controller error logging do not panic on a closed stderr stream.

This is a production-source rule, not proof that dependencies, allocation failure or every possible execution path cannot panic. Test fixtures still contain unwrap assertions, and the retained historical POC crates have not received the same enforcement. Programmatic TypeScript APIs retain rejected promises for validation failures, which callers must handle. Those scope limitations remain explicit; whole-workspace panic freedom is not claimed.

Error-handling verification: strict Clippy passed for all seven production crates and all their targets with the deployer feature. The seven-crate suite passed 118 tests plus doctests. Source compiler behavior and four source CLI boundary cases passed; nine Python packaging tests include five command failure boundaries. The final CLI/runtime rerun passed 31 tests, including closed-stdout and non-UTF-8 argument regressions, plus doctests; evidence is in `work/reakt-migration/no-panic-process-tests.log`. These checks do not constitute a new live cluster or production-image qualification.


The deployment CLI now opens a configured persistent libSQL file or TLS primary through `InventoryStore`. It checks the predecessor before reserving/resuming an upgrade, republishes protected active UIDs before starting a new transition, and requires committed publication before reporting success, including on committed retries. Endpoint credentials/query paths are rejected, connection errors omit tokens, and connection/publication operations have deadlines. The runtime does not receive database credentials. See `INVENTORY.md` for environment configuration and the post-commit retry contract.

This integration passed 46 CLI, inventory and platform tests plus doctests, including real local database reopen/corruption and predecessor checks. After the TLS startup-order correction, the final CLI rerun passed all nine tests; strict Clippy and workspace formatting passed. Evidence: `work/reakt-migration/inventory-cli-tests.log`, `inventory-cli-final-tests.log`, and `inventory-cli-clippy-verified.log`. Live remote Turso/PVC upgrade validation and independent review are not established. Agent status was rechecked: all three reviewers remain stopped at the account usage limit. Full-plan registry replacement remains an open production gate.


Inventory preflight now compares full persisted records with the approved predecessor and protected planning claims before reservation and resume. Publication verifies the predecessor content and permits only unknown-UID completion within one generation; missing resources, metadata changes, and known-UID changes reject without overwriting the head. The deployment planner and real libSQL-backed platform tests cover these mutations. This pass completed 60 tests plus doctests, strict Clippy, workspace formatting and whitespace checks (`work/reakt-migration/inventory-content-tests.log`, `inventory-content-clippy-v3.log`). Full-plan registry storage and live PVC/Turso verification remain open; this change does not replace them or establish independent review.

The deployment review now enumerates required issued-deployer API access for staging, upgrade retirement, cleanup and adoption, including Role delegation conditions and unrestricted-name creation scope. Privileged CI owns RBAC and controller image upgrades; runtime authority remains the approved compiled capabilities. Bootstrap discovery/minting and effective RBAC/admission evaluation are explicitly outside this report. See `DEPLOYMENT-PERMISSIONS.md`. The CLI/platform suite passed 43 tests plus doctests, including initial/upgrade report regressions and a check against kube's actual exec request method; strict Clippy, workspace formatting and whitespace checks passed (`work/reakt-migration/deployment-permissions-tests.log`, `deployment-permissions-clippy.log`). No new live-cluster or independent review evidence is established by these checks.

Old-writer retirement now derives the exact credential-bearing ServiceAccount and RoleBindings from the approved predecessor, rejects missing/empty UIDs and unrelated RoleBinding keys before quiescence, and requires the complete matching UID checkpoint before upgrade activation. Tokenless process ServiceAccounts are excluded from credential retirement. This closes the prior substring-based RoleBinding selection and nonempty-only checkpoint checks. Platform verification passed 36 tests plus doctests, strict Clippy, formatting and whitespace checks (`work/reakt-migration/retirement-tests.log`, `retirement-clippy.log`). These are local tests; live image qualification and independent review remain outstanding.

Storage migration dependency audit: the current registry persists full compiled generations and staged Secret hashes, while generation allocation depends on a permanently growing array capped at 4096 entries. `STATE-MIGRATION.md` records the required artifact-resolution, protected-coordination and metadata-only inventory boundaries, replacement order and acceptance evidence. This is an implementation contract, not a completed migration. The next prerequisite is immutable historical release resolution without a prior CI workspace. All three independent agents were rechecked and remain errored at the account usage limit. No production storage code changed during this audit.

Historical artifact resolution now has a library boundary: immutable OCI/release references, digest-pinned ORAS transport into owned temporary storage, protected release-digest comparison and the complete existing signature/acceptance verifier before returning a verified handle. Invalid references, changed transport executables, failed transport and malformed/mismatched releases reject. The release suite passed 23 tests plus doctests, strict Clippy, formatting and whitespace checks (`artifact-resolver-tests.log`, `artifact-resolver-clippy.log` under `work/reakt-migration`). ORAS is unavailable locally; transport tests use a stand-in. Real registry acceptance, CLI/compact-state integration and historical policy retention remain open. See `ARTIFACT-RESOLUTION.md`.

Real artifact transport verification used checksum-verified ORAS 1.3.4 and a disposable Distribution registry over TLS. It exposed ORAS's localhost HTTP default; the resolver now explicitly passes `--plain-http=false` and accepts a trusted absolute CA bundle path. The opt-in test passed real retrieval through schema validation, wrong release-digest rejection, absent OCI-digest rejection and untrusted-certificate rejection. The registry container was removed. The normal 23 release tests plus doctests, strict Clippy, formatting and whitespace checks also passed. Evidence: `work/reakt-migration/artifact-resolver-real-oras.log` and `oras/evidence.json`. The payload was deliberately invalid; this does not establish successful production release promotion, retention, compact-state recovery or deployment integration.

Deployment artifact retrieval is now integrated. `deploy --artifact-reference FILE` or `REAKT_RELEASE_REFERENCE` selects an immutable OCI artifact using trusted CI ORAS configuration. The fetched-content type grants no staging authority; the existing protected cluster policy still verifies the release, and the exact requested digest is checked again before credential minting. The temporary directory remains owned throughout deployment. Local and remote input selections conflict explicitly. CLI/reference tests cover bounded strict JSON, immutable references and selection; a real CLI process rejected a mutable reference before Kubernetes authentication. The CLI/release suite passed 34 tests plus doctests (the opt-in real registry test was not rerun), strict Clippy, formatting and whitespace checks. Evidence: `artifact-deploy-tests.log`, `artifact-deploy-clippy.log`, `artifact-deploy-process.log`. Successful signed-release deployment from OCI and automatic historical predecessor resolution remain unverified; full-plan protected state is still not migrated.

Bootstrap now binds its initial scope-discovery read to the subsequently verified release digest and derives credential scope from the verified compiled plan. An artifact changed between those reads rejects before compatibility discovery or credential minting, including when the replacement is itself an approved release. The requested OCI digest remains an additional constraint. All 12 CLI tests and strict Clippy passed, along with formatting and whitespace checks (`artifact-scope-tests.log`, `artifact-scope-clippy.log`). This closes a local/remote artifact-read consistency gap; it does not qualify full OCI deployment or the remaining compact-state migration.

Signed OCI retrieval now has a passing opt-in integration test. The test publishes the existing signed verifier fixture using real ORAS/TLS, excludes signing keys, deletes the source workspace, resolves the signed release, confirms download cleanup and rejects the same artifact under an unapproved signer policy. The isolated registry was removed. Evidence: `work/reakt-migration/artifact-signed-oci.log`. The fixture is deliberately synthetic and does not qualify a production controller image or live deployment; it establishes the resolver's complete signature-verification path after workspace loss. No production code changed in this pass.

Metadata-only cleanup planning is now a public boundary used by the transition planner: `plan_inventory_cleanup` accepts a prior inventory snapshot, next resource identities, protected claims and cleanup ceiling, with no previous compiled artifact. It checks complete claim correspondence, generation, state and UIDs, and preserves prior delete/orphan policy. The surrounding deployment request still carries old compiled plans and must be refactored next. All 67 existing CLI/deploy/platform tests plus doctests and the new direct metadata-only test passed; strict Clippy, formatting and whitespace checks passed (`metadata-cleanup-tests.log`, `metadata-cleanup-direct-tests.log`, `metadata-cleanup-clippy.log`). The interrupted release-location run had passed tests but failed a test lint; that discarded implementation and its tests have been removed, not presented as verified.

Active UID capture now takes persisted inventory metadata directly instead of enumerating/validating an old compiled resource set. The CLI loads the snapshot first. Scope, active generation, complete protected resource membership, claim ownership and recorded UIDs are checked; live reads retain exact Apply-manager and replacement/deletion checks. A simulated Kubernetes regression covers successful metadata capture and incomplete-inventory rejection before workload reads. All 49 CLI/platform tests plus doctests passed. Strict Clippy also checked the `test-support` live-candidate build after its API update; formatting and whitespace checks passed (`metadata-capture-tests.log`, `metadata-capture-clippy.log`). The disposable candidate helper projects its test inventory locally; no new live persistent-backend upgrade was run. Remaining publication and transition-model dependencies on old plans are still open.

Captured UID publication no longer reconstructs the old compiled inventory. The CLI invokes `publish_captured_inventory`, which reads authoritative claims, checks the capture revision and unchanged inventory head, and conditionally fills only unknown UIDs while preserving metadata/policies. Registry rechecks bracket publication. Real local libSQL tests cover reopen, unchanged metadata, forged caller claims, stale heads and known UID changes; Kubernetes responses are simulated. All 51 CLI/platform tests plus doctests, strict Clippy, formatting and whitespace checks passed (`captured-publication-tests.log`, `captured-publication-clippy.log`). New-generation commit publication and previous-generation transition representation still need migration; no live upgrade was claimed.

Protected generation allocations now bind a versioned digest of approved inventory metadata and scope, excluding UIDs and all desired values. Planning, UID capture and publication verify this commitment; captured publication also requires the allocation's transition ID on every claim. Tests reject changed deletion policy, legacy authority, ambiguous allocations and foreign-transition claims without changing the database head. The CLI/deploy/platform suite passed 78 tests; after adding explicit legacy replay errors, all 25 deploy checkpoint/lifecycle tests passed again (`work/inventory-checkpoint-full-tests.log`, `work/inventory-checkpoint-final-tests.log`). Strict Clippy, including platform test-support, formatting and whitespace checks passed (`work/inventory-checkpoint-clippy.log`). Independent review passed. This deliberately prevents begin/resume/replay/republish of pre-checkpoint POC transitions, although their JSON still decodes; explicit recovery is required and has not been implemented. See [the state migration contract](STATE-MIGRATION.md). Full-plan storage removal and live cluster/PV/Turso upgrade acceptance remain unfinished.

New v3 deployment requests carry predecessor metadata instead of a duplicated compiled generation. The planner uses persisted inventory for membership and prior delete/orphan policy, checks exact protected allocation/claims, and preserves safe absence handling when no UID was captured. The platform authenticates retirement namespaces against the unique verified historical transition before reservation and writer operations. CLI review separately verifies that historical record to retain detailed before/after reports. Pre-v3 POC records, including checkpoint-bearing v2 records, require explicit recovery; no automatic conversion is implemented. Each transition still persists its new compiled plan and predecessor inventory in the registry, so this does not satisfy bounded coordination or the greater-than-one-MiB tracking acceptance gate.

The v3 predecessor integration passed 99 compiler, deploy, platform and CLI tests, with two opt-in live-cluster tests skipped (`work/predecessor-final-tests.log`). The final deploy lifecycle rerun passed 23 tests (`work/predecessor-lifecycle-final.log`), and the final CLI rerun passed 13 tests after extracting predecessor preparation (`work/predecessor-cli-final.log`). Strict Clippy across all four crates with platform test-support, formatting and whitespace checks passed (`work/predecessor-final-clippy.log`). Independent adversarial review passed after correcting optional-UID handling and test-fixture failure masking. These checks use local libSQL and simulated Kubernetes responses; they do not establish a live pod-image upgrade or production release qualification.

Writer shutdown now uses a protected checkpoint stored on the current upgrade, captured atomically during reservation or backfilled from verified history with an owning-executor CAS and reload. Quiescence, retirement, activation publication and upgrade-authority checks consume the checkpoint without historical transition lookup. It contains only typed allocation/binding/proxy identity and exact retirement UIDs, with a 128 KiB bound; source capability commitments are checked during projection. Secret-content digests are no longer used for TLS staging or verification. Create responses are compared with ephemeral generated data, and resumed Secrets require the recorded UID, scope, `Opaque` type and immutability. Exact legacy TLS digest entries are omitted from the next registry write while unrelated commitments and UIDs remain intact.

The combined deploy/platform/CLI run passed 89 tests (`work/writer-checkpoint-full-tests.log`). After final checkpoint hardening, the platform suite passed 48 tests plus doctests, with two opt-in live-cluster tests skipped (`work/writer-checkpoint-final-platform.log`). Tests cover backfill retries, registry races without writes, checkpoint substitution, removal of historical source after capture, capability mismatch, size bounds, TLS admission changes and private-hash canaries. Registry compaction and live cluster/PV/Turso upgrade verification remain open; the test-only removal of a historical transition is not a production history-deletion implementation.

Final strict Clippy across deploy/platform/CLI and platform test-support passed (`work/writer-checkpoint-final-clippy.log`), as did formatting and whitespace checks. Independent adversarial review found no blocking findings. Historical records remain required for checkpoint creation/backfill and other consumers until the remaining storage migration is complete.

The v4 allocation cutover replaces permanent per-domain allocation vectors with one protected registry-wide sequence and a committed active allocation per domain. Names encode the full registry UUID and sequence; unsigned counters use canonical decimal strings to preserve precision through strict JSON. Reservation consumes the sequence in the existing registry CAS, and promotion happens only at final commit. CLI retry routing retains pending transition identity and rejects arbitrary new overrides or exhausted counters before inventory writes. Historical transition plans are still retained, so this removes allocation-history growth but does not complete registry compaction.

Registry v2 state binds its enclosing ConfigMap UID and a separately protected, scope-bound authority ConfigMap in `reakt-platform`. Deployment permissions request only exact GET access to the authority. Provisioning and the disposable lifecycle harness initialize the registry after its UID is assigned; copied state, wrong anchors and authority namespace overlap reject. Authority rebinding remains trusted platform recovery and must not race deployment. The runtime/inventory/policy schemas remain unchanged. Pre-v4 plans require explicit recovery, which is not implemented automatically.

The combined deploy/platform/CLI suite passed 100 tests, with two opt-in live tests ignored (`work/bounded-allocation-final-tests.log`). Strict all-target Clippy with platform test-support passed (`work/bounded-allocation-final-clippy.log`), along with formatting, whitespace and lifecycle harness syntax checks. Tests exercise counter exhaustion, strict decimal serialization, canonical replay, pending routing, cross-domain allocation after domain-entry removal, competing reservation CAS, copied registry state, anchor scope/deletion, commit promotion and existing checkpoint/inventory paths. Independent adversarial source review found no blocking findings. These are local tests with simulated Kubernetes responses and local inventory storage; no live pod upgrade, PV/Turso acceptance or full registry compaction is claimed.

Active writer checkpoints are now published atomically with committed allocations and transition records. New predecessor preparation and reservation use that metadata plus persistent inventory without a historical plan. Existing v4 checkpoints can be backfilled only from authenticated undisturbed committed history using CAS and reload, with no overwrite of present authority. Qualified initial activation and crash-resume wire fixtures preserve the existing ordering/RBAC checks; unqualified writers reject before activation or predecessor shutdown mutations.

Deployment review v2 can authenticate missing-history fallback against the reviewed transition's immutable predecessor checkpoint. It reports known added/removed membership and prior deletion policy while explicitly marking unknown structural changes, source locations, reactions, input authority, permissions and compatibility. Retained invalid history still rejects. This fallback does not claim the complete structural diff required by specification section 25. Historical transitions remain retained in production, and full-plan/claim-list compaction and live persistent-backend upgrade acceptance remain open.

The combined deploy/platform/CLI suite passed 104 tests with two live tests ignored (`work/active-writer-final-tests.log`). The final platform rerun passed 37 tests after adding explicit pre-mutation rejection coverage (`work/active-writer-final-platform-tests.log`). Strict all-target Clippy with test-support passed (`work/active-writer-final-clippy.log`), along with formatting, whitespace and lifecycle harness syntax checks. Tests cover atomic commit publication, authenticated backfill and races, predecessor removal before reservation, immutable-checkpoint review fallback, and qualified activation ordering/crash recovery. Independent source review found no blocking findings after correcting fallback authentication to the transition-local checkpoint. No live pod-image upgrade or production compaction was performed.

Registry claim storage now uses namespace-registry/v3 with a required immutable ClaimRef and a non-serializable hydrated registry. The platform authenticates the UID/scope/anchor before reading claims, independently verifies backend results, and publishes/read-verifies each snapshot before the existing Kubernetes CAS selects it. All registry writers use this path; exact admission responses are required. Empty-registry bootstrap and the disposable lifecycle driver use the same production boundary. The CLI shares the configured metadata database across inventory and coordination tables without passing credentials to the runtime. Pre-v3 registries require explicit recovery; automatic migration is not implemented.

Registry integration verification: `cuenv task registryTest` passed 112 tests with 2 live tests skipped (`work/registry-snapshot-final-tests.log`); `cuenv task registryCheck` passed strict all-target Clippy (`work/registry-snapshot-final-clippy.log`). Independent source/harness/test review, formatting and Python harness parsing passed. Tests include real local libSQL reopen through authenticated platform loads, false backend references, corrupted snapshots and rejected admission mutations. Full-plan retention, nested claim inputs, activation approvals, bounded-state acceptance and live persistent-backend upgrades remain unfinished.
